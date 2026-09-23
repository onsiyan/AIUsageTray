use crate::{
    accounts::{AccountRecord, OPENROUTER, VerifiedIdentity},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError},
    providers::shared::{
        bearer_headers, invalid_payload, json_bool, json_number, json_string, map_http_error,
        missing_auth, normalize_percent,
    },
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        CreditsSnapshot, RateLimitWindow, SpendSnapshot, UsageAdapter, UsageAdapterErrorCode,
        UsageMetric, UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot,
        UsageSourceDiagnostic, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    env,
    sync::Arc,
    time::Duration as StdDuration,
};
use url::Url;

const DEFAULT_API_URL: &str = "https://openrouter.ai/api/v1/";
const USER_AGENT: &str = "CodexUsageMonitor/0.1";
const DEFAULT_KEY_DEADLINE: StdDuration = StdDuration::from_secs(4);
const DEFAULT_OPTIONAL_DEADLINE: StdDuration = StdDuration::from_secs(4);
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub struct OpenRouterUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    base_url: Url,
    fetch_credits: bool,
    fetch_activity: bool,
    activity_workspace_id: Option<String>,
    activity_group_by_workspace: bool,
    key_deadline: StdDuration,
    optional_deadline: StdDuration,
}

impl OpenRouterUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        fetch_credits: bool,
    ) -> Result<Self, TransportError> {
        let configured_url = env::var("OPENROUTER_API_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_URL.to_owned());
        let base_url = normalize_api_base(&configured_url)?;
        let activity_workspace_id = env::var("OPENROUTER_ACTIVITY_WORKSPACE_ID")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let activity_group_by_workspace = env::var("OPENROUTER_ACTIVITY_GROUP_BY_WORKSPACE")
            .ok()
            .is_some_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes"
                )
            });
        Ok(Self {
            transport,
            auth,
            base_url,
            fetch_credits,
            // Activity is management-key-only. The request is skipped when no
            // management credential is available, so ordinary API keys never
            // create a predictable 403 on every refresh.
            fetch_activity: true,
            activity_workspace_id,
            activity_group_by_workspace,
            key_deadline: DEFAULT_KEY_DEADLINE,
            optional_deadline: DEFAULT_OPTIONAL_DEADLINE,
        })
    }

    /// Enables or disables the optional 30-day management activity enrichment.
    /// Key quota and credit balance remain enabled independently.
    pub fn with_activity(mut self, enabled: bool) -> Self {
        self.fetch_activity = enabled;
        self
    }

    /// Optionally scopes Activity to one OpenRouter workspace.  Leaving the
    /// workspace unset preserves OpenRouter's account-wide aggregation.
    pub fn with_activity_scope(
        mut self,
        workspace_id: Option<String>,
        group_by_workspace: bool,
    ) -> Self {
        self.activity_workspace_id = workspace_id
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        self.activity_group_by_workspace = group_by_workspace;
        self
    }

    /// Sets the bounded deadlines used by the key and optional enrichment
    /// requests.  This is primarily useful for a host that has a stricter
    /// refresh budget or deterministic transport tests.
    pub fn with_deadlines(
        mut self,
        key_deadline: StdDuration,
        optional_deadline: StdDuration,
    ) -> Self {
        self.key_deadline = key_deadline;
        self.optional_deadline = optional_deadline;
        self
    }

    async fn get(
        &self,
        path: &str,
        material: &AccountAuthMaterial,
        token: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        self.get_with_query(path, material, token, &[]).await
    }

    async fn get_with_query(
        &self,
        path: &str,
        material: &AccountAuthMaterial,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        let mut url = url;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in query {
                pairs.append_pair(name, value);
            }
        }
        let mut headers = bearer_headers(material, USER_AGENT);
        headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
        if let Some(referer) = env::var("OPENROUTER_HTTP_REFERER")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            headers.insert("HTTP-Referer".to_owned(), referer);
        }
        if let Some(title) = env::var("OPENROUTER_X_TITLE")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            headers.insert("X-Title".to_owned(), title);
        }
        let deadline = if path == "key" {
            self.key_deadline
        } else {
            self.optional_deadline
        };
        tokio::time::timeout(
            deadline,
            self.transport.send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers,
                body: None,
            }),
        )
        .await
        .map_err(|_| TransportError::Timeout(path.to_owned()))?
    }
}

