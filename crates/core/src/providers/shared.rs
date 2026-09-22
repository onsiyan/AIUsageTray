use crate::{
    auth::AccountAuthMaterial,
    transport::UsageHttpResponse,
    usage::{UsageAdapterError, UsageAdapterErrorCode, UsageProbeResult},
};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

pub fn bearer_headers(
    material: &AccountAuthMaterial,
    user_agent: &str,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([
        ("Accept".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), user_agent.to_owned()),
    ]);
    if let Some(token) = material.bearer_token.as_deref() {
        headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
    }
    if let Some(cookie) = material.cookie_header() {
        headers.insert("Cookie".to_owned(), cookie);
    }
    headers
}

pub fn missing_auth(provider: &str) -> UsageProbeResult {
    UsageProbeResult::failure(UsageAdapterError {
        code: UsageAdapterErrorCode::AuthenticationUnavailable,
        message: format!("{provider} credentials are unavailable"),
        http_status_code: None,
        retry_after_seconds: None,
    })
}

pub fn invalid_payload(provider: &str, reason: impl Into<String>) -> UsageProbeResult {
    UsageProbeResult::failure(UsageAdapterError {
        code: UsageAdapterErrorCode::InvalidPayload,
        message: format!("{provider} returned an invalid payload: {}", reason.into()),
        http_status_code: None,
        retry_after_seconds: None,
    })
}

pub fn map_http_error(response: &UsageHttpResponse, provider: &str) -> UsageProbeResult {
    let code = match response.status_code {
        401 => UsageAdapterErrorCode::Unauthorized,
        403 => UsageAdapterErrorCode::Forbidden,
        429 => UsageAdapterErrorCode::RateLimited,
        500..=599 => UsageAdapterErrorCode::TransientHttp,
        _ => UsageAdapterErrorCode::HttpError,
    };
    UsageProbeResult::failure(UsageAdapterError {
        code,
        message: format!("{provider} returned HTTP {}", response.status_code),
        http_status_code: Some(response.status_code),
        retry_after_seconds: retry_after_seconds(response),
    })
}

/// Antigravity uses `RESOURCE_EXHAUSTED` for both a temporary upstream rate
/// limit and an actual quota exhaustion. The response body is the only place
/// that distinguishes the two, so keep that distinction at the provider
/// boundary instead of making every caller infer it from HTTP 429.
pub fn map_antigravity_http_error(
    response: &UsageHttpResponse,
    operation: &str,
) -> UsageProbeResult {
    let body = response.body.to_ascii_lowercase();
    let quota_exhausted = (response.status_code == 429 || body.contains("resource_exhausted"))
        && (body.contains("quota_exhausted")
            || body.contains("quota exhausted")
            || body.contains("quota exceeded")
            || body.contains("quota limit")
            || body.contains("weekly limit")
            || body.contains("five hour limit")
            || body.contains("5-hour limit")
            || body.contains("quotaresetdelay")
            || body.contains("quota_reset_delay")
            || body.contains("quotaresettimestamp")
            || body.contains("quota_reset"));
    let code = match response.status_code {
        401 => UsageAdapterErrorCode::Unauthorized,
        403 => UsageAdapterErrorCode::Forbidden,
        429 if quota_exhausted => UsageAdapterErrorCode::QuotaExhausted,
        429 => UsageAdapterErrorCode::RateLimited,
        500..=599 => UsageAdapterErrorCode::TransientHttp,
        _ if quota_exhausted => UsageAdapterErrorCode::QuotaExhausted,
        _ => UsageAdapterErrorCode::HttpError,
    };
    let message = match code {
        UsageAdapterErrorCode::QuotaExhausted => {
            format!("Antigravity quota exhausted during {operation}")
        }
        UsageAdapterErrorCode::RateLimited => {
            format!("Antigravity request rate-limited during {operation}")
        }
        _ => format!(
            "Antigravity {operation} returned HTTP {}",
            response.status_code
        ),
    };
    UsageProbeResult::failure(UsageAdapterError {
        code,
        message,
        http_status_code: Some(response.status_code),
        retry_after_seconds: retry_after_seconds(response),
    })
}

fn retry_after_seconds(response: &UsageHttpResponse) -> Option<u64> {
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
}

pub fn json_string(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| match value.get(*name) {
        Some(serde_json::Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        Some(serde_json::Value::Number(value)) => Some(value.to_string()),
        _ => None,
    })
}

pub fn json_number(value: &serde_json::Value, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| match value.get(*name) {
        Some(serde_json::Value::Number(value)) => value.as_f64(),
        Some(serde_json::Value::String(value)) => value.parse::<f64>().ok(),
        _ => None,
    })
}

pub fn json_bool(value: &serde_json::Value, names: &[&str]) -> Option<bool> {
    names.iter().find_map(|name| match value.get(*name) {
        Some(serde_json::Value::Bool(value)) => Some(*value),
        Some(serde_json::Value::String(value)) => value.parse::<bool>().ok(),
        _ => None,
    })
}

pub fn normalize_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

pub fn parse_datetime(value: &serde_json::Value, names: &[&str]) -> Option<DateTime<Utc>> {
    let text = json_string(value, names)?;
    DateTime::parse_from_rfc3339(&text)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

pub fn reset_at(value: &serde_json::Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    parse_datetime(
        value,
        &[
            "resetAt",
            "reset_at",
            "resetTime",
            "reset_time",
            "resetsAt",
            "resets_at",
        ],
    )
    .or_else(|| {
        json_number(
            value,
            &[
                "resetInSec",
                "reset_in_sec",
                "resetSeconds",
                "reset_seconds",
            ],
        )
        .map(|seconds| now + Duration::seconds(seconds.max(0.0) as i64))
    })
}

pub fn session_key(material: &AccountAuthMaterial) -> Option<String> {
    material
        .cookies
        .iter()
        .find(|cookie| cookie.name.eq_ignore_ascii_case("sessionKey"))
        .map(|cookie| cookie.value.clone())
        .or_else(|| material.bearer_token.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status_code: u16, body: &str) -> UsageHttpResponse {
        UsageHttpResponse {
            status_code,
            body: body.to_owned(),
            headers: BTreeMap::from([("Retry-After".to_owned(), "17".to_owned())]),
        }
    }

    #[test]
    fn antigravity_distinguishes_explicit_quota_exhaustion() {
        let result = map_antigravity_http_error(
            &response(
                429,
                r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"weekly limit reached"}}"#,
            ),
            "retrieveUserQuotaSummary",
        );
        let error = result.error.unwrap();
        assert_eq!(error.code, UsageAdapterErrorCode::QuotaExhausted);
        assert_eq!(error.retry_after_seconds, Some(17));
    }

    #[test]
    fn generic_resource_exhausted_remains_retryable_rate_limit() {
        let result = map_antigravity_http_error(
            &response(
                429,
                r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"try again later"}}"#,
            ),
            "fetchAvailableModels",
        );
        assert_eq!(
            result.error.unwrap().code,
            UsageAdapterErrorCode::RateLimited
        );
    }
}
