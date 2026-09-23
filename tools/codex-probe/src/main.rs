use codex_usage_core::{
    accounts::{AccountId, AccountRecord, AccountStore, OPENAI},
    auth::{
        AccountAuthMaterialProvider, AccountOAuthMaterialProvider, OAuthCredentialProviderRegistry,
        OAuthCredentialStore, StoredOAuthCredential,
    },
    oauth_loopback::CodexOAuthCallbackListenerFactory,
    oauth_service::OAuthAuthorizationService,
    providers::registry::ProviderRegistryConfig,
    providers::{codex_workspace::resolve_workspace_name, openai::oauth_definition},
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::{ReqwestUsageHttpTransport, UsageHttpTransport},
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    WindowsDefaultBrowserLauncher,
};
use std::{env, path::PathBuf, sync::Arc, time::Duration};

type Transport = ReqwestUsageHttpTransport;
type CredentialStore = codex_usage_windows_auth::WindowsCredentialManagerStore;
type CallbackFactory = CodexOAuthCallbackListenerFactory;
type Browser = WindowsDefaultBrowserLauncher;
type Authorization =
    OAuthAuthorizationService<Transport, CredentialStore, CallbackFactory, Browser>;

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    resolve_workspace_names: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Arguments {
        database,
        label,
        resolve_workspace_names,
    } = parse_arguments()?;

    let database_path = database.unwrap_or_else(default_accounts_database_path);
    if resolve_workspace_names && !database_path.is_file() {
        return Err(format!(
            "Codex account database does not exist: {}",
            database_path.display()
        )
        .into());
    }
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let credential_store = Arc::new(WindowsCredentialManagerStore);
    let provider = oauth_definition();
    let authorization = create_authorization(transport.clone(), credential_store.clone());
    if resolve_workspace_names {
        return resolve_saved_workspace_names(&database_path, transport, &provider, authorization)
            .await;
    }
    let provisional =
        AccountRecord::create("Codex account", "pending@local.invalid", None, OPENAI, None)?;

    println!("Opening OpenAI authorization in your default browser.");
    println!("Complete sign-in there; the browser will return to this app on localhost.");
    let login = authorization
        .login(provisional.id, &provider, Duration::from_secs(300))
        .await?;
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

fn announce_cli_account_reference(account: &AccountRecord) {
    if std::env::var_os("CODEX_USAGE_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("CODEX_USAGE_ACCOUNT_REF={account_ref}");
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

async fn resolve_saved_workspace_names(
    database_path: &std::path::Path,
    transport: Arc<Transport>,
    provider: &codex_usage_core::auth::OAuthProviderDefinition,
    authorization: Authorization,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SqliteStore::open(database_path)?;
    let accounts = store
        .list()
        .await?
        .into_iter()
        .filter(|account| {
            account.provider_id == OPENAI
                && account.workspace_id.is_some()
                && account.workspace_name.is_none()
        })
        .collect::<Vec<_>>();
    if accounts.is_empty() {
        println!("No Codex accounts are missing a workspace name.");
        return Ok(());
    }

    let mut updated = 0;
    let mut unresolved = 0;
    let mut failed = 0;
    for account in accounts {
        let workspace_id = account
            .workspace_id
            .as_deref()
            .expect("filtered account has a workspace id");
        let tokens = match authorization.access_token(account.id, provider).await {
            Ok(tokens) => tokens,
            Err(error) => {
                failed += 1;
                eprintln!(
                    "Workspace name lookup failed for {}: {error}",
                    account.email
                );
                continue;
            }
        };
        match resolve_workspace_name(transport.as_ref(), &tokens.access_token, workspace_id).await {
            Ok(Some(workspace_name)) => {
                let named_account = account.with_workspace_name(Some(&workspace_name));
                store.upsert(&named_account).await?;
                println!("{} — workspace: {}", account.email, workspace_name);
                updated += 1;
            }
            Ok(None) => {
                eprintln!(
                    "Workspace id was not listed for {}; keeping it unchanged.",
                    account.email
                );
                unresolved += 1;
            }
            Err(error) => {
                eprintln!(
                    "Workspace name lookup failed for {}: {error}",
                    account.email
                );
                failed += 1;
            }
        }
    }
    println!("Workspace-name lookup: {updated} updated, {unresolved} unresolved, {failed} failed.");
    if failed > 0 {
        return Err(format!("workspace-name lookup failed for {failed} Codex account(s)").into());
    }
    Ok(())
}

async fn probe_and_print(
    database_path: &std::path::Path,
    transport: Arc<Transport>,
    credential_store: Arc<CredentialStore>,
    provider: &codex_usage_core::auth::OAuthProviderDefinition,
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

fn format_metric_status(metric: &codex_usage_core::usage::UsageMetric) -> String {
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
    parse_arguments_from(env::args_os().skip(1))
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
            "--resolve-workspace-names" => {
                arguments.resolve_workspace_names = true;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: codex-usage-codex-probe [--database PATH] [--label LABEL]\n       codex-usage-codex-probe --resolve-workspace-names [--database PATH]\n\nAdds a Codex account through OpenAI OAuth in the default browser and receives the authorization callback on localhost. OAuth credentials are stored per account in Windows Credential Manager. Workspace names are optionally resolved from OpenAI's account metadata endpoint. Usage is queried from WHAM with that account's bearer token. It does not read browser cookies, Codex auth files, or launch Codex CLI/app-server."
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
    use super::{format_metric_status, parse_arguments_from, persist_oauth_login_account};
    use codex_usage_core::{
        accounts::{AccountRecord, AccountStore, InMemoryAccountStore, OPENAI},
        auth::{InMemoryOAuthCredentialStore, OAuthCredentialStore, StoredOAuthCredential},
        usage::UsageMetric,
    };
    use std::{collections::BTreeMap, ffi::OsString, sync::Arc};

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

        let resolve_arguments = parse_arguments_from(
            ["--resolve-workspace-names", "--database", "accounts.db"].map(OsString::from),
        )
        .unwrap();
        assert!(resolve_arguments.resolve_workspace_names);
        assert_eq!(
            resolve_arguments.database.as_deref(),
            Some(std::path::Path::new("accounts.db"))
        );
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
