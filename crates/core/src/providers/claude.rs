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
use async_trait::async_trait;
use chrono::{Duration, SecondsFormat, Utc};
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use url::Url;

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

#[derive(Debug, Clone)]
struct CachedCliResult {
    recorded_at: chrono::DateTime<Utc>,
    result: UsageProbeResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CliCacheKey {
    account_id: AccountId,
    binary: String,
    account_scope: String,
    use_web_extras: bool,
    include_prepaid_balance: bool,
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

    async fn oauth_rate_limit_remaining(&self, access_token: &str) -> Option<i64> {
        let key = oauth_token_key(access_token);
        let now = Utc::now();
        let mut blocked = self.oauth_rate_limit_until.lock().await;
        let until = blocked.get(&key).copied()?;
        if until <= now {
            blocked.remove(&key);
            return None;
        }
        Some((until - now).num_seconds().max(1))
    }

    async fn record_oauth_rate_limit(
        &self,
        access_token: &str,
        response: &crate::transport::UsageHttpResponse,
    ) {
        let retry_after = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse::<i64>().ok())
            .filter(|seconds| *seconds >= 0)
            .unwrap_or(300)
            // An untrusted header must not overflow chrono or block for days.
            .min(24 * 60 * 60);
        self.oauth_rate_limit_until.lock().await.insert(
            oauth_token_key(access_token),
            Utc::now() + chrono::Duration::seconds(retry_after),
        );
    }

    async fn clear_oauth_rate_limit(&self, access_token: &str) {
        self.oauth_rate_limit_until
            .lock()
            .await
            .remove(&oauth_token_key(access_token));
    }

    async fn get(
        &self,
        path: &str,
        session_key: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Cookie".to_owned(), format!("sessionKey={session_key}")),
                    ("User-Agent".to_owned(), "UsageMonitor/0.1".to_owned()),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }

