use crate::{
    accounts::{AccountRecord, OPENAI, VerifiedIdentity},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError, OAuthProviderDefinition},
    providers::shared::{
        bearer_headers, invalid_payload, json_bool, json_number, json_string, map_http_error,
        missing_auth, normalize_percent,
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
    fetch_spend_controls: bool,
    fetch_workspace_balance: bool,
    fetch_reset_credits: bool,
}

impl WhamUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        fetch_spend_controls: bool,
        fetch_workspace_balance: bool,
    ) -> Result<Self, TransportError> {
        let environment = env::vars().collect::<HashMap<_, _>>();
        Ok(Self {
            transport,
            auth,
            base_url: resolve_base_url(&environment)?,
            fetch_spend_controls,
            fetch_workspace_balance,
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
            material
                .user_agent
                .as_deref()
                .unwrap_or("CodexUsageMonitor/0.1"),
        );
        if include_account_header {
            if let Some(account_id) = account.and_then(|account| account.workspace_id.as_deref()) {
                headers.insert("ChatGPT-Account-Id".to_owned(), account_id.to_owned());
            }
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

        let workspace_account_id = account
            .workspace_id
            .clone()
            .or_else(|| snapshot.response_account_id.clone());
        let is_workspace_plan =
            is_backend_api_base(&base_url) && is_workspace_plan(snapshot.plan_type.as_deref());
        let monthly_usage_path = if is_workspace_plan && self.fetch_spend_controls {
            workspace_account_id.as_deref().map(|account_id| {
                // Relative to the `/backend-api/` base URL; a leading
                // `/backend-api` segment would be duplicated by `Url::join`.
                format!(
                    "accounts/{}/spend-controls/current-user/monthly-usage",
                    percent_encode(account_id)
                )
            })
        } else {
            None
        };
        let needs_workspace_balance = self.fetch_workspace_balance
            && snapshot
                .credits
                .as_ref()
                .and_then(|credits| credits.balance)
                .is_none();
        let workspace_balance_path = if is_workspace_plan && needs_workspace_balance {
            workspace_account_id.as_deref().map(|account_id| {
                format!("accounts/{}/remaining_balance", percent_encode(account_id))
            })
        } else {
            None
        };
        let monthly_usage_request = async {
            match monthly_usage_path.as_deref() {
                Some(path) => Some(self.get(path, Some(account), &material, true).await),
                None => None,
            }
        };
        let workspace_balance_request = async {
            match workspace_balance_path.as_deref() {
                Some(path) => Some(self.get(path, Some(account), &material, true).await),
                None => None,
            }
        };
        let (monthly_usage_response, workspace_balance_response) =
            tokio::join!(monthly_usage_request, workspace_balance_request);

        if let Some(monthly_usage_response) = monthly_usage_response {
            match monthly_usage_response {
                Ok(response) if response.is_success() => {
                    if let Some(enrichment) = parse_monthly_usage(&response.body) {
                        snapshot.spend = Some(merge_spend(snapshot.spend.take(), enrichment));
                    } else {
                        source_diagnostics
                            .push(optional_payload_diagnostic("workspace.monthly-usage"));
                    }
                }
                Ok(response) => source_diagnostics.push(optional_response_diagnostic(
                    "workspace.monthly-usage",
                    &response,
                )),
                Err(error) => source_diagnostics.push(optional_transport_diagnostic(
                    "workspace.monthly-usage",
                    &error,
                )),
            }
        }

        if let Some(workspace_balance_response) = workspace_balance_response {
            match workspace_balance_response {
                Ok(response) if response.is_success() => {
                    if let Some(balance) = parse_balance(&response.body) {
                        snapshot.credits = Some(CreditsSnapshot {
                            has_credits: snapshot
                                .credits
                                .as_ref()
                                .and_then(|credits| credits.has_credits),
                            unlimited: snapshot
                                .credits
                                .as_ref()
                                .and_then(|credits| credits.unlimited),
                            balance: Some(balance),
                            currency_code: snapshot
                                .credits
                                .as_ref()
                                .and_then(|credits| credits.currency_code.clone()),
                            approximate_message_cost: snapshot
                                .credits
                                .as_ref()
                                .and_then(|credits| credits.approximate_message_cost),
                            limit: snapshot
                                .credits
                                .as_ref()
                                .and_then(|credits| credits.limit.clone()),
                            balance_read_succeeded: Some(true),
                            credits_available: Some(balance > 0.0),
                        });
                    } else {
                        source_diagnostics
                            .push(optional_payload_diagnostic("workspace.remaining-balance"));
                    }
                }
                Ok(response) => source_diagnostics.push(optional_response_diagnostic(
                    "workspace.remaining-balance",
                    &response,
                )),
                Err(error) => source_diagnostics.push(optional_transport_diagnostic(
                    "workspace.remaining-balance",
                    &error,
                )),
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
    if reset_at_utc.is_none() {
        return None;
    }
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

fn parse_monthly_usage(body: &str) -> Option<SpendSnapshot> {
    let root: Value = serde_json::from_str(body).ok()?;
    let usage = json_number(&root, &["current_month_usage"]);
    let limit_object = root.get("effective_monthly_limit");
    let limit = limit_object.and_then(|value| json_number(value, &["limit"]));
    let mode = limit_object.and_then(|value| json_string(value, &["enforcement_mode"]));
    let limit_enabled = mode.map(|mode| {
        !matches!(
            mode.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled" | "no_limit"
        )
    });
    let used_percent = usage
        .zip(limit.filter(|value| *value > 0.0))
        .map(|(usage, limit)| normalize_percent(usage / limit * 100.0));
    (usage.is_some() || limit.is_some() || used_percent.is_some() || limit_enabled.is_some())
        .then_some(SpendSnapshot {
            monthly_usage: usage,
            monthly_limit: limit,
            used_percent,
            limit_enabled,
            currency_code: None,
        })
}

fn parse_balance(body: &str) -> Option<f64> {
    let root: Value = serde_json::from_str(body).ok()?;
    json_number(&root, &["balance"]).map(|value| value.max(0.0))
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

fn merge_spend(current: Option<SpendSnapshot>, enrichment: SpendSnapshot) -> SpendSnapshot {
    let Some(current) = current else {
        return enrichment;
    };
    let currencies_match = match (
        current.currency_code.as_deref(),
        enrichment.currency_code.as_deref(),
    ) {
        (Some(current), Some(enrichment)) => current.eq_ignore_ascii_case(enrichment),
        (None, None) => true,
        _ => false,
    };
    if !currencies_match {
        return match (
            current.currency_code.is_some(),
            enrichment.currency_code.is_some(),
        ) {
            (true, false) => current,
            _ => enrichment,
        };
    }

    SpendSnapshot {
        monthly_usage: enrichment.monthly_usage.or(current.monthly_usage),
        monthly_limit: enrichment.monthly_limit.or(current.monthly_limit),
        used_percent: enrichment.used_percent.or(current.used_percent),
        limit_enabled: enrichment.limit_enabled.or(current.limit_enabled),
        currency_code: enrichment.currency_code.or(current.currency_code),
    }
}

fn is_workspace_plan(plan_type: Option<&str>) -> bool {
    matches!(
        plan_type
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some(
            "team"
                | "business"
                | "education"
                | "quorum"
                | "k12"
                | "enterprise"
                | "edu"
                | "free_workspace"
        )
    )
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

fn percent_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
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
mod tests {
    use super::*;
    use crate::{
        accounts::AccountRecord, transport::UsageHttpRequest, usage::UsageAdapterErrorCode,
    };
    use async_trait::async_trait;
    use std::{collections::BTreeMap, sync::Mutex};

    #[test]
    fn codex_oauth_definition_uses_openai_endpoints_and_loopback_callback() {
        let provider = oauth_definition();

        assert_eq!(
            provider.authorization_endpoint.as_str(),
            "https://auth.openai.com/oauth/authorize"
        );
        assert_eq!(
            provider.token_endpoint.as_str(),
            "https://auth.openai.com/oauth/token"
        );
        assert_eq!(
            provider.redirect_uri.as_str(),
            "http://localhost:1455/auth/callback"
        );
        assert_eq!(provider.client_id, CODEX_OAUTH_CLIENT_ID);
        assert!(provider.client_secret.is_none());
        assert_eq!(
            provider.scopes,
            [
                "openid",
                "profile",
                "email",
                "offline_access",
                "api.connectors.read",
                "api.connectors.invoke",
            ]
        );
        assert_eq!(
            provider
                .authorization_parameters
                .get("id_token_add_organizations"),
            Some(&"true".to_owned())
        );
        assert_eq!(
            provider
                .authorization_parameters
                .get("codex_cli_simplified_flow"),
            Some(&"true".to_owned())
        );
        assert_eq!(
            provider.authorization_parameters.get("originator"),
            Some(&"codex_usage_monitor_rust".to_owned())
        );
    }

    #[test]
    fn parses_numeric_reset_and_stable_spark_windows() {
        let account = AccountRecord::create(
            "codex",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("acct-1".to_owned()),
        )
        .unwrap();
        let body = r#"{
            "account_id":"acct-1",
            "plan_type":"plus",
            "rate_limit":{"primary_window":{"used_percent":40,"reset_at":4102444800,"limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"GPT-5.3-Codex-Spark","metered_feature":"spark","rate_limit":{"primary_window":{"used_percent":12,"reset_at":4102444800,"limit_window_seconds":18000},"secondary_window":{"used_percent":4,"reset_at":4103049600,"limit_window_seconds":604800}}}]
        }"#;
        let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
        assert_eq!(
            snapshot.primary_window_kind,
            Some(UsagePrimaryWindowKind::Session)
        );
        assert_eq!(snapshot.additional_windows.len(), 2);
        assert_eq!(snapshot.additional_windows[0].key, "codex-spark");
        assert_eq!(snapshot.additional_windows[1].key, "codex-spark-weekly");
        assert_eq!(
            snapshot.primary.unwrap().reset_at_utc.unwrap().timestamp(),
            4102444800
        );
    }

    #[test]
    fn spend_enrichment_does_not_merge_amounts_with_different_currencies() {
        let current = SpendSnapshot {
            monthly_usage: Some(10.0),
            monthly_limit: Some(100.0),
            used_percent: Some(10.0),
            limit_enabled: Some(true),
            currency_code: Some("USD".to_owned()),
        };
        let enrichment = SpendSnapshot {
            monthly_usage: Some(20.0),
            monthly_limit: None,
            used_percent: None,
            limit_enabled: None,
            currency_code: Some("EUR".to_owned()),
        };

        let merged = merge_spend(Some(current), enrichment);
        assert_eq!(merged.monthly_usage, Some(20.0));
        assert_eq!(merged.monthly_limit, None);
        assert_eq!(merged.currency_code.as_deref(), Some("EUR"));
    }

    #[test]
    fn non_spark_extra_prefers_primary_and_uses_a_stable_slug() {
        let account =
            AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();
        let body = r#"{
            "rate_limit":{"primary_window":{"used_percent":1,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"Model Family / Pro","rate_limit":{"primary_window":{"used_percent":2,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000},"secondary_window":{"used_percent":8,"reset_at":"2030-01-02T00:00:00Z","limit_window_seconds":604800}}}]
        }"#;
        let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
        assert_eq!(snapshot.additional_windows.len(), 1);
        assert_eq!(snapshot.additional_windows[0].key, "codex-model-family-pro");
        assert_eq!(snapshot.additional_windows[0].window.used_percent, 2.0);
    }

    #[test]
    fn reset_credit_inventory_keeps_expiry_and_available_count() {
        let inventory = parse_credit_inventory(
            r#"{"available_count":2,"credits":[{"id":"c1","status":"available","granted_at":"2030-01-01T00:00:00Z","expires_at":4102444800,"title":"Five hour","description":"reset"}]}"#,
        )
        .unwrap();
        assert_eq!(inventory.available_count, 2);
        assert_eq!(inventory.credits[0].id.as_deref(), Some("c1"));
        assert_eq!(
            inventory.credits[0].expires_at_utc.unwrap().timestamp(),
            4102444800
        );
    }

    #[test]
    fn individual_limit_precedence_is_exposed_with_credit_reset() {
        let account =
            AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();
        let body = r#"{
            "individual_limit":{"limit":100,"used":25,"remaining_percent":75,"reset_at":4102444800},
            "rate_limit":{"primary_window":{"used_percent":10,"reset_at":4102444800,"limit_window_seconds":18000}},
            "credits":{"has_credits":true,"balance":0}
        }"#;
        let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
        let credits = snapshot.credits.unwrap();
        let limit = credits.limit.unwrap();
        assert_eq!(limit.limit, Some(100.0));
        assert_eq!(limit.used, Some(25.0));
        assert_eq!(limit.remaining, Some(75.0));
        assert_eq!(limit.used_percent, Some(25.0));
        assert_eq!(snapshot.data_confidence, "authoritative");
    }

    #[test]
    fn over_quota_primary_window_is_preserved_and_secondary_remains_available() {
        let account =
            AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();
        let body = r#"{
            "rate_limit":{
                "primary_window":{"used_percent":101,"reset_at":4102444800,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":20,"reset_at":4103049600,"limit_window_seconds":604800}
            }
        }"#;
        let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
        assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 101.0);
        assert_eq!(snapshot.primary.as_ref().unwrap().remaining_percent(), 0.0);
        assert!(snapshot.secondary.is_some());
        assert_eq!(snapshot.data_confidence, "authoritative");
    }

    #[test]
    fn configured_root_uses_codex_usage_fallback_and_backend_base_keeps_wham_path() {
        let root = resolve_base_url(&HashMap::from([(
            "CODEX_CHATGPT_BASE_URL".to_owned(),
            "https://example.test".to_owned(),
        )]))
        .unwrap();
        assert_eq!(root.as_str(), "https://example.test/");
        assert!(!is_backend_api_base(&root));

        let backend = resolve_base_url(&HashMap::from([(
            "CODEX_CHATGPT_BASE_URL".to_owned(),
            "https://example.test/backend-api".to_owned(),
        )]))
        .unwrap();
        assert!(is_backend_api_base(&backend));
    }

    #[tokio::test]
    async fn account_oauth_bearer_queries_wham_without_cookies_or_session_preflight() {
        let transport = Arc::new(CodexUsageTransport::default());
        let auth = Arc::new(StaticOAuthAuth(AccountAuthMaterial {
            bearer_token: Some("account-scoped-oauth-token".to_owned()),
            secondary_bearer_token: Some("unverified-secondary-token".to_owned()),
            oauth_access_token: Some("unverified-oauth-token".to_owned()),
            cookies: vec![crate::auth::CookieValue {
                name: "legacy-session-cookie".to_owned(),
                value: "must-not-be-sent".to_owned(),
            }],
            ..AccountAuthMaterial::default()
        }));
        let adapter = WhamUsageAdapter::new(transport.clone(), auth, false, false)
            .unwrap()
            .with_reset_credits(false);
        let account = AccountRecord::create(
            "codex",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("acct-1".to_owned()),
        )
        .unwrap();

        let result = adapter.probe(&account).await.unwrap();

        assert!(result.succeeded());
        assert_eq!(
            result
                .identity
                .as_ref()
                .and_then(|identity| identity.provider_account_id.as_deref()),
            Some("chatgpt-user-1")
        );
        let snapshot = result.snapshot.unwrap();
        assert_eq!(snapshot.source.as_deref(), Some("codex-oauth"));
        assert_eq!(
            snapshot.observed_email.as_deref(),
            Some("codex@example.com")
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/backend-api/wham/usage");
        assert_eq!(
            requests[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer account-scoped-oauth-token")
        );
        assert!(!requests[0].headers.contains_key("Cookie"));
        assert_eq!(
            requests[0]
                .headers
                .get("ChatGPT-Account-Id")
                .map(String::as_str),
            Some("acct-1")
        );
    }

    #[tokio::test]
    async fn codex_independent_usage_enrichment_requests_run_concurrently() {
        let transport = Arc::new(ParallelCodexRequestTransport {
            usage_and_reset_barrier: tokio::sync::Barrier::new(2),
            workspace_enrichment_barrier: tokio::sync::Barrier::new(2),
        });
        let auth = Arc::new(StaticOAuthAuth(oauth_material()));
        let adapter = WhamUsageAdapter::new(transport, auth, true, true)
            .unwrap()
            .with_reset_credits(true);
        let account = AccountRecord::create(
            "codex",
            "codex@example.com",
            None,
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap();

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), adapter.probe(&account))
                .await
                .expect("independent Codex usage requests should not wait on each other")
                .unwrap();

        assert!(result.succeeded());
    }

    #[tokio::test]
    async fn optional_endpoint_failures_are_reported_without_failing_codex_usage() {
        let transport = Arc::new(OptionalEndpointFailureTransport {
            requests: Mutex::new(Vec::new()),
        });
        let auth = Arc::new(StaticOAuthAuth(oauth_material()));
        let adapter = WhamUsageAdapter::new(transport.clone(), auth, true, true).unwrap();
        let account = AccountRecord::create(
            "codex",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("acct-1".to_owned()),
        )
        .unwrap();

        let result = adapter.probe(&account).await.unwrap();

        assert!(result.succeeded());
        let snapshot = result.snapshot.unwrap();
        assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 40.0);
        assert_eq!(
            snapshot
                .source_diagnostics
                .iter()
                .map(|diagnostic| diagnostic.source.as_str())
                .collect::<Vec<_>>(),
            [
                "wham.reset-credits",
                "workspace.monthly-usage",
                "workspace.remaining-balance",
            ]
        );
        for diagnostic in &snapshot.source_diagnostics {
            assert_eq!(diagnostic.code, UsageAdapterErrorCode::Forbidden);
            assert_eq!(diagnostic.http_status_code, Some(403));
            assert_eq!(diagnostic.retry_after_seconds, Some(17));
            assert!(!diagnostic.message.contains("private response body"));
        }
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let paths = requests
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>();
        assert!(paths.contains(
            &"/backend-api/accounts/acct-1/spend-controls/current-user/monthly-usage".to_owned()
        ));
        assert!(paths.contains(&"/backend-api/accounts/acct-1/remaining_balance".to_owned()));
    }

    #[tokio::test]
    async fn cookie_only_material_is_rejected_without_a_browser_session_request() {
        let transport = Arc::new(CodexUsageTransport::default());
        let auth = Arc::new(StaticOAuthAuth(AccountAuthMaterial {
            cookies: vec![crate::auth::CookieValue {
                name: "session".to_owned(),
                value: "browser-cookie".to_owned(),
            }],
            ..AccountAuthMaterial::default()
        }));
        let adapter = WhamUsageAdapter::new(transport.clone(), auth, false, false).unwrap();
        let account =
            AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();

        let result = adapter.probe(&account).await.unwrap();

        assert_eq!(
            result.error.unwrap().code,
            UsageAdapterErrorCode::AuthenticationUnavailable
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn wham_account_id_is_validated_against_workspace_not_user_identity() {
        let account = AccountRecord::create(
            "codex",
            "codex@example.com",
            Some("chatgpt-user-1".to_owned()),
            OPENAI,
            Some("workspace-1".to_owned()),
        )
        .unwrap();

        let matching = parse_wham_usage(
            &account,
            r#"{"account_id":"workspace-1","plan_type":"team","rate_limit":{"primary_window":{"used_percent":10,"reset_at":4102444800,"limit_window_seconds":18000}}}"#,
        )
        .unwrap();
        assert!(matches!(matching, Ok(Some(_))));

        let mismatched = parse_wham_usage(
            &account,
            r#"{"account_id":"other-workspace","plan_type":"team","rate_limit":{"primary_window":{"used_percent":10,"reset_at":4102444800,"limit_window_seconds":18000}}}"#,
        )
        .unwrap();
        assert!(matches!(
            mismatched,
            Err(error) if error.code == UsageAdapterErrorCode::AccountMismatch
        ));
    }

    fn oauth_material() -> AccountAuthMaterial {
        AccountAuthMaterial {
            bearer_token: Some("account-scoped-oauth-token".to_owned()),
            ..AccountAuthMaterial::default()
        }
    }

    struct StaticOAuthAuth(AccountAuthMaterial);

    #[async_trait]
    impl AccountAuthMaterialProvider for StaticOAuthAuth {
        async fn get(
            &self,
            _account: &AccountRecord,
        ) -> Result<Option<AccountAuthMaterial>, AuthError> {
            Ok(Some(self.0.clone()))
        }
    }

    #[derive(Default)]
    struct CodexUsageTransport {
        requests: Mutex<Vec<UsageHttpRequest>>,
    }

    struct OptionalEndpointFailureTransport {
        requests: Mutex<Vec<UsageHttpRequest>>,
    }

    struct ParallelCodexRequestTransport {
        usage_and_reset_barrier: tokio::sync::Barrier,
        workspace_enrichment_barrier: tokio::sync::Barrier,
    }

    #[async_trait]
    impl UsageHttpTransport for ParallelCodexRequestTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            let path = request.url.path();
            let (status_code, body) = if path.ends_with("/wham/usage") {
                self.usage_and_reset_barrier.wait().await;
                (
                    200,
                    r#"{"account_id":"workspace-1","plan_type":"team","rate_limit":{"primary_window":{"used_percent":40,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}}}"#.to_owned(),
                )
            } else if path.ends_with("/wham/rate-limit-reset-credits") {
                self.usage_and_reset_barrier.wait().await;
                (404, String::new())
            } else {
                self.workspace_enrichment_barrier.wait().await;
                (404, String::new())
            };
            Ok(UsageHttpResponse {
                status_code,
                body,
                headers: BTreeMap::new(),
            })
        }
    }

    #[async_trait]
    impl UsageHttpTransport for OptionalEndpointFailureTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            let path = request.url.path().to_owned();
            self.requests.lock().unwrap().push(request);
            let (status_code, body, headers) = match path.as_str() {
                "/backend-api/wham/usage" => (
                    200,
                    r#"{"account_id":"acct-1","plan_type":"team","rate_limit":{"primary_window":{"used_percent":40,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}}}"#.to_owned(),
                    BTreeMap::new(),
                ),
                _ => (
                    403,
                    "private response body".to_owned(),
                    BTreeMap::from([("Retry-After".to_owned(), "17".to_owned())]),
                ),
            };
            Ok(UsageHttpResponse {
                status_code,
                body,
                headers,
            })
        }
    }

    #[async_trait]
    impl UsageHttpTransport for CodexUsageTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            let path = request.url.path().to_owned();
            self.requests.lock().unwrap().push(request);
            let (status_code, body) = if path == "/backend-api/wham/usage" {
                (
                    200,
                    r#"{"account_id":"acct-1","plan_type":"plus","rate_limit":{"primary_window":{"used_percent":40,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}}}"#.to_owned(),
                )
            } else {
                (404, String::new())
            };
            Ok(UsageHttpResponse {
                status_code,
                body,
                headers: BTreeMap::new(),
            })
        }
    }
    struct HtmlUsageTransport;

    #[async_trait]
    impl UsageHttpTransport for HtmlUsageTransport {
        async fn send(
            &self,
            _request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            Ok(UsageHttpResponse {
                status_code: 200,
                body: "<html>maintenance</html>".to_owned(),
                headers: BTreeMap::new(),
            })
        }
    }

    #[tokio::test]
    async fn non_json_usage_body_is_an_invalid_payload_not_a_network_failure() {
        let auth = Arc::new(StaticOAuthAuth(oauth_material()));
        let adapter = WhamUsageAdapter::new(Arc::new(HtmlUsageTransport), auth, false, false)
            .unwrap()
            .with_reset_credits(false);
        let account =
            AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();

        let result = adapter.probe(&account).await.unwrap();
        assert_eq!(
            result.error.unwrap().code,
            UsageAdapterErrorCode::InvalidPayload
        );
    }
}
