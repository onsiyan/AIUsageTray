//! z.ai / GLM Coding Plan quotas through the quota monitor endpoint.
//!
//! An API key reads `GET /api/monitor/usage/quota/limit` on `api.z.ai`
//! (global) or `open.bigmodel.cn` (BigModel, China mainland). Keys belong to
//! one region; the region found when the account was added is stored with
//! it as the secondary token (`bigmodel-cn`), so refreshes go straight there.
//!
//! Parsing rules: `TOKENS_LIMIT` and `CREDIT_LIMIT`
//! entries are Coding Plan windows, the shortest first and the longest
//! second; `TIME_LIMIT` is the MCP quota. An integer `percentage` is
//! required, counts refine it when present, and a five-hour reset further
//! away than five hours is dropped rather than guessed at.

use crate::{
    accounts::{AccountRecord, VerifiedIdentity, ZAI},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{invalid_payload, json_string, map_http_error, missing_auth},
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, RateLimitWindow, UsageAdapter, UsageMetric,
        UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, TimeZone, Timelike, Utc, Weekday};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use url::Url;

const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const USER_AGENT: &str = "UsageMonitor/0.1";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(8);
/// Clock skew allowed on a five-hour reset.
const SKEW_SECONDS: i64 = 60;

pub const MCP_WINDOW_NAME: &str = "MCP";

/// Where a key is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZaiRegion {
    Global,
    BigModelChina,
}

impl ZaiRegion {
    pub const BIGMODEL_CN: &'static str = "bigmodel-cn";

    fn base_url(self) -> &'static str {
        match self {
            Self::Global => "https://api.z.ai",
            Self::BigModelChina => "https://open.bigmodel.cn",
        }
    }

    /// The region stored with an account; anything else is global.
    pub fn from_stored(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(Self::BIGMODEL_CN) => Self::BigModelChina,
            _ => Self::Global,
        }
    }

    /// The value to store with an account, `None` for the default.
    pub fn stored(self) -> Option<&'static str> {
        match self {
            Self::Global => None,
            Self::BigModelChina => Some(Self::BIGMODEL_CN),
        }
    }
}

pub struct ZaiUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    deadline: Duration,
}

