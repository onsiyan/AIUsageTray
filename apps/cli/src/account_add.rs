//! `account add`: runs the provider sign-in helper and relays its output.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccountAddProvider {
    Codex,
    Claude,
    OpenRouter,
    OpenCodeGo,
    Antigravity,
    DeepSeek,
    Copilot,
    Cursor,
    Kimi,
    Zai,
    Xai,
    MiniMax,
    MiMo,
}

impl AccountAddProvider {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match normalize_name(value).as_str() {
            "codex" | "openai" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "openrouter" => Some(Self::OpenRouter),
            "opencode" | "opencodego" => Some(Self::OpenCodeGo),
            "antigravity" => Some(Self::Antigravity),
            "deepseek" => Some(Self::DeepSeek),
            "copilot" | "githubcopilot" => Some(Self::Copilot),
            "cursor" => Some(Self::Cursor),
            "kimi" | "kimicode" => Some(Self::Kimi),
            "zai" | "z.ai" | "glm" => Some(Self::Zai),
            "xai" | "x.ai" => Some(Self::Xai),
            "minimax" => Some(Self::MiniMax),
            "mimo" | "xiaomi" => Some(Self::MiMo),
            _ => None,
        }
    }

    pub(super) fn provider_id(self) -> &'static str {
        match self {
            Self::Codex => OPENAI,
            Self::Claude => CLAUDE,
            Self::OpenRouter => OPENROUTER,
            Self::OpenCodeGo => OPENCODE_GO,
            Self::Antigravity => ANTIGRAVITY,
            Self::DeepSeek => DEEPSEEK,
            Self::Copilot => COPILOT,
            Self::Cursor => CURSOR,
            Self::Kimi => KIMI,
            Self::Zai => ZAI,
            Self::Xai => XAI,
            Self::MiniMax => MINIMAX,
            Self::MiMo => MIMO,
        }
    }

    /// The sign-in helper's subcommand for this provider.
    pub(super) fn login_command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenRouter => "openrouter",
            Self::OpenCodeGo => "opencode-go",
            Self::Antigravity => "antigravity",
            Self::DeepSeek => "deepseek",
            Self::Copilot => "copilot",
            Self::Cursor => "cursor",
            Self::Kimi => "kimi",
            Self::Zai => "zai",
            Self::Xai => "xai",
            Self::MiniMax => "minimax",
            Self::MiMo => "mimo",
        }
    }

    /// The environment variable that may hold the API key of a provider
    /// added from a key; `None` for providers that sign in.
    pub(super) fn api_key_environment(self) -> Option<&'static str> {
        match self {
            Self::OpenRouter => Some("OPENROUTER_API_KEY"),
            Self::DeepSeek => Some("DEEPSEEK_API_KEY"),
            Self::Kimi => Some("KIMI_CODE_API_KEY"),
            Self::Zai => Some("Z_AI_API_KEY"),
            Self::Xai => Some("XAI_MANAGEMENT_API_KEY"),
            Self::MiniMax => Some("MINIMAX_CODING_API_KEY"),
            Self::MiMo => Some("MIMO_COOKIE"),
            _ => None,
        }
    }

    /// Whether the credential may come from stdin: an API key, or the
    /// GitHub token of a Copilot device-flow sign-in finished elsewhere, or
    /// a Cursor session cookie copied from cursor.com.
    pub(super) fn accepts_stdin_credentials(self) -> bool {
        self.api_key_environment().is_some() || matches!(self, Self::Copilot | Self::Cursor)
    }
}

/// Every provider's API-key variables, kept away from other providers'
/// login flows.
const API_KEY_ENVIRONMENT: &[&str] = &[
    "OPENROUTER_API_KEY",
    "OPENROUTER_MANAGEMENT_API_KEY",
    "DEEPSEEK_API_KEY",
    "KIMI_CODE_API_KEY",
    "Z_AI_API_KEY",
    "XAI_MANAGEMENT_API_KEY",
    "XAI_TEAM_ID",
    "MINIMAX_CODING_API_KEY",
    "MIMO_COOKIE",
];

