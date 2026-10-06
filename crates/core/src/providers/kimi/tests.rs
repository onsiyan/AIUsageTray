//! Payload cases follow CodexBar's Kimi tests and documented responses.

use super::*;
use crate::{
    auth::AccountAuthMaterial, transport::UsageHttpResponse, usage::UsageAdapterErrorCode,
};
use std::sync::Mutex;

fn account() -> AccountRecord {
    AccountRecord::create("Kimi Code", "kimi@local.invalid", None, KIMI, None).unwrap()
}

fn snapshot_of(body: &str) -> Result<UsageSnapshot, String> {
    parse_usage(body)?.snapshot(&account(), Utc::now())
}

#[test]
fn the_documented_count_response_shows_five_hour_and_weekly_quotas() {
    let snapshot = snapshot_of(
        r#"{"usage":{"limit":"2048","used":"214","remaining":"1834","resetTime":"2026-01-09T15:23:13.716839300Z"},
            "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
              "detail":{"limit":"200","used":"139","remaining":"61","resetTime":"2026-01-06T13:33:02.717479433Z"}}]}"#,
    )
    .unwrap();
    let five_hour = snapshot.primary.unwrap();
    assert_eq!(five_hour.name, FIVE_HOUR_WINDOW_NAME);
    assert_eq!(five_hour.used_percent, 69.5);
    assert_eq!(five_hour.limit_window_seconds, FIVE_HOURS);
    assert_eq!(
        five_hour.reset_at_utc.unwrap().timestamp(),
        DateTime::parse_from_rfc3339("2026-01-06T13:33:02Z")
            .unwrap()
            .timestamp()
    );
    let weekly = snapshot.secondary.unwrap();
    assert_eq!(weekly.name, WEEKLY_WINDOW_NAME);
    assert!((weekly.used_percent - 214.0 / 2048.0 * 100.0).abs() < 1e-9);
    assert_eq!(weekly.limit_window_seconds, WEEK);
    assert_eq!(
        snapshot.primary_window_kind,
        Some(UsagePrimaryWindowKind::Session)
    );
    assert!(snapshot.additional_windows.is_empty());
}

#[test]
fn ratio_pools_take_precedence_and_add_the_monthly_total() {
    let snapshot = snapshot_of(
        r#"{"usage":{"limit":"2048","used":"1000"},
            "usages":{"limit_5h":{"used_ratio":0.25,"reset_time":"2026-01-06T13:33:02Z"},
                      "limit_7d":{"used_ratio":0.5,"reset_time":"2026-01-09T15:23:13Z"},
                      "limit_month_total":{"used_ratio":1.4,"reset_time":"2026-02-01T00:00:00Z"}},
            "user":{"membership":{"level":"LEVEL_INTERMEDIATE"}},"version":"GOODS_VERSION_V1"}"#,
    )
    .unwrap();
    assert_eq!(snapshot.primary.unwrap().used_percent, 25.0);
    assert_eq!(snapshot.secondary.unwrap().used_percent, 50.0);
    let monthly = &snapshot.additional_windows[0];
    assert_eq!(monthly.name, MONTHLY_WINDOW_NAME);
    // Overage is capped at a full bar.
    assert_eq!(monthly.window.used_percent, 100.0);
    assert_eq!(snapshot.plan_type.as_deref(), Some("Allegretto"));
}

#[test]
fn a_zero_ratio_placeholder_falls_back_to_matching_counts() {
    // Older mixed responses: a 0 ratio next to real counts with the same
    // reset (within two seconds) is a placeholder.
    let snapshot = snapshot_of(
        r#"{"usage":{"limit":"2048","used":"214","resetTime":"2026-01-09T15:23:13Z"},
            "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
              "detail":{"limit":"200","used":"100","resetTime":"2026-01-06T13:33:02.000Z"}}],
            "usages":{"limit_5h":{"used_ratio":0,"reset_time":"2026-01-06T13:33:03.450Z"}}}"#,
    )
    .unwrap();
    assert_eq!(snapshot.primary.unwrap().used_percent, 50.0);

    // A real zero (no matching counts in use) stays zero.
    let real_zero = snapshot_of(
        r#"{"usages":{"limit_5h":{"used_ratio":0,"reset_time":"2026-01-06T13:33:03Z"},
                      "limit_7d":{"used_ratio":0.1}}}"#,
    )
    .unwrap();
    assert_eq!(real_zero.primary.unwrap().used_percent, 0.0);
}

