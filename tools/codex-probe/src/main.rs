use codex_usage_core::{
    accounts::{AccountId, AccountRecord, AccountStore, OPENAI},
    auth::{
        AccountAuthMaterialProvider, AccountOAuthMaterialProvider, OAuthCredentialProviderRegistry,
        OAuthCredentialStore, StoredOAuthCredential,
    },
    oauth_loopback::CodexOAuthCallbackListenerFactory,
    oauth_service::OAuthAuthorizationService,
    providers::openai::oauth_definition,
    providers::registry::ProviderRegistryConfig,
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::SqliteStore,
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Arguments { database, label } = parse_arguments()?;

    let database_path = database.unwrap_or_else(default_database_path);
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
        None,
    ) {
        Ok(account) => account,
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

    println!("OpenAI OAuth account linked: {}", account.email);
    println!("Account: {}", account.email);
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

async fn persist_oauth_login_account(
    account_store: &dyn AccountStore,
    credential_store: &dyn OAuthCredentialStore,
    account: AccountRecord,
    provisional_account_id: AccountId,
    credential: &StoredOAuthCredential,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    let existing_without_stable_identity = if account.provider_account_id.is_none() {
        account_store.list().await?.into_iter().find(|existing| {
            existing.provider_id == account.provider_id
                && existing.email.eq_ignore_ascii_case(&account.email)
        })
    } else {
        None
    };
    let resolved_result = if let Some(existing) = existing_without_stable_identity {
        Ok(existing)
    } else {
        account_store
            .upsert_or_get_by_provider_identity(&account)
            .await
    };
    let resolved = match resolved_result {
        Ok(account) => account,
        Err(error) => {
            if let Err(cleanup_error) = credential_store.remove(provisional_account_id).await {
                eprintln!("Warning: temporary OAuth credential cleanup failed: {cleanup_error}");
            }
            return Err(error.into());
        }
    };

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
            "--help" | "-h" => {
                println!(
                    "Usage: codex-usage-codex-probe [--database PATH] [--label LABEL]\n\nAdds a Codex account through OpenAI OAuth in the default browser and receives the authorization callback on localhost. OAuth credentials are stored per account in Windows Credential Manager; usage is queried from WHAM with that account's bearer token. It does not read browser cookies, Codex auth files, or launch Codex CLI/app-server."
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

    fn credential(refresh_token: &str, provider_account_id: Option<&str>) -> StoredOAuthCredential {
        StoredOAuthCredential {
            provider_id: OPENAI.to_owned(),
            refresh_token: refresh_token.to_owned(),
            client_id: Some("codex-public-client".to_owned()),
            client_secret: None,
            id_token: None,
            provider_account_id: provider_account_id.map(str::to_owned),
            workspace_id: None,
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
            Some("chatgpt-account-1".to_owned()),
            OPENAI,
            None,
        )
        .unwrap();
        accounts.upsert(&existing).await.unwrap();
        credentials
            .save(
                existing.id,
                &credential("old-refresh-token", Some("chatgpt-account-1")),
            )
            .await
            .unwrap();

        let provisional = AccountRecord::create(
            "Codex account",
            "codex@example.com",
            Some("chatgpt-account-1".to_owned()),
            OPENAI,
            None,
        )
        .unwrap();
        credentials
            .save(
                provisional.id,
                &credential("new-refresh-token", Some("chatgpt-account-1")),
            )
            .await
            .unwrap();

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &credential("new-refresh-token", Some("chatgpt-account-1")),
        )
        .await
        .unwrap();

        assert_eq!(resolved.id, existing.id);
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
        let new_credential = credential("new-refresh-token", None);
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
}

fn default_database_path() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("CodexUsageMonitor-Rust")
        .join("accounts.db")
}
