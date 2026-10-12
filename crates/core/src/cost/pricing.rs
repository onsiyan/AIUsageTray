//! API list prices per model, from models.dev.
//!
//! A table ships inside the app so costs work offline; once a day a fresh one
//! is downloaded from models.dev (no key needed) and kept beside the scan
//! cache. Downloaded prices win, and models the download lacks keep the
//! shipped price.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{CostTool, TokenCounts};

pub(super) const BUNDLED_PRICES: &str = include_str!("prices.json");
const DOWNLOADED_FILE: &str = "prices.json";
/// When the last download was tried, so a failing one is not retried on
/// every scan.
const ATTEMPT_FILE: &str = "prices-attempt.txt";
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const REFRESH_AFTER: chrono::Duration = chrono::Duration::hours(24);
const RETRY_AFTER: chrono::Duration = chrono::Duration::hours(1);
/// models.dev providers whose prices the logs need.
const PROVIDERS: [&str; 3] = ["openai", "anthropic", "google"];

/// Where the prices in a report came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceSource {
    /// Downloaded from models.dev, rather than the table shipped in the app.
    pub downloaded: bool,
    pub updated_at: Option<DateTime<Utc>>,
}

/// USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct ModelPrice {
    input: f64,
    output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
    /// Rates for requests past a long-context threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    long: Option<LongContextPrice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct LongContextPrice {
    threshold: u64,
    input: f64,
    output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PriceFile {
    fetched_at: Option<DateTime<Utc>>,
    /// Keyed `provider/model`, such as `openai/gpt-5.5`.
    models: HashMap<String, ModelPrice>,
}

/// What tokens cost at list price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Priced {
    pub cost_usd: f64,
    /// The cache reads at the full input rate, less what they cost.
    pub cache_savings_usd: f64,
}

#[derive(Debug, Clone)]
pub struct PriceTable {
    models: HashMap<String, ModelPrice>,
    pub source: PriceSource,
}

impl PriceTable {
    /// The shipped table, overlaid with the last download if there is one.
    pub fn load(cache_directory: &Path) -> Self {
        let bundled = serde_json::from_str::<PriceFile>(BUNDLED_PRICES).unwrap_or_default();
        let downloaded = fs::read(cache_directory.join(DOWNLOADED_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PriceFile>(&bytes).ok())
            .filter(|file| !file.models.is_empty());
        let mut models = bundled.models;
        let source = match downloaded {
            Some(downloaded) => {
                models.extend(downloaded.models);
                PriceSource {
                    downloaded: true,
                    updated_at: downloaded.fetched_at,
                }
            }
            None => PriceSource {
                downloaded: false,
                updated_at: bundled.fetched_at,
            },
        };
        Self { models, source }
    }

    fn lookup(&self, tool: CostTool, model: &str) -> Option<&ModelPrice> {
        match tool {
            CostTool::Codex => self.lookup_in("openai", model_keys(model)),
            CostTool::Claude => self.lookup_in("anthropic", model_keys(model)),
            CostTool::Antigravity => {
                let keys = antigravity_model_keys(model);
                let name = keys.first().map_or("", String::as_str);
                if name.starts_with("claude") {
                    self.lookup_in("anthropic", keys)
                } else if name.starts_with("gpt") {
                    self.lookup_in("openai", keys)
                } else {
                    self.lookup_in("google", keys.clone())
                        .or_else(|| self.latest_gemini(name))
                }
            }
        }
    }

    fn lookup_in(&self, provider: &str, keys: Vec<String>) -> Option<&ModelPrice> {
        keys.into_iter()
            .find_map(|model| self.models.get(&format!("{provider}/{model}")))
    }

    /// A Gemini name with no version, such as `gemini-pro-default`: the
    /// newest Gemini of that kind (`pro` or `flash`) with a price.
    fn latest_gemini(&self, name: &str) -> Option<&ModelPrice> {
        let kind = ["pro", "flash"]
            .into_iter()
            .find(|kind| name.starts_with(&format!("gemini-{kind}")))?;
        self.models
            .iter()
            .filter_map(|(key, price)| {
                let rest = key.strip_prefix("google/gemini-")?;
                let (version, model_kind) = rest.split_once('-')?;
                let model_kind = model_kind.strip_suffix("-preview").unwrap_or(model_kind);
                let number = version.parse::<f64>().ok()?;
                (model_kind == kind).then_some((number, price))
            })
            .max_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, price)| price)
    }

    /// The list price of `tokens`, or `None` when the model's price is
    /// unknown.
    pub fn cost(
        &self,
        tool: CostTool,
        model: &str,
        tokens: &TokenCounts,
        long_context: bool,
    ) -> Option<f64> {
        self.price(tool, model, tokens, long_context)
            .map(|priced| priced.cost_usd)
    }

    /// The list price of `tokens` and what their cache reads saved.
    pub fn price(
        &self,
        tool: CostTool,
        model: &str,
        tokens: &TokenCounts,
        long_context: bool,
    ) -> Option<Priced> {
        let price = self.lookup(tool, model)?;
        let (input, output, cache_read, cache_write) = match &price.long {
            Some(long) if long_context => (
                long.input,
                long.output,
                long.cache_read.or(price.cache_read),
                long.cache_write.or(price.cache_write),
            ),
            _ => (
                price.input,
                price.output,
                price.cache_read,
                price.cache_write,
            ),
        };
        let cache_read = cache_read.unwrap_or(input);
        let cache_write = cache_write.unwrap_or(input);
        let five_minute_writes = tokens.cache_write - tokens.cache_write_1h.min(tokens.cache_write);
        let dollars = tokens.input as f64 * input
            + tokens.cache_read as f64 * cache_read
            + five_minute_writes as f64 * cache_write
            // Anthropic bills one-hour cache writes at twice the input rate.
            + tokens.cache_write_1h.min(tokens.cache_write) as f64 * input * 2.0
            + tokens.output as f64 * output;
        let savings = tokens.cache_read as f64 * (input - cache_read).max(0.0);
        Some(Priced {
            cost_usd: dollars / 1_000_000.0,
            cache_savings_usd: savings / 1_000_000.0,
        })
    }
}

