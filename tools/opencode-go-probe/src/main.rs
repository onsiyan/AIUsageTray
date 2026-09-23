//! Add and exercise one OpenCode Go browser account on Windows.
//!
//! Login deliberately happens in the user's normal browser. This command
//! never receives a password, embeds a WebView, or keeps a browser process
//! alive. After the user presses Connect in the local extension, only the
//! selected cookies are copied into the account-scoped Windows Credential
//! Manager entry and the normal authoritative web adapter is run once.

use codex_usage_core::{
    accounts::{AccountId, AccountRecord, AccountStore, OPENCODE_GO},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        OAuthBrowserLauncher, StoredAuthMaterialProvider,
    },
    providers::{opencode_go::OpenCodeGoSourceMode, registry::ProviderRegistryConfig},
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::ReqwestUsageHttpTransport,
    usage::UsageSnapshotStore,
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore, WindowsDefaultBrowserLauncher,
    browser_bridge::BrowserBridgeSession,
};
use std::{env, path::PathBuf, sync::Arc, time::Duration};
use url::Url;

const PENDING_EMAIL: &str = "pending@opencode.local";

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    resume_account: Option<AccountId>,
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
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);

    let account = if let Some(account_id) = arguments.resume_account {
        let account = account_store
            .get(account_id)
            .await?
            .ok_or("OpenCode Go account was not found in the selected database")?;
        if account.provider_id != OPENCODE_GO {
            return Err("the selected account is not an OpenCode Go account".into());
        }
        let material = secure_material_store
            .get(account.id)
            .await?
            .ok_or("the account has no saved browser session in Credential Manager")?;
        let mut cookie_names = material
            .cookies
            .iter()
            .map(|cookie| cookie.name.clone())
            .collect::<Vec<_>>();
        cookie_names.sort_unstable();
        cookie_names.dedup();
        println!(
            "Stored session metadata: {} cookies; names={} (values withheld).",
            material.cookies.len(),
            cookie_names.join(",")
        );
        println!("Reusing the saved OpenCode Go browser session.");
        account
    } else {
        // The ID scopes the imported session in Windows Credential Manager.
        // Keep both after a parsing/subscription error so the same browser
        // login can be retried without asking the user to sign in again.
        let label = arguments
            .label
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("OpenCode Go account");
        let mut account = AccountRecord::create(label, PENDING_EMAIL, None, OPENCODE_GO, None)?;

        let timeout = Duration::from_secs(arguments.timeout_seconds.max(1));
        let bridge = BrowserBridgeSession::bind(OPENCODE_GO).await?;
        let bridge_info = bridge.info();
        println!("Opening OpenCode Go sign-in in the Windows default browser.");
        println!("After sign-in, load the unpacked extension from:");
        println!("  {}", extension_path().display());
        println!(
            "Then press Connect in the extension. The endpoint and pairing code are pre-filled automatically."
        );
        println!("  {}", bridge_info.pairing_code);
        println!("Bridge endpoint: {}", bridge_info.endpoint);
        let bootstrap_url = Url::parse(&bridge.bootstrap_url())?;
        WindowsDefaultBrowserLauncher.open(&bootstrap_url).await?;
        let bridged = bridge.wait(timeout).await?;
        println!("Browser session received from the explicit extension action.");

        account.browser_profile_id = bridged.profile_id.clone();
        account_store.upsert(&account).await?;

        let material = AccountAuthMaterial {
            cookies: bridged.cookies,
            user_agent: bridged.user_agent,
            ..AccountAuthMaterial::default()
        };
        secure_material_store.save(account.id, &material).await?;
        account
    };

    println!("Account ID: {}", account.id);

    // Account addition must validate the captured session against the same
    // web source that will be used by the eventual tray refresh.  Do not let
    // a local database or an ambient API key make a bad cookie look valid.
    let auth = Arc::new(StoredAuthMaterialProvider::new(
        secure_material_store.clone(),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let runtime = UsageRuntime::from_dependencies(
        account_store.clone(),
        snapshot_store,
        transport,
        auth,
        ProviderRegistryConfig {
            opencode_go_source_mode: OpenCodeGoSourceMode::Web,
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
        if let Some(error) = outcome.error {
            return Err(format!(
                "OpenCode Go usage validation failed: {:?}: {}. The account and browser session were preserved; retry with --resume-account {}.",
                error.code, error.message, account.id
            )
            .into());
        }
        return Err(format!(
            "OpenCode Go usage validation failed: {:?}. The account and browser session were preserved; retry with --resume-account {}.",
            outcome.status, account.id
        )
        .into());
    }

    // Refresh persists verified email/provider identity.  The web adapter
    // also returns the selected workspace as response_account_id; pin it so
    // later refreshes cannot drift to another workspace in the same session.
    let mut persisted = account_store
        .get(account.id)
        .await?
        .ok_or("new OpenCode Go account disappeared from storage")?;
    if let Some(workspace) = outcome
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.response_account_id.as_deref())
        .filter(|value| value.starts_with("wrk_") || value.starts_with("org_"))
    {
        persisted = persisted.with_workspace_id(Some(workspace));
        account_store.upsert(&persisted).await?;
    }

    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!("Account: {} ({})", persisted.label, persisted.email);
    println!(
        "OK | source={} | plan={} | confidence={}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.plan_type.as_deref().unwrap_or("unknown"),
        snapshot.data_confidence
    );
    if let Some(workspace) = persisted.workspace_id.as_deref() {
        println!("Workspace: {workspace}");
    }
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
    if let Some(credits) = snapshot.credits.as_ref() {
        println!(
            "  credits: {}",
            credits
                .balance
                .map(|value| format!("${value:.4}"))
                .unwrap_or_else(|| "unavailable".to_owned())
        );
    }
    if let Some(spend) = snapshot.spend.as_ref() {
        println!(
            "  monthly: used={} | limit={} | remaining={}",
            format_money(spend.monthly_usage),
            format_money(spend.monthly_limit),
            spend
                .remaining_percent()
                .map(|value| format!("{value:.2}%"))
                .unwrap_or_else(|| "unknown".to_owned())
        );
    }
    for diagnostic in &snapshot.source_diagnostics {
        println!(
            "  diagnostic[{}]: {:?} | {}",
            diagnostic.source, diagnostic.code, diagnostic.message
        );
    }
    println!("Database: {}", database_path.display());
    println!("Credential storage: Windows Credential Manager");
    Ok(())
}

fn parse_arguments() -> Result<Arguments, Box<dyn std::error::Error>> {
    let mut arguments = Arguments {
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
            "--timeout-seconds" => {
                arguments.timeout_seconds = values
                    .next()
                    .ok_or("--timeout-seconds requires a number")?
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|_| "--timeout-seconds must be a positive integer")?;
            }
            "--resume-account" => {
                let raw = values
                    .next()
                    .ok_or("--resume-account requires an account UUID")?
                    .to_string_lossy()
                    .into_owned();
                arguments.resume_account = Some(
                    raw.parse::<AccountId>()
                        .map_err(|_| "--resume-account must be a valid account UUID")?,
                );
            }
            "--help" | "-h" => {
                println!(
                    "Usage: codex-usage-opencode-go-probe [--database PATH] [--label LABEL] [--timeout-seconds N] [--resume-account ACCOUNT_ID]\n\nWithout --resume-account, opens the Windows default browser and waits for an explicit Connect click in the local extension. The one-shot loopback bridge stores selected OpenCode cookies in Windows Credential Manager. --resume-account retries usage against a saved account/session without reopening the browser."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn extension_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("opencode-browser-bridge-extension")
}

fn format_money(value: Option<f64>) -> String {
    value
        .map(|value| format!("${value:.4}"))
        .unwrap_or_else(|| "unknown".to_owned())
}
