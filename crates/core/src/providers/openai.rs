use crate::{
    accounts::{AccountRecord, OPENAI, VerifiedIdentity},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError, OAuthProviderDefinition},
    providers::shared::{
        bearer_headers, invalid_payload, json_bool, json_number, json_string, map_http_error,
        missing_auth,
    },
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        CreditLimitSnapshot, CreditsSnapshot, RateLimitWindow, SpendSnapshot, UsageAdapter,
        UsageAdapterErrorCode, UsageCreditInventory, UsageCreditRecord, UsageMetric,
        UsagePrimaryWindowKind, UsageProbeResult, UsageSnapshot, UsageSourceDiagnostic,
        UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    env,
    sync::Arc,
};
use url::Url;

const ADAPTER_ID: &str = "openai-wham";
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub fn oauth_definition() -> OAuthProviderDefinition {
    let mut definition = OAuthProviderDefinition::new(
        OPENAI,
        Url::parse("https://auth.openai.com/oauth/authorize").expect("static Codex OAuth URL"),
        Url::parse("https://auth.openai.com/oauth/token").expect("static Codex OAuth URL"),
        Url::parse("http://localhost:1455/auth/callback").expect("static Codex callback URL"),
        CODEX_OAUTH_CLIENT_ID,
        None,
        [
            "openid",
            "profile",
            "email",
            "offline_access",
            "api.connectors.read",
            "api.connectors.invoke",
        ],
    )
    .expect("static Codex OAuth definition");
    definition.authorization_parameters = BTreeMap::from([
        ("id_token_add_organizations".to_owned(), "true".to_owned()),
        ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
        // Identify this application accurately instead of impersonating the
        // first-party Codex CLI originator.
        (
            "originator".to_owned(),
            "codex_usage_monitor_rust".to_owned(),
        ),
    ]);
    definition
}

pub struct WhamUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    base_url: Url,
    fetch_reset_credits: bool,
}

impl WhamUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        let environment = env::vars().collect::<HashMap<_, _>>();
        Ok(Self {
            transport,
            auth,
            base_url: resolve_base_url(&environment)?,
            fetch_reset_credits: true,
        })
    }

    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, TransportError> {
        let environment = environment.into_iter().collect::<HashMap<_, _>>();
        self.base_url = resolve_base_url(&environment)?;
        Ok(self)
    }

    pub fn with_reset_credits(mut self, enabled: bool) -> Self {
        self.fetch_reset_credits = enabled;
        self
    }

    fn usage_path_for_base_url(&self, base_url: &Url) -> &'static str {
        if is_backend_api_base(base_url) {
            "wham/usage"
        } else {
            "api/codex/usage"
        }
    }

    fn reset_credits_path(&self) -> &'static str {
        "wham/rate-limit-reset-credits"
    }

    async fn get(
        &self,
        path: &str,
        account: Option<&AccountRecord>,
        material: &AccountAuthMaterial,
        include_account_header: bool,
    ) -> Result<UsageHttpResponse, TransportError> {
        let url = self
            .base_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        let mut headers = bearer_headers(
            material,
            material.user_agent.as_deref().unwrap_or("UsageMonitor/0.1"),
        );
        if include_account_header
            && let Some(account_id) = account.and_then(|account| account.workspace_id.as_deref())
        {
            headers.insert("ChatGPT-Account-Id".to_owned(), account_id.to_owned());
        }
        if path.ends_with("wham/rate-limit-reset-credits") {
            headers.insert("OpenAI-Beta".to_owned(), "codex-1".to_owned());
            headers.insert("originator".to_owned(), "Codex Desktop".to_owned());
        }
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers,
                body: None,
            })
            .await
    }
}