#[async_trait]
impl UsageAdapter for OpenRouterUsageAdapter {
    fn adapter_id(&self) -> &str {
        OPENROUTER
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = match self.auth.get(account).await {
            Ok(Some(material)) => material,
            Ok(None) => return Ok(missing_auth("OpenRouter")),
            Err(AuthError::ReauthenticationRequired(_)) => return Ok(missing_auth("OpenRouter")),
            Err(error) => return Ok(invalid_payload("OpenRouter", error.to_string())),
        };
        let Some(token) = material
            .bearer_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(missing_auth("OpenRouter"));
        };

        // /key and /credits are independent account-scoped sources. Keep a
        // usable credit balance when quota is temporarily unavailable, while
        // preserving the key failure if no other source yields data.
        let mut source_diagnostics = Vec::new();
        let mut key_failure = None;
        let key_data = match self.get("key", &material, token).await {
            Ok(response) if response.is_success() => {
                match serde_json::from_str::<Value>(&response.body) {
                    Ok(root) => match root.get("data").filter(|data| data.is_object()) {
                        Some(data) => Some(data.clone()),
                        None => {
                            source_diagnostics.push(invalid_source_diagnostic(
                                "key",
                                "key response is missing object data",
                            ));
                            key_failure = Some(Ok(invalid_payload(
                                "OpenRouter",
                                "key response is missing data",
                            )));
                            None
                        }
                    },
                    Err(error) => {
                        source_diagnostics.push(invalid_source_diagnostic(
                            "key",
                            "key response was not valid JSON",
                        ));
                        key_failure = Some(Err(TransportError::Serialization(error.to_string())));
                        None
                    }
                }
            }
            Ok(response) => {
                source_diagnostics.push(response_diagnostic("key", &response));
                key_failure = Some(Ok(map_http_error(&response, "OpenRouter")));
                None
            }
            Err(error) => {
                source_diagnostics.push(transport_diagnostic("key", &error));
                key_failure = Some(Err(error));
                None
            }
        };
        let empty_data = Value::Null;
        let data = key_data.as_ref().unwrap_or(&empty_data);

        let now = Utc::now();
        let cumulative_usage = non_negative(json_number(data, &["usage"]));
        let limit = non_negative(json_number(data, &["limit"]));
        let explicit_remaining = json_number(data, &["limit_remaining"])
            .filter(|value| value.is_finite())
            .map(|remaining| {
                limit.map_or_else(|| remaining.max(0.0), |limit| remaining.clamp(0.0, limit))
            });
        let reset_label = json_string(data, &["limit_reset"])
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let reset = reset_label
            .as_deref()
            .and_then(|label| reset_boundary(label, now));
        let period_usage = reset_label
            .as_deref()
            .and_then(|label| period_usage(data, label));
        let used_percent =
            key_limit_used_percent(limit, explicit_remaining, period_usage, cumulative_usage);
        let workspace_id = json_string(data, &["workspace_id", "workspaceId"])
            .or_else(|| account.workspace_id.clone());
        let is_management_key =
            json_bool(data, &["is_management_key", "isManagementKey"]).unwrap_or(false);
        let is_free_tier = json_bool(data, &["is_free_tier", "isFreeTier"]);
        let plan_type = is_free_tier.map(|free| {
            if free {
                "Free tier".to_owned()
            } else {
                "Paid".to_owned()
            }
        });

        let mut metrics = Vec::new();
        if let Some(metric) = parse_free_model_daily_requests(data, now) {
            metrics.push(metric);
        }
        for (suffix, name, property) in [
            ("daily", "Daily key usage", "usage_daily"),
            ("weekly", "Weekly key usage", "usage_weekly"),
            ("monthly", "Monthly key usage", "usage_monthly"),
        ] {
            let used_amount = non_negative(json_number(data, &[property]));
            if let Some(used_amount) = used_amount {
                let period_reset = reset_boundary(suffix, now);
                let is_limited_period = reset_label.as_deref() == Some(suffix);
                metrics.push(UsageMetric {
                    key: format!("key.{suffix}"),
                    name: name.to_owned(),
                    used_percent: is_limited_period.then(|| {
                        key_limit_used_percent(limit, explicit_remaining, Some(used_amount), None)
                            .unwrap_or(0.0)
                    }),
                    used_amount: Some(used_amount),
                    limit_amount: is_limited_period.then_some(limit).flatten(),
                    remaining_amount: is_limited_period
                        .then(|| {
                            explicit_remaining
                                .or_else(|| limit.map(|value| (value - used_amount).max(0.0)))
                        })
                        .flatten(),
                    unit: Some("USD".to_owned()),
                    reset_at_utc: period_reset.map(|(at, _)| at),
                    reset_label: Some(suffix.to_owned()),
                    metadata: HashMap::new(),
                });
            }
        }

