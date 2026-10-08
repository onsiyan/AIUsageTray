//! Codex and Claude Code usage on other machines, read over SSH.
//!
//! The system's OpenSSH client runs in batch mode, so it signs in only with
//! the user's keys and `~/.ssh/config` and never asks for a password. It
//! sends a small script (`remote/scan.py` for Linux and macOS, run by
//! `python3`; `remote/scan.ps1` for Windows, run by Windows PowerShell) that
//! reads the machine's logs by the same rules as this PC's reader and prints
//! the usage summed by half hour and model. Nothing is installed; the script
//! keeps a small state file there so later runs read only what is new.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{CostTool, TokenCounts};

const UNIX_SCRIPT: &str = include_str!("remote/scan.py");
const WINDOWS_SCRIPT: &str = include_str!("remote/scan.ps1");
/// Machines the user added, one per line: `name<TAB>target<TAB>os`.
const HOSTS_FILE: &str = "ssh-hosts.txt";
/// The reply's version, which the scripts print as `usage_monitor`.
const REPLY_VERSION: u64 = 1;
/// The line that ends the Windows script on the input.
const WINDOWS_SCRIPT_END: &str = "#usage-monitor-end";
/// Reads the Windows script up to [`WINDOWS_SCRIPT_END`] and runs it; exits
/// with 3 if a minute passes without a line.
const WINDOWS_BOOTSTRAP: &str = concat!(
    "$r=New-Object IO.StreamReader([Console]::OpenStandardInput(),",
    "(New-Object Text.UTF8Encoding($false)));",
    "$b=New-Object Text.StringBuilder;$t=$r.ReadLineAsync();",
    "while($t.Wait(60000)){$x=$t.Result;",
    "if($null -eq $x -or $x -eq '#usage-monitor-end'){break};",
    "[void]$b.AppendLine($x);$t=$r.ReadLineAsync()};",
    "if(-not $t.IsCompleted){exit 3};",
    "& ([scriptblock]::Create($b.ToString()))"
);
/// A first read of large logs can take minutes on the other machine.
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What runs the script on the other machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteOs {
    /// Linux or macOS, with `python3`.
    Unix,
    Windows,
}

impl RemoteOs {
    fn key(self) -> &'static str {
        match self {
            Self::Unix => "unix",
            Self::Windows => "windows",
        }
    }
}

/// A machine whose logs are counted with this PC's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHost {
    /// What the app calls it.
    pub name: String,
    /// What `ssh` connects to: a `Host` from `~/.ssh/config` or `user@host`.
    pub target: String,
    pub os: RemoteOs,
}

impl SshHost {
    /// Checks a new machine's fields: a name not already taken, and a target
    /// `ssh` reads as a destination rather than an option.
    pub fn new(
        name: &str,
        target: &str,
        os: RemoteOs,
        taken: &[SshHost],
    ) -> Result<Self, HostProblem> {
        let name = name.trim();
        let target = target.trim();
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(HostProblem::Name);
        }
        if taken
            .iter()
            .any(|host| host.name.eq_ignore_ascii_case(name))
        {
            return Err(HostProblem::NameTaken);
        }
        let valid_target = !target.is_empty()
            && !target.starts_with('-')
            && !target
                .chars()
                .any(|character| character.is_whitespace() || character.is_control());
        if !valid_target {
            return Err(HostProblem::Target);
        }
        Ok(Self {
            name: name.to_owned(),
            target: target.to_owned(),
            os,
        })
    }
}

/// Why a machine could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostProblem {
    Name,
    NameTaken,
    Target,
}

/// One machine's usage as last read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteUsage {
    /// The tools whose log folders exist there.
    pub found: Vec<CostTool>,
    pub rows: Vec<RemoteRow>,
    pub synced_at: DateTime<Utc>,
}

