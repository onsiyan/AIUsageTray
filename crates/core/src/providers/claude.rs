pub use super::claude_planner::ClaudeSourceMode;
use crate::{
    accounts::{AccountId, AccountRecord, CLAUDE, VerifiedIdentity},
    auth::{
        AccountAuthMaterialProvider, AccountAuthMaterialStore, AccountBrowserSessionRefresher,
        AuthError,
    },
    providers::claude_cli::{self, ClaudeCliError, ClaudeCliProbeOptions},
    providers::claude_planner::{self, ClaudeRuntime, ClaudeSource},
    providers::shared::{
        invalid_payload, json_number, json_string, missing_auth, normalize_percent, reset_at,
    },
    transport::{TransportError, UsageHttpRequest, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, CreditsSnapshot, RateLimitWindow, SpendSnapshot, UsageAdapter,
        UsageAdapterErrorCode, UsageCreditInventory, UsageCreditRecord, UsageMetric,
        UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot, UsageSourceDiagnostic,
        UsageWindowKind,
    },
};

/// Claude Code version reported to the OAuth usage endpoint. The server only
/// includes reset grants for sufficiently recent CLI versions.
const CLAUDE_CODE_CLIENT_VERSION: &str = "2.1.999";
mod admin;
mod cli;
mod oauth;
mod spend;
mod web;

use admin::*;
use cli::*;
use oauth::*;
use spend::*;
use web::*;

use async_trait::async_trait;
use chrono::{Duration, SecondsFormat, Utc};
pub use oauth::fetch_oauth_identity;
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use url::Url;
pub use web::fetch_web_identity;

pub struct ClaudeUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    auth_material_store: Option<Arc<dyn AccountAuthMaterialStore>>,
    browser_session_refresher: Option<Arc<dyn AccountBrowserSessionRefresher>>,
    base_url: Url,
    oauth_base_url: Url,
    fetch_prepaid_credits: bool,
    fetch_web_extras: bool,
    fetch_account_identity: bool,
    source_mode: ClaudeSourceMode,
    runtime: ClaudeRuntime,
    oauth_rate_limit_until: Arc<Mutex<HashMap<String, chrono::DateTime<Utc>>>>,
    cli_rate_limit_until: Arc<Mutex<HashMap<String, chrono::DateTime<Utc>>>>,
    cli_cache: Arc<Mutex<HashMap<CliCacheKey, CachedCliResult>>>,
}

