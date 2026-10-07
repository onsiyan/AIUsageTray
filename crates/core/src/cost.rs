//! What Codex and Claude Code usage on this PC would cost at API list prices,
//! read from the tools' own session logs (`~/.codex/sessions`,
//! `~/.claude/projects`).
//!
//! The figures are estimates: subscription plans bill separately, and the
//! logs belong to the PC rather than to one account, so every account that
//! signed in here is counted together. Each file is read once and then only
//! from where the last scan stopped, so later scans cost little even when the
//! logs run to gigabytes.

mod pricing;
mod scan;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

pub use pricing::{PriceSource, PriceTable, Priced, refresh_prices_if_stale};
pub use scan::LogRoots;

/// Days a report covers, today included.
pub const REPORT_DAYS: u32 = 30;

/// The tool whose logs a figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostTool {
    Codex,
    Claude,
}

impl CostTool {
    pub const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }
}

/// Tokens by how they are billed. The classes do not overlap, except that
/// `cache_write_1h` is part of `cache_write` and `reasoning` part of `output`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    /// Input read at the full rate (not from the cache).
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// Claude's one-hour cache writes, billed at twice the input rate.
    #[serde(default)]
    pub cache_write_1h: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
}

impl TokenCounts {
    pub fn add(&mut self, other: &Self) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cache_write_1h += other.cache_write_1h;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }

    /// Every token the model read or wrote.
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

/// Usage of one model on one day, as stored between scans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct UsageRow {
    day: NaiveDate,
    model: String,
    /// The request was past the model's long-context threshold, where
    /// higher rates apply.
    #[serde(default)]
    long_context: bool,
    tokens: TokenCounts,
    records: u64,
}

/// What the logs on this PC add up to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostReport {
    pub generated_at: DateTime<Utc>,
    pub today: NaiveDate,
    pub tools: Vec<ToolCost>,
    pub prices: PriceSource,
    pub scan_ms: u64,
}

