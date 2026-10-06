//! Command-line arguments and subcommands.

use super::*;

#[derive(Debug, Parser)]
#[command(
    name = "usage-monitor-cli",
    version,
    about = "Manage locally saved provider accounts and usage"
)]
pub(super) struct Cli {
    /// Use a different SQLite account database.
    #[arg(long, global = true)]
    pub(super) database: Option<PathBuf>,

    /// Write JSON results and structured runtime errors to stdout.
    #[arg(long, global = true)]
    pub(super) json: bool,

    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
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
pub(super) enum AccountCommand {
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
pub(super) struct AccountAddArgs {
    /// Provider: codex, claude, openrouter, opencode-go, antigravity, deepseek, or copilot.
    pub(super) provider: String,
    /// Optional display alias assigned after sign-in succeeds.
    #[arg(long)]
    pub(super) alias: Option<String>,
    /// Read the API key (OpenRouter, DeepSeek) or GitHub token (Copilot) from stdin instead of a command-line argument.
    #[arg(long, conflicts_with = "credentials_stdin")]
    pub(super) api_key_stdin: bool,
    /// Read the API key and, for OpenRouter, an optional management key from two stdin lines.
    #[arg(long, conflicts_with = "api_key_stdin")]
    pub(super) credentials_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum AliasCommand {
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
pub(super) struct SelectorArgs {
    pub(super) selector: String,
    /// Disambiguate an exact name or email match by provider.
    #[arg(long)]
    pub(super) provider: Option<String>,
    /// Disambiguate by exact workspace id or workspace name.
    #[arg(long)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Args)]
pub(super) struct AccountRemoveArgs {
    #[command(flatten)]
    pub(super) selection: SelectorArgs,
    /// Skip the interactive confirmation (required in JSON/non-interactive use).
    #[arg(short = 'y', long)]
    pub(super) yes: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum UsageCommand {
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
