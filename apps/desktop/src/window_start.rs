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

use chrono::{DateTime, Utc};
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
/// The reset each account was last handled for, so each reset is handled once.
static HANDLED: Mutex<Option<HashMap<AccountId, DateTime<Utc>>>> = Mutex::new(None);
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

/// When the reading's five-hour window reset, if it already has.
pub(crate) fn elapsed_session_reset(
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
    let reset_at = primary.reset_at_utc?;
    (is_session && !snapshot.primary_window_is_synthetic && reset_at <= now).then_some(reset_at)
}

/// Records that this reset is being handled. False when it already was.
pub(crate) fn claim(account_id: AccountId, reset_at: DateTime<Utc>) -> bool {
    let Ok(mut handled) = HANDLED.lock() else {
        return false;
    };
    let handled = handled.get_or_insert_with(HashMap::new);
    if handled.get(&account_id) == Some(&reset_at) {
        return false;
    }
    handled.insert(account_id, reset_at);
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

/// Sends the one-word message through the official CLI, hidden.
pub(crate) async fn run(cli: Cli) -> Result<(), String> {
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
    fn only_a_passed_five_hour_reset_is_due() {
        let now = Utc::now();
        let past = now - TimeDelta::minutes(1);
        let future = now + TimeDelta::minutes(1);
        let session = Some(UsagePrimaryWindowKind::Session);
        assert_eq!(
            elapsed_session_reset(&snapshot(session, 18_000, past), now),
            Some(past)
        );
        assert_eq!(
            elapsed_session_reset(&snapshot(session, 18_000, future), now),
            None
        );
        assert_eq!(
            elapsed_session_reset(&snapshot(None, 18_000, past), now),
            Some(past)
        );
        let weekly = Some(UsagePrimaryWindowKind::Weekly);
        assert_eq!(
            elapsed_session_reset(&snapshot(weekly, 604_800, past), now),
            None
        );
        assert_eq!(
            elapsed_session_reset(&snapshot(None, 604_800, past), now),
            None
        );
        let mut synthetic = snapshot(session, 18_000, past);
        synthetic.primary_window_is_synthetic = true;
        assert_eq!(elapsed_session_reset(&synthetic, now), None);
    }

    #[test]
    fn each_reset_is_handled_once() {
        let account = AccountId::new();
        let reset = Utc::now();
        assert!(claim(account, reset));
        assert!(!claim(account, reset));
        assert!(claim(account, reset + TimeDelta::hours(5)));
    }

    #[test]
    fn saved_accounts_are_read_back() {
        let account = AccountId::new();
        let saved = parse_accounts(&format!("{account}\nnot an id\n\n"));
        assert_eq!(saved, HashSet::from([account]));
    }
}