        for (suffix, name, property) in [
            ("daily", "Daily BYOK usage", "byok_usage_daily"),
            ("weekly", "Weekly BYOK usage", "byok_usage_weekly"),
            ("monthly", "Monthly BYOK usage", "byok_usage_monthly"),
        ] {
            if let Some(used_amount) = non_negative(json_number(data, &[property])) {
                let mut metadata = HashMap::new();
                metadata.insert("scope".to_owned(), "byok".to_owned());
                metrics.push(UsageMetric {
                    key: format!("key.byok.{suffix}"),
                    name: name.to_owned(),
                    used_percent: None,
                    used_amount: Some(used_amount),
                    limit_amount: None,
                    remaining_amount: None,
                    unit: Some("USD".to_owned()),
                    reset_at_utc: reset_boundary(suffix, now).map(|(at, _)| at),
                    reset_label: Some(suffix.to_owned()),
                    metadata,
                });
            }
        }

        if let Some(limit) = limit {
            let mut metadata = HashMap::new();
            metadata.insert(
                "limit_reset".to_owned(),
                reset_label.clone().unwrap_or_else(|| "none".to_owned()),
            );
            if let Some(label) = json_string(data, &["label"]) {
                metadata.insert("label".to_owned(), label);
            }
            if let Some(is_free_tier) = is_free_tier {
                metadata.insert("is_free_tier".to_owned(), is_free_tier.to_string());
            }
            if let Some(workspace_id) = workspace_id.as_deref() {
                metadata.insert("workspace_id".to_owned(), workspace_id.to_owned());
            }
            for (property, key) in [
                ("organization_id", "organization_id"),
                ("expires_at", "expires_at"),
                ("creator_user_id", "creator_user_id"),
            ] {
                if let Some(value) = json_string(data, &[property]) {
                    metadata.insert(key.to_owned(), value);
                }
            }
            if let Some(include_byok_in_limit) =
                json_bool(data, &["include_byok_in_limit", "includeByokInLimit"])
            {
                metadata.insert(
                    "include_byok_in_limit".to_owned(),
                    include_byok_in_limit.to_string(),
                );
            }
            if let Some(is_provisioning_key) =
                json_bool(data, &["is_provisioning_key", "isProvisioningKey"])
            {
                metadata.insert(
                    "is_provisioning_key".to_owned(),
                    is_provisioning_key.to_string(),
                );
            }
            if let Some(regions) = data.get("allowed_data_regions").and_then(Value::as_array) {
                let regions = regions
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                if !regions.is_empty() {
                    metadata.insert("allowed_data_regions".to_owned(), regions);
                }
            }
            if is_management_key {
                metadata.insert("is_management_key".to_owned(), "true".to_owned());
            }
            metrics.push(UsageMetric {
                key: "key.limit".to_owned(),
                name: "API key limit".to_owned(),
                used_percent,
                used_amount: period_usage.or(cumulative_usage),
                limit_amount: Some(limit),
                remaining_amount: explicit_remaining.or_else(|| {
                    used_percent.map(|percent| (limit * (1.0 - percent / 100.0)).max(0.0))
                }),
                unit: Some("USD".to_owned()),
                reset_at_utc: reset.map(|(at, _)| at),
                reset_label: reset_label.clone(),
                metadata,
            });
        }

        // Credits are optional enrichment and remain account-key scoped. A
        // separately configured management key must never replace the
        // selected key here: OpenRouter's credits endpoint is account-wide for
        // management credentials, which would mix balances across accounts.
        let credits = if self.fetch_credits {
            match self.get("credits", &material, token).await {
                Ok(response) if response.is_success() => match parse_credits(&response.body) {
                    Ok(credits) => Some(credits),
                    Err(reason) => {
                        source_diagnostics.push(invalid_source_diagnostic("credits", reason));
                        None
                    }
                },
                Ok(response) => {
                    source_diagnostics.push(response_diagnostic("credits", &response));
                    None
                }
                Err(error) => {
                    source_diagnostics.push(transport_diagnostic("credits", &error));
                    None
                }
            }
        } else {
            None
        };

