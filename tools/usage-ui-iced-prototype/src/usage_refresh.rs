use crate::{UsageProvider, dashboard::AccountUsageEntry};

#[derive(Debug, Clone, Copy)]
pub struct RefreshSummary {
    pub attempted: usize,
    pub updated: usize,
    pub not_updated: usize,
}

#[derive(Debug, Clone)]
pub enum RefreshEvent {
    AccountUpdated(AccountUsageEntry),
    Finished(RefreshSummary),
    Failed(String),
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{RefreshEvent, RefreshSummary, UsageProvider};
    use crate::dashboard::{self, AccountUsageEntry};
    use codex_usage_core::{
        accounts::{AccountRecord, AccountStore},
        auth::{
            AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
            AccountBrowserSessionRefresher, AccountOAuthMaterialProvider,
            CompositeAuthMaterialProvider, OAuthCredentialProviderRegistry,
            StoredAuthMaterialProvider,
        },
        oauth_loopback::{CodexOAuthCallbackListenerFactory, LoopbackOAuthCallbackListenerFactory},
        oauth_service::OAuthAuthorizationService,
        providers::{antigravity, openai, registry::ProviderRegistryConfig},
        refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
        runtime::UsageRuntime,
        storage::{SqliteStore, default_accounts_database_path},
        transport::{ReqwestUsageHttpTransport, UsageHttpTransport},
        usage::UsageSnapshotStore,
    };
    use codex_usage_windows_auth::{
        WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
        WindowsDefaultBrowserLauncher, browser_cookies::WindowsBrowserCookieImporter,
    };
    use std::{sync::Arc, time::Duration};

    #[cfg(test)]
    use codex_usage_core::accounts::{CLAUDE, OPENAI};

    const MAX_CONCURRENT_ACCOUNT_REFRESHES: usize = 4;

    pub fn refresh_accounts_for_provider(
        provider: UsageProvider,
        selected_provider_phase: bool,
    ) -> async_channel::Receiver<RefreshEvent> {
        let (sender, receiver) = async_channel::bounded(8);
        let worker_sender = sender.clone();
        let spawn_result = std::thread::Builder::new()
            .name("usage-dashboard-refresh".to_owned())
            .spawn(move || {
                let mut completion = WorkerCompletionGuard {
                    sender: worker_sender.clone(),
                    completed: false,
                };
                let result = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(refresh_accounts_for_provider_inner(
                        provider,
                        selected_provider_phase,
                        worker_sender.clone(),
                    )),
                    Err(error) => Err(format!(
                        "could not start the usage refresh runtime: {error}"
                    )),
                };
                let event = match result {
                    Ok(summary) => RefreshEvent::Finished(summary),
                    Err(error) => RefreshEvent::Failed(error),
                };
                let _ = worker_sender.send_blocking(event);
                completion.completed = true;
            });

        if let Err(error) = spawn_result {
            let _ = sender.send_blocking(RefreshEvent::Failed(format!(
                "could not start the usage refresh worker: {error}"
            )));
        }

