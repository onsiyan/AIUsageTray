//! Usage the app has seen, kept after the tools delete their logs.
//!
//! Codex and Claude Code clear old sessions on their own, so each scan's
//! totals by day and model are saved here, for this PC and for every machine
//! read over SSH, and joined with what the logs still hold.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::{CostTool, UsageRow};

pub(super) const HISTORY_FILE: &str = "history.json";

/// Usage by tool, summed by day and model.
pub(super) type ToolRows = BTreeMap<CostTool, Vec<UsageRow>>;

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct History {
    /// The reading rules ([`super::scan::CACHE_VERSION`]) the rows were
    /// counted under.
    rules: u32,
    /// By machine name; this PC is the empty name.
    machines: BTreeMap<String, ToolRows>,
}

impl History {
    pub(super) fn load(cache_directory: &Path) -> Self {
        fs::read(cache_directory.join(HISTORY_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Joins what `machine`'s logs hold now with what earlier scans saw, in
    /// both `rows` and the history.
    ///
    /// A log only grows until it is deleted, so of the two figures for a
    /// day's model the larger is kept. Under new reading rules the logs'
    /// figures replace the old ones wherever the logs still cover the day.
    pub(super) fn keep(&mut self, machine: &str, rows: &mut ToolRows, rules: u32) {
        let same_rules = self.rules == rules;
        let kept = self.machines.entry(machine.to_owned()).or_default();
        for tool in CostTool::ALL {
            let live = rows.remove(&tool).unwrap_or_default();
            let old = kept.remove(&tool).unwrap_or_default();
            let covered = live.iter().map(|row| row.day).collect::<Vec<NaiveDate>>();
            let mut joined: BTreeMap<(NaiveDate, String, bool), UsageRow> = BTreeMap::new();
            for row in old {
                if same_rules || !covered.contains(&row.day) {
                    joined.insert((row.day, row.model.clone(), row.long_context), row);
                }
            }
            for row in live {
                let key = (row.day, row.model.clone(), row.long_context);
                match joined.get(&key) {
                    Some(earlier) if earlier.tokens.total() > row.tokens.total() => {}
                    _ => {
                        joined.insert(key, row);
                    }
                }
            }
            if joined.is_empty() {
                continue;
            }
            let joined = joined.into_values().collect::<Vec<_>>();
            kept.insert(tool, joined.clone());
            rows.insert(tool, joined);
        }
    }

    pub(super) fn save(&mut self, cache_directory: &Path, rules: u32) {
        self.rules = rules;
        if fs::create_dir_all(cache_directory).is_err() {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(self) else {
            return;
        };
        let path = cache_directory.join(HISTORY_FILE);
        let temporary = path.with_extension("json.tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::rename(&temporary, &path);
        }
    }
}
