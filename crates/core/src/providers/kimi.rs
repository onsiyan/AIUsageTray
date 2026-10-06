//! Kimi Code subscription quotas through the Kimi Code API.
//!
//! A Kimi Code API key reads `GET /coding/v1/usages` on its region's host:
//! `api.kimi.com` for keys from kimi.com (China, the default) or
//! `api.kimi.ai` for keys from kimi.ai (International). The region that
//! accepts the key is found when the account is added and kept as the
//! account's secondary token. Following CodexBar, two response shapes are
//! understood:
//!
//! - ratio pools under `usages`: `limit_5h`, `limit_7d`, and
//!   `limit_month_total`, each a `used_ratio` with its `reset_time`;
//! - the older counts: `usage` for the weekly quota and `limits[0]` for the
//!   short rate-limit window, as `limit` / `used` / `remaining` strings.
//!
//! Ratio pools win where present. A missing window stays missing; nothing is
//! synthesized.

use crate::{
    accounts::{AccountRecord, KIMI, VerifiedIdentity},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{invalid_payload, json_string, map_http_error, missing_auth},
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, RateLimitWindow, UsageAdapter, UsagePrimaryWindowKind,
        UsageProbeResult, UsageSnapshot, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use url::Url;

const USER_AGENT: &str = "UsageMonitor/0.1";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(8);
const FIVE_HOURS: i64 = 5 * 60 * 60;
const WEEK: i64 = 7 * 24 * 60 * 60;

pub const FIVE_HOUR_WINDOW_NAME: &str = "5 hours";
pub const WEEKLY_WINDOW_NAME: &str = "Weekly";
pub const MONTHLY_WINDOW_NAME: &str = "Total usage";

/// The Kimi site an API key was created on; each has its own API host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiRegion {
    China,
    International,
}

impl KimiRegion {
    pub const INTERNATIONAL: &'static str = "international";

    fn usage_url(self) -> Url {
        Url::parse(match self {
            Self::China => "https://api.kimi.com/coding/v1/usages",
            Self::International => "https://api.kimi.ai/coding/v1/usages",
        })
        .expect("static Kimi usage URL")
    }

    /// The region stored with an account; anything else is China, where
    /// every account added before regions were known came from.
    pub fn from_stored(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(Self::INTERNATIONAL) => Self::International,
            _ => Self::China,
        }
    }

    /// The value to store with an account, `None` for the default.
    pub fn stored(self) -> Option<&'static str> {
        match self {
            Self::China => None,
            Self::International => Some(Self::INTERNATIONAL),
        }
    }
}

async fn fetch_usage(
    transport: &dyn UsageHttpTransport,
    region: KimiRegion,
    api_key: &str,
    deadline: Duration,
) -> Result<UsageHttpResponse, TransportError> {
    tokio::time::timeout(
        deadline,
        transport.send(UsageHttpRequest {
            method: Method::GET,
            url: region.usage_url(),
            headers: BTreeMap::from([
                ("Authorization".to_owned(), format!("Bearer {api_key}")),
                ("Accept".to_owned(), "application/json".to_owned()),
                ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ]),
            body: None,
        }),
    )
    .await
    .map_err(|_| TransportError::Timeout("kimi code usage".to_owned()))?
}

/// Finds the region whose host accepts the key and answers with usage.
pub async fn detect_region(
    transport: &dyn UsageHttpTransport,
    api_key: &str,
) -> Result<KimiRegion, String> {
    let mut last_status = None;
    for region in [KimiRegion::China, KimiRegion::International] {
        let response = fetch_usage(transport, region, api_key, DEFAULT_DEADLINE)
            .await
            .map_err(|error| format!("Could not reach Kimi Code: {error}"))?;
        if response.is_success() && parse_usage(&response.body).is_ok() {
            return Ok(region);
        }
        last_status = Some(response.status_code);
    }
    Err(format!(
        "Neither api.kimi.com nor api.kimi.ai accepted this API key (HTTP {})",
        last_status.unwrap_or_default()
    ))
}

pub struct KimiUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    deadline: Duration,
}

impl KimiUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            deadline: DEFAULT_DEADLINE,
        })
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

