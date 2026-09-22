//! Claude Code CLI usage probing.
//!
//! The CLI does not expose the quota panel as a stable JSON endpoint. The
//! supported path is therefore the same one used by the provider's own
//! desktop integration: run the real CLI inside a pseudo terminal, send
//! `/usage`, keep the terminal alive while the panel renders, and parse the
//! rendered quota rows. The probe is deliberately isolated from the user's
//! project and disables updater/MCP side effects.

use crate::usage::{AdditionalRateLimitWindow, RateLimitWindow, UsageWindowKind};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;
use uuid::Uuid;

const SESSION_WINDOW_SECONDS: i64 = 5 * 60 * 60;
const WEEKLY_WINDOW_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct ClaudeCliUsage {
    pub primary: RateLimitWindow,
    pub secondary: Option<RateLimitWindow>,
    pub additional_windows: Vec<AdditionalRateLimitWindow>,
    pub observed_email: Option<String>,
    pub organization: Option<String>,
    pub plan_type: Option<String>,
    pub raw_text: String,
}

#[derive(Debug, Error, Clone)]
pub enum ClaudeCliError {
    #[error("Claude CLI is not installed or not on PATH")]
    NotInstalled,
    #[error("Claude CLI is not logged in")]
    NotLoggedIn,
    #[error("Claude CLI PTY launch failed: {0}")]
    Launch(String),
    #[error("Claude CLI usage probe timed out")]
    TimedOut,
    #[error("Claude CLI process exited before usage was rendered")]
    ProcessExited,
    #[error("Claude CLI output was too large")]
    OutputTooLarge,
    #[error("Claude CLI usage could not be parsed: {0}")]
    Parse(String),
    #[error("Claude CLI usage endpoint is rate limited")]
    RateLimited,
}

#[derive(Debug, Clone, Copy)]
pub struct ClaudeCliProbeOptions {
    pub timeout: Duration,
    pub retry_timeout: Duration,
    pub use_background_cache: bool,
    /// Manual refreshes bypass the provider cooldown and background cache.
    pub user_initiated: bool,
}

impl Default for ClaudeCliProbeOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(24),
            retry_timeout: Duration::from_secs(60),
            use_background_cache: false,
            user_initiated: true,
        }
    }
}

impl ClaudeCliProbeOptions {
    pub fn automatic() -> Self {
        Self {
            timeout: Duration::from_secs(12),
            retry_timeout: Duration::from_secs(60),
            use_background_cache: true,
            user_initiated: false,
        }
    }

    pub fn explicit() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeCliAuthStatus {
    LoggedIn,
    LoggedOut,
    Unavailable,
}

/// Performs the cheap non-interactive auth check before launching the full
/// TUI. An unsupported response is `Unavailable` so older CLI versions still
/// fall through to the real `/usage` probe.
pub async fn auth_status(
    environment: &HashMap<String, String>,
    timeout: Duration,
) -> Result<ClaudeCliAuthStatus, ClaudeCliError> {
    let environment = environment.clone();
    tokio::task::spawn_blocking(move || auth_status_blocking(&environment, timeout))
        .await
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?
}

fn auth_status_blocking(
    environment: &HashMap<String, String>,
    timeout: Duration,
) -> Result<ClaudeCliAuthStatus, ClaudeCliError> {
    let binary = resolve_binary(environment).ok_or(ClaudeCliError::NotInstalled)?;
    let working_directory = probe_working_directory();
    let _cleanup = ProbeDirectoryGuard(working_directory.clone());
    let args = ["auth".to_owned(), "status".to_owned(), "--json".to_owned()];
    let output = run_direct_command(&binary, environment, &working_directory, &args, timeout)?;
    Ok(parse_auth_status(&output).unwrap_or(ClaudeCliAuthStatus::Unavailable))
}

fn parse_auth_status(output: &str) -> Option<ClaudeCliAuthStatus> {
    let clean = strip_terminal_sequences(output);
    if let Some(status) = parse_auth_status_value(clean.trim()) {
        return Some(status);
    }
    if let (Some(start), Some(end)) = (clean.find('{'), clean.rfind('}'))
        && start < end
        && let Some(status) = parse_auth_status_value(&clean[start..=end])
    {
        return Some(status);
    }
    for line in clean.lines().rev() {
        if let Some(status) = parse_auth_status_value(line.trim()) {
            return Some(status);
        }
    }
    None
}

fn parse_auth_status_value(value: &str) -> Option<ClaudeCliAuthStatus> {
    let root = serde_json::from_str::<Value>(value).ok()?;
    let logged_in = root
        .get("loggedIn")
        .or_else(|| root.get("logged_in"))
        .and_then(Value::as_bool)?;
    Some(if logged_in {
        ClaudeCliAuthStatus::LoggedIn
    } else {
        ClaudeCliAuthStatus::LoggedOut
    })
}

const CLI_STATE_FILE_ENV: &str = "CODEX_USAGE_CLAUDE_STATE_FILE";
const CLI_STATE_FILE_NAME: &str = "claude-cli-state.json";

/// Returns the process-independent cooldown remaining for the CLI usage
/// endpoint. This state contains only a timestamp; no credentials or usage
/// payloads are written to disk.
pub fn persisted_rate_limit_remaining(environment: &HashMap<String, String>) -> Option<i64> {
    let path = state_file_path(environment)?;
    let body = fs::read_to_string(path).ok()?;
    let root: Value = serde_json::from_str(&body).ok()?;
    let value = root
        .get("cli_rate_limit_until_utc")
        .and_then(Value::as_str)?;
    let until = DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&Utc);
    let remaining = (until - Utc::now()).num_seconds();
    (remaining > 0).then_some(remaining)
}

