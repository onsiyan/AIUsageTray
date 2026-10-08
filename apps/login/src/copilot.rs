//! Add a GitHub Copilot account.
//!
//! With `--credentials-stdin` the GitHub token comes from stdin (the desktop
//! app runs GitHub's device flow itself and passes the token on). Otherwise
//! this runs the device flow in the terminal: it prints the code, opens
//! GitHub's device page, and waits for the user to enter the code there.
//!
//! The token is stored only in the account's Windows Credential Manager
//! entry. Accounts are matched by GitHub user id, so signing in again
//! refreshes the token of the existing account instead of adding a copy.

use std::{
    env,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use url::Url;
use usage_monitor_core::{
    accounts::{AccountRecord, AccountStore, COPILOT},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        OAuthBrowserLauncher, StoredAuthMaterialProvider,
    },
    providers::{
        copilot::{self, GitHubIdentity},
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

/// GitHub hides the user's email from a `read:user` token unless it is
/// public, so Copilot accounts carry a placeholder the app never shows.
const PLACEHOLDER_EMAIL: &str = "copilot@local.invalid";

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    token_stdin: bool,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments()?;
    let database_path = arguments
        .database
        .unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let token = if arguments.token_stdin {
        read_stdin_token()?
    } else {
        sign_in_with_device_flow(transport.as_ref()).await?
    };
    let identity = copilot::fetch_identity(transport.as_ref(), &token).await?;

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite.clone();
    let account = find_or_create_account(
        account_store.as_ref(),
        &identity,
        arguments.label.as_deref(),
    )
    .await?;
    account_store.upsert(&account).await?;

    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let material = AccountAuthMaterial {
        bearer_token: Some(token),
        oauth_scopes: vec!["read:user".to_owned()],
        ..AccountAuthMaterial::default()
    };
    secure_material_store.save(account.id, &material).await?;
    let auth = Arc::new(StoredAuthMaterialProvider::new(
        secure_material_store.clone(),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    println!("Credential storage: Windows Credential Manager");
    let runtime = UsageRuntime::from_dependencies_with_auth_store(
        account_store,
        snapshot_store,
        transport,
        auth,
        secure_material_store as Arc<dyn AccountAuthMaterialStore>,
        ProviderRegistryConfig::default(),
        RefreshCoordinatorConfig {
            cadence: RefreshCadence::Manual,
            ..RefreshCoordinatorConfig::default()
        },
    )?;
    let account = sqlite
        .get(account.id)
        .await?
        .ok_or("Copilot account disappeared after its token was saved")?;
    if std::env::var_os("USAGE_MONITOR_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("USAGE_MONITOR_ACCOUNT_REF={account_ref}");
    }

    println!("Database: {}", database_path.display());
    println!(
        "Account: {} (GitHub user {})",
        account.label, identity.login
    );

    let outcome = runtime
        .refresh_account(account, RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        if let Some(error) = outcome.error {
            return Err(format!(
                "Copilot refresh failed: {:?}: {}",
                error.code, error.message
            )
            .into());
        }
        return Err(format!("Copilot refresh failed: {:?}", outcome.status).into());
    }

    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!(
        "OK | plan={} | source={}",
        snapshot.plan_type.as_deref().unwrap_or("unknown"),
        snapshot.source.as_deref().unwrap_or("unknown"),
    );
    for window in snapshot.primary.iter().chain(snapshot.secondary.iter()) {
        println!("  {}: {:.0}% used", window.name, window.used_percent);
    }
    for metric in &snapshot.metrics {
        println!("  {}", metric.name);
    }
    Ok(())
}

async fn sign_in_with_device_flow(
    transport: &ReqwestUsageHttpTransport,
) -> Result<String, Box<dyn std::error::Error>> {
    let code = copilot::request_device_code(transport).await?;
    println!("Enter this code on GitHub: {}", code.user_code);
    println!("GitHub device page: {}", code.verification_uri);
    if let Ok(url) = Url::parse(&code.verification_uri)
        && let Err(error) = WindowsDefaultBrowserLauncher.open(&url).await
    {
        println!("Open the page yourself ({error}).");
    }
    println!("Waiting for the code to be entered…");
    Ok(copilot::poll_for_token(transport, &code).await?)
}

async fn find_or_create_account(
    store: &dyn AccountStore,
    identity: &GitHubIdentity,
    label: Option<&str>,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    let user_id = identity.id.to_string();
    if let Some(account) = store.list().await?.into_iter().find(|account| {
        account.provider_id == COPILOT && account.provider_account_id.as_deref() == Some(&user_id)
    }) {
        return Ok(account.with_identity(None, Some(&user_id))?);
    }
    Ok(AccountRecord::create(
        label
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(&identity.login),
        PLACEHOLDER_EMAIL,
        Some(user_id),
        COPILOT,
        None,
    )?)
}

fn parse_arguments() -> Result<Arguments, Box<dyn std::error::Error>> {
    let mut arguments = Arguments::default();
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
            // Accounts are matched by GitHub user, never duplicated.
            "--new" => {}
            "--api-key-stdin" | "--credentials-stdin" => arguments.token_stdin = true,
            "--help" | "-h" => {
                println!(
                    "Usage: ai-usage-tray-login copilot [--database PATH] [--label LABEL] [--credentials-stdin]\n\nSigns in to GitHub with a device code (or reads a GitHub token from stdin), stores the token in Windows Credential Manager, and reads the account's Copilot quotas once."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn read_stdin_token() -> Result<String, Box<dyn std::error::Error>> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "GitHub token from stdin was empty".into())
}
