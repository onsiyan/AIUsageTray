//! GitHub Copilot quotas through GitHub's Copilot usage endpoint.
//!
//! Accounts sign in with GitHub's OAuth device flow (the same public client
//! the VS Code Copilot extension uses, as CodexBar does). The resulting
//! GitHub token reads `GET /copilot_internal/user`, which reports the monthly
//! premium-request and chat quotas as `quota_snapshots`, or, on older plans,
//! as `monthly_quotas` / `limited_user_quotas` counts.
//!
//! The payload shape varies by plan, so parsing follows CodexBar's rules:
//! numbers may arrive as strings, percentages are derived when missing,
//! zero-entitlement placeholders (Copilot Business usage billing) never
//! become fake "0% used" bars, and unlimited quotas are not drawn as bars.

use crate::{
    accounts::{AccountRecord, COPILOT, VerifiedIdentity},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{invalid_payload, json_bool, json_string, map_http_error, missing_auth},
    transport::{TransportError, UsageHttpRequest, UsageHttpTransport},
    usage::{
        RateLimitWindow, UsageAdapter, UsageAdapterError, UsageAdapterErrorCode, UsageMetric,
        UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use url::Url;

const USAGE_URL: &str = "https://api.github.com/copilot_internal/user";
const USER_URL: &str = "https://api.github.com/user";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// The public OAuth client of the VS Code Copilot extension.
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const SCOPES: &str = "read:user";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(8);

pub const PREMIUM_WINDOW_NAME: &str = "Premium requests";
pub const CHAT_WINDOW_NAME: &str = "Chat";

pub struct CopilotUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    usage_url: Url,
    deadline: Duration,
}

impl CopilotUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            usage_url: parse_url(USAGE_URL)?,
            deadline: DEFAULT_DEADLINE,
        })
    }

    /// Bounds the usage request, for hosts with a stricter refresh budget.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

#[async_trait]
impl UsageAdapter for CopilotUsageAdapter {
    fn adapter_id(&self) -> &str {
        COPILOT
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let token = match self.auth.get(account).await {
            Ok(Some(material)) => material
                .bearer_token
                .map(|token| token.trim().to_owned())
                .filter(|token| !token.is_empty()),
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => None,
            Err(error) => return Ok(invalid_payload("Copilot", error.to_string())),
        };
        let Some(token) = token else {
            return Ok(missing_auth("Copilot"));
        };

        let response = tokio::time::timeout(
            self.deadline,
            self.transport.send(UsageHttpRequest {
                method: Method::GET,
                url: self.usage_url.clone(),
                headers: copilot_headers(&token),
                body: None,
            }),
        )
        .await
        .map_err(|_| TransportError::Timeout("copilot usage".to_owned()))??;
        if response.status_code == 404 {
            // GitHub answers 404 when the account has no Copilot seat.
            return Ok(UsageProbeResult::failure(UsageAdapterError {
                code: UsageAdapterErrorCode::NoSubscription,
                message: "This GitHub account has no Copilot access".to_owned(),
                http_status_code: Some(404),
                retry_after_seconds: None,
            }));
        }
        if !response.is_success() {
            return Ok(map_http_error(&response, "Copilot"));
        }
        let usage = match parse_usage(&response.body) {
            Ok(usage) => usage,
            Err(reason) => return Ok(invalid_payload("Copilot", reason)),
        };
        let snapshot = match usage.snapshot(account, Utc::now()) {
            Ok(snapshot) => snapshot,
            Err(reason) => return Ok(invalid_payload("Copilot", reason)),
        };
        // The usage endpoint does not name the GitHub user; the account keeps
        // the identity verified when it signed in.
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: None,
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

/// Headers the Copilot endpoint expects, matching the VS Code extension.
fn copilot_headers(token: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".to_owned(), format!("token {token}")),
        ("Accept".to_owned(), "application/json".to_owned()),
        ("Editor-Version".to_owned(), "vscode/1.96.2".to_owned()),
        (
            "Editor-Plugin-Version".to_owned(),
            "copilot-chat/0.26.7".to_owned(),
        ),
        (
            "User-Agent".to_owned(),
            "GitHubCopilotChat/0.26.7".to_owned(),
        ),
        ("X-Github-Api-Version".to_owned(), "2025-04-01".to_owned()),
    ])
}

