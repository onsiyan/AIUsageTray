//! Starts a new five-hour window as soon as the last one resets.
//!
//! A subscription's five-hour window only starts counting at the first
//! message after a reset. For the accounts the user turns this on for, the
//! app sends one tiny message through the official `codex` or `claude` CLI
//! right after the reset, so the next window starts at once. The app never
//! sends anything itself: the CLI uses its own sign-in, and only when that
//! sign-in is the same account.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use usage_monitor_core::accounts::AccountId;
use usage_monitor_core::usage::{UsagePrimaryWindowKind, UsageSnapshot};

use crate::theme::preference_directory;

const ENABLED_FILE: &str = "window-start.txt";
/// How often the app looks for a window that reset.
pub(crate) const CHECK_EVERY: Duration = Duration::from_secs(30);
const RUN_TIMEOUT: Duration = Duration::from_secs(180);
const PROMPT: &str = "Reply with the single word: ok";
/// Primary windows this long or shorter are the session (five-hour) kind.
const SESSION_WINDOW_MAX_SECONDS: i64 = 6 * 60 * 60;
/// How far an idle window's reset may sit short of a full window after the
/// reading.
const IDLE_SLACK_SECONDS: i64 = 120;
/// No second start for an account sooner than this.
const RETRY_AFTER_MINUTES: i64 = 10;

/// Failure kept when the CLI is signed in with another account; the card
/// shows it in the user's language.
pub(crate) const OTHER_ACCOUNT: &str = "other-account";

/// The official CLI that starts the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cli {
    Codex,
    Claude,
}

impl Cli {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn arguments(self) -> &'static [&'static str] {
        match self {
            // Ephemeral and read-only: nothing is saved and nothing can run.
            Self::Codex => &[
                "exec",
                "--skip-git-repo-check",
                "--ephemeral",
                "--sandbox",
                "read-only",
                PROMPT,
            ],
            Self::Claude => &["-p", "--model", "haiku", "--no-session-persistence", PROMPT],
        }
    }
}

static ENABLED: Mutex<Option<HashSet<AccountId>>> = Mutex::new(None);
/// The idle window each account was last started for, and when.
/// An idle window's key, and when it was started.
type Started = (DateTime<Utc>, DateTime<Utc>);
static HANDLED: Mutex<Option<HashMap<AccountId, Started>>> = Mutex::new(None);
/// The last failure for each account, shown on its card.
static FAILURES: Mutex<Option<HashMap<AccountId, String>>> = Mutex::new(None);

pub(crate) fn is_enabled(account_id: AccountId) -> bool {
    ENABLED.lock().ok().is_some_and(|enabled| {
        enabled
            .as_ref()
            .is_some_and(|set| set.contains(&account_id))
    })
}

pub(crate) fn any_enabled() -> bool {
    ENABLED
        .lock()
        .ok()
        .is_some_and(|enabled| enabled.as_ref().is_some_and(|set| !set.is_empty()))
}

pub(crate) fn failure(account_id: AccountId) -> Option<String> {
    FAILURES.lock().ok()?.as_ref()?.get(&account_id).cloned()
}

pub(crate) fn set_failure(account_id: AccountId, failure: Option<String>) {
    if let Ok(mut failures) = FAILURES.lock() {
        let failures = failures.get_or_insert_with(HashMap::new);
        match failure {
            Some(failure) => failures.insert(account_id, failure),
            None => failures.remove(&account_id),
        };
    }
}

/// Reads the saved accounts; called once at start.
pub(crate) fn load_saved() {
    let saved = preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(ENABLED_FILE)).ok())
        .map(|text| parse_accounts(&text))
        .unwrap_or_default();
    if let Ok(mut enabled) = ENABLED.lock() {
        *enabled = Some(saved);
    }
}

/// Turns the feature on or off for one account and saves the choice.
pub(crate) fn toggle(account_id: AccountId) -> io::Result<()> {
    let text = {
        let mut enabled = ENABLED
            .lock()
            .map_err(|_| io::Error::other("window start state is poisoned"))?;
        let enabled = enabled.get_or_insert_with(HashSet::new);
        if !enabled.remove(&account_id) {
            enabled.insert(account_id);
        }
        let mut lines = enabled.iter().map(ToString::to_string).collect::<Vec<_>>();
        lines.sort();
        lines.join("\n")
    };
    set_failure(account_id, None);
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(ENABLED_FILE), text)
}

