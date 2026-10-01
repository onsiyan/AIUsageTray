//! Add Claude accounts through a direct browser OAuth sign-in.
//!
//! Like the Codex and Antigravity flows, this opens Claude's authorization page
//! in the default browser (PKCE + localhost callback), verifies the signed-in
//! identity, and stores the per-account credentials in Windows Credential
//! Manager. It does not need the Claude Code CLI or a temporary profile.

use std::{env, path::PathBuf, sync::Arc, time::Duration};
use usage_monitor_core::{
    accounts::{AccountRecord, AccountStore, CLAUDE, VerifiedIdentity},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        StoredAuthMaterialProvider,
    },
    claude_oauth,
    oauth_loopback::LoopbackOAuthCallbackListenerFactory,
    providers::{
        claude::{ClaudeSourceMode, fetch_oauth_identity},
        registry::ProviderRegistryConfig,
    },
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::ReqwestUsageHttpTransport,
    usage::UsageSnapshotStore,
};
use usage_monitor_windows::{
    WindowsCredentialManagerAuthMaterialStore, WindowsDefaultBrowserLauncher,
};

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    email: Option<String>,
    force_new: bool,
    login: bool,
    timeout_seconds: u64,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments()?;
    let database_path = arguments
        .database
        .unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite;
    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);

    let (identity, material) = if arguments.login {
        let timeout = Duration::from_secs(arguments.timeout_seconds.max(1));
        println!("Opening Claude sign-in in your default browser. Complete authentication there.");
        let material = claude_oauth::login(
            transport.as_ref(),
            &LoopbackOAuthCallbackListenerFactory,
            &WindowsDefaultBrowserLauncher,
            timeout,
        )
        .await?;
        require_claude_code_oauth_material(&material)?;
        let access_token = material
            .bearer_token
            .as_deref()
            .expect("OAuth material validation requires an access token");
        let identity = fetch_oauth_identity(transport.as_ref(), access_token).await?;
        (identity, material)
    } else {
        let account =
            find_existing_account(account_store.as_ref(), arguments.email.as_deref()).await?;
        let material = secure_material_store
            .get(account.id)
            .await?
            .ok_or("the selected Claude account has no stored Claude Code OAuth credentials")?;
        require_claude_code_oauth_material(&material)?;
        let identity = VerifiedIdentity {
            email: Some(account.email.clone()),
            provider_account_id: account.provider_account_id.clone(),
            plan_type: None,
        };
        (identity, material)
    };

    let email = identity
        .email
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or("Claude OAuth profile did not return an email address")?;
    if let Some(expected_email) = arguments.email.as_deref()
        && !expected_email.eq_ignore_ascii_case(email)
    {
        return Err(format!("Claude login belongs to {}, not {}", email, expected_email).into());
    }

    let mut account = if arguments.login {
        find_or_create_account(
            account_store.as_ref(),
            &identity,
            arguments.label.as_deref(),
            arguments.force_new,
        )
        .await?
    } else {
        find_existing_account(account_store.as_ref(), arguments.email.as_deref()).await?
    };
    account.browser_kind = None;
    account.browser_profile_id = None;
    account_store.upsert(&account).await?;

    secure_material_store.save(account.id, &material).await?;
    let account = account_store
        .get(account.id)
        .await?
        .ok_or("Claude account disappeared after its OAuth credentials were saved")?;
    announce_cli_account_reference(&account);

    let stored_material = Arc::new(StoredAuthMaterialProvider::new(
        secure_material_store.clone(),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let auth = stored_material as Arc<dyn AccountAuthMaterialProvider>;

    let runtime = UsageRuntime::from_dependencies_with_auth_store(
        account_store,
        snapshot_store,
        transport,
        auth,
        secure_material_store as Arc<dyn AccountAuthMaterialStore>,
        ProviderRegistryConfig {
            claude_source_mode: ClaudeSourceMode::OAuth,
            fetch_claude_account_identity: true,
            fetch_claude_web_extras: false,
            fetch_claude_prepaid_credits: true,
            ..ProviderRegistryConfig::default()
        },
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
            .unwrap_or_else(|| "Claude usage refresh did not produce a snapshot".to_owned());
        return Err(message.into());
    }
    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!("Account: {} ({})", account.label, account.email);
    println!(
        "OK | source={} | plan={} | confidence={}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.plan_type.as_deref().unwrap_or("unknown"),
        snapshot.data_confidence
    );
    for window in snapshot.all_rate_windows() {
        println!(
            "  {}: {:.2}% remaining | reset {}",
            window.name,
            window.remaining_percent(),
            window
                .reset_at_utc
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "unknown".to_owned())
        );
    }
    println!("Database: {}", database_path.display());
    Ok(())
}

