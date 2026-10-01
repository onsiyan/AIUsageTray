use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, error::ErrorKind};
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::{self, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command as TokioCommand,
};
use usage_monitor_core::{
    accounts::{ANTIGRAVITY, AccountRecord, AccountStore, CLAUDE, OPENAI, OPENCODE_GO, OPENROUTER},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        AccountBrowserSessionRefresher, AccountOAuthMaterialProvider,
        CompositeAuthMaterialProvider, OAuthCredentialProviderRegistry, OAuthCredentialStore,
        StoredAuthMaterialProvider,
    },
    claude_oauth::ClaudeOAuthRefreshingAuthMaterialProvider,
    oauth_loopback::{CodexOAuthCallbackListenerFactory, LoopbackOAuthCallbackListenerFactory},
    oauth_service::OAuthAuthorizationService,
    opencode_go_oauth::OpenCodeGoOAuthRefreshingAuthMaterialProvider,
    providers::{
        antigravity,
        claude::ClaudeSourceMode,
        openai,
        opencode_go::OpenCodeGoSourceMode,
        registry::{ProviderRegistry, ProviderRegistryConfig, ProviderRegistryError},
    },
    refresh::{
        RefreshCadence, RefreshCoordinatorConfig, RefreshOutcome, RefreshReason, RefreshStatus,
    },
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::{ReqwestUsageHttpTransport, TransportError, UsageHttpTransport},
    usage::{
        UsageAdapter, UsageAdapterError, UsageProbeResult, UsageSnapshot, UsageSnapshotStore,
        UsageWindowKind,
    },
};
use usage_monitor_windows::{
    WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    WindowsDefaultBrowserLauncher, browser_cookies::WindowsBrowserCookieImporter,
};

const EXIT_REFRESH_FAILED: i32 = 6;
const EXIT_OPERATION_NOT_CONFIRMED: i32 = 5;
const ACCOUNT_REF_MARKER: &str = "CODEX_USAGE_ACCOUNT_REF=";
const ACCOUNT_ADD_CHILD_ENV: &str = "CODEX_USAGE_CLI_CHILD";

#[derive(Debug, Parser)]
#[command(
    name = "usage-monitor-cli",
    version,
    about = "Manage locally saved provider accounts and usage"
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
    /// Refresh and display usage for a provider account.
    Usage {
        #[command(subcommand)]
        command: UsageCommand,
    },
    /// Summarize account and cached-snapshot availability without networking.
    Status,
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Add an account using that provider's existing sign-in flow.
    Add(AccountAddArgs),
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
    /// Remove one account and its locally saved credentials and usage history.
    #[command(alias = "delete")]
    Remove(AccountRemoveArgs),
}