#[async_trait]
impl UsageAdapter for WhamUsageAdapter {
    fn adapter_id(&self) -> &str {
        ADAPTER_ID
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let source_material = match self.auth.get(account).await {
            Ok(Some(material)) => material,
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => {
                return Ok(missing_auth("Codex"));
            }
            Err(error) => return Ok(invalid_payload("OpenAI", error.to_string())),
        };
        let Some(access_token) = source_material
            .bearer_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
        else {
            return Ok(missing_auth("Codex OAuth"));
        };
        // Only the account-scoped OAuth bearer credential is valid for Codex
        // usage. Ignore any legacy cookie, secondary token, or unrelated
        // material that may coexist in a host's composite auth source.
        let material = AccountAuthMaterial {
            bearer_token: Some(access_token.to_owned()),
            user_agent: source_material.user_agent.clone(),
            ..AccountAuthMaterial::default()
        };
        let base_url = self.base_url.clone();
        let usage_request = self.get(
            self.usage_path_for_base_url(&base_url),
            Some(account),
            &material,
            true,
        );
        let reset_credits_request = async {
            if self.fetch_reset_credits {
                Some(
                    self.get(self.reset_credits_path(), Some(account), &material, true)
                        .await,
                )
            } else {
                None
            }
        };
        let (usage_response, reset_credits_response) =
            tokio::join!(usage_request, reset_credits_request);
        let usage_response = usage_response?;
        if !usage_response.is_success() {
            return Ok(map_http_error(&usage_response, "OpenAI"));
        }
        let mut snapshot = match parse_wham_usage(account, &usage_response.body) {
            Ok(Ok(Some(snapshot))) => snapshot,
            Ok(Ok(None)) => {
                return Ok(invalid_payload(
                    "OpenAI",
                    "usage response contained no valid windows",
                ));
            }
            Ok(Err(error)) => return Ok(UsageProbeResult::failure(error)),
            // A non-JSON 200 body (for example an HTML interstitial) is an
            // invalid provider payload, not a network failure.
            Err(error) => return Ok(invalid_payload("OpenAI", error.to_string())),
        };
        snapshot.observed_email = Some(account.email.clone());
        snapshot.source = Some("codex-oauth".to_owned());
        let mut source_diagnostics = Vec::new();

        if let Some(reset_credits_response) = reset_credits_response {
            match reset_credits_response {
                Ok(response) if response.is_success() => {
                    if let Some(inventory) = parse_credit_inventory(&response.body) {
                        snapshot.credit_inventory = Some(inventory);
                    } else {
                        source_diagnostics.push(optional_payload_diagnostic("wham.reset-credits"));
                    }
                }
                Ok(response) => source_diagnostics.push(optional_response_diagnostic(
                    "wham.reset-credits",
                    &response,
                )),
                Err(error) => source_diagnostics
                    .push(optional_transport_diagnostic("wham.reset-credits", &error)),
            }
        }

        snapshot.source_diagnostics = source_diagnostics;
        let identity = VerifiedIdentity {
            email: Some(account.email.clone()),
            provider_account_id: account.provider_account_id.clone(),
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

fn optional_payload_diagnostic(source: &str) -> UsageSourceDiagnostic {
    UsageSourceDiagnostic {
        source: source.to_owned(),
        code: UsageAdapterErrorCode::InvalidPayload,
        message: format!("OpenAI {source} response did not match the expected data shape"),
        http_status_code: None,
        retry_after_seconds: None,
    }
}

fn optional_response_diagnostic(
    source: &str,
    response: &UsageHttpResponse,
) -> UsageSourceDiagnostic {
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
        message: format!(
            "OpenAI {source} request returned HTTP {}",
            response.status_code
        ),
        http_status_code: Some(response.status_code),
        retry_after_seconds: response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse::<u64>().ok()),
    }
}

fn optional_transport_diagnostic(source: &str, error: &TransportError) -> UsageSourceDiagnostic {
    let (code, message) = match error {
        TransportError::Timeout(_) => (
            UsageAdapterErrorCode::NetworkFailure,
            format!("OpenAI {source} request timed out"),
        ),
        TransportError::Request(error) if error.is_timeout() => (
            UsageAdapterErrorCode::NetworkFailure,
            format!("OpenAI {source} request timed out"),
        ),
        TransportError::Request(_) => (
            UsageAdapterErrorCode::NetworkFailure,
            format!("OpenAI {source} request failed"),
        ),
        TransportError::Serialization(_) => (
            UsageAdapterErrorCode::InvalidPayload,
            format!("OpenAI {source} response could not be processed"),
        ),
        TransportError::InvalidUrl(_) | TransportError::InvalidHeader { .. } => (
            UsageAdapterErrorCode::Unknown,
            format!("OpenAI {source} request could not be built"),
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

fn parse_wham_usage(
    account: &AccountRecord,
    body: &str,
) -> Result<Result<Option<UsageSnapshot>, crate::usage::UsageAdapterError>, TransportError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| TransportError::Serialization(error.to_string()))?;
    let response_account_id = json_string(&root, &["account_id", "accountId"]);
    if let (Some(expected), Some(actual)) = (
        account.workspace_id.as_deref(),
        response_account_id.as_deref(),
    ) && expected != actual
    {
        return Ok(Err(crate::usage::UsageAdapterError {
            code: crate::usage::UsageAdapterErrorCode::AccountMismatch,
            message: "the usage response belongs to a different account".to_owned(),
            http_status_code: None,
            retry_after_seconds: None,
        }));
    }
    let rate_limit = root.get("rate_limit");
    let primary_value = rate_limit.and_then(|value| value.get("primary_window"));
    let secondary_value = rate_limit.and_then(|value| value.get("secondary_window"));
    let primary = primary_value
        .and_then(|value| parse_strict_window(Some(value), UsageWindowKind::Primary, "Primary"));
    let secondary = secondary_value.and_then(|value| {
        parse_strict_window(Some(value), UsageWindowKind::Secondary, "Secondary")
    });
    let (additional, additional_decode_failed) = parse_additional_windows(&root);
    let credits = parse_credits(&root);
    let spend = parse_spend(&root);
    let primary_decode_failed =
        primary_value.is_some_and(|value| !value.is_null() && primary.is_none());
    let secondary_decode_failed =
        secondary_value.is_some_and(|value| !value.is_null() && secondary.is_none());
    let data_confidence =
        if primary_decode_failed || secondary_decode_failed || additional_decode_failed {
            "unknown"
        } else {
            "authoritative"
        };
    if primary.is_none()
        && secondary.is_none()
        && additional.is_empty()
        && credits.is_none()
        && spend.is_none()
    {
        return Ok(Ok(None));
    }
    let mut metrics = Vec::new();
    if let Some(window) = primary.as_ref() {
        metrics.push(metric("primary", window));
    }
    if let Some(window) = secondary.as_ref() {
        metrics.push(metric("secondary", window));
    }
    for window in &additional {
        metrics.push(metric(&window.key, &window.window));
    }
    let primary_window_kind = primary.as_ref().map(primary_window_kind);
    Ok(Ok(Some(UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id,
        plan_type: json_string(&root, &["plan_type", "planType"]),
        primary,
        primary_window_kind,
        primary_window_is_synthetic: false,
        secondary,
        additional_windows: additional,
        credits,
        credit_inventory: None,
        spend,
        observed_email: None,
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics,
        source_diagnostics: Vec::new(),
        provider_id: OPENAI.to_owned(),
        source: Some("api".to_owned()),
        data_confidence: data_confidence.to_owned(),
    })))
}

fn parse_strict_window(
    value: Option<&Value>,
    kind: UsageWindowKind,
    name: &str,
) -> Option<RateLimitWindow> {
    let value = value?.as_object()?;
    let value = Value::Object(value.clone());
    let used_percent = json_number(&value, &["used_percent"])?;
    if !used_percent.is_finite() || used_percent < 0.0 {
        return None;
    }
    let reset_at_utc = parse_window_reset(&value);
    reset_at_utc?;
    let seconds = json_number(&value, &["limit_window_seconds"])?;
    if !seconds.is_finite() || seconds < 1.0 || seconds > i64::MAX as f64 {
        return None;
    }
    let limit_window_seconds = seconds as i64;
    Some(RateLimitWindow {
        kind,
        name: name.to_owned(),
        used_percent,
        reset_at_utc,
        limit_window_seconds,
    })
}

fn parse_window_reset(value: &Value) -> Option<DateTime<Utc>> {
    let reset = value.get("reset_at")?;
    match reset {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|value| value.with_timezone(&Utc)),
        Value::Number(_) => epoch_value(reset),
        _ => None,
    }
}