    async fn get_oauth(
        &self,
        access_token: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let mut url = self
            .oauth_base_url
            .join("api/oauth/usage")
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        // `cedar_ember=1` adds the usage-limit reset grants. The server only
        // returns them to a recent Claude Code CLI client, so identify as one.
        url.query_pairs_mut().append_pair("cedar_ember", "1");
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    ("Authorization".to_owned(), format!("Bearer {access_token}")),
                    ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
                    (
                        "User-Agent".to_owned(),
                        format!("claude-cli/{CLAUDE_CODE_CLIENT_VERSION} (external, cli)"),
                    ),
                    ("x-app".to_owned(), "cli".to_owned()),
                    (
                        "anthropic-client-platform".to_owned(),
                        "claude_code_cli".to_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }

    async fn get_oauth_profile(
        &self,
        access_token: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let url = self
            .oauth_base_url
            .join("api/oauth/profile")
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    ("Authorization".to_owned(), format!("Bearer {access_token}")),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }

    async fn get_admin(
        &self,
        path: &str,
        api_key: &str,
        start: chrono::DateTime<Utc>,
        end: chrono::DateTime<Utc>,
        group_by: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let mut url = self
            .oauth_base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair(
                "starting_at",
                &start.to_rfc3339_opts(SecondsFormat::Secs, true),
            )
            .append_pair("ending_at", &end.to_rfc3339_opts(SecondsFormat::Secs, true))
            .append_pair("bucket_width", "1d")
            .append_pair("limit", "31")
            .append_pair("group_by[]", group_by);
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
                    ("x-api-key".to_owned(), api_key.to_owned()),
                    ("User-Agent".to_owned(), "UsageMonitor/0.1".to_owned()),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }

    async fn probe_oauth(
        &self,
        account: &AccountRecord,
        access_token: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        if let Some(retry_after_seconds) = self.oauth_rate_limit_remaining(access_token).await {
            return Ok(UsageProbeResult::failure(crate::usage::UsageAdapterError {
                code: crate::usage::UsageAdapterErrorCode::RateLimited,
                message: "Claude OAuth usage endpoint is temporarily rate-limited".to_owned(),
                http_status_code: Some(429),
                retry_after_seconds: Some(retry_after_seconds as u64),
            }));
        }
        let response = self.get_oauth(access_token).await?;
        if !response.is_success() {
            if response.status_code == 429 {
                self.record_oauth_rate_limit(access_token, &response).await;
            }
            return Ok(map_claude_http_error(&response, "Claude OAuth"));
        }
        self.clear_oauth_rate_limit(access_token).await;
        let root: Value = serde_json::from_str(&response.body)
            .map_err(|error| TransportError::Serialization(error.to_string()))?;
        let now = Utc::now();
        let five_hour = parse_window(
            root.get("five_hour"),
            UsageWindowKind::Primary,
            "Session",
            now,
        );
        let weekly = parse_window(
            root.get("seven_day"),
            UsageWindowKind::Secondary,
            "Weekly",
            now,
        );
        // Promote the weekly lane to the primary lane when Claude has no live
        // five-hour session window (enterprise/credit accounts). The weekly
        // lane is retained independently below, exactly as the source API
        // exposes it.
        let usage_primary = five_hour
            .clone()
            .or_else(|| weekly.clone())
            .or_else(|| {
                parse_window(
                    root.get("seven_day_oauth_apps"),
                    UsageWindowKind::Primary,
                    "OAuth apps weekly",
                    now,
                )
            })
            .or_else(|| {
                parse_window(
                    root.get("seven_day_sonnet"),
                    UsageWindowKind::Primary,
                    "Sonnet weekly",
                    now,
                )
            })
            .or_else(|| {
                parse_window(
                    root.get("seven_day_opus"),
                    UsageWindowKind::Primary,
                    "Opus weekly",
                    now,
                )
            })
            .map(|mut window| {
                window.kind = UsageWindowKind::Primary;
                window
            });
        // The weekly lane is independent from the primary fallback. If the
        // API omits five_hour, the reference still exposes seven_day as both
        // the selected primary and the explicit weekly lane.
        let secondary = weekly.clone();
        let additional = parse_claude_extra_windows(&root, now);

        let (spend, credits) = parse_extra_usage(&root);
        let primary = usage_primary.or_else(|| spend.as_ref().and_then(spend_limit_window));
        if primary.is_none() && secondary.is_none() && additional.is_empty() && spend.is_none() {
            return Ok(invalid_payload(
                "Claude OAuth",
                "no usage lanes were present",
            ));
        }
        let mut metrics = Vec::new();
        let primary_metric_key = primary
            .as_ref()
            .is_some_and(|window| window.name == "Spend limit")
            .then_some("spend_limit")
            .unwrap_or("session");
        add_metric(&mut metrics, primary_metric_key, primary.as_ref());
        add_metric(&mut metrics, "weekly", secondary.as_ref());
        for item in &additional {
            add_metric(&mut metrics, &item.key, Some(&item.window));
        }
        let profile = if self.fetch_account_identity {
            self.get_oauth_profile(access_token)
                .await
                .ok()
                .filter(|response| response.is_success())
                .and_then(|response| parse_claude_profile(&response.body))
        } else {
            None
        };
        if let Some(profile) = profile.as_ref()
            && !identity_matches(
                account,
                profile.email.as_deref(),
                [
                    profile.organization_id.as_deref(),
                    profile.account_id.as_deref(),
                ],
            )
        {
            return Ok(account_mismatch(
                "Claude OAuth profile belongs to another account",
            ));
        }
        let response_account_id = profile
            .as_ref()
            .and_then(|profile| {
                profile
                    .organization_id
                    .clone()
                    .or_else(|| profile.account_id.clone())
            })
            .or_else(|| account.provider_account_id.clone());
        let observed_email = profile
            .as_ref()
            .and_then(|profile| profile.email.clone())
            .or_else(|| Some(account.email.clone()));
        // The usage payload rarely names the subscription; the profile does.
        let plan_type = claude_oauth_plan_type(&root).or_else(|| {
            profile
                .as_ref()
                .and_then(|profile| profile.plan_type.clone())
        });
        let primary_kind = primary_window_kind(primary.as_ref());
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: response_account_id.clone(),
            plan_type: plan_type.clone(),
            primary,
            primary_window_kind: primary_kind,
            primary_window_is_synthetic: false,
            secondary,
            additional_windows: additional,
            credits,
            credit_inventory: parse_claude_reset_grants(&root, now),
            spend,
            observed_email: observed_email.clone(),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id: CLAUDE.to_owned(),
            source: Some("oauth".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        Ok(UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: observed_email,
                provider_account_id: response_account_id,
                plan_type,
            }),
        ))
    }

    async fn probe_web(
        &self,
        account: &AccountRecord,
        session_key: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        let unauthorized = crate::usage::UsageAdapterErrorCode::Unauthorized;
        let initial = self.probe_web_once(account, session_key).await?;
        if initial.error.as_ref().map(|error| error.code) != Some(unauthorized) {
            return Ok(initial);
        }
        let (Some(refresher), Some(store)) = (
            self.browser_session_refresher.as_ref(),
            self.auth_material_store.as_ref(),
        ) else {
            return Ok(initial);
        };
        let Ok(Some(replacement)) = refresher.reimport(account).await else {
            return Ok(initial);
        };
        let Some(replacement_key) = claude_session_key(&replacement) else {
            return Ok(initial);
        };
        if replacement_key == session_key {
            return Ok(initial);
        }

        let (identity, verified_session_key) = match fetch_web_identity_for_account(
            self.transport.as_ref(),
            &replacement_key,
            account.provider_account_id.as_deref(),
        )
        .await
        {
            Ok(identity) => identity,
            Err(_) => return Ok(initial),
        };
        let email_matches = identity
            .email
            .as_deref()
            .is_some_and(|email| email.trim().eq_ignore_ascii_case(&account.email));
        let organization_matches = account
            .provider_account_id
            .as_deref()
            .is_none_or(|expected| identity.provider_account_id.as_deref() == Some(expected));
        if !email_matches || !organization_matches {
            return Ok(account_mismatch(
                "Claude browser session belongs to another account",
            ));
        }

        // The imported cookie is not written until both the account email and
        // the account's selected organization have been verified.
        let replaced = store
            .replace_cookie_if_matches(account.id, "sessionKey", session_key, &verified_session_key)
            .await
            .unwrap_or(false);
        let persisted = replaced
            || store
                .get(account.id)
                .await
                .ok()
                .flatten()
                .and_then(|material| claude_session_key(&material))
                .as_deref()
                == Some(verified_session_key.as_str());
        let mut retried = self.probe_web_once(account, &verified_session_key).await?;
        if persisted {
            retried.session_token_was_refreshed = true;
        } else if !retried.session_token_was_refreshed
            && retried.succeeded()
            && let Some(snapshot) = retried.snapshot.as_mut()
            && !snapshot
                .source_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.source == "auth.session-key")
        {
            snapshot.source_diagnostics.push(UsageSourceDiagnostic {
                source: "auth.session-key".to_owned(),
                code: UsageAdapterErrorCode::Unknown,
                message: "Claude usage was fetched with the verified replacement session, but secure storage did not confirm saving it; a later refresh may need to re-import the browser session".to_owned(),
                http_status_code: None,
                retry_after_seconds: None,
            });
        }
        Ok(retried)
    }

    async fn probe_web_once(
        &self,
        account: &AccountRecord,
        session_key: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        let initial_session_key = session_key.to_owned();
        let mut session_key = initial_session_key.clone();
        let organizations = self.get("api/organizations", &session_key).await?;
        if !organizations.is_success() {
            return Ok(map_claude_http_error(&organizations, "Claude"));
        }
        update_session_key_from_response(&mut session_key, &organizations);
        let organization_id =
            select_organization(&organizations.body, account.provider_account_id.as_deref())
                .ok_or_else(|| {
                    TransportError::Serialization("Claude organization id was not found".to_owned())
                })?;
        let usage_path = format!("api/organizations/{organization_id}/usage");
        let usage = self.get(&usage_path, &session_key).await?;
        if !usage.is_success() {
            return Ok(map_claude_http_error(&usage, "Claude"));
        }
        update_session_key_from_response(&mut session_key, &usage);
        let root: Value = serde_json::from_str(&usage.body)
            .map_err(|error| TransportError::Serialization(error.to_string()))?;
        let now = Utc::now();
        let five_hour = parse_window(
            root.get("five_hour"),
            UsageWindowKind::Primary,
            "Session",
            now,
        );
        let weekly = parse_window(
            root.get("seven_day"),
            UsageWindowKind::Secondary,
            "Weekly",
            now,
        );
        let usage_primary = five_hour
            .clone()
            .or_else(|| Some(synthetic_session_window()))
            .map(|mut window| {
                window.kind = UsageWindowKind::Primary;
                window
            });
        let secondary = weekly.clone();
        let additional = parse_claude_extra_windows(&root, now);
        let (mut spend, mut credits) = parse_extra_usage(&root);
        if self.fetch_prepaid_credits {
            let overage_path = format!("api/organizations/{organization_id}/overage_spend_limit");
            if let Ok(response) = self.get(&overage_path, &session_key).await {
                update_session_key_from_response(&mut session_key, &response);
                if response.is_success() {
                    spend = parse_overage_spend(&response.body).or(spend);
                }
            }
            let credits_path = format!("api/organizations/{organization_id}/prepaid/credits");
            if let Ok(response) = self.get(&credits_path, &session_key).await {
                update_session_key_from_response(&mut session_key, &response);
                if response.is_success() {
                    credits = parse_prepaid_credits(&response.body).or(credits);
                }
            }
        }
        let primary = usage_primary.or_else(|| spend.as_ref().and_then(spend_limit_window));
        // A response can rotate sessionKey while still returning valid usage.
        // Fetch the account identity on that exceptional path even when
        // optional identity enrichment is disabled; persistence must never be
        // based only on an unverified usage payload.
        let should_fetch_identity =
            self.fetch_account_identity || session_key != initial_session_key;
        let account_info = if should_fetch_identity {
            match self.get("api/account", &session_key).await {
                Ok(response) if response.is_success() => {
                    update_session_key_from_response(&mut session_key, &response);
                    parse_claude_web_account(&response.body, &organization_id)
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(account_info) = account_info.as_ref()
            && !identity_matches(
                account,
                account_info.email.as_deref(),
                [Some(organization_id.as_str())],
            )
        {
            return Ok(account_mismatch(
                "Claude Web session belongs to another account",
            ));
        }
        let identity_verified_for_rotation = account_info
            .as_ref()
            .and_then(|info| info.email.as_deref())
            .is_some_and(|email| email.trim().eq_ignore_ascii_case(&account.email))
            && account
                .provider_account_id
                .as_deref()
                .is_none_or(|expected| expected == organization_id);
        let observed_email = account_info
            .as_ref()
            .and_then(|info| info.email.clone())
            .or_else(|| Some(account.email.clone()));
        let plan_type = account_info.and_then(|info| info.plan_type);
        let mut metrics = Vec::new();
        let primary_metric_key = primary
            .as_ref()
            .is_some_and(|window| window.name == "Spend limit")
            .then_some("spend_limit")
            .unwrap_or("session");
        add_metric(&mut metrics, primary_metric_key, primary.as_ref());
        add_metric(&mut metrics, "weekly", secondary.as_ref());
        for item in &additional {
            add_metric(&mut metrics, &item.key, Some(&item.window));
        }
        if primary.is_none()
            && secondary.is_none()
            && additional.is_empty()
            && spend.is_none()
            && credits.is_none()
        {
            return Ok(invalid_payload("Claude", "no usage lanes were present"));
        }
        let mut source_diagnostics = Vec::new();
        let mut session_token_was_refreshed = false;
        if session_key != initial_session_key {
            if let Some(store) = self.auth_material_store.as_ref() {
                if !identity_verified_for_rotation {
                    source_diagnostics.push(UsageSourceDiagnostic {
                        source: "auth.session-key".to_owned(),
                        code: UsageAdapterErrorCode::Unknown,
                        message: "Claude returned a renewed Web session, but its account identity could not be verified; the saved cookie was left unchanged".to_owned(),
                        http_status_code: None,
                        retry_after_seconds: None,
                    });
                } else {
                    match store
                        .replace_cookie_if_matches(
                            account.id,
                            "sessionKey",
                            &initial_session_key,
                            &session_key,
                        )
                        .await
                    {
                        Ok(replaced) => session_token_was_refreshed = replaced,
                        Err(_) => source_diagnostics.push(UsageSourceDiagnostic {
                            source: "auth.session-key".to_owned(),
                            code: UsageAdapterErrorCode::Unknown,
                            message: "Claude renewed its Web session but the updated cookie could not be saved securely".to_owned(),
                            http_status_code: None,
                            retry_after_seconds: None,
                        }),
                    }
                }
            }
        }
        let primary_kind = primary_window_kind(primary.as_ref());
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: Some(organization_id.clone()),
            plan_type: plan_type.clone(),
            primary,
            primary_window_kind: primary_kind,
            primary_window_is_synthetic: five_hour.is_none(),
            secondary,
            additional_windows: additional,
            credits,
            credit_inventory: None,
            spend,
            observed_email: observed_email.clone(),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics,
            provider_id: CLAUDE.to_owned(),
            source: Some("browser".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        let mut result = UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: observed_email,
                provider_account_id: Some(organization_id),
                plan_type,
            }),
        );
        result.session_token_was_refreshed = session_token_was_refreshed;
        Ok(result)
    }

    async fn probe_admin(
        &self,
        account: &AccountRecord,
        api_key: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        let end = Utc::now();
        let start = end - Duration::days(30);
        let costs = self
            .get_admin(
                "v1/organizations/cost_report",
                api_key,
                start,
                end,
                "description",
            )
            .await?;
        if !costs.is_success() {
            return Ok(map_claude_http_error(&costs, "Claude Admin API"));
        }
        let messages = self
            .get_admin(
                "v1/organizations/usage_report/messages",
                api_key,
                start,
                end,
                "model",
            )
            .await?;
        if !messages.is_success() {
            return Ok(map_claude_http_error(&messages, "Claude Admin API"));
        }
        let mut totals = parse_admin_usage(&costs.body).ok_or_else(|| {
            TransportError::Serialization("Claude Admin API cost report was invalid".to_owned())
        })?;
        if let Some(message_totals) = parse_admin_usage(&messages.body) {
            totals.merge_messages(message_totals);
        }
        let now = Utc::now();
        let mut metrics = Vec::new();
        if let Some(cost_usd) = totals.cost_usd {
            metrics.push(UsageMetric {
                key: "admin-cost-30d".to_owned(),
                name: "API spend (30d)".to_owned(),
                used_percent: None,
                used_amount: Some(cost_usd),
                limit_amount: None,
                remaining_amount: None,
                unit: Some("USD".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata: HashMap::new(),
            });
        }
        if totals.total_tokens > 0 {
            metrics.push(UsageMetric {
                key: "admin-tokens-30d".to_owned(),
                name: "Tokens (30d)".to_owned(),
                used_percent: None,
                used_amount: Some(totals.total_tokens as f64),
                limit_amount: None,
                remaining_amount: None,
                unit: Some("tokens".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata: HashMap::new(),
            });
        }
        for model in totals.models {
            let mut metadata = HashMap::new();
            metadata.insert("input_tokens".to_owned(), model.input_tokens.to_string());
            metadata.insert(
                "cache_creation_tokens".to_owned(),
                model.cache_creation_tokens.to_string(),
            );
            metadata.insert(
                "cache_read_tokens".to_owned(),
                model.cache_read_tokens.to_string(),
            );
            metadata.insert("output_tokens".to_owned(), model.output_tokens.to_string());
            metrics.push(UsageMetric {
                key: format!("admin-model-{}", slugify(&model.name)),
                name: model.name,
                used_percent: None,
                used_amount: Some(model.total_tokens as f64),
                limit_amount: None,
                remaining_amount: None,
                unit: Some("tokens".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata,
            });
        }
        let spend = totals.cost_usd.map(|cost_usd| SpendSnapshot {
            monthly_usage: Some(cost_usd),
            monthly_limit: None,
            used_percent: None,
            limit_enabled: Some(false),
            currency_code: Some("USD".to_owned()),
        });
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: account.provider_account_id.clone(),
            plan_type: Some("Admin API".to_owned()),
            primary: None,
            primary_window_kind: None,
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits: None,
            credit_inventory: None,
            spend,
            observed_email: Some(account.email.clone()),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id: CLAUDE.to_owned(),
            source: Some("admin-api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        Ok(UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: Some(account.email.clone()),
                provider_account_id: account.provider_account_id.clone(),
                plan_type: Some("Admin API".to_owned()),
            }),
        ))
    }

    async fn probe_cli(
        &self,
        account: &AccountRecord,
        options: ClaudeCliProbeOptions,
        session_key: Option<&str>,
    ) -> UsageProbeResult {
        let environment = std::env::vars().collect::<HashMap<_, _>>();
        let binary = match claude_cli::resolve_binary(&environment) {
            Some(binary) => binary,
            None => {
                return map_claude_cli_error(ClaudeCliError::NotInstalled);
            }
        };
        let binary_label = binary.to_string_lossy().to_ascii_lowercase();
        let cache_key = CliCacheKey {
            account_id: account.id,
            binary: binary_label.clone(),
            account_scope: account
                .provider_account_id
                .clone()
                .unwrap_or_else(|| account.email.to_ascii_lowercase()),
            // The Rust adapter currently enriches OAuth directly with Web;
            // CLI web extras stay an explicit future key rather than being
            // silently conflated with this cache entry.
            use_web_extras: self.fetch_web_extras,
            include_prepaid_balance: self.fetch_prepaid_credits,
        };
        if options.use_background_cache {
            if let Some(cached) = self.cli_cached(&cache_key).await {
                return cached;
            }
        }
        if !options.user_initiated {
            if let Some(retry_after_seconds) = self
                .cli_rate_limit_remaining(&binary_label, &environment)
                .await
            {
                return UsageProbeResult::failure(crate::usage::UsageAdapterError {
                    code: crate::usage::UsageAdapterErrorCode::RateLimited,
                    message: "Claude CLI usage endpoint is temporarily rate-limited".to_owned(),
                    http_status_code: None,
                    retry_after_seconds: Some(retry_after_seconds as u64),
                });
            }
            if matches!(
                claude_cli::auth_status(&environment, std::time::Duration::from_secs(5)).await,
                Ok(claude_cli::ClaudeCliAuthStatus::LoggedOut)
            ) {
                return map_claude_cli_error(ClaudeCliError::NotLoggedIn);
            }
        }
        let result = claude_cli::probe(&environment, options).await;
        let result = match result {
            Err(ClaudeCliError::TimedOut) | Err(ClaudeCliError::Parse(_))
                if options.retry_timeout > options.timeout =>
            {
                let mut retry_options = options;
                retry_options.timeout = options.retry_timeout;
                retry_options.retry_timeout = options.retry_timeout;
                claude_cli::probe(&environment, retry_options).await
            }
            result => result,
        };
        let usage = match result {
            Ok(usage) => {
                self.cli_rate_limit_clear(&binary_label, &environment).await;
                usage
            }
            Err(error) => {
                if matches!(error, ClaudeCliError::RateLimited) {
                    self.cli_rate_limit_record(&binary_label, &environment)
                        .await;
                }
                return map_claude_cli_error(error);
            }
        };
        // The CLI reads the machine's global Claude Code login, which is not
        // bound to this account. As an automatic fallback it may only report
        // usage when it proves it is signed in as this same account;
        // otherwise another account's usage would be published here.
        if let Some(rejection) =
            unverified_cli_fallback(self.source_mode, account, usage.observed_email.as_deref())
        {
            return rejection;
        }
        if self.fetch_account_identity && !email_matches(account, usage.observed_email.as_deref()) {
            return account_mismatch("Claude CLI session belongs to another account");
        }
        let now = Utc::now();
        let mut metrics = Vec::new();
        add_metric(&mut metrics, "session", Some(&usage.primary));
        add_metric(&mut metrics, "weekly", usage.secondary.as_ref());
        for item in &usage.additional_windows {
            add_metric(&mut metrics, &item.key, Some(&item.window));
        }
        let observed_email = usage
            .observed_email
            .clone()
            .or_else(|| Some(account.email.clone()));
        // The CLI status panel exposes a display organization, not a stable
        // provider UUID. Do not persist that label as an account id.
        let response_account_id = account.provider_account_id.clone();
        let plan_type = usage.plan_type.clone();
        let result = UsageProbeResult::success(
            UsageSnapshot {
                account_id: account.id,
                observed_at_utc: now,
                response_account_id: response_account_id.clone(),
                plan_type: plan_type.clone(),
                primary: Some(usage.primary),
                primary_window_kind: Some(UsagePrimaryWindowKind::Session),
                primary_window_is_synthetic: false,
                secondary: usage.secondary,
                additional_windows: usage.additional_windows,
                credits: None,
                credit_inventory: None,
                spend: None,
                observed_email: observed_email.clone(),
                is_stale: false,
                stale_reason: None,
                stale_at_utc: None,
                metrics,
                source_diagnostics: Vec::new(),
                provider_id: CLAUDE.to_owned(),
                source: Some("cli".to_owned()),
                data_confidence: "authoritative".to_owned(),
            },
            Some(VerifiedIdentity {
                email: observed_email,
                provider_account_id: response_account_id,
                plan_type,
            }),
        );
        let result = if self.runtime == ClaudeRuntime::App && self.fetch_web_extras {
            self.merge_web_extras(account, result, session_key).await
        } else {
            result
        };
        if options.use_background_cache {
            self.cli_cache_store(cache_key, &result).await;
        }
        result
    }

    async fn cli_rate_limit_remaining(
        &self,
        key: &str,
        environment: &HashMap<String, String>,
    ) -> Option<i64> {
        let now = Utc::now();
        let mut blocked = self.cli_rate_limit_until.lock().await;
        if let Some(until) = blocked.get(key).copied() {
            if until > now {
                return Some((until - now).num_seconds().max(1));
            }
            blocked.remove(key);
        }
        drop(blocked);
        let remaining = claude_cli::persisted_rate_limit_remaining(environment)?;
        self.cli_rate_limit_until
            .lock()
            .await
            .insert(key.to_owned(), now + Duration::seconds(remaining.max(1)));
        Some(remaining.max(1))
    }

    async fn cli_rate_limit_record(&self, key: &str, environment: &HashMap<String, String>) {
        self.cli_rate_limit_until
            .lock()
            .await
            .insert(key.to_owned(), Utc::now() + Duration::minutes(5));
        claude_cli::record_persisted_rate_limit(environment, 5 * 60);
    }

    async fn cli_rate_limit_clear(&self, key: &str, environment: &HashMap<String, String>) {
        self.cli_rate_limit_until.lock().await.remove(key);
        claude_cli::clear_persisted_rate_limit(environment);
    }

    async fn cli_cached(&self, key: &CliCacheKey) -> Option<UsageProbeResult> {
        let now = Utc::now();
        let mut cache = self.cli_cache.lock().await;
        let entry = cache.get(key).cloned()?;
        let expired = (now - entry.recorded_at) >= Duration::minutes(15);
        let reset = entry.result.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .all_rate_windows()
                .any(|window| window.reset_at_utc.is_some_and(|reset| reset <= now))
        });
        if expired || reset {
            cache.remove(key);
            return None;
        }
        Some(entry.result)
    }

    async fn cli_cache_store(&self, key: CliCacheKey, result: &UsageProbeResult) {
        if result.succeeded() {
            self.cli_cache.lock().await.insert(
                key,
                CachedCliResult {
                    recorded_at: Utc::now(),
                    result: result.clone(),
                },
            );
        }
    }

    async fn merge_web_extras(
        &self,
        account: &AccountRecord,
        oauth: UsageProbeResult,
        session_key: Option<&str>,
    ) -> UsageProbeResult {
        let Some(session_key) = session_key else {
            return oauth;
        };
        let Some(mut snapshot) = oauth.snapshot.clone() else {
            return oauth;
        };
        let web = match self.probe_web(account, session_key).await {
            Ok(result) if result.succeeded() => result,
            Ok(_) | Err(_) => return oauth,
        };
        let session_token_was_refreshed =
            oauth.session_token_was_refreshed || web.session_token_was_refreshed;
        let Some(web_snapshot) = web.snapshot else {
            return oauth;
        };
        if snapshot.spend.is_none() {
            snapshot.spend = web_snapshot.spend;
        }
        if snapshot.credits.is_none() {
            snapshot.credits = web_snapshot.credits;
        }
        let mut existing_window_keys = snapshot
            .additional_windows
            .iter()
            .map(|window| window.key.clone())
            .collect::<std::collections::HashSet<_>>();
        for window in web_snapshot.additional_windows {
            if existing_window_keys.insert(window.key.clone()) {
                snapshot.additional_windows.push(window);
            }
        }
        let mut existing_metric_keys = snapshot
            .metrics
            .iter()
            .map(|metric| metric.key.clone())
            .collect::<std::collections::HashSet<_>>();
        for metric in web_snapshot.metrics {
            if existing_metric_keys.insert(metric.key.clone()) {
                snapshot.metrics.push(metric);
            }
        }
        for diagnostic in web_snapshot.source_diagnostics {
            if !snapshot
                .source_diagnostics
                .iter()
                .any(|existing| existing.source == diagnostic.source)
            {
                snapshot.source_diagnostics.push(diagnostic);
            }
        }
        let mut result = UsageProbeResult::success(snapshot, oauth.identity);
        result.session_token_was_refreshed = session_token_was_refreshed;
        result
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

/// The CLI reads the machine's global Claude Code login. As an automatic
/// fallback it is accepted only when it reports this account's email.
fn unverified_cli_fallback(
    source_mode: ClaudeSourceMode,
    account: &AccountRecord,
    cli_email: Option<&str>,
) -> Option<UsageProbeResult> {
    let verified = cli_email.is_some_and(|email| email_matches(account, Some(email)));
    (source_mode == ClaudeSourceMode::Automatic && !verified).then(|| {
        UsageProbeResult::failure(crate::usage::UsageAdapterError {
            code: UsageAdapterErrorCode::AuthenticationUnavailable,
            message: "Claude CLI is not verified as signed in to this account".to_owned(),
            http_status_code: None,
            retry_after_seconds: None,
        })
    })
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

async fn probe_oauth_with_scope(
    adapter: &ClaudeUsageAdapter,
    account: &AccountRecord,
    access_token: &str,
    material: &crate::auth::AccountAuthMaterial,
    session_key: Option<&str>,
) -> Result<UsageProbeResult, TransportError> {
    if !material.oauth_scopes.is_empty()
        && !material
            .oauth_scopes
            .iter()
            .any(|scope| scope.eq_ignore_ascii_case("user:profile"))
    {
        return Ok(UsageProbeResult::failure(crate::usage::UsageAdapterError {
            code: crate::usage::UsageAdapterErrorCode::Forbidden,
            message: "Claude OAuth token is missing the required user:profile scope".to_owned(),
            http_status_code: Some(403),
            retry_after_seconds: None,
        }));
    }
    let oauth = adapter.probe_oauth(account, access_token).await?;
    if !oauth.succeeded() || !adapter.fetch_web_extras {
        return Ok(oauth);
    }
    Ok(adapter.merge_web_extras(account, oauth, session_key).await)
}

fn select_organization(body: &str, requested_id: Option<&str>) -> Option<String> {
    let root: Value = serde_json::from_str(body).ok()?;
    let organizations = root
        .get("organizations")
        .or_else(|| root.as_array().map(|_| &root))?
        .as_array()?;
    let candidates = organizations
        .iter()
        .filter_map(|item| {
            let id = json_string(item, &["uuid", "id"])?;
            let capabilities = item.get("capabilities").and_then(Value::as_array);
            let (has_chat_capability, is_not_api_only) = if let Some(capabilities) = capabilities {
                let capabilities = capabilities
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_ascii_lowercase)
                    .collect::<Vec<_>>();
                let has_chat_capability = capabilities.iter().any(|value| value == "chat");
                let is_api_only =
                    !capabilities.is_empty() && capabilities.iter().all(|value| value == "api");
                (has_chat_capability, !is_api_only)
            } else {
                (
                    item.get("has_chat_capability")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    !item
                        .get("is_api_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
            };
            Some((id, has_chat_capability, is_not_api_only))
        })
        .collect::<Vec<_>>();
    if let Some(requested) = requested_id.map(str::trim).filter(|id| !id.is_empty()) {
        return candidates
            .iter()
            .find(|(id, _, _)| id == requested)
            .map(|(id, _, _)| id.clone());
    }

    candidates
        .iter()
        .find(|(_, has_chat, _)| *has_chat)
        .map(|(id, _, _)| id.clone())
        .or_else(|| {
            candidates
                .iter()
                .find(|(_, _, is_not_api_only)| *is_not_api_only)
                .map(|(id, _, _)| id.clone())
        })
        .or_else(|| candidates.first().map(|(id, _, _)| id.clone()))
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

#[derive(Debug, Clone)]
struct ClaudeProfile {
    account_id: Option<String>,
    organization_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
}

/// Fetches the identity associated with a Claude Code OAuth access token.
///
/// This is intentionally separate from [`ClaudeUsageAdapter::probe`]: a host
/// needs the verified identity before it can create the durable account record
/// that will be used by the normal refresh runtime. The endpoint and headers
/// are the same ones used by the OAuth usage adapter; no browser cookies or
/// WebView state are involved.
pub async fn fetch_oauth_identity(
    transport: &dyn UsageHttpTransport,
    access_token: &str,
) -> Result<VerifiedIdentity, TransportError> {
    let url = Url::parse("https://api.anthropic.com/api/oauth/profile")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let response = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url,
            headers: [
                ("Accept".to_owned(), "application/json".to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("Authorization".to_owned(), format!("Bearer {access_token}")),
                ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
                ("User-Agent".to_owned(), "claude-code/2.1.0".to_owned()),
            ]
            .into_iter()
            .collect(),
            body: None,
        })
        .await?;
    if !response.is_success() {
        return Err(TransportError::Serialization(format!(
            "Claude OAuth profile returned HTTP {}",
            response.status_code
        )));
    }
    let profile = parse_claude_profile(&response.body).ok_or_else(|| {
        TransportError::Serialization(
            "Claude OAuth profile did not contain an account identity".to_owned(),
        )
    })?;
    if profile.email.is_none() && profile.account_id.is_none() && profile.organization_id.is_none()
    {
        return Err(TransportError::Serialization(
            "Claude OAuth profile did not contain an account identity".to_owned(),
        ));
    }
    Ok(VerifiedIdentity {
        email: profile.email,
        provider_account_id: profile.organization_id.or(profile.account_id),
        plan_type: profile.plan_type,
    })
}

/// Fetches the identity associated with a Claude Web `sessionKey` cookie.
///
/// This is the Web-session counterpart to [`fetch_oauth_identity`]. It is
/// used by the user-driven browser login bridge before an account record is
/// created, so the account can be bound to the organization that supplied the
/// cookie rather than to a placeholder email.
pub async fn fetch_web_identity(
    transport: &dyn UsageHttpTransport,
    session_key: &str,
) -> Result<VerifiedIdentity, TransportError> {
    let (identity, _) = fetch_web_identity_for_account(transport, session_key, None).await?;
    Ok(identity)
}

async fn fetch_web_identity_for_account(
    transport: &dyn UsageHttpTransport,
    session_key: &str,
    expected_organization_id: Option<&str>,
) -> Result<(VerifiedIdentity, String), TransportError> {
    let mut session_key = session_key.trim().to_owned();
    if !session_key.starts_with("sk-ant-") || session_key.len() <= "sk-ant-".len() {
        return Err(TransportError::Serialization(
            "Claude Web session key is missing or invalid".to_owned(),
        ));
    }
    let base_url = Url::parse("https://claude.ai/")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let headers = |session_key: &str| {
        [
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Cookie".to_owned(), format!("sessionKey={session_key}")),
            ("User-Agent".to_owned(), "UsageMonitor/0.1".to_owned()),
        ]
        .into_iter()
        .collect()
    };

    let organizations_url = base_url
        .join("api/organizations")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let organizations = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url: organizations_url,
            headers: headers(&session_key),
            body: None,
        })
        .await?;
    if !organizations.is_success() {
        return Err(TransportError::Serialization(format!(
            "Claude Web organizations returned HTTP {}",
            organizations.status_code
        )));
    }
    update_session_key_from_response(&mut session_key, &organizations);
    let organization_id = select_organization(&organizations.body, expected_organization_id)
        .or_else(|| select_organization(&organizations.body, None))
        .ok_or_else(|| {
            TransportError::Serialization(
                "Claude Web organizations did not contain an organization id".to_owned(),
            )
        })?;

    let account_url = base_url
        .join("api/account")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let account = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url: account_url,
            headers: headers(&session_key),
            body: None,
        })
        .await?;
    if !account.is_success() {
        return Err(TransportError::Serialization(format!(
            "Claude Web account returned HTTP {}",
            account.status_code
        )));
    }
    update_session_key_from_response(&mut session_key, &account);
    let account = parse_claude_web_account(&account.body, &organization_id).ok_or_else(|| {
        TransportError::Serialization(
            "Claude Web account did not contain an account identity".to_owned(),
        )
    })?;
    if account.email.is_none() {
        return Err(TransportError::Serialization(
            "Claude Web account did not contain an email address".to_owned(),
        ));
    }
    Ok((
        VerifiedIdentity {
            email: account.email,
            provider_account_id: Some(organization_id),
            plan_type: account.plan_type,
        },
        session_key,
    ))
}