fn parse_url(url: &str) -> Result<Url, TransportError> {
    Url::parse(url).map_err(|error| TransportError::InvalidUrl(error.to_string()))
}

/// One quota lane of the usage payload.
#[derive(Debug, Clone, PartialEq)]
struct QuotaSnapshot {
    entitlement: f64,
    remaining: f64,
    credits_used: Option<f64>,
    percent_remaining: f64,
    has_percent_remaining: bool,
    unlimited: bool,
    entitlement_was_read: bool,
    remaining_was_read: bool,
}

impl QuotaSnapshot {
    fn parse(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let entitlement = number(object.get("entitlement"));
        let remaining = number(object.get("remaining"));
        let unlimited = object
            .get("unlimited")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let percent = number(object.get("percent_remaining"));
        let (percent_remaining, has_percent_remaining) = if unlimited {
            (100.0, true)
        } else if let Some(percent) = percent {
            (percent, true)
        } else if let (Some(entitlement), Some(remaining)) = (entitlement, remaining)
            && entitlement > 0.0
        {
            (remaining / entitlement * 100.0, true)
        } else {
            (0.0, false)
        };
        Some(Self {
            entitlement: entitlement.unwrap_or(0.0),
            remaining: remaining.unwrap_or(0.0),
            credits_used: number(object.get("credits_used")),
            percent_remaining,
            has_percent_remaining,
            unlimited,
            entitlement_was_read: entitlement.is_some(),
            remaining_was_read: remaining.is_some(),
        })
    }

    /// A lane built from the older monthly / remaining counts.
    fn from_counts(monthly: Option<f64>, limited: Option<f64>) -> Option<Self> {
        let entitlement = monthly?.max(0.0);
        let remaining = limited?.max(0.0);
        if entitlement <= 0.0 {
            return None;
        }
        Some(Self {
            entitlement,
            remaining,
            credits_used: None,
            percent_remaining: (remaining / entitlement * 100.0).clamp(0.0, 100.0),
            has_percent_remaining: true,
            unlimited: false,
            entitlement_was_read: true,
            remaining_was_read: true,
        })
    }

    fn used_percent(&self) -> f64 {
        (100.0 - self.percent_remaining).max(0.0)
    }

    /// No usable quota: Copilot Business usage billing reports lanes with a
    /// zero entitlement, sometimes with `percent_remaining: 100`.
    fn is_placeholder(&self) -> bool {
        if self.unlimited {
            return false;
        }
        (self.entitlement == 0.0
            && self.remaining == 0.0
            && self.percent_remaining == 0.0
            && !self.has_percent_remaining)
            || (self.entitlement_was_read
                && self.remaining_was_read
                && self.entitlement == 0.0
                && self.remaining == 0.0)
    }

    fn is_usable(&self) -> bool {
        !self.is_placeholder() && self.has_percent_remaining
    }

    fn with_credits_used(mut self, credits_used: Option<f64>) -> Self {
        self.credits_used = credits_used;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CopilotUsage {
    premium: Option<QuotaSnapshot>,
    chat: Option<QuotaSnapshot>,
    plan: String,
    token_based_billing: bool,
    quota_reset_date: Option<String>,
}

fn parse_usage(body: &str) -> Result<CopilotUsage, String> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| format!("usage JSON could not be parsed: {error}"))?;
    if !root.is_object() {
        return Err("usage response is not an object".to_owned());
    }

    let (direct_premium, direct_chat) = root
        .get("quota_snapshots")
        .map(parse_quota_snapshots)
        .unwrap_or((None, None));
    let counts = |name: &str| {
        let monthly = number(
            root.get("monthly_quotas")
                .and_then(|quotas| quotas.get(name)),
        );
        let limited = number(
            root.get("limited_user_quotas")
                .and_then(|quotas| quotas.get(name)),
        );
        QuotaSnapshot::from_counts(monthly, limited)
    };
    let premium = preferred_quota(direct_premium.as_ref(), counts("completions"));
    let chat = preferred_quota(direct_chat.as_ref(), counts("chat"));
    let (premium, chat) = if premium.is_some() || chat.is_some() {
        (premium, chat)
    } else {
        (direct_premium, direct_chat)
    };