fn epoch_value(value: &Value) -> Option<DateTime<Utc>> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse::<f64>().ok())?;
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    if number > 100_000_000_000.0 {
        DateTime::<Utc>::from_timestamp_millis(number as i64)
    } else {
        DateTime::<Utc>::from_timestamp(number as i64, 0)
    }
}

fn primary_window_kind(window: &RateLimitWindow) -> UsagePrimaryWindowKind {
    match window.limit_window_seconds {
        seconds if seconds > 0 && seconds <= 6 * 60 * 60 => UsagePrimaryWindowKind::Session,
        seconds if seconds >= 6 * 24 * 60 * 60 => UsagePrimaryWindowKind::Weekly,
        _ => UsagePrimaryWindowKind::Other,
    }
}

fn parse_additional_windows(root: &Value) -> (Vec<crate::usage::AdditionalRateLimitWindow>, bool) {
    let Some(additional) = root.get("additional_rate_limits") else {
        return (Vec::new(), false);
    };
    let mut result = Vec::new();
    let mut decode_failed = false;
    if let Some(object) = additional.as_object() {
        for (key, value) in object {
            decode_failed |= parse_additional_entry(key, value, &mut result);
        }
    } else if let Some(array) = additional.as_array() {
        for (index, value) in array.iter().enumerate() {
            let key =
                json_string(value, &["id", "key"]).unwrap_or_else(|| format!("additional_{index}"));
            decode_failed |= parse_additional_entry(&key, value, &mut result);
        }
    } else if !additional.is_null() {
        decode_failed = true;
    }
    (result, decode_failed)
}

