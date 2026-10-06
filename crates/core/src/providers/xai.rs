//! xAI API prepaid credits and spend through the xAI Management API.
//!
//! Following CodexBar, an xAI Management API key (xAI Console, Settings >
//! Management Keys; inference keys are refused) and the team ID read:
//!
//! - `GET /v1/billing/teams/{team}/prepaid/balance`: `total.val` is the
//!   ledger in cents, negative while credit remains, so the balance is its
//!   negation;
//! - `POST /v1/billing/teams/{team}/usage`: daily USD spend over the last 30
//!   UTC days. History is optional: when it fails the balance still shows.
//!
//! The key is the account's bearer token and the team ID its secondary token.

use crate::{
    accounts::{AccountRecord, VerifiedIdentity, XAI},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{invalid_payload, map_http_error, missing_auth},
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        CreditsSnapshot, UsageAdapter, UsageAdapterError, UsageAdapterErrorCode, UsageMetric,
        UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot, UsageSourceDiagnostic,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use url::Url;

const BILLING_URL: &str = "https://management-api.x.ai/v1/billing/teams/";
const USER_AGENT: &str = "UsageMonitor/0.1";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);
const HISTORY_DAYS: i64 = 30;

const REJECTED_KEY: &str = "xAI rejected the Management API key. Create one in the xAI Console under Settings > Management Keys; inference API keys are not accepted.";

pub struct XaiUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    billing_url: Url,
    deadline: Duration,
}

impl XaiUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            billing_url: Url::parse(BILLING_URL)
                .map_err(|error| TransportError::InvalidUrl(error.to_string()))?,
            deadline: DEFAULT_DEADLINE,
        })
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    fn team_url(&self, team: &str, path: &[&str]) -> Url {
        let mut url = self.billing_url.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().push(team).extend(path);
        }
        url
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        api_key: &str,
        body: Option<String>,
    ) -> Result<UsageHttpResponse, TransportError> {
        let mut headers = BTreeMap::from([
            ("Authorization".to_owned(), format!("Bearer {api_key}")),
            ("Accept".to_owned(), "application/json".to_owned()),
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
        ]);
        if body.is_some() {
            headers.insert("Content-Type".to_owned(), "application/json".to_owned());
        }
        tokio::time::timeout(
            self.deadline,
            self.transport.send(UsageHttpRequest {
                method,
                url,
                headers,
                body,
            }),
        )
        .await
        .map_err(|_| TransportError::Timeout("xai billing".to_owned()))?
    }
}

/// A team ID is one path segment of the billing URL.
pub fn valid_team_id(team: &str) -> bool {
    !team.is_empty() && !team.contains('/') && team != "." && team != ".."
}

#[async_trait]
impl UsageAdapter for XaiUsageAdapter {
    fn adapter_id(&self) -> &str {
        XAI
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let trimmed = |value: Option<String>| {
            value
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let (api_key, team) = match self.auth.get(account).await {
            Ok(Some(material)) => (
                trimmed(material.bearer_token),
                trimmed(material.secondary_bearer_token),
            ),
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => (None, None),
            Err(error) => return Ok(invalid_payload("xAI", error.to_string())),
        };
        let Some(api_key) = api_key else {
            return Ok(missing_auth("xAI"));
        };
        let Some(team) = team.filter(|team| valid_team_id(team)) else {
            return Ok(UsageProbeResult::failure(UsageAdapterError {
                code: UsageAdapterErrorCode::AuthenticationUnavailable,
                message: "Missing or invalid xAI team ID".to_owned(),
                http_status_code: None,
                retry_after_seconds: None,
            }));
        };

        let response = self
            .send(
                Method::GET,
                self.team_url(&team, &["prepaid", "balance"]),
                &api_key,
                None,
            )
            .await?;
        if !response.is_success() {
            return Ok(balance_error(&response));
        }
        let balance = match parse_balance(&response.body) {
            Ok(balance) => balance,
            Err(reason) => return Ok(invalid_payload("xAI", reason)),
        };

        let now = Utc::now();
        let mut diagnostics = Vec::new();
        let history = match self
            .send(
                Method::POST,
                self.team_url(&team, &["usage"]),
                &api_key,
                Some(usage_request(now).to_string()),
            )
            .await
        {
            Ok(response) if matches!(response.status_code, 401 | 403) => {
                return Ok(rejected_key(response.status_code));
            }
            Ok(response) if response.is_success() => match parse_history(&response.body) {
                Ok(history) => Some(history),
                Err(reason) => {
                    diagnostics.push(history_diagnostic(
                        UsageAdapterErrorCode::InvalidPayload,
                        reason,
                        None,
                    ));
                    None
                }
            },
            Ok(response) => {
                diagnostics.push(history_diagnostic(
                    UsageAdapterErrorCode::HttpError,
                    format!("xAI usage history returned HTTP {}", response.status_code),
                    Some(response.status_code),
                ));
                None
            }
            Err(error) => {
                diagnostics.push(history_diagnostic(
                    UsageAdapterErrorCode::NetworkFailure,
                    error.to_string(),
                    None,
                ));
                None
            }
        };

        let mut snapshot = snapshot(account, balance, history.as_ref(), now);
        snapshot.source_diagnostics = diagnostics;
        // The Management API does not say who owns the key, so the account
        // keeps the label it was added with.
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: None,
            plan_type: None,
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

fn rejected_key(status: u16) -> UsageProbeResult {
    UsageProbeResult::failure(UsageAdapterError {
        code: UsageAdapterErrorCode::Unauthorized,
        message: REJECTED_KEY.to_owned(),
        http_status_code: Some(status),
        retry_after_seconds: None,
    })
}

fn balance_error(response: &UsageHttpResponse) -> UsageProbeResult {
    match response.status_code {
        401 | 403 => rejected_key(response.status_code),
        404 => UsageProbeResult::failure(UsageAdapterError {
            code: UsageAdapterErrorCode::HttpError,
            message: "xAI returned 404 for this team. Check the team ID, and that the Management key belongs to the same team.".to_owned(),
            http_status_code: Some(404),
            retry_after_seconds: None,
        }),
        _ => map_http_error(response, "xAI"),
    }
}

fn history_diagnostic(
    code: UsageAdapterErrorCode,
    message: String,
    http_status_code: Option<u16>,
) -> UsageSourceDiagnostic {
    UsageSourceDiagnostic {
        source: "xai.usage".to_owned(),
        code,
        message,
        http_status_code,
        retry_after_seconds: None,
    }
}

/// The remaining prepaid credit in USD: `total.val` is a cent amount, with
/// credit as a negative ledger value.
fn parse_balance(body: &str) -> Result<f64, String> {
    let root: Value =
        serde_json::from_str(body).map_err(|error| format!("invalid balance JSON: {error}"))?;
    let raw = root
        .pointer("/total/val")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| is_decimal(raw))
        .ok_or("balance total.val is not a cent amount")?;
    let cents: f64 = raw
        .parse()
        .map_err(|_| "balance total.val is not a cent amount")?;
    // `-0.0` would print as "-$0.00".
    Ok(if cents == 0.0 { 0.0 } else { -cents / 100.0 })
}

fn is_decimal(raw: &str) -> bool {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "0"));
    !whole.is_empty()
        && !fraction.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
}