/// Persists the CLI cooldown best-effort. A failure to write this small
/// advisory state must never turn a successful usage probe into an error.
pub fn record_persisted_rate_limit(environment: &HashMap<String, String>, seconds: i64) {
    let Some(path) = state_file_path(environment) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let body = serde_json::json!({
        "cli_rate_limit_until_utc": (Utc::now() + ChronoDuration::seconds(seconds.max(1)))
            .to_rfc3339(),
    });
    let _ = fs::write(path, body.to_string());
}

pub fn clear_persisted_rate_limit(environment: &HashMap<String, String>) {
    if let Some(path) = state_file_path(environment) {
        let _ = fs::remove_file(path);
    }
}

fn state_file_path(environment: &HashMap<String, String>) -> Option<PathBuf> {
    if let Some(path) = environment
        .get(CLI_STATE_FILE_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(path));
    }
    let root = environment
        .get("LOCALAPPDATA")
        .cloned()
        .or_else(|| environment.get("XDG_STATE_HOME").cloned())
        .or_else(|| env::var("LOCALAPPDATA").ok())
        .or_else(|| env::var("XDG_STATE_HOME").ok())?;
    Some(
        PathBuf::from(root)
            .join("CodexUsageMonitor")
            .join(CLI_STATE_FILE_NAME),
    )
}

/// Returns the CLI executable selected by the explicit environment override
/// or by the current process PATH. The resolver accepts npm's `.cmd` shim on
/// Windows and executes it through `cmd.exe` inside the PTY.
pub fn resolve_binary(environment: &HashMap<String, String>) -> Option<PathBuf> {
    if let Some(explicit) = environment
        .get("CLAUDE_CLI_PATH")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }

    let path_value = environment
        .get("PATH")
        .cloned()
        .or_else(|| env::var("PATH").ok())
        .unwrap_or_default();
    for directory in env::split_paths(&OsString::from(path_value)) {
        for candidate in candidate_names(&directory) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let resolver = if cfg!(windows) { "where.exe" } else { "which" };
    let output = Command::new(resolver).arg("claude").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

pub fn is_available(environment: &HashMap<String, String>) -> bool {
    resolve_binary(environment).is_some()
}

/// Result of the provider-owned Claude login command.
///
/// Claude Code owns the authorization URL and callback contract. The host must
/// therefore launch the official `claude auth login --claudeai` command rather
/// than constructing an undocumented OAuth URL itself. The command opens the
/// user's default browser; this result only exposes the non-secret terminal
/// output and, when present, the one-time authorization URL printed by the CLI.
#[derive(Debug, Clone)]
pub struct ClaudeCliLoginResult {
    pub output: String,
    pub auth_link: Option<String>,
}

/// Runs Claude Code's official browser login flow in an isolated PTY.
///
/// The PTY is needed because Claude Code may prompt with "press ENTER to open
/// in browser" and may render the success marker only after the browser flow
/// returns. No credentials are read or printed by this function.
pub async fn login(
    environment: &HashMap<String, String>,
    timeout: Duration,
) -> Result<ClaudeCliLoginResult, ClaudeCliError> {
    let environment = environment.clone();
    tokio::task::spawn_blocking(move || login_blocking(&environment, timeout))
        .await
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?
}

fn login_blocking(
    environment: &HashMap<String, String>,
    timeout: Duration,
) -> Result<ClaudeCliLoginResult, ClaudeCliError> {
    let binary = resolve_binary(environment).ok_or(ClaudeCliError::NotInstalled)?;
    let working_directory = probe_working_directory();
    let _cleanup = ProbeDirectoryGuard(working_directory.clone());
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let arguments = [
        "auth".to_owned(),
        "login".to_owned(),
        "--claudeai".to_owned(),
    ];
    let command = command_builder(&binary, environment, &working_directory, &arguments);
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(Ok(buffer[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });

    const SUCCESS_MARKERS: [&str; 3] = [
        "successfullyloggedin",
        "loginsuccessful",
        "loggedinsuccessfully",
    ];
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut enter_sent = false;
    let mut success_seen_at: Option<Instant> = None;
    let mut process_status = None;

    loop {
        if output.len() > MAX_OUTPUT_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ClaudeCliError::OutputTooLarge);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ClaudeCliError::TimedOut);
        }
        if let Some(recorded) = success_seen_at
            && recorded.elapsed() >= Duration::from_millis(350)
        {
            break;
        }

        match receiver.recv_timeout(Duration::from_millis(80)) {
            Ok(Ok(chunk)) => {
                output.extend_from_slice(&chunk);
                let text = String::from_utf8_lossy(&output);
                let normalized = normalize_terminal_text(&text);
                if !enter_sent && normalized.contains("pressentertoopeninbrowser") {
                    write_pty(&mut writer, "\r")?;
                    enter_sent = true;
                }
                if success_seen_at.is_none()
                    && SUCCESS_MARKERS
                        .iter()
                        .any(|marker| normalized.contains(marker))
                {
                    success_seen_at = Some(Instant::now());
                }
            }
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ClaudeCliError::Launch(error));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some(status) = child
            .try_wait()
            .map_err(|error| ClaudeCliError::Launch(error.to_string()))?
        {
            process_status = Some(status);
            break;
        }
    }

    if process_status.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    drop(writer);
    drop(pair.master);
    if output.is_empty() {
        return Err(ClaudeCliError::ProcessExited);
    }

    let text = String::from_utf8_lossy(&output).into_owned();
    let success = success_seen_at.is_some()
        || process_status
            .as_ref()
            .is_some_and(|status| status.success());
    if !success {
        let code = process_status
            .as_ref()
            .map(|status| status.exit_code())
            .unwrap_or(1);
        return Err(ClaudeCliError::Launch(format!(
            "Claude login exited with status {code}"
        )));
    }
    Ok(ClaudeCliLoginResult {
        auth_link: first_http_url(&text),
        output: text,
    })
}

