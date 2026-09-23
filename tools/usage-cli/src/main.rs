use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, error::ErrorKind};
use codex_usage_core::{
    accounts::{AccountRecord, AccountStore, CLAUDE, OPENAI, OPENCODE_GO},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        AccountBrowserSessionRefresher, AccountOAuthMaterialProvider,
        CompositeAuthMaterialProvider, OAuthCredentialProviderRegistry, StoredAuthMaterialProvider,
    },
    oauth_loopback::{CodexOAuthCallbackListenerFactory, LoopbackOAuthCallbackListenerFactory},
    oauth_service::OAuthAuthorizationService,
    providers::{
        antigravity, claude::ClaudeSourceMode, openai, opencode_go::OpenCodeGoSourceMode,
        registry::ProviderRegistryConfig,
    },
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::{ReqwestUsageHttpTransport, UsageHttpTransport},
    usage::{UsageSnapshot, UsageSnapshotStore, UsageWindowKind},
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    WindowsDefaultBrowserLauncher, browser_cookies::WindowsBrowserCookieImporter,
};
use serde_json::{Value, json};
use std::{path::PathBuf, process, sync::Arc, time::Duration};

const EXIT_USAGE_NOT_CACHED: i32 = 5;
const EXIT_REFRESH_FAILED: i32 = 6;

#[derive(Debug, Parser)]
#[command(
    name = "codex-usage",
    version,
    about = "Read and refresh locally saved provider usage accounts"
)]
struct Cli {
    /// Use a different SQLite account database.
    #[arg(long, global = true)]
    database: Option<PathBuf>,

