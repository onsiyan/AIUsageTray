use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, error::ErrorKind};
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::{self, Stdio},
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command as TokioCommand,
};
use usage_monitor_core::{
    accounts::{
        ANTIGRAVITY, AccountRecord, AccountStore, CLAUDE, COPILOT, CURSOR, DEEPSEEK, KIMI, MIMO,
        MINIMAX, OPENAI, OPENCODE_GO, OPENROUTER, XAI, ZAI,
    },
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        AccountOAuthMaterialProvider, CompositeAuthMaterialProvider,
        OAuthCredentialProviderRegistry, OAuthCredentialStore, StoredAuthMaterialProvider,
    },
    claude_oauth::ClaudeOAuthRefreshingAuthMaterialProvider,
    oauth_loopback::{CodexOAuthCallbackListenerFactory, LoopbackOAuthCallbackListenerFactory},
    oauth_service::OAuthAuthorizationService,
    opencode_go_oauth::OpenCodeGoOAuthRefreshingAuthMaterialProvider,
    providers::{
        antigravity, openai,
        opencode_go::OpenCodeGoSourceMode,
        registry::{ProviderRegistry, ProviderRegistryConfig, ProviderRegistryError},
    },
    refresh::{
        RefreshCadence, RefreshCoordinatorConfig, RefreshOutcome, RefreshReason, RefreshStatus,
    },
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::{
        ReqwestUsageHttpTransport, TransportError, USAGE_REQUEST_TIMEOUT, UsageHttpTransport,
    },
    usage::{
        UsageAdapter, UsageAdapterError, UsageProbeResult, UsageSnapshot, UsageSnapshotStore,
        UsageWindowKind,
    },
};
use usage_monitor_windows::{
    WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    WindowsDefaultBrowserLauncher,
};

const EXIT_REFRESH_FAILED: i32 = 6;
const EXIT_OPERATION_NOT_CONFIRMED: i32 = 5;
const ACCOUNT_REF_MARKER: &str = "USAGE_MONITOR_ACCOUNT_REF=";
const ACCOUNT_ADD_CHILD_ENV: &str = "USAGE_MONITOR_CLI_CHILD";
mod account;
mod account_add;
mod args;
mod cost;
mod output;
mod selection;
mod usage;

use account::*;
use account_add::*;
use args::*;
use output::*;
use selection::*;
use usage::*;

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
        Command::Cost { offline } => cost::execute_cost(json_output, !offline).await,
        Command::Reset { yes } => execute_reset(yes, json_output),
    }
}

/// Removes the app's credentials and data folders.
fn execute_reset(yes: bool, json_output: bool) -> Result<i32, CliFailure> {
    if !yes {
        return Err(CliFailure::new(
            "confirmation_required",
            "Reset removes every saved account and key. Re-run with --yes to confirm.",
            5,
        ));
    }
    let credentials = usage_monitor_windows::remove_all_credentials()
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    let mut folders = Vec::new();
    for variable in ["LOCALAPPDATA", "APPDATA"] {
        let Some(root) = std::env::var_os(variable) else {
            continue;
        };
        let folder = PathBuf::from(root).join("UsageMonitor");
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            // Codex's sign-ins as they were before the app replaced them.
            if entry.file_name() == "codex-auth-backups" {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            removed.map_err(|error| {
                CliFailure::runtime(format!("could not remove {}: {error}", path.display()))
            })?;
        }
        // Gone unless backups are left in it.
        let _ = std::fs::remove_dir(&folder);
        folders.push(folder.display().to_string());
    }
    if json_output {
        print_json(json!({
            "schema_version": 1,
            "removed_credentials": credentials,
            "cleared_folders": folders,
        }));
    } else {
        println!("Removed {credentials} saved credentials and the app's data.");
    }
    Ok(0)
}

#[cfg(test)]
mod tests;
