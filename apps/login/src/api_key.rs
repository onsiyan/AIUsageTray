//! Add an account of a provider that is read with an API key, without
//! putting the key in the SQLite database or on the command line.
//!
//! The key comes from the provider's environment variable, or from stdin
//! with `--api-key-stdin` / `--credentials-stdin` (first non-empty line). A
//! provider that also needs a second value (an id that goes with the key)
//! takes it from the next stdin line or its own variable. Both are written
//! only to the account's Windows Credential Manager entry before the normal
//! runtime reads the account's usage once. Kimi Code and z.ai keys belong to
//! one region; the region that accepts the key is found here and stored in
//! the second value's place. Xiaomi MiMo is read with a console session
//! cookie instead of a key; only its MiMo cookies are kept.

use std::{
    env,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use usage_monitor_core::{
    accounts::{AccountRecord, AccountStore, KIMI, MIMO, MINIMAX, XAI, ZAI},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        StoredAuthMaterialProvider,
    },
    providers::{kimi, mimo, minimax, registry::ProviderRegistryConfig, xai, zai},
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::ReqwestUsageHttpTransport,
    usage::UsageSnapshotStore,
};
use usage_monitor_windows::WindowsCredentialManagerAuthMaterialStore;

pub struct ApiKeyProvider {
    /// The `usage-monitor-login` subcommand.
    pub command: &'static str,
    pub provider_id: &'static str,
    pub name: &'static str,
    pub key_variable: &'static str,
    /// A required value that goes with the key, kept as the secondary token.
    pub second: Option<SecondValue>,
    /// Find the region whose host accepts the key and keep it as the
    /// secondary token.
    pub region: Option<Region>,
    /// Rewrites what was pasted into what is stored, or rejects it.
    pub key_format: Option<KeyFormat>,
}

pub struct KeyFormat {
    pub normalize: fn(&str) -> Option<String>,
    pub error: &'static str,
}

#[derive(Clone, Copy)]
pub enum Region {
    Kimi,
    MiniMax,
    Zai,
}

pub struct SecondValue {
    pub variable: &'static str,
    pub name: &'static str,
    /// Rejects a value that cannot be right before anything is saved.
    pub is_valid: fn(&str) -> bool,
}

pub const KIMI_CODE: ApiKeyProvider = ApiKeyProvider {
    command: "kimi",
    provider_id: KIMI,
    name: "Kimi Code",
    key_variable: "KIMI_CODE_API_KEY",
    second: None,
    region: Some(Region::Kimi),
    key_format: None,
};

pub const ZAI_CODING_PLAN: ApiKeyProvider = ApiKeyProvider {
    command: "zai",
    provider_id: ZAI,
    name: "z.ai",
    key_variable: "Z_AI_API_KEY",
    second: None,
    region: Some(Region::Zai),
    key_format: None,
};

pub const MINIMAX_CODING_PLAN: ApiKeyProvider = ApiKeyProvider {
    command: "minimax",
    provider_id: MINIMAX,
    name: "MiniMax",
    key_variable: "MINIMAX_CODING_API_KEY",
    second: None,
    region: Some(Region::MiniMax),
    key_format: None,
};

pub const XIAOMI_MIMO: ApiKeyProvider = ApiKeyProvider {
    command: "mimo",
    provider_id: MIMO,
    name: "Xiaomi MiMo",
    key_variable: "MIMO_COOKIE",
    second: None,
    region: None,
    key_format: Some(KeyFormat {
        normalize: mimo::cookie_header,
        error: "The Xiaomi MiMo cookie needs api-platform_serviceToken and userId; copy the Cookie header from platform.xiaomimimo.com while signed in",
    }),
};

pub const XAI_MANAGEMENT: ApiKeyProvider = ApiKeyProvider {
    command: "xai",
    provider_id: XAI,
    name: "xAI",
    key_variable: "XAI_MANAGEMENT_API_KEY",
    second: Some(SecondValue {
        variable: "XAI_TEAM_ID",
        name: "team ID",
        is_valid: xai::valid_team_id,
    }),
    region: None,
    key_format: None,
};

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    force_new: bool,
    stdin: bool,
}