fn parse_additional_entry(
    key: &str,
    value: &Value,
    result: &mut Vec<crate::usage::AdditionalRateLimitWindow>,
) -> bool {
    let Some(value_object) = value.as_object() else {
        return !value.is_null();
    };
    let value = Value::Object(value_object.clone());
    let label = json_string(&value, &["limit_name", "name", "metered_feature"])
        .unwrap_or_else(|| key.to_owned());
    let source = value.get("rate_limit").unwrap_or(&value);
    let spark = label.to_ascii_lowercase().contains("spark")
        || json_string(&value, &["metered_feature"])
            .is_some_and(|feature| feature.to_ascii_lowercase().contains("spark"));
    if spark {
        let mut decode_failed = false;
        let primary_value = source.get("primary_window");
        let secondary_value = source.get("secondary_window");
        if let Some(window) = parse_strict_window(
            primary_value,
            UsageWindowKind::Additional,
            "Codex Spark 5-hour",
        ) {
            result.push(crate::usage::AdditionalRateLimitWindow {
                key: "codex-spark".to_owned(),
                name: "Codex Spark 5-hour".to_owned(),
                window,
            });
        } else if primary_value.is_some_and(|value| !value.is_null()) {
            decode_failed = true;
        }
        if let Some(window) = parse_strict_window(
            secondary_value,
            UsageWindowKind::Additional,
            "Codex Spark Weekly",
        ) {
            result.push(crate::usage::AdditionalRateLimitWindow {
                key: "codex-spark-weekly".to_owned(),
                name: "Codex Spark Weekly".to_owned(),
                window,
            });
        } else if secondary_value.is_some_and(|value| !value.is_null()) {
            decode_failed = true;
        }
        return decode_failed;
    }

    let source_window = source
        .get("primary_window")
        .or_else(|| source.get("secondary_window"));
    if let Some(window) = parse_strict_window(source_window, UsageWindowKind::Additional, &label) {
        let id_source = json_string(&value, &["metered_feature", "limit_name"])
            .unwrap_or_else(|| key.to_owned());
        let stable_key = format!("codex-{}", slug(&id_source));
        if !stable_key.ends_with("-") && !result.iter().any(|item| item.key == stable_key) {
            result.push(crate::usage::AdditionalRateLimitWindow {
                key: stable_key,
                name: label,
                window,
            });
        }
        false
    } else {
        source_window.is_some_and(|value| !value.is_null())
    }
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut last_was_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            result.push('-');
            last_was_dash = true;
        }
    }
    result.trim_matches('-').to_owned()
}