fn parse_claude_profile(body: &str) -> Option<ClaudeProfile> {
    let root: Value = serde_json::from_str(body).ok()?;
    let account = root.get("account");
    let organization = root.get("organization");
    Some(ClaudeProfile {
        account_id: account
            .and_then(|value| json_string(value, &["uuid", "id"]))
            .or_else(|| json_string(&root, &["accountUuid", "account_uuid"])),
        organization_id: organization
            .and_then(|value| json_string(value, &["uuid", "id"]))
            .or_else(|| json_string(&root, &["organizationUuid", "organization_uuid"])),
        email: account
            .and_then(|value| json_string(value, &["emailAddress", "email_address", "email"]))
            .or_else(|| json_string(&root, &["emailAddress", "email_address", "email"])),
        plan_type: claude_profile_plan_type(account, organization),
    })
}

/// The OAuth profile names the subscription in
/// `organization.organization_type` (for example `claude_pro`, `claude_max`),
/// with the Max multiplier in `rate_limit_tier` and the Team seat in
/// `seat_tier`. `account.has_claude_max`/`has_claude_pro` are the fallback.
fn claude_profile_plan_type(
    account: Option<&Value>,
    organization: Option<&Value>,
) -> Option<String> {
    let field = |names: &[&str]| organization.and_then(|value| json_string(value, names));
    let organization_type = field(&["organization_type", "organizationType"]);
    let rate_limit_tier = field(&["rate_limit_tier", "rateLimitTier"]);
    let seat_tier = field(&["seat_tier", "seatTier"]);
    claude_plan_label(
        organization_type.as_deref(),
        rate_limit_tier.as_deref(),
        None,
        seat_tier.as_deref(),
    )
    .or_else(|| {
        let flag = |name: &str| {
            account
                .and_then(|value| value.get(name))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        if flag("has_claude_max") {
            Some(claude_plan_label(
                Some("max"),
                rate_limit_tier.as_deref(),
                None,
                None,
            )?)
        } else if flag("has_claude_pro") {
            Some("Claude Pro".to_owned())
        } else {
            None
        }
    })
}

#[derive(Debug, Clone)]
struct ClaudeWebAccount {
    email: Option<String>,
    plan_type: Option<String>,
}

fn parse_claude_web_account(body: &str, organization_id: &str) -> Option<ClaudeWebAccount> {
    let root: Value = serde_json::from_str(body).ok()?;
    let email = json_string(&root, &["email_address", "emailAddress", "email"]);
    let membership = root
        .get("memberships")
        .and_then(Value::as_array)
        .and_then(|memberships| {
            memberships.iter().find(|membership| {
                membership
                    .get("organization")
                    .and_then(|organization| json_string(organization, &["uuid", "id"]))
                    .is_some_and(|id| id == organization_id)
            })
        })
        .or_else(|| root.get("memberships").and_then(Value::as_array)?.first());
    let organization = membership.and_then(|value| value.get("organization"));
    let rate_limit_tier =
        organization.and_then(|value| json_string(value, &["rate_limit_tier", "rateLimitTier"]));
    let billing_type =
        organization.and_then(|value| json_string(value, &["billing_type", "billingType"]));
    let seat_tier = membership.and_then(|value| json_string(value, &["seat_tier", "seatTier"]));
    Some(ClaudeWebAccount {
        email,
        plan_type: claude_plan_label(
            None,
            rate_limit_tier.as_deref(),
            billing_type.as_deref(),
            seat_tier.as_deref(),
        ),
    })
}

fn claude_oauth_plan_type(root: &Value) -> Option<String> {
    let subscription = json_string(root, &["subscriptionType", "subscription_type"]);
    let rate_limit_tier = json_string(root, &["rate_limit_tier", "rateLimitTier"]);
    claude_plan_label(
        subscription.as_deref(),
        rate_limit_tier.as_deref(),
        None,
        None,
    )
    .or_else(|| json_string(root, &["plan"]))
}

fn claude_plan_label(
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
    billing_type: Option<&str>,
    seat_tier: Option<&str>,
) -> Option<String> {
    let combined = [subscription_type, rate_limit_tier, billing_type]
        .into_iter()
        .flatten()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if combined.contains("max") {
        let multiplier = rate_limit_tier.and_then(max_usage_multiplier);
        return Some(match multiplier {
            Some(multiplier) => format!("Claude Max {multiplier}"),
            None => "Claude Max".to_owned(),
        });
    }
    let label = if combined.contains("pro") {
        Some("Claude Pro".to_owned())
    } else if combined.contains("team") {
        match seat_tier.map(|value| value.to_ascii_lowercase()).as_deref() {
            Some("team_standard") => Some("Claude Team Standard".to_owned()),
            Some("team_tier_1") => Some("Claude Team Premium".to_owned()),
            _ => Some("Claude Team".to_owned()),
        }
    } else if combined.contains("enterprise") {
        Some("Claude Enterprise".to_owned())
    } else if combined.contains("ultra") {
        Some("Claude Ultra".to_owned())
    } else {
        None
    };
    label
}

fn max_usage_multiplier(rate_limit_tier: &str) -> Option<String> {
    let words = rate_limit_tier
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let max_index = words.iter().position(|word| *word == "max")?;
    let multiplier = words.get(max_index + 1)?.to_owned();
    (multiplier.ends_with('x') && multiplier[..multiplier.len() - 1].parse::<u32>().is_ok())
        .then_some(multiplier)
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

fn map_claude_cli_error(error: ClaudeCliError) -> UsageProbeResult {
    let (code, retry_after_seconds) = match error {
        ClaudeCliError::NotInstalled | ClaudeCliError::NotLoggedIn => (
            crate::usage::UsageAdapterErrorCode::AuthenticationUnavailable,
            None,
        ),
        ClaudeCliError::RateLimited => {
            (crate::usage::UsageAdapterErrorCode::RateLimited, Some(300))
        }
        ClaudeCliError::TimedOut | ClaudeCliError::ProcessExited | ClaudeCliError::Launch(_) => {
            (crate::usage::UsageAdapterErrorCode::TransientHttp, None)
        }
        ClaudeCliError::OutputTooLarge | ClaudeCliError::Parse(_) => {
            (crate::usage::UsageAdapterErrorCode::InvalidPayload, None)
        }
    };
    UsageProbeResult::failure(crate::usage::UsageAdapterError {
        code,
        message: error.to_string(),
        http_status_code: None,
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

fn oauth_is_account_boundary(result: &UsageProbeResult) -> bool {
    result
        .error
        .as_ref()
        .is_some_and(|error| error.code == crate::usage::UsageAdapterErrorCode::AccountMismatch)
}

fn oauth_token_key(access_token: &str) -> String {
    let digest = Sha256::digest(access_token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Default)]
struct AdminUsageTotals {
    cost_usd: Option<f64>,
    total_tokens: u64,
    models: Vec<AdminModelTotals>,
}

#[derive(Debug)]
struct AdminModelTotals {
    name: String,
    input_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

impl AdminUsageTotals {
    fn merge_messages(&mut self, messages: Self) {
        self.total_tokens = self.total_tokens.saturating_add(messages.total_tokens);
        self.models.extend(messages.models);
    }
}

fn parse_admin_usage(body: &str) -> Option<AdminUsageTotals> {
    let root: Value = serde_json::from_str(body).ok()?;
    let buckets = root.get("data")?.as_array()?;
    let mut totals = AdminUsageTotals::default();
    let mut models = HashMap::<String, AdminModelTotals>::new();
    for bucket in buckets {
        // An empty day bucket must not invalidate the whole 30-day report.
        let Some(results) = bucket.get("results").and_then(Value::as_array) else {
            continue;
        };
        for result in results {
            if let Some(amount) = json_number(result, &["amount"]) {
                totals.cost_usd = Some(totals.cost_usd.unwrap_or_default() + amount / 100.0);
            }
            let input = json_u64(result, &["uncached_input_tokens"]);
            let cache_creation = result
                .get("cache_creation")
                .map(|value| {
                    json_u64(value, &["ephemeral_1h_input_tokens"])
                        .saturating_add(json_u64(value, &["ephemeral_5m_input_tokens"]))
                })
                .unwrap_or_default();
            let cache_read = json_u64(result, &["cache_read_input_tokens"]);
            let output = json_u64(result, &["output_tokens"]);
            let total = input
                .saturating_add(cache_creation)
                .saturating_add(cache_read)
                .saturating_add(output);
            if total > 0 {
                totals.total_tokens = totals.total_tokens.saturating_add(total);
                let name =
                    json_string(result, &["model"]).unwrap_or_else(|| "Claude API".to_owned());
                let model = models
                    .entry(name.clone())
                    .or_insert_with(|| AdminModelTotals {
                        name,
                        input_tokens: 0,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        output_tokens: 0,
                        total_tokens: 0,
                    });
                model.input_tokens = model.input_tokens.saturating_add(input);
                model.cache_creation_tokens =
                    model.cache_creation_tokens.saturating_add(cache_creation);
                model.cache_read_tokens = model.cache_read_tokens.saturating_add(cache_read);
                model.output_tokens = model.output_tokens.saturating_add(output);
                model.total_tokens = model.total_tokens.saturating_add(total);
            }
        }
    }
    totals.models = models.into_values().collect();
    Some(totals)
}

fn json_u64(value: &Value, names: &[&str]) -> u64 {
    names
        .iter()
        .find_map(|name| {
            value.get(*name).and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
                    .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
            })
        })
        .unwrap_or_default()
}

fn normalize_claude_admin_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let token = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim();
    token
        .to_ascii_lowercase()
        .starts_with("sk-ant-admin")
        .then_some(token.to_owned())
}

fn normalize_claude_oauth_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let token = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim();
    token
        .to_ascii_lowercase()
        .starts_with("sk-ant-oat")
        .then_some(token.to_owned())
}

fn claude_session_key(material: &crate::auth::AccountAuthMaterial) -> Option<String> {
    material
        .cookies
        .iter()
        .find(|cookie| cookie.name.eq_ignore_ascii_case("sessionKey"))
        .map(|cookie| cookie.value.trim().to_owned())
        .filter(|value| value.starts_with("sk-ant-") && value.len() > "sk-ant-".len())
}

fn update_session_key_from_response(
    session_key: &mut String,
    response: &crate::transport::UsageHttpResponse,
) {
    if let Some(renewed) = rotated_claude_session_key(response) {
        *session_key = renewed;
    }
}

fn rotated_claude_session_key(response: &crate::transport::UsageHttpResponse) -> Option<String> {
    if response.status_code != 200 {
        return None;
    }
    let set_cookie = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, value)| value)?;

    let mut latest_session_key = None;
    for header_line in set_cookie.lines() {
        let bytes = header_line.as_bytes();
        let cookie_name = b"sessionKey=";
        if bytes.len() < cookie_name.len() {
            continue;
        }
        for start in 0..=bytes.len() - cookie_name.len() {
            if !bytes[start..start + cookie_name.len()].eq_ignore_ascii_case(cookie_name) {
                continue;
            }
            let boundary = header_line[..start].trim_end();
            if !boundary.is_empty() && !boundary.ends_with(',') {
                continue;
            }
            let value_start = start + cookie_name.len();
            let value_end = header_line[value_start..]
                .find([';', ',', '\r', '\n'])
                .map_or(header_line.len(), |offset| value_start + offset);
            let candidate = header_line[value_start..value_end].trim();
            if candidate.starts_with("sk-ant-") && !candidate.chars().any(char::is_whitespace) {
                latest_session_key = Some(candidate.to_owned());
            }
        }
    }
    latest_session_key
}