        // Account Activity is available only with a management key. Keep it
        // independent from the main key/credits probe so a missing optional
        // permission never hides authoritative quota data.
        // Activity is deliberately restricted to OpenRouter's documented
        // first-party origin. A custom API origin may emulate `/key` and
        // `/credits`, but it must never cause a management credential to be
        // sent to an arbitrary proxy.
        if self.fetch_activity && is_official_api_base(&self.base_url) {
            let activity_token = material
                .secondary_bearer_token
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| is_management_key.then_some(token));
            if let Some(activity_token) = activity_token {
                let mut query = Vec::new();
                if let Some(workspace_id) = self
                    .activity_workspace_id
                    .as_deref()
                    .or(account.workspace_id.as_deref())
                {
                    query.push(("workspace_id", workspace_id.to_owned()));
                }
                if self.activity_group_by_workspace {
                    query.push(("group_by", "workspace".to_owned()));
                }
                match self
                    .get_with_query("activity", &material, activity_token, &query)
                    .await
                {
                    Ok(response) if response.is_success() => {
                        match parse_activity_report(&response.body) {
                            Ok(report) => {
                                metrics.extend(report.metrics);
                                metrics.push(report.summary);
                            }
                            Err(reason) => {
                                source_diagnostics
                                    .push(invalid_source_diagnostic("activity", reason));
                            }
                        }
                    }
                    Ok(response) => {
                        source_diagnostics.push(response_diagnostic("activity", &response));
                    }
                    Err(error) => {
                        source_diagnostics.push(transport_diagnostic("activity", &error));
                    }
                }
            }
        }

        let primary = limit.and_then(|limit| {
            (limit > 0.0)
                .then_some(used_percent)
                .flatten()
                .map(|used_percent| RateLimitWindow {
                    kind: UsageWindowKind::Primary,
                    name: "API key limit".to_owned(),
                    used_percent,
                    reset_at_utc: reset.map(|(at, _)| at),
                    limit_window_seconds: reset.map(|(_, seconds)| seconds).unwrap_or_default(),
                })
        });
        let monthly_usage = non_negative(json_number(data, &["usage_monthly"]));
        let monthly_limit = (reset_label.as_deref() == Some("monthly"))
            .then_some(limit)
            .flatten();
        let spend = (monthly_usage.is_some() || monthly_limit.is_some()).then(|| SpendSnapshot {
            monthly_usage,
            monthly_limit,
            used_percent: monthly_limit.and_then(|value| {
                key_limit_used_percent(Some(value), explicit_remaining, monthly_usage, None)
            }),
            limit_enabled: Some(monthly_limit.is_some()),
            currency_code: None,
        });

        if primary.is_none() && spend.is_none() && credits.is_none() && metrics.is_empty() {
            return match key_failure {
                Some(failure) => failure,
                None => Ok(invalid_payload("OpenRouter", "no usage data was present")),
            };
        }

        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: workspace_id.clone(),
            plan_type,
            primary,
            primary_window_kind: Some(UsagePrimaryWindowKind::Spend),
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits,
            credit_inventory: None,
            spend,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics,
            provider_id: OPENROUTER.to_owned(),
            source: Some("api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: workspace_id,
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

fn normalize_api_base(raw: &str) -> Result<Url, TransportError> {
    let mut url =
        Url::parse(raw.trim()).map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(TransportError::InvalidUrl(
            "OpenRouter API URL must be an HTTPS URL with a host".to_owned(),
        ));
    }
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/api/v1") {
        format!("{path}/")
    } else {
        format!("{path}/api/v1/")
    };
    url.set_path(&path);
    Ok(url)
}

fn is_official_api_base(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("openrouter.ai"))
        && url.port_or_known_default() == Some(443)
        && url.path() == "/api/v1/"
}

fn non_negative(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value >= 0.0)
}

fn period_usage(data: &Value, reset_label: &str) -> Option<f64> {
    let property = match reset_label {
        "daily" => "usage_daily",
        "weekly" => "usage_weekly",
        "monthly" => "usage_monthly",
        _ => return None,
    };
    non_negative(json_number(data, &[property]))
}

