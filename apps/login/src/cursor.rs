//! Add a Cursor account.
//!
//! Without options this takes the session of the Cursor app signed in on
//! this computer. With `--credentials-stdin` it reads a session pasted from
//! cursor.com instead: the `WorkosCursorSessionToken` cookie, its value, or
//! a whole `Cookie:` header.
//!
//! The session is confirmed with cursor.com, then stored only in the
//! account's Windows Credential Manager entry. Accounts are matched by
//! Cursor user id, so adding the same account again refreshes its session.

use chrono::Utc;
use std::{
    env,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use usage_monitor_core::{
    accounts::{AccountRecord, AccountStore, CURSOR},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        StoredAuthMaterialProvider,
    },
    providers::{
        cursor::{self, CursorIdentity, CursorSession},
        registry::ProviderRegistryConfig,
    },
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
    session_stdin: bool,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments()?;
    let database_path = arguments
        .database
        .unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let session = if arguments.session_stdin {
        let token = cursor::access_token_from_input(&read_stdin()?)
            .ok_or("That is not a Cursor session. Copy the WorkosCursorSessionToken cookie from cursor.com.")?;
        CursorSession::from_access_token(&token)?
    } else {
        app_session()?
    };
    if !session.is_usable(Utc::now()) {
        return Err("This Cursor session has expired. Sign in to Cursor again.".into());
    }

    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let identity = cursor::fetch_identity(transport.as_ref(), &session).await?;

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
        bearer_token: Some(session.access_token.clone()),
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
        .ok_or("Cursor account disappeared after its session was saved")?;
    if std::env::var_os("USAGE_MONITOR_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("USAGE_MONITOR_ACCOUNT_REF={account_ref}");
    }

    println!("Database: {}", database_path.display());
    println!(
        "Account: {} (Cursor user {})",
        account.label, identity.user_id
    );

    let outcome = runtime
        .refresh_account(account, RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        if let Some(error) = outcome.error {
            return Err(
                format!("Cursor refresh failed: {:?}: {}", error.code, error.message).into(),
            );
        }
        return Err(format!("Cursor refresh failed: {:?}", outcome.status).into());
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

/// The session of the Cursor app signed in on this computer.
fn app_session() -> Result<CursorSession, Box<dyn std::error::Error>> {
    let database =
        cursor::default_app_database().ok_or("Cursor's settings folder was not found")?;
    let token = cursor::read_app_token(&database)?.ok_or(
        "The Cursor app is not signed in on this computer. Sign in to Cursor, or paste the WorkosCursorSessionToken cookie from cursor.com.",
    )?;
    Ok(CursorSession::from_access_token(&token)?)
}

async fn find_or_create_account(
    store: &dyn AccountStore,
    identity: &CursorIdentity,
    label: Option<&str>,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    let email = identity
        .email
        .as_deref()
        .ok_or("Cursor did not say which account this session belongs to")?;
    if let Some(account) = store.list().await?.into_iter().find(|account| {
        account.provider_id == CURSOR
            && account.provider_account_id.as_deref() == Some(identity.user_id.as_str())
    }) {
        return Ok(account.with_identity(Some(email), Some(&identity.user_id))?);
    }
    Ok(AccountRecord::create(
        label
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(email),
        email,
        Some(identity.user_id.clone()),
        CURSOR,
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
            // Accounts are matched by Cursor user, never duplicated.
            "--new" => {}
            "--api-key-stdin" | "--credentials-stdin" => arguments.session_stdin = true,
            "--help" | "-h" => {
                println!(
                    "Usage: ai-usage-tray-login cursor [--database PATH] [--label LABEL] [--credentials-stdin]\n\nTakes the session of the Cursor app signed in on this computer (or, with --credentials-stdin, a WorkosCursorSessionToken cookie from cursor.com), stores it in Windows Credential Manager, and reads the account's Cursor usage once."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn read_stdin() -> Result<String, Box<dyn std::error::Error>> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    if value.trim().is_empty() {
        return Err("The Cursor session from stdin was empty".into());
    }
    Ok(value)
}