struct OAuthLimitWindow {
    key: String,
    name: String,
    window: RateLimitWindow,
}

fn parse_claude_extra_windows(
    root: &Value,
    now: chrono::DateTime<Utc>,
) -> Vec<AdditionalRateLimitWindow> {
    let mut windows = Vec::new();
    for (key, name) in [
        ("seven_day_sonnet", "Sonnet weekly"),
        ("seven_day_opus", "Opus weekly"),
        ("seven_day_oauth_apps", "OAuth apps weekly"),
        ("iguana_necktie", "Additional weekly"),
    ] {
        if let Some(window) = parse_window(root.get(key), UsageWindowKind::Additional, name, now) {
            windows.push(AdditionalRateLimitWindow {
                key: key.to_owned(),
                name: name.to_owned(),
                window,
            });
        }
    }

    let routine_keys = [
        "seven_day_routines",
        "seven_day_claude_routines",
        "claude_routines",
        "routines",
        "routine",
        "seven_day_cowork",
        "cowork",
    ];
    if let Some((key, window)) = routine_keys.iter().find_map(|key| {
        parse_window(
            root.get(*key),
            UsageWindowKind::Additional,
            "Daily Routines",
            now,
        )
        .map(|window| (*key, window))
    }) {
        windows.push(AdditionalRateLimitWindow {
            key: key.to_owned(),
            name: "Daily Routines".to_owned(),
            window,
        });
    }

    for limit in parse_oauth_limits(root.get("limits"), now) {
        if !windows.iter().any(|existing| existing.key == limit.key) {
            windows.push(AdditionalRateLimitWindow {
                key: limit.key,
                name: limit.name,
                window: limit.window,
            });
        }
    }
    windows
}