pub async fn run(provider: &ApiKeyProvider) -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments(provider)?;
    let database_path = arguments
        .database
        .unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let (api_key, mut second) = if arguments.stdin {
        read_stdin_values(provider)?
    } else {
        let variable = |name: &str| {
            env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let api_key = variable(provider.key_variable).ok_or_else(|| {
            format!(
                "{} is not set; use a process-scoped environment variable or --api-key-stdin",
                provider.key_variable
            )
        })?;
        let second =
            match &provider.second {
                Some(second) => Some(variable(second.variable).ok_or_else(|| {
                    format!("{} is not set; it goes with the key", second.variable)
                })?),
                None => None,
            };
        (api_key, second)
    };

    if let (Some(rule), Some(value)) = (&provider.second, &second)
        && !(rule.is_valid)(value)
    {
        return Err(format!("The {} `{value}` is not valid", rule.name).into());
    }

    let api_key = match &provider.key_format {
        Some(format) => (format.normalize)(&api_key).ok_or(format.error)?,
        None => api_key,
    };

    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    match provider.region {
        Some(Region::Kimi) => {
            let region = kimi::detect_region(transport.as_ref(), &api_key).await?;
            second = region.stored().map(str::to_owned);
        }
        Some(Region::MiniMax) => {
            let region = minimax::detect_region(transport.as_ref(), &api_key).await?;
            second = region.stored().map(str::to_owned);
        }
        Some(Region::Zai) => {
            let region = zai::detect_region(transport.as_ref(), &api_key).await?;
            second = region.stored().map(str::to_owned);
        }
        None => {}
    }

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite.clone();
    let default_label = format!("{} account", provider.name);
    let label = arguments
        .label
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&default_label);
    let account = find_or_create_account(
        account_store.as_ref(),
        provider.provider_id,
        label,
        arguments.force_new,
    )
    .await?;
    account_store.upsert(&account).await?;

    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let material = AccountAuthMaterial {
        bearer_token: Some(api_key),
        secondary_bearer_token: second,
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
    let account = sqlite.get(account.id).await?.ok_or_else(|| {
        format!(
            "{} account disappeared after its key was saved",
            provider.name
        )
    })?;
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
                "{} refresh failed: {:?}: {}",
                provider.name, error.code, error.message
            )
            .into());
        }
        return Err(format!("{} refresh failed: {:?}", provider.name, outcome.status).into());
    }

    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!(
        "OK | plan={} | source={}",
        snapshot.plan_type.as_deref().unwrap_or("unknown"),
        snapshot.source.as_deref().unwrap_or("unknown"),
    );
    for window in snapshot.all_rate_windows() {
        println!("  {}: {:.0}% used", window.name, window.used_percent);
    }
    for metric in &snapshot.metrics {
        println!("  {}", metric.name);
    }
    Ok(())
}

/// Each explicit add gets its own credential slot; re-adding under the same
/// label replaces that account's key.
async fn find_or_create_account(
    store: &dyn AccountStore,
    provider_id: &str,
    label: &str,
    force_new: bool,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    if !force_new
        && let Some(account) = store
            .list()
            .await?
            .into_iter()
            .find(|account| account.provider_id == provider_id && account.label == label)
    {
        return Ok(account);
    }
    Ok(AccountRecord::create(
        label,
        format!("{provider_id}@local.invalid"),
        None,
        provider_id,
        None,
    )?)
}

fn parse_arguments(provider: &ApiKeyProvider) -> Result<Arguments, Box<dyn std::error::Error>> {
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
            "--api-key-stdin" | "--credentials-stdin" => arguments.stdin = true,
            "--help" | "-h" => {
                let second = provider.second.as_ref().map_or(String::new(), |second| {
                    format!(
                        " The {} comes from the second stdin line or {}.",
                        second.name, second.variable
                    )
                });
                println!(
                    "Usage: usage-monitor-login {} [--database PATH] [--label LABEL] [--new] [--api-key-stdin]\n\nReads a {} API key from {} or stdin without printing it, stores it in Windows Credential Manager, and reads the account's usage once.{second}",
                    provider.command, provider.name, provider.key_variable
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn read_stdin_values(
    provider: &ApiKeyProvider,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    let mut lines = value.lines().map(str::trim);
    let api_key = lines
        .by_ref()
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{} API key from stdin was empty", provider.name))?;
    let second = match &provider.second {
        Some(second) => Some(
            lines
                .find(|line| !line.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("The {} is missing from stdin", second.name))?,
        ),
        None => None,
    };
    Ok((api_key, second))
}