fn parse_accounts(text: &str) -> HashSet<AccountId> {
    text.lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// A key for the five-hour window waiting to start, if it is: its reset
/// passed, or the reading shows it idle (nothing used and, for Codex, a reset
/// a full window after the reading, which moves with every reading; for
/// Claude, no reset time at all).
pub(crate) fn idle_session_window(
    snapshot: &UsageSnapshot,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let primary = snapshot.primary.as_ref()?;
    let is_session = match snapshot.primary_window_kind {
        Some(kind) => kind == UsagePrimaryWindowKind::Session,
        None => {
            primary.limit_window_seconds > 0
                && primary.limit_window_seconds <= SESSION_WINDOW_MAX_SECONDS
        }
    };
    if !is_session || snapshot.primary_window_is_synthetic {
        return None;
    }
    match primary.reset_at_utc {
        Some(reset_at) if reset_at <= now => Some(reset_at),
        Some(reset_at) => {
            let full_window = TimeDelta::seconds(primary.limit_window_seconds)
                - TimeDelta::seconds(IDLE_SLACK_SECONDS);
            (primary.used_percent == 0.0 && reset_at - snapshot.observed_at_utc >= full_window)
                .then_some(reset_at)
        }
        None => (primary.used_percent == 0.0).then_some(snapshot.observed_at_utc),
    }
}

/// Records that this idle window is being started. False when it already
/// was, or when the account was started moments ago and the provider may not
/// show it yet.
pub(crate) fn claim(account_id: AccountId, key: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let Ok(mut handled) = HANDLED.lock() else {
        return false;
    };
    let handled = handled.get_or_insert_with(HashMap::new);
    if let Some((last_key, last_at)) = handled.get(&account_id)
        && (*last_key == key || now - *last_at < TimeDelta::minutes(RETRY_AFTER_MINUTES))
    {
        return false;
    }
    handled.insert(account_id, (key, now));
    true
}

/// The email Claude Code is signed in with, from its own settings file.
pub(crate) fn claude_code_email() -> Option<String> {
    let path = claude_settings_path()?;
    let text = fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    value
        .pointer("/oauthAccount/emailAddress")
        .and_then(serde_json::Value::as_str)
        .map(|email| email.trim().to_owned())
        .filter(|email| !email.is_empty())
}

fn claude_settings_path() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(Path::new(&directory).join(".claude.json"));
    }
    home_directory().map(|home| home.join(".claude.json"))
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Finds the CLI on PATH, then where its installers usually put it.
fn locate(cli: Cli) -> Option<PathBuf> {
    let extensions: &[&str] = if cfg!(windows) {
        &["exe", "cmd", "bat"]
    } else {
        &[""]
    };
    let mut directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(app_data) = std::env::var_os("APPDATA") {
        directories.push(Path::new(&app_data).join("npm"));
    }
    if let Some(home) = home_directory() {
        directories.push(home.join(".local").join("bin"));
    }
    directories.iter().find_map(|directory| {
        extensions.iter().find_map(|extension| {
            let mut path = directory.join(cli.name());
            if !extension.is_empty() {
                path.set_extension(extension);
            }
            path.is_file().then_some(path)
        })
    })
}

/// Sends the one-word message through the official CLI, hidden. The app's
/// tasks do not run on Tokio, so the CLI is driven from its own thread and
/// runtime.
pub(crate) async fn run(cli: Cli) -> Result<(), String> {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
            .and_then(|runtime| runtime.block_on(send_through(cli)));
        let _ = sender.send_blocking(result);
    });
    receiver
        .recv()
        .await
        .unwrap_or_else(|_| Err(format!("{} stopped", cli.name())))
}