fn parse_oauth_limits(value: Option<&Value>, now: chrono::DateTime<Utc>) -> Vec<OAuthLimitWindow> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            // Deliberately do not filter `is_active`: observed
            // enforceable scoped limits can report false. The stable shape is
            // group=weekly + kind=weekly_scoped.
            if !json_string(entry, &["group"])
                .is_some_and(|group| group.eq_ignore_ascii_case("weekly"))
                || !json_string(entry, &["kind"])
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("weekly_scoped"))
            {
                return None;
            }
            let percent = json_number(entry, &["percent", "utilization", "used_percent"])?;
            let reset_at = reset_at(entry, now);
            let model = entry
                .get("scope")
                .and_then(|scope| scope.get("model"))
                .and_then(Value::as_object)?;
            let model_name = json_string(
                &Value::Object(model.clone()),
                &["display_name", "displayName"],
            )?;
            let model_id = json_string(&Value::Object(model.clone()), &["id"]);
            if is_all_models_scope(model_id.as_deref(), &model_name) {
                return None;
            }
            let identity = model_id.as_deref().unwrap_or(&model_name);
            let slug = slugify(identity);
            if slug.is_empty() {
                return None;
            }
            let window_name = model_name.clone();
            Some(OAuthLimitWindow {
                key: format!("claude-weekly-scoped-{slug}"),
                name: model_name,
                window: RateLimitWindow {
                    kind: UsageWindowKind::Additional,
                    name: window_name,
                    used_percent: normalize_percent(percent),
                    reset_at_utc: reset_at,
                    limit_window_seconds: 7 * 24 * 60 * 60,
                },
            })
        })
        .collect()
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    slug.trim_matches('-').to_owned()
}