fn key_limit_used_percent(
    limit: Option<f64>,
    explicit_remaining: Option<f64>,
    period_usage: Option<f64>,
    cumulative_usage: Option<f64>,
) -> Option<f64> {
    let limit = limit.filter(|value| *value > 0.0)?;
    if let Some(remaining) = explicit_remaining.filter(|value| value.is_finite()) {
        return Some(normalize_percent(
            (limit - remaining.clamp(0.0, limit)) / limit * 100.0,
        ));
    }
    period_usage
        .or(cumulative_usage)
        .map(|used| normalize_percent(used / limit * 100.0))
}

fn reset_boundary(label: &str, now: DateTime<Utc>) -> Option<(DateTime<Utc>, i64)> {
    let today = now.date_naive();
    let (date, seconds) = match label {
        "daily" => (today + Duration::days(1), 86_400),
        "weekly" => {
            let days_from_monday = i64::from(today.weekday().num_days_from_monday());
            (
                today - Duration::days(days_from_monday) + Duration::days(7),
                604_800,
            )
        }
        "monthly" => {
            let first_of_month = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)?;
            let (year, month) = if today.month() == 12 {
                (today.year() + 1, 1)
            } else {
                (today.year(), today.month() + 1)
            };
            let first_of_next_month = NaiveDate::from_ymd_opt(year, month, 1)?;
            (
                first_of_next_month,
                (first_of_next_month - first_of_month).num_days() * 86_400,
            )
        }
        _ => return None,
    };
    let midnight = date.and_hms_opt(0, 0, 0)?;
    Some((Utc.from_utc_datetime(&midnight), seconds))
}

#[derive(Debug)]
struct ActivityReport {
    metrics: Vec<UsageMetric>,
    summary: UsageMetric,
}

fn parse_activity_report(body: &str) -> Result<ActivityReport, String> {
    let root = serde_json::from_str::<Value>(body)
        .map_err(|error| format!("activity JSON could not be parsed: {error}"))?;
    let rows = root
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "activity response is missing data[]".to_owned())?;

    let mut metrics = Vec::with_capacity(rows.len());
    let mut total_usage = 0.0;
    let mut total_requests = 0_u64;
    let mut total_prompt_tokens = 0_u64;
    let mut total_completion_tokens = 0_u64;
    let mut total_reasoning_tokens = 0_u64;
    let mut models = BTreeSet::new();

    for (index, row) in rows.iter().enumerate() {
        let usage = non_negative(json_number(row, &["usage"]))
            .ok_or_else(|| format!("activity row {index} has invalid usage"))?;
        total_usage += usage;
        if !total_usage.is_finite() {
            return Err("activity usage total is not finite".to_owned());
        }

        let requests = activity_integer(row, "requests", index)?;
        let prompt_tokens = activity_integer(row, "prompt_tokens", index)?;
        let completion_tokens = activity_integer(row, "completion_tokens", index)?;
        let reasoning_tokens = activity_integer(row, "reasoning_tokens", index)?;
        add_safe_total(&mut total_requests, requests, "requests")?;
        add_safe_total(&mut total_prompt_tokens, prompt_tokens, "prompt_tokens")?;
        add_safe_total(
            &mut total_completion_tokens,
            completion_tokens,
            "completion_tokens",
        )?;
        add_safe_total(
            &mut total_reasoning_tokens,
            reasoning_tokens,
            "reasoning_tokens",
        )?;

        let date = json_string(row, &["date"]).unwrap_or_else(|| "unknown-date".to_owned());
        let model = json_string(row, &["model", "model_permaslug"])
            .unwrap_or_else(|| "unknown-model".to_owned());
        let endpoint = json_string(row, &["endpoint_id", "provider_name"])
            .unwrap_or_else(|| index.to_string());
        models.insert(model.clone());

        let mut metadata = HashMap::new();
        for (property, key) in [
            ("date", "date"),
            ("model", "model"),
            ("model_permaslug", "model_permaslug"),
            ("endpoint_id", "endpoint_id"),
            ("provider_name", "provider_name"),
        ] {
            if let Some(value) = json_string(row, &[property]) {
                metadata.insert(key.to_owned(), value);
            }
        }
        for (property, key) in [
            ("requests", "requests"),
            ("prompt_tokens", "prompt_tokens"),
            ("completion_tokens", "completion_tokens"),
            ("reasoning_tokens", "reasoning_tokens"),
        ] {
            if let Some(value) = activity_integer(row, property, index)? {
                metadata.insert(key.to_owned(), value.to_string());
            }
        }
        if let Some(value) = json_number(row, &["byok_usage_inference"]) {
            if value.is_finite() && value >= 0.0 {
                metadata.insert("byok_usage_inference".to_owned(), value.to_string());
            }
        }
        metrics.push(UsageMetric {
            key: format!(
                "activity.{}.{}.{}",
                metric_component(&date),
                metric_component(&model),
                metric_component(&endpoint)
            ),
            name: format!("Activity — {model} ({date})"),
            used_percent: None,
            used_amount: Some(usage),
            limit_amount: None,
            remaining_amount: None,
            unit: Some("USD".to_owned()),
            reset_at_utc: None,
            reset_label: None,
            metadata,
        });
    }

    let mut summary_metadata = HashMap::new();
    summary_metadata.insert("window".to_owned(), "last-30-completed-utc-days".to_owned());
    summary_metadata.insert("rows".to_owned(), rows.len().to_string());
    summary_metadata.insert("requests".to_owned(), total_requests.to_string());
    summary_metadata.insert("prompt_tokens".to_owned(), total_prompt_tokens.to_string());
    summary_metadata.insert(
        "completion_tokens".to_owned(),
        total_completion_tokens.to_string(),
    );
    summary_metadata.insert(
        "total_tokens".to_owned(),
        total_prompt_tokens
            .checked_add(total_completion_tokens)
            .ok_or_else(|| "activity input/output token total overflowed".to_owned())?
            .to_string(),
    );
    summary_metadata.insert(
        "reasoning_tokens".to_owned(),
        total_reasoning_tokens.to_string(),
    );
    summary_metadata.insert(
        "models".to_owned(),
        models.iter().cloned().collect::<Vec<_>>().join(","),
    );
    summary_metadata.insert("model_count".to_owned(), models.len().to_string());

    Ok(ActivityReport {
        metrics,
        summary: UsageMetric {
            key: "activity.summary".to_owned(),
            name: "Activity summary — last 30 days (UTC)".to_owned(),
            used_percent: None,
            used_amount: Some(total_usage),
            limit_amount: None,
            remaining_amount: None,
            unit: Some("USD".to_owned()),
            reset_at_utc: None,
            reset_label: None,
            metadata: summary_metadata,
        },
    })
}