fn parse_credits(root: &Value) -> Option<CreditsSnapshot> {
    let credit_limit = parse_credit_limit(root);
    let credits = root.get("credits");
    if credits.is_none() && credit_limit.is_none() {
        return None;
    }
    let balance = credits.and_then(|value| json_number(value, &["balance"]));
    let has_credits = credits.and_then(|value| json_bool(value, &["has_credits", "hasCredits"]));
    let unlimited = credits.and_then(|value| json_bool(value, &["unlimited"]));
    let result = CreditsSnapshot {
        has_credits,
        unlimited,
        balance,
        currency_code: None,
        approximate_message_cost: credits
            .and_then(|value| json_number(value, &["approximate_message_cost"])),
        limit: credit_limit,
        balance_read_succeeded: Some(balance.is_some()),
        credits_available: credits
            .and_then(|value| json_bool(value, &["available", "credits_available"]))
            .or_else(|| {
                has_credits
                    .or(unlimited)
                    .or_else(|| balance.map(|value| value > 0.0))
            }),
    };
    (result.has_credits.is_some()
        || result.unlimited.is_some()
        || result.balance.is_some()
        || result.approximate_message_cost.is_some()
        || result.limit.is_some())
    .then_some(result)
}

fn parse_credit_limit(root: &Value) -> Option<CreditLimitSnapshot> {
    let source = root
        .get("individual_limit")
        .or_else(|| root.get("individualLimit"))
        .or_else(|| {
            root.get("rate_limit").and_then(|value| {
                value
                    .get("individual_limit")
                    .or_else(|| value.get("individualLimit"))
            })
        })
        .or_else(|| {
            root.get("spend_control")
                .or_else(|| root.get("spendControl"))
                .and_then(|value| {
                    value
                        .get("individual_limit")
                        .or_else(|| value.get("individualLimit"))
                })
        })?;
    let limit = json_number(source, &["limit"]).filter(|value| value.is_finite() && *value > 0.0);
    let remaining_percent = json_number(source, &["remaining_percent", "remainingPercent"])
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
    let used = json_number(source, &["used"])
        .filter(|value| value.is_finite() && *value >= 0.0)
        .or_else(|| {
            limit
                .zip(remaining_percent)
                .map(|(limit, remaining)| limit * (100.0 - remaining) / 100.0)
        });
    let used_percent = json_number(source, &["used_percent", "usedPercent"])
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .or_else(|| {
            used.zip(limit)
                .map(|(used, limit)| (used / limit * 100.0).clamp(0.0, 100.0))
        })
        .or_else(|| remaining_percent.map(|remaining| (100.0 - remaining).clamp(0.0, 100.0)));
    let remaining = json_number(source, &["remaining"])
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| limit.map_or(value, |limit| value.min(limit)))
        .or_else(|| limit.zip(used).map(|(limit, used)| (limit - used).max(0.0)))
        .or_else(|| {
            limit
                .zip(remaining_percent)
                .map(|(limit, remaining)| limit * remaining / 100.0)
        });
    let reset_at_utc = json_datetime(source, &["resets_at", "resetsAt", "reset_at", "resetAt"]);
    let unit = json_string(source, &["unit", "currency"]);
    limit.is_some().then_some(CreditLimitSnapshot {
        limit,
        used,
        remaining,
        used_percent,
        reset_at_utc,
        unit,
        read_succeeded: true,
    })
}