    /// Write JSON results and structured runtime errors to stdout.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List and manage saved accounts.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Read cached usage or explicitly refresh a provider.
    Usage {
        #[command(subcommand)]
        command: UsageCommand,
    },
    /// Summarize account and cached-snapshot availability without networking.
    Status,
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// List accounts. The first column is the stable account reference.
    List {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Show one account selected by reference, alias, name, or email.
    Get(SelectorArgs),
    /// Set or clear the optional display alias.
    Alias {
        #[command(subcommand)]
        command: AliasCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AliasCommand {
    Set {
        selector: String,
        alias: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    Clear {
        selector: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug, Args)]
struct SelectorArgs {
    selector: String,
    /// Disambiguate an exact name or email match by provider.
    #[arg(long)]
    provider: Option<String>,
    /// Disambiguate by exact workspace id or workspace name.
    #[arg(long)]
    workspace: Option<String>,
}

#[derive(Debug, Subcommand)]
enum UsageCommand {
    /// Read the latest saved snapshot. This command never makes a network call.
    Get(SelectorArgs),
    /// Explicitly contact the provider and save the resulting snapshot.
    Refresh {
        /// An account reference or exact name/email. Omit only with --all.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        selector: Option<String>,
        /// Refresh every saved account.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug)]
struct CliFailure {
    code: &'static str,
    message: String,
    exit_code: i32,
    candidates: Vec<String>,
}

impl CliFailure {
    fn new(code: &'static str, message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            code,
            message: message.into(),
            exit_code,
            candidates: Vec::new(),
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self::new("runtime_error", message, 1)
    }

    fn no_accounts() -> Self {
        Self::new("no_accounts", "No saved accounts were found.", 3)
    }
}

#[derive(Debug, Default)]
struct SelectorFilters<'a> {
    provider: Option<&'a str>,
    workspace: Option<&'a str>,
}

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                return;
            }
            let exit_code = error.exit_code();
            if json_requested() {
                print_json(json!({
                    "schema_version": 1,
                    "error": {
                        "code": "invalid_arguments",
                        "message": error.to_string(),
                        "candidates": [],
                    }
                }));
            } else {
                let _ = error.print();
            }
            process::exit(exit_code);
        }
    };
    let json_output = cli.json;
    let exit_code = match execute(cli).await {
        Ok(exit_code) => exit_code,
        Err(failure) => {
            emit_failure(&failure, json_output);
            failure.exit_code
        }
    };
    if exit_code != 0 {
        process::exit(exit_code);
    }
}

fn json_requested() -> bool {
    std::env::args_os().any(|argument| argument == "--json")
}

async fn execute(cli: Cli) -> Result<i32, CliFailure> {
    let database_path = cli.database.unwrap_or_else(default_accounts_database_path);
    let json_output = cli.json;
    match cli.command {
        Command::Account { command } => {
            execute_account_command(&database_path, command, json_output).await
        }
        Command::Usage { command } => {
            execute_usage_command(&database_path, command, json_output).await
        }
        Command::Status => execute_status(&database_path, json_output).await,
    }
}

async fn execute_account_command(
    database_path: &std::path::Path,
    command: AccountCommand,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let store = open_store(database_path)?;
    let account_store: Arc<dyn AccountStore> = store.clone();
    match command {
        AccountCommand::List {
            provider,
            workspace,
        } => {
            let accounts = account_store
                .list()
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let accounts = accounts
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
                .collect::<Vec<_>>();
            if json_output {
                print_json(json!({
                    "schema_version": 1,
                    "accounts": accounts.iter().map(|account| account_value(account)).collect::<Vec<_>>(),
                }));
            } else {
                print_account_table(&accounts);
            }
            Ok(0)
        }
        AccountCommand::Get(arguments) => {
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
            )?;
            if json_output {
                print_json(json!({
                    "schema_version": 1,
                    "account": account_value(account),
                }));
            } else {
                print_account_details(account);
            }
            Ok(0)
        }
        AccountCommand::Alias { command } => {
            let (selector, alias, provider, workspace) = match command {
                AliasCommand::Set {
                    selector,
                    alias,
                    provider,
                    workspace,
                } => (selector, Some(alias), provider, workspace),
                AliasCommand::Clear {
                    selector,
                    provider,
                    workspace,
                } => (selector, None, provider, workspace),
            };
            let accounts = account_store
                .list()
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let account = resolve_account(
                &accounts,
                &selector,
                &SelectorFilters {
                    provider: provider.as_deref(),
                    workspace: workspace.as_deref(),
                },
            )?;
            let updated = account_store
                .set_alias(account.id, alias.as_deref())
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?
                .ok_or_else(|| {
                    CliFailure::new(
                        "account_not_found",
                        "The account disappeared during alias update.",
                        3,
                    )
                })?;
            if json_output {
                print_json(json!({
                    "schema_version": 1,
                    "account": account_value(&updated),
                }));
            } else {
                print_account_details(&updated);
            }
            Ok(0)
        }
    }
}