fn is_all_models_scope(model_id: Option<&str>, model_name: &str) -> bool {
    let name = slugify(model_name);
    if name == "all-models" {
        return true;
    }
    model_id
        .map(slugify)
        .is_some_and(|id| id == "all-models" || id.ends_with("-all-models"))
}

/// Maps Claude's usage-limit reset grants (`cedar_ember`) to the shared reset
/// credit inventory. Each remaining reset of a grant becomes one credit, so a
/// grant with two resets left lists twice, as it does in Claude's own UI.
fn parse_claude_reset_grants(
    root: &Value,
    now: chrono::DateTime<Utc>,
) -> Option<UsageCreditInventory> {
    let block = root.get("cedar_ember").filter(|block| block.is_object())?;
    let grants = block.get("grants").and_then(Value::as_array)?;
    let mut credits = Vec::new();
    let mut available_count = 0_u32;
    for grant in grants {
        let resets_left = grant
            .get("resets_left")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u64::from(u8::MAX)) as u32;
        let expires_at_utc = claude_grant_time(grant, "ends_at");
        if resets_left == 0 || expires_at_utc.is_some_and(|expires_at| expires_at <= now) {
            continue;
        }
        let paused = grant.get("paused").and_then(Value::as_bool) == Some(true);
        let clears = grant
            .get("clears")
            .and_then(Value::as_array)
            .map(|clears| clears.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let (reset_type, title) = if clears.contains(&"seven_day") {
            ("full", "Full reset")
        } else if clears.contains(&"five_hour") {
            ("five_hour", "5-hour reset")
        } else {
            ("reset", "Usage-limit reset")
        };
        if !paused {
            available_count = available_count.saturating_add(resets_left);
        }
        for _ in 0..resets_left {
            credits.push(UsageCreditRecord {
                id: json_string(grant, &["id"]),
                reset_type: Some(reset_type.to_owned()),
                status: Some(if paused { "paused" } else { "available" }.to_owned()),
                granted_at_utc: claude_grant_time(grant, "starts_at"),
                expires_at_utc,
                redeem_started_at_utc: None,
                redeemed_at_utc: None,
                title: Some(title.to_owned()),
                description: json_string(grant, &["label"]),
            });
        }
    }
    Some(UsageCreditInventory {
        available_count,
        credits,
    })
}