#[test]
fn counts_are_read_from_remaining_and_odd_counts_are_not_timed() {
    let from_remaining = snapshot_of(r#"{"usage":{"limit":"100","remaining":"40"}}"#).unwrap();
    let weekly = from_remaining.primary.unwrap();
    // With only a weekly quota, it leads.
    assert_eq!(weekly.name, WEEKLY_WINDOW_NAME);
    assert_eq!(weekly.used_percent, 60.0);
    assert_eq!(
        from_remaining.primary_window_kind,
        Some(UsagePrimaryWindowKind::Weekly)
    );

    let unreliable = snapshot_of(r#"{"usage":{"limit":100,"remaining":"500"}}"#).unwrap();
    let weekly = unreliable.primary.unwrap();
    assert_eq!(weekly.used_percent, 0.0);
    assert_eq!(weekly.limit_window_seconds, 0);

    assert!(snapshot_of(r#"{"usage":{"limit":"0"}}"#).is_err());
    assert!(snapshot_of(r#"{}"#).is_err());
}

#[test]
fn unknown_membership_levels_and_catalogs_are_kept_as_sent() {
    let level = |body: &str| plan_name(&serde_json::from_str(body).unwrap());
    assert_eq!(
        level(r#"{"user":{"membership":{"level":"LEVEL_FREE"}}}"#).as_deref(),
        Some("Adagio")
    );
    assert_eq!(
        level(r#"{"user":{"membership":{"level":"LEVEL_FREE"}},"version":"GOODS_VERSION_V2"}"#)
            .as_deref(),
        Some("LEVEL_FREE")
    );
    assert_eq!(
        level(r#"{"user":{"membership":{"level":"LEVEL_UNSPECIFIED"}}}"#),
        None
    );
}

struct KeyAuth;

#[async_trait]
impl AccountAuthMaterialProvider for KeyAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("sk-kimi-test".to_owned()),
            ..AccountAuthMaterial::default()
        }))
    }
}

struct CannedTransport {
    status_code: u16,
    body: &'static str,
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[async_trait]
impl UsageHttpTransport for CannedTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request);
        Ok(UsageHttpResponse {
            status_code: self.status_code,
            body: self.body.to_owned(),
            headers: BTreeMap::new(),
        })
    }
}

fn probe(status_code: u16, body: &'static str) -> (UsageProbeResult, Vec<UsageHttpRequest>) {
    let transport = Arc::new(CannedTransport {
        status_code,
        body,
        requests: Mutex::new(Vec::new()),
    });
    let adapter = KimiUsageAdapter::new(transport.clone(), Arc::new(KeyAuth)).unwrap();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(adapter.probe(&account()))
        .unwrap();
    let requests = std::mem::take(&mut *transport.requests.lock().unwrap());
    (result, requests)
}

#[test]
fn the_probe_reads_the_code_usage_endpoint_with_the_api_key() {
    let (result, requests) = probe(
        200,
        r#"{"usages":{"limit_5h":{"used_ratio":0.1},"limit_7d":{"used_ratio":0.2}}}"#,
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.as_str(), USAGE_URL);
    assert_eq!(
        requests[0].headers.get("Authorization").map(String::as_str),
        Some("Bearer sk-kimi-test")
    );
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.provider_id, KIMI);
    assert_eq!(snapshot.primary.unwrap().used_percent, 10.0);

    let (rejected, _) = probe(401, r#"{"error":"invalid key"}"#);
    assert_eq!(
        rejected.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );
}