/// The sign-in helper shipped next to the CLI.
pub(super) const LOGIN_HELPER_BINARY: &str = "usage-monitor-login";

pub(super) fn build_account_add_arguments(
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
        AccountAddProvider::OpenRouter
        | AccountAddProvider::DeepSeek
        | AccountAddProvider::Kimi
        | AccountAddProvider::Zai
        | AccountAddProvider::Xai
        | AccountAddProvider::MiniMax
        | AccountAddProvider::MiMo => {
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
        // Without a credential on stdin, the Copilot helper runs GitHub's
        // device flow in the terminal and the Cursor helper takes the signed-in
        // Cursor app's session. Both match accounts by user, never doubling.
        AccountAddProvider::Copilot | AccountAddProvider::Cursor => {
            if arguments.api_key_stdin || arguments.credentials_stdin {
                result.push("--credentials-stdin".into());
            }
        }
        AccountAddProvider::Antigravity => {
            // Open the normal login flow even when another Antigravity account
            // is already saved. Identity deduplication remains in the probe.
            result.push("--new".into());
        }
    }
    result
}

pub(super) fn account_add_uses_stdin(
    provider: AccountAddProvider,
    arguments: &AccountAddArgs,
) -> bool {
    provider.accepts_stdin_credentials() && (arguments.api_key_stdin || arguments.credentials_stdin)
}

pub(super) fn has_api_key_source(
    arguments: &AccountAddArgs,
    environment_api_key: Option<&str>,
) -> bool {
    arguments.api_key_stdin
        || arguments.credentials_stdin
        || environment_api_key.is_some_and(|key| !key.trim().is_empty())
}

pub(super) fn login_helper_path(current_executable: &Path) -> PathBuf {
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

pub(super) async fn execute_account_add(
    database_path: &Path,
    account_store: &Arc<dyn AccountStore>,
    arguments: AccountAddArgs,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let Some(provider) = AccountAddProvider::parse(&arguments.provider) else {
        return Err(CliFailure::new(
            "unsupported_provider",
            format!(
                "Unsupported provider `{}`. Choose codex, claude, openrouter, opencode-go, antigravity, deepseek, copilot, cursor, kimi, zai, xai, or minimax.",
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
    let api_key_environment = provider.api_key_environment();
    if !provider.accepts_stdin_credentials()
        && (arguments.api_key_stdin || arguments.credentials_stdin)
    {
        return Err(CliFailure::new(
            "invalid_arguments",
            "Stdin credential options can only be used with `account add openrouter`, `account add deepseek`, `account add kimi`, `account add zai`, `account add xai`, `account add minimax`, `account add copilot`, or `account add cursor`.",
            2,
        ));
    }
    if let Some(variable) = api_key_environment
        && !has_api_key_source(&arguments, std::env::var(variable).ok().as_deref())
    {
        return Err(CliFailure::new(
            "api_key_required",
            format!(
                "Provide {variable} in this process environment, or pass the key through --api-key-stdin / --credentials-stdin. Keys are never accepted as command-line arguments."
            ),
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
    for variable in API_KEY_ENVIRONMENT {
        let belongs_to_provider = match provider {
            AccountAddProvider::OpenRouter => variable.starts_with("OPENROUTER_"),
            AccountAddProvider::DeepSeek => variable.starts_with("DEEPSEEK_"),
            AccountAddProvider::Kimi => variable.starts_with("KIMI_"),
            AccountAddProvider::Zai => variable.starts_with("Z_AI_"),
            AccountAddProvider::Xai => variable.starts_with("XAI_"),
            AccountAddProvider::MiniMax => variable.starts_with("MINIMAX_"),
            AccountAddProvider::MiMo => variable.starts_with("MIMO_"),
            _ => false,
        };
        if !belongs_to_provider {
            command.env_remove(variable);
        }
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

pub(super) async fn forward_probe_stdout<R>(
    stream: R,
    json_output: bool,
) -> std::io::Result<Vec<String>>
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

pub(super) async fn forward_probe_stderr<R>(stream: R) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        eprintln!("{line}");
    }
    Ok(())
}
