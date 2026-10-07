//! MiniMax Coding (Token) Plan quotas through a Coding Plan API key.
//!
//! Following CodexBar, a Coding Plan key (`sk-cp-...`) reads
//! `GET /v1/token_plan/remains`, falling back to the older
//! `GET /v1/api/openplatform/coding_plan/remains`, on its region's API host:
//! `api.minimax.io` (Global) or `api.minimaxi.com` (China mainland). The
//! region that accepts the key is found when the account is added and kept
//! as the account's secondary token.
//!
//! `model_remains` lists one entry per model family. Each has a short
//! interval quota and, for text generation, a weekly quota. The
//! `*_usage_count` fields are the REMAINING counts, not the used ones; Token
//! Plan responses report `*_remaining_percent` instead and leave the counts
//! at zero. Lanes a plan does not include arrive as status 3 with nothing
//! remaining and are not drawn.

use crate::{
    accounts::{AccountRecord, MINIMAX, VerifiedIdentity},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{invalid_payload, map_http_error, missing_auth},
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, RateLimitWindow, UsageAdapter, UsageAdapterError,
        UsageAdapterErrorCode, UsageMetric, UsagePrimaryWindowKind, UsageProbeResult,
        UsageSnapshot, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use url::Url;

const USER_AGENT: &str = "UsageMonitor/0.1";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(8);
const TOKEN_PLAN_PATH: &str = "v1/token_plan/remains";
const CODING_PLAN_PATH: &str = "v1/api/openplatform/coding_plan/remains";

/// The MiniMax platform a key was created on; each has its own API host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiniMaxRegion {
    Global,
    ChinaMainland,
}

impl MiniMaxRegion {
    pub const CHINA_MAINLAND: &'static str = "cn";

    fn url(self, path: &str) -> Url {
        let host = match self {
            Self::Global => "https://api.minimax.io/",
            Self::ChinaMainland => "https://api.minimaxi.com/",
        };
        Url::parse(host)
            .and_then(|base| base.join(path))
            .expect("static MiniMax URL")
    }

    /// The region stored with an account; anything else is Global.
    pub fn from_stored(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(Self::CHINA_MAINLAND) => Self::ChinaMainland,
            _ => Self::Global,
        }
    }

    /// The value to store with an account, `None` for the default.
    pub fn stored(self) -> Option<&'static str> {
        match self {
            Self::Global => None,
            Self::ChinaMainland => Some(Self::CHINA_MAINLAND),
        }
    }
}

/// Why one remains request did not produce usage.
#[derive(Debug)]
enum Failure {
    Rejected(String),
    Http(UsageHttpResponse),
    Payload(String),
}

impl Failure {
    /// The older endpoint is worth a try after anything but a transport
    /// error; a key the Token Plan endpoint rejects can still be a legacy
    /// Coding Plan key.
    fn into_result(self) -> UsageProbeResult {
        match self {
            Self::Rejected(message) => UsageProbeResult::failure(UsageAdapterError {
                code: UsageAdapterErrorCode::Unauthorized,
                message,
                http_status_code: None,
                retry_after_seconds: None,
            }),
            Self::Http(response) => map_http_error(&response, "MiniMax"),
            Self::Payload(reason) => invalid_payload("MiniMax", reason),
        }
    }
}

async fn fetch_remains(
    transport: &dyn UsageHttpTransport,
    region: MiniMaxRegion,
    api_key: &str,
    deadline: Duration,
) -> Result<Result<Remains, Failure>, TransportError> {
    let mut failure = None;
    for path in [TOKEN_PLAN_PATH, CODING_PLAN_PATH] {
        let response = tokio::time::timeout(
            deadline,
            transport.send(UsageHttpRequest {
                method: Method::GET,
                url: region.url(path),
                headers: BTreeMap::from([
                    ("Authorization".to_owned(), format!("Bearer {api_key}")),
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    ("MM-API-Source".to_owned(), "UsageMonitor".to_owned()),
                    ("User-Agent".to_owned(), USER_AGENT.to_owned()),
                ]),
                body: None,
            }),
        )
        .await
        .map_err(|_| TransportError::Timeout("minimax remains".to_owned()))??;
        let attempt = if matches!(response.status_code, 401 | 403) {
            Err(Failure::Rejected(
                "MiniMax rejected the Coding Plan API key".to_owned(),
            ))
        } else if !response.is_success() {
            Err(Failure::Http(response))
        } else {
            parse_remains(&response.body)
        };
        match attempt {
            Ok(remains) => return Ok(Ok(remains)),
            // A rejection on the Token Plan endpoint wins over whatever the
            // older endpoint says, as in CodexBar.
            Err(error) => {
                if !matches!(failure, Some(Failure::Rejected(_))) {
                    failure = Some(error);
                }
            }
        }
    }
    Ok(Err(failure.expect("both endpoints were tried")))
}