impl CostReport {
    pub fn tool(&self, tool: CostTool) -> Option<&ToolCost> {
        self.tools.iter().find(|cost| cost.tool == tool)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCost {
    pub tool: CostTool,
    /// Whether the tool's log folder exists on this PC.
    pub logs_found: bool,
    /// One entry per day, oldest first, ending today; quiet days are zero.
    pub days: Vec<DayCost>,
    /// Each model's use per day, for totals over any part of the report.
    pub model_days: Vec<ModelDay>,
}

impl ToolCost {
    /// Cost and tokens over the last `days` days, today included.
    pub fn last(&self, days: usize) -> DayCost {
        let mut total = DayCost {
            day: self.days.last().map_or(NaiveDate::MIN, |day| day.day),
            ..DayCost::default()
        };
        for day in self.days.iter().rev().take(days) {
            total.cost_usd += day.cost_usd;
            total.cache_savings_usd += day.cache_savings_usd;
            total.tokens.add(&day.tokens);
            total.unpriced_tokens += day.unpriced_tokens;
        }
        total
    }

    /// Each model's cost and tokens over the last `days` days, most
    /// expensive first.
    pub fn models(&self, days: usize) -> Vec<ModelCost> {
        let first = self
            .days
            .len()
            .checked_sub(days)
            .and_then(|index| self.days.get(index))
            .or(self.days.first())
            .map_or(NaiveDate::MIN, |day| day.day);
        let mut models: BTreeMap<&str, ModelCost> = BTreeMap::new();
        for usage in self.model_days.iter().filter(|usage| usage.day >= first) {
            let model = models.entry(&usage.model).or_insert_with(|| ModelCost {
                model: usage.model.clone(),
                cost_usd: Some(0.0),
                tokens: TokenCounts::default(),
            });
            model.tokens.add(&usage.tokens);
            model.cost_usd = model
                .cost_usd
                .zip(usage.cost_usd)
                .map(|(total, cost)| total + cost);
        }
        let mut models = models.into_values().collect::<Vec<_>>();
        models.sort_by(|left, right| {
            right
                .cost_usd
                .unwrap_or(-1.0)
                .total_cmp(&left.cost_usd.unwrap_or(-1.0))
                .then(right.tokens.total().cmp(&left.tokens.total()))
        });
        models
    }

    pub fn has_usage(&self) -> bool {
        self.days.iter().any(|day| day.tokens.total() > 0)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DayCost {
    pub day: NaiveDate,
    pub cost_usd: f64,
    /// What the cache reads saved against paying the full input rate.
    #[serde(default)]
    pub cache_savings_usd: f64,
    pub tokens: TokenCounts,
    /// Tokens of models without a known price, left out of `cost_usd`.
    pub unpriced_tokens: u64,
}

/// One model's use on one day.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDay {
    pub day: NaiveDate,
    pub model: String,
    /// `None` when the model has no known price.
    pub cost_usd: Option<f64>,
    pub cache_savings_usd: f64,
    pub tokens: TokenCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub model: String,
    /// `None` when the model has no known price.
    pub cost_usd: Option<f64>,
    pub tokens: TokenCounts,
}

/// Where scan results and downloaded prices are kept.
pub fn default_cache_directory() -> PathBuf {
    crate::storage::default_accounts_database_path()
        .parent()
        .map(|directory| directory.join("cost"))
        .unwrap_or_else(|| std::env::temp_dir().join("UsageMonitor-cost"))
}

/// Reads what is new in the logs and reports the last [`REPORT_DAYS`] days.
/// Blocking: logs can be large, so hosts run it off the UI thread.
pub fn scan_report(roots: &LogRoots, cache_directory: &std::path::Path) -> CostReport {
    let started = std::time::Instant::now();
    let now = Local::now();
    let today = now.date_naive();
    let first_day = today
        .checked_sub_days(Days::new(u64::from(REPORT_DAYS - 1)))
        .unwrap_or(today);
    let usage = scan::scan(roots, cache_directory, today);
    let prices = PriceTable::load(cache_directory);
    let tools = CostTool::ALL
        .into_iter()
        .map(|tool| {
            let rows = usage.rows.get(&tool).map(Vec::as_slice).unwrap_or_default();
            build_tool_cost(
                tool,
                usage.found.contains(&tool),
                rows,
                first_day,
                today,
                &prices,
            )
        })
        .collect();
    CostReport {
        generated_at: now.with_timezone(&Utc),
        today,
        tools,
        prices: prices.source.clone(),
        scan_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

fn build_tool_cost(
    tool: CostTool,
    logs_found: bool,
    rows: &[UsageRow],
    first_day: NaiveDate,
    today: NaiveDate,
    prices: &PriceTable,
) -> ToolCost {
    let mut days = BTreeMap::new();
    let mut day = first_day;
    while day <= today {
        days.insert(
            day,
            DayCost {
                day,
                ..DayCost::default()
            },
        );
        day = day.succ_opt().unwrap_or(today + Days::new(1));
    }
    let mut model_days: BTreeMap<(NaiveDate, &str), ModelDay> = BTreeMap::new();
    for row in rows {
        let Some(day) = days.get_mut(&row.day) else {
            continue;
        };
        let priced = prices.price(tool, &row.model, &row.tokens, row.long_context);
        day.tokens.add(&row.tokens);
        let usage = model_days
            .entry((row.day, &row.model))
            .or_insert_with(|| ModelDay {
                day: row.day,
                model: row.model.clone(),
                cost_usd: Some(0.0),
                cache_savings_usd: 0.0,
                tokens: TokenCounts::default(),
            });
        usage.tokens.add(&row.tokens);
        match priced {
            Some(priced) => {
                day.cost_usd += priced.cost_usd;
                day.cache_savings_usd += priced.cache_savings_usd;
                usage.cache_savings_usd += priced.cache_savings_usd;
                if let Some(total) = &mut usage.cost_usd {
                    *total += priced.cost_usd;
                }
            }
            None => {
                day.unpriced_tokens += row.tokens.total();
                usage.cost_usd = None;
            }
        }
    }
    ToolCost {
        tool,
        logs_found,
        days: days.into_values().collect(),
        model_days: model_days.into_values().collect(),
    }
}