async fn execute_usage_command(
    database_path: &std::path::Path,
    command: UsageCommand,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let store = open_store(database_path)?;
    let account_store: Arc<dyn AccountStore> = store.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = store.clone();
    match command {
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
            )?;
            let snapshot = snapshot_store
                .get_latest(account.id)
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?
                .ok_or_else(|| {
                    CliFailure::new(
                        "usage_not_cached",
                        format!(
                            "No usage snapshot is saved for {}. Run `codex-usage usage refresh {}` explicitly.",
                            account.account_ref.as_deref().unwrap_or("this account"),
                            account.account_ref.as_deref().unwrap_or("<account-ref>")
                        ),
                        EXIT_USAGE_NOT_CACHED,
                    )
                })?;
            if json_output {
                print_json(json!({
                    "schema_version": 1,
                    "account": account_value(account),
                    "snapshot": snapshot_value(&snapshot),
                }));
            } else {
                print_usage(account, &snapshot);
            }
            Ok(0)
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

async fn execute_status(
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

async fn refresh_accounts(
    accounts: &[AccountRecord],
    account_store: &Arc<dyn AccountStore>,
    snapshot_store: &Arc<dyn UsageSnapshotStore>,
    json_output: bool,
) -> Result<i32, CliFailure> {
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
    let session_refresher = Arc::new(
        WindowsBrowserCookieImporter::from_process()
            .map_err(|error| CliFailure::runtime(error.to_string()))?,
    ) as Arc<dyn AccountBrowserSessionRefresher>;

    let mut results = Vec::with_capacity(accounts.len());
    for account in accounts {
        let saved_material = auth_store
            .get(account.id)
            .await
            .map_err(|error| CliFailure::runtime(error.to_string()))?;
        let provider_config = provider_config_for(account, saved_material.as_ref());
        let runtime = UsageRuntime::from_dependencies_with_auth_store_and_session_refresher(
            Arc::clone(account_store),
            Arc::clone(snapshot_store),
            Arc::clone(&transport) as Arc<dyn UsageHttpTransport>,
            Arc::clone(&auth),
            Arc::clone(&auth_store) as Arc<dyn AccountAuthMaterialStore>,
            Arc::clone(&session_refresher),
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
        results.push((account, outcome));
    }

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
    let mut config = ProviderRegistryConfig {
        enable_antigravity_local_probe: false,
        ..ProviderRegistryConfig::default()
    };
    match account.provider_id.as_str() {
        CLAUDE => {
            let has_oauth = material.is_some_and(|material| {
                material.oauth_refresh_token.is_some()
                    || material
                        .bearer_token
                        .as_deref()
                        .is_some_and(|token| token.starts_with("sk-ant-oat"))
            });
            let has_admin_key = material.is_some_and(|material| {
                material
                    .bearer_token
                    .as_deref()
                    .is_some_and(|token| token.starts_with("sk-ant-admin"))
            });
            config.claude_source_mode = if has_oauth {
                ClaudeSourceMode::OAuth
            } else if has_admin_key {
                ClaudeSourceMode::AdminApi
            } else {
                ClaudeSourceMode::Web
            };
        }
        OPENCODE_GO => {
            let has_browser_session = material.is_some_and(|material| {
                material.cookies.iter().any(|cookie| {
                    ["auth", "__Host-auth", "__Host-console_session"]
                        .iter()
                        .any(|name| cookie.name.eq_ignore_ascii_case(name))
                })
            });
            config.opencode_go_source_mode = if has_browser_session {
                OpenCodeGoSourceMode::Web
            } else {
                OpenCodeGoSourceMode::Api
            };
        }
        _ => {}
    }
    config
}

fn open_store(path: &std::path::Path) -> Result<Arc<SqliteStore>, CliFailure> {
    SqliteStore::open(path)
        .map(Arc::new)
        .map_err(|error| CliFailure::runtime(error.to_string()))
}

fn resolve_account<'a>(
    accounts: &'a [AccountRecord],
    selector: &str,
    filters: &SelectorFilters<'_>,
) -> Result<&'a AccountRecord, CliFailure> {
    let selector = selector.trim();
    if let Some(account) = accounts.iter().find(|account| {
        account
            .account_ref
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(selector))
    }) {
        let provider_matches = filters
            .provider
            .is_none_or(|provider| provider_matches(&account.provider_id, provider));
        let workspace_matches = filters
            .workspace
            .is_none_or(|workspace| workspace_matches(account, workspace));
        return if provider_matches && workspace_matches {
            Ok(account)
        } else {
            Err(CliFailure::new(
                "account_not_found",
                format!("Account reference `{selector}` does not match the supplied filters."),
                3,
            ))
        };
    }

    let matches = accounts
        .iter()
        .filter(|account| {
            filters
                .provider
                .is_none_or(|provider| provider_matches(&account.provider_id, provider))
        })
        .filter(|account| {
            filters
                .workspace
                .is_none_or(|workspace| workspace_matches(account, workspace))
        })
        .filter(|account| {
            account
                .alias
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(selector))
                || account.label.eq_ignore_ascii_case(selector)
                || account.email.eq_ignore_ascii_case(selector)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [account] => Ok(*account),
        [] => Err(CliFailure::new(
            "account_not_found",
            format!("No saved account matches `{selector}` and the supplied filters."),
            3,
        )),
        _ => Err(ambiguous_account_failure(selector, &matches)),
    }
}

