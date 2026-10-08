//! Add a DeepSeek account from its API key without putting the key in the
//! SQLite database or on the command line.
//!
//! The key is read from `DEEPSEEK_API_KEY`, or from stdin with
//! `--api-key-stdin` / `--credentials-stdin` (first non-empty line). It is
//! written only to the account's Windows Credential Manager entry before the
//! normal runtime reads the balance once.

use std::{
    env,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use usage_monitor_core::{
    accounts::{AccountRecord, AccountStore, DEEPSEEK},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        StoredAuthMaterialProvider,
    },
    providers::registry::ProviderRegistryConfig,
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::ReqwestUsageHttpTransport,
    usage::UsageSnapshotStore,
};
use usage_monitor_windows::WindowsCredentialManagerAuthMaterialStore;

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    force_new: bool,
    key_stdin: bool,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments()?;
    let database_path = arguments
        .database
        .unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let api_key = if arguments.key_stdin {
        read_stdin_key()?
    } else {
        env::var("DEEPSEEK_API_KEY")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or("DEEPSEEK_API_KEY is not set; use a process-scoped environment variable or --api-key-stdin")?
    };

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite.clone();
    let label = arguments
        .label
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("DeepSeek account");
    let account =
        find_or_create_account(account_store.as_ref(), label, arguments.force_new).await?;
    account_store.upsert(&account).await?;

    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let material = AccountAuthMaterial {
        bearer_token: Some(api_key),
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
        .ok_or("DeepSeek account disappeared after its key was saved")?;
    if std::env::var_os("USAGE_MONITOR_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("USAGE_MONITOR_ACCOUNT_REF={account_ref}");
    }

    println!("Database: {}", database_path.display());
    println!("Account: {}", account.label);

    let outcome = runtime
        .refresh_account(account, RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        if let Some(error) = outcome.error {
            return Err(format!(
                "DeepSeek refresh failed: {:?}: {}",
                error.code, error.message
            )
            .into());
        }
        return Err(format!("DeepSeek refresh failed: {:?}", outcome.status).into());
    }

    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!(
        "OK | source={} | confidence={}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.data_confidence
    );
    for metric in &snapshot.metrics {
        match metric.remaining_amount {
            Some(amount) => println!(
                "  {}: {amount:.2} {}",
                metric.name,
                metric.unit.as_deref().unwrap_or("")
            ),
            None => println!("  {}", metric.name),
        }
    }
    Ok(())
}

async fn find_or_create_account(
    store: &dyn AccountStore,
    label: &str,
    force_new: bool,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    if !force_new
        && let Some(account) = store
            .list()
            .await?
            .into_iter()
            .find(|account| account.provider_id == DEEPSEEK && account.label == label)
    {
        return Ok(account);
    }
    Ok(AccountRecord::create(
        label,
        "deepseek@local.invalid",
        None,
        DEEPSEEK,
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
            "--new" => arguments.force_new = true,
            // The desktop app sends every key-based provider's credentials
            // as lines on stdin; DeepSeek has a single key.
            "--api-key-stdin" | "--credentials-stdin" => arguments.key_stdin = true,
            "--help" | "-h" => {
                println!(
                    "Usage: ai-usage-tray-login deepseek [--database PATH] [--label LABEL] [--new] [--api-key-stdin]\n\nReads a DeepSeek API key without printing it, stores it in Windows Credential Manager, and reads the account's balance once."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn read_stdin_key() -> Result<String, Box<dyn std::error::Error>> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "DeepSeek API key from stdin was empty".into())
}