/// The last 30 UTC days, today included, in daily USD sums.
fn usage_request(now: DateTime<Utc>) -> Value {
    let start = (now - ChronoDuration::days(HISTORY_DAYS - 1))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc();
    let timestamp = |time: DateTime<Utc>| time.format("%Y-%m-%d %H:%M:%S").to_string();
    json!({
        "analyticsRequest": {
            "timeRange": {
                "startTime": timestamp(start),
                "endTime": timestamp(now),
                "timezone": "Etc/GMT",
            },
            "timeUnit": "TIME_UNIT_DAY",
            "values": [{ "name": "usd", "aggregation": "AGGREGATION_SUM" }],
            "groupBy": [],
            "filters": [],
        }
    })
}

#[derive(Debug, Clone, PartialEq)]
struct SpendHistory {
    /// USD per UTC day, summed over every series.
    daily: BTreeMap<NaiveDate, f64>,
    /// xAI cut the history short; the total is a lower bound.
    partial: bool,
}

fn parse_history(body: &str) -> Result<SpendHistory, String> {
    let root: Value =
        serde_json::from_str(body).map_err(|error| format!("invalid usage JSON: {error}"))?;
    let series = root
        .get("timeSeries")
        .and_then(Value::as_array)
        .ok_or("usage history has no timeSeries")?;
    let mut daily = BTreeMap::new();
    for series in series {
        let points = series
            .get("dataPoints")
            .and_then(Value::as_array)
            .ok_or("usage series has no dataPoints")?;
        for point in points {
            let day = point
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .ok_or("usage point has no valid timestamp")?
                .with_timezone(&Utc)
                .date_naive();
            let value = point
                .get("values")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or("usage point has no valid USD value")?;
            *daily.entry(day).or_insert(0.0) += value;
        }
    }
    Ok(SpendHistory {
        daily,
        partial: root.get("limitReached").and_then(Value::as_bool) == Some(true),
    })
}

fn snapshot(
    account: &AccountRecord,
    balance: f64,
    history: Option<&SpendHistory>,
    now: DateTime<Utc>,
) -> UsageSnapshot {
    let amount = |key: &str, name: &str| UsageMetric {
        key: key.to_owned(),
        name: name.to_owned(),
        used_percent: None,
        used_amount: None,
        limit_amount: None,
        remaining_amount: None,
        unit: Some("USD".to_owned()),
        reset_at_utc: None,
        reset_label: None,
        metadata: HashMap::new(),
    };
    let mut metrics = vec![UsageMetric {
        remaining_amount: Some(balance),
        ..amount("balance", "Balance")
    }];
    if let Some(history) = history {
        let today = history.daily.get(&now.date_naive()).copied().unwrap_or(0.0);
        metrics.push(UsageMetric {
            used_amount: Some(today),
            ..amount("spend.today", "Today")
        });
        let (key, name) = if history.partial {
            ("spend.30d.partial", "Last 30 days (partial)")
        } else {
            ("spend.30d", "Last 30 days")
        };
        metrics.push(UsageMetric {
            used_amount: Some(history.daily.values().sum()),
            ..amount(key, name)
        });
    }

    UsageSnapshot {
        account_id: account.id,
        observed_at_utc: now,
        response_account_id: None,
        plan_type: None,
        primary: None,
        primary_window_kind: Some(UsagePrimaryWindowKind::Spend),
        primary_window_is_synthetic: false,
        secondary: None,
        additional_windows: Vec::new(),
        credits: Some(CreditsSnapshot {
            has_credits: Some(balance > 0.0),
            unlimited: Some(false),
            balance: Some(balance),
            currency_code: Some("USD".to_owned()),
            approximate_message_cost: None,
            limit: None,
            balance_read_succeeded: Some(true),
            credits_available: Some(balance > 0.0),
        }),
        credit_inventory: None,
        spend: None,
        observed_email: None,
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics,
        source_diagnostics: Vec::new(),
        provider_id: XAI.to_owned(),
        source: Some("api".to_owned()),
        data_confidence: if history.is_some_and(|history| history.partial) {
            "estimated"
        } else {
            "authoritative"
        }
        .to_owned(),
    }
}

#[cfg(test)]
mod tests;
