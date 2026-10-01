//! Claude usage through the account's own OAuth sign-in.
//!
//! Every Claude account is added with Claude's browser OAuth flow, so the
//! OAuth usage API is the one source: it returns the session, weekly, and
//! model-scoped windows, extra-usage spend, and usage-limit reset grants.

use crate::{
    accounts::{AccountRecord, CLAUDE, VerifiedIdentity},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{
        invalid_payload, json_number, json_string, missing_auth, normalize_percent, reset_at,
    },
    transport::{TransportError, UsageHttpRequest, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, CreditsSnapshot, RateLimitWindow, SpendSnapshot, UsageAdapter,
        UsageAdapterErrorCode, UsageCreditInventory, UsageCreditRecord, UsageMetric,
        UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot, UsageWindowKind,
    },
};

/// Claude Code version reported to the OAuth usage endpoint. The server only
/// includes reset grants for sufficiently recent CLI versions.
const CLAUDE_CODE_CLIENT_VERSION: &str = "2.1.999";
mod oauth;
mod spend;

use oauth::*;
use spend::*;

use async_trait::async_trait;
use chrono::Utc;
pub use oauth::fetch_oauth_identity;
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use url::Url;

pub struct ClaudeUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    oauth_base_url: Url,
    fetch_account_identity: bool,
    oauth_rate_limit_until: Arc<Mutex<HashMap<String, chrono::DateTime<Utc>>>>,
}

impl ClaudeUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            oauth_base_url: Url::parse("https://api.anthropic.com/")
                .map_err(|error| TransportError::InvalidUrl(error.to_string()))?,
            fetch_account_identity: false,
            oauth_rate_limit_until: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Also reads the OAuth profile to verify the account's identity and
    /// name its plan. Opt-in so a plain usage poll stays a single request.
    pub fn with_account_identity(mut self, enabled: bool) -> Self {
        self.fetch_account_identity = enabled;
        self
    }
}

#[async_trait]
impl UsageAdapter for ClaudeUsageAdapter {
    fn adapter_id(&self) -> &str {
        CLAUDE
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = match self.auth.get(account).await {
            Ok(Some(material)) => material,
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => {
                return Ok(missing_auth("Claude OAuth"));
            }
            Err(error) => return Ok(invalid_payload("Claude", error.to_string())),
        };
        let Some(access_token) = material
            .bearer_token
            .as_deref()
            .and_then(normalize_claude_oauth_token)
        else {
            return Ok(missing_auth("Claude OAuth"));
        };
        if !material.oauth_scopes.is_empty()
            && !material
                .oauth_scopes
                .iter()
                .any(|scope| scope.eq_ignore_ascii_case("user:profile"))
        {
            return Ok(UsageProbeResult::failure(crate::usage::UsageAdapterError {
                code: UsageAdapterErrorCode::Forbidden,
                message: "Claude OAuth token is missing the required user:profile scope".to_owned(),
                http_status_code: Some(403),
                retry_after_seconds: None,
            }));
        }
        Ok(self
            .probe_oauth(account, &access_token)
            .await
            .unwrap_or_else(transport_failure))
    }
}

fn transport_failure(error: TransportError) -> UsageProbeResult {
    let code = match &error {
        TransportError::Serialization(_) => UsageAdapterErrorCode::InvalidPayload,
        TransportError::InvalidUrl(_) | TransportError::InvalidHeader { .. } => {
            UsageAdapterErrorCode::Unknown
        }
        TransportError::Timeout(_) | TransportError::Request(_) => {
            UsageAdapterErrorCode::NetworkFailure
        }
    };
    UsageProbeResult::failure(crate::usage::UsageAdapterError {
        code,
        message: format!("Claude {error}"),
        http_status_code: None,
        retry_after_seconds: None,
    })
}