fn require_claude_code_oauth_material(
    material: &AccountAuthMaterial,
) -> Result<(), Box<dyn std::error::Error>> {
    let has_access_token = material
        .bearer_token
        .as_deref()
        .is_some_and(|token| token.starts_with("sk-ant-oat"));
    let has_refresh_token = material
        .oauth_refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    if !has_access_token || !has_refresh_token {
        return Err(
            "Claude Code sign-in did not provide both OAuth access and refresh credentials".into(),
        );
    }
    Ok(())
}

fn announce_cli_account_reference(account: &AccountRecord) {
    if env::var_os("CODEX_USAGE_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("CODEX_USAGE_ACCOUNT_REF={account_ref}");
    }
}

async fn find_or_create_account(
    store: &dyn AccountStore,
    identity: &VerifiedIdentity,
    label: Option<&str>,
    force_new: bool,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    let email = identity
        .email
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or("Claude OAuth profile did not return an email address")?;
    if !force_new {
        if let Some(account) = store.list().await?.into_iter().find(|account| {
            account.provider_id == CLAUDE
                && (account.email.eq_ignore_ascii_case(email)
                    || identity
                        .provider_account_id
                        .as_deref()
                        .is_some_and(|id| account.provider_account_id.as_deref() == Some(id)))
        }) {
            return Ok(account.with_identity(Some(email), identity.provider_account_id.as_deref())?);
        }
    }
    Ok(AccountRecord::create(
        label.unwrap_or("Claude account"),
        email,
        identity.provider_account_id.clone(),
        CLAUDE,
        None,
    )?)
}

async fn find_existing_account(
    store: &dyn AccountStore,
    email: Option<&str>,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    let mut accounts = store
        .list()
        .await?
        .into_iter()
        .filter(|account| account.provider_id == CLAUDE)
        .collect::<Vec<_>>();
    if let Some(email) = email {
        accounts.retain(|account| account.email.eq_ignore_ascii_case(email));
    }
    match accounts.len() {
        1 => Ok(accounts.remove(0)),
        0 => Err("no matching Claude account exists".into()),
        count => Err(format!(
            "{count} Claude accounts exist; pass --email when using --probe-existing"
        )
        .into()),
    }
}

fn parse_arguments() -> Result<Arguments, Box<dyn std::error::Error>> {
    let mut arguments = Arguments {
        login: true,
        timeout_seconds: 300,
        ..Arguments::default()
    };
    let mut values = env::args_os().skip(2);
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
            "--email" => {
                arguments.email = Some(
                    values
                        .next()
                        .ok_or("--email requires a value")?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--timeout-seconds" => {
                arguments.timeout_seconds = values
                    .next()
                    .ok_or("--timeout-seconds requires a number")?
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|_| "--timeout-seconds must be a positive integer")?;
            }
            "--new" => arguments.force_new = true,
            "--probe-existing" => arguments.login = false,
            "--help" | "-h" => {
                println!(
                    "Usage: usage-monitor-login claude [--database PATH] [--label LABEL] [--email EMAIL] [--new] [--timeout-seconds N] [--probe-existing]\n\nSigns in to Claude with OAuth in the default browser, verifies the account with Claude's OAuth profile endpoint, stores per-account credentials in Windows Credential Manager, and probes OAuth usage."
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
    use super::*;

    #[test]
    fn claude_probe_accepts_only_refreshable_oauth_material() {
        let oauth = AccountAuthMaterial {
            bearer_token: Some("sk-ant-oat-test".to_owned()),
            oauth_refresh_token: Some("refresh-test".to_owned()),
            ..AccountAuthMaterial::default()
        };
        assert!(require_claude_code_oauth_material(&oauth).is_ok());

        let web_session = AccountAuthMaterial::from_cookie_header("sessionKey=web-session", None);
        assert!(require_claude_code_oauth_material(&web_session).is_err());

        let non_refreshable_oauth = AccountAuthMaterial {
            bearer_token: Some("sk-ant-oat-test".to_owned()),
            ..AccountAuthMaterial::default()
        };
        assert!(require_claude_code_oauth_material(&non_refreshable_oauth).is_err());
    }
}