fn first_http_url(text: &str) -> Option<String> {
    let pattern = r#"https?://[A-Za-z0-9._~:/?#\[\]@!$&'()*+,;=%-]+"#;
    let regex = Regex::new(pattern).ok()?;
    let found = regex.find(text)?.as_str();
    Some(
        found
            .trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '"', '\''])
            .to_owned(),
    )
}

pub async fn probe(
    environment: &HashMap<String, String>,
    options: ClaudeCliProbeOptions,
) -> Result<ClaudeCliUsage, ClaudeCliError> {
    let environment = environment.clone();
    tokio::task::spawn_blocking(move || probe_blocking(&environment, options))
        .await
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?
}

fn probe_blocking(
    environment: &HashMap<String, String>,
    options: ClaudeCliProbeOptions,
) -> Result<ClaudeCliUsage, ClaudeCliError> {
    let binary = resolve_binary(environment).ok_or(ClaudeCliError::NotInstalled)?;
    let working_directory = probe_working_directory();
    let _cleanup = ProbeDirectoryGuard(working_directory.clone());
    let (usage_text, status_text) = match capture_warm_session(
        &binary,
        environment,
        &working_directory,
        options.timeout,
        options.retry_timeout.min(Duration::from_secs(12)),
    ) {
        Ok(captured) => captured,
        Err(_error @ (ClaudeCliError::TimedOut | ClaudeCliError::Parse(_))) => (
            capture_panel(
                &binary,
                environment,
                &working_directory,
                "/usage",
                options.retry_timeout.min(Duration::from_secs(8)),
            )
            .or_else(|_| {
                capture_direct(
                    &binary,
                    environment,
                    &working_directory,
                    options.retry_timeout.min(Duration::from_secs(8)),
                )
            })?,
            String::new(),
        ),
        Err(error) => return Err(error),
    };
    match parse_usage(&usage_text, &status_text, Utc::now()) {
        Ok(usage) => Ok(usage),
        Err(error @ ClaudeCliError::Parse(_)) => {
            let direct = capture_direct(
                &binary,
                environment,
                &working_directory,
                options.retry_timeout.min(Duration::from_secs(8)),
            )?;
            parse_usage(&direct, "", Utc::now()).or(Err(error))
        }
        Err(error) => Err(error),
    }
}

fn candidate_names(directory: &Path) -> [PathBuf; 4] {
    [
        directory.join("claude.exe"),
        directory.join("claude.cmd"),
        directory.join("claude.bat"),
        directory.join("claude"),
    ]
}