fn activity_integer(row: &Value, property: &str, index: usize) -> Result<Option<u64>, String> {
    let Some(value) = row.get(property) else {
        return Ok(None);
    };
    let parsed = match value {
        Value::Number(value) => value
            .as_u64()
            .or_else(|| {
                value
                    .as_i64()
                    .filter(|value| *value >= 0)
                    .map(|value| value as u64)
            })
            .or_else(|| {
                value.as_f64().and_then(|value| {
                    (value.is_finite() && value >= 0.0 && value.fract() == 0.0)
                        .then_some(value as u64)
                })
            }),
        Value::String(value) => value.trim().parse::<u64>().ok(),
        _ => None,
    };
    let Some(parsed) = parsed.filter(|value| *value <= MAX_SAFE_INTEGER) else {
        return Err(format!(
            "activity row {index} has unsafe {property} (maximum {MAX_SAFE_INTEGER})"
        ));
    };
    Ok(Some(parsed))
}

fn add_safe_total(total: &mut u64, value: Option<u64>, property: &str) -> Result<(), String> {
    if let Some(value) = value {
        *total = total
            .checked_add(value)
            .filter(|total| *total <= MAX_SAFE_INTEGER)
            .ok_or_else(|| format!("activity {property} total exceeded safe integer range"))?;
    }
    Ok(())
}

fn parse_free_model_daily_requests(data: &Value, now: DateTime<Utc>) -> Option<UsageMetric> {
    let free_requests = data.get("free_model_daily_requests")?;
    let used = non_negative(json_number(free_requests, &["used"]));
    let limit = non_negative(json_number(free_requests, &["limit"]));
    let remaining = non_negative(json_number(free_requests, &["remaining"]));
    if used.is_none() && limit.is_none() && remaining.is_none() {
        return None;
    }

    let used_percent = key_limit_used_percent(limit, remaining, used, None);
    let mut metadata = HashMap::new();
    metadata.insert("scope".to_owned(), "free-models".to_owned());
    metadata.insert("period".to_owned(), "daily".to_owned());
    Some(UsageMetric {
        key: "free-model.daily-requests".to_owned(),
        name: "Free model daily requests".to_owned(),
        used_percent,
        used_amount: used,
        limit_amount: limit,
        remaining_amount: remaining
            .or_else(|| limit.zip(used).map(|(limit, used)| (limit - used).max(0.0))),
        unit: Some("requests".to_owned()),
        reset_at_utc: reset_boundary("daily", now).map(|(at, _)| at),
        reset_label: Some("daily".to_owned()),
        metadata,
    })
}