#[async_trait]
impl UsageAdapter for KimiUsageAdapter {
    fn adapter_id(&self) -> &str {
        KIMI
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let (api_key, region) = match self.auth.get(account).await {
            Ok(Some(material)) => (
                material
                    .bearer_token
                    .map(|key| key.trim().to_owned())
                    .filter(|key| !key.is_empty()),
                KimiRegion::from_stored(material.secondary_bearer_token.as_deref()),
            ),
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => (None, KimiRegion::China),
            Err(error) => return Ok(invalid_payload("Kimi Code", error.to_string())),
        };
        let Some(api_key) = api_key else {
            return Ok(missing_auth("Kimi Code"));
        };

        let response =
            fetch_usage(self.transport.as_ref(), region, &api_key, self.deadline).await?;
        if !response.is_success() {
            return Ok(map_http_error(&response, "Kimi Code"));
        }
        let snapshot = match parse_usage(&response.body)
            .and_then(|usage| usage.snapshot(account, Utc::now()))
        {
            Ok(snapshot) => snapshot,
            Err(reason) => return Ok(invalid_payload("Kimi Code", reason)),
        };
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: None,
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

/// A `used_ratio` pool.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RatioPool {
    used_ratio: f64,
    reset_at: Option<DateTime<Utc>>,
}

impl RatioPool {
    fn parse(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        let used_ratio = number(value.get("used_ratio")).filter(|ratio| *ratio >= 0.0)?;
        Some(Self {
            used_ratio,
            reset_at: json_string(value, &["reset_time"])
                .as_deref()
                .and_then(parse_timestamp),
        })
    }

    fn used_percent(&self) -> f64 {
        self.used_ratio.min(1.0) * 100.0
    }
}

/// A count-based quota: `limit` with `used` or `remaining`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Counts {
    used: f64,
    limit: f64,
    /// False when neither `used` nor a valid `remaining` was given: the
    /// quota still shows (at 0%), but not as a timed window.
    reliable: bool,
    reset_at: Option<DateTime<Utc>>,
}

impl Counts {
    fn parse(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        let limit = number(value.get("limit")).filter(|limit| *limit > 0.0)?;
        let reset_at = json_string(value, &["resetTime", "resetAt", "reset_time", "reset_at"])
            .as_deref()
            .and_then(parse_timestamp);
        // `used` is authoritative and may exceed the limit during overage.
        if let Some(used) = number(value.get("used")).filter(|used| *used >= 0.0) {
            return Some(Self {
                used,
                limit,
                reliable: true,
                reset_at,
            });
        }
        if let Some(remaining) =
            number(value.get("remaining")).filter(|remaining| (0.0..=limit).contains(remaining))
        {
            return Some(Self {
                used: limit - remaining,
                limit,
                reliable: true,
                reset_at,
            });
        }
        Some(Self {
            used: 0.0,
            limit,
            reliable: false,
            reset_at,
        })
    }

    fn used_percent(&self) -> f64 {
        (self.used / self.limit * 100.0).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct KimiUsage {
    five_hour_pool: Option<RatioPool>,
    weekly_pool: Option<RatioPool>,
    monthly_pool: Option<RatioPool>,
    weekly_counts: Option<Counts>,
    rate_limit_counts: Option<Counts>,
    /// Length of the `limits[0]` window, when its unit is known.
    rate_limit_seconds: Option<i64>,
    plan: Option<String>,
}

fn parse_usage(body: &str) -> Result<KimiUsage, String> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| format!("usage JSON could not be parsed: {error}"))?;
    if !root.is_object() {
        return Err("usage response is not an object".to_owned());
    }
    let pools = root.get("usages");
    let rate_limit = root
        .get("limits")
        .and_then(Value::as_array)
        .and_then(|limits| limits.first());
    Ok(KimiUsage {
        five_hour_pool: RatioPool::parse(pools.and_then(|pools| pools.get("limit_5h"))),
        weekly_pool: RatioPool::parse(pools.and_then(|pools| pools.get("limit_7d"))),
        monthly_pool: RatioPool::parse(pools.and_then(|pools| pools.get("limit_month_total"))),
        weekly_counts: Counts::parse(root.get("usage")),
        rate_limit_counts: Counts::parse(rate_limit.and_then(|limit| limit.get("detail"))),
        rate_limit_seconds: rate_limit
            .and_then(|limit| limit.get("window"))
            .and_then(window_seconds),
        plan: plan_name(&root),
    })
}

/// `{"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"}` in seconds.
fn window_seconds(window: &Value) -> Option<i64> {
    let duration = window
        .get("duration")
        .and_then(Value::as_i64)
        .filter(|duration| *duration > 0)?;
    let unit = match json_string(window, &["timeUnit"])?.as_str() {
        "TIME_UNIT_MINUTE" => 60,
        "TIME_UNIT_HOUR" => 60 * 60,
        "TIME_UNIT_DAY" => 24 * 60 * 60,
        _ => return None,
    };
    duration.checked_mul(unit)
}

/// The membership level, by the names of Kimi's V1 membership catalog.
fn plan_name(root: &Value) -> Option<String> {
    let level = root
        .get("user")
        .and_then(|user| user.get("membership"))
        .and_then(|membership| json_string(membership, &["level"]))
        .map(|level| level.trim().to_owned())
        .filter(|level| !level.is_empty() && level != "LEVEL_UNSPECIFIED")?;
    let catalog = match root.get("version") {
        None | Some(Value::Null) => true,
        Some(Value::String(version)) => version == "GOODS_VERSION_V1",
        Some(_) => false,
    };
    if !catalog {
        return Some(level);
    }
    Some(
        match level.as_str() {
            "LEVEL_FREE" => "Adagio",
            "LEVEL_TRIAL" => "Andante",
            "LEVEL_BASIC" => "Moderato",
            "LEVEL_INTERMEDIATE" => "Allegretto",
            "LEVEL_ADVANCED" => "Allegro",
            other => other,
        }
        .to_owned(),
    )
}