/// The names to try for a logged model: as logged, then without a context
/// tag (`[1m]`), a provider prefix, or a date suffix (`-20251001`).
pub(super) fn model_keys(model: &str) -> Vec<String> {
    let mut name = model.trim().to_ascii_lowercase();
    if let Some(tag) = name.find('[') {
        name.truncate(tag);
    }
    for prefix in ["openai/", "anthropic/"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.to_owned();
        }
    }
    let mut keys = vec![name.clone()];
    if let Some((base, date)) = name.rsplit_once('-')
        && date.len() == 8
        && date.bytes().all(|byte| byte.is_ascii_digit())
    {
        keys.push(base.to_owned());
    }
    keys
}

/// The names to try for a model Antigravity logged. Its names carry
/// serving tags after the model (`gemini-3.8-flash-tiered`,
/// `gemini-3.1-pro-low`, `claude-opus-4-6-thinking`), so each name is tried
/// with its tags dropped one at a time from the end, and each also as a
/// `-preview` model.
pub(super) fn antigravity_model_keys(model: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut name = model.trim().to_ascii_lowercase();
    loop {
        keys.push(name.clone());
        keys.push(format!("{name}-preview"));
        match name.rsplit_once('-') {
            // Stop at the model family, before a bare `gemini` or `claude`.
            Some((rest, _)) if rest.contains('-') => name = rest.to_owned(),
            _ => break,
        }
    }
    keys
}

/// Downloads models.dev prices when the kept copy is a day old (or missing),
/// trying at most once an hour. Returns whether new prices were saved.
pub async fn refresh_prices_if_stale(cache_directory: &Path) -> Result<bool, String> {
    let now = Utc::now();
    let kept = fs::read(cache_directory.join(DOWNLOADED_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PriceFile>(&bytes).ok())
        .and_then(|file| file.fetched_at);
    if kept.is_some_and(|fetched| now - fetched < REFRESH_AFTER) {
        return Ok(false);
    }
    let attempted = fs::read_to_string(cache_directory.join(ATTEMPT_FILE))
        .ok()
        .and_then(|text| DateTime::parse_from_rfc3339(text.trim()).ok());
    if attempted.is_some_and(|attempted| now - attempted.with_timezone(&Utc) < RETRY_AFTER) {
        return Ok(false);
    }
    fs::create_dir_all(cache_directory).map_err(|error| error.to_string())?;
    let _ = fs::write(cache_directory.join(ATTEMPT_FILE), now.to_rfc3339());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(MODELS_DEV_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?;
    let catalog = response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())?;
    let models = models_dev_prices(&catalog);
    if models.is_empty() {
        return Err("models.dev listed no prices".to_owned());
    }
    let file = PriceFile {
        fetched_at: Some(now),
        models,
    };
    let bytes = serde_json::to_vec(&file).map_err(|error| error.to_string())?;
    let path = cache_directory.join(DOWNLOADED_FILE);
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
    Ok(true)
}

/// The OpenAI, Anthropic, and Google prices in a models.dev catalog.
pub(super) fn models_dev_prices(catalog: &Value) -> HashMap<String, ModelPrice> {
    let mut prices = HashMap::new();
    for provider in PROVIDERS {
        let Some(models) = catalog
            .get(provider)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (id, model) in models {
            if let Some(price) = model.get("cost").and_then(model_price) {
                prices.insert(format!("{provider}/{}", id.to_ascii_lowercase()), price);
            }
        }
    }
    prices
}

fn model_price(cost: &Value) -> Option<ModelPrice> {
    let rate = |value: &Value, field: &str| {
        value
            .get(field)
            .and_then(Value::as_f64)
            .filter(|rate| rate.is_finite() && *rate >= 0.0)
    };
    // models.dev lists context tiers; older entries only the 200k one.
    let long = cost
        .get("tiers")
        .and_then(Value::as_array)
        .and_then(|tiers| tiers.first())
        .and_then(|tier| {
            let threshold = tier.get("tier")?.get("size")?.as_u64()?;
            Some((threshold, tier))
        })
        .or_else(|| cost.get("context_over_200k").map(|tier| (200_000, tier)))
        .and_then(|(threshold, tier)| {
            Some(LongContextPrice {
                threshold,
                input: rate(tier, "input")?,
                output: rate(tier, "output")?,
                cache_read: rate(tier, "cache_read"),
                cache_write: rate(tier, "cache_write"),
            })
        });
    Some(ModelPrice {
        input: rate(cost, "input")?,
        output: rate(cost, "output")?,
        cache_read: rate(cost, "cache_read"),
        cache_write: rate(cost, "cache_write"),
        long,
    })
}
