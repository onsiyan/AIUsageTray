use codex_usage_core::{
    accounts::{ANTIGRAVITY, AccountRecord, AccountStore},
    auth::{
        AccountAuthMaterialProvider, AccountOAuthMaterialProvider, CompositeAuthMaterialProvider,
        OAuthCredentialProviderRegistry, StoredAuthMaterialProvider,
    },
    auth_sources::{EnvironmentAuthMaterialProvider, LocalFileAuthMaterialProvider},
    oauth_loopback::LoopbackOAuthCallbackListenerFactory,
    oauth_service::OAuthAuthorizationService,
    providers::antigravity::oauth_definition,
    providers::registry::ProviderRegistryConfig,
    refresh::{RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::SqliteStore,
    transport::{ReqwestUsageHttpTransport, UsageHttpTransport},
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    WindowsDefaultBrowserLauncher,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

type Transport = ReqwestUsageHttpTransport;
type CredentialStore = WindowsCredentialManagerStore;
type CallbackFactory = LoopbackOAuthCallbackListenerFactory;
type Browser = WindowsDefaultBrowserLauncher;
type Authorization =
    OAuthAuthorizationService<Transport, CredentialStore, CallbackFactory, Browser>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let force_new_account =
        std::env::args().any(|argument| matches!(argument.as_str(), "--new" | "--login-new"));
    let data_root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("CodexUsageMonitor-Rust"))
        .join("CodexUsageMonitor-Rust");
    std::fs::create_dir_all(&data_root)?;
    let database_path = data_root.join("accounts.db");
    let account_store = Arc::new(SqliteStore::open(&database_path)?);
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let credential_store = Arc::new(WindowsCredentialManagerStore);
    let provider = oauth_definition();

    let account = if let Some(account) = if !force_new_account {
        account_store
            .list()
            .await?
            .into_iter()
            .find(|account| account.provider_id == ANTIGRAVITY)
    } else {
        None
    } {
        println!("Reusing the Rust-isolated Antigravity account.");
        account
    } else {
        if force_new_account {
            println!("Opening a new isolated Antigravity account login.");
        }
        println!("The default browser will open for the Rust OAuth test.");
        println!("Complete login and consent, then return to this terminal.");
        let authorization = Arc::new(create_authorization(
            transport.clone(),
            credential_store.clone(),
        ));
        let pending = AccountRecord::create(
            "Antigravity account",
            "pending@local.invalid",
            None,
            ANTIGRAVITY,
            None,
        )?;
        let login = authorization
            .login(pending.id, &provider, Duration::from_secs(300))
            .await?;
        let identity = login.identity.as_ref();
        let account = pending.with_identity(
            identity.and_then(|identity| identity.email.as_deref()),
            identity.and_then(|identity| identity.provider_account_id.as_deref()),
        )?;
        account_store.upsert(&account).await?;
        println!("Rust OAuth login succeeded.");
        account
    };

    probe_and_print(
        &database_path,
        transport.clone(),
        credential_store.clone(),
        &provider,
        &account,
    )
    .await?;

    println!("Restarting Rust authorization service to verify Credential Manager reload...");
    probe_and_print(
        &database_path,
        transport,
        credential_store,
        &provider,
        &account,
    )
    .await?;

    println!("Rust OAuth probe completed successfully.");
    println!("Rust-local database: {}", database_path.display());
    Ok(())
}

fn create_authorization(
    transport: Arc<Transport>,
    credential_store: Arc<CredentialStore>,
) -> Authorization {
    OAuthAuthorizationService::new(
        transport,
        credential_store,
        Arc::new(LoopbackOAuthCallbackListenerFactory),
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
    let oauth_material = Arc::new(AccountOAuthMaterialProvider {
        authorization,
        credentials: credential_store,
        providers: registry,
    }) as Arc<dyn AccountAuthMaterialProvider>;
    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let imported_material = Arc::new(StoredAuthMaterialProvider::new(
        secure_material_store.clone(),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let local_file_material = Arc::new(LocalFileAuthMaterialProvider::from_process(account.id))
        as Arc<dyn AccountAuthMaterialProvider>;
    let environment_material = Arc::new(EnvironmentAuthMaterialProvider::from_process(account.id))
        as Arc<dyn AccountAuthMaterialProvider>;
    let base_auth_material = Arc::new(CompositeAuthMaterialProvider::new([
        imported_material,
        oauth_material,
        local_file_material,
        environment_material,
    ])) as Arc<dyn AccountAuthMaterialProvider>;
    let runtime = UsageRuntime::from_sqlite_path_with_auth_store(
        database_path,
        transport as Arc<dyn UsageHttpTransport>,
        base_auth_material,
        secure_material_store,
        ProviderRegistryConfig::default(),
        RefreshCoordinatorConfig {
            cadence: codex_usage_core::refresh::RefreshCadence::Manual,
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
    println!(
        "Plan: {}",
        snapshot.plan_type.as_deref().unwrap_or("unknown")
    );
    println!(
        "Source: {} | quota windows: {} | metrics/models: {}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.additional_windows.len(),
        snapshot.metrics.len()
    );
    for metric in &snapshot.metrics {
        let remaining = metric
            .remaining_percent()
            .map(|value| format!("{value:.2}% remaining"))
            .or_else(|| {
                metric
                    .remaining_amount
                    .map(|value| format!("{value:.2}% remaining"))
            })
            .unwrap_or_else(|| "unknown remaining quota".to_owned());
        println!("{}: {remaining}", metric.name);
    }
    Ok(())
}