fn capture_panel(
    binary: &Path,
    environment: &HashMap<String, String>,
    working_directory: &Path,
    subcommand: &str,
    timeout: Duration,
) -> Result<String, ClaudeCliError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;

    let command_args = vec![
        "--allowed-tools".to_owned(),
        String::new(),
        "--strict-mcp-config".to_owned(),
        "--settings".to_owned(),
        r#"{"remoteControlAtStartup":false}"#.to_owned(),
        "--session-id".to_owned(),
        Uuid::new_v4().to_string().to_ascii_lowercase(),
    ];
    let command = command_builder(binary, environment, &working_directory, &command_args);

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(Ok(buffer[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });

    // Claude's TUI drops early slash commands while its account/configuration
    // bootstrap is still running. Let the PTY reach its interactive prompt in
    // the same way the reference probe does before sending `/usage` or
    // `/status`.
    thread::sleep(Duration::from_secs(2));
    write_pty(&mut writer, &format!("{subcommand}\r"))?;
    let deadline = Instant::now() + timeout;
    let mut last_enter = Instant::now();
    let mut last_output = Instant::now();
    let is_usage = subcommand.eq_ignore_ascii_case("/usage");
    let mut output = Vec::new();
    let mut stopped_at: Option<Instant> = None;
    let mut triggered = std::collections::HashSet::new();

    loop {
        if output.len() > MAX_OUTPUT_BYTES {
            let _ = child.kill();
            return Err(ClaudeCliError::OutputTooLarge);
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            return Err(ClaudeCliError::TimedOut);
        }
        if let Some(stopped) = stopped_at
            && now.duration_since(stopped) >= Duration::from_millis(1500)
        {
            break;
        }

        match receiver.recv_timeout(Duration::from_millis(80)) {
            Ok(Ok(chunk)) => {
                output.extend_from_slice(&chunk);
                last_output = Instant::now();
                let text = String::from_utf8_lossy(&output);
                let normalized = normalize_terminal_text(&text);
                for (needle, response) in prompt_responses() {
                    if !triggered.contains(needle) && normalized.contains(needle) {
                        write_pty(&mut writer, response)?;
                        triggered.insert(needle);
                    }
                }
                if stopped_at.is_none()
                    && ((is_usage
                        && (usage_capture_complete(&normalized)
                            || normalized.contains("failedtoloadusagedata")
                            || normalized.contains("currentlyusingyoursubscription")))
                        || (!is_usage
                            && (normalized.contains("email")
                                || normalized.contains("organization"))))
                {
                    stopped_at = Some(Instant::now());
                }
            }
            Ok(Err(error)) => {
                let _ = child.kill();
                return Err(ClaudeCliError::Launch(error));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if output.is_empty() {
                    return Err(ClaudeCliError::ProcessExited);
                }
                break;
            }
        }

        if stopped_at.is_none() && is_usage && last_enter.elapsed() >= Duration::from_millis(800) {
            write_pty(&mut writer, "\r")?;
            last_enter = Instant::now();
        }
        if stopped_at.is_none() && !is_usage && last_output.elapsed() >= Duration::from_secs(3) {
            stopped_at = Some(Instant::now());
        }
        if child
            .try_wait()
            .map_err(|error| ClaudeCliError::Launch(error.to_string()))?
            .is_some()
        {
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    drop(writer);
    drop(pair.master);
    if output.is_empty() {
        return Err(ClaudeCliError::ProcessExited);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// Captures `/usage` and `/status` through one PTY process. Claude's startup
/// is expensive and the TUI loses early commands; keeping the same process
/// alive for the identity enrichment mirrors the reference session lifecycle.
fn capture_warm_session(
    binary: &Path,
    environment: &HashMap<String, String>,
    working_directory: &Path,
    usage_timeout: Duration,
    status_timeout: Duration,
) -> Result<(String, String), ClaudeCliError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let command_args = vec![
        "--allowed-tools".to_owned(),
        String::new(),
        "--strict-mcp-config".to_owned(),
        "--settings".to_owned(),
        r#"{"remoteControlAtStartup":false}"#.to_owned(),
        "--session-id".to_owned(),
        Uuid::new_v4().to_string().to_ascii_lowercase(),
    ];
    let command = command_builder(binary, environment, working_directory, &command_args);
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(Ok(buffer[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });

    thread::sleep(Duration::from_secs(2));
    let result = (|| {
        let mut triggered = std::collections::HashSet::new();
        let usage = collect_session_panel(
            &mut writer,
            &receiver,
            "/usage",
            usage_timeout,
            true,
            &mut triggered,
        )?;
        // The usage panel redraws continuously. Drain only bytes already
        // queued before issuing the next command; future bytes belong to the
        // status capture and remain in the same warm session.
        while receiver.try_recv().is_ok() {}
        let status = collect_session_panel(
            &mut writer,
            &receiver,
            "/status",
            status_timeout,
            false,
            &mut triggered,
        )
        .unwrap_or_default();
        Ok::<_, ClaudeCliError>((usage, status))
    })();

    let _ = child.kill();
    let _ = child.wait();
    drop(writer);
    drop(pair.master);
    result
}

fn collect_session_panel(
    writer: &mut Box<dyn Write + Send>,
    receiver: &mpsc::Receiver<Result<Vec<u8>, String>>,
    subcommand: &str,
    timeout: Duration,
    is_usage: bool,
    triggered: &mut std::collections::HashSet<&'static str>,
) -> Result<String, ClaudeCliError> {
    write_pty(writer, &format!("{subcommand}\r"))?;
    let deadline = Instant::now() + timeout;
    let mut last_enter = Instant::now();
    let mut last_output = Instant::now();
    let mut output = Vec::new();
    let mut stopped_at = None;

    loop {
        if output.len() > MAX_OUTPUT_BYTES {
            return Err(ClaudeCliError::OutputTooLarge);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(ClaudeCliError::TimedOut);
        }
        if let Some(stopped) = stopped_at
            && now.duration_since(stopped)
                >= if is_usage {
                    Duration::from_millis(1500)
                } else {
                    Duration::from_millis(250)
                }
        {
            break;
        }

        match receiver.recv_timeout(Duration::from_millis(80)) {
            Ok(Ok(chunk)) => {
                output.extend_from_slice(&chunk);
                last_output = Instant::now();
                let text = String::from_utf8_lossy(&output);
                let normalized = normalize_terminal_text(&text);
                for (needle, response) in prompt_responses() {
                    if !triggered.contains(needle) && normalized.contains(needle) {
                        write_pty(writer, response)?;
                        triggered.insert(needle);
                    }
                }
                if stopped_at.is_none()
                    && ((is_usage
                        && (usage_capture_complete(&normalized)
                            || normalized.contains("failedtoloadusagedata")
                            || normalized.contains("currentlyusingyoursubscription")))
                        || (!is_usage
                            && (normalized.contains("email")
                                || normalized.contains("organization"))))
                {
                    stopped_at = Some(Instant::now());
                }
            }
            Ok(Err(error)) => return Err(ClaudeCliError::Launch(error)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if output.is_empty() {
                    return Err(ClaudeCliError::ProcessExited);
                }
                break;
            }
        }

        if stopped_at.is_none() && is_usage && last_enter.elapsed() >= Duration::from_millis(800) {
            write_pty(writer, "\r")?;
            last_enter = Instant::now();
        }
        if stopped_at.is_none() && !is_usage && last_output.elapsed() >= Duration::from_secs(3) {
            stopped_at = Some(Instant::now());
        }
    }

    if output.is_empty() {
        return Err(ClaudeCliError::ProcessExited);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// A bounded non-PTY fallback for versions of the CLI that render `/usage`
/// correctly in a one-shot invocation but stall when the interactive panel is
/// opened. This is attempted only after the PTY path times out or reports a
/// loading/parse failure.
fn capture_direct(
    binary: &Path,
    environment: &HashMap<String, String>,
    working_directory: &Path,
    timeout: Duration,
) -> Result<String, ClaudeCliError> {
    let direct_args = [
        "--settings".to_owned(),
        r#"{"remoteControlAtStartup":false}"#.to_owned(),
        "/usage".to_owned(),
    ];
    run_direct_command(
        binary,
        environment,
        working_directory,
        &direct_args,
        timeout,
    )
}

fn run_direct_command(
    binary: &Path,
    environment: &HashMap<String, String>,
    working_directory: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<String, ClaudeCliError> {
    let is_cmd_shim = cfg!(windows)
        && matches!(
            binary.extension().and_then(|extension| extension.to_str()),
            Some(extension) if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        );
    let mut command = if is_cmd_shim {
        let mut command = Command::new("cmd.exe");
        command.args(["/d", "/s", "/c"]);
        let mut command_line = quote_cmd_arg(&binary.to_string_lossy());
        for arg in args {
            command_line.push(' ');
            command_line.push_str(&quote_cmd_arg(arg));
        }
        command.arg(command_line);
        command
    } else {
        let mut command = Command::new(binary);
        command.args(args);
        command
    };
    command
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("DISABLE_AUTOUPDATER", "1");
    for key in environment
        .keys()
        .filter(|key| key.starts_with("ANTHROPIC_"))
    {
        command.env_remove(key);
    }
    command.env_remove("CLAUDE_OAUTH_TOKEN");
    command.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    for (key, value) in environment {
        if !key.starts_with("ANTHROPIC_")
            && key != "CLAUDE_OAUTH_TOKEN"
            && key != "CLAUDE_CODE_OAUTH_TOKEN"
        {
            command.env(key, value);
        }
    }

    let mut child = command
        .spawn()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .map_err(|error| ClaudeCliError::Launch(error.to_string()))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ClaudeCliError::TimedOut);
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child
        .wait_with_output()
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))?;
    let mut text = output.stdout;
    text.extend_from_slice(&output.stderr);
    if text.is_empty() {
        return Err(ClaudeCliError::ProcessExited);
    }
    Ok(String::from_utf8_lossy(&text).into_owned())
}

fn command_builder(
    binary: &Path,
    environment: &HashMap<String, String>,
    working_directory: &Path,
    args: &[String],
) -> CommandBuilder {
    let mut command = if cfg!(windows)
        && matches!(
            binary.extension().and_then(|extension| extension.to_str()),
            Some(extension) if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        ) {
        let mut command = CommandBuilder::new("cmd.exe");
        command.args(["/d", "/s", "/c"]);
        let mut command_line = quote_cmd_arg(&binary.to_string_lossy());
        for arg in args {
            command_line.push(' ');
            command_line.push_str(&quote_cmd_arg(arg));
        }
        command.arg(command_line);
        command
    } else {
        let mut command = CommandBuilder::new(binary);
        command.args(args);
        command
    };
    command.cwd(working_directory);
    command.env("DISABLE_AUTOUPDATER", "1");
    for key in environment
        .keys()
        .filter(|key| key.starts_with("ANTHROPIC_"))
    {
        command.env_remove(key);
    }
    command.env_remove("CLAUDE_OAUTH_TOKEN");
    command.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    for (key, value) in environment {
        if !key.starts_with("ANTHROPIC_")
            && key != "CLAUDE_OAUTH_TOKEN"
            && key != "CLAUDE_CODE_OAUTH_TOKEN"
        {
            command.env(key, value);
        }
    }
    command
}

fn quote_cmd_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn write_pty(writer: &mut Box<dyn Write + Send>, value: &str) -> Result<(), ClaudeCliError> {
    writer
        .write_all(value.as_bytes())
        .and_then(|_| writer.flush())
        .map_err(|error| ClaudeCliError::Launch(error.to_string()))
}

fn probe_working_directory() -> PathBuf {
    let path = env::temp_dir().join(format!("codex-usage-claude-{}", Uuid::new_v4()));
    let _ = fs::create_dir_all(&path);
    path
}

struct ProbeDirectoryGuard(PathBuf);

impl Drop for ProbeDirectoryGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn prompt_responses() -> [(&'static str, &'static str); 9] {
    [
        ("doyoutrustthefilesinthisfolder", "y\r"),
        ("quicksafetycheck", "\r"),
        ("yesitrustthisfolder", "\r"),
        ("readytocodehere", "\r"),
        ("pressentertocontinue", "\r"),
        ("showplan", "\r"),
        ("showplanusagelimits", "\r"),
        ("showclaudecode", "\r"),
        ("showclaudecodestatus", "\r"),
    ]
}

fn usage_capture_complete(normalized: &str) -> bool {
    let Some(start) = normalized.rfind("currentsession") else {
        return false;
    };
    Regex::new(r"\d{1,3}(?:\.\d+)?%")
        .expect("valid percentage expression")
        .is_match(&normalized[start..])
}

pub fn parse_usage(
    usage_text: &str,
    status_text: &str,
    now: DateTime<Utc>,
) -> Result<ClaudeCliUsage, ClaudeCliError> {
    let clean = strip_terminal_sequences(usage_text);
    if clean.trim().is_empty() {
        return Err(ClaudeCliError::TimedOut);
    }
    let normalized = normalize_terminal_text(&clean);
    if normalized.contains("notloggedin")
        || normalized.contains("pleaselogin")
        || normalized.contains("runclaudelogin")
    {
        return Err(ClaudeCliError::NotLoggedIn);
    }
    if normalized.contains("ratelimit") || normalized.contains("ratelimited") {
        return Err(ClaudeCliError::RateLimited);
    }
    if normalized.contains("currentlyusingyoursubscription")
        && normalized.contains("claudecodeusage")
    {
        return Err(ClaudeCliError::Parse(
            "Claude returned a subscription notice without session quota data".to_owned(),
        ));
    }

    let panel = latest_usage_panel(&clean);
    let session = extract_labeled_window(
        &panel,
        &["Current session"],
        UsageWindowKind::Primary,
        "Session",
        SESSION_WINDOW_SECONDS,
        now,
    )?
    .ok_or_else(|| ClaudeCliError::Parse("missing Current session".to_owned()))?;
    let weekly = extract_labeled_window(
        &panel,
        &["Current week (all models)", "Current week (All models)"],
        UsageWindowKind::Secondary,
        "Weekly",
        WEEKLY_WINDOW_SECONDS,
        now,
    )?;
    let additional_windows = extract_scoped_windows(&panel, now);
    let identity = parse_identity(status_text);
    Ok(ClaudeCliUsage {
        primary: session,
        secondary: weekly,
        additional_windows,
        observed_email: identity.email,
        organization: identity.organization,
        plan_type: identity.plan_type,
        raw_text: format!("{usage_text}{status_text}"),
    })
}

#[derive(Debug, Default)]
struct ParsedIdentity {
    email: Option<String>,
    organization: Option<String>,
    plan_type: Option<String>,
}

fn parse_identity(status_text: &str) -> ParsedIdentity {
    let clean = strip_terminal_sequences(status_text);
    let email = Regex::new(r"(?i)\b[\w.!#$%&'*+/=?^`{|}~-]+@[\w.-]+\.[A-Za-z]{2,}\b")
        .expect("valid email expression")
        .find(&clean)
        .map(|value| value.as_str().to_ascii_lowercase());
    let organization = clean.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        (lower.contains("organization") || lower.contains("workspace"))
            .then(|| {
                line.split_once(':')
                    .map(|(_, value)| value.trim().to_owned())
            })
            .flatten()
            .filter(|value| !value.is_empty())
    });
    let lower = clean.to_ascii_lowercase();
    let plan_type = if lower.contains("max") {
        Some("Claude Max".to_owned())
    } else if lower.contains("pro") {
        Some("Claude Pro".to_owned())
    } else if lower.contains("team") {
        Some("Claude Team".to_owned())
    } else if lower.contains("enterprise") {
        Some("Claude Enterprise".to_owned())
    } else {
        None
    };
    ParsedIdentity {
        email,
        organization,
        plan_type,
    }
}

fn latest_usage_panel(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .rposition(|line| normalize_terminal_text(line).contains("currentsession"))
        .unwrap_or(0);
    lines[start..].join("\n")
}

fn extract_labeled_window(
    text: &str,
    labels: &[&str],
    kind: UsageWindowKind,
    name: &str,
    window_seconds: i64,
    now: DateTime<Utc>,
) -> Result<Option<RateLimitWindow>, ClaudeCliError> {
    let lines = text.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        if !labels
            .iter()
            .any(|label| normalize_terminal_text(line).contains(&normalize_terminal_text(label)))
        {
            continue;
        }
        let end = (index + 12).min(lines.len());
        for candidate in &lines[index..end] {
            if let Some(percent_left) = first_percent(candidate) {
                return Ok(Some(RateLimitWindow {
                    kind,
                    name: name.to_owned(),
                    used_percent: (100.0 - percent_left).clamp(0.0, 100.0),
                    reset_at_utc: reset_at_near(&lines[index..end], now),
                    limit_window_seconds: window_seconds,
                }));
            }
        }
        return Ok(None);
    }
    Ok(None)
}

fn extract_scoped_windows(text: &str, now: DateTime<Utc>) -> Vec<AdditionalRateLimitWindow> {
    let lines = text.lines().collect::<Vec<_>>();
    let label_regex = Regex::new(r"(?i)current\s+week\s*\(([^)]+)\)")
        .expect("valid Claude model label expression");
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(captures) = label_regex.captures(line) else {
            continue;
        };
        let Some(model) = captures.get(1).map(|value| value.as_str().trim()) else {
            continue;
        };
        let normalized = normalize_terminal_text(model);
        if normalized.is_empty() || normalized == "allmodels" {
            continue;
        }
        let end = (index + 12).min(lines.len());
        let Some(percent_left) = lines[index..end]
            .iter()
            .find_map(|line| first_percent(line))
        else {
            continue;
        };
        let (key, title) = match normalized.as_str() {
            "sonnet" | "sonnetonly" => (
                "claude-weekly-sonnet".to_owned(),
                "Sonnet weekly".to_owned(),
            ),
            "opus" => ("claude-weekly-opus".to_owned(), "Opus weekly".to_owned()),
            _ => {
                let key = format!("claude-weekly-scoped-{}", slugify(model));
                let title = if normalized.ends_with("only") {
                    model.to_owned()
                } else {
                    format!("{model} only")
                };
                (key, title)
            }
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        output.push(AdditionalRateLimitWindow {
            key,
            name: title.clone(),
            window: RateLimitWindow {
                kind: UsageWindowKind::Additional,
                name: title,
                used_percent: (100.0 - percent_left).clamp(0.0, 100.0),
                reset_at_utc: reset_at_near(&lines[index..end], now),
                limit_window_seconds: WEEKLY_WINDOW_SECONDS,
            },
        });
    }
    output
}

fn first_percent(line: &str) -> Option<f64> {
    Regex::new(r"(?i)(\d{1,3}(?:\.\d+)?)\s*%")
        .expect("valid percentage expression")
        .captures(line)
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse::<f64>().ok())
        .filter(|value| *value <= 100.0)
}