impl ZaiUsageAdapter {
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

async fn fetch_quota(
    transport: &dyn UsageHttpTransport,
    region: ZaiRegion,
    api_key: &str,
    deadline: Duration,
) -> Result<UsageHttpResponse, TransportError> {
    let url = Url::parse(&format!("{}{QUOTA_PATH}", region.base_url()))
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    tokio::time::timeout(
        deadline,
        transport.send(UsageHttpRequest {
            method: Method::GET,
            url,
            headers: BTreeMap::from([
                ("Authorization".to_owned(), format!("Bearer {api_key}")),
                ("Accept".to_owned(), "application/json".to_owned()),
                ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ]),
            body: None,
        }),
    )
    .await
    .map_err(|_| TransportError::Timeout("z.ai quota".to_owned()))?
}

/// The region that accepts `api_key`: global first, then BigModel.
pub async fn detect_region(
    transport: &dyn UsageHttpTransport,
    api_key: &str,
) -> Result<ZaiRegion, String> {
    let mut last_status = None;
    for region in [ZaiRegion::Global, ZaiRegion::BigModelChina] {
        let response = fetch_quota(transport, region, api_key, DEFAULT_DEADLINE)
            .await
            .map_err(|error| format!("Could not reach z.ai: {error}"))?;
        // Both hosts may answer HTTP 200 with a failure in the body.
        if response.is_success()
            && serde_json::from_str::<Value>(&response.body)
                .is_ok_and(|root| root.get("success").and_then(Value::as_bool) == Some(true))
        {
            return Ok(region);
        }
        last_status = Some(response.status_code);
    }
    Err(format!(
        "Neither api.z.ai nor open.bigmodel.cn accepted this API key (HTTP {})",
        last_status.unwrap_or_default()
    ))
}

#[async_trait]
impl UsageAdapter for ZaiUsageAdapter {
    fn adapter_id(&self) -> &str {
        ZAI
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = match self.auth.get(account).await {
            Ok(material) => material,
            Err(AuthError::ReauthenticationRequired(_)) => None,
            Err(error) => return Ok(invalid_payload("z.ai", error.to_string())),
        };
        let api_key = material
            .as_ref()
            .and_then(|material| material.bearer_token.as_deref())
            .map(str::trim)
            .filter(|key| !key.is_empty());
        let Some(api_key) = api_key else {
            return Ok(missing_auth("z.ai"));
        };
        let region = ZaiRegion::from_stored(
            material
                .as_ref()
                .and_then(|material| material.secondary_bearer_token.as_deref()),
        );

        let response = fetch_quota(self.transport.as_ref(), region, api_key, self.deadline).await?;
        if !response.is_success() {
            return Ok(map_http_error(&response, "z.ai"));
        }
        let now = Utc::now();
        let snapshot = match parse_quota(&response.body, now) {
            Ok(quota) => quota.snapshot(account, now),
            Err(reason) => return Ok(invalid_payload("z.ai", reason)),
        };
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: None,
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitKind {
    Tokens,
    Credits,
    Time,
}

#[derive(Debug, Clone, PartialEq)]
struct Limit {
    kind: LimitKind,
    unit: i64,
    number: i64,
    used_percent: f64,
    window_minutes: Option<i64>,
    reset_at: Option<DateTime<Utc>>,
}

impl Limit {
    /// One `data.limits[]` entry; `Ok(None)` for a limit type not shown.
    fn parse(raw: &Value) -> Result<Option<Self>, String> {
        // Every entry must be well-formed, even of a type not shown.
        let malformed = || "a quota limit entry is malformed".to_owned();
        let kind = raw
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let unit = integer(raw.get("unit")).ok_or_else(malformed)?;
        let number = integer(raw.get("number")).ok_or_else(malformed)?;
        let mut percent = integer(raw.get("percentage")).ok_or_else(malformed)? as f64;
        let kind = match kind {
            "TOKENS_LIMIT" => LimitKind::Tokens,
            "CREDIT_LIMIT" => LimitKind::Credits,
            "TIME_LIMIT" => LimitKind::Time,
            _ => return Ok(None),
        };
        let optional = |name: &str| match raw.get(name) {
            None | Some(Value::Null) => Ok(None),
            value => integer(value)
                .map(Some)
                .ok_or_else(|| format!("limit {name} must be an integer")),
        };
        let usage = optional("usage")?;
        let current = optional("currentValue")?;
        let remaining = optional("remaining")?;
        if let Some(usage) = usage.filter(|usage| *usage > 0) {
            let used = match (remaining, current) {
                (Some(remaining), current) => {
                    Some((usage - remaining).max(current.unwrap_or(usage - remaining)))
                }
                (None, Some(current)) => Some(current),
                (None, None) => None,
            };
            if let Some(used) = used {
                percent = used.clamp(0, usage) as f64 / usage as f64 * 100.0;
            }
        }
        let multiplier = match unit {
            1 => Some(24 * 60),
            3 => Some(60),
            5 => Some(1),
            6 => Some(7 * 24 * 60),
            _ => None,
        };
        let window_minutes = multiplier
            .filter(|_| number > 0)
            .map(|minutes| minutes * number);
        Ok(Some(Self {
            kind,
            unit,
            number,
            used_percent: percent.clamp(0.0, 100.0),
            window_minutes,
            reset_at: optional("nextResetTime")?.and_then(DateTime::from_timestamp_millis),
        }))
    }

    fn window(&self, kind: UsageWindowKind, now: DateTime<Utc>) -> RateLimitWindow {
        let minutes = if self.kind == LimitKind::Time && self.unit == 5 && self.number == 1 {
            // z.ai marks the monthly MCP quota as "1 minute".
            Some(30 * 24 * 60)
        } else {
            self.window_minutes
        };
        // A five-hour Coding Plan reset cannot be ten hours away; never guess
        // a timezone correction.
        let five_hour = self.kind != LimitKind::Time && self.window_minutes == Some(300);
        let reset_at = self.reset_at.filter(|reset| {
            !five_hour || (*reset - now).num_seconds() <= 5 * 60 * 60 + SKEW_SECONDS
        });
        RateLimitWindow {
            kind,
            name: self.name(),
            used_percent: self.used_percent,
            reset_at_utc: reset_at,
            limit_window_seconds: minutes.map_or(0, |minutes| minutes * 60),
        }
    }

    fn name(&self) -> String {
        if self.kind == LimitKind::Time {
            return MCP_WINDOW_NAME.to_owned();
        }
        match self.window_minutes {
            Some(300) => "5 hours".to_owned(),
            Some(1440) => "Daily".to_owned(),
            Some(10080) => "Weekly".to_owned(),
            _ => {
                let unit = match self.unit {
                    1 => "day",
                    3 => "hour",
                    5 => "minute",
                    6 => "week",
                    _ => return "Quota".to_owned(),
                };
                let plural = if self.number == 1 { "" } else { "s" };
                format!("{} {unit}{plural}", self.number)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ZaiQuota {
    plan_limits: Vec<Limit>,
    mcp: Option<Limit>,
    plan: Option<String>,
    now: DateTime<Utc>,
}

fn parse_quota(body: &str, now: DateTime<Utc>) -> Result<ZaiQuota, String> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| format!("quota JSON could not be parsed: {error}"))?;
    let ok = root.get("success").and_then(Value::as_bool) == Some(true)
        && root.get("code").and_then(Value::as_i64) == Some(200);
    if !ok {
        return Err(json_string(&root, &["msg", "message"])
            .unwrap_or_else(|| "the quota request was not successful".to_owned()));
    }
    let data = root
        .get("data")
        .filter(|data| data.is_object())
        .ok_or("quota data is missing")?;
    let limits = data
        .get("limits")
        .and_then(Value::as_array)
        .ok_or("quota limits are missing")?;
    let mut parsed = Vec::new();
    for raw in limits {
        if let Some(limit) = Limit::parse(raw)? {
            parsed.push(limit);
        }
    }
    let mcp = parsed
        .iter()
        .rfind(|limit| limit.kind == LimitKind::Time)
        .cloned();
    let mut plan_limits = parsed
        .into_iter()
        .filter(|limit| limit.kind != LimitKind::Time)
        .collect::<Vec<_>>();
    plan_limits.sort_by_key(|limit| limit.window_minutes.unwrap_or(i64::MAX));
    let plan = ["planName", "plan", "plan_type", "packageName", "level"]
        .iter()
        .find_map(|name| data.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .map(str::to_owned);
    Ok(ZaiQuota {
        plan_limits,
        mcp,
        plan,
        now,
    })
}

impl ZaiQuota {
    fn snapshot(&self, account: &AccountRecord, now: DateTime<Utc>) -> UsageSnapshot {
        let shortest = self.plan_limits.first();
        let longest = (self.plan_limits.len() >= 2)
            .then(|| self.plan_limits.last())
            .flatten();
        let primary = shortest
            .map(|limit| limit.window(UsageWindowKind::Primary, now))
            .or_else(|| {
                self.mcp
                    .as_ref()
                    .map(|mcp| mcp.window(UsageWindowKind::Primary, now))
            });
        let secondary = longest.map(|limit| limit.window(UsageWindowKind::Secondary, now));
        // MCP is its own lane only next to a Coding Plan window.
        let additional_windows = match (shortest, &self.mcp) {
            (Some(_), Some(mcp)) => vec![AdditionalRateLimitWindow {
                key: "mcp".to_owned(),
                name: MCP_WINDOW_NAME.to_owned(),
                window: mcp.window(UsageWindowKind::Additional, now),
            }],
            _ => Vec::new(),
        };
        let primary_kind = primary
            .as_ref()
            .map(|window| match window.limit_window_seconds {
                18_000 => UsagePrimaryWindowKind::Session,
                604_800 => UsagePrimaryWindowKind::Weekly,
                _ => UsagePrimaryWindowKind::Other,
            });
        // Credit plans cost double at peak hours, so say which it is now.
        let metrics = self
            .plan_limits
            .iter()
            .any(|limit| limit.kind == LimitKind::Credits)
            .then(|| quota_rate(self.now))
            .into_iter()
            .collect();

        UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: None,
            plan_type: self.plan.clone(),
            primary_window_kind: primary_kind,
            primary,
            primary_window_is_synthetic: false,
            secondary,
            additional_windows,
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id: ZAI.to_owned(),
            source: Some("api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        }
    }
}

/// Peak is Monday to Friday 06:00-10:00 UTC; credit plans charge 1x then
/// and 0.5x off-peak. The note says which applies and when that changes.
fn quota_rate(now: DateTime<Utc>) -> UsageMetric {
    const PEAK_START: u32 = 6;
    const PEAK_END: u32 = 10;
    let weekday = !matches!(now.weekday(), Weekday::Sat | Weekday::Sun);
    let peak = weekday && (PEAK_START..PEAK_END).contains(&now.hour());
    let at_hour = |date: chrono::NaiveDate, hour: u32| {
        Utc.from_utc_datetime(&date.and_hms_opt(hour, 0, 0).expect("valid hour"))
    };
    let change = if peak {
        at_hour(now.date_naive(), PEAK_END)
    } else {
        let mut date = now.date_naive();
        if now.hour() >= PEAK_START {
            date += ChronoDuration::days(1);
        }
        while matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            date += ChronoDuration::days(1);
        }
        at_hour(date, PEAK_START)
    };
    let (key, name) = if peak {
        ("rate.peak", "Quota rate: peak")
    } else {
        ("rate.off_peak", "Quota rate: off-peak")
    };
    UsageMetric {
        key: key.to_owned(),
        name: name.to_owned(),
        used_percent: None,
        used_amount: None,
        limit_amount: None,
        remaining_amount: None,
        unit: None,
        reset_at_utc: Some(change),
        reset_label: None,
        metadata: HashMap::new(),
    }
}

/// An integer, as z.ai sends them; fractional or textual values are not.
fn integer(value: Option<&Value>) -> Option<i64> {
    value?.as_i64()
}

#[cfg(test)]
mod tests;