/// A model's usage in one half hour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteRow {
    pub tool: CostTool,
    /// Unix seconds at the start of the half hour (UTC).
    pub slot: i64,
    pub model: String,
    pub long_context: bool,
    pub tokens: TokenCounts,
}

/// Why a machine could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteError {
    /// No OpenSSH client on this PC.
    NoSsh,
    /// The machine did not take the user's keys.
    SignIn,
    /// The machine's host key is not trusted yet.
    HostKey,
    /// The machine could not be reached.
    Unreachable,
    /// A Linux or macOS machine without `python3`.
    NoPython,
    TimedOut,
    /// Anything else, with what `ssh` or the script said.
    Failed(String),
}

/// The machines the user added, as saved in `directory`.
pub fn load_hosts(directory: &Path) -> Vec<SshHost> {
    let Ok(contents) = fs::read_to_string(directory.join(HOSTS_FILE)) else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    for line in contents.lines() {
        let mut fields = line.split('\t');
        let (Some(name), Some(target), Some(os)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let os = match os.trim() {
            "windows" => RemoteOs::Windows,
            "unix" => RemoteOs::Unix,
            _ => continue,
        };
        if let Ok(host) = SshHost::new(name, target, os, &hosts) {
            hosts.push(host);
        }
    }
    hosts
}

pub fn save_hosts(directory: &Path, hosts: &[SshHost]) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let contents = hosts
        .iter()
        .map(|host| format!("{}\t{}\t{}", host.name, host.target, host.os.key()))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(directory.join(HOSTS_FILE), contents)
}

/// The `Host` names in `~/.ssh/config`, without patterns, to offer as
/// targets.
pub fn ssh_config_hosts() -> Vec<String> {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(PathBuf::from(home).join(".ssh").join("config")) else {
        return Vec::new();
    };
    config_hosts(&contents)
}

fn config_hosts(contents: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        let Some((keyword, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for name in rest.split_whitespace() {
            let pattern = name.contains(['*', '?', '!']);
            if !pattern && !hosts.iter().any(|known| known == name) {
                hosts.push(name.to_owned());
            }
        }
    }
    hosts
}

/// Reads one machine's usage. Blocking, for up to `timeout`.
pub fn fetch(host: &SshHost, timeout: Duration) -> Result<RemoteUsage, RemoteError> {
    let (remote_command, script) = match host.os {
        RemoteOs::Unix => ("python3 -".to_owned(), UNIX_SCRIPT),
        RemoteOs::Windows => {
            // The script is too long for a command line, so a short command
            // reads it from the input instead, up to an end line rather than
            // the input's end: through some SSH servers the end of input never
            // arrives, and PowerShell would wait for it forever. A minute
            // without a line ends the wait.
            let bootstrap = WINDOWS_BOOTSTRAP
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            (
                format!(
                    "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}",
                    base64_encode(&bootstrap)
                ),
                WINDOWS_SCRIPT,
            )
        }
    };
    let script = match host.os {
        RemoteOs::Unix => script.to_owned(),
        RemoteOs::Windows => format!(
            "{script}
{WINDOWS_SCRIPT_END}
"
        ),
    };
    let mut command = Command::new(ssh_program());
    command
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=20",
            "-T",
            "--",
            &host.target,
            &remote_command,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashes up for each sync.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => RemoteError::NoSsh,
        _ => RemoteError::Failed(error.to_string()),
    })?;

    let mut stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(stdin) = &mut stdin {
            let _ = stdin.write_all(script.as_bytes());
        }
        // Dropping the input ends it, so the script starts.
        drop(stdin);
    });
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || read_all(stdout));
    let err_reader = std::thread::spawn(move || read_all(stderr));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RemoteError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(RemoteError::Failed(error.to_string())),
        }
    };
    let _ = writer.join();
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    if let Some(usage) = parse_reply(&stdout, Utc::now()) {
        return Ok(usage);
    }
    Err(classify_failure(status.code(), &stderr, host.os))
}