    Ok(CopilotUsage {
        premium,
        chat,
        plan: json_string(&root, &["copilot_plan"]).unwrap_or_else(|| "unknown".to_owned()),
        token_based_billing: json_bool(&root, &["token_based_billing"]).unwrap_or(false),
        quota_reset_date: json_string(&root, &["quota_reset_date"]),
    })
}

/// The premium and chat lanes of `quota_snapshots`. Unfamiliar key names
/// still yield a lane: "chat" names the chat lane, and "premium",
/// "completion", or "code" the premium one.
fn parse_quota_snapshots(value: &Value) -> (Option<QuotaSnapshot>, Option<QuotaSnapshot>) {
    let Some(object) = value.as_object() else {
        return (None, None);
    };
    let keep = |snapshot: QuotaSnapshot| {
        (!snapshot.is_placeholder() || snapshot.credits_used.is_some()).then_some(snapshot)
    };
    let mut premium = object
        .get("premium_interactions")
        .and_then(QuotaSnapshot::parse)
        .and_then(keep);
    let mut chat = object
        .get("chat")
        .and_then(QuotaSnapshot::parse)
        .and_then(keep);
    if premium.is_none() || chat.is_none() {
        let mut fallback_premium = None;
        let mut fallback_chat = None;
        let mut first_usable = None;
        for (key, value) in object {
            let Some(snapshot) = QuotaSnapshot::parse(value).and_then(keep) else {
                continue;
            };
            let name = key.to_ascii_lowercase();
            if first_usable.is_none() {
                first_usable = Some(snapshot.clone());
            }
            if fallback_chat.is_none() && name.contains("chat") {
                fallback_chat = Some(snapshot);
                continue;
            }
            if fallback_premium.is_none()
                && (name.contains("premium")
                    || name.contains("completion")
                    || name.contains("code"))
            {
                fallback_premium = Some(snapshot);
            }
        }
        premium = premium.or(fallback_premium);
        chat = chat.or(fallback_chat);
        if premium.is_none() && chat.is_none() {
            chat = first_usable;
        }
    }
    (premium, chat)
}

/// Picks the direct lane when it is usable, otherwise the count-based one,
/// keeping a real credit counter from the direct lane either way.
fn preferred_quota(
    direct: Option<&QuotaSnapshot>,
    fallback: Option<QuotaSnapshot>,
) -> Option<QuotaSnapshot> {
    let fallback = fallback.filter(QuotaSnapshot::is_usable);
    if let Some(direct) = direct
        && direct.unlimited
        && let Some(fallback) = fallback.clone()
    {
        return Some(fallback.with_credits_used(direct.credits_used));
    }
    if let Some(direct) = direct.filter(|direct| direct.is_usable()) {
        return Some(direct.clone());
    }
    let fallback = fallback?;
    match direct.and_then(|direct| direct.credits_used) {
        Some(credits_used) => Some(fallback.with_credits_used(Some(credits_used))),
        None => Some(fallback),
    }
}

