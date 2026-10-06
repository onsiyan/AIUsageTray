//! Payload cases follow CodexBar's Copilot tests.

use super::*;
use crate::{auth::AccountAuthMaterial, transport::UsageHttpResponse};
use std::sync::Mutex;

fn account() -> AccountRecord {
    AccountRecord::create("octocat", "copilot@local.invalid", None, COPILOT, None).unwrap()
}

fn snapshot_of(body: &str) -> Result<UsageSnapshot, String> {
    parse_usage(body)?.snapshot(&account(), Utc::now())
}

#[test]
fn premium_and_chat_quotas_become_two_bars_with_the_reset_date() {
    let snapshot = snapshot_of(
        r#"{"copilot_plan":"individual","quota_reset_date":"2025-02-01",
            "quota_snapshots":{
              "premium_interactions":{"entitlement":300,"remaining":240,"percent_remaining":80,"quota_id":"premium_interactions"},
              "chat":{"entitlement":"300","remaining":"150","percent_remaining":"50","quota_id":"chat"}}}"#,
    )
    .unwrap();
    let premium = snapshot.primary.unwrap();
    assert_eq!(premium.name, PREMIUM_WINDOW_NAME);
    assert_eq!(premium.used_percent, 20.0);
    assert_eq!(
        premium.reset_at_utc.unwrap().to_rfc3339(),
        "2025-02-01T00:00:00+00:00"
    );
    let chat = snapshot.secondary.unwrap();
    assert_eq!(chat.name, CHAT_WINDOW_NAME);
    assert_eq!(chat.used_percent, 50.0);
    assert_eq!(snapshot.plan_type.as_deref(), Some("individual"));
    assert!(snapshot.metrics.is_empty());
}

#[test]
fn a_chat_only_plan_shows_the_chat_bar() {
    let snapshot = snapshot_of(
        r#"{"copilot_plan":"free","quota_snapshots":{"chat":{"entitlement":200,"remaining":75,"percent_remaining":37.5}}}"#,
    )
    .unwrap();
    let chat = snapshot.primary.unwrap();
    assert_eq!(chat.name, CHAT_WINDOW_NAME);
    assert_eq!(chat.used_percent, 62.5);
    assert!(snapshot.secondary.is_none());
}

#[test]
fn the_percentage_is_derived_when_missing_and_unknown_when_underdetermined() {
    let derived =
        parse_usage(r#"{"quota_snapshots":{"chat":{"entitlement":200,"remaining":50}}}"#).unwrap();
    assert_eq!(derived.chat.unwrap().percent_remaining, 25.0);

    let unknown = QuotaSnapshot::parse(&serde_json::json!({"entitlement": 200})).unwrap();
    assert!(!unknown.has_percent_remaining);
    assert!(!unknown.is_usable());
}

#[test]
fn older_plans_report_monthly_and_remaining_counts() {
    let usage = parse_usage(
        r#"{"copilot_plan":"free",
            "monthly_quotas":{"chat":400,"completions":2000},
            "limited_user_quotas":{"chat":100,"completions":1500}}"#,
    )
    .unwrap();
    assert_eq!(usage.chat.as_ref().unwrap().percent_remaining, 25.0);
    assert_eq!(usage.premium.as_ref().unwrap().percent_remaining, 75.0);
}

#[test]
fn counts_without_a_remaining_value_or_with_a_zero_month_are_not_guessed() {
    let usage = parse_usage(
        r#"{"monthly_quotas":{"chat":400,"completions":0},"limited_user_quotas":{"completions":0}}"#,
    )
    .unwrap();
    assert!(usage.chat.is_none());
    assert!(usage.premium.is_none());
}

#[test]
fn a_direct_lane_without_a_percentage_falls_back_to_the_counts() {
    let usage = parse_usage(
        r#"{"quota_snapshots":{"chat":{"remaining":30,"quota_id":"chat"}},
            "monthly_quotas":{"chat":400},"limited_user_quotas":{"chat":100}}"#,
    )
    .unwrap();
    let chat = usage.chat.unwrap();
    assert_eq!(chat.entitlement, 400.0);
    assert_eq!(chat.percent_remaining, 25.0);
}

