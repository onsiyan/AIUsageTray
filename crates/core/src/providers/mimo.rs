//! Xiaomi MiMo balance and token plan through the platform console.
//!
//! MiMo has no API for usage; like CodexBar, this reads the endpoints the
//! console at platform.xiaomimimo.com uses, authenticated with its session
//! cookies (`api-platform_serviceToken` and `userId`). The user pastes the
//! console's `Cookie:` header when adding the account; only the MiMo cookies
//! are kept from it, and browsers are never read.
//!
//! - `GET /api/v1/balance` (required): the balance, with paid and granted
//!   funds when MiMo reports them.
//! - `GET /api/v1/tokenPlan/detail` and `tokenPlan/usage`: the plan and its
//!   monthly token allowance. Best effort: without them the balance shows.

use crate::{
    accounts::{AccountRecord, MIMO, VerifiedIdentity},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{invalid_payload, json_bool, json_string, map_http_error, missing_auth},
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        CreditsSnapshot, RateLimitWindow, UsageAdapter, UsageAdapterError, UsageAdapterErrorCode,
        UsageMetric, UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use url::Url;

const API_URL: &str = "https://platform.xiaomimimo.com/api/v1/";
const ORIGIN: &str = "https://platform.xiaomimimo.com";
const REFERER: &str = "https://platform.xiaomimimo.com/#/console/balance";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(15);
const REQUIRED_COOKIES: [&str; 2] = ["api-platform_serviceToken", "userId"];
const KNOWN_COOKIES: [&str; 4] = [
    "api-platform_ph",
    "api-platform_serviceToken",
    "api-platform_slh",
    "userId",
];
/// The token plan is monthly; MiMo gives its end but not its start.
const PLAN_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;
pub const TOKEN_PLAN_WINDOW_NAME: &str = "Monthly tokens";

/// The MiMo cookies from whatever the user pasted: a whole `Cookie:` header,
/// a copied request, or just `name=value` pairs. `None` when either required
/// cookie is missing.
pub fn cookie_header(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let start = lower
        .find("cookie:")
        .map_or(0, |index| index + "cookie:".len());
    let header = input[start..]
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(|character| matches!(character, '\'' | '"'));
    let mut cookies = BTreeMap::new();
    for pair in header.split(';') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if KNOWN_COOKIES.contains(&name) && !value.is_empty() {
            cookies.insert(name, value);
        }
    }
    REQUIRED_COOKIES
        .iter()
        .all(|name| cookies.contains_key(name))
        .then(|| {
            cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
}

pub struct MiMoUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    api_url: Url,
    deadline: Duration,
}

impl MiMoUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            api_url: Url::parse(API_URL)
                .map_err(|error| TransportError::InvalidUrl(error.to_string()))?,
            deadline: DEFAULT_DEADLINE,
        })
    }

    async fn get(&self, path: &str, cookie: &str) -> Result<UsageHttpResponse, TransportError> {
        let url = self
            .api_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        let headers = BTreeMap::from([
            (
                "Accept".to_owned(),
                "application/json, text/plain, */*".to_owned(),
            ),
            ("Accept-Language".to_owned(), "en-US,en;q=0.9".to_owned()),
            ("Cookie".to_owned(), cookie.to_owned()),
            ("Origin".to_owned(), ORIGIN.to_owned()),
            ("Referer".to_owned(), REFERER.to_owned()),
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("x-timeZone".to_owned(), "UTC+00:00".to_owned()),
        ]);
        tokio::time::timeout(
            self.deadline,
            self.transport.send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers,
                body: None,
            }),
        )
        .await
        .map_err(|_| TransportError::Timeout("mimo".to_owned()))?
    }

    /// An optional endpoint's `data`, or `None` when it fails in any way.
    async fn optional_data(&self, path: &str, cookie: &str) -> Option<Value> {
        let response = self.get(path, cookie).await.ok()?;
        if !response.is_success() {
            return None;
        }
        let root: Value = serde_json::from_str(&response.body).ok()?;
        (root.get("code").and_then(Value::as_i64) == Some(0))
            .then(|| root.get("data").cloned())
            .flatten()
            .filter(Value::is_object)
    }
}