#[derive(Debug, Args)]
struct AccountAddArgs {
    /// Provider: codex, claude, openrouter, opencode-go, or antigravity.
    provider: String,
    /// Optional display alias assigned after sign-in succeeds.
    #[arg(long)]
    alias: Option<String>,
    /// Read OpenRouter's primary API key from stdin instead of a command-line argument.
    #[arg(long, conflicts_with = "credentials_stdin")]
    api_key_stdin: bool,
    /// Read OpenRouter's primary key and optional management key from two stdin lines.
    #[arg(long, conflicts_with = "api_key_stdin")]
    credentials_stdin: bool,
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

#[derive(Debug, Args)]
struct AccountRemoveArgs {
    #[command(flatten)]
    selection: SelectorArgs,
    /// Skip the interactive confirmation (required in JSON/non-interactive use).
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum UsageCommand {
    /// Keep the adaptive usage refresh scheduler running until this process exits.
    Watch,
    /// Refresh the selected account, then display its latest usage snapshot.
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
        AccountCommand::Add(arguments) => {
            execute_account_add(database_path, &account_store, arguments, json_output).await
        }
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
        AccountCommand::Remove(arguments) => {
            execute_account_remove(&account_store, arguments, json_output).await
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemovalConfirmationMode {
    Prompt,
    Confirmed,
}

fn removal_confirmation_mode(
    yes: bool,
    json_output: bool,
    interactive: bool,
) -> Result<RemovalConfirmationMode, CliFailure> {
    if yes {
        return Ok(RemovalConfirmationMode::Confirmed);
    }
    if json_output || !interactive {
        return Err(CliFailure::new(
            "confirmation_required",
            "Account removal needs confirmation. Re-run with --yes to confirm explicitly.",
            EXIT_OPERATION_NOT_CONFIRMED,
        ));
    }
    Ok(RemovalConfirmationMode::Prompt)
}

fn confirm_account_removal(
    account: &AccountRecord,
    yes: bool,
    json_output: bool,
) -> Result<(), CliFailure> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    match removal_confirmation_mode(yes, json_output, interactive)? {
        RemovalConfirmationMode::Confirmed => Ok(()),
        RemovalConfirmationMode::Prompt => {
            eprint!(
                "Remove local account {} ({}, {}) and its saved credentials and usage history? This does not revoke access with the provider. [y/N] ",
                account.account_ref.as_deref().unwrap_or("?"),
                provider_name(&account.provider_id),
                account.email,
            );
            std::io::stderr()
                .flush()
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                Ok(())
            } else {
                Err(CliFailure::new(
                    "operation_cancelled",
                    "Account removal cancelled; no changes were made.",
                    EXIT_OPERATION_NOT_CONFIRMED,
                ))
            }
        }
    }
}

async fn execute_account_remove(
    account_store: &Arc<dyn AccountStore>,
    arguments: AccountRemoveArgs,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let accounts = account_store
        .list()
        .await
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    let account = resolve_account(
        &accounts,
        &arguments.selection.selector,
        &SelectorFilters {
            provider: arguments.selection.provider.as_deref(),
            workspace: arguments.selection.workspace.as_deref(),
        },
    )?
    .clone();

    confirm_account_removal(&account, arguments.yes, json_output)?;

    let oauth_store = WindowsCredentialManagerStore;
    let auth_store = WindowsCredentialManagerAuthMaterialStore;
    remove_local_account_data(&account, account_store.as_ref(), &oauth_store, &auth_store).await?;

    if json_output {
        print_json(json!({
            "schema_version": 1,
            "removed": account_value(&account),
            "removed_local_usage_history": true,
            "provider_access_revoked": false,
        }));
    } else {
        println!(
            "Removed local account {} ({}). Its saved credentials and usage history were deleted; provider access was not revoked.",
            account.account_ref.as_deref().unwrap_or("?"),
            account.email,
        );
    }
    Ok(0)
}

async fn remove_local_account_data(
    account: &AccountRecord,
    account_store: &dyn AccountStore,
    oauth_store: &dyn OAuthCredentialStore,
    auth_store: &dyn AccountAuthMaterialStore,
) -> Result<(), CliFailure> {
    if let Err(error) = oauth_store.remove(account.id).await {
        return Err(CliFailure::new(
            "account_removal_failed",
            format!(
                "Could not remove the saved OAuth credential: {error}. The account record and usage history were left in place; credential state may be partial."
            ),
            1,
        ));
    }
    if let Err(error) = auth_store.remove(account.id).await {
        return Err(CliFailure::new(
            "account_removal_partial",
            format!(
                "The OAuth credential was removed, but saved authentication material could not be removed: {error}. The account record and usage history remain."
            ),
            1,
        ));
    }
    if let Err(error) = account_store.remove(account.id).await {
        return Err(CliFailure::new(
            "account_removal_partial",
            format!(
                "Saved credentials were removed, but the account record and usage history could not be removed: {error}."
            ),
            1,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountAddProvider {
    Codex,
    Claude,
    OpenRouter,
    OpenCodeGo,
    Antigravity,
}

impl AccountAddProvider {
    fn parse(value: &str) -> Option<Self> {
        match normalize_name(value).as_str() {
            "codex" | "openai" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "openrouter" => Some(Self::OpenRouter),
            "opencode" | "opencodego" => Some(Self::OpenCodeGo),
            "antigravity" => Some(Self::Antigravity),
            _ => None,
        }
    }

    fn provider_id(self) -> &'static str {
        match self {
            Self::Codex => OPENAI,
            Self::Claude => CLAUDE,
            Self::OpenRouter => OPENROUTER,
            Self::OpenCodeGo => OPENCODE_GO,
            Self::Antigravity => ANTIGRAVITY,
        }
    }

    /// The sign-in helper's subcommand for this provider.
    fn login_command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenRouter => "openrouter",
            Self::OpenCodeGo => "opencode-go",
            Self::Antigravity => "antigravity",
        }
    }
}

/// The sign-in helper shipped next to the CLI.
const LOGIN_HELPER_BINARY: &str = "usage-monitor-login";

fn build_account_add_arguments(
    provider: AccountAddProvider,
    database_path: &Path,
    arguments: &AccountAddArgs,
) -> Vec<std::ffi::OsString> {
    let mut result = vec![
        provider.login_command().into(),
        "--database".into(),
        database_path.as_os_str().to_owned(),
    ];
    if let Some(alias) = arguments
        .alias
        .as_deref()
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
    {
        result.extend(["--label".into(), alias.into()]);
    }

    match provider {
        AccountAddProvider::Codex => {}
        // The Claude probe owns its single official Claude Code OAuth login
        // flow and always uses the system-default browser.
        AccountAddProvider::Claude => {}
        AccountAddProvider::OpenRouter => {
            // Each explicit add gets its own credential slot; a repeated label
            // must not silently replace another key.
            result.push("--new".into());
            if arguments.api_key_stdin {
                result.push("--api-key-stdin".into());
            }
            if arguments.credentials_stdin {
                result.push("--credentials-stdin".into());
            }
        }
        AccountAddProvider::OpenCodeGo => {}
        AccountAddProvider::Antigravity => {
            // Open the normal login flow even when another Antigravity account
            // is already saved. Identity deduplication remains in the probe.
            result.push("--new".into());
        }
    }
    result
}

fn account_add_uses_stdin(provider: AccountAddProvider, arguments: &AccountAddArgs) -> bool {
    provider == AccountAddProvider::OpenRouter
        && (arguments.api_key_stdin || arguments.credentials_stdin)
}

fn has_openrouter_key_source(
    arguments: &AccountAddArgs,
    environment_api_key: Option<&str>,
) -> bool {
    arguments.api_key_stdin
        || arguments.credentials_stdin
        || environment_api_key.is_some_and(|key| !key.trim().is_empty())
}

fn login_helper_path(current_executable: &Path) -> PathBuf {
    let extension = std::env::consts::EXE_EXTENSION;
    let filename = if extension.is_empty() {
        LOGIN_HELPER_BINARY.to_owned()
    } else {
        format!("{LOGIN_HELPER_BINARY}.{extension}")
    };
    current_executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(filename)
}

async fn execute_account_add(
    database_path: &Path,
    account_store: &Arc<dyn AccountStore>,
    arguments: AccountAddArgs,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let Some(provider) = AccountAddProvider::parse(&arguments.provider) else {
        return Err(CliFailure::new(
            "unsupported_provider",
            format!(
                "Unsupported provider `{}`. Choose codex, claude, openrouter, opencode-go, or antigravity.",
                arguments.provider
            ),
            2,
        ));
    };

    if arguments
        .alias
        .as_deref()
        .is_some_and(|alias| alias.trim().is_empty())
    {
        return Err(CliFailure::new(
            "invalid_arguments",
            "--alias cannot be empty or whitespace.",
            2,
        ));
    }
    if provider != AccountAddProvider::OpenRouter
        && (arguments.api_key_stdin || arguments.credentials_stdin)
    {
        return Err(CliFailure::new(
            "invalid_arguments",
            "OpenRouter stdin credential options can only be used with `account add openrouter`.",
            2,
        ));
    }
    if provider == AccountAddProvider::OpenRouter
        && !has_openrouter_key_source(
            &arguments,
            std::env::var("OPENROUTER_API_KEY").ok().as_deref(),
        )
    {
        return Err(CliFailure::new(
            "openrouter_key_required",
            "Provide OPENROUTER_API_KEY in this process environment, or pass the key through --api-key-stdin / --credentials-stdin. Keys are never accepted as command-line arguments.",
            2,
        ));
    }

    let current_executable = std::env::current_exe().map_err(|error| {
        CliFailure::runtime(format!("Could not locate the CLI executable: {error}"))
    })?;
    let probe_path = login_helper_path(&current_executable);
    if !probe_path.is_file() {
        return Err(CliFailure::new(
            "provider_login_helper_missing",
            format!(
                "The provider login helper is missing next to the CLI: {}. Build or install the complete workspace so its provider helpers are bundled with usage-monitor-cli.",
                probe_path.display()
            ),
            1,
        ));
    }

    let mut command = TokioCommand::new(&probe_path);
    command
        .args(build_account_add_arguments(
            provider,
            database_path,
            &arguments,
        ))
        .env(ACCOUNT_ADD_CHILD_ENV, "1")
        .stdin(if account_add_uses_stdin(provider, &arguments) {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if provider != AccountAddProvider::OpenRouter {
        command
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("OPENROUTER_MANAGEMENT_API_KEY");
    }
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        CliFailure::runtime(format!(
            "Could not start the {} account login flow: {error}",
            provider_name(provider.provider_id())
        ))
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliFailure::runtime("Provider login helper stdout was not available."))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CliFailure::runtime("Provider login helper stderr was not available."))?;

    let stdout_task = tokio::spawn(forward_probe_stdout(stdout, json_output));
    let stderr_task = tokio::spawn(forward_probe_stderr(stderr));
    let status = child.wait().await.map_err(|error| {
        CliFailure::runtime(format!("Provider login flow could not finish: {error}"))
    })?;
    let account_refs = stdout_task
        .await
        .map_err(|error| {
            CliFailure::runtime(format!("Provider login output task failed: {error}"))
        })?
        .map_err(|error| {
            CliFailure::runtime(format!("Could not read provider login output: {error}"))
        })?;
    stderr_task
        .await
        .map_err(|error| {
            CliFailure::runtime(format!("Provider diagnostic output task failed: {error}"))
        })?
        .map_err(|error| {
            CliFailure::runtime(format!("Could not read provider diagnostics: {error}"))
        })?;

    let account_ref = match account_refs.as_slice() {
        [account_ref] => Some(account_ref.as_str()),
        [] => None,
        _ => {
            return Err(CliFailure::runtime(
                "Provider login helper returned more than one account reference.",
            ));
        }
    };

    if !status.success() {
        let mut failure = if let Some(account_ref) = account_ref {
            CliFailure::new(
                "account_saved_but_validation_failed",
                format!(
                    "The provider login failed validation after saving {account_ref}. The account/session was retained; retry with `usage refresh {account_ref}` after resolving the provider issue."
                ),
                EXIT_REFRESH_FAILED,
            )
        } else {
            CliFailure::new(
                "account_add_failed",
                "The provider account login did not complete. See the provider diagnostics above.",
                1,
            )
        };
        if let Some(account_ref) = account_ref {
            failure.candidates.push(account_ref.to_owned());
        }
        return Err(failure);
    }

    let account_ref = account_ref.ok_or_else(|| {
        CliFailure::runtime(
            "Provider login succeeded but did not return its stable account reference.",
        )
    })?;
    let accounts = account_store
        .list()
        .await
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    let mut account = resolve_account(
        &accounts,
        account_ref,
        &SelectorFilters {
            provider: Some(provider.provider_id()),
            workspace: None,
        },
    )?
    .clone();

    if let Some(alias) = arguments.alias.as_deref() {
        account = account_store
            .set_alias(account.id, Some(alias.trim()))
            .await
            .map_err(|error| CliFailure::runtime(error.to_string()))?
            .ok_or_else(|| {
                CliFailure::new(
                    "account_not_found",
                    "The saved account disappeared while assigning its alias.",
                    3,
                )
            })?;
    }

    if json_output {
        print_json(json!({
            "schema_version": 1,
            "result": "account_added",
            "account": account_value(&account),
        }));
    } else {
        print_account_details(&account);
    }
    Ok(0)
}

async fn forward_probe_stdout<R>(stream: R, json_output: bool) -> std::io::Result<Vec<String>>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    let mut account_refs = Vec::new();
    while let Some(line) = lines.next_line().await? {
        if let Some(account_ref) = line.strip_prefix(ACCOUNT_REF_MARKER) {
            account_refs.push(account_ref.trim().to_owned());
        } else if json_output {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    Ok(account_refs)
}

async fn forward_probe_stderr<R>(stream: R) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        eprintln!("{line}");
    }
    Ok(())
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

async fn run_usage_scheduler(
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
    let session_refresher = Arc::new(
        WindowsBrowserCookieImporter::from_process()
            .map_err(|error| CliFailure::runtime(error.to_string()))?,
    ) as Arc<dyn AccountBrowserSessionRefresher>;
    let providers = per_account_source_registry(transport, auth, auth_store, session_refresher)
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    let runtime = UsageRuntime::new(
        account_store,
        snapshot_store,
        Arc::new(providers),
        RefreshCoordinatorConfig {
            cadence: RefreshCadence::Adaptive,
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

async fn collect_refresh_results(
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
        results.push((account.clone(), outcome));
    }
    Ok(results)
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

/// Source modes that `provider_config_for` can select for one account.
const SOURCE_VARIANTS: [(ClaudeSourceMode, OpenCodeGoSourceMode); 4] = [
    (ClaudeSourceMode::OAuth, OpenCodeGoSourceMode::Web),
    (ClaudeSourceMode::OAuth, OpenCodeGoSourceMode::Api),
    (ClaudeSourceMode::AdminApi, OpenCodeGoSourceMode::Web),
    (ClaudeSourceMode::AdminApi, OpenCodeGoSourceMode::Api),
];

/// Builds the long-running scheduler's providers so each account is probed
/// with the same per-account source selection as `usage refresh`, instead of
/// one global Claude/OpenCode Go mode for every account.
fn per_account_source_registry(
    transport: Arc<ReqwestUsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    auth_store: Arc<WindowsCredentialManagerAuthMaterialStore>,
    session_refresher: Arc<dyn AccountBrowserSessionRefresher>,
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
    for (claude_source_mode, opencode_go_source_mode) in SOURCE_VARIANTS {
        let registry = ProviderRegistry::from_dependencies_with_auth_store_and_session_refresher(
            Arc::clone(&transport),
            Arc::clone(&auth),
            Arc::clone(&store),
            Arc::clone(&session_refresher),
            ProviderRegistryConfig {
                claude_source_mode,
                opencode_go_source_mode,
                enable_antigravity_local_probe: false,
                ..ProviderRegistryConfig::default()
            },
        )?;
        variants.push(((claude_source_mode, opencode_go_source_mode), registry));
    }
    let base = &variants[0].1;
    let adapters = base
        .canonical_ids()
        .iter()
        .filter_map(|provider_id| {
            if provider_id == CLAUDE || provider_id == OPENCODE_GO {
                Some(Arc::new(PerAccountSourceAdapter {
                    provider_id: provider_id.clone(),
                    auth_store: Arc::clone(&auth_store),
                    variants: variants
                        .iter()
                        .filter_map(|(modes, registry)| Some((*modes, registry.get(provider_id)?)))
                        .collect(),
                }) as Arc<dyn UsageAdapter>)
            } else {
                base.get(provider_id)
            }
        })
        .collect::<Vec<_>>();
    ProviderRegistry::from_adapters(adapters)
}

struct PerAccountSourceAdapter {
    provider_id: String,
    auth_store: Arc<WindowsCredentialManagerAuthMaterialStore>,
    variants: Vec<(
        (ClaudeSourceMode, OpenCodeGoSourceMode),
        Arc<dyn UsageAdapter>,
    )>,
}

#[async_trait::async_trait]
impl UsageAdapter for PerAccountSourceAdapter {
    fn adapter_id(&self) -> &str {
        &self.provider_id
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = self.auth_store.get(account.id).await.ok().flatten();
        let config = provider_config_for(account, material.as_ref());
        // Only this provider's own source mode is meaningful for the account.
        let adapter = self
            .variants
            .iter()
            .find(|((claude_mode, opencode_go_mode), _)| {
                if self.provider_id == CLAUDE {
                    *claude_mode == config.claude_source_mode
                } else {
                    *opencode_go_mode == config.opencode_go_source_mode
                }
            })
            .or_else(|| self.variants.first())
            .map(|(_, adapter)| adapter)
            .expect("source variants are registered");
        adapter.probe(account).await
    }
}

fn provider_config_for(
    account: &AccountRecord,
    material: Option<&AccountAuthMaterial>,
) -> ProviderRegistryConfig {
    ProviderRegistryConfig::for_account(account, material)
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

fn window_value(key: &str, window: &usage_monitor_core::usage::RateLimitWindow) -> Value {
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
    outcome: &usage_monitor_core::refresh::RefreshOutcome,
) -> Value {
    json!({
        "account_ref": account.account_ref,
        "provider": provider_name(&account.provider_id),
        "provider_id": account.provider_id,
        "status": refresh_status_name(outcome.status),
        "completed_at_utc": outcome.completed_at_utc,
        "snapshot": outcome.snapshot.as_ref().map(snapshot_value),
        "error": outcome.error.as_ref().map(refresh_error_value),
        "storage_error": outcome.storage_error,
    })
}

fn usage_get_value(account: &AccountRecord, outcome: &RefreshOutcome) -> Value {
    json!({
        "schema_version": 1,
        "account": account_value(account),
        "snapshot": outcome.snapshot.as_ref().map(snapshot_value),
        "refresh": {
            "status": refresh_status_name(outcome.status),
            "completed_at_utc": outcome.completed_at_utc,
            "error": outcome.error.as_ref().map(refresh_error_value),
            "storage_error": outcome.storage_error,
        },
    })
}

fn refresh_error_value(error: &UsageAdapterError) -> Value {
    json!({
        "code": format!("{:?}", error.code).to_ascii_lowercase(),
        "message": error.message,
        "http_status_code": error.http_status_code,
        "retry_after_seconds": error.retry_after_seconds,
    })
}

fn usage_get_exit_code(status: RefreshStatus) -> i32 {
    if status == RefreshStatus::Updated {
        0
    } else {
        EXIT_REFRESH_FAILED
    }
}

fn print_usage_get(account: &AccountRecord, outcome: &RefreshOutcome) {
    if let Some(snapshot) = &outcome.snapshot {
        print_usage(account, snapshot);
    } else {
        println!(
            "{}\t{}\t{}",
            account.account_ref.as_deref().unwrap_or("?"),
            provider_name(&account.provider_id),
            account.display_name()
        );
        println!("No current usage snapshot is available.");
    }
    println!("Refresh: {}", refresh_status_name(outcome.status));
    if let Some(error) = &outcome.error {
        println!(
            "Refresh error: {}: {}",
            format!("{:?}", error.code).to_ascii_lowercase(),
            error.message
        );
    }
    if let Some(error) = &outcome.storage_error {
        println!("Storage error: {error}");
    }
}

fn account_status_name(account: &AccountRecord) -> &'static str {
    use usage_monitor_core::accounts::AccountStatus;
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

fn print_window(key: &str, window: &usage_monitor_core::usage::RateLimitWindow) {
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
mod tests;