impl CopilotUsage {
    fn snapshot(
        &self,
        account: &AccountRecord,
        now: DateTime<Utc>,
    ) -> Result<UsageSnapshot, String> {
        let reset_at = self.quota_reset_date.as_deref().and_then(parse_reset_date);
        let premium = self
            .premium
            .as_ref()
            .and_then(|quota| quota_window(quota, PREMIUM_WINDOW_NAME, reset_at));
        let chat = self
            .chat
            .as_ref()
            .and_then(|quota| quota_window(quota, CHAT_WINDOW_NAME, reset_at));
        let unlimited = [&self.premium, &self.chat]
            .into_iter()
            .flatten()
            .any(|quota| quota.unlimited);

        let mut metrics = Vec::new();
        for (lane, window) in [("premium", &premium), ("chat", &chat)] {
            if let Some(window) = window
                && window.used_percent > 100.0
            {
                // Kept out of `used_percent`, which would draw a second bar.
                metrics.push(UsageMetric {
                    metadata: HashMap::from([(
                        "used_percent".to_owned(),
                        format!("{:.0}", window.used_percent),
                    )]),
                    ..note(
                        &format!("{lane}.over_quota"),
                        &format!("{}: {:.0}% used", window.name, window.used_percent),
                    )
                });
            }
        }
        // GitHub reports `credits_used: 0` on metered seats too; the row is
        // shown only when it says something.
        let credits_used = self
            .premium
            .as_ref()
            .and_then(|quota| quota.credits_used)
            .or_else(|| self.chat.as_ref().and_then(|quota| quota.credits_used))
            .filter(|credits| credits.is_finite());
        if let Some(credits_used) = credits_used
            && (self.token_based_billing || unlimited || credits_used > 0.0)
        {
            metrics.push(UsageMetric {
                used_amount: Some(credits_used),
                unit: Some("credits".to_owned()),
                ..note("credits.used", "Credits used")
            });
        }

        let (primary, secondary) = match (premium, chat) {
            (Some(premium), chat) => (Some(premium), chat),
            (None, Some(chat)) => (Some(chat), None),
            (None, None) if self.token_based_billing || unlimited => {
                if metrics.is_empty() {
                    metrics.push(if unlimited {
                        note("quota.unlimited", "Unlimited")
                    } else {
                        note("quota.usage_billed", "Billed by usage")
                    });
                }
                (None, None)
            }
            (None, None) => return Err("usage response has no quota".to_owned()),
        };
        let mut primary = primary;
        let mut secondary = secondary;
        if let Some(window) = &mut primary {
            window.kind = UsageWindowKind::Primary;
        }
        if let Some(window) = &mut secondary {
            window.kind = UsageWindowKind::Secondary;
        }

        Ok(UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: None,
            plan_type: Some(self.plan.clone()).filter(|plan| plan != "unknown"),
            primary_window_kind: primary.as_ref().map(|_| UsagePrimaryWindowKind::Other),
            primary,
            primary_window_is_synthetic: false,
            secondary,
            additional_windows: Vec::new(),
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id: COPILOT.to_owned(),
            source: Some("api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        })
    }
}

/// A bar for a metered lane; unlimited lanes and placeholders have none.
fn quota_window(
    quota: &QuotaSnapshot,
    name: &str,
    reset_at: Option<DateTime<Utc>>,
) -> Option<RateLimitWindow> {
    if quota.unlimited || !quota.is_usable() {
        return None;
    }
    Some(RateLimitWindow {
        kind: UsageWindowKind::Primary,
        name: name.to_owned(),
        used_percent: quota.used_percent(),
        reset_at_utc: reset_at,
        limit_window_seconds: 0,
    })
}

fn note(key: &str, name: &str) -> UsageMetric {
    UsageMetric {
        key: key.to_owned(),
        name: name.to_owned(),
        used_percent: None,
        used_amount: None,
        limit_amount: None,
        remaining_amount: None,
        unit: None,
        reset_at_utc: None,
        reset_label: None,
        metadata: HashMap::new(),
    }
}

/// `quota_reset_date` is a date (`2025-02-01`) or a full timestamp.
fn parse_reset_date(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|time| time.and_utc())
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

/// GitHub's device flow: the user enters `user_code` at `verification_uri`
/// while the app polls for the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubIdentity {
    pub id: u64,
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotLoginError {
    /// The code expired before the user entered it.
    Expired,
    /// The user declined the authorization on GitHub.
    Denied,
    Rejected(String),
    Transport(String),
}

impl std::fmt::Display for CopilotLoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => formatter.write_str("The GitHub sign-in code expired"),
            Self::Denied => formatter.write_str("The GitHub sign-in was declined"),
            Self::Rejected(reason) => write!(formatter, "GitHub rejected the sign-in: {reason}"),
            Self::Transport(reason) => write!(formatter, "Could not reach GitHub: {reason}"),
        }
    }
}

impl std::error::Error for CopilotLoginError {}

impl From<TransportError> for CopilotLoginError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error.to_string())
    }
}

fn form_request(url: &str, fields: &[(&str, &str)]) -> Result<UsageHttpRequest, TransportError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    Ok(UsageHttpRequest {
        method: Method::POST,
        url: parse_url(url)?,
        headers: BTreeMap::from([
            ("Accept".to_owned(), "application/json".to_owned()),
            (
                "Content-Type".to_owned(),
                "application/x-www-form-urlencoded".to_owned(),
            ),
        ]),
        body: Some(body),
    })
}