/// Finds the region whose host accepts the key and answers with quotas.
pub async fn detect_region(
    transport: &dyn UsageHttpTransport,
    api_key: &str,
) -> Result<MiniMaxRegion, String> {
    for region in [MiniMaxRegion::Global, MiniMaxRegion::ChinaMainland] {
        match fetch_remains(transport, region, api_key, DEFAULT_DEADLINE).await {
            Ok(Ok(_)) => return Ok(region),
            Ok(Err(_)) => {}
            Err(error) => return Err(format!("Could not reach MiniMax: {error}")),
        }
    }
    Err("Neither api.minimax.io nor api.minimaxi.com accepted this Coding Plan API key".to_owned())
}

pub struct MiniMaxUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    deadline: Duration,
}

impl MiniMaxUsageAdapter {
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
impl UsageAdapter for MiniMaxUsageAdapter {
    fn adapter_id(&self) -> &str {
        MINIMAX
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let (api_key, region) = match self.auth.get(account).await {
            Ok(Some(material)) => (
                material
                    .bearer_token
                    .map(|key| key.trim().to_owned())
                    .filter(|key| !key.is_empty()),
                MiniMaxRegion::from_stored(material.secondary_bearer_token.as_deref()),
            ),
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => (None, MiniMaxRegion::Global),
            Err(error) => return Ok(invalid_payload("MiniMax", error.to_string())),
        };
        let Some(api_key) = api_key else {
            return Ok(missing_auth("MiniMax"));
        };
        let remains =
            match fetch_remains(self.transport.as_ref(), region, &api_key, self.deadline).await? {
                Ok(remains) => remains,
                Err(failure) => return Ok(failure.into_result()),
            };
        let snapshot = remains.snapshot(account, Utc::now());
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: None,
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

/// One `model_remains` entry.
#[derive(Debug, Clone, PartialEq)]
struct ModelRemains {
    model_name: Option<String>,
    interval: Lane,
    weekly: Lane,
}

/// One quota of a model family: counts are remaining, not used.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Lane {
    total: Option<i64>,
    remaining: Option<i64>,
    remaining_percent: Option<f64>,
    status: Option<i64>,
    start: Option<i64>,
    end: Option<i64>,
    remains_time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
struct Remains {
    plan: Option<String>,
    models: Vec<ModelRemains>,
    points_balance: Option<f64>,
}

fn parse_remains(body: &str) -> Result<Remains, Failure> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| Failure::Payload(format!("remains JSON could not be parsed: {error}")))?;
    let data = root
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(&root);
    let status = data
        .get("base_resp")
        .or_else(|| root.get("base_resp"))
        .filter(|status| status.is_object());
    if let Some(status) = status {
        let code = integer(status.get("status_code")).unwrap_or(0);
        if code != 0 {
            let message = status
                .get("status_msg")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .map_or_else(|| format!("status_code {code}"), str::to_owned);
            let lower = message.to_ascii_lowercase();
            return Err(
                if code == 1004
                    || lower.contains("cookie")
                    || lower.contains("log in")
                    || lower.contains("login")
                    || lower == "invalid api key"
                {
                    Failure::Rejected(format!("MiniMax rejected the key: {message}"))
                } else {
                    Failure::Payload(message)
                },
            );
        }
    }
    let models = data
        .get("model_remains")
        .and_then(Value::as_array)
        .filter(|models| !models.is_empty())
        .ok_or_else(|| Failure::Payload("remains response has no model_remains".to_owned()))?
        .iter()
        .map(|item| ModelRemains {
            model_name: item
                .get("model_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            interval: Lane {
                total: integer(item.get("current_interval_total_count")),
                remaining: integer(item.get("current_interval_usage_count")),
                remaining_percent: number(item.get("current_interval_remaining_percent")),
                status: integer(item.get("current_interval_status")),
                start: integer(item.get("start_time")),
                end: integer(item.get("end_time")),
                remains_time: integer(item.get("remains_time")),
            },
            weekly: Lane {
                total: integer(item.get("current_weekly_total_count")),
                remaining: integer(item.get("current_weekly_usage_count")),
                remaining_percent: number(item.get("current_weekly_remaining_percent")),
                status: integer(item.get("current_weekly_status")),
                start: integer(item.get("weekly_start_time")),
                end: integer(item.get("weekly_end_time")),
                remains_time: integer(item.get("weekly_remains_time")),
            },
        })
        .collect::<Vec<_>>();
    let title = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
    };
    let plan = [
        "current_subscribe_title",
        "plan_name",
        "combo_title",
        "current_plan_title",
    ]
    .into_iter()
    .find_map(|key| title(data.get(key)))
    .or_else(|| title(data.pointer("/current_combo_card/title")))
    .or_else(|| inferred_plan(&models));
    let points_balance = [
        "points_balance",
        "point_balance",
        "credits_balance",
        "credit_balance",
        "balance",
    ]
    .into_iter()
    .find_map(|key| number(data.get(key)))
    .filter(|balance| *balance >= 0.0);
    Ok(Remains {
        plan,
        models,
        points_balance,
    })
}

fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.fract() == 0.0)
                .map(|value| value as i64)
        }),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|value: &f64| value.is_finite())
}

fn is_text_generation(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower == "general" || lower.contains("minimax-m") || lower.starts_with("m2.")
}

/// CodexBar's family names for the models MiniMax lists.
fn service_name(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    if lower == "general" {
        "General".to_owned()
    } else if lower == "video" {
        "Video".to_owned()
    } else if is_text_generation(model) {
        "Text Generation".to_owned()
    } else if lower.contains("speech") {
        "Text to Speech".to_owned()
    } else if lower.contains("hailuo") && lower.contains("fast") {
        "Image to Video".to_owned()
    } else if lower.contains("hailuo") {
        "Text to Video".to_owned()
    } else if lower.starts_with("image-") {
        "Image Generation".to_owned()
    } else if lower.contains("music") {
        "Music Generation".to_owned()
    } else {
        model.to_owned()
    }
}

/// A Token Plan lane that exists in the schema but is not part of the
/// subscription: status 3, nothing counted, everything "remaining".
fn is_unavailable_placeholder(lane: &Lane) -> bool {
    lane.status == Some(3)
        && lane.total.unwrap_or(0) == 0
        && lane.remaining.unwrap_or(0) == 0
        && lane
            .remaining_percent
            .is_some_and(|percent| percent >= 100.0)
}

/// Plus plans list a video lane they do not include.
fn inferred_plan(models: &[ModelRemains]) -> Option<String> {
    let text = models
        .iter()
        .any(|model| model.model_name.as_deref().is_some_and(is_text_generation));
    let unavailable_video = models.iter().any(|model| {
        model
            .model_name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("video"))
            && is_unavailable_placeholder(&model.interval)
    });
    (text && unavailable_video).then(|| "Plus".to_owned())
}

/// Epoch seconds or milliseconds; anything earlier than 2001 is not a time.
fn epoch(value: Option<i64>) -> Option<DateTime<Utc>> {
    let raw = value?;
    if raw > 1_000_000_000_000 {
        DateTime::from_timestamp_millis(raw)
    } else if raw > 1_000_000_000 {
        DateTime::from_timestamp(raw, 0)
    } else {
        None
    }
}

/// A drawable quota: its used share, reset, and length.
struct Quota {
    service: String,
    text_lane: bool,
    weekly: bool,
    used_percent: f64,
    reset_at: Option<DateTime<Utc>>,
    seconds: i64,
}

impl Quota {
    fn from_lane(model: &str, lane: &Lane, weekly: bool, now: DateTime<Utc>) -> Option<Self> {
        if is_unavailable_placeholder(lane) {
            return None;
        }
        // A weekly text lane at status 3 with everything remaining is the
        // plan's unlimited weekly allowance: nothing to draw.
        if weekly
            && lane.status == Some(3)
            && lane
                .remaining_percent
                .is_some_and(|percent| percent >= 100.0)
        {
            return None;
        }
        let used_percent = if let Some(remaining) = lane.remaining_percent {
            100.0 - remaining
        } else {
            let total = lane.total.filter(|total| *total > 0)?;
            let remaining = lane.remaining?;
            (total - remaining).max(0) as f64 / total as f64 * 100.0
        }
        .clamp(0.0, 100.0);
        let start = epoch(lane.start);
        let end = epoch(lane.end);
        let reset_at = end.filter(|end| *end > now).or_else(|| {
            let remains = lane.remains_time.filter(|remains| *remains > 0)?;
            let seconds = if remains > 1_000_000 {
                remains / 1000
            } else {
                remains
            };
            Some(now + ChronoDuration::seconds(seconds))
        });
        let seconds = match (start, end) {
            (Some(start), Some(end)) if end > start => (end - start).num_seconds(),
            _ => 0,
        };
        Some(Self {
            service: service_name(model),
            text_lane: is_text_generation(model),
            weekly,
            used_percent,
            reset_at,
            seconds,
        })
    }