fn reset_at_near(lines: &[&str], now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let line = lines.iter().find(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("reset")
    })?;
    let day = duration_component(line, r"(?i)(\d+)\s*(?:days?|d)\b");
    let hour = duration_component(line, r"(?i)(\d+)\s*(?:hours?|h)\b");
    let minute = duration_component(line, r"(?i)(\d+)\s*(?:minutes?|mins?|m)\b");
    let second = duration_component(line, r"(?i)(\d+)\s*(?:seconds?|secs?|s)\b");
    let seconds = day * 86_400 + hour * 3_600 + minute * 60 + second;
    if seconds > 0 {
        return Some(now + ChronoDuration::seconds(seconds));
    }

    let timestamp =
        Regex::new(r"(?i)(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2}))")
            .expect("valid absolute reset expression")
            .captures(line)
            .and_then(|captures| captures.get(1))
            .and_then(|value| DateTime::parse_from_rfc3339(value.as_str()).ok())
            .map(|value| value.with_timezone(&Utc));
    timestamp
}

fn duration_component(line: &str, pattern: &str) -> i64 {
    Regex::new(pattern)
        .expect("valid duration expression")
        .captures(line)
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse::<i64>().ok())
        .unwrap_or(0)
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            slug.push('-');
            dash = true;
        }
    }
    slug.trim_matches('-').to_owned()
}