#[async_trait]
impl UsageAdapter for MiMoUsageAdapter {
    fn adapter_id(&self) -> &str {
        MIMO
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = match self.auth.get(account).await {
            Ok(Some(material)) => material,
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => {
                return Ok(missing_auth("Xiaomi MiMo"));
            }
            Err(error) => return Ok(invalid_payload("Xiaomi MiMo", error.to_string())),
        };
        let Some(cookie) = material.bearer_token.as_deref().and_then(cookie_header) else {
            return Ok(missing_auth("Xiaomi MiMo"));
        };

        let (balance, detail, usage) = tokio::join!(
            self.get("balance", &cookie),
            self.optional_data("tokenPlan/detail", &cookie),
            self.optional_data("tokenPlan/usage", &cookie),
        );
        let balance = balance?;
        // The console answers an expired session with a redirect to sign in.
        if (300..400).contains(&balance.status_code) || balance.status_code == 401 {
            return Ok(session_expired(
                "Xiaomi MiMo login required; paste a new cookie",
            ));
        }
        if balance.status_code == 403 {
            return Ok(session_expired(
                "Xiaomi MiMo session expired; sign in again and paste a new cookie",
            ));
        }
        if !balance.is_success() {
            return Ok(map_http_error(&balance, "Xiaomi MiMo"));
        }
        let balance = match parse_balance(&balance.body) {
            Ok(balance) => balance,
            Err(Failure::SignedOut(message)) => return Ok(session_expired(&message)),
            Err(Failure::Payload(reason)) => return Ok(invalid_payload("Xiaomi MiMo", reason)),
        };
        let plan = detail.as_ref().map(parse_plan_detail).unwrap_or_default();
        let tokens = usage.as_ref().and_then(parse_token_usage);

        let snapshot = MiMoUsage {
            balance,
            plan,
            tokens,
        }
        .snapshot(account, Utc::now());
        // The console does not name the signed-in user, so the account keeps
        // the label it was added with.
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: None,
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

fn session_expired(message: &str) -> UsageProbeResult {
    UsageProbeResult::failure(UsageAdapterError {
        code: UsageAdapterErrorCode::Unauthorized,
        message: message.to_owned(),
        http_status_code: None,
        retry_after_seconds: None,
    })
}

#[derive(Debug)]
enum Failure {
    SignedOut(String),
    Payload(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Balance {
    total: f64,
    currency: String,
    paid: Option<f64>,
    granted: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct PlanDetail {
    code: Option<String>,
    period_end: Option<DateTime<Utc>>,
}

/// The monthly allowance's used share, 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TokenUsage {
    fraction: f64,
}

fn decimal(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

fn parse_balance(body: &str) -> Result<Balance, Failure> {
    // An HTML sign-in page instead of JSON means the session is over.
    let root: Value = serde_json::from_str(body).map_err(|_| {
        Failure::SignedOut("Xiaomi MiMo session expired; paste a new cookie".to_owned())
    })?;
    let code = root
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| Failure::Payload("balance response has no code".to_owned()))?;
    if code != 0 {
        if matches!(code, 401 | 403) {
            return Err(Failure::SignedOut(
                "Xiaomi MiMo session expired; paste a new cookie".to_owned(),
            ));
        }
        let message = json_string(&root, &["message"]).unwrap_or_else(|| format!("code {code}"));
        return Err(Failure::Payload(message));
    }
    let data = root
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| Failure::Payload("missing balance payload".to_owned()))?;
    let total = decimal(data.get("balance"))
        .ok_or_else(|| Failure::Payload("invalid balance value".to_owned()))?;
    let currency = json_string(data, &["currency"])
        .map(|currency| currency.trim().to_owned())
        .filter(|currency| !currency.is_empty())
        .ok_or_else(|| Failure::Payload("missing currency".to_owned()))?;
    Ok(Balance {
        total,
        currency,
        paid: decimal(data.get("cashBalance")),
        granted: decimal(data.get("giftBalance")),
    })
}

fn parse_plan_detail(data: &Value) -> PlanDetail {
    PlanDetail {
        code: json_string(data, &["planCode"]).filter(|code| !code.trim().is_empty()),
        // MiMo writes the period end as UTC without a zone.
        period_end: json_string(data, &["currentPeriodEnd"])
            .and_then(|text| NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S").ok())
            .map(|time| time.and_utc()),
    }
    .clean(json_bool(data, &["expired"]) == Some(true))
}

impl PlanDetail {
    /// An expired plan has no allowance to reset.
    fn clean(mut self, expired: bool) -> Self {
        if expired {
            self.period_end = None;
        }
        self
    }
}

fn parse_token_usage(data: &Value) -> Option<TokenUsage> {
    let item = data.get("monthUsage")?.get("items")?.as_array()?.first()?;
    let used = decimal(item.get("used"))?;
    let limit = decimal(item.get("limit")).filter(|limit| *limit > 0.0)?;
    let fraction = decimal(item.get("percent")).unwrap_or(used / limit);
    Some(TokenUsage {
        fraction: fraction.clamp(0.0, 1.0),
    })
}

/// "standard" reads "Standard", as on the console.
fn plan_name(code: &str) -> String {
    let mut characters = code.trim().chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

struct MiMoUsage {
    balance: Balance,
    plan: PlanDetail,
    tokens: Option<TokenUsage>,
}

impl MiMoUsage {
    fn snapshot(&self, account: &AccountRecord, now: DateTime<Utc>) -> UsageSnapshot {
        let balance = &self.balance;
        let primary = self.tokens.map(|tokens| RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: TOKEN_PLAN_WINDOW_NAME.to_owned(),
            used_percent: tokens.fraction * 100.0,
            reset_at_utc: self.plan.period_end,
            limit_window_seconds: PLAN_WINDOW_SECONDS,
        });
        let amount = |key: &str, name: &str, value: f64| UsageMetric {
            key: key.to_owned(),
            name: name.to_owned(),
            used_percent: None,
            used_amount: None,
            limit_amount: None,
            remaining_amount: Some(value),
            unit: Some(balance.currency.clone()),
            reset_at_utc: None,
            reset_label: None,
            metadata: HashMap::new(),
        };
        let mut metrics = vec![amount("balance", "Balance", balance.total)];
        if let (Some(paid), Some(granted)) = (balance.paid, balance.granted)
            && granted > 0.0
        {
            metrics.push(amount("balance.topped_up", "Paid", paid));
            metrics.push(amount("balance.granted", "Granted", granted));
        }
        UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: None,
            plan_type: self.plan.code.as_deref().map(plan_name),
            primary_window_kind: Some(if primary.is_some() {
                UsagePrimaryWindowKind::Other
            } else {
                UsagePrimaryWindowKind::Spend
            }),
            primary,
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits: Some(CreditsSnapshot {
                has_credits: Some(balance.total > 0.0),
                unlimited: Some(false),
                balance: Some(balance.total),
                currency_code: Some(balance.currency.clone()),
                approximate_message_cost: None,
                limit: None,
                balance_read_succeeded: Some(true),
                credits_available: Some(balance.total > 0.0),
            }),
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id: MIMO.to_owned(),
            source: Some("console".to_owned()),
            data_confidence: "authoritative".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests;