fn parse_spend(root: &Value) -> Option<SpendSnapshot> {
    if let Some(limit) = parse_credit_limit(root) {
        return Some(SpendSnapshot {
            monthly_usage: limit.used,
            monthly_limit: limit.limit,
            used_percent: limit.used_percent,
            limit_enabled: Some(limit.limit.is_some()),
            currency_code: None,
        });
    }
    let source = root
        .get("spend_controls")
        .or_else(|| root.get("spend_control"))
        .unwrap_or(root);
    let result = SpendSnapshot {
        monthly_usage: json_number(source, &["monthly_usage", "used"]),
        monthly_limit: json_number(source, &["monthly_limit", "limit"]),
        used_percent: json_number(source, &["used_percent"]),
        limit_enabled: json_bool(source, &["limit_enabled"]),
        currency_code: None,
    };
    (result.monthly_usage.is_some()
        || result.monthly_limit.is_some()
        || result.used_percent.is_some()
        || result.limit_enabled.is_some())
    .then_some(result)
}

fn parse_credit_inventory(body: &str) -> Option<UsageCreditInventory> {
    let root: Value = serde_json::from_str(body).ok()?;
    let entries = root.get("credits")?.as_array()?;
    let available_count = json_number(&root, &["available_count", "availableCount"])
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u32)
        .unwrap_or_else(|| {
            entries
                .iter()
                .filter(|entry| {
                    json_string(entry, &["status"])
                        .is_none_or(|status| status.eq_ignore_ascii_case("available"))
                })
                .count() as u32
        });
    let credits = entries
        .iter()
        .map(|entry| UsageCreditRecord {
            id: json_string(entry, &["id"]),
            reset_type: json_string(entry, &["reset_type", "resetType"]),
            status: json_string(entry, &["status"]),
            granted_at_utc: json_datetime(entry, &["granted_at", "grantedAt"]),
            expires_at_utc: json_datetime(entry, &["expires_at", "expiresAt"]),
            redeem_started_at_utc: json_datetime(entry, &["redeem_started_at", "redeemStartedAt"]),
            redeemed_at_utc: json_datetime(entry, &["redeemed_at", "redeemedAt"]),
            title: json_string(entry, &["title"]),
            description: json_string(entry, &["description"]),
        })
        .collect::<Vec<_>>();
    Some(UsageCreditInventory {
        available_count,
        credits,
    })
}

fn json_datetime(value: &Value, names: &[&str]) -> Option<DateTime<Utc>> {
    names
        .iter()
        .find_map(|name| value.get(*name))
        .and_then(epoch_or_rfc3339)
}

fn epoch_or_rfc3339(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|value| value.with_timezone(&Utc))
            .or_else(|| epoch_value(value)),
        Value::Number(_) => epoch_value(value),
        _ => None,
    }
}

fn metric(key: &str, window: &RateLimitWindow) -> UsageMetric {
    UsageMetric {
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
    }
}

fn resolve_base_url(environment: &HashMap<String, String>) -> Result<Url, TransportError> {
    let configured = environment
        .get("CODEX_CHATGPT_BASE_URL")
        .or_else(|| environment.get("CHATGPT_BASE_URL"))
        .and_then(|value| non_empty(value));
    let raw = configured.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let mut url =
        Url::parse(&raw).map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    if matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
        && !url.path().contains("/backend-api")
    {
        url.set_path("/backend-api/");
    }
    if !url.path().ends_with('/') {
        let next = format!("{}/", url.path());
        url.set_path(&next);
    }
    Ok(url)
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['"', '\'']);
    (!value.is_empty()).then_some(value.to_owned())
}

fn is_backend_api_base(url: &Url) -> bool {
    url.path().trim_end_matches('/').ends_with("/backend-api")
}

#[cfg(test)]
mod tests;
