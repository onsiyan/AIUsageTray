//! The subscriptions the Cost tab weighs the API value against: the plan of
//! each saved Codex and Claude account, at its monthly list price or the
//! price the user set for that plan.

use std::{collections::BTreeMap, fs, io};

use usage_monitor_core::cost::CostTool;

use crate::{
    UsageProvider,
    dashboard::{AccountUsageEntry, belongs_to_provider},
};

/// `key<TAB>monthly dollars` per line, for plans whose price the user set.
const PLAN_PRICES_FILE: &str = "plan-prices.txt";

/// The days a monthly price is spread over.
pub(super) const DAYS_PER_MONTH: f64 = 30.0;

/// One plan and how many saved accounts are on it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Plan {
    /// `codex:team`, `claude:claude pro`: the tool and the plan as reported.
    pub key: String,
    pub tool: CostTool,
    pub name: String,
    pub accounts: usize,
    /// One account's monthly price; `None` when the plan has no list price
    /// and the user set none.
    pub monthly_usd: Option<f64>,
    /// The user set this price.
    pub custom: bool,
}

impl Plan {
    /// What the plan's accounts cost over `days` days.
    pub fn paid_over(&self, days: usize) -> Option<f64> {
        self.monthly_usd
            .map(|price| price * self.accounts as f64 * days as f64 / DAYS_PER_MONTH)
    }
}

/// The plans of the saved Codex and Claude accounts, Codex first, then by
/// name. Accounts whose plan is not known yet are left out.
pub(super) fn plans(entries: &[AccountUsageEntry], prices: &BTreeMap<String, f64>) -> Vec<Plan> {
    let mut counted: BTreeMap<(u8, String), (CostTool, String, usize)> = BTreeMap::new();
    for entry in entries {
        let Some(plan) = entry
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.plan_type.as_deref())
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
        else {
            continue;
        };
        let tool = if belongs_to_provider(&entry.account.provider_id, UsageProvider::Codex) {
            CostTool::Codex
        } else if belongs_to_provider(&entry.account.provider_id, UsageProvider::Claude) {
            CostTool::Claude
        } else {
            continue;
        };
        let order = match tool {
            CostTool::Codex => 0,
            CostTool::Claude => 1,
        };
        counted
            .entry((order, plan_key(tool, plan)))
            .or_insert_with(|| (tool, plan_name(tool, plan), 0))
            .2 += 1;
    }
    counted
        .into_iter()
        .map(|((_, key), (tool, name, accounts))| {
            let custom = prices.get(&key).copied();
            let plan = key.split_once(':').map_or("", |(_, plan)| plan);
            Plan {
                monthly_usd: custom.or_else(|| list_price(tool, plan)),
                custom: custom.is_some(),
                key,
                tool,
                name,
                accounts,
            }
        })
        .collect()
}

fn plan_key(tool: CostTool, plan: &str) -> String {
    let tool = match tool {
        CostTool::Codex => "codex",
        CostTool::Claude => "claude",
    };
    format!("{tool}:{}", plan.to_lowercase())
}

/// `Business`, `Pro`, `Max 20x`: the plan without the tool's name.
fn plan_name(tool: CostTool, plan: &str) -> String {
    match tool {
        CostTool::Codex => match plan.to_lowercase().as_str() {
            // ChatGPT Team is now called Business.
            "team" | "business" => "Business".to_owned(),
            _ => capitalize(plan),
        },
        CostTool::Claude => {
            let name = plan.strip_prefix("Claude ").unwrap_or(plan);
            capitalize(name)
        }
    }
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default()
}

/// Monthly list price in dollars, billed month to month, for the plans
/// whose price is public. `plan` is lowercase.
fn list_price(tool: CostTool, plan: &str) -> Option<f64> {
    let price = match (tool, plan) {
        (CostTool::Codex, "free" | "guest") => 0.0,
        (CostTool::Codex, "plus") => 20.0,
        (CostTool::Codex, "pro") => 200.0,
        (CostTool::Codex, "team" | "business") => 30.0,
        (CostTool::Claude, "claude free") => 0.0,
        (CostTool::Claude, "claude pro") => 20.0,
        (CostTool::Claude, "claude max" | "claude max 5x") => 100.0,
        (CostTool::Claude, "claude max 20x") => 200.0,
        (CostTool::Claude, "claude team" | "claude team standard") => 25.0,
        (CostTool::Claude, "claude team premium") => 125.0,
        _ => return None,
    };
    Some(price)
}

pub(super) fn load_prices() -> BTreeMap<String, f64> {
    crate::theme::preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(PLAN_PRICES_FILE)).ok())
        .map(|contents| parse_prices(&contents))
        .unwrap_or_default()
}

fn parse_prices(contents: &str) -> BTreeMap<String, f64> {
    contents
        .lines()
        .filter_map(|line| {
            let (key, price) = line.split_once('\t')?;
            let price = price.trim().parse::<f64>().ok()?;
            (price.is_finite() && price >= 0.0).then(|| (key.trim().to_owned(), price))
        })
        .collect()
}

pub(super) fn save_prices(prices: &BTreeMap<String, f64>) -> io::Result<()> {
    let directory = crate::theme::preference_directory()?;
    fs::create_dir_all(&directory)?;
    let contents = prices
        .iter()
        .map(|(key, price)| format!("{key}\t{price}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(directory.join(PLAN_PRICES_FILE), contents)
}

/// A typed price: `30`, `$25.50`, `٣٠`. `None` for anything else.
pub(super) fn parse_typed_price(typed: &str) -> Option<f64> {
    let digits = typed
        .trim()
        .trim_start_matches('$')
        .trim()
        .chars()
        .map(|character| match character {
            // Arabic-Indic digits and decimal separator.
            '٠'..='٩' => char::from(b'0' + (character as u32 - '٠' as u32) as u8),
            '٫' | ',' => '.',
            other => other,
        })
        .collect::<String>();
    digits
        .parse::<f64>()
        .ok()
        .filter(|price| price.is_finite() && *price >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::{list_price, parse_prices, parse_typed_price, plan_key, plan_name};
    use usage_monitor_core::cost::CostTool;

    #[test]
    fn plans_read_as_their_public_names_and_prices() {
        assert_eq!(plan_name(CostTool::Codex, "team"), "Business");
        assert_eq!(plan_name(CostTool::Codex, "plus"), "Plus");
        assert_eq!(plan_name(CostTool::Claude, "Claude Max 20x"), "Max 20x");
        assert_eq!(
            plan_key(CostTool::Claude, "Claude Pro"),
            "claude:claude pro"
        );
        assert_eq!(list_price(CostTool::Codex, "team"), Some(30.0));
        assert_eq!(list_price(CostTool::Claude, "claude pro"), Some(20.0));
        assert_eq!(list_price(CostTool::Codex, "enterprise"), None);
    }

    #[test]
    fn typed_and_saved_prices_parse() {
        assert_eq!(parse_typed_price(" $25.50 "), Some(25.5));
        assert_eq!(parse_typed_price("٣٠"), Some(30.0));
        assert_eq!(parse_typed_price("-4"), None);
        assert_eq!(parse_typed_price("abc"), None);
        let saved = parse_prices("codex:team\t25\nbroken\nclaude:claude pro\tx");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved.get("codex:team"), Some(&25.0));
    }
}
