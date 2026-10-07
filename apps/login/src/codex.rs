use std::{
    env,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use url::Url;
use usage_monitor_core::{
    accounts::{AccountId, AccountRecord, AccountStore, OPENAI},
    auth::{
        AccountAuthMaterialProvider, AccountOAuthMaterialProvider, OAuthBrowserLauncher,
        OAuthCredentialProviderRegistry, OAuthCredentialStore, OAuthLoginResult,
        OAuthProviderDefinition, StoredOAuthCredential,
    },
    oauth_loopback::{CodexOAuthCallbackListenerFactory, is_callback_bind_failure},
    oauth_service::OAuthAuthorizationService,
    providers::registry::ProviderRegistryConfig,
    providers::{codex_device, codex_workspace::resolve_workspace_name, openai::oauth_definition},
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::{ReqwestUsageHttpTransport, UsageHttpTransport},
};
use usage_monitor_windows::{
    WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    WindowsDefaultBrowserLauncher,
};

type Transport = ReqwestUsageHttpTransport;
type CredentialStore = usage_monitor_windows::WindowsCredentialManagerStore;
type CallbackFactory = CodexOAuthCallbackListenerFactory;
type Browser = WindowsDefaultBrowserLauncher;
type Authorization =
    OAuthAuthorizationService<Transport, CredentialStore, CallbackFactory, Browser>;

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    /// The desktop app ran the device-code sign-in and passes the
    /// authorization code and its verifier on stdin, one per line.
    authorization_stdin: bool,
    /// Fail instead of falling back to a device code when localhost cannot
    /// be listened on; for callers that cannot show the code as it comes.
    no_device_code: bool,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Arguments {
        database,
        label,
        authorization_stdin,
        no_device_code,
    } = parse_arguments()?;

    let database_path = database.unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let credential_store = Arc::new(WindowsCredentialManagerStore);
    let provider = oauth_definition();
    let authorization = create_authorization(transport.clone(), credential_store.clone());
    let provisional =
        AccountRecord::create("Codex account", "pending@local.invalid", None, OPENAI, None)?;

    let login = if authorization_stdin {
        let (code, verifier) = read_stdin_authorization()?;
        authorization
            .login_with_authorization_code(
                provisional.id,
                &provider,
                &code,
                &device_redirect_uri(),
                &verifier,
            )
            .await?
    } else {
        println!("Opening OpenAI authorization in your default browser.");
        println!("Complete sign-in there; the browser will return to this app on localhost.");
        match authorization
            .login(provisional.id, &provider, Duration::from_secs(300))
            .await
        {
            Ok(login) => login,
            Err(error) if is_callback_bind_failure(&error) && !no_device_code => {
                eprintln!("{error}");
                sign_in_with_device_code(
                    &authorization,
                    transport.as_ref(),
                    &provider,
                    provisional.id,
                )
                .await?
            }
            Err(error) => return Err(error.into()),
        }
    };
    let Some(identity) = login.identity.as_ref() else {
        credential_store.remove(provisional.id).await?;
        return Err("OpenAI did not return a verifiable account identity".into());
    };
    let Some(email) = identity
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
    else {
        credential_store.remove(provisional.id).await?;
        return Err("OpenAI OAuth identity did not include an email".into());
    };
    let workspace_name = if let Some(workspace_id) = identity.workspace_id.as_deref() {
        match resolve_workspace_name(transport.as_ref(), &login.tokens.access_token, workspace_id)
            .await
        {
            Ok(name) => name,
            Err(error) => {
                eprintln!(
                    "Workspace name lookup was unavailable; keeping the workspace id: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    let account_label = label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Codex — {email}"));
    let account = match AccountRecord::create(
        account_label,
        email,
        identity.provider_account_id.clone(),
        OPENAI,
        identity.workspace_id.clone(),
    ) {
        Ok(account) => account.with_workspace_name(workspace_name.as_deref()),
        Err(error) => {
            credential_store.remove(provisional.id).await?;
            return Err(error.into());
        }
    };
    let account = persist_oauth_login_account(
        account_store.as_ref(),
        credential_store.as_ref(),
        account,
        provisional.id,
        &login.credential,
    )
    .await?;
    announce_cli_account_reference(&account);

    println!("OpenAI OAuth account linked: {}", account.email);
    println!("Account: {}", account.email);
    println!(
        "Workspace: {}",
        account.workspace_name.as_deref().unwrap_or("unavailable")
    );
    println!("Database: {}", database_path.display());
    probe_and_print(
        &database_path,
        transport.clone(),
        credential_store.clone(),
        &provider,
        &account,
    )
    .await?;
    println!("Restarting the authorization service to verify saved-token refresh...");
    probe_and_print(
        &database_path,
        transport,
        credential_store,
        &provider,
        &account,
    )
    .await?;
    Ok(())
}

fn device_redirect_uri() -> Url {
    Url::parse(codex_device::REDIRECT_URI).expect("static Codex device redirect")
}

/// The sign-in for computers that cannot receive the reply on localhost:
/// OpenAI shows a code that the user enters on its page.
async fn sign_in_with_device_code(
    authorization: &Authorization,
    transport: &Transport,
    provider: &OAuthProviderDefinition,
    account_id: AccountId,
) -> Result<OAuthLoginResult, Box<dyn std::error::Error>> {
    println!("This computer blocks the usual sign-in reply, so sign in with a code instead.");
    let code = codex_device::request_device_code(transport, &provider.client_id).await?;
    println!(
        "Open {} and enter this code: {}",
        codex_device::VERIFICATION_URL,
        code.user_code
    );
    let page = Url::parse(codex_device::VERIFICATION_URL)?;
    if let Err(error) = WindowsDefaultBrowserLauncher.open(&page).await {
        eprintln!("Could not open the browser: {error}");
    }
    let granted = codex_device::poll_for_authorization(transport, &code).await?;
    Ok(authorization
        .login_with_authorization_code(
            account_id,
            provider,
            &granted.authorization_code,
            &device_redirect_uri(),
            &granted.code_verifier,
        )
        .await?)
}

fn read_stdin_authorization() -> Result<(String, String), Box<dyn std::error::Error>> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    parse_authorization(&value)
}

fn parse_authorization(value: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let mut lines = value.lines().map(str::trim).filter(|line| !line.is_empty());
    match (lines.next(), lines.next()) {
        (Some(code), Some(verifier)) => Ok((code.to_owned(), verifier.to_owned())),
        _ => Err("an authorization code and its verifier are needed on stdin".into()),
    }
}

fn announce_cli_account_reference(account: &AccountRecord) {
    if std::env::var_os("USAGE_MONITOR_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("USAGE_MONITOR_ACCOUNT_REF={account_ref}");
    }
}

async fn persist_oauth_login_account(
    account_store: &dyn AccountStore,
    credential_store: &dyn OAuthCredentialStore,
    account: AccountRecord,
    provisional_account_id: AccountId,
    credential: &StoredOAuthCredential,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    let existing_accounts = account_store.list().await?;
    let existing_by_identity = account.provider_account_id.as_deref().and_then(|identity| {
        existing_accounts.iter().find(|existing| {
            existing.provider_id == account.provider_id
                && existing.provider_account_id.as_deref() == Some(identity)
                && existing.workspace_id == account.workspace_id
        })
    });
    let legacy_workspace_identity = if account.provider_id == OPENAI {
        account.workspace_id.as_deref().and_then(|workspace_id| {
            existing_accounts.iter().find(|existing| {
                existing.provider_id == OPENAI
                    && existing.provider_account_id.as_deref() == Some(workspace_id)
                    && existing.workspace_id.is_none()
                    && existing.email.eq_ignore_ascii_case(&account.email)
            })
        })
    } else {
        None
    };
    let existing_without_stable_identity = if account.provider_account_id.is_none() {
        existing_accounts.iter().find(|existing| {
            existing.provider_id == account.provider_id
                && existing.email.eq_ignore_ascii_case(&account.email)
                && existing.workspace_id == account.workspace_id
        })
    } else {
        None
    };
    let resolved_result: Result<AccountRecord, Box<dyn std::error::Error>> =
        if let Some(existing) = existing_by_identity {
            existing
                .with_identity(Some(&account.email), account.provider_account_id.as_deref())
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
        } else if let Some(legacy) = legacy_workspace_identity {
            // The first Rust Codex OAuth implementation mistakenly stored the
            // selected workspace id as the provider identity. Repair that row by
            // exact email + workspace match without merging other workspace users.
            legacy
                .with_identity(Some(&account.email), account.provider_account_id.as_deref())
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
        } else if let Some(existing) = existing_without_stable_identity {
            Ok(existing.clone())
        } else {
            account_store
                .upsert_or_get_by_provider_identity(&account)
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
        };
    let resolved = match resolved_result {
        Ok(account) => account,
        Err(error) => {
            if let Err(cleanup_error) = credential_store.remove(provisional_account_id).await {
                eprintln!("Warning: temporary OAuth credential cleanup failed: {cleanup_error}");
            }
            return Err(error);
        }
    };
    let resolved = if let Some(workspace_id) = account.workspace_id.as_deref() {
        resolved.with_workspace_id(Some(workspace_id))
    } else {
        resolved
    };
    let resolved = if let Some(workspace_name) = account.workspace_name.as_deref() {
        resolved.with_workspace_name(Some(workspace_name))
    } else {
        resolved
    };
    if let Err(error) = account_store.upsert(&resolved).await {
        if let Err(cleanup_error) = credential_store.remove(provisional_account_id).await {
            eprintln!("Warning: temporary OAuth credential cleanup failed: {cleanup_error}");
        }
        return Err(error.into());
    }

    if let Err(error) = credential_store.save(resolved.id, credential).await {
        if resolved.id != provisional_account_id
            && let Err(cleanup_error) = credential_store.remove(provisional_account_id).await
        {
            eprintln!("Warning: temporary OAuth credential cleanup failed: {cleanup_error}");
        }
        return Err(error.into());
    }
    if resolved.id != provisional_account_id
        && let Err(error) = credential_store.remove(provisional_account_id).await
    {
        eprintln!("Warning: temporary OAuth credential cleanup failed: {error}");
    }
    Ok(resolved)
}

fn create_authorization(
    transport: Arc<Transport>,
    credential_store: Arc<CredentialStore>,
) -> Authorization {
    OAuthAuthorizationService::new(
        transport,
        credential_store,
        Arc::new(CodexOAuthCallbackListenerFactory),
        Arc::new(WindowsDefaultBrowserLauncher),
    )
}

async fn probe_and_print(
    database_path: &std::path::Path,
    transport: Arc<Transport>,
    credential_store: Arc<CredentialStore>,
    provider: &usage_monitor_core::auth::OAuthProviderDefinition,
    account: &AccountRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    let authorization = Arc::new(create_authorization(
        transport.clone(),
        credential_store.clone(),
    ));
    let registry = Arc::new(OAuthCredentialProviderRegistry::new([provider.clone()]));
    let auth = Arc::new(AccountOAuthMaterialProvider {
        authorization,
        credentials: credential_store,
        providers: registry,
    }) as Arc<dyn AccountAuthMaterialProvider>;
    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let runtime = UsageRuntime::from_sqlite_path_with_auth_store(
        database_path,
        transport as Arc<dyn UsageHttpTransport>,
        auth,
        secure_material_store,
        ProviderRegistryConfig::default(),
        RefreshCoordinatorConfig {
            cadence: RefreshCadence::Manual,
            ..RefreshCoordinatorConfig::default()
        },
    )?;
    let outcome = runtime
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        let message = outcome
            .error
            .as_ref()
            .map(|error| error.message.clone())
            .unwrap_or_else(|| "usage refresh did not produce a snapshot".to_owned());
        return Err(message.into());
    }
    let snapshot = outcome.snapshot.expect("updated outcome has a snapshot");
    let rate_windows = snapshot.all_rate_windows().collect::<Vec<_>>();
    let additional_metrics = snapshot
        .metrics
        .iter()
        .filter(|metric| {
            !rate_windows
                .iter()
                .any(|window| window.name.eq_ignore_ascii_case(&metric.name))
        })
        .collect::<Vec<_>>();
    println!(
        "Plan: {}",
        snapshot.plan_type.as_deref().unwrap_or("unknown")
    );
    println!(
        "Source: {} | rate windows: {} | additional metrics: {}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        rate_windows.len(),
        additional_metrics.len()
    );
    for window in &rate_windows {
        let reset = window
            .reset_at_utc
            .as_ref()
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_owned());
        println!(
            "{}: {:.2}% remaining | reset {}",
            window.name,
            window.remaining_percent(),
            reset
        );
    }
    for metric in additional_metrics {
        println!("{}", format_metric_status(metric));
    }
    Ok(())
}