fn parse_window(
    value: Option<&Value>,
    kind: UsageWindowKind,
    name: &str,
    now: chrono::DateTime<Utc>,
) -> Option<RateLimitWindow> {
    let value = value?.as_object()?;
    let value = Value::Object(value.clone());
    let percent = json_number(&value, &["utilization", "used_percent", "usedPercent"])?;
    let reset = reset_at(&value, now);
    let window_seconds = match kind {
        UsageWindowKind::Primary => 5 * 60 * 60,
        UsageWindowKind::Secondary | UsageWindowKind::Additional => 7 * 24 * 60 * 60,
    };
    Some(RateLimitWindow {
        kind,
        name: name.to_owned(),
        used_percent: normalize_percent(percent),
        reset_at_utc: reset,
        limit_window_seconds: window_seconds,
    })
}

fn primary_window_kind(window: Option<&RateLimitWindow>) -> Option<UsagePrimaryWindowKind> {
    let name = window?.name.to_ascii_lowercase();
    if name.contains("spend") {
        Some(UsagePrimaryWindowKind::Spend)
    } else if name.contains("week") || name.contains("sonnet") || name.contains("opus") {
        Some(UsagePrimaryWindowKind::Weekly)
    } else if name.contains("session") {
        Some(UsagePrimaryWindowKind::Session)
    } else {
        Some(UsagePrimaryWindowKind::Other)
    }
}

fn spend_limit_window(spend: &SpendSnapshot) -> Option<RateLimitWindow> {
    let limit = spend.monthly_limit.filter(|value| *value > 0.0)?;
    let used_percent = spend
        .used_percent
        .or_else(|| spend.monthly_usage.map(|used| used / limit * 100.0))?;
    Some(RateLimitWindow {
        kind: UsageWindowKind::Primary,
        name: "Spend limit".to_owned(),
        used_percent: normalize_percent(used_percent),
        reset_at_utc: None,
        limit_window_seconds: 0,
    })
}

fn map_claude_http_error(
    response: &crate::transport::UsageHttpResponse,
    source: &str,
) -> UsageProbeResult {
    let challenge = crate::providers::shared::is_cloudflare_challenge(response);
    let code = if challenge {
        UsageAdapterErrorCode::CloudflareChallenge
    } else {
        match response.status_code {
            401 => UsageAdapterErrorCode::Unauthorized,
            403 => UsageAdapterErrorCode::Forbidden,
            429 => UsageAdapterErrorCode::RateLimited,
            500..=599 => UsageAdapterErrorCode::TransientHttp,
            _ => UsageAdapterErrorCode::HttpError,
        }
    };
    let message = if challenge {
        format!("{source} is behind a Cloudflare challenge")
    } else if response.status_code == 401 {
        format!("{source} credentials are unauthorized")
    } else {
        format!("{source} returned HTTP {}", response.status_code)
    };
    let retry_after_seconds = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok());
    UsageProbeResult::failure(crate::usage::UsageAdapterError {
        code,
        message,
        http_status_code: Some(response.status_code),
        retry_after_seconds,
    })
}

fn identity_matches<'a>(
    account: &AccountRecord,
    response_email: Option<&str>,
    response_account_ids: impl IntoIterator<Item = Option<&'a str>>,
) -> bool {
    let email_matches = response_email
        .map(|email| email.trim().eq_ignore_ascii_case(&account.email))
        .unwrap_or(true);
    let id_matches = account
        .provider_account_id
        .as_deref()
        .is_none_or(|expected| {
            response_account_ids
                .into_iter()
                .flatten()
                .any(|actual| expected.trim() == actual.trim())
        });
    email_matches && id_matches
}

fn account_mismatch(message: &str) -> UsageProbeResult {
    UsageProbeResult::failure(crate::usage::UsageAdapterError {
        code: UsageAdapterErrorCode::AccountMismatch,
        message: message.to_owned(),
        http_status_code: None,
        retry_after_seconds: None,
    })
}

fn add_metric(metrics: &mut Vec<UsageMetric>, key: &str, window: Option<&RateLimitWindow>) {
    if let Some(window) = window {
        metrics.push(UsageMetric {
            key: key.to_owned(),
            name: window.name.clone(),
            used_percent: Some(window.used_percent),
            used_amount: None,
            limit_amount: None,
            remaining_amount: None,
            unit: None,
            reset_at_utc: window.reset_at_utc,
            reset_label: None,
            metadata: HashMap::new(),
        });
    }
}

#[cfg(test)]
mod tests;