fn read_all(source: Option<impl Read>) -> String {
    let mut bytes = Vec::new();
    if let Some(mut source) = source {
        let _ = source.read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Windows' own OpenSSH, where it is installed, else whatever `ssh` is on
/// the path.
fn ssh_program() -> PathBuf {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let system = PathBuf::from(root)
            .join("System32")
            .join("OpenSSH")
            .join("ssh.exe");
        if system.is_file() {
            return system;
        }
    }
    PathBuf::from("ssh")
}

fn classify_failure(code: Option<i32>, stderr: &str, os: RemoteOs) -> RemoteError {
    let said = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let all = said.join(" ").to_ascii_lowercase();
    if all.contains("host key verification failed")
        || all.contains("remote host identification has changed")
    {
        return RemoteError::HostKey;
    }
    if all.contains("permission denied") || all.contains("too many authentication failures") {
        return RemoteError::SignIn;
    }
    if all.contains("could not resolve hostname")
        || all.contains("connection timed out")
        || all.contains("connection refused")
        || all.contains("no route to host")
        || all.contains("network is unreachable")
    {
        return RemoteError::Unreachable;
    }
    if os == RemoteOs::Unix
        && (code == Some(127)
            || all.contains("python3: command not found")
            || all.contains("python3: not found"))
    {
        return RemoteError::NoPython;
    }
    let detail = said.last().copied().unwrap_or("no reply from the machine");
    RemoteError::Failed(detail.chars().take(200).collect())
}

/// A reply row: tool, half-hour start, model, long context (0 or 1), then
/// input, cache reads, cache writes, one-hour cache writes, output and
/// reasoning tokens.
type ReplyRow = (CostTool, i64, String, u8, u64, u64, u64, u64, u64, u64);

#[derive(Deserialize)]
struct Reply {
    usage_monitor: u64,
    #[serde(default)]
    found: Vec<CostTool>,
    #[serde(default)]
    rows: Vec<ReplyRow>,
}

/// The script's reply: the last output line that is one. Lines before it
/// (a login banner, say) are skipped.
fn parse_reply(stdout: &str, synced_at: DateTime<Utc>) -> Option<RemoteUsage> {
    let reply = stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| line.starts_with("{\"usage_monitor\""))
        .find_map(|line| serde_json::from_str::<Reply>(line).ok())?;
    if reply.usage_monitor != REPLY_VERSION {
        return None;
    }
    let rows = reply
        .rows
        .into_iter()
        .map(
            |(
                tool,
                slot,
                model,
                long_context,
                input,
                cache_read,
                cache_write,
                cache_write_1h,
                output,
                reasoning,
            )| {
                RemoteRow {
                    tool,
                    slot,
                    model,
                    long_context: long_context != 0,
                    tokens: TokenCounts {
                        input,
                        cache_read,
                        cache_write,
                        cache_write_1h,
                        output,
                        reasoning,
                    },
                }
            },
        )
        .collect();
    Some(RemoteUsage {
        found: reply.found,
        rows,
        synced_at,
    })
}

/// Where a machine's last reading is kept, so the page shows it at once.
fn stored_path(cache_directory: &Path, name: &str) -> PathBuf {
    let safe = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    // The hash keeps names that differ only in other characters apart.
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    cache_directory.join(format!("remote-{safe}-{hash:016x}.json"))
}