        drop(sender);
        receiver
    }

    struct WorkerCompletionGuard {
        sender: async_channel::Sender<RefreshEvent>,
        completed: bool,
    }

    impl Drop for WorkerCompletionGuard {
        fn drop(&mut self) {
            if !self.completed {
                let _ = self.sender.send_blocking(RefreshEvent::Failed(
                    "usage refresh worker ended unexpectedly".to_owned(),
                ));
            }
        }
    }

    async fn refresh_accounts_for_provider_inner(
        provider: UsageProvider,
        selected_provider_phase: bool,
        sender: async_channel::Sender<RefreshEvent>,
    ) -> Result<RefreshSummary, String> {
        let store = Arc::new(
            SqliteStore::open(default_accounts_database_path())
                .map_err(|error| format!("could not open the accounts database: {error}"))?,
        );
        let accounts = store
            .list()
            .await
            .map_err(|error| format!("could not load saved accounts: {error}"))?;
        let targets = accounts_for_phase(accounts, provider, selected_provider_phase);
        let attempted = targets.len();
        let mut updated = 0;
        let mut not_updated = 0;

        if !targets.is_empty() {
            let transport = Arc::new(
                ReqwestUsageHttpTransport::new(Duration::from_secs(45))
                    .map_err(|error| format!("could not create the provider transport: {error}"))?,
            );
            let oauth_store = Arc::new(WindowsCredentialManagerStore);
            let auth_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
            let auth = build_auth_provider(
                Arc::clone(&transport),
                Arc::clone(&oauth_store),
                Arc::clone(&auth_store),
            );
            let session_refresher = Arc::new(
                WindowsBrowserCookieImporter::from_process().map_err(|error| {
                    format!("could not initialize browser session access: {error}")
                })?,
            ) as Arc<dyn AccountBrowserSessionRefresher>;
            let account_store: Arc<dyn AccountStore> = store.clone();
            let snapshot_store: Arc<dyn UsageSnapshotStore> = store.clone();
            let account_auth_store: Arc<dyn AccountAuthMaterialStore> = auth_store.clone();

            let mut targets = targets.into_iter();
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..MAX_CONCURRENT_ACCOUNT_REFRESHES {
                let Some(account) = targets.next() else {
                    break;
                };
                spawn_account_refresh(
                    &mut tasks,
                    account,
                    Arc::clone(&account_store),
                    Arc::clone(&snapshot_store),
                    Arc::clone(&transport),
                    Arc::clone(&auth),
                    Arc::clone(&account_auth_store),
                    Arc::clone(&session_refresher),
                );
            }

            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok(Ok((entry, account_updated, account_not_updated))) => {
                        updated += usize::from(account_updated);
                        not_updated += usize::from(account_not_updated);
                        if sender
                            .send(RefreshEvent::AccountUpdated(entry))
                            .await
                            .is_err()
                        {
                            tasks.abort_all();
                            break;
                        }
                    }
                    Ok(Err(_)) => {
                        not_updated += 1;
                    }
                    Err(_) => {
                        not_updated += 1;
                    }
                }

                if let Some(account) = targets.next() {
                    spawn_account_refresh(
                        &mut tasks,
                        account,
                        Arc::clone(&account_store),
                        Arc::clone(&snapshot_store),
                        Arc::clone(&transport),
                        Arc::clone(&auth),
                        Arc::clone(&account_auth_store),
                        Arc::clone(&session_refresher),
                    );
                }
            }
        }

        Ok(RefreshSummary {
            attempted,
            updated,
            not_updated,
        })
    }

    fn spawn_account_refresh(
        tasks: &mut tokio::task::JoinSet<Result<(AccountUsageEntry, bool, bool), String>>,
        account: AccountRecord,
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        transport: Arc<ReqwestUsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        auth_store: Arc<dyn AccountAuthMaterialStore>,
        session_refresher: Arc<dyn AccountBrowserSessionRefresher>,
    ) {
        tasks.spawn(async move {
            let material = match auth_store.get(account.id).await {
                Ok(material) => material,
                Err(_) => {
                    let entry = account_usage_entry(account, snapshot_store.as_ref()).await?;
                    return Ok((entry, false, true));
                }
            };
            let runtime =
                match UsageRuntime::from_dependencies_with_auth_store_and_session_refresher(
                    account_store,
                    snapshot_store.clone(),
                    transport as Arc<dyn UsageHttpTransport>,
                    auth,
                    auth_store,
                    session_refresher,
                    provider_config_for(&account, material.as_ref()),
                    RefreshCoordinatorConfig {
                        cadence: RefreshCadence::Manual,
                        ..RefreshCoordinatorConfig::default()
                    },
                ) {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let entry = account_usage_entry(account, snapshot_store.as_ref()).await?;
                        return Ok((entry, false, true));
                    }
                };

            let outcome = runtime
                .refresh_account(account.clone(), RefreshReason::Manual)
                .await;
            let (account_updated, account_not_updated) = match outcome.status {
                RefreshStatus::Updated => (true, false),
                RefreshStatus::Failed
                | RefreshStatus::RetainedStale
                | RefreshStatus::Invalidated => (false, true),
                RefreshStatus::Skipped => (false, false),
            };
            let entry = account_usage_entry(account, snapshot_store.as_ref()).await?;
            Ok((entry, account_updated, account_not_updated))
        });
    }

    async fn account_usage_entry(
        account: AccountRecord,
        snapshot_store: &dyn UsageSnapshotStore,
    ) -> Result<AccountUsageEntry, String> {
        let snapshot = snapshot_store
            .get_latest(account.id)
            .await
            .map_err(|error| format!("could not reload saved usage: {error}"))?;
        Ok(AccountUsageEntry { account, snapshot })
    }

    fn accounts_for_phase(
        accounts: Vec<AccountRecord>,
        provider: UsageProvider,
        selected_provider_phase: bool,
    ) -> Vec<AccountRecord> {
        accounts
            .into_iter()
            .filter(|account| {
                dashboard::belongs_to_provider(&account.provider_id, provider)
                    == selected_provider_phase
            })
            .collect()
    }

    fn build_auth_provider(
        transport: Arc<ReqwestUsageHttpTransport>,
        oauth_store: Arc<WindowsCredentialManagerStore>,
        auth_store: Arc<WindowsCredentialManagerAuthMaterialStore>,
    ) -> Arc<dyn AccountAuthMaterialProvider> {
        let codex_authorization = Arc::new(OAuthAuthorizationService::new(
            Arc::clone(&transport),
            Arc::clone(&oauth_store),
            Arc::new(CodexOAuthCallbackListenerFactory),
            Arc::new(WindowsDefaultBrowserLauncher),
        ));
        let codex_oauth = Arc::new(AccountOAuthMaterialProvider {
            authorization: codex_authorization,
            credentials: Arc::clone(&oauth_store),
            providers: Arc::new(OAuthCredentialProviderRegistry::new([
                openai::oauth_definition(),
            ])),
        }) as Arc<dyn AccountAuthMaterialProvider>;

        let antigravity_authorization = Arc::new(OAuthAuthorizationService::new(
            transport,
            Arc::clone(&oauth_store),
            Arc::new(LoopbackOAuthCallbackListenerFactory),
            Arc::new(WindowsDefaultBrowserLauncher),
        ));
        let antigravity_oauth = Arc::new(AccountOAuthMaterialProvider {
            authorization: antigravity_authorization,
            credentials: oauth_store,
            providers: Arc::new(OAuthCredentialProviderRegistry::new([
                antigravity::oauth_definition(),
            ])),
        }) as Arc<dyn AccountAuthMaterialProvider>;
        let stored = Arc::new(StoredAuthMaterialProvider::new(auth_store))
            as Arc<dyn AccountAuthMaterialProvider>;

        Arc::new(CompositeAuthMaterialProvider::new([
            stored,
            codex_oauth,
            antigravity_oauth,
        ]))
    }

    fn provider_config_for(
        account: &AccountRecord,
        material: Option<&AccountAuthMaterial>,
    ) -> ProviderRegistryConfig {
        ProviderRegistryConfig::for_account(account, material)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn account(label: &str, email: &str, provider: &str) -> AccountRecord {
            AccountRecord::create(label, email, None, provider, None).unwrap()
        }

        #[test]
        fn phase_filter_keeps_selected_provider_separate_from_the_rest() {
            let accounts = vec![
                account("router", "router@example.com", "openrouter"),
                account("codex", "codex@example.com", OPENAI),
                account("claude", "claude@example.com", CLAUDE),
            ];

            let priority = accounts_for_phase(accounts.clone(), UsageProvider::Codex, true);
            let remaining = accounts_for_phase(accounts, UsageProvider::Codex, false);

            assert_eq!(priority.len(), 1);
            assert_eq!(priority[0].provider_id, OPENAI);
            assert_eq!(remaining.len(), 2);
            assert!(
                remaining
                    .iter()
                    .all(|account| account.provider_id != OPENAI)
            );
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows::refresh_accounts_for_provider;

#[cfg(not(target_os = "windows"))]
pub fn refresh_accounts_for_provider(
    _provider: UsageProvider,
    _selected_provider_phase: bool,
) -> async_channel::Receiver<RefreshEvent> {
    let (sender, receiver) = async_channel::bounded(1);
    let _ = sender.send_blocking(RefreshEvent::Failed(
        "live usage refresh is only available on Windows".to_owned(),
    ));
    receiver
}