async fn send_through(cli: Cli) -> Result<(), String> {
    let program = locate(cli).ok_or_else(|| format!("The {} CLI was not found", cli.name()))?;
    let directory = std::env::temp_dir();
    let mut command = tokio::process::Command::new(&program);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command
        .args(cli.arguments())
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|error| format!("Could not start {}: {error}", cli.name()))?;
    let output = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("{} did not finish in time", cli.name()))?
        .map_err(|error| format!("{} failed: {error}", cli.name()))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    let detail = if detail.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        detail.to_owned()
    };
    let last_line = detail.lines().last().unwrap_or_default();
    Err(format!(
        "{} failed ({}): {last_line}",
        cli.name(),
        output.status
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use usage_monitor_core::usage::{RateLimitWindow, UsageWindowKind};

    fn snapshot(
        kind: Option<UsagePrimaryWindowKind>,
        seconds: i64,
        reset_at: DateTime<Utc>,
    ) -> UsageSnapshot {
        let mut snapshot: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "account_id": "00000000-0000-0000-0000-000000000000",
            "observed_at_utc": reset_at - TimeDelta::hours(1),
            "response_account_id": null,
            "plan_type": null,
            "primary": null,
            "secondary": null,
            "additional_windows": [],
            "credits": null,
            "spend": null,
            "observed_email": null,
            "is_stale": false,
            "stale_reason": null,
            "stale_at_utc": null,
            "metrics": [],
            "provider_id": "codex",
            "source": null,
            "data_confidence": "high",
        }))
        .expect("snapshot");
        snapshot.primary_window_kind = kind;
        snapshot.primary = Some(RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: "5h".to_owned(),
            used_percent: 40.0,
            reset_at_utc: Some(reset_at),
            limit_window_seconds: seconds,
        });
        snapshot
    }

    #[test]
    fn a_reset_or_idle_five_hour_window_is_due() {
        let now = Utc::now();
        let past = now - TimeDelta::minutes(1);
        let running = now + TimeDelta::minutes(30);
        let session = Some(UsagePrimaryWindowKind::Session);
        // Reset passed.
        let due = |snapshot: &UsageSnapshot| idle_session_window(snapshot, now);
        assert_eq!(due(&snapshot(session, 18_000, past)), Some(past));
        assert_eq!(due(&snapshot(None, 18_000, past)), Some(past));
        // Running: used, reset ahead.
        assert_eq!(due(&snapshot(session, 18_000, running)), None);
        // Idle Codex: nothing used, reset a full window after the reading.
        let mut idle = snapshot(session, 18_000, running);
        idle.observed_at_utc = running - TimeDelta::hours(5);
        idle.primary.as_mut().unwrap().used_percent = 0.0;
        assert_eq!(due(&idle), Some(running));
        // Fresh but nothing used yet within a started window: not idle.
        let mut started = idle.clone();
        started.observed_at_utc = running - TimeDelta::hours(4);
        assert_eq!(due(&started), None);
        // Idle Claude: nothing used and no reset time.
        let mut claude = idle.clone();
        claude.primary.as_mut().unwrap().reset_at_utc = None;
        assert_eq!(due(&claude), Some(claude.observed_at_utc));
        // Weekly or synthetic windows never are.
        let weekly = Some(UsagePrimaryWindowKind::Weekly);
        assert_eq!(due(&snapshot(weekly, 604_800, past)), None);
        assert_eq!(due(&snapshot(None, 604_800, past)), None);
        let mut synthetic = snapshot(session, 18_000, past);
        synthetic.primary_window_is_synthetic = true;
        assert_eq!(due(&synthetic), None);
    }

    #[test]
    fn each_idle_window_is_started_once_and_not_too_often() {
        let account = AccountId::new();
        let now = Utc::now();
        let reset = now;
        assert!(claim(account, reset, now));
        assert!(!claim(account, reset, now + TimeDelta::hours(1)));
        // A new idle reading moments later waits.
        assert!(!claim(
            account,
            reset + TimeDelta::minutes(1),
            now + TimeDelta::minutes(1)
        ));
        assert!(claim(
            account,
            reset + TimeDelta::minutes(11),
            now + TimeDelta::minutes(11)
        ));
    }

    #[test]
    fn saved_accounts_are_read_back() {
        let account = AccountId::new();
        let saved = parse_accounts(&format!("{account}\nnot an id\n\n"));
        assert_eq!(saved, HashSet::from([account]));
    }
}