fn ambiguous_account_failure(selector: &str, matches: &[&AccountRecord]) -> CliFailure {
    let candidates = matches
        .iter()
        .filter_map(|account| account.account_ref.clone())
        .collect::<Vec<_>>();
    let mut failure = CliFailure::new(
        "ambiguous_account",
        format!("`{selector}` matches more than one account; add --provider or --workspace."),
        4,
    );
    failure.candidates = candidates;
    failure
}

fn provider_matches(account_provider: &str, requested: &str) -> bool {
    let account_provider = normalize_name(account_provider);
    let requested = normalize_name(requested);
    if requested == "codex" {
        return account_provider == normalize_name(OPENAI) || account_provider == "codex";
    }
    account_provider == requested
}

fn normalize_name(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| *character != '-' && *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn workspace_matches(account: &AccountRecord, requested: &str) -> bool {
    account
        .workspace_id
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(requested.trim()))
        || account
            .workspace_name
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(requested.trim()))
}

fn account_value(account: &AccountRecord) -> Value {
    json!({
        "account_ref": account.account_ref,
        "provider": provider_name(&account.provider_id),
        "provider_id": account.provider_id,
        "name": account.display_name(),
        "label": account.label,
        "alias": account.alias,
        "email": account.email,
        "workspace": account.workspace_id.as_ref().map(|id| json!({
            "id": id,
            "name": account.workspace_name,
        })),
        "status": account_status_name(account),
    })
}

fn snapshot_value(snapshot: &UsageSnapshot) -> Value {
    let mut windows = Vec::new();
    if let Some(window) = &snapshot.primary {
        windows.push(window_value("primary", window));
    }
    if let Some(window) = &snapshot.secondary {
        windows.push(window_value("secondary", window));
    }
    windows.extend(
        snapshot
            .additional_windows
            .iter()
            .map(|additional| window_value(&additional.key, &additional.window)),
    );
    json!({
        "observed_at_utc": snapshot.observed_at_utc,
        "provider": provider_name(&snapshot.provider_id),
        "provider_id": snapshot.provider_id,
        "source": snapshot.source,
        "data_confidence": snapshot.data_confidence,
        "plan_type": snapshot.plan_type,
        "observed_email": snapshot.observed_email,
        "response_account_id": snapshot.response_account_id,
        "primary_window_kind": snapshot.primary_window_kind,
        "primary_window_is_synthetic": snapshot.primary_window_is_synthetic,
        "is_stale": snapshot.is_stale,
        "stale_reason": snapshot.stale_reason,
        "stale_at_utc": snapshot.stale_at_utc,
        "windows": windows,
        "metrics": snapshot.metrics,
        "credits": snapshot.credits,
        "credit_inventory": snapshot.credit_inventory,
        "spend": snapshot.spend,
        "source_diagnostics": snapshot.source_diagnostics,
    })
}

fn window_value(key: &str, window: &codex_usage_core::usage::RateLimitWindow) -> Value {
    json!({
        "key": key,
        "kind": window_kind_name(window.kind),
        "name": window.name,
        "used_percent": window.used_percent,
        "remaining_percent": window.remaining_percent(),
        "reset_at_utc": window.reset_at_utc,
        "limit_window_seconds": window.limit_window_seconds,
    })
}

fn window_kind_name(kind: UsageWindowKind) -> &'static str {
    match kind {
        UsageWindowKind::Primary => "primary",
        UsageWindowKind::Secondary => "secondary",
        UsageWindowKind::Additional => "additional",
    }
}

fn refresh_result_value(
    account: &AccountRecord,
    outcome: &codex_usage_core::refresh::RefreshOutcome,
) -> Value {
    json!({
        "account_ref": account.account_ref,
        "provider": provider_name(&account.provider_id),
        "provider_id": account.provider_id,
        "status": refresh_status_name(outcome.status),
        "completed_at_utc": outcome.completed_at_utc,
        "snapshot": outcome.snapshot.as_ref().map(snapshot_value),
        "error": outcome.error.as_ref().map(|error| json!({
            "code": format!("{:?}", error.code).to_ascii_lowercase(),
            "message": error.message,
            "http_status_code": error.http_status_code,
            "retry_after_seconds": error.retry_after_seconds,
        })),
        "storage_error": outcome.storage_error,
    })
}