fn strip_terminal_sequences(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            } else if index < bytes.len() && bytes[index] == b']' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte == 0x07 {
                        break;
                    }
                    if byte == 0x1b && index < bytes.len() && bytes[index] == b'\\' {
                        index += 1;
                        break;
                    }
                }
            }
            continue;
        }
        if bytes[index] == b'\r' {
            index += 1;
            continue;
        }
        output.push(bytes[index] as char);
        index += 1;
    }
    output
}

fn normalize_terminal_text(text: &str) -> String {
    strip_terminal_sequences(text)
        .chars()
        .filter(|character| !character.is_whitespace())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_weekly_scoped_models_and_relative_resets() {
        let now = DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let usage = r#"
Current session
  73% left
  Resets in 4h 4m
Current week (all models)
  57% left
  Resets in 2d 10h
Current week (Sonnet only)
  80% left
Current week (Fable)
  42% left
  Resets in 1d 2h
"#;
        let parsed = parse_usage(usage, "Email: user@example.com\nPlan: Pro", now).unwrap();
        assert_eq!(parsed.primary.used_percent, 27.0);
        assert_eq!(parsed.secondary.unwrap().used_percent, 43.0);
        assert_eq!(parsed.additional_windows.len(), 2);
        assert_eq!(parsed.additional_windows[0].key, "claude-weekly-sonnet");
        assert_eq!(
            parsed.additional_windows[1].key,
            "claude-weekly-scoped-fable"
        );
        assert_eq!(parsed.observed_email.as_deref(), Some("user@example.com"));
        assert_eq!(parsed.plan_type.as_deref(), Some("Claude Pro"));
        assert_eq!(
            parsed.primary.reset_at_utc.unwrap(),
            now + ChronoDuration::hours(4) + ChronoDuration::minutes(4)
        );
    }

    #[test]
    fn reset_parser_accepts_long_units_and_absolute_timestamps() {
        let now = DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let usage = r#"
Current session
  10% left
  Resets in 2 days 3 hours 5 minutes
Current week (all models)
  20% left
  Resets at 2030-01-08T12:30:00Z
"#;
        let parsed = parse_usage(usage, "", now).unwrap();
        assert_eq!(
            parsed.primary.reset_at_utc,
            Some(
                now + ChronoDuration::days(2)
                    + ChronoDuration::hours(3)
                    + ChronoDuration::minutes(5)
            )
        );
        assert_eq!(
            parsed.secondary.unwrap().reset_at_utc,
            Some(
                DateTime::parse_from_rfc3339("2030-01-08T12:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn strips_ansi_sequences_and_rejects_missing_session() {
        let now = Utc::now();
        let result = parse_usage("\x1b[31mCurrent week\x1b[0m\n50% left", "", now);
        assert!(
            matches!(result, Err(ClaudeCliError::Parse(message)) if message.contains("Current session"))
        );
    }

    #[test]
    fn detects_completion_only_after_session_percent() {
        assert!(!usage_capture_complete("currentsession"));
        assert!(usage_capture_complete("currentsession73%left"));
    }

    #[test]
    fn parses_json_auth_status_without_starting_the_tui() {
        assert_eq!(
            parse_auth_status(r#"{"loggedIn":true}"#),
            Some(ClaudeCliAuthStatus::LoggedIn)
        );
        assert_eq!(
            parse_auth_status("startup\n{\"logged_in\":false}\n"),
            Some(ClaudeCliAuthStatus::LoggedOut)
        );
        assert_eq!(
            parse_auth_status("diagnostic\n{\n  \"loggedIn\": true\n}\n"),
            Some(ClaudeCliAuthStatus::LoggedIn)
        );
        assert_eq!(parse_auth_status("not-json"), None);
    }

    #[test]
    fn persisted_cli_rate_limit_contains_only_a_bounded_timestamp() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut environment = HashMap::new();
        environment.insert(
            CLI_STATE_FILE_ENV.to_owned(),
            path.to_string_lossy().into_owned(),
        );
        record_persisted_rate_limit(&environment, 60);
        let remaining = persisted_rate_limit_remaining(&environment).unwrap();
        assert!((1..=60).contains(&remaining));
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("cli_rate_limit_until_utc"));
        assert!(!body.contains("token"));
        clear_persisted_rate_limit(&environment);
        assert!(!path.exists());
    }
}
