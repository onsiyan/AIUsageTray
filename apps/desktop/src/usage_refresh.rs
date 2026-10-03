use crate::{UsageProvider, dashboard::AccountUsageEntry};

#[derive(Debug, Clone, Copy)]
pub struct RefreshSummary {
    pub attempted: usize,
    pub updated: usize,
    pub not_updated: usize,
}

#[derive(Debug, Clone)]
pub enum RefreshEvent {
    /// Boxed: an account's usage snapshot is large, the other events are tiny.
    AccountUpdated(Box<AccountUsageEntry>),
    Finished(RefreshSummary),
    Failed(String),
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{RefreshEvent, RefreshSummary, UsageProvider};
    use crate::dashboard::{self, AccountUsageEntry};
    use std::{
        sync::{Arc, OnceLock},
        time::Duration,
    };
    use usage_monitor_core::{
        accounts::{AccountRecord, AccountStore},
        auth::{
            AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
            AccountOAuthMaterialProvider, CompositeAuthMaterialProvider,
            OAuthCredentialProviderRegistry, StoredAuthMaterialProvider,
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
    use usage_monitor_windows::{
        WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
        WindowsDefaultBrowserLauncher,
    };

    #[cfg(test)]
    use usage_monitor_core::accounts::{CLAUDE, OPENAI};

    const MAX_CONCURRENT_ACCOUNT_REFRESHES: usize = 4;

    /// Everything a refresh needs, built once and kept for the life of the
    /// app. Reusing it keeps OAuth access tokens cached in memory (so a
    /// refresh does not spend a token refresh per account), keeps HTTP
    /// connections alive between refreshes, and avoids reopening the
    /// database and starting a runtime every time.
    struct RefreshContext {
        runtime: tokio::runtime::Runtime,
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        transport: Arc<ReqwestUsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        auth_store: Arc<dyn AccountAuthMaterialStore>,
    }

    static REFRESH_CONTEXT: OnceLock<Result<RefreshContext, String>> = OnceLock::new();

    fn refresh_context() -> Result<&'static RefreshContext, String> {
        REFRESH_CONTEXT
            .get_or_init(build_refresh_context)
            .as_ref()
            .map_err(Clone::clone)
    }

    fn build_refresh_context() -> Result<RefreshContext, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("usage-refresh")
            .enable_all()
            .build()
            .map_err(|error| format!("could not start the usage refresh runtime: {error}"))?;
        let store = Arc::new(
            SqliteStore::open(default_accounts_database_path())
                .map_err(|error| format!("could not open the accounts database: {error}"))?,
        );
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
        Ok(RefreshContext {
            runtime,
            account_store: store.clone(),
            snapshot_store: store,
            transport,
            auth,
            auth_store,
        })
    }

    pub fn refresh_accounts_for_provider(
        provider: UsageProvider,
    ) -> async_channel::Receiver<RefreshEvent> {
        let (sender, receiver) = async_channel::bounded(8);
        let context = match refresh_context() {
            Ok(context) => context,
            Err(error) => {
                let _ = sender.try_send(RefreshEvent::Failed(error));
                return receiver;
            }
        };
        let worker = context.runtime.spawn(refresh_accounts_for_provider_inner(
            context,
            provider,
            sender.clone(),
        ));
        // Report the outcome, including a panic, so the dashboard never
        // waits forever for a refresh to finish.
        context.runtime.spawn(async move {
            let event = match worker.await {
                Ok(Ok(summary)) => RefreshEvent::Finished(summary),
                Ok(Err(error)) => RefreshEvent::Failed(error),
                Err(_) => {
                    RefreshEvent::Failed("usage refresh worker ended unexpectedly".to_owned())
                }
            };
            let _ = sender.send(event).await;
        });
        receiver
    }

    async fn refresh_accounts_for_provider_inner(
        context: &'static RefreshContext,
        provider: UsageProvider,
        sender: async_channel::Sender<RefreshEvent>,
    ) -> Result<RefreshSummary, String> {
        let accounts = context
            .account_store
            .list()
            .await
            .map_err(|error| format!("could not load saved accounts: {error}"))?;
        let targets = prioritize_accounts(accounts, provider);
        refresh_targets(targets, sender, |account| refresh_account(context, account)).await
    }

