//! Reads token usage from Codex and Claude Code session logs.
//!
//! Every file's place and running state are kept between scans, so a log
//! that grew is read only from where the last scan stopped, and an unchanged
//! one is not opened at all.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Days, Local, NaiveDate};
use memchr::memmem;
use serde::{Deserialize, Serialize};

use super::{CostTool, TokenCounts, UsageRow};

pub(super) const CACHE_FILE: &str = "scan-cache.json";
/// Bumped when the stored shape or the reading rules change, so old results
/// are read again.
const CACHE_VERSION: u32 = 1;
/// Usage kept between scans: the report's days plus a margin.
const RETAINED_DAYS: u64 = 40;
/// OpenAI's long-context rates apply to requests with more input than this.
pub(super) const CODEX_LONG_CONTEXT_TOKENS: u64 = 272_000;
/// Codex puts a line's kind near its start; only that much of an
/// uninteresting line is kept while it is skipped.
const CODEX_PREFIX_BYTES: usize = 192;

/// Where each tool keeps its session logs.
#[derive(Debug, Clone, Default)]
pub struct LogRoots {
    pub codex: Vec<PathBuf>,
    pub claude: Vec<PathBuf>,
}

impl LogRoots {
    /// `CODEX_HOME` (else `~/.codex`) sessions, and `CLAUDE_CONFIG_DIR` (else
    /// `~/.claude` and `~/.config/claude`) projects.
    pub fn from_environment() -> Self {
        let home = env::var_os("USERPROFILE")
            .or_else(|| env::var_os("HOME"))
            .map(PathBuf::from);
        let codex_home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".codex")));
        let codex = codex_home
            .map(|codex_home| {
                vec![
                    codex_home.join("sessions"),
                    codex_home.join("archived_sessions"),
                ]
            })
            .unwrap_or_default();
        let claude = match env::var("CLAUDE_CONFIG_DIR") {
            Ok(directories) if !directories.trim().is_empty() => directories
                .split([',', ';'])
                .map(str::trim)
                .filter(|directory| !directory.is_empty())
                .map(|directory| PathBuf::from(directory).join("projects"))
                .collect(),
            _ => home
                .map(|home| {
                    vec![
                        home.join(".claude").join("projects"),
                        home.join(".config").join("claude").join("projects"),
                    ]
                })
                .unwrap_or_default(),
        };
        Self { codex, claude }
    }

    fn of(&self, tool: CostTool) -> &[PathBuf] {
        match tool {
            CostTool::Codex => &self.codex,
            CostTool::Claude => &self.claude,
        }
    }
}

/// Usage found, by tool, and which tools have a log folder at all.
pub(super) struct ScannedUsage {
    pub rows: BTreeMap<CostTool, Vec<UsageRow>>,
    pub found: BTreeSet<CostTool>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ScanCache {
    version: u32,
    files: BTreeMap<String, FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    tool: CostTool,
    size: u64,
    modified: u64,
    /// Where reading stopped: just past the last complete line.
    offset: u64,
    #[serde(default)]
    codex: CodexState,
    /// Codex usage, already summed by day and model.
    #[serde(default)]
    rows: Vec<UsageRow>,
    /// Claude usage, one entry per response, since the same response can be
    /// written more than once and in more than one file.
    #[serde(default)]
    claude: Vec<ClaudeRecord>,
}

/// What a Codex log said last, needed to read the lines after it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CodexState {
    model: Option<String>,
    last_total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ClaudeRecord {
    /// The response's message and request ids, to drop repeats.
    key: String,
    day: NaiveDate,
    model: String,
    tokens: TokenCounts,
}

pub(super) fn scan(roots: &LogRoots, cache_directory: &Path, today: NaiveDate) -> ScannedUsage {
    let cache_path = cache_directory.join(CACHE_FILE);
    let mut cache = fs::read(&cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ScanCache>(&bytes).ok())
        .filter(|cache| cache.version == CACHE_VERSION)
        .unwrap_or_default();
    let first_kept = today
        .checked_sub_days(Days::new(RETAINED_DAYS))
        .unwrap_or(today);
    // A file last written before the kept days holds nothing to report.
    let oldest_modified = first_kept
        .and_hms_opt(0, 0, 0)
        .and_then(|start| start.and_local_timezone(Local).earliest())
        .map_or(0, |start| u64::try_from(start.timestamp()).unwrap_or(0));

    let mut files = BTreeMap::new();
    let mut found = BTreeSet::new();
    for tool in CostTool::ALL {
        for root in roots.of(tool) {
            if !root.is_dir() {
                continue;
            }
            found.insert(tool);
            for path in jsonl_files(root) {
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_secs());
                if modified < oldest_modified {
                    continue;
                }
                let key = path.to_string_lossy().into_owned();
                let previous = cache.files.remove(&key);
                let entry = read_file(&path, tool, metadata.len(), modified, previous, first_kept);
                if let Some(entry) = entry {
                    files.insert(key, entry);
                }
            }
        }
    }