impl ClaudeUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        fetch_prepaid_credits: bool,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            auth_material_store: None,
            browser_session_refresher: None,
            base_url: Url::parse("https://claude.ai/")
                .map_err(|error| TransportError::InvalidUrl(error.to_string()))?,
            oauth_base_url: Url::parse("https://api.anthropic.com/")
                .map_err(|error| TransportError::InvalidUrl(error.to_string()))?,
            fetch_prepaid_credits,
            fetch_web_extras: true,
            fetch_account_identity: false,
            source_mode: ClaudeSourceMode::Automatic,
            runtime: ClaudeRuntime::App,
            oauth_rate_limit_until: Arc::new(Mutex::new(HashMap::new())),
            cli_rate_limit_until: Arc::new(Mutex::new(HashMap::new())),
            cli_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Enables the same optional identity enrichment used by the reference
    /// implementation. It is deliberately opt-in at the adapter constructor
    /// so a plain usage poll never adds a second identity request.
    pub fn with_account_identity(mut self, enabled: bool) -> Self {
        self.fetch_account_identity = enabled;
        self
    }

    /// Persists a server-rotated Claude Web session only after the adapter has
    /// verified the response identity against this account's recorded email.
    pub fn with_auth_material_store(mut self, store: Arc<dyn AccountAuthMaterialStore>) -> Self {
        self.auth_material_store = Some(store);
        self
    }

    /// Enables account-bound browser session recovery after a real 401. The
    /// replacement cookie is identity-checked before secure storage or retry.
    pub fn with_browser_session_refresher(
        mut self,
        refresher: Arc<dyn AccountBrowserSessionRefresher>,
    ) -> Self {
        self.browser_session_refresher = Some(refresher);
        self
    }

    pub fn with_web_extras(mut self, enabled: bool) -> Self {
        self.fetch_web_extras = enabled;
        self
    }

    pub fn with_source_mode(mut self, source_mode: ClaudeSourceMode) -> Self {
        self.source_mode = source_mode;
        self
    }

    pub fn with_runtime(mut self, runtime: ClaudeRuntime) -> Self {
        self.runtime = runtime;
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
                crate::auth::AccountAuthMaterial::default()
            }
            Err(error) => return Ok(invalid_payload("Claude", error.to_string())),
        };
        let admin_key = material
            .bearer_token
            .as_deref()
            .and_then(normalize_claude_admin_token);
        let access_token = material
            .bearer_token
            .as_deref()
            .and_then(normalize_claude_oauth_token);
        let session_key = claude_session_key(&material);
        let environment = std::env::vars().collect::<HashMap<_, _>>();
        let plan = claude_planner::plan(claude_planner::ClaudeSourcePlanningInput {
            runtime: self.runtime,
            selected_source: self.source_mode,
            has_admin_api_key: admin_key.is_some(),
            has_web_session: session_key.is_some(),
            has_cli: claude_cli::is_available(&environment),
            has_oauth_credentials: access_token.is_some(),
        });

        // Report the failure of the first source actually attempted (the
        // account's preferred one). A later fallback failing for an unrelated
        // reason must not replace, for example, a retryable OAuth rate limit.
        let mut first_failure = None;
        for step in &plan.ordered_steps {
            if !step.is_plausibly_available {
                continue;
            }
            let result = match step.source {
                ClaudeSource::AdminApi => {
                    self.probe_admin(account, admin_key.as_deref().expect("planner checked key"))
                        .await
                }
                ClaudeSource::OAuth => {
                    probe_oauth_with_scope(
                        self,
                        account,
                        access_token.as_deref().expect("planner checked token"),
                        &material,
                        session_key.as_deref(),
                    )
                    .await
                }
                ClaudeSource::Web => {
                    self.probe_web(
                        account,
                        session_key.as_deref().expect("planner checked session"),
                    )
                    .await
                }
                ClaudeSource::Cli => {
                    let options = if self.source_mode == ClaudeSourceMode::Automatic {
                        ClaudeCliProbeOptions::automatic()
                    } else {
                        ClaudeCliProbeOptions::explicit()
                    };
                    Ok(self
                        .probe_cli(account, options, session_key.as_deref())
                        .await)
                }
            };
            // A transport error in one source must not skip the remaining
            // fallbacks.
            let result = result.unwrap_or_else(transport_failure);
            if result.succeeded() || oauth_is_account_boundary(&result) {
                return Ok(result);
            }
            first_failure.get_or_insert(result);
        }

        Ok(first_failure.unwrap_or_else(|| missing_auth(missing_source_label(self.source_mode))))
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

fn missing_source_label(source_mode: ClaudeSourceMode) -> &'static str {
    match source_mode {
        ClaudeSourceMode::Automatic => "Claude",
        ClaudeSourceMode::Cli => "Claude CLI",
        ClaudeSourceMode::OAuth => "Claude OAuth",
        ClaudeSourceMode::Web => "Claude Web",
        ClaudeSourceMode::AdminApi => "Claude Admin API",
    }
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

fn synthetic_session_window() -> RateLimitWindow {
    RateLimitWindow {
        kind: UsageWindowKind::Primary,
        name: "Session".to_owned(),
        used_percent: 0.0,
        reset_at_utc: None,
        limit_window_seconds: 5 * 60 * 60,
    }
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
        crate::usage::UsageAdapterErrorCode::CloudflareChallenge
    } else {
        match response.status_code {
            401 => crate::usage::UsageAdapterErrorCode::Unauthorized,
            403 => crate::usage::UsageAdapterErrorCode::Forbidden,
            429 => crate::usage::UsageAdapterErrorCode::RateLimited,
            500..=599 => crate::usage::UsageAdapterErrorCode::TransientHttp,
            _ => crate::usage::UsageAdapterErrorCode::HttpError,
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

fn email_matches(account: &AccountRecord, response_email: Option<&str>) -> bool {
    response_email
        .map(|email| email.trim().eq_ignore_ascii_case(&account.email))
        .unwrap_or(true)
}

fn account_mismatch(message: &str) -> UsageProbeResult {
    UsageProbeResult::failure(crate::usage::UsageAdapterError {
        code: crate::usage::UsageAdapterErrorCode::AccountMismatch,
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
