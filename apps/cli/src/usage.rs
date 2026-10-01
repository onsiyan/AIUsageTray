//! `usage` and `status` commands, refreshes, and the `usage watch` scheduler.

use super::*;

pub(super) async fn execute_usage_command(
    database_path: &std::path::Path,
    command: UsageCommand,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let store = open_store(database_path)?;
    let account_store: Arc<dyn AccountStore> = store.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = store.clone();
    match command {
        UsageCommand::Watch => run_usage_scheduler(account_store, snapshot_store).await,
        UsageCommand::Get(arguments) => {
            let accounts = account_store
                .list()
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let account = resolve_account(
                &accounts,
                &arguments.selector,
                &SelectorFilters {
                    provider: arguments.provider.as_deref(),
                    workspace: arguments.workspace.as_deref(),
                },
            )?
            .clone();
            let results = collect_refresh_results(
                std::slice::from_ref(&account),
                &account_store,
                &snapshot_store,
            )
            .await?;
            let (_, outcome) = results
                .into_iter()
                .next()
                .expect("one selected account is refreshed");
            let exit_code = usage_get_exit_code(outcome.status);
            if json_output {
                print_json(usage_get_value(&account, &outcome));
            } else {
                print_usage_get(&account, &outcome);
            }
            Ok(exit_code)
        }
        UsageCommand::Refresh {
            selector,
            all,
            provider,
            workspace,
        } => {
            let accounts = account_store
                .list()
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            if accounts.is_empty() {
                return Err(CliFailure::no_accounts());
            }
            let targets = if all {
                accounts
                    .iter()
                    .filter(|account| {
                        provider
                            .as_deref()
                            .is_none_or(|value| provider_matches(&account.provider_id, value))
                    })
                    .filter(|account| {
                        workspace
                            .as_deref()
                            .is_none_or(|value| workspace_matches(account, value))
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                vec![
                    resolve_account(
                        &accounts,
                        selector
                            .as_deref()
                            .expect("clap requires selector unless --all"),
                        &SelectorFilters {
                            provider: provider.as_deref(),
                            workspace: workspace.as_deref(),
                        },
                    )?
                    .clone(),
                ]
            };
            if targets.is_empty() {
                return Err(CliFailure::new(
                    "no_matching_accounts",
                    "No saved accounts match the requested filters.",
                    3,
                ));
            }
            refresh_accounts(&targets, &account_store, &snapshot_store, json_output).await
        }
    }
}

pub(super) async fn run_usage_scheduler(
    account_store: Arc<dyn AccountStore>,
    snapshot_store: Arc<dyn UsageSnapshotStore>,
) -> Result<i32, CliFailure> {
    let accounts = account_store
        .list()
        .await
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    if accounts.is_empty() {
        return Err(CliFailure::no_accounts());
    }

    let transport = Arc::new(
        ReqwestUsageHttpTransport::new(Duration::from_secs(45))
            .map_err(|error| CliFailure::runtime(error.to_string()))?,
    );
    let oauth_store = Arc::new(WindowsCredentialManagerStore);
    let auth_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let auth = build_auth_provider(
        Arc::clone(&transport),
        Arc::clone(&oauth_store),
        Arc::clone(&auth_store),
    );
    let providers = per_account_source_registry(transport, auth, auth_store)
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    let runtime = UsageRuntime::new(
        account_store,
        snapshot_store,
        Arc::new(providers),
        RefreshCoordinatorConfig {
            cadence: RefreshCadence::Automatic,
            ..RefreshCoordinatorConfig::default()
        },
    );

    eprintln!(
        "[scheduler] started for {} saved accounts (adaptive cadence; initial refresh begins now). Stop this process to stop scheduled refreshes.",
        accounts.len()
    );
    runtime
        .coordinator()
        .clone()
        .run()
        .await
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    Ok(0)
}

pub(super) async fn execute_status(
    database_path: &std::path::Path,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let store = open_store(database_path)?;
    let accounts = store
        .list()
        .await
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    let mut with_snapshot = 0usize;
    let mut stale = 0usize;
    let mut never_refreshed = Vec::new();
    let mut latest_observed: Option<DateTime<Utc>> = None;
    for account in &accounts {
        match store
            .get_latest(account.id)
            .await
            .map_err(|error| CliFailure::runtime(error.to_string()))?
        {
            Some(snapshot) => {
                with_snapshot += 1;
                stale += usize::from(snapshot.is_stale);
                latest_observed = Some(
                    latest_observed
                        .map(|current| current.max(snapshot.observed_at_utc))
                        .unwrap_or(snapshot.observed_at_utc),
                );
            }
            None => never_refreshed.push(account.account_ref.clone()),
        }
    }
    let never_refreshed = never_refreshed.into_iter().flatten().collect::<Vec<_>>();
    let status = json!({
        "schema_version": 1,
        "account_count": accounts.len(),
        "accounts_with_snapshot": with_snapshot,
        "accounts_without_snapshot": accounts.len() - with_snapshot,
        "stale_snapshots": stale,
        "never_refreshed": never_refreshed,
        "latest_observed_at_utc": latest_observed,
    });
    if json_output {
        print_json(status);
    } else {
        println!("Saved accounts: {}", accounts.len());
        println!("With cached usage: {with_snapshot}");
        println!("Without cached usage: {}", accounts.len() - with_snapshot);
        println!("Stale snapshots: {stale}");
        println!(
            "Latest observation (UTC): {}",
            latest_observed
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "none".to_owned())
        );
    }
    Ok(0)
}

pub(super) async fn refresh_accounts(
    accounts: &[AccountRecord],
    account_store: &Arc<dyn AccountStore>,
    snapshot_store: &Arc<dyn UsageSnapshotStore>,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let results = collect_refresh_results(accounts, account_store, snapshot_store).await?;

    let all_updated = results
        .iter()
        .all(|(_, outcome)| outcome.status == RefreshStatus::Updated);
    if json_output {
        print_json(json!({
            "schema_version": 1,
            "results": results.iter().map(|(account, outcome)| refresh_result_value(account, outcome)).collect::<Vec<_>>(),
        }));
    } else {
        for (account, outcome) in &results {
            let reference = account.account_ref.as_deref().unwrap_or("?");
            match &outcome.error {
                Some(error) => println!(
                    "{reference}\t{}\t{}: {}",
                    refresh_status_name(outcome.status),
                    format!("{:?}", error.code).to_ascii_lowercase(),
                    error.message
                ),
                None => println!("{reference}\t{}", refresh_status_name(outcome.status)),
            }
        }
    }
    Ok(if all_updated { 0 } else { EXIT_REFRESH_FAILED })
}

pub(super) async fn collect_refresh_results(
    accounts: &[AccountRecord],
    account_store: &Arc<dyn AccountStore>,
    snapshot_store: &Arc<dyn UsageSnapshotStore>,
) -> Result<Vec<(AccountRecord, usage_monitor_core::refresh::RefreshOutcome)>, CliFailure> {
    let transport = Arc::new(
        ReqwestUsageHttpTransport::new(Duration::from_secs(45))
            .map_err(|error| CliFailure::runtime(error.to_string()))?,
    );
    let oauth_store = Arc::new(WindowsCredentialManagerStore);
    let auth_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let auth = build_auth_provider(
        Arc::clone(&transport),
        Arc::clone(&oauth_store),
        Arc::clone(&auth_store),
    );

    let mut results = Vec::with_capacity(accounts.len());
    for account in accounts {
        let saved_material = auth_store
            .get(account.id)
            .await
            .map_err(|error| CliFailure::runtime(error.to_string()))?;
        let provider_config = provider_config_for(account, saved_material.as_ref());
        let runtime = UsageRuntime::from_dependencies_with_auth_store(
            Arc::clone(account_store),
            Arc::clone(snapshot_store),
            Arc::clone(&transport) as Arc<dyn UsageHttpTransport>,
            Arc::clone(&auth),
            Arc::clone(&auth_store) as Arc<dyn AccountAuthMaterialStore>,
            provider_config,
            RefreshCoordinatorConfig {
                cadence: RefreshCadence::Manual,
                ..RefreshCoordinatorConfig::default()
            },
        )
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
        let outcome = runtime
            .refresh_account(account.clone(), RefreshReason::Manual)
            .await;
        results.push((account.clone(), outcome));
    }
    Ok(results)
}

pub(super) fn build_auth_provider(
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

/// OpenCode Go source modes that `provider_config_for` can select for one
/// account. Every other provider has a single source.
pub(super) const OPENCODE_GO_SOURCE_VARIANTS: [OpenCodeGoSourceMode; 2] =
    [OpenCodeGoSourceMode::Web, OpenCodeGoSourceMode::Api];

/// Builds the long-running scheduler's providers so each OpenCode Go account
/// is probed with the same per-account source selection as `usage refresh`,
/// instead of one global mode for every account.
pub(super) fn per_account_source_registry(
    transport: Arc<ReqwestUsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    auth_store: Arc<WindowsCredentialManagerAuthMaterialStore>,
) -> Result<ProviderRegistry, ProviderRegistryError> {
    let transport = transport as Arc<dyn UsageHttpTransport>;
    let store = Arc::clone(&auth_store) as Arc<dyn AccountAuthMaterialStore>;
    // One refreshing auth chain shared by every variant keeps per-account
    // token-rotation locks and back-off state in one place.
    let auth = Arc::new(OpenCodeGoOAuthRefreshingAuthMaterialProvider::new(
        auth,
        Arc::clone(&store),
        Arc::clone(&transport),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let auth = Arc::new(ClaudeOAuthRefreshingAuthMaterialProvider::new(
        auth,
        Arc::clone(&store),
        Arc::clone(&transport),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let mut variants = Vec::new();
    for opencode_go_source_mode in OPENCODE_GO_SOURCE_VARIANTS {
        let registry = ProviderRegistry::from_dependencies(
            Arc::clone(&transport),
            Arc::clone(&auth),
            ProviderRegistryConfig {
                opencode_go_source_mode,
            },
        )?;
        variants.push((opencode_go_source_mode, registry));
    }
    let base = &variants[0].1;
    let adapters = base
        .canonical_ids()
        .iter()
        .filter_map(|provider_id| {
            if provider_id == OPENCODE_GO {
                Some(Arc::new(PerAccountSourceAdapter {
                    provider_id: provider_id.clone(),
                    auth_store: Arc::clone(&auth_store),
                    variants: variants
                        .iter()
                        .filter_map(|(mode, registry)| Some((*mode, registry.get(provider_id)?)))
                        .collect(),
                }) as Arc<dyn UsageAdapter>)
            } else {
                base.get(provider_id)
            }
        })
        .collect::<Vec<_>>();
    ProviderRegistry::from_adapters(adapters)
}

pub(super) struct PerAccountSourceAdapter {
    pub(super) provider_id: String,
    pub(super) auth_store: Arc<WindowsCredentialManagerAuthMaterialStore>,
    pub(super) variants: Vec<(OpenCodeGoSourceMode, Arc<dyn UsageAdapter>)>,
}

#[async_trait::async_trait]
impl UsageAdapter for PerAccountSourceAdapter {
    fn adapter_id(&self) -> &str {
        &self.provider_id
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = self.auth_store.get(account.id).await.ok().flatten();
        let mode = provider_config_for(account, material.as_ref()).opencode_go_source_mode;
        let adapter = self
            .variants
            .iter()
            .find(|(variant, _)| *variant == mode)
            .or_else(|| self.variants.first())
            .map(|(_, adapter)| adapter)
            .expect("source variants are registered");
        adapter.probe(account).await
    }
}

pub(super) fn provider_config_for(
    account: &AccountRecord,
    material: Option<&AccountAuthMaterial>,
) -> ProviderRegistryConfig {
    ProviderRegistryConfig::for_account(account, material)
}