fn account_status_name(account: &AccountRecord) -> &'static str {
    use codex_usage_core::accounts::AccountStatus;
    match account.status {
        AccountStatus::Active => "active",
        AccountStatus::NeedsReauthentication => "needs_reauthentication",
        AccountStatus::Paused => "paused",
        AccountStatus::Disabled => "disabled",
    }
}

fn provider_name(provider_id: &str) -> &str {
    match provider_id {
        "openai" | "codex" => "codex",
        "opencodego" => "opencode-go",
        _ => provider_id,
    }
}

fn refresh_status_name(status: RefreshStatus) -> &'static str {
    match status {
        RefreshStatus::Updated => "updated",
        RefreshStatus::RetainedStale => "retained_stale",
        RefreshStatus::Failed => "failed",
        RefreshStatus::Invalidated => "invalidated",
        RefreshStatus::Skipped => "skipped",
    }
}

fn print_json(value: Value) {
    println!(
        "{}",
        serde_json::to_string(&value).expect("CLI JSON values serialize")
    );
}

fn emit_failure(failure: &CliFailure, json_output: bool) {
    if json_output {
        print_json(json!({
            "schema_version": 1,
            "error": {
                "code": failure.code,
                "message": failure.message,
                "candidates": failure.candidates,
            }
        }));
    } else {
        eprintln!("{}: {}", failure.code, failure.message);
        if !failure.candidates.is_empty() {
            eprintln!("Candidates: {}", failure.candidates.join(", "));
        }
    }
}

fn print_account_table(accounts: &[&AccountRecord]) {
    println!("REF\tPROVIDER\tNAME\tEMAIL\tWORKSPACE\tSTATUS");
    for account in accounts {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            account.account_ref.as_deref().unwrap_or("?"),
            provider_name(&account.provider_id),
            account.display_name(),
            account.email,
            account.workspace_name.as_deref().unwrap_or(""),
            account_status_name(account),
        );
    }
}

fn print_account_details(account: &AccountRecord) {
    println!(
        "Reference: {}",
        account.account_ref.as_deref().unwrap_or("?")
    );
    println!("Provider: {}", provider_name(&account.provider_id));
    println!("Name: {}", account.display_name());
    println!("Email: {}", account.email);
    println!(
        "Workspace: {}",
        account.workspace_name.as_deref().unwrap_or("none")
    );
    println!("Status: {}", account_status_name(account));
}

fn print_usage(account: &AccountRecord, snapshot: &UsageSnapshot) {
    println!(
        "{}\t{}\t{}",
        account.account_ref.as_deref().unwrap_or("?"),
        provider_name(&account.provider_id),
        account.display_name()
    );
    println!("Observed (UTC): {}", snapshot.observed_at_utc.to_rfc3339());
    println!(
        "Plan: {}",
        snapshot.plan_type.as_deref().unwrap_or("unknown")
    );
    println!("Stale: {}", snapshot.is_stale);
    if let Some(window) = &snapshot.primary {
        print_window("primary", window);
    }
    if let Some(window) = &snapshot.secondary {
        print_window("secondary", window);
    }
    for additional in &snapshot.additional_windows {
        print_window(&additional.key, &additional.window);
    }
}