    fn window_name(&self) -> String {
        if self.weekly {
            return "Weekly".to_owned();
        }
        let hours = self.seconds as f64 / 3600.0;
        if (23.0..=25.0).contains(&hours) {
            "Daily".to_owned()
        } else if (4.0..=6.0).contains(&hours) {
            "5 hours".to_owned()
        } else if (1.0..23.0).contains(&hours) {
            format!("{} hours", hours as i64)
        } else {
            "Quota".to_owned()
        }
    }

    /// Text lanes are the plan's own quota and need no service prefix.
    fn name(&self) -> String {
        if self.text_lane {
            self.window_name()
        } else {
            format!("{} · {}", self.service, self.window_name())
        }
    }

    fn window(&self, kind: UsageWindowKind) -> RateLimitWindow {
        RateLimitWindow {
            kind,
            name: self.name(),
            used_percent: self.used_percent,
            reset_at_utc: self.reset_at,
            limit_window_seconds: self.seconds,
        }
    }
}

impl Remains {
    fn quotas(&self, now: DateTime<Utc>) -> Vec<Quota> {
        let mut quotas = Vec::new();
        for model in &self.models {
            let Some(name) = model.model_name.as_deref() else {
                continue;
            };
            quotas.extend(Quota::from_lane(name, &model.interval, false, now));
            // The weekly counts mean something only for text generation.
            if is_text_generation(name) {
                quotas.extend(Quota::from_lane(name, &model.weekly, true, now));
            }
        }
        if quotas.is_empty() {
            // Older responses carry one unnamed entry for the plan itself.
            if let Some(first) = self.models.first() {
                let mut quota = Quota::from_lane("general", &first.interval, false, now);
                if let Some(quota) = quota.as_mut() {
                    quota.text_lane = true;
                }
                quotas.extend(quota);
            }
        }
        // Text lanes first, the short window before the weekly one; a stable
        // sort keeps MiniMax's order otherwise.
        quotas.sort_by_key(|quota| (!quota.text_lane, quota.weekly));
        quotas
    }

    fn snapshot(&self, account: &AccountRecord, now: DateTime<Utc>) -> UsageSnapshot {
        let quotas = self.quotas(now);
        let mut lanes = quotas.iter();
        let primary_quota = lanes.next();
        let primary = primary_quota.map(|quota| quota.window(UsageWindowKind::Primary));
        let secondary = lanes
            .next()
            .map(|quota| quota.window(UsageWindowKind::Secondary));
        let additional_windows = lanes
            .map(|quota| AdditionalRateLimitWindow {
                key: format!(
                    "{}.{}",
                    quota.service.to_ascii_lowercase().replace(' ', "_"),
                    if quota.weekly { "weekly" } else { "interval" }
                ),
                name: quota.name(),
                window: quota.window(UsageWindowKind::Additional),
            })
            .collect();
        let metrics = self
            .points_balance
            .map(|balance| UsageMetric {
                key: "balance.points".to_owned(),
                name: "Points balance".to_owned(),
                used_percent: None,
                used_amount: None,
                limit_amount: None,
                remaining_amount: Some(balance),
                unit: Some("points".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata: HashMap::new(),
            })
            .into_iter()
            .collect();

        UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: None,
            plan_type: self.plan.clone(),
            primary_window_kind: primary_quota.map(|quota| {
                if quota.weekly {
                    UsagePrimaryWindowKind::Weekly
                } else if quota.window_name() == "5 hours" {
                    UsagePrimaryWindowKind::Session
                } else {
                    UsagePrimaryWindowKind::Other
                }
            }),
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
            provider_id: MINIMAX.to_owned(),
            source: Some("api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests;
