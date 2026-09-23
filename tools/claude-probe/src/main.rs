//! Exercise the real Claude Web add-account flow on Windows.
//!
//! The user signs in through the normal default browser. The probe never asks
//! for a password, embeds a WebView, or keeps a browser process alive: after
//! opening `claude.ai/login`, the Windows auth layer polls a private copy of
//! the selected Chromium cookie database until `sessionKey` is available.

use codex_usage_core::{
    accounts::{AccountRecord, AccountStore, CLAUDE, VerifiedIdentity},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        AccountBrowserSessionRefresher, CompositeAuthMaterialProvider, StoredAuthMaterialProvider,
    },
    auth_sources::{EnvironmentAuthMaterialProvider, LocalFileAuthMaterialProvider},
    providers::{
        claude::{ClaudeSourceMode, fetch_web_identity},
        registry::ProviderRegistryConfig,
    },
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::ReqwestUsageHttpTransport,
    usage::UsageSnapshotStore,
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore,
    browser_cookies::{BrowserKind, BrowserLoginOptions, WindowsBrowserCookieImporter},
};
use std::{env, path::PathBuf, sync::Arc, time::Duration};
use url::Url;

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    email: Option<String>,
    force_new: bool,
    login: bool,
    browser: Option<BrowserKind>,
    profile_id: Option<String>,
    timeout_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let browser_importer = WindowsBrowserCookieImporter::from_process()?;
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);

    let (identity, browser_material, imported_browser) = if arguments.login {
        let timeout = Duration::from_secs(arguments.timeout_seconds.max(1));
        let login_url = Url::parse("https://claude.ai/login")?;
        println!("Checking for an existing Claude Web session first.");
        let result = browser_importer
            .open_and_wait_for_provider(
                CLAUDE,
                &login_url,
                BrowserLoginOptions {
                    browser: arguments.browser,
                    profile_id: arguments.profile_id.clone(),
                    timeout,
                    ..BrowserLoginOptions::default()
                },
            )
            .await?;
        if result.opened_browser {
            println!(
                "Browser session captured from {} profile {} after {}s.",
                result.imported.browser,
                result.imported.profile_id,
                result.elapsed.as_secs()
            );
        } else {
            println!(
                "Reusing the existing {} browser session from profile {}.",
                result.imported.browser, result.imported.profile_id
            );
        }
        let session_key = result
            .imported
            .cookies
            .iter()
            .find(|cookie| cookie.name.eq_ignore_ascii_case("sessionKey"))
            .map(|cookie| cookie.value.as_str())
            .ok_or("Claude browser session did not contain sessionKey")?;
        let identity = fetch_web_identity(transport.as_ref(), session_key).await?;
        let material = AccountAuthMaterial {
            cookies: result.imported.cookies,
            user_agent: result.imported.user_agent,
            ..AccountAuthMaterial::default()
        };
        (
            identity,
            material,
            Some((result.imported.browser, result.imported.profile_id)),
        )
    } else {
        let account =
            find_existing_account(account_store.as_ref(), arguments.email.as_deref()).await?;
        let material = secure_material_store
            .get(account.id)
            .await?
            .ok_or("the selected Claude account has no stored browser session")?;
        let identity = VerifiedIdentity {
            email: Some(account.email.clone()),
            provider_account_id: account.provider_account_id.clone(),
            plan_type: None,
        };
        (identity, material, None)
    };

    let email = identity
        .email
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or("Claude Web account did not return an email address")?;
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
    if let Some((browser, profile_id)) = imported_browser.as_ref() {
        account.browser_kind = Some(browser.as_str().to_owned());
        account.browser_profile_id = Some(profile_id.to_owned());
    }
    account_store.upsert(&account).await?;

    let mut material_to_store = browser_material;
    if let Some(existing) = secure_material_store.get(account.id).await? {
        material_to_store.fill_missing_from(&existing);
    }
    secure_material_store
        .save(account.id, &material_to_store)
        .await?;
    let account = account_store
        .get(account.id)
        .await?
        .ok_or("Claude account disappeared after its browser session was saved")?;
    announce_cli_account_reference(&account);

    let stored_material = Arc::new(StoredAuthMaterialProvider::new(
        secure_material_store.clone(),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let account_file_material = Arc::new(LocalFileAuthMaterialProvider::from_process(account.id))
        as Arc<dyn AccountAuthMaterialProvider>;
    let environment_material = Arc::new(EnvironmentAuthMaterialProvider::from_process(account.id))
        as Arc<dyn AccountAuthMaterialProvider>;
    let auth = Arc::new(CompositeAuthMaterialProvider::new([
        stored_material,
        account_file_material,
        environment_material,
    ])) as Arc<dyn AccountAuthMaterialProvider>;

    let runtime = UsageRuntime::from_dependencies_with_auth_store_and_session_refresher(
        account_store,
        snapshot_store,
        transport,
        auth,
        secure_material_store as Arc<dyn AccountAuthMaterialStore>,
        Arc::new(browser_importer) as Arc<dyn AccountBrowserSessionRefresher>,
        ProviderRegistryConfig {
            claude_source_mode: ClaudeSourceMode::Web,
            fetch_claude_account_identity: true,
            fetch_claude_web_extras: true,
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
        .ok_or("Claude Web account did not return an email address")?;
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
    let mut values = env::args_os().skip(1);
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
            "--browser" => {
                let value = values
                    .next()
                    .ok_or("--browser requires chrome, edge, brave, or chromium")?
                    .to_string_lossy()
                    .to_ascii_lowercase();
                arguments.browser = Some(parse_browser(&value)?);
            }
            "--profile" => {
                arguments.profile_id = Some(
                    values
                        .next()
                        .ok_or("--profile requires a Chromium profile id")?
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
                    "Usage: codex-usage-claude-probe [--database PATH] [--label LABEL] [--email EMAIL] [--new] [--browser chrome|edge|brave|chromium] [--profile PROFILE] [--timeout-seconds N] [--probe-existing]\n\nOpens the normal browser at claude.ai/login, waits for a sessionKey, stores it in Windows Credential Manager, and probes Claude Web usage."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn parse_browser(value: &str) -> Result<BrowserKind, Box<dyn std::error::Error>> {
    BrowserKind::ALL
        .into_iter()
        .find(|browser| browser.as_str() == value)
        .ok_or_else(|| format!("unsupported browser: {value}").into())
}