fn claude_grant_time(grant: &Value, key: &str) -> Option<chrono::DateTime<Utc>> {
    grant
        .get(key)
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn parse_extra_usage(root: &Value) -> (Option<SpendSnapshot>, Option<CreditsSnapshot>) {
    let Some(extra) = root.get("extra_usage") else {
        return (None, None);
    };
    if extra
        .get("is_enabled")
        .and_then(Value::as_bool)
        .is_some_and(|enabled| !enabled)
    {
        return (None, None);
    }
    let used = json_number(extra, &["used_credits", "usedCredits"]);
    let limit = json_number(
        extra,
        &["monthly_limit", "monthly_credit_limit", "monthlyLimit"],
    );
    // The provider's own implementation treats an incomplete pair as
    // unusable rather than displaying a misleading partial spend balance.
    let (Some(used), Some(limit)) = (
        used.filter(|value| value.is_finite() && *value >= 0.0),
        limit.filter(|value| value.is_finite() && *value > 0.0),
    ) else {
        return (None, None);
    };
    // Claude reports extra-usage amounts in cents. Normalize to major units
    // before exposing them through the provider-neutral spend contract.
    let used = used / 100.0;
    let limit = limit / 100.0;
    if limit <= 0.0 {
        return (None, None);
    }
    let currency_code = normalized_claude_currency(json_string(extra, &["currency"]))
        .unwrap_or_else(|| "USD".to_owned());
    let percent = json_number(extra, &["utilization", "used_percent", "usedPercent"])
        .filter(|value| value.is_finite())
        .map(normalize_percent)
        .or_else(|| {
            let percent = used / limit * 100.0;
            percent.is_finite().then(|| normalize_percent(percent))
        });
    (
        Some(SpendSnapshot {
            monthly_usage: Some(used),
            monthly_limit: Some(limit),
            used_percent: percent,
            limit_enabled: Some(true),
            currency_code: Some(currency_code),
        }),
        None,
    )
}

fn parse_overage_spend(body: &str) -> Option<SpendSnapshot> {
    let root: Value = serde_json::from_str(body).ok()?;
    if root.get("is_enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let used = json_number(&root, &["used_credits", "usedCredits"])
        .filter(|value| value.is_finite() && *value >= 0.0)?;
    let limit = json_number(
        &root,
        &["monthly_credit_limit", "monthly_limit", "monthlyLimit"],
    )
    .filter(|value| value.is_finite() && *value > 0.0)?;
    let currency_code = normalized_claude_currency(json_string(&root, &["currency"]))?;
    let used = used / 100.0;
    let limit = limit / 100.0;
    if limit <= 0.0 {
        return None;
    }
    let used_percent = json_number(&root, &["utilization", "used_percent", "usedPercent"])
        .filter(|value| value.is_finite())
        .map(normalize_percent)
        .or_else(|| {
            let percent = used / limit * 100.0;
            percent.is_finite().then(|| normalize_percent(percent))
        });
    Some(SpendSnapshot {
        monthly_usage: Some(used),
        monthly_limit: Some(limit),
        used_percent,
        limit_enabled: Some(true),
        currency_code: Some(currency_code),
    })
}

fn parse_prepaid_credits(body: &str) -> Option<CreditsSnapshot> {
    let root: Value = serde_json::from_str(body).ok()?;
    let amount = json_number(&root, &["amount", "balance", "remaining"])
        .filter(|value| value.is_finite() && *value >= 0.0)?;
    let currency_code = normalized_claude_currency(json_string(&root, &["currency"]))?;
    Some(CreditsSnapshot {
        has_credits: Some(true),
        unlimited: Some(false),
        balance: Some(amount / 100.0),
        currency_code: Some(currency_code),
        approximate_message_cost: None,
        limit: None,
        balance_read_succeeded: Some(true),
        credits_available: Some(amount > 0.0),
    })
}

fn normalized_claude_currency(value: Option<String>) -> Option<String> {
    let value = value?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_ascii_uppercase())
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
mod tests {
    use super::*;

    #[test]
    fn oauth_weekly_lane_is_selected_when_five_hour_is_missing() {
        let root: Value = serde_json::json!({
            "five_hour": null,
            "seven_day": {
                "utilization": 41,
                "resets_at": "2030-01-02T00:00:00Z"
            },
            "seven_day_cowork": {
                "utilization": 9,
                "resets_at": "2030-01-03T00:00:00Z"
            },
            "limits": [{
                "kind": "weekly_scoped",
                "group": "weekly",
                "percent": 12,
                "scope": {
                    "model": {
                        "id": "fable",
                        "display_name": "Fable"
                    }
                }
            }]
        });
        let now = Utc::now();
        let five_hour = parse_window(
            root.get("five_hour"),
            UsageWindowKind::Primary,
            "Session",
            now,
        );
        let weekly = parse_window(
            root.get("seven_day"),
            UsageWindowKind::Secondary,
            "Weekly",
            now,
        );
        let primary = five_hour.clone().or_else(|| weekly.clone());
        let secondary = weekly.clone();
        assert_eq!(primary.unwrap().used_percent, 41.0);
        assert_eq!(
            primary_window_kind(secondary.as_ref()),
            Some(UsagePrimaryWindowKind::Weekly)
        );
        let secondary = secondary.unwrap();
        assert_eq!(secondary.used_percent, 41.0);
        assert_eq!(secondary.limit_window_seconds, 7 * 24 * 60 * 60);

        let additional = parse_claude_extra_windows(&root, now);
        assert_eq!(additional.len(), 2);
        assert_eq!(additional[0].name, "Daily Routines");
        assert_eq!(additional[1].key, "claude-weekly-scoped-fable");
    }

    #[test]
    fn web_missing_five_hour_uses_a_synthetic_session_lane() {
        let primary = synthetic_session_window();
        assert_eq!(primary.name, "Session");
        assert_eq!(primary.used_percent, 0.0);
        assert_eq!(primary.reset_at_utc, None);
        assert_eq!(
            primary_window_kind(Some(&primary)),
            Some(UsagePrimaryWindowKind::Session)
        );
    }

    #[test]
    fn spend_limit_becomes_primary_only_when_no_usage_lane_exists() {
        let root = serde_json::json!({
            "extra_usage": {
                "is_enabled": true,
                "used_credits": 250,
                "monthly_limit": 1000,
                "utilization": 25
            }
        });
        let (spend, _) = parse_extra_usage(&root);
        let primary = spend.as_ref().and_then(spend_limit_window).unwrap();
        assert_eq!(primary.name, "Spend limit");
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(spend.unwrap().monthly_usage, Some(2.5));
    }

    #[test]
    fn extra_usage_rejects_disabled_or_invalid_values_and_preserves_currency() {
        let root = serde_json::json!({
            "extra_usage": {
                "is_enabled": true,
                "used_credits": 1250,
                "monthly_limit": 5000,
                "currency": " usd ",
                "utilization": 25
            }
        });
        let (spend, _) = parse_extra_usage(&root);
        let spend = spend.unwrap();
        assert_eq!(spend.monthly_usage, Some(12.5));
        assert_eq!(spend.monthly_limit, Some(50.0));
        assert_eq!(spend.currency_code.as_deref(), Some("USD"));

        let disabled = serde_json::json!({
            "extra_usage": { "is_enabled": false, "used_credits": 1, "monthly_limit": 100 }
        });
        assert!(parse_extra_usage(&disabled).0.is_none());

        let invalid = serde_json::json!({
            "extra_usage": { "used_credits": "NaN", "monthly_limit": 100 }
        });
        assert!(parse_extra_usage(&invalid).0.is_none());

        let negative = serde_json::json!({
            "extra_usage": { "used_credits": -1, "monthly_limit": 100 }
        });
        assert!(parse_extra_usage(&negative).0.is_none());
    }

    #[test]
    fn overage_spend_requires_enabled_state_currency_and_finite_amounts() {
        let valid = r#"{"is_enabled":true,"used_credits":125,"monthly_credit_limit":1000,"currency":"USD"}"#;
        let spend = parse_overage_spend(valid).unwrap();
        assert_eq!(spend.monthly_usage, Some(1.25));
        assert_eq!(spend.monthly_limit, Some(10.0));
        assert_eq!(spend.currency_code.as_deref(), Some("USD"));

        assert!(parse_overage_spend(
            r#"{"is_enabled":false,"used_credits":125,"monthly_credit_limit":1000,"currency":"USD"}"#
        )
        .is_none());
        assert!(
            parse_overage_spend(
                r#"{"is_enabled":true,"used_credits":125,"monthly_credit_limit":1000}"#
            )
            .is_none()
        );
        assert!(parse_overage_spend(
            r#"{"is_enabled":true,"used_credits":"NaN","monthly_credit_limit":1000,"currency":"USD"}"#
        )
        .is_none());
    }

    #[test]
    fn prepaid_credits_requires_a_valid_amount_and_currency() {
        let credits = parse_prepaid_credits(r#"{"amount":0,"currency":"usd"}"#).unwrap();
        assert_eq!(credits.balance, Some(0.0));
        assert_eq!(credits.currency_code.as_deref(), Some("USD"));
        assert_eq!(credits.credits_available, Some(false));

        assert!(parse_prepaid_credits(r#"{"amount":100}"#).is_none());
        assert!(parse_prepaid_credits(r#"{"amount":-1,"currency":"USD"}"#).is_none());
        assert!(parse_prepaid_credits(r#"{"amount":"Infinity","currency":"USD"}"#).is_none());
    }

    #[test]
    fn organization_selection_prefers_chat_capability_arrays() {
        let organizations = r#"[
            {"uuid":"api-org","capabilities":["api"]},
            {"uuid":"chat-org","capabilities":["chat"]}
        ]"#;
        assert_eq!(
            select_organization(organizations, None).as_deref(),
            Some("chat-org")
        );
    }

    #[test]
    fn organization_selection_honors_bound_org_and_supports_legacy_flags() {
        let organizations = r#"[
            {"uuid":"api-org","capabilities":["api"]},
            {"uuid":"legacy-chat-org","has_chat_capability":true,"is_api_only":false}
        ]"#;
        assert_eq!(
            select_organization(organizations, Some("api-org")).as_deref(),
            Some("api-org")
        );
        assert_eq!(
            select_organization(organizations, None).as_deref(),
            Some("legacy-chat-org")
        );
    }

    #[test]
    fn organization_selection_does_not_fallback_when_bound_org_is_missing() {
        let organizations = r#"[
            {"uuid":"another-chat-org","capabilities":["chat"]}
        ]"#;
        assert_eq!(
            select_organization(organizations, Some("missing-org")),
            None
        );
        assert_eq!(
            select_organization(organizations, Some(" another-chat-org ")).as_deref(),
            Some("another-chat-org")
        );
    }

    #[test]
    fn claude_profile_accepts_nested_account_and_organization_shape() {
        let profile = parse_claude_profile(
            r#"{"account":{"uuid":"acct-1","email_address":"user@example.com"},"organization":{"uuid":"org-1"}}"#,
        )
        .unwrap();
        assert_eq!(profile.account_id.as_deref(), Some("acct-1"));
        assert_eq!(profile.organization_id.as_deref(), Some("org-1"));
        assert_eq!(profile.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn admin_usage_parser_normalizes_cost_and_model_tokens() {
        let costs = parse_admin_usage(
            r#"{"data":[{"starting_at":"2030-01-01T00:00:00Z","ending_at":"2030-01-02T00:00:00Z","results":[{"amount":"1250","description":"Claude"}]}]}"#,
        )
        .unwrap();
        assert_eq!(costs.cost_usd, Some(12.5));

        let messages = parse_admin_usage(
            r#"{"data":[{"starting_at":"2030-01-01T00:00:00Z","ending_at":"2030-01-02T00:00:00Z","results":[{"uncached_input_tokens":10,"cache_creation":{"ephemeral_5m_input_tokens":2},"cache_read_input_tokens":3,"output_tokens":5,"model":"claude-sonnet"}]}]}"#,
        )
        .unwrap();
        assert_eq!(messages.total_tokens, 20);
        assert_eq!(messages.models[0].name, "claude-sonnet");
    }

    #[test]
    fn web_session_key_requires_a_claude_session_cookie() {
        let invalid = crate::auth::AccountAuthMaterial {
            bearer_token: Some("sk-ant-api-invalid".to_owned()),
            cookies: vec![crate::auth::CookieValue {
                name: "sessionKey".to_owned(),
                value: "not-a-session".to_owned(),
            }],
            ..Default::default()
        };
        assert!(claude_session_key(&invalid).is_none());

        let valid = crate::auth::AccountAuthMaterial {
            cookies: vec![crate::auth::CookieValue {
                name: "sessionKey".to_owned(),
                value: "sk-ant-sid-test".to_owned(),
            }],
            ..Default::default()
        };
        assert_eq!(
            claude_session_key(&valid).as_deref(),
            Some("sk-ant-sid-test")
        );
    }

    #[test]
    fn rotated_web_session_key_requires_success_and_a_valid_cookie() {
        let response = |status_code, set_cookie: &str| crate::transport::UsageHttpResponse {
            status_code,
            body: String::new(),
            headers: [("Set-Cookie".to_owned(), set_cookie.to_owned())]
                .into_iter()
                .collect(),
        };

        let renewed = response(
            200,
            "__cf_bm=cloudflare; Expires=Wed, 21 Oct 2030 07:28:00 GMT\nsessionKey=sk-ant-sid-renewed; Path=/; HttpOnly",
        );
        assert_eq!(
            rotated_claude_session_key(&renewed).as_deref(),
            Some("sk-ant-sid-renewed")
        );
        assert!(rotated_claude_session_key(&response(401, "sessionKey=sk-ant-sid-new")).is_none());
        assert!(
            rotated_claude_session_key(&response(200, "sessionKey=not-a-claude-cookie")).is_none()
        );
    }

    #[test]
    fn plan_inference_preserves_max_usage_multiplier() {
        assert_eq!(
            claude_plan_label(None, Some("default_claude_max_5x"), None, None).as_deref(),
            Some("Claude Max 5x")
        );
    }

    #[test]
    fn cloudflare_challenge_is_distinct_from_a_rejected_claude_session() {
        let challenge = crate::transport::UsageHttpResponse {
            status_code: 403,
            body: "Just a moment...".to_owned(),
            headers: std::collections::BTreeMap::new(),
        };
        let error = map_claude_http_error(&challenge, "Claude").error.unwrap();
        assert_eq!(
            error.code,
            crate::usage::UsageAdapterErrorCode::CloudflareChallenge
        );

        let unauthorized = crate::transport::UsageHttpResponse {
            status_code: 401,
            body: String::new(),
            headers: std::collections::BTreeMap::new(),
        };
        let error = map_claude_http_error(&unauthorized, "Claude")
            .error
            .unwrap();
        assert_eq!(
            error.code,
            crate::usage::UsageAdapterErrorCode::Unauthorized
        );
    }
    #[test]
    fn automatic_cli_fallback_requires_the_same_signed_in_account() {
        let account =
            AccountRecord::create("Claude", "me@example.com", None, CLAUDE, None).unwrap();
        let automatic = ClaudeSourceMode::Automatic;
        assert!(unverified_cli_fallback(automatic, &account, None).is_some());
        assert!(unverified_cli_fallback(automatic, &account, Some("other@example.com")).is_some());
        assert!(unverified_cli_fallback(automatic, &account, Some("ME@example.com")).is_none());
        assert!(unverified_cli_fallback(ClaudeSourceMode::Cli, &account, None).is_none());
    }

    #[test]
    fn oauth_profile_names_the_subscription_plan() {
        let pro = parse_claude_profile(
            r#"{"account":{"email":"a@example.com","has_claude_pro":true,"has_claude_max":false},
                "organization":{"organization_type":"claude_pro","rate_limit_tier":"default_claude_ai","billing_type":"stripe_subscription","seat_tier":null}}"#,
        )
        .unwrap();
        assert_eq!(pro.plan_type.as_deref(), Some("Claude Pro"));

        let max = parse_claude_profile(
            r#"{"account":{"has_claude_max":true},
                "organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x"}}"#,
        )
        .unwrap();
        assert_eq!(max.plan_type.as_deref(), Some("Claude Max 20x"));

        let team = parse_claude_profile(
            r#"{"organization":{"organization_type":"claude_team","seat_tier":"team_standard"}}"#,
        )
        .unwrap();
        assert_eq!(team.plan_type.as_deref(), Some("Claude Team Standard"));

        let flags_only = parse_claude_profile(r#"{"account":{"has_claude_pro":true}}"#).unwrap();
        assert_eq!(flags_only.plan_type.as_deref(), Some("Claude Pro"));

        let unknown = parse_claude_profile(r#"{"account":{"email":"a@example.com"}}"#).unwrap();
        assert_eq!(unknown.plan_type, None);
    }

    #[test]
    fn reset_grants_become_reset_credits() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-30T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let root: Value = serde_json::from_str(
            r#"{"cedar_ember":{"eligible":true,"grants":[
                {"id":"launch","label":"Launch reset","resets_total":1,"resets_left":1,
                 "starts_at":"2026-09-22T19:00:00+03:00","ends_at":"2026-10-22T19:00:00+03:00",
                 "clears":["five_hour","seven_day"],"paused":false},
                {"id":"session","resets_left":2,"ends_at":"2026-10-01T00:00:00Z",
                 "clears":["five_hour"],"paused":true},
                {"id":"used","resets_left":0,"clears":["five_hour"]},
                {"id":"expired","resets_left":1,"ends_at":"2026-09-01T00:00:00Z","clears":["seven_day"]}
            ]}}"#,
        )
        .unwrap();

        let inventory = parse_claude_reset_grants(&root, now).unwrap();

        assert_eq!(inventory.available_count, 1);
        assert_eq!(inventory.credits.len(), 3);
        let full = &inventory.credits[0];
        assert_eq!(full.title.as_deref(), Some("Full reset"));
        assert_eq!(full.status.as_deref(), Some("available"));
        assert_eq!(full.description.as_deref(), Some("Launch reset"));
        assert_eq!(
            full.expires_at_utc.unwrap().to_rfc3339(),
            "2026-10-22T16:00:00+00:00"
        );
        assert!(inventory.credits[1..].iter().all(|credit| {
            credit.title.as_deref() == Some("5-hour reset")
                && credit.status.as_deref() == Some("paused")
        }));

        let ineligible: Value =
            serde_json::from_str(r#"{"cedar_ember":{"eligible":false,"grants":[]}}"#).unwrap();
        let empty = parse_claude_reset_grants(&ineligible, now).unwrap();
        assert_eq!(empty.available_count, 0);
        let absent: Value = serde_json::from_str(r#"{"cedar_ember":null}"#).unwrap();
        assert!(parse_claude_reset_grants(&absent, now).is_none());
    }
}