#[test]
fn unfamiliar_quota_names_still_yield_lanes() {
    let usage = parse_usage(
        r#"{"quota_snapshots":{
              "code_assist":{"entitlement":100,"remaining":40,"percent_remaining":40},
              "chat_messages":{"entitlement":50,"remaining":50,"percent_remaining":100}}}"#,
    )
    .unwrap();
    assert_eq!(usage.premium.unwrap().percent_remaining, 40.0);
    assert_eq!(usage.chat.unwrap().percent_remaining, 100.0);
}

#[test]
fn business_usage_billing_placeholders_never_show_as_zero_percent_used() {
    let snapshot = snapshot_of(
        r#"{"copilot_plan":"business","token_based_billing":true,"quota_snapshots":{
              "premium_interactions":{"entitlement":0,"remaining":0,"percent_remaining":100},
              "chat":{"entitlement":0,"remaining":0,"percent_remaining":100},
              "completions":{"entitlement":0,"remaining":0,"percent_remaining":100}}}"#,
    )
    .unwrap();
    assert!(snapshot.primary.is_none());
    assert!(snapshot.secondary.is_none());
    assert_eq!(snapshot.metrics.len(), 1);
    assert_eq!(snapshot.metrics[0].key, "quota.usage_billed");
}

#[test]
fn usage_billed_seats_show_their_credits_used() {
    let snapshot = snapshot_of(
        r#"{"copilot_plan":"business","token_based_billing":true,"quota_snapshots":{
              "premium_interactions":{"entitlement":0,"remaining":0,"percent_remaining":100,"credits_used":31}}}"#,
    )
    .unwrap();
    assert!(snapshot.primary.is_none());
    let credits = snapshot
        .metrics
        .iter()
        .find(|metric| metric.key == "credits.used")
        .unwrap();
    assert_eq!(credits.used_amount, Some(31.0));
}

#[test]
fn a_metered_seat_reporting_zero_credits_shows_no_credit_row() {
    let snapshot = snapshot_of(
        r#"{"copilot_plan":"individual","quota_snapshots":{
              "premium_interactions":{"entitlement":300,"remaining":300,"percent_remaining":100,"credits_used":0}}}"#,
    )
    .unwrap();
    assert!(snapshot.primary.is_some());
    assert!(snapshot.metrics.is_empty());
}

#[test]
fn an_unlimited_lane_has_no_bar_but_a_finite_lane_keeps_its_bar() {
    let snapshot = snapshot_of(
        r#"{"copilot_plan":"individual","quota_snapshots":{
              "premium_interactions":{"entitlement":300,"remaining":150,"percent_remaining":50},
              "chat":{"unlimited":true}}}"#,
    )
    .unwrap();
    assert_eq!(snapshot.primary.unwrap().name, PREMIUM_WINDOW_NAME);
    assert!(snapshot.secondary.is_none());

    let unlimited_only =
        snapshot_of(r#"{"copilot_plan":"pro","quota_snapshots":{"chat":{"unlimited":true}}}"#)
            .unwrap();
    assert!(unlimited_only.primary.is_none());
    assert_eq!(unlimited_only.metrics[0].key, "quota.unlimited");
}

#[test]
fn an_unlimited_direct_lane_uses_the_finite_monthly_count_and_keeps_its_credits() {
    let usage = parse_usage(
        r#"{"quota_snapshots":{"chat":{"unlimited":true,"credits_used":12}},
            "monthly_quotas":{"chat":400},"limited_user_quotas":{"chat":300}}"#,
    )
    .unwrap();
    let chat = usage.chat.unwrap();
    assert!(!chat.unlimited);
    assert_eq!(chat.percent_remaining, 75.0);
    assert_eq!(chat.credits_used, Some(12.0));
}