impl KimiUsage {
    /// A ratio pool, unless it is a zero placeholder sitting next to real
    /// counts for the same window (older mixed responses send both; their
    /// reset clocks differ by about a second and a half).
    fn ratio_window(
        &self,
        pool: Option<RatioPool>,
        counts: Option<Counts>,
        seconds: i64,
        counts_seconds: Option<i64>,
    ) -> Option<RatioPool> {
        let pool = pool?;
        let placeholder = pool.used_ratio == 0.0
            && self.monthly_pool.is_none()
            && self.weekly_counts.is_some_and(|weekly| weekly.reliable)
            && counts_seconds == Some(seconds)
            && counts.is_some_and(|counts| {
                counts.reliable
                    && counts.used > 0.0
                    && match (counts.reset_at, pool.reset_at) {
                        (Some(count_reset), Some(pool_reset)) => {
                            (count_reset - pool_reset).num_milliseconds().abs() <= 2_000
                        }
                        _ => false,
                    }
            });
        (!placeholder).then_some(pool)
    }

    fn snapshot(
        &self,
        account: &AccountRecord,
        now: DateTime<Utc>,
    ) -> Result<UsageSnapshot, String> {
        let window = |kind, name: &str, used_percent: f64, reset_at, seconds| RateLimitWindow {
            kind,
            name: name.to_owned(),
            used_percent,
            reset_at_utc: reset_at,
            limit_window_seconds: seconds,
        };
        let from_pool = |kind, name: &str, pool: RatioPool, seconds| {
            window(kind, name, pool.used_percent(), pool.reset_at, seconds)
        };
        let from_counts = |kind, name: &str, counts: Counts, seconds: i64| {
            window(
                kind,
                name,
                counts.used_percent(),
                counts.reset_at,
                if counts.reliable { seconds } else { 0 },
            )
        };

        let rate_limit_seconds = self.rate_limit_seconds.unwrap_or(FIVE_HOURS);
        let five_hour = match self.ratio_window(
            self.five_hour_pool,
            self.rate_limit_counts,
            FIVE_HOURS,
            Some(rate_limit_seconds),
        ) {
            Some(pool) => Some(from_pool(
                UsageWindowKind::Primary,
                FIVE_HOUR_WINDOW_NAME,
                pool,
                FIVE_HOURS,
            )),
            None => self.rate_limit_counts.map(|counts| {
                from_counts(
                    UsageWindowKind::Primary,
                    FIVE_HOUR_WINDOW_NAME,
                    counts,
                    rate_limit_seconds,
                )
            }),
        };
        let weekly = match self.ratio_window(self.weekly_pool, self.weekly_counts, WEEK, Some(WEEK))
        {
            Some(pool) => Some(from_pool(
                UsageWindowKind::Secondary,
                WEEKLY_WINDOW_NAME,
                pool,
                WEEK,
            )),
            None => self.weekly_counts.map(|counts| {
                from_counts(UsageWindowKind::Secondary, WEEKLY_WINDOW_NAME, counts, WEEK)
            }),
        };
        let monthly = self.monthly_pool.map(|pool| AdditionalRateLimitWindow {
            key: "monthly_total".to_owned(),
            name: MONTHLY_WINDOW_NAME.to_owned(),
            window: from_pool(UsageWindowKind::Additional, MONTHLY_WINDOW_NAME, pool, 0),
        });

        // The short window leads, as on the other subscription cards.
        let (primary, secondary, primary_kind) = match (five_hour, weekly) {
            (Some(five_hour), weekly) => (Some(five_hour), weekly, UsagePrimaryWindowKind::Session),
            (None, Some(mut weekly)) => {
                weekly.kind = UsageWindowKind::Primary;
                (Some(weekly), None, UsagePrimaryWindowKind::Weekly)
            }
            (None, None) => (None, None, UsagePrimaryWindowKind::Other),
        };
        if primary.is_none() && monthly.is_none() {
            return Err("usage response has no quota window".to_owned());
        }

        Ok(UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: None,
            plan_type: self.plan.clone(),
            primary_window_kind: primary.as_ref().map(|_| primary_kind),
            primary,
            primary_window_is_synthetic: false,
            secondary,
            additional_windows: monthly.into_iter().collect(),
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics: Vec::new(),
            source_diagnostics: Vec::new(),
            provider_id: KIMI.to_owned(),
            source: Some("api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        })
    }
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// A number given as a JSON number or a numeric string.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|value: &f64| value.is_finite())
}

#[cfg(test)]
mod tests;