    cache = ScanCache {
        version: CACHE_VERSION,
        files,
    };
    if fs::create_dir_all(cache_directory).is_ok()
        && let Ok(bytes) = serde_json::to_vec(&cache)
    {
        let temporary = cache_path.with_extension("json.tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::rename(&temporary, &cache_path);
        }
    }

    let mut rows: BTreeMap<CostTool, Vec<UsageRow>> = BTreeMap::new();
    let mut claude: HashMap<&str, &ClaudeRecord> = HashMap::new();
    for entry in cache.files.values() {
        rows.entry(entry.tool)
            .or_default()
            .extend(entry.rows.iter().cloned());
        for record in &entry.claude {
            // A response written twice keeps its fuller usage.
            claude
                .entry(record.key.as_str())
                .and_modify(|kept| {
                    if record.tokens.output > kept.tokens.output {
                        *kept = record;
                    }
                })
                .or_insert(record);
        }
    }
    let claude_rows = rows.entry(CostTool::Claude).or_default();
    for record in claude.into_values() {
        add_usage(
            claude_rows,
            record.day,
            &record.model,
            false,
            &record.tokens,
        );
    }
    ScannedUsage { rows, found }
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind)
                    if kind.is_file()
                        && path
                            .extension()
                            .is_some_and(|extension| extension == "jsonl") =>
                {
                    files.push(path);
                }
                _ => {}
            }
        }
    }
    files.sort();
    files
}

/// Brings one file's entry up to date, reading only what was added since
/// `previous` when the file just grew.
fn read_file(
    path: &Path,
    tool: CostTool,
    size: u64,
    modified: u64,
    previous: Option<FileEntry>,
    first_kept: NaiveDate,
) -> Option<FileEntry> {
    let mut entry = match previous {
        Some(entry) if entry.tool == tool && entry.size == size && entry.modified == modified => {
            return Some(prune(entry, first_kept));
        }
        // Appended to: carry on from where the last scan stopped.
        Some(entry) if entry.tool == tool && entry.offset <= size => entry,
        _ => FileEntry {
            tool,
            size: 0,
            modified: 0,
            offset: 0,
            codex: CodexState::default(),
            rows: Vec::new(),
            claude: Vec::new(),
        },
    };
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(entry.offset)).ok()?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let read = match tool {
        CostTool::Codex => {
            let mut state = entry.codex.clone();
            let rows = &mut entry.rows;
            let read = for_each_line(reader, CODEX_PREFIX_BYTES, is_codex_usage_line, |line| {
                read_codex_line(line, &mut state, rows, first_kept);
            });
            entry.codex = state;
            read
        }
        CostTool::Claude => {
            let records = &mut entry.claude;
            for_each_line(
                reader,
                usize::MAX,
                |_| true,
                |line| {
                    if let Some(record) = read_claude_line(line, first_kept) {
                        records.push(record);
                    }
                },
            )
        }
    };
    let Ok(read) = read else {
        return None;
    };
    entry.offset += read;
    entry.size = size;
    entry.modified = modified;
    Some(prune(entry, first_kept))
}

fn prune(mut entry: FileEntry, first_kept: NaiveDate) -> FileEntry {
    entry.rows.retain(|row| row.day >= first_kept);
    entry.claude.retain(|record| record.day >= first_kept);
    entry
}

/// Calls `handle` with each complete line that `wanted` accepts after seeing
/// its first `decide_after` bytes, without holding the rest of an unwanted
/// line in memory. Returns the bytes consumed through the last complete line,
/// so a line still being written is read whole next time.
pub(super) fn for_each_line(
    mut reader: impl BufRead,
    decide_after: usize,
    wanted: impl Fn(&[u8]) -> bool,
    mut handle: impl FnMut(&[u8]),
) -> io::Result<u64> {
    let mut line = Vec::new();
    let mut decided: Option<bool> = None;
    let mut consumed = 0u64;
    let mut complete = 0u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(complete);
        }
        let (part, ends_line) = match memchr::memchr(b'\n', available) {
            Some(end) => (&available[..end], true),
            None => (available, false),
        };
        if decided != Some(false) {
            line.extend_from_slice(part);
            if decided.is_none() && line.len() >= decide_after {
                decided = Some(wanted(&line[..decide_after]));
                if decided == Some(false) {
                    line.clear();
                }
            }
        }
        let used = part.len() + usize::from(ends_line);
        reader.consume(used);
        consumed += used as u64;
        if ends_line {
            let keep = decided.unwrap_or_else(|| wanted(&line));
            if keep {
                handle(&line);
            }
            line.clear();
            decided = None;
            complete = consumed;
        }
    }
}

fn is_codex_usage_line(prefix: &[u8]) -> bool {
    memmem::find(prefix, br#""type":"token_count""#).is_some()
        || memmem::find(prefix, br#""type":"turn_context""#).is_some()
}

#[derive(Deserialize)]
struct CodexLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<CodexPayload>,
}

#[derive(Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    model: Option<String>,
    info: Option<CodexInfo>,
}

