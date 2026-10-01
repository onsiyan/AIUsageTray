//! Add an OpenRouter account from its API key without putting secrets in the
//! SQLite database or on the command line.
//!
//! The primary key is read from `OPENROUTER_API_KEY` (or from stdin with
//! `--api-key-stdin`) and the optional management key from
//! `OPENROUTER_MANAGEMENT_API_KEY` (or the second stdin line with
//! `--credentials-stdin`). Both are written only to the account's Windows
//! Credential Manager entry before the normal runtime refreshes it.

use std::{
    env,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use usage_monitor_core::{
    accounts::{AccountRecord, AccountStore, OPENROUTER},
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
    api_key_stdin: bool,
    credentials_stdin: bool,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments()?;
    let database_path = arguments
        .database
        .unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let (primary_key, management_key) = if arguments.credentials_stdin {
        read_stdin_credentials()?
    } else {
        let primary_key = if arguments.api_key_stdin {
            read_stdin_secret("OpenRouter API key")?
        } else {
            read_environment_secret("OPENROUTER_API_KEY")?
        };
        (
            primary_key,
            read_optional_environment_secret("OPENROUTER_MANAGEMENT_API_KEY"),
        )
    };
    let has_management_key = management_key.is_some();

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite.clone();
    let label = arguments
        .label
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("OpenRouter account");
    let account =
        find_or_create_account(account_store.as_ref(), label, arguments.force_new).await?;
    account_store.upsert(&account).await?;

    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let material = AccountAuthMaterial {
        bearer_token: Some(primary_key),
        secondary_bearer_token: management_key,
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
        .ok_or("OpenRouter account disappeared after its key was saved")?;
    announce_cli_account_reference(&account);

    println!("Database: {}", database_path.display());
    println!("Account: {}", account.label);
    if has_management_key {
        println!("Management API key: provided for optional Activity only");
    }

    let outcome = runtime
        .refresh_account(account, RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        if let Some(error) = outcome.error {
            return Err(format!(
                "OpenRouter refresh failed: {:?}: {}",
                error.code, error.message
            )
            .into());
        }
        return Err(format!("OpenRouter refresh failed: {:?}", outcome.status).into());
    }

    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!(
        "OK | source={} | plan={} | confidence={}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.plan_type.as_deref().unwrap_or("unknown"),
        snapshot.data_confidence
    );
    if let Some(window) = snapshot.primary.as_ref() {
        println!(
            "  key limit: {:.2}% remaining | reset {}",
            window.remaining_percent(),
            window
                .reset_at_utc
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "none".to_owned())
        );
    } else {
        println!("  key limit: no configured limit or unavailable");
    }
    if let Some(spend) = snapshot.spend.as_ref() {
        println!(
            "  monthly spend: used={} | limit={} | remaining={}",
            format_number(spend.monthly_usage),
            format_number(spend.monthly_limit),
            format_percent(spend.remaining_percent())
        );
    }
    if let Some(credits) = snapshot.credits.as_ref() {
        println!(
            "  credits: balance={} | available={}",
            format_number(credits.balance),
            format_bool(credits.credits_available)
        );
    } else {
        println!("  credits: unavailable (the selected key may not have management access)");
    }

    for metric in snapshot.metrics.iter().filter(|metric| {
        metric.key.starts_with("key.") || metric.key == "free-model.daily-requests"
    }) {
        println!(
            "  {}: used={} | remaining={} | reset {}",
            metric.name,
            format_metric_amount(metric.used_amount, metric.unit.as_deref()),
            format_metric_amount(metric.remaining_amount, metric.unit.as_deref()),
            metric
                .reset_at_utc
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "none".to_owned())
        );
    }
    let activity_count = snapshot
        .metrics
        .iter()
        .filter(|metric| metric.key.starts_with("activity.") && metric.key != "activity.summary")
        .count();
    println!("  activity rows: {activity_count}");
    if let Some(summary) = snapshot
        .metrics
        .iter()
        .find(|metric| metric.key == "activity.summary")
    {
        println!(
            "  activity summary: spend={} | requests={} | tokens={} | models={}",
            format_metric_amount(summary.used_amount, summary.unit.as_deref()),
            summary
                .metadata
                .get("requests")
                .map(String::as_str)
                .unwrap_or("unknown"),
            summary
                .metadata
                .get("total_tokens")
                .map(String::as_str)
                .unwrap_or("unknown"),
            summary
                .metadata
                .get("model_count")
                .map(String::as_str)
                .unwrap_or("unknown")
        );
    }
    for diagnostic in &snapshot.source_diagnostics {
        println!(
            "  diagnostic[{}]: {:?} | {}",
            diagnostic.source, diagnostic.code, diagnostic.message
        );
    }
    Ok(())
}

fn announce_cli_account_reference(account: &AccountRecord) {
    if std::env::var_os("USAGE_MONITOR_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("USAGE_MONITOR_ACCOUNT_REF={account_ref}");
    }
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
            .find(|account| account.provider_id == OPENROUTER && account.label == label)
    {
        return Ok(account);
    }
    Ok(AccountRecord::create(
        label,
        "openrouter@local.invalid",
        None,
        OPENROUTER,
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
            "--api-key-stdin" => arguments.api_key_stdin = true,
            "--credentials-stdin" => arguments.credentials_stdin = true,
            "--help" | "-h" => {
                println!(
                    "Usage: usage-monitor-login openrouter [--database PATH] [--label LABEL] [--new] [--api-key-stdin | --credentials-stdin]\n\nReads OpenRouter credentials without printing them, stores them in Windows Credential Manager, and runs the account's usage probe. Set OPENROUTER_MANAGEMENT_API_KEY optionally for Activity. With --credentials-stdin, line 1 is the primary key and line 2 is the optional management key."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn read_environment_secret(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    read_optional_environment_secret(name).ok_or_else(|| {
        format!("{name} is not set; use a process-scoped environment variable or --api-key-stdin")
            .into()
    })
}

fn read_optional_environment_secret(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_stdin_secret(label: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(format!("{label} from stdin was empty").into());
    }
    Ok(value)
}

fn read_stdin_credentials() -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    let mut lines = value.lines().map(str::trim).filter(|line| !line.is_empty());
    let primary = lines
        .next()
        .ok_or("primary OpenRouter API key from stdin was empty")?;
    let management = lines.next().map(str::to_owned);
    Ok((primary.to_owned(), management))
}

fn format_number(value: Option<f64>) -> String {
    value
        .map(|value| format!("${value:.4}"))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn format_metric_amount(value: Option<f64>, unit: Option<&str>) -> String {
    match unit {
        Some("requests") => value
            .map(|value| format!("{value:.0}"))
            .unwrap_or_else(|| "unknown".to_owned()),
        _ => format_number(value),
    }
}

fn format_percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}%"))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn format_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}