/// Starts a device-flow sign-in.
pub async fn request_device_code(
    transport: &dyn UsageHttpTransport,
) -> Result<DeviceCode, CopilotLoginError> {
    let response = transport
        .send(form_request(
            DEVICE_CODE_URL,
            &[("client_id", CLIENT_ID), ("scope", SCOPES)],
        )?)
        .await?;
    if !response.is_success() {
        return Err(CopilotLoginError::Rejected(format!(
            "HTTP {}",
            response.status_code
        )));
    }
    parse_device_code(&response.body)
}

fn parse_device_code(body: &str) -> Result<DeviceCode, CopilotLoginError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|_| CopilotLoginError::Rejected("unreadable device code".to_owned()))?;
    let field = |name: &str| {
        json_string(&root, &[name])
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CopilotLoginError::Rejected(format!("device code is missing {name}")))
    };
    Ok(DeviceCode {
        device_code: field("device_code")?,
        user_code: field("user_code")?,
        verification_uri: field("verification_uri")?,
        expires_in: root
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
        interval: root
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
    })
}

/// Waits until the user enters the code on GitHub and returns the token.
pub async fn poll_for_token(
    transport: &dyn UsageHttpTransport,
    code: &DeviceCode,
) -> Result<String, CopilotLoginError> {
    let request = form_request(
        ACCESS_TOKEN_URL,
        &[
            ("client_id", CLIENT_ID),
            ("device_code", &code.device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ],
    )?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in.max(60));
    let mut interval = code.interval;
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(CopilotLoginError::Expired);
        }
        let response = transport.send(request.clone()).await?;
        match token_poll_outcome(&response.body) {
            TokenPoll::Token(token) => return Ok(token),
            TokenPoll::Pending => {}
            // GitHub asks for a longer interval, and says which.
            TokenPoll::SlowDown(next) => interval = next.unwrap_or(interval + 5),
            TokenPoll::Failed(error) => return Err(error),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TokenPoll {
    Token(String),
    Pending,
    SlowDown(Option<u64>),
    Failed(CopilotLoginError),
}

fn token_poll_outcome(body: &str) -> TokenPoll {
    let Ok(root) = serde_json::from_str::<Value>(body) else {
        return TokenPoll::Failed(CopilotLoginError::Rejected(
            "unreadable token response".to_owned(),
        ));
    };
    match json_string(&root, &["error"]).as_deref() {
        Some("authorization_pending") => TokenPoll::Pending,
        Some("slow_down") => TokenPoll::SlowDown(root.get("interval").and_then(Value::as_u64)),
        Some("expired_token") => TokenPoll::Failed(CopilotLoginError::Expired),
        Some("access_denied") => TokenPoll::Failed(CopilotLoginError::Denied),
        Some(other) => TokenPoll::Failed(CopilotLoginError::Rejected(other.to_owned())),
        None => match json_string(&root, &["access_token"]).filter(|token| !token.is_empty()) {
            Some(token) => TokenPoll::Token(token),
            None => TokenPoll::Failed(CopilotLoginError::Rejected(
                "no access token in the response".to_owned(),
            )),
        },
    }
}

/// The GitHub user a token belongs to.
pub async fn fetch_identity(
    transport: &dyn UsageHttpTransport,
    token: &str,
) -> Result<GitHubIdentity, CopilotLoginError> {
    let response = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url: parse_url(USER_URL)?,
            headers: BTreeMap::from([
                ("Authorization".to_owned(), format!("token {token}")),
                ("Accept".to_owned(), "application/json".to_owned()),
                ("User-Agent".to_owned(), "UsageMonitor/0.1".to_owned()),
            ]),
            body: None,
        })
        .await?;
    if !response.is_success() {
        return Err(CopilotLoginError::Rejected(format!(
            "identity lookup returned HTTP {}",
            response.status_code
        )));
    }
    let root: Value = serde_json::from_str(&response.body)
        .map_err(|_| CopilotLoginError::Rejected("unreadable GitHub user".to_owned()))?;
    match (
        root.get("id").and_then(Value::as_u64),
        json_string(&root, &["login"]),
    ) {
        (Some(id), Some(login)) if !login.is_empty() => Ok(GitHubIdentity { id, login }),
        _ => Err(CopilotLoginError::Rejected(
            "GitHub user is missing its id or login".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests;
