//! Grouped quota summary: parsing, bucket windows, and the snapshot built from it.

use super::*;

#[derive(Clone, Debug)]
pub(super) struct LocalQuotaSummaryGroup {
    pub(super) buckets: Vec<LocalQuotaSummaryBucket>,
}

#[derive(Clone, Debug)]
pub(super) struct LocalQuotaSummaryBucket {
    pub(super) bucket_id: String,
    pub(super) display_name: String,
    pub(super) window: Option<String>,
    pub(super) remaining_fraction: Option<f64>,
    pub(super) reset_at_utc: Option<DateTime<Utc>>,
    pub(super) reset_description: Option<String>,
    pub(super) disabled: bool,
    pub(super) group_name: String,
}

pub(super) fn parse_quota_summary(root: &Value) -> Vec<LocalQuotaSummaryGroup> {
    let candidates = [root.get("response"), root.get("summary"), Some(root)];
    for candidate in candidates.into_iter().flatten() {
        let payload = candidate.get("quotaSummary").unwrap_or(candidate);
        let Some(groups) = payload.get("groups").and_then(Value::as_array) else {
            continue;
        };
        let parsed = groups
            .iter()
            .filter_map(parse_quota_summary_group)
            .collect::<Vec<_>>();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    Vec::new()
}

pub(super) fn has_usable_quota_summary(groups: &[LocalQuotaSummaryGroup]) -> bool {
    groups.iter().any(|group| {
        group.buckets.iter().any(|bucket| {
            !bucket.disabled
                && bucket.remaining_fraction.is_some_and(|remaining| {
                    remaining.is_finite() && (0.0..=1.0).contains(&remaining)
                })
        })
    })
}

pub(super) fn quota_summary_response_diagnostic(
    response: &UsageHttpResponse,
) -> UsageSourceDiagnostic {
    let code = match response.status_code {
        401 => crate::usage::UsageAdapterErrorCode::Unauthorized,
        403 => crate::usage::UsageAdapterErrorCode::Forbidden,
        429 => crate::usage::UsageAdapterErrorCode::RateLimited,
        500..=599 => crate::usage::UsageAdapterErrorCode::TransientHttp,
        _ => crate::usage::UsageAdapterErrorCode::HttpError,
    };
    UsageSourceDiagnostic {
        source: "retrieveUserQuotaSummary".to_owned(),
        code,
        message: format!(
            "Antigravity retrieveUserQuotaSummary returned HTTP {}",
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

pub(super) fn quota_summary_empty_diagnostic(status_code: u16) -> UsageSourceDiagnostic {
    UsageSourceDiagnostic {
        source: "retrieveUserQuotaSummary".to_owned(),
        code: crate::usage::UsageAdapterErrorCode::Unknown,
        message: "Antigravity retrieveUserQuotaSummary returned no usable quota buckets".to_owned(),
        http_status_code: Some(status_code),
        retry_after_seconds: None,
    }
}

pub(super) fn quota_summary_payload_diagnostic(status_code: u16) -> UsageSourceDiagnostic {
    UsageSourceDiagnostic {
        source: "retrieveUserQuotaSummary".to_owned(),
        code: crate::usage::UsageAdapterErrorCode::InvalidPayload,
        message: "Antigravity retrieveUserQuotaSummary returned an unreadable payload".to_owned(),
        http_status_code: Some(status_code),
        retry_after_seconds: None,
    }
}

pub(super) fn quota_summary_transport_diagnostic(error: &TransportError) -> UsageSourceDiagnostic {
    let message = match error {
        TransportError::Timeout(_) => "request timed out",
        TransportError::Request(error) if error.is_timeout() => "request timed out",
        TransportError::Request(_) => "request failed",
        TransportError::Serialization(_) => "request could not be serialized",
        TransportError::InvalidUrl(_) | TransportError::InvalidHeader { .. } => {
            "request could not be built"
        }
    };
    let code = match error {
        TransportError::Timeout(_) => crate::usage::UsageAdapterErrorCode::NetworkFailure,
        TransportError::Request(_) => crate::usage::UsageAdapterErrorCode::NetworkFailure,
        TransportError::Serialization(_) => crate::usage::UsageAdapterErrorCode::InvalidPayload,
        TransportError::InvalidUrl(_) | TransportError::InvalidHeader { .. } => {
            crate::usage::UsageAdapterErrorCode::Unknown
        }
    };
    UsageSourceDiagnostic {
        source: "retrieveUserQuotaSummary".to_owned(),
        code,
        message: format!("Antigravity retrieveUserQuotaSummary {message}"),
        http_status_code: None,
        retry_after_seconds: None,
    }
}

pub(super) fn parse_quota_summary_group(value: &Value) -> Option<LocalQuotaSummaryGroup> {
    let display_name =
        json_string(value, &["displayName", "display_name", "name"]).unwrap_or_default();
    let buckets = value
        .get("buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bucket| parse_quota_summary_bucket(bucket, &display_name))
        .collect::<Vec<_>>();
    (!buckets.is_empty()).then_some(LocalQuotaSummaryGroup { buckets })
}

pub(super) fn parse_quota_summary_bucket(
    value: &Value,
    group_name: &str,
) -> Option<LocalQuotaSummaryBucket> {
    let display_name =
        json_string(value, &["displayName", "display_name", "name"]).unwrap_or_default();
    let bucket_id = json_string(value, &["bucketId", "bucket_id", "id"])
        .or_else(|| (!display_name.is_empty()).then(|| display_name.clone()))?;
    let remaining_fraction = json_number(value, &["remainingFraction", "remaining_fraction"])
        .or_else(|| value.get("remaining").and_then(parse_remaining_fraction));
    Some(LocalQuotaSummaryBucket {
        bucket_id,
        display_name,
        window: json_string(value, &["window"]),
        remaining_fraction,
        reset_at_utc: parse_date_value(value.get("resetTime").or_else(|| value.get("reset_time"))),
        reset_description: json_string(value, &["description"]),
        disabled: crate::providers::shared::json_bool(value, &["disabled"]).unwrap_or(false),
        group_name: group_name.to_owned(),
    })
}

pub(super) fn parse_remaining_fraction(value: &Value) -> Option<f64> {
    if let Some(value) = json_number(value, &["remainingFraction", "remaining_fraction"]) {
        return Some(value);
    }
    let is_remaining_fraction = json_string(value, &["case"])
        .is_some_and(|case| case.eq_ignore_ascii_case("remainingFraction"));
    is_remaining_fraction.then(|| json_number(value, &["value"]))?
}

pub(super) fn parse_date_value(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
            return Some(parsed.with_timezone(&Utc));
        }
        if let Ok(seconds) = text.parse::<f64>() {
            return timestamp_to_utc(seconds);
        }
    }
    value.as_f64().and_then(timestamp_to_utc)
}

pub(super) fn timestamp_to_utc(value: f64) -> Option<DateTime<Utc>> {
    if !value.is_finite() {
        return None;
    }
    if value.abs() >= 100_000_000_000.0 {
        DateTime::from_timestamp_millis(value as i64)
    } else {
        DateTime::from_timestamp(value as i64, 0)
    }
}

#[derive(Clone, Copy)]
pub(super) enum LocalBucketKind {
    Session,
    Weekly,
    Other,
}

pub(super) fn local_bucket_kind(bucket: &LocalQuotaSummaryBucket) -> LocalBucketKind {
    let values = [
        bucket.bucket_id.as_str(),
        bucket.display_name.as_str(),
        bucket.window.as_deref().unwrap_or_default(),
    ];
    for value in values {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        let tokens = normalized
            .split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        let is_session = tokens.iter().any(|token| {
            matches!(
                *token,
                "session" | "5h" | "5hr" | "5hrs" | "5hour" | "5hours"
            )
        }) || tokens
            .windows(2)
            .any(|pair| pair[0] == "five" && pair[1] == "hour");
        if is_session {
            return LocalBucketKind::Session;
        }
        if tokens
            .iter()
            .any(|token| matches!(*token, "week" | "weekly"))
        {
            return LocalBucketKind::Weekly;
        }
    }
    LocalBucketKind::Other
}

pub(super) fn local_group_title(group_name: &str) -> String {
    let lower = group_name.trim().to_ascii_lowercase();
    if lower.contains("gemini") {
        "Gemini".to_owned()
    } else if lower.contains("claude") || lower.contains("gpt") {
        "Claude/GPT".to_owned()
    } else if group_name.trim().is_empty() {
        "Quota".to_owned()
    } else {
        group_name.trim().to_owned()
    }
}

pub(super) fn local_bucket_title(bucket: &LocalQuotaSummaryBucket) -> String {
    match local_bucket_kind(bucket) {
        LocalBucketKind::Session => "5-hour".to_owned(),
        LocalBucketKind::Weekly => "weekly".to_owned(),
        LocalBucketKind::Other => bucket.display_name.clone(),
    }
}

pub(super) fn local_window_minutes(bucket: &LocalQuotaSummaryBucket) -> Option<i64> {
    match local_bucket_kind(bucket) {
        LocalBucketKind::Session => Some(300),
        LocalBucketKind::Weekly => Some(10_080),
        LocalBucketKind::Other => None,
    }
}

pub(super) fn local_bucket_window(
    bucket: &LocalQuotaSummaryBucket,
    kind: UsageWindowKind,
    name: String,
) -> RateLimitWindow {
    let used_percent = bucket
        .remaining_fraction
        .map(|remaining| ((1.0 - remaining) * 100.0).clamp(0.0, 100.0))
        .unwrap_or(0.0);
    RateLimitWindow {
        kind,
        name,
        used_percent,
        reset_at_utc: bucket.reset_at_utc,
        limit_window_seconds: local_window_seconds(bucket),
    }
}

pub(super) fn local_window_seconds(bucket: &LocalQuotaSummaryBucket) -> i64 {
    match local_bucket_kind(bucket) {
        LocalBucketKind::Session => 5 * 60 * 60,
        LocalBucketKind::Weekly => 7 * 24 * 60 * 60,
        LocalBucketKind::Other => 0,
    }
}

pub(super) fn local_bucket_metric(bucket: &LocalQuotaSummaryBucket) -> UsageMetric {
    let group_title = local_group_title(&bucket.group_name);
    let bucket_title = local_bucket_title(bucket);
    let usage_known = !bucket.disabled && bucket.remaining_fraction.is_some();
    let used_percent = usage_known
        .then_some(bucket.remaining_fraction)
        .flatten()
        .map(|remaining| ((1.0 - remaining) * 100.0).clamp(0.0, 100.0));
    let mut metadata = HashMap::from([
        ("source".to_owned(), "local-quota-summary".to_owned()),
        ("group".to_owned(), group_title.clone()),
        ("raw_group".to_owned(), bucket.group_name.clone()),
        ("bucket_id".to_owned(), bucket.bucket_id.clone()),
        ("raw_bucket".to_owned(), bucket.display_name.clone()),
        ("usage_known".to_owned(), usage_known.to_string()),
    ]);
    if let Some(remaining) = bucket.remaining_fraction {
        metadata.insert("remaining_fraction".to_owned(), remaining.to_string());
    }
    if let Some(minutes) = local_window_minutes(bucket) {
        metadata.insert("window_minutes".to_owned(), minutes.to_string());
    }
    let window_seconds = local_window_seconds(bucket);
    if window_seconds > 0 {
        metadata.insert("window_seconds".to_owned(), window_seconds.to_string());
    }
    if let Some(description) = bucket.reset_description.as_deref() {
        metadata.insert("reset_description".to_owned(), description.to_owned());
    }
    UsageMetric {
        key: format!("antigravity-quota-summary-{}", bucket.bucket_id),
        name: format!("{group_title} {bucket_title}"),
        used_percent,
        used_amount: used_percent,
        limit_amount: usage_known.then_some(100.0),
        remaining_amount: usage_known
            .then_some(bucket.remaining_fraction)
            .flatten()
            .map(|value| value * 100.0),
        unit: usage_known.then(|| "percent".to_owned()),
        reset_at_utc: bucket.reset_at_utc,
        reset_label: bucket.reset_description.clone(),
        metadata,
    }
}

pub(super) fn snapshot_from_quota_summary(
    account: &AccountRecord,
    groups: &[LocalQuotaSummaryGroup],
    models: &[Quota],
    observed_email: Option<String>,
    plan_type: Option<String>,
    source: &str,
) -> UsageSnapshot {
    let buckets = groups
        .iter()
        .flat_map(|group| group.buckets.iter())
        .collect::<Vec<_>>();
    let all_windows = buckets
        .iter()
        .filter(|bucket| !bucket.disabled && bucket.remaining_fraction.is_some())
        .map(|bucket| {
            let group_title = local_group_title(&bucket.group_name);
            let bucket_title = local_bucket_title(bucket);
            AdditionalRateLimitWindow {
                key: format!("antigravity-quota-summary-{}", bucket.bucket_id),
                name: format!("{group_title} {bucket_title}"),
                window: local_bucket_window(
                    bucket,
                    UsageWindowKind::Additional,
                    format!("{group_title} {bucket_title}"),
                ),
            }
        })
        .collect::<Vec<_>>();

    let representative = |family: &str, kind: UsageWindowKind| {
        buckets
            .iter()
            .filter(|bucket| {
                !bucket.disabled
                    && bucket.remaining_fraction.is_some()
                    && local_group_title(&bucket.group_name)
                        .to_ascii_lowercase()
                        .contains(family)
            })
            .max_by(|left, right| {
                let left_used = left
                    .remaining_fraction
                    .map(|value| 1.0 - value)
                    .unwrap_or_default();
                let right_used = right
                    .remaining_fraction
                    .map(|value| 1.0 - value)
                    .unwrap_or_default();
                left_used.total_cmp(&right_used)
            })
            .map(|bucket| {
                let group_title = local_group_title(&bucket.group_name);
                let bucket_title = local_bucket_title(bucket);
                local_bucket_window(bucket, kind, format!("{group_title} {bucket_title}"))
            })
    };
    let primary = representative("gemini", UsageWindowKind::Primary).or_else(|| {
        buckets
            .iter()
            .find(|bucket| !bucket.disabled && bucket.remaining_fraction.is_some())
            .map(|bucket| {
                let group_title = local_group_title(&bucket.group_name);
                let bucket_title = local_bucket_title(bucket);
                local_bucket_window(
                    bucket,
                    UsageWindowKind::Primary,
                    format!("{group_title} {bucket_title}"),
                )
            })
    });
    let secondary = representative("claude/gpt", UsageWindowKind::Secondary);

    let effective_models = models
        .iter()
        .map(|quota| apply_summary_quota(quota, groups))
        .collect::<Vec<_>>();
    let mut metrics = buckets
        .iter()
        .map(|bucket| local_bucket_metric(bucket))
        .collect::<Vec<_>>();
    metrics.extend(effective_models.iter().map(quota_metric));

    UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id: account.workspace_id.clone(),
        plan_type,
        primary,
        primary_window_kind: None,
        primary_window_is_synthetic: false,
        secondary,
        additional_windows: all_windows,
        credits: None,
        credit_inventory: None,
        spend: None,
        observed_email,
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics,
        source_diagnostics: Vec::new(),
        provider_id: ANTIGRAVITY.to_owned(),
        source: Some(source.to_owned()),
        data_confidence: "authoritative".to_owned(),
    }
}

pub(super) fn apply_summary_quota(quota: &Quota, groups: &[LocalQuotaSummaryGroup]) -> Quota {
    let Some(bucket) = select_summary_bucket(quota, groups) else {
        return quota.clone();
    };
    let Some(remaining_fraction) = bucket.remaining_fraction else {
        return quota.clone();
    };
    let used_percent = ((1.0 - remaining_fraction) * 100.0).clamp(0.0, 100.0);
    let mut next = quota.clone();
    next.remaining_fraction = Some(remaining_fraction);
    next.used_percent = used_percent;
    next.window.used_percent = used_percent;
    next.window.reset_at_utc = bucket.reset_at_utc;
    next.window.limit_window_seconds = local_window_seconds(bucket);
    next
}

pub(super) fn select_summary_bucket<'a>(
    quota: &Quota,
    groups: &'a [LocalQuotaSummaryGroup],
) -> Option<&'a LocalQuotaSummaryBucket> {
    let model_lower = format!("{} {}", quota.key, quota.name).to_ascii_lowercase();
    let is_third_party = model_lower.contains("claude") || model_lower.contains("gpt");
    let mut session = None;
    let mut weekly = None;

    for group in groups {
        let group_lower = group
            .buckets
            .first()
            .map(|bucket| bucket.group_name.to_ascii_lowercase())
            .unwrap_or_default();
        let group_is_third_party = group_lower.contains("claude")
            || group_lower.contains("gpt")
            || group_lower.contains("3p");
        if group_is_third_party != is_third_party {
            continue;
        }
        for bucket in &group.buckets {
            if bucket.disabled || bucket.remaining_fraction.is_none() {
                continue;
            }
            match local_bucket_kind(bucket) {
                LocalBucketKind::Session => {
                    session = more_constrained_bucket(session, Some(bucket));
                }
                LocalBucketKind::Weekly => {
                    weekly = more_constrained_bucket(weekly, Some(bucket));
                }
                LocalBucketKind::Other => {}
            }
        }
    }

    match (session, weekly) {
        (Some(_session), Some(weekly)) if weekly.remaining_fraction <= Some(0.001) => Some(weekly),
        (Some(session), Some(weekly)) => {
            if session.remaining_fraction <= weekly.remaining_fraction {
                Some(session)
            } else {
                Some(weekly)
            }
        }
        (Some(session), None) => Some(session),
        (None, Some(weekly)) => Some(weekly),
        (None, None) => None,
    }
}

pub(super) fn more_constrained_bucket<'a>(
    current: Option<&'a LocalQuotaSummaryBucket>,
    candidate: Option<&'a LocalQuotaSummaryBucket>,
) -> Option<&'a LocalQuotaSummaryBucket> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => {
            if candidate.remaining_fraction < current.remaining_fraction {
                Some(candidate)
            } else {
                Some(current)
            }
        }
        (None, candidate) => candidate,
        (current, None) => current,
    }
}