fn print_window(key: &str, window: &codex_usage_core::usage::RateLimitWindow) {
    println!(
        "{key}\t{}\t{:.1}% used\t{:.1}% remaining\treset={}",
        window.name,
        window.used_percent,
        window.remaining_percent(),
        window
            .reset_at_utc
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_owned())
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_usage_core::accounts::ANTIGRAVITY;

    fn account(
        provider: &str,
        reference: &str,
        label: &str,
        email: &str,
        workspace_id: Option<&str>,
        workspace_name: Option<&str>,
    ) -> AccountRecord {
        let mut account = AccountRecord::create(
            label,
            email,
            None,
            provider,
            workspace_id.map(str::to_owned),
        )
        .unwrap();
        account.account_ref = Some(reference.to_owned());
        account.workspace_name = workspace_name.map(str::to_owned);
        account
    }

    #[test]
    fn stable_reference_selects_account_and_json_hides_internal_uuid() {
        let first = account(
            OPENAI,
            "ch1",
            "Work",
            "same@example.com",
            Some("ws-1"),
            None,
        );
        let mut second = account(
            CLAUDE,
            "cc1",
            "Work",
            "same@example.com",
            Some("ws-2"),
            None,
        );
        second.alias = Some("ch1".to_owned());
        let accounts = [first.clone(), second];

        let selected = resolve_account(&accounts, "CH1", &SelectorFilters::default()).unwrap();
        assert_eq!(selected.id, first.id);
        let value = account_value(selected);
        assert_eq!(value["account_ref"], "ch1");
        assert_eq!(value["provider"], "codex");
        assert_eq!(value["provider_id"], "openai");
        assert!(value.get("id").is_none());
        assert!(!value.to_string().contains(&first.id.to_string()));

        let filtered = resolve_account(
            &accounts,
            "ch1",
            &SelectorFilters {
                provider: Some("claude"),
                workspace: None,
            },
        )
        .unwrap_err();
        assert_eq!(filtered.code, "account_not_found");
    }

    #[test]
    fn ambiguous_email_requires_provider_or_workspace_qualifier() {
        let first = account(
            OPENAI,
            "ch1",
            "Personal",
            "same@example.com",
            Some("ws-1"),
            None,
        );
        let second = account(
            OPENAI,
            "ch2",
            "Work",
            "same@example.com",
            Some("ws-2"),
            None,
        );
        let accounts = [first, second];

        let error = resolve_account(&accounts, "same@example.com", &SelectorFilters::default())
            .unwrap_err();
        assert_eq!(error.code, "ambiguous_account");
        assert_eq!(error.candidates, ["ch1", "ch2"]);

        let selected = resolve_account(
            &accounts,
            "same@example.com",
            &SelectorFilters {
                provider: Some("codex"),
                workspace: Some("ws-2"),
            },
        )
        .unwrap();
        assert_eq!(selected.account_ref.as_deref(), Some("ch2"));
    }

    #[test]
    fn command_line_accepts_json_after_subcommands_and_refresh_all() {
        let parsed =
            Cli::try_parse_from(["codex-usage", "usage", "refresh", "--all", "--json"]).unwrap();
        assert!(parsed.json);
        assert!(matches!(
            parsed.command,
            Command::Usage {
                command: UsageCommand::Refresh { all: true, .. }
            }
        ));
    }

    #[test]
    fn refresh_modes_do_not_fall_back_to_global_cli_or_local_usage() {
        let claude = account(CLAUDE, "cc1", "Claude", "claude@example.com", None, None);
        let config = provider_config_for(&claude, None);
        assert_eq!(config.claude_source_mode, ClaudeSourceMode::Web);

        let opencode = account(
            OPENCODE_GO,
            "oc1",
            "OpenCode",
            "opencode@example.com",
            None,
            None,
        );
        assert_eq!(
            provider_config_for(&opencode, None).opencode_go_source_mode,
            OpenCodeGoSourceMode::Api
        );
        let browser_material =
            AccountAuthMaterial::from_cookie_header("auth=browser-session", None);
        assert_eq!(
            provider_config_for(&opencode, Some(&browser_material)).opencode_go_source_mode,
            OpenCodeGoSourceMode::Web
        );
        let unrelated_cookie = AccountAuthMaterial::from_cookie_header("unrelated=value", None);
        assert_eq!(
            provider_config_for(&opencode, Some(&unrelated_cookie)).opencode_go_source_mode,
            OpenCodeGoSourceMode::Api
        );

        let antigravity = account(
            ANTIGRAVITY,
            "ag1",
            "Google",
            "google@example.com",
            None,
            None,
        );
        assert!(!provider_config_for(&antigravity, None).enable_antigravity_local_probe);
    }
}
