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
                {"date":"2030-01-02","endpoint_id":"endpoint-1","model":"openai/gpt-5","prompt_tokens":50,"completion_tokens":125,"reasoning_tokens":25,"requests":5,"usage":0.015,"byok_usage_inference":0.005},
                {"date":"2029-12-01","endpoint_id":"old","model":"old-model","prompt_tokens":10,"completion_tokens":20,"requests":1,"usage":999}
            ]}"#,
            r#"{"data":[
                {"date":"2030-01-02","endpoint_id":"endpoint-1","model":"openai/gpt-5","prompt_tokens":50,"completion_tokens":125,"reasoning_tokens":25,"requests":5,"usage":0.015,"byok_usage_inference":0.005},
                {"date":"2030-01-03","endpoint_id":"endpoint-2","model":"anthropic/claude","prompt_tokens":10,"completion_tokens":20,"reasoning_tokens":30,"requests":2,"usage":0.025}
            ]}"#,
            NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2030, 1, 3).unwrap(),
        )
        .unwrap();

    assert_eq!(report.metrics.len(), 2);
    assert!((report.summary.used_amount.unwrap() - 0.045).abs() < 1e-12);
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
            .get("byok_usage_inference")
            .map(String::as_str),
        Some("0.005")
    );
    assert_eq!(
        report
            .summary
            .metadata
            .get("metered_usage")
            .map(String::as_str),
        Some("0.04")
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
        r#"{{"data":[{{"date":"2030-01-02","model":"openai/gpt-5","usage":0.01,"requests":1,"prompt_tokens":{},"completion_tokens":10}}]}}"#,
        MAX_SAFE_INTEGER + 1
    );
    let error = parse_activity_report(
        &body,
        r#"{"data":[]}"#,
        NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
        NaiveDate::from_ymd_opt(2030, 1, 3).unwrap(),
    )
    .unwrap_err();
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