fn metric_component(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.is_empty() {
        "unknown".to_owned()
    } else {
        normalized
    }
}

fn parse_credits(body: &str) -> Result<CreditsSnapshot, String> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| format!("credits JSON could not be parsed: {error}"))?;
    let data = root
        .get("data")
        .ok_or_else(|| "credits response is missing data".to_owned())?;
    let total = non_negative(json_number(data, &["total_credits"]))
        .ok_or_else(|| "credits response is missing total_credits".to_owned())?;
    let usage = non_negative(json_number(data, &["total_usage"]))
        .ok_or_else(|| "credits response is missing total_usage".to_owned())?;
    let balance = (total - usage).max(0.0);
    Ok(CreditsSnapshot {
        has_credits: Some(true),
        unlimited: Some(false),
        balance: Some(balance),
        currency_code: None,
        approximate_message_cost: None,
        limit: None,
        balance_read_succeeded: Some(true),
        credits_available: Some(balance > 0.0),
    })
}

fn invalid_source_diagnostic(source: &str, reason: impl Into<String>) -> UsageSourceDiagnostic {
    UsageSourceDiagnostic {
        source: source.to_owned(),
        code: UsageAdapterErrorCode::InvalidPayload,
        message: format!(
            "OpenRouter {source} response was invalid: {}",
            reason.into()
        ),
        http_status_code: None,
        retry_after_seconds: None,
    }
}

fn response_diagnostic(source: &str, response: &UsageHttpResponse) -> UsageSourceDiagnostic {
    let code = match response.status_code {
        401 => UsageAdapterErrorCode::Unauthorized,
        403 => UsageAdapterErrorCode::Forbidden,
        429 => UsageAdapterErrorCode::RateLimited,
        500..=599 => UsageAdapterErrorCode::TransientHttp,
        _ => UsageAdapterErrorCode::HttpError,
    };
    UsageSourceDiagnostic {
        source: source.to_owned(),
        code,
        message: format!("OpenRouter {source} returned HTTP {}", response.status_code),
        http_status_code: Some(response.status_code),
        retry_after_seconds: response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse::<u64>().ok()),
    }
}