#[test]
fn going_over_quota_is_spelled_out() {
    let snapshot = snapshot_of(
        r#"{"quota_snapshots":{"premium_interactions":{"entitlement":300,"remaining":-60,"percent_remaining":-20}}}"#,
    )
    .unwrap();
    assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 120.0);
    assert_eq!(snapshot.metrics[0].name, "Premium requests: 120% used");
    assert_eq!(snapshot.metrics[0].key, "premium.over_quota");
}

#[test]
fn a_payload_without_any_quota_is_rejected() {
    assert!(snapshot_of(r#"{"copilot_plan":"free"}"#).is_err());
    assert!(parse_usage("[]").is_err());
    assert!(parse_usage("not json").is_err());
}

#[test]
fn reset_dates_may_be_plain_dates_or_timestamps() {
    assert_eq!(
        parse_reset_date("2025-02-01").unwrap().to_rfc3339(),
        "2025-02-01T00:00:00+00:00"
    );
    assert_eq!(
        parse_reset_date("2025-02-01T10:30:00Z")
            .unwrap()
            .to_rfc3339(),
        "2025-02-01T10:30:00+00:00"
    );
    assert!(parse_reset_date("soon").is_none());
}

#[test]
fn the_device_flow_reads_codes_and_every_poll_outcome() {
    let code = parse_device_code(
        r#"{"device_code":"dc","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":899,"interval":5}"#,
    )
    .unwrap();
    assert_eq!(code.user_code, "ABCD-1234");
    assert_eq!(code.interval, 5);
    assert!(parse_device_code(r#"{"user_code":"X"}"#).is_err());

    assert_eq!(
        token_poll_outcome(r#"{"error":"authorization_pending"}"#),
        TokenPoll::Pending
    );
    assert_eq!(
        token_poll_outcome(r#"{"error":"slow_down","interval":10}"#),
        TokenPoll::SlowDown(Some(10))
    );
    assert_eq!(
        token_poll_outcome(r#"{"error":"expired_token"}"#),
        TokenPoll::Failed(CopilotLoginError::Expired)
    );
    assert_eq!(
        token_poll_outcome(r#"{"error":"access_denied"}"#),
        TokenPoll::Failed(CopilotLoginError::Denied)
    );
    assert_eq!(
        token_poll_outcome(r#"{"access_token":"gho_x","token_type":"bearer","scope":"read:user"}"#),
        TokenPoll::Token("gho_x".to_owned())
    );
}

struct TokenAuth;

#[async_trait]
impl AccountAuthMaterialProvider for TokenAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("gho_test".to_owned()),
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
    let adapter = CopilotUsageAdapter::new(transport.clone(), Arc::new(TokenAuth)).unwrap();
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
fn the_probe_calls_the_usage_endpoint_as_the_copilot_extension_does() {
    let (result, requests) = probe(
        200,
        r#"{"copilot_plan":"individual","quota_snapshots":{"premium_interactions":{"entitlement":300,"remaining":270,"percent_remaining":90}}}"#,
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.as_str(), USAGE_URL);
    let header = |name: &str| requests[0].headers.get(name).map(String::as_str);
    assert_eq!(header("Authorization"), Some("token gho_test"));
    assert_eq!(header("Editor-Version"), Some("vscode/1.96.2"));
    assert_eq!(header("X-Github-Api-Version"), Some("2025-04-01"));
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.provider_id, COPILOT);
    assert_eq!(snapshot.primary.unwrap().used_percent, 10.0);
}

#[test]
fn a_revoked_token_and_a_missing_seat_are_told_apart() {
    let (revoked, _) = probe(401, r#"{"message":"Bad credentials"}"#);
    assert_eq!(
        revoked.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );
    let (no_seat, _) = probe(404, r#"{"message":"Not Found"}"#);
    assert_eq!(
        no_seat.error.unwrap().code,
        UsageAdapterErrorCode::NoSubscription
    );
}