    async fn refresh_targets<F, Fut>(
        targets: Vec<AccountRecord>,
        sender: async_channel::Sender<RefreshEvent>,
        refresh: F,
    ) -> Result<RefreshSummary, String>
    where
        F: Fn(AccountRecord) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(AccountUsageEntry, bool, bool), String>>
            + Send
            + 'static,
    {
        let attempted = targets.len();
        let mut updated = 0;
        let mut not_updated = 0;

        if !targets.is_empty() {
            let mut targets = targets.into_iter();
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..MAX_CONCURRENT_ACCOUNT_REFRESHES {
                let Some(account) = targets.next() else {
                    break;
                };
                tasks.spawn(refresh(account));
            }

            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok(Ok((entry, account_updated, account_not_updated))) => {
                        updated += usize::from(account_updated);
                        not_updated += usize::from(account_not_updated);
                        if sender
                            .send(RefreshEvent::AccountUpdated(Box::new(entry)))
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
                    tasks.spawn(refresh(account));
                }
            }
        }

        Ok(RefreshSummary {
            attempted,
            updated,
            not_updated,
        })
    }

    async fn refresh_account(
        context: &'static RefreshContext,
        account: AccountRecord,
    ) -> Result<(AccountUsageEntry, bool, bool), String> {
        let snapshot_store = &context.snapshot_store;
        let material = match context.auth_store.get(account.id).await {
            Ok(material) => material,
            Err(_) => {
                let entry = account_usage_entry(account, snapshot_store.as_ref()).await?;
                return Ok((entry, false, true));
            }
        };
        let runtime = match UsageRuntime::from_dependencies_with_auth_store(
            Arc::clone(&context.account_store),
            snapshot_store.clone(),
            Arc::clone(&context.transport) as Arc<dyn UsageHttpTransport>,
            Arc::clone(&context.auth),
            Arc::clone(&context.auth_store),
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
            RefreshStatus::Failed | RefreshStatus::RetainedStale | RefreshStatus::Invalidated => {
                (false, true)
            }
            RefreshStatus::Skipped => (false, false),
        };
        let entry = account_usage_entry(account, snapshot_store.as_ref()).await?;
        Ok((entry, account_updated, account_not_updated))
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

    fn prioritize_accounts(
        accounts: Vec<AccountRecord>,
        provider: UsageProvider,
    ) -> Vec<AccountRecord> {
        use std::collections::VecDeque;
        let (mut priority, others): (Vec<_>, Vec<_>) = accounts
            .into_iter()
            .partition(|account| dashboard::belongs_to_provider(&account.provider_id, provider));
        let mut groups: Vec<(String, VecDeque<AccountRecord>)> = Vec::new();
        for account in others {
            let provider_id = account.provider_id.to_ascii_lowercase();
            if let Some((_, queue)) = groups.iter_mut().find(|(id, _)| *id == provider_id) {
                queue.push_back(account);
            } else {
                groups.push((provider_id, VecDeque::from([account])));
            }
        }
        // The visible provider gets the first slots. Round-robin the other
        // providers so one with many accounts cannot delay all the others.
        while groups.iter().any(|(_, queue)| !queue.is_empty()) {
            for (_, queue) in &mut groups {
                if let Some(account) = queue.pop_front() {
                    priority.push(account);
                }
            }
        }
        priority
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

        // Explicit diagnostic only: performs normal refreshes of saved accounts.
        #[test]
        #[ignore = "contacts providers using the local saved accounts"]
        fn live_refresh_latency() {
            for pass in ["cold", "warm"] {
                let started = std::time::Instant::now();
                {
                    let receiver = refresh_accounts_for_provider(UsageProvider::Codex);
                    while let Ok(event) = receiver.recv_blocking() {
                        match event {
                            RefreshEvent::AccountUpdated(entry) => println!(
                                "{pass}: {} delivered at {:.3}s, stale={}",
                                entry.account.account_ref.as_deref().unwrap_or("?"),
                                started.elapsed().as_secs_f64(),
                                entry
                                    .snapshot
                                    .as_ref()
                                    .is_none_or(|snapshot| snapshot.is_stale),
                            ),
                            RefreshEvent::Finished(summary) => {
                                println!(
                                    "{pass}: {summary:?} at {:.3}s",
                                    started.elapsed().as_secs_f64()
                                );
                                break;
                            }
                            RefreshEvent::Failed(error) => panic!("refresh failed: {error}"),
                        }
                    }
                }
            }
        }

        fn account(label: &str, email: &str, provider: &str) -> AccountRecord {
            AccountRecord::create(label, email, None, provider, None).unwrap()
        }

        #[test]
        fn visible_provider_is_first_without_excluding_other_accounts() {
            let accounts = vec![
                account("router", "router@example.com", "openrouter"),
                account("router 2", "router2@example.com", "openrouter"),
                account("codex", "codex@example.com", OPENAI),
                account("claude", "claude@example.com", CLAUDE),
            ];

            let ordered = prioritize_accounts(accounts, UsageProvider::Codex);
            assert_eq!(ordered.len(), 4);
            assert_eq!(ordered[0].provider_id, OPENAI);
            assert_eq!(ordered[1].provider_id, "openrouter");
            assert_eq!(ordered[2].provider_id, CLAUDE);
            assert_eq!(ordered[3].email, "router2@example.com");
        }

        #[tokio::test]
        async fn slow_visible_accounts_do_not_block_other_providers_or_exceed_the_limit() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            let accounts = (0..8)
                .map(|index| {
                    account(
                        if index == 7 { "error" } else { "test" },
                        &format!("a{index}@example.com"),
                        if index < 2 { OPENAI } else { CLAUDE },
                    )
                })
                .collect();
            let (events, receiver) = async_channel::bounded(16);
            let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
            let selected_gate = Arc::new(tokio::sync::Semaphore::new(0));
            let other_gate = Arc::new(tokio::sync::Semaphore::new(0));
            let active = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let worker = tokio::spawn({
                let selected_gate = Arc::clone(&selected_gate);
                let other_gate = Arc::clone(&other_gate);
                let peak = Arc::clone(&peak);
                async move {
                    refresh_targets(accounts, events, move |account| {
                        let started = started.clone();
                        let selected_gate = Arc::clone(&selected_gate);
                        let other_gate = Arc::clone(&other_gate);
                        let active = Arc::clone(&active);
                        let peak = Arc::clone(&peak);
                        async move {
                            peak.fetch_max(
                                active.fetch_add(1, Ordering::SeqCst) + 1,
                                Ordering::SeqCst,
                            );
                            started.send(()).unwrap();
                            let gate = if account.provider_id == OPENAI {
                                selected_gate
                            } else {
                                other_gate
                            };
                            gate.acquire().await.unwrap().forget();
                            active.fetch_sub(1, Ordering::SeqCst);
                            if account.label == "error" {
                                return Err("provider failed".to_owned());
                            }
                            Ok((
                                AccountUsageEntry {
                                    account,
                                    snapshot: None,
                                },
                                true,
                                false,
                            ))
                        }
                    })
                    .await
                    .unwrap()
                }
            });
            for _ in 0..MAX_CONCURRENT_ACCOUNT_REFRESHES {
                tokio::time::timeout(Duration::from_secs(1), starts.recv())
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert!(
                starts.try_recv().is_err(),
                "queued accounts must wait for a slot"
            );
            other_gate.add_permits(1);
            let first = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            let RefreshEvent::AccountUpdated(first) = first else {
                panic!("expected an incremental account result");
            };
            assert_eq!(first.account.provider_id, CLAUDE);
            selected_gate.add_permits(2);
            other_gate.add_permits(8);
            let summary = tokio::time::timeout(Duration::from_secs(1), worker)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(summary.attempted, 8);
            assert_eq!(summary.updated, 7);
            assert_eq!(summary.not_updated, 1);
            assert_eq!(
                peak.load(Ordering::SeqCst),
                MAX_CONCURRENT_ACCOUNT_REFRESHES
            );
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows::refresh_accounts_for_provider;

#[cfg(not(target_os = "windows"))]
pub fn refresh_accounts_for_provider(
    _provider: UsageProvider,
) -> async_channel::Receiver<RefreshEvent> {
    let (sender, receiver) = async_channel::bounded(1);
    let _ = sender.send_blocking(RefreshEvent::Failed(
        "live usage refresh is only available on Windows".to_owned(),
    ));
    receiver
}