fn transport_diagnostic(source: &str, error: &TransportError) -> UsageSourceDiagnostic {
    let (code, message) = match error {
        TransportError::Timeout(path) => (
            UsageAdapterErrorCode::NetworkFailure,
            format!("OpenRouter {source} request timed out ({path})"),
        ),
        TransportError::Request(error) if error.is_timeout() => (
            UsageAdapterErrorCode::NetworkFailure,
            format!("OpenRouter {source} request timed out"),
        ),
        TransportError::Request(error) => (
            UsageAdapterErrorCode::NetworkFailure,
            format!("OpenRouter {source} request failed: {error}"),
        ),
        TransportError::Serialization(reason) => (
            UsageAdapterErrorCode::InvalidPayload,
            format!("OpenRouter {source} response serialization failed: {reason}"),
        ),
        TransportError::InvalidUrl(reason) | TransportError::InvalidHeader { reason, .. } => (
            UsageAdapterErrorCode::Unknown,
            format!("OpenRouter {source} request could not be built: {reason}"),
        ),
    };
    UsageSourceDiagnostic {
        source: source.to_owned(),
        code,
        message,
        http_status_code: None,
        retry_after_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_limit_prefers_server_remaining_over_cumulative_usage() {
        let percent = key_limit_used_percent(Some(100.0), Some(74.5), Some(25.5), Some(400.0));
        assert_eq!(percent, Some(25.5));
    }

    #[test]
    fn key_limit_clamps_server_remaining_to_the_limit_bounds() {
        assert_eq!(
            key_limit_used_percent(Some(100.0), Some(120.0), Some(40.0), Some(80.0)),
            Some(0.0)
        );
        assert_eq!(
            key_limit_used_percent(Some(100.0), Some(-5.0), Some(40.0), Some(80.0)),
            Some(100.0)
        );
    }

    #[test]
    fn reset_boundaries_are_utc_calendar_boundaries() {
        let now = Utc.with_ymd_and_hms(2026, 2, 18, 12, 34, 56).unwrap();
        let (daily, daily_seconds) = reset_boundary("daily", now).unwrap();
        assert_eq!(daily, Utc.with_ymd_and_hms(2026, 2, 19, 0, 0, 0).unwrap());
        assert_eq!(daily_seconds, 86_400);

        let (weekly, weekly_seconds) = reset_boundary("weekly", now).unwrap();
        assert_eq!(weekly, Utc.with_ymd_and_hms(2026, 2, 23, 0, 0, 0).unwrap());
        assert_eq!(weekly_seconds, 604_800);

        let (monthly, monthly_seconds) = reset_boundary("monthly", now).unwrap();
        assert_eq!(monthly, Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap());
        assert_eq!(monthly_seconds, 28 * 86_400);
    }

    #[test]
    fn free_model_request_limit_is_normalized_as_a_request_metric() {
        let data = json!({
            "free_model_daily_requests": {
                "limit": 50,
                "remaining": 38,
                "used": 12
            }
        });
        let metric = parse_free_model_daily_requests(
            &data,
            Utc.with_ymd_and_hms(2026, 2, 18, 12, 0, 0).unwrap(),
        )
        .unwrap();
        assert_eq!(metric.key, "free-model.daily-requests");
        assert_eq!(metric.used_percent, Some(24.0));
        assert_eq!(metric.remaining_amount, Some(38.0));
        assert_eq!(metric.unit.as_deref(), Some("requests"));
    }

    #[test]
    fn activity_is_only_enabled_for_the_first_party_origin() {
        let official = normalize_api_base("https://openrouter.ai/api/v1").unwrap();
        let custom = normalize_api_base("https://gateway.example.test").unwrap();
        assert!(is_official_api_base(&official));
        assert!(!is_official_api_base(&custom));
    }

    #[test]
    fn api_base_requires_https_before_credentials_can_be_sent() {
        assert!(normalize_api_base("https://gateway.example.test").is_ok());
        assert!(normalize_api_base("http://gateway.example.test").is_err());
        assert!(normalize_api_base("ftp://gateway.example.test").is_err());
    }

    #[test]
    fn activity_report_adds_a_compact_summary_without_double_counting_reasoning() {
        let report = parse_activity_report(
            r#"{"data":[
                {"date":"2030-01-02","endpoint_id":"endpoint-1","model":"openai/gpt-5","prompt_tokens":50,"completion_tokens":125,"reasoning_tokens":25,"requests":5,"usage":0.015},
                {"date":"2030-01-03","endpoint_id":"endpoint-2","model":"anthropic/claude","prompt_tokens":10,"completion_tokens":20,"reasoning_tokens":30,"requests":2,"usage":0.025}
            ]}"#,
        )
        .unwrap();

        assert_eq!(report.metrics.len(), 2);
        assert!((report.summary.used_amount.unwrap() - 0.04).abs() < 1e-12);
        assert_eq!(
            report.summary.metadata.get("rows").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            report
                .summary
                .metadata
                .get("total_tokens")
                .map(String::as_str),
            Some("205")
        );
        assert_eq!(
            report
                .summary
                .metadata
                .get("reasoning_tokens")
                .map(String::as_str),
            Some("55")
        );
        assert_eq!(
            report
                .summary
                .metadata
                .get("model_count")
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn activity_report_rejects_token_counts_outside_safe_integer_range() {
        let body = format!(
            r#"{{"data":[{{"date":"2030-01-02","model":"openai/gpt-5","usage":0.01,"prompt_tokens":{}}}]}}"#,
            MAX_SAFE_INTEGER + 1
        );
        let error = parse_activity_report(&body).unwrap_err();
        assert!(error.contains("unsafe prompt_tokens"));
    }

    #[test]
    fn optional_endpoint_failures_are_labeled_by_source() {
        let response = UsageHttpResponse {
            status_code: 403,
            body: String::new(),
            headers: Default::default(),
        };
        let diagnostic = response_diagnostic("credits", &response);
        assert_eq!(diagnostic.source, "credits");
        assert_eq!(diagnostic.code, UsageAdapterErrorCode::Forbidden);
        assert_eq!(diagnostic.http_status_code, Some(403));

        let diagnostic =
            transport_diagnostic("activity", &TransportError::Timeout("activity".to_owned()));
        assert_eq!(diagnostic.source, "activity");
        assert!(diagnostic.message.contains("timed out"));
    }
}
