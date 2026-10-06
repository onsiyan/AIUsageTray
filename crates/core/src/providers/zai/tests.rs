//! Payload cases follow CodexBar's z.ai plugin fixtures.

use super::*;
use crate::{auth::AccountAuthMaterial, usage::UsageAdapterErrorCode};
use std::sync::Mutex;

fn account() -> AccountRecord {
    AccountRecord::create("z.ai account", "zai@local.invalid", None, ZAI, None).unwrap()
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

fn snapshot_at(body: &str, now: DateTime<Utc>) -> Result<UsageSnapshot, String> {
    Ok(parse_quota(body, now)?.snapshot(&account(), now))
}

fn millis(time: DateTime<Utc>) -> i64 {
    time.timestamp_millis()
}

#[test]
fn coding_plan_limits_become_five_hour_weekly_and_mcp_lanes() {
    let now = at("2026-03-10T12:00:00Z");
    let body = format!(
        r#"{{"code":200,"success":true,"msg":"ok","data":{{"planName":"GLM Coding Pro","limits":[
            {{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":40,"nextResetTime":{weekly}}},
            {{"type":"TIME_LIMIT","unit":5,"number":1,"percentage":10,"usage":1000,"currentValue":250,"remaining":750,"nextResetTime":{monthly}}},
            {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":20,"nextResetTime":{five_hour}}}]}}}}"#,
        weekly = millis(at("2026-03-14T00:00:00Z")),
        monthly = millis(at("2026-04-01T00:00:00Z")),
        five_hour = millis(at("2026-03-10T15:00:00Z")),
    );
    let snapshot = snapshot_at(&body, now).unwrap();

    let five_hour = snapshot.primary.unwrap();
    assert_eq!(five_hour.name, "5 hours");
    assert_eq!(five_hour.used_percent, 20.0);
    assert_eq!(five_hour.limit_window_seconds, 5 * 60 * 60);
    assert_eq!(five_hour.reset_at_utc, Some(at("2026-03-10T15:00:00Z")));
    assert_eq!(
        snapshot.primary_window_kind,
        Some(UsagePrimaryWindowKind::Session)
    );

    let weekly = snapshot.secondary.unwrap();
    assert_eq!(weekly.name, "Weekly");
    assert_eq!(weekly.used_percent, 40.0);

    // MCP counts refine the percentage: 250 of 1000 used.
    let mcp = &snapshot.additional_windows[0];
    assert_eq!(mcp.name, MCP_WINDOW_NAME);
    assert_eq!(mcp.window.used_percent, 25.0);
    // The "1 minute" MCP marker is a monthly quota.
    assert_eq!(mcp.window.limit_window_seconds, 30 * 24 * 60 * 60);
    assert_eq!(snapshot.plan_type.as_deref(), Some("GLM Coding Pro"));
    // Token plans have no peak pricing note.
    assert!(snapshot.metrics.is_empty());
}

#[test]
fn an_implausible_five_hour_reset_is_dropped_but_the_usage_kept() {
    let now = at("2026-03-10T12:00:00Z");
    let body = format!(
        r#"{{"code":200,"success":true,"data":{{"limits":[
            {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":55,"nextResetTime":{}}}]}}}}"#,
        millis(at("2026-03-10T22:00:00Z")),
    );
    let window = snapshot_at(&body, now).unwrap().primary.unwrap();
    assert_eq!(window.used_percent, 55.0);
    assert_eq!(window.reset_at_utc, None);
}

#[test]
fn an_mcp_only_plan_leads_with_mcp_and_unknown_types_are_skipped() {
    let snapshot = snapshot_at(
        r#"{"code":200,"success":true,"data":{"limits":[
            {"type":"SOMETHING_NEW","unit":3,"number":1,"percentage":99},
            {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":12}]}}"#,
        Utc::now(),
    )
    .unwrap();
    let mcp = snapshot.primary.unwrap();
    assert_eq!(mcp.name, MCP_WINDOW_NAME);
    assert_eq!(mcp.used_percent, 12.0);
    assert!(snapshot.secondary.is_none());
    assert!(snapshot.additional_windows.is_empty());
}

#[test]
fn malformed_or_failed_responses_are_errors_not_zero_usage() {
    let now = Utc::now();
    assert_eq!(
        snapshot_at(r#"{"code":401,"success":false,"msg":"token expired"}"#, now).unwrap_err(),
        "token expired"
    );
    assert!(snapshot_at(r#"{"code":200,"success":true,"data":{}}"#, now).is_err());
    // A fractional percentage is not what z.ai sends.
    assert!(
        snapshot_at(
            r#"{"code":200,"success":true,"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12.5}]}}"#,
            now
        )
        .is_err()
    );
    // No limits at all: nothing to show, but not a failure.
    let empty = snapshot_at(r#"{"code":200,"success":true,"data":{"limits":[]}}"#, now).unwrap();
    assert!(empty.primary.is_none());
}

#[test]
fn credit_plans_say_whether_peak_pricing_applies() {
    let body = r#"{"code":200,"success":true,"data":{"limits":[
        {"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":30}]}}"#;
    // Tuesday 07:00 UTC is peak, until 10:00.
    let peak = snapshot_at(body, at("2026-03-10T07:00:00Z")).unwrap();
    assert_eq!(peak.metrics[0].key, "rate.peak");
    assert_eq!(
        peak.metrics[0].reset_at_utc,
        Some(at("2026-03-10T10:00:00Z"))
    );
    // Friday 12:00 UTC is off-peak until Monday 06:00.
    let off_peak = snapshot_at(body, at("2026-03-13T12:00:00Z")).unwrap();
    assert_eq!(off_peak.metrics[0].key, "rate.off_peak");
    assert_eq!(
        off_peak.metrics[0].reset_at_utc,
        Some(at("2026-03-16T06:00:00Z"))
    );
    // Early Tuesday is off-peak until 06:00 the same day.
    let early = snapshot_at(body, at("2026-03-10T03:30:00Z")).unwrap();
    assert_eq!(
        early.metrics[0].reset_at_utc,
        Some(at("2026-03-10T06:00:00Z"))
    );
}

struct KeyAuth(Option<&'static str>);

#[async_trait]
impl AccountAuthMaterialProvider for KeyAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("zai-key".to_owned()),
            secondary_bearer_token: self.0.map(str::to_owned),
            ..AccountAuthMaterial::default()
        }))
    }
}

/// Answers by host: `accepting` hosts succeed, others reject the key.
struct HostTransport {
    accepting: &'static str,
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[async_trait]
impl UsageHttpTransport for HostTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let accepted = request.url.host_str() == Some(self.accepting);
        self.requests.lock().unwrap().push(request);
        Ok(UsageHttpResponse {
            status_code: if accepted { 200 } else { 401 },
            body: if accepted {
                r#"{"code":200,"success":true,"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":5}]}}"#
            } else {
                r#"{"code":401,"success":false,"msg":"invalid api key"}"#
            }
            .to_owned(),
            headers: BTreeMap::new(),
        })
    }
}

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn the_region_is_found_when_the_account_is_added_and_used_after() {
    let transport = HostTransport {
        accepting: "open.bigmodel.cn",
        requests: Mutex::new(Vec::new()),
    };
    assert_eq!(
        run(detect_region(&transport, "zai-key")),
        Ok(ZaiRegion::BigModelChina)
    );
    let hosts = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.url.host_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(hosts, ["api.z.ai", "open.bigmodel.cn"]);
    assert_eq!(ZaiRegion::BigModelChina.stored(), Some("bigmodel-cn"));

    let transport = Arc::new(HostTransport {
        accepting: "open.bigmodel.cn",
        requests: Mutex::new(Vec::new()),
    });
    let adapter =
        ZaiUsageAdapter::new(transport.clone(), Arc::new(KeyAuth(Some("bigmodel-cn")))).unwrap();
    let result = run(adapter.probe(&account())).unwrap();
    assert_eq!(result.snapshot.unwrap().primary.unwrap().used_percent, 5.0);
    let request = transport.requests.lock().unwrap()[0].clone();
    assert_eq!(
        request.url.as_str(),
        "https://open.bigmodel.cn/api/monitor/usage/quota/limit"
    );
    assert_eq!(
        request.headers.get("Authorization").map(String::as_str),
        Some("Bearer zai-key")
    );

    // A global account is read on api.z.ai, and a rejected key says so.
    let adapter = ZaiUsageAdapter::new(transport.clone(), Arc::new(KeyAuth(None))).unwrap();
    let rejected = run(adapter.probe(&account())).unwrap();
    assert_eq!(
        rejected.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );
}