pub fn store(cache_directory: &Path, name: &str, usage: &RemoteUsage) -> io::Result<()> {
    fs::create_dir_all(cache_directory)?;
    let bytes = serde_json::to_vec(usage).map_err(io::Error::other)?;
    let path = stored_path(cache_directory, name);
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

pub fn stored(cache_directory: &Path, name: &str) -> Option<RemoteUsage> {
    let bytes = fs::read(stored_path(cache_directory, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Removes a machine's kept reading.
pub fn forget(cache_directory: &Path, name: &str) {
    let _ = fs::remove_file(stored_path(cache_directory, name));
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(char::from(
                    ALPHABET[(triple >> (18 - 6 * index)) as usize & 63],
                ));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_bootstrap_waits_for_the_end_line_and_fits_a_command_line() {
        assert!(WINDOWS_BOOTSTRAP.contains(&format!("'{WINDOWS_SCRIPT_END}'")));
        assert!(
            !WINDOWS_SCRIPT
                .lines()
                .any(|line| line == WINDOWS_SCRIPT_END)
        );
        let encoded = base64_encode(
            &WINDOWS_BOOTSTRAP
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        assert!(encoded.len() < 2_000);
    }

    #[test]
    fn replies_parse_past_a_login_banner() {
        let stdout = "Welcome to the server\n{\"usage_monitor\":1,\"found\":[\"codex\"],\"rows\":[[\"codex\",1791408600,\"gpt-5.5\",1,10,20,0,0,5,2]]}\n";
        let usage = parse_reply(stdout, Utc::now()).unwrap();
        assert_eq!(usage.found, vec![CostTool::Codex]);
        assert_eq!(usage.rows.len(), 1);
        let row = &usage.rows[0];
        assert!(row.long_context);
        assert_eq!(row.tokens.cache_read, 20);
        assert_eq!(row.tokens.reasoning, 2);
        assert!(parse_reply("{\"usage_monitor\":2,\"rows\":[]}", Utc::now()).is_none());
        assert!(parse_reply("bash: python3: command not found", Utc::now()).is_none());
    }

    #[test]
    fn failures_are_told_apart() {
        let unix = RemoteOs::Unix;
        assert_eq!(
            classify_failure(Some(255), "user@vps: Permission denied (publickey).", unix),
            RemoteError::SignIn
        );
        assert_eq!(
            classify_failure(Some(255), "Host key verification failed.", unix),
            RemoteError::HostKey
        );
        assert_eq!(
            classify_failure(
                Some(255),
                "ssh: Could not resolve hostname nope: No such host",
                unix
            ),
            RemoteError::Unreachable
        );
        assert_eq!(
            classify_failure(Some(127), "bash: python3: command not found", unix),
            RemoteError::NoPython
        );
        assert_eq!(
            classify_failure(Some(1), "boom\n", RemoteOs::Windows),
            RemoteError::Failed("boom".to_owned())
        );
    }

    #[test]
    fn targets_cannot_pass_as_options() {
        let os = RemoteOs::Unix;
        assert!(SshHost::new("vps", "root@203.0.113.5", os, &[]).is_ok());
        assert_eq!(
            SshHost::new("x", "-oProxyCommand=calc", os, &[]),
            Err(HostProblem::Target)
        );
        assert_eq!(SshHost::new("x", "a b", os, &[]), Err(HostProblem::Target));
        assert_eq!(SshHost::new(" ", "vps", os, &[]), Err(HostProblem::Name));
        let taken = [SshHost::new("VPS", "vps", os, &[]).unwrap()];
        assert_eq!(
            SshHost::new("vps", "vps", os, &taken),
            Err(HostProblem::NameTaken)
        );
    }

    #[test]
    fn config_hosts_skip_patterns() {
        let config = "Host vps pc\n  HostName 203.0.113.5\nHost *.internal !bad\nhost laptop\n";
        assert_eq!(config_hosts(config), vec!["vps", "pc", "laptop"]);
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn hosts_round_trip_through_their_file() {
        let directory = std::env::temp_dir().join(format!("um-hosts-{}", std::process::id()));
        let hosts = vec![
            SshHost::new("vps", "root@203.0.113.5", RemoteOs::Unix, &[]).unwrap(),
            SshHost::new("pc", "pc", RemoteOs::Windows, &[]).unwrap(),
        ];
        save_hosts(&directory, &hosts).unwrap();
        assert_eq!(load_hosts(&directory), hosts);
        let _ = fs::remove_dir_all(&directory);
    }
}
