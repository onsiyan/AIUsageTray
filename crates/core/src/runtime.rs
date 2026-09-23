//! Runtime composition for the backend.
//!
//! This is intentionally a small composition root: storage, shared transport,
//! authentication material, provider registry, and refresh coordinator are
//! assembled once and then handed to the host. No UI or provider-specific
//! scheduling belongs here.

use crate::{
    accounts::{AccountRecord, AccountStore},
    auth::{AccountAuthMaterialProvider, AccountAuthMaterialStore, AccountBrowserSessionRefresher},
    claude_oauth::ClaudeOAuthRefreshingAuthMaterialProvider,
    providers::registry::{ProviderRegistry, ProviderRegistryConfig, ProviderRegistryError},
    refresh::{
        RefreshCoordinatorConfig, RefreshCoordinatorError, RefreshOutcome, RefreshReason,
        UsageRefreshCoordinator,
    },
    storage::SqliteStore,
    transport::UsageHttpTransport,
    usage::{StorageError, UsageSnapshotStore},
};
use std::{path::Path, sync::Arc};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeBootstrapError {
    #[error("runtime storage initialization failed: {0}")]
    Storage(#[from] StorageError),
    #[error("runtime provider initialization failed: {0}")]
    Providers(#[from] ProviderRegistryError),
}

/// The backend object a future tray host can own for its entire process
/// lifetime. The same account and snapshot stores are used by every adapter
/// and by the coordinator.
pub struct UsageRuntime {
    account_store: Arc<dyn AccountStore>,
    snapshot_store: Arc<dyn UsageSnapshotStore>,
    providers: Arc<ProviderRegistry>,
    coordinator: Arc<UsageRefreshCoordinator>,
}

impl UsageRuntime {
    pub fn new(
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        providers: Arc<ProviderRegistry>,
        refresh_config: RefreshCoordinatorConfig,
    ) -> Arc<Self> {
        let coordinator = UsageRefreshCoordinator::new_with_registry(
            Arc::clone(&account_store),
            Arc::clone(&snapshot_store),
            &providers,
            refresh_config,
        );
        Arc::new(Self {
            account_store,
            snapshot_store,
            providers,
            coordinator,
        })
    }

    /// Composition root for hosts that already own their transport and auth
    /// implementations (including Windows Credential Manager integrations).
    pub fn from_dependencies(
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        provider_config: ProviderRegistryConfig,
        refresh_config: RefreshCoordinatorConfig,
    ) -> Result<Arc<Self>, RuntimeBootstrapError> {
        let providers = Arc::new(ProviderRegistry::from_dependencies(
            transport,
            auth,
            provider_config,
        )?);
        Ok(Self::new(
            account_store,
            snapshot_store,
            providers,
            refresh_config,
        ))
    }

    /// Composition root for hosts that also provide the secure per-account
    /// auth store. Claude OAuth refresh is installed here so a tray host does
    /// not need to remember provider-specific refresh plumbing; all other
    /// providers continue through the injected auth chain unchanged.
    pub fn from_dependencies_with_auth_store(
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        auth_store: Arc<dyn AccountAuthMaterialStore>,
        provider_config: ProviderRegistryConfig,
        refresh_config: RefreshCoordinatorConfig,
    ) -> Result<Arc<Self>, RuntimeBootstrapError> {
        Self::from_dependencies_with_auth_services(
            account_store,
            snapshot_store,
            transport,
            auth,
            auth_store,
            None,
            provider_config,
            refresh_config,
        )
    }

    /// Composition root for hosts that also support non-interactive browser
    /// session re-import from each account's saved browser/profile binding.
    pub fn from_dependencies_with_auth_store_and_session_refresher(
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        auth_store: Arc<dyn AccountAuthMaterialStore>,
        session_refresher: Arc<dyn AccountBrowserSessionRefresher>,
        provider_config: ProviderRegistryConfig,
        refresh_config: RefreshCoordinatorConfig,
    ) -> Result<Arc<Self>, RuntimeBootstrapError> {
        Self::from_dependencies_with_auth_services(
            account_store,
            snapshot_store,
            transport,
            auth,
            auth_store,
            Some(session_refresher),
            provider_config,
            refresh_config,
        )
    }

    fn from_dependencies_with_auth_services(
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        auth_store: Arc<dyn AccountAuthMaterialStore>,
        session_refresher: Option<Arc<dyn AccountBrowserSessionRefresher>>,
        provider_config: ProviderRegistryConfig,
        refresh_config: RefreshCoordinatorConfig,
    ) -> Result<Arc<Self>, RuntimeBootstrapError> {
        let auth = Arc::new(ClaudeOAuthRefreshingAuthMaterialProvider::new(
            auth,
            Arc::clone(&auth_store),
            Arc::clone(&transport),
        )) as Arc<dyn AccountAuthMaterialProvider>;
        let providers = Arc::new(match session_refresher {
            Some(session_refresher) => {
                ProviderRegistry::from_dependencies_with_auth_store_and_session_refresher(
                    Arc::clone(&transport),
                    auth,
                    auth_store,
                    session_refresher,
                    provider_config,
                )?
            }
            None => ProviderRegistry::from_dependencies_with_auth_store(
                Arc::clone(&transport),
                auth,
                auth_store,
                provider_config,
            )?,
        });
        Ok(Self::new(
            account_store,
            snapshot_store,
            providers,
            refresh_config,
        ))
    }

    /// Convenience composition root for the durable SQLite-backed runtime.
    /// Credential/token policy remains injected through `auth`; this method
    /// never stores access tokens in SQLite.
    pub fn from_sqlite_path(
        path: impl AsRef<Path>,
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        provider_config: ProviderRegistryConfig,
        refresh_config: RefreshCoordinatorConfig,
    ) -> Result<Arc<Self>, RuntimeBootstrapError> {
        let sqlite_store = Arc::new(SqliteStore::open(path)?);
        let account_store: Arc<dyn AccountStore> = sqlite_store.clone();
        let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite_store;
        Self::from_dependencies(
            account_store,
            snapshot_store,
            transport,
            auth,
            provider_config,
            refresh_config,
        )
    }

    /// SQLite convenience constructor with automatic Claude OAuth rotation
    /// backed by the supplied secure auth-material store.
    pub fn from_sqlite_path_with_auth_store(
        path: impl AsRef<Path>,
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        auth_store: Arc<dyn AccountAuthMaterialStore>,
        provider_config: ProviderRegistryConfig,
        refresh_config: RefreshCoordinatorConfig,
    ) -> Result<Arc<Self>, RuntimeBootstrapError> {
        let sqlite_store = Arc::new(SqliteStore::open(path)?);
        let account_store: Arc<dyn AccountStore> = sqlite_store.clone();
        let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite_store;
        Self::from_dependencies_with_auth_store(
            account_store,
            snapshot_store,
            transport,
            auth,
            auth_store,
            provider_config,
            refresh_config,
        )
    }

    pub fn account_store(&self) -> &Arc<dyn AccountStore> {
        &self.account_store
    }

    pub fn snapshot_store(&self) -> &Arc<dyn UsageSnapshotStore> {
        &self.snapshot_store
    }

    pub fn providers(&self) -> &ProviderRegistry {
        &self.providers
    }

    pub fn coordinator(&self) -> &Arc<UsageRefreshCoordinator> {
        &self.coordinator
    }

    pub async fn refresh_now(&self) -> Result<Vec<RefreshOutcome>, RefreshCoordinatorError> {
        self.coordinator.refresh_all(RefreshReason::Manual).await
    }

    pub async fn refresh_account(
        &self,
        account: AccountRecord,
        reason: RefreshReason,
    ) -> RefreshOutcome {
        self.coordinator.refresh_account(account, reason).await
    }
}