fn format_metric_status(metric: &usage_monitor_core::usage::UsageMetric) -> String {
    let remaining = metric
        .remaining_percent()
        .map(|value| format!("{value:.2}% remaining"))
        .or_else(|| {
            metric.remaining_amount.map(|value| {
                format!(
                    "{value:.2} {} remaining",
                    metric.unit.as_deref().unwrap_or("units")
                )
            })
        })
        .unwrap_or_else(|| "unknown remaining quota".to_owned());
    let reset = metric
        .reset_at_utc
        .as_ref()
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_owned());
    format!("{}: {remaining} | reset {reset}", metric.name)
}

fn parse_arguments() -> Result<Arguments, Box<dyn std::error::Error>> {
    parse_arguments_from(env::args_os().skip(2))
}

fn parse_arguments_from(
    values: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Arguments, Box<dyn std::error::Error>> {
    let mut arguments = Arguments::default();
    let mut values = values.into_iter();
    while let Some(argument) = values.next() {
        match argument.to_string_lossy().as_ref() {
            "--database" => {
                arguments.database = Some(PathBuf::from(
                    values.next().ok_or("--database requires a path")?,
                ));
            }
            "--label" => {
                arguments.label = Some(
                    values
                        .next()
                        .ok_or("--label requires a value")?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--credentials-stdin" => arguments.authorization_stdin = true,
            "--no-device-code" => arguments.no_device_code = true,
            "--help" | "-h" => {
                println!(
                    "Usage: usage-monitor-login codex [--database PATH] [--label LABEL] [--credentials-stdin] [--no-device-code]\n\nAdds a Codex account through OpenAI OAuth in the default browser and receives the authorization callback on localhost. When this computer cannot listen on localhost, it signs in with a one-time code entered on OpenAI's page instead. With --credentials-stdin, an authorization code and its verifier from a device-code sign-in are read from stdin, one per line. --no-device-code fails instead of offering the code, for callers that cannot show it. OAuth credentials are stored per account in Windows Credential Manager, and the workspace name is read from OpenAI's account metadata. Usage is queried from WHAM with that account's bearer token. It does not read browser cookies, Codex auth files, or launch Codex CLI/app-server."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::{
        format_metric_status, parse_arguments_from, parse_authorization,
        persist_oauth_login_account,
    };
    use std::{collections::BTreeMap, ffi::OsString, sync::Arc};
    use usage_monitor_core::{
        accounts::{AccountRecord, AccountStore, InMemoryAccountStore, OPENAI},
        auth::{InMemoryOAuthCredentialStore, OAuthCredentialStore, StoredOAuthCredential},
        usage::UsageMetric,
    };

    fn credential(
        refresh_token: &str,
        provider_account_id: Option<&str>,
        workspace_id: Option<&str>,
    ) -> StoredOAuthCredential {
        StoredOAuthCredential {
            provider_id: OPENAI.to_owned(),
            refresh_token: refresh_token.to_owned(),
            client_id: Some("codex-public-client".to_owned()),
            client_secret: None,
            id_token: None,
            provider_account_id: provider_account_id.map(str::to_owned),
            workspace_id: workspace_id.map(str::to_owned),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn arguments_accept_database_and_label_for_oauth_account_add() {
        let arguments = parse_arguments_from(
            ["--database", "accounts.db", "--label", "Codex Work"].map(OsString::from),
        )
        .unwrap();

        assert_eq!(
            arguments.database.as_deref(),
            Some(std::path::Path::new("accounts.db"))
        );
        assert_eq!(arguments.label.as_deref(), Some("Codex Work"));
        assert!(
            parse_arguments_from(["--resolve-workspace-names"].map(OsString::from)).is_err(),
            "maintenance modes are not part of the sign-in helper"
        );
    }

    #[test]
    fn a_device_authorization_comes_on_stdin_as_two_lines() {
        let arguments = parse_arguments_from(["--credentials-stdin"].map(OsString::from)).unwrap();
        assert!(arguments.authorization_stdin);
        assert_eq!(
            parse_authorization("\n code-1 \nverifier-1\n").unwrap(),
            ("code-1".to_owned(), "verifier-1".to_owned())
        );
        assert!(parse_authorization("code-only\n").is_err());
    }

    #[test]
    fn arguments_no_longer_accept_cookie_import_profile_options() {
        assert!(parse_arguments_from(["--browser", "chrome"].map(OsString::from)).is_err());
    }

    #[test]
    fn amount_metrics_are_not_mislabeled_as_percentages() {
        let metric = UsageMetric {
            key: "credits".to_owned(),
            name: "Credits".to_owned(),
            used_percent: None,
            used_amount: None,
            limit_amount: None,
            remaining_amount: Some(2.5),
            unit: Some("credits".to_owned()),
            reset_at_utc: None,
            reset_label: None,
            metadata: Default::default(),
        };

        let output = format_metric_status(&metric);

        assert_eq!(output, "Credits: 2.50 credits remaining | reset unknown");
        assert!(!output.contains("% remaining"));
    }

    #[tokio::test]
    async fn duplicate_openai_identity_reuses_account_and_moves_oauth_credential() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let credentials = Arc::new(InMemoryOAuthCredentialStore::default());
        let existing = AccountRecord::create(
            "Codex personal",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap();
        accounts.upsert(&existing).await.unwrap();
        credentials
            .save(
                existing.id,
                &credential(
                    "old-refresh-token",
                    Some("chatgpt-user-1"),
                    Some("workspace-1"),
                ),
            )
            .await
            .unwrap();

        let provisional = AccountRecord::create(
            "Codex account",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap()
        .with_workspace_name(Some("Team North"));
        credentials
            .save(
                provisional.id,
                &credential(
                    "new-refresh-token",
                    Some("chatgpt-user-1"),
                    Some("workspace-1"),
                ),
            )
            .await
            .unwrap();

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &credential(
                "new-refresh-token",
                Some("chatgpt-user-1"),
                Some("workspace-1"),
            ),
        )
        .await
        .unwrap();

        assert_eq!(resolved.id, existing.id);
        assert_eq!(resolved.workspace_name.as_deref(), Some("Team North"));
        assert_eq!(accounts.list().await.unwrap().len(), 1);
        assert_eq!(
            credentials
                .get(existing.id)
                .await
                .unwrap()
                .unwrap()
                .refresh_token,
            "new-refresh-token"
        );
        assert!(credentials.get(provisional.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unavailable_workspace_name_lookup_preserves_a_previously_saved_name() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let credentials = Arc::new(InMemoryOAuthCredentialStore::default());
        let existing = AccountRecord::create(
            "Codex work",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap()
        .with_workspace_name(Some("Engineering"));
        accounts.upsert(&existing).await.unwrap();
        let provisional = AccountRecord::create(
            "Codex account",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap();
        let new_credential = credential(
            "new-refresh-token",
            Some("chatgpt-user-1"),
            Some("workspace-1"),
        );
        credentials
            .save(provisional.id, &new_credential)
            .await
            .unwrap();

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &new_credential,
        )
        .await
        .unwrap();

        assert_eq!(resolved.id, existing.id);
        assert_eq!(resolved.workspace_name.as_deref(), Some("Engineering"));
    }

    #[tokio::test]
    async fn duplicate_openai_email_without_account_claim_reuses_account() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let credentials = Arc::new(InMemoryOAuthCredentialStore::default());
        let existing =
            AccountRecord::create("Codex personal", "codex@example.com", None, OPENAI, None)
                .unwrap();
        accounts.upsert(&existing).await.unwrap();
        let provisional =
            AccountRecord::create("Codex account", "codex@example.com", None, OPENAI, None)
                .unwrap();
        let new_credential = credential("new-refresh-token", None, None);
        credentials
            .save(provisional.id, &new_credential)
            .await
            .unwrap();

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &new_credential,
        )
        .await
        .unwrap();

        assert_eq!(resolved.id, existing.id);
        assert!(credentials.get(provisional.id).await.unwrap().is_none());
        assert_eq!(
            credentials
                .get(existing.id)
                .await
                .unwrap()
                .unwrap()
                .refresh_token,
            "new-refresh-token"
        );
    }

    #[tokio::test]
    async fn different_users_in_the_same_workspace_remain_separate_accounts() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let credentials = Arc::new(InMemoryOAuthCredentialStore::default());
        let first = AccountRecord::create(
            "Codex first seat",
            "first@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("shared-workspace".to_owned()),
        )
        .unwrap();
        accounts.upsert(&first).await.unwrap();
        let provisional = AccountRecord::create(
            "Codex second seat",
            "second@example.com",
            Some("chatgpt-user-2".to_owned()),
            OPENAI,
            Some("shared-workspace".to_owned()),
        )
        .unwrap();
        let second_credential = credential(
            "second-refresh-token",
            Some("chatgpt-user-2"),
            Some("shared-workspace"),
        );

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &second_credential,
        )
        .await
        .unwrap();

        assert_ne!(resolved.id, first.id);
        assert_eq!(
            resolved.provider_account_id.as_deref(),
            Some("chatgpt-user-2")
        );
        assert_eq!(resolved.workspace_id.as_deref(), Some("shared-workspace"));
        assert_eq!(accounts.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn same_user_in_different_workspaces_remains_separate_accounts() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let credentials = Arc::new(InMemoryOAuthCredentialStore::default());
        let first = AccountRecord::create(
            "Codex workspace one",
            "user@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap();
        accounts.upsert(&first).await.unwrap();
        let provisional = AccountRecord::create(
            "Codex workspace two",
            "user@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-2".to_owned()),
        )
        .unwrap();
        let second_credential = credential(
            "workspace-two-refresh-token",
            Some("chatgpt-user-1"),
            Some("workspace-2"),
        );

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &second_credential,
        )
        .await
        .unwrap();

        assert_ne!(resolved.id, first.id);
        assert_eq!(
            resolved.provider_account_id.as_deref(),
            Some("chatgpt-user-1")
        );
        assert_eq!(resolved.workspace_id.as_deref(), Some("workspace-2"));
        assert_eq!(accounts.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn legacy_workspace_keyed_codex_row_is_repaired_by_email_and_workspace() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let credentials = Arc::new(InMemoryOAuthCredentialStore::default());
        let legacy = AccountRecord::create(
            "Codex legacy",
            "codex@example.com",
            Some("workspace-1".to_owned()),
            OPENAI,
            None,
        )
        .unwrap();
        accounts.upsert(&legacy).await.unwrap();
        let provisional = AccountRecord::create(
            "Codex account",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap();
        let new_credential = credential(
            "new-refresh-token",
            Some("chatgpt-user-1"),
            Some("workspace-1"),
        );

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &new_credential,
        )
        .await
        .unwrap();

        assert_eq!(resolved.id, legacy.id);
        assert_eq!(
            resolved.provider_account_id.as_deref(),
            Some("chatgpt-user-1")
        );
        assert_eq!(resolved.workspace_id.as_deref(), Some("workspace-1"));
        assert_eq!(accounts.list().await.unwrap().len(), 1);
        assert!(credentials.get(provisional.id).await.unwrap().is_none());
        assert_eq!(
            credentials
                .get(legacy.id)
                .await
                .unwrap()
                .unwrap()
                .provider_account_id
                .as_deref(),
            Some("chatgpt-user-1")
        );
    }
}