#[derive(Deserialize)]
struct CodexInfo {
    total_token_usage: Option<CodexUsage>,
    last_token_usage: Option<CodexUsage>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

/// Counts one Codex `token_count` event, or notes the model a
/// `turn_context` switches to.
///
/// Each event carries the turn's own usage (`last_token_usage`) and the
/// session's running total. The turn's own usage is counted, because a
/// forked session's running total starts from its parent's; the running
/// total only spots an event repeated with nothing new.
fn read_codex_line(
    line: &[u8],
    state: &mut CodexState,
    rows: &mut Vec<UsageRow>,
    first_kept: NaiveDate,
) {
    let Ok(parsed) = serde_json::from_slice::<CodexLine>(line) else {
        return;
    };
    let Some(payload) = parsed.payload else {
        return;
    };
    if parsed.kind.as_deref() == Some("turn_context") {
        if let Some(model) = payload.model.filter(|model| !model.trim().is_empty()) {
            state.model = Some(model);
        }
        return;
    }
    if payload.kind.as_deref() != Some("token_count") {
        return;
    }
    let Some(info) = payload.info else {
        return;
    };
    let total = info.total_token_usage.as_ref().map(|usage| {
        if usage.total_tokens > 0 {
            usage.total_tokens
        } else {
            usage.input_tokens + usage.output_tokens
        }
    });
    if total.is_some() && total == state.last_total {
        return;
    }
    if total.is_some() {
        state.last_total = total;
    }
    let Some(last) = info.last_token_usage else {
        return;
    };
    let model = info
        .model
        .or(payload.model)
        .filter(|model| !model.trim().is_empty())
        .or_else(|| state.model.clone())
        .unwrap_or_else(|| "unknown".to_owned());
    let Some(day) = parsed.timestamp.as_deref().and_then(local_day) else {
        return;
    };
    if day < first_kept {
        return;
    }
    // Codex counts cached reads and cache writes inside `input_tokens`.
    let cache_read = last.cached_input_tokens.min(last.input_tokens);
    let cache_write = last
        .cache_write_input_tokens
        .min(last.input_tokens - cache_read);
    let tokens = TokenCounts {
        input: last.input_tokens - cache_read - cache_write,
        cache_read,
        cache_write,
        cache_write_1h: 0,
        output: last.output_tokens,
        reasoning: last.reasoning_output_tokens.min(last.output_tokens),
    };
    if tokens.total() == 0 {
        return;
    }
    let long_context = last.input_tokens > CODEX_LONG_CONTEXT_TOKENS;
    add_usage(rows, day, &model, long_context, &tokens);
}

#[derive(Deserialize)]
struct ClaudeLine {
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    cache_creation: Option<ClaudeCacheCreation>,
}

#[derive(Deserialize)]
struct ClaudeCacheCreation {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

/// One Claude Code response's usage. Claude reports input, cache reads, and
/// cache writes as separate counts.
fn read_claude_line(line: &[u8], first_kept: NaiveDate) -> Option<ClaudeRecord> {
    memmem::find(line, br#""usage""#)?;
    let parsed = serde_json::from_slice::<ClaudeLine>(line).ok()?;
    let message = parsed.message?;
    let usage = message.usage?;
    let model = message.model.filter(|model| {
        let model = model.trim();
        !model.is_empty() && model != "<synthetic>"
    })?;
    let day = parsed.timestamp.as_deref().and_then(local_day)?;
    if day < first_kept {
        return None;
    }
    let tokens = TokenCounts {
        input: usage.input_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_creation_input_tokens,
        cache_write_1h: usage
            .cache_creation
            .map_or(0, |creation| creation.ephemeral_1h_input_tokens)
            .min(usage.cache_creation_input_tokens),
        output: usage.output_tokens,
        reasoning: 0,
    };
    if tokens.total() == 0 {
        return None;
    }
    let key = match (message.id, parsed.request_id) {
        (Some(id), Some(request)) => format!("{id}:{request}"),
        (Some(id), None) => id,
        // Nothing identifies the response, so it is never treated as a repeat.
        _ => format!("{}:{}", parsed.timestamp.unwrap_or_default(), line.len()),
    };
    Some(ClaudeRecord {
        key,
        day,
        model,
        tokens,
    })
}

/// The local calendar day of an RFC 3339 timestamp.
fn local_day(timestamp: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|time| time.with_timezone(&Local).date_naive())
}

fn add_usage(
    rows: &mut Vec<UsageRow>,
    day: NaiveDate,
    model: &str,
    long_context: bool,
    tokens: &TokenCounts,
) {
    if let Some(row) = rows
        .iter_mut()
        .find(|row| row.day == day && row.long_context == long_context && row.model == model)
    {
        row.tokens.add(tokens);
        row.records += 1;
    } else {
        rows.push(UsageRow {
            day,
            model: model.to_owned(),
            long_context,
            tokens: *tokens,
            records: 1,
        });
    }
}
