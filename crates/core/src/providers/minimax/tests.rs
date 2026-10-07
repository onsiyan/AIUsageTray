//! Payload cases follow CodexBar's MiniMax tests.

use super::*;
use crate::auth::AccountAuthMaterial;
use std::sync::Mutex;

const TOKEN_PLAN: &str = r#"{
  "model_remains": [
    {
      "start_time": 1782043200000,
      "end_time": 1782057600000,
      "remains_time": 7003536,
      "current_interval_total_count": 0,
      "current_interval_usage_count": 0,
      "model_name": "general",
      "current_weekly_total_count": 0,
      "current_weekly_usage_count": 0,
      "weekly_start_time": 1781452800000,
      "weekly_end_time": 1782057600000,
      "weekly_remains_time": 7003536,
      "current_interval_status": 1,
      "current_interval_remaining_percent": 100,
      "current_weekly_status": 1,
      "current_weekly_remaining_percent": 70,
      "weekly_boost_permille": 1500
    },
    {
      "start_time": 1781971200000,
      "end_time": 1782057600000,
      "remains_time": 7003536,
      "current_interval_total_count": 0,
      "current_interval_usage_count": 0,
      "model_name": "video",
      "current_weekly_total_count": 0,
      "current_weekly_usage_count": 0,
      "weekly_start_time": 1781452800000,
      "weekly_end_time": 1782057600000,
      "weekly_remains_time": 7003536,
      "current_interval_status": 3,
      "current_interval_remaining_percent": 100,
      "current_weekly_status": 3,
      "current_weekly_remaining_percent": 100
    }
  ],
  "base_resp": { "status_code": 0, "status_msg": "success" }
}"#;

const LEGACY_CODING_PLAN: &str = r#"{
  "base_resp": { "status_code": 0 },
  "current_subscribe_title": "Max",
  "model_remains": [
    {
      "current_interval_total_count": 1000,
      "current_interval_usage_count": 250,
      "start_time": 1700000000000,
      "end_time": 1700018000000,
      "remains_time": 240000
    }
  ]
}"#;

fn account() -> AccountRecord {
    AccountRecord::create(
        "MiniMax account",
        "minimax@local.invalid",
        None,
        MINIMAX,
        None,
    )
    .unwrap()
}

fn snapshot_of(body: &str, now: DateTime<Utc>) -> UsageSnapshot {
    parse_remains(body).unwrap().snapshot(&account(), now)
}

#[test]
fn token_plan_lanes_become_five_hour_and_weekly_bars() {
    let now = DateTime::from_timestamp(1_782_050_596, 0).unwrap();
    let snapshot = snapshot_of(TOKEN_PLAN, now);

    let five_hour = snapshot.primary.unwrap();
    assert_eq!(five_hour.name, "5 hours");
    assert_eq!(five_hour.used_percent, 0.0);
    assert_eq!(five_hour.limit_window_seconds, 4 * 60 * 60);
    assert_eq!(
        five_hour.reset_at_utc,
        DateTime::from_timestamp_millis(1_782_057_600_000)
    );
    assert_eq!(
        snapshot.primary_window_kind,
        Some(UsagePrimaryWindowKind::Session)
    );

    let weekly = snapshot.secondary.unwrap();
    assert_eq!(weekly.name, "Weekly");
    assert_eq!(weekly.used_percent, 30.0);

    // The video lane is not part of this plan, which makes it Plus.
    assert!(snapshot.additional_windows.is_empty());
    assert_eq!(snapshot.plan_type.as_deref(), Some("Plus"));
}

#[test]
fn legacy_counts_are_remaining_not_used() {
    let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let snapshot = snapshot_of(LEGACY_CODING_PLAN, now);
    let window = snapshot.primary.unwrap();
    // 250 of 1000 remain, so 75% is used.
    assert_eq!(window.used_percent, 75.0);
    assert_eq!(window.name, "5 hours");
    assert_eq!(snapshot.plan_type.as_deref(), Some("Max"));
    assert!(snapshot.secondary.is_none());
}

#[test]
fn other_services_follow_the_text_lanes_with_their_names() {
    let now = DateTime::from_timestamp(1_782_050_596, 0).unwrap();
    let body = TOKEN_PLAN
        .replace(
            r#""model_name": "video",
      "current_weekly_total_count": 0,"#,
            r#""model_name": "speech-2.8-hd",
      "current_weekly_total_count": 0,"#,
        )
        .replace(
            r#""current_interval_status": 3,
      "current_interval_remaining_percent": 100,"#,
            r#""current_interval_status": 1,
      "current_interval_remaining_percent": 40,"#,
        );
    let snapshot = snapshot_of(&body, now);
    assert_eq!(snapshot.additional_windows.len(), 1);
    let speech = &snapshot.additional_windows[0];
    assert_eq!(speech.name, "Text to Speech · Daily");
    assert_eq!(speech.window.used_percent, 60.0);
    // Speech has no weekly lane, even though the counts are present.
    assert_eq!(snapshot.secondary.unwrap().name, "Weekly");
}

#[test]
fn status_errors_and_missing_quotas_are_failures() {
    assert!(matches!(
        parse_remains(r#"{"base_resp":{"status_code":1004,"status_msg":"invalid api key"}}"#),
        Err(Failure::Rejected(_))
    ));
    assert!(matches!(
        parse_remains(r#"{"base_resp":{"status_code":2013,"status_msg":"invalid params"}}"#),
        Err(Failure::Payload(message)) if message == "invalid params"
    ));
    assert!(matches!(
        parse_remains(r#"{"base_resp":{"status_code":0},"model_remains":[]}"#),
        Err(Failure::Payload(_))
    ));
    // Data may also be nested under `data`.
    let nested = format!(r#"{{"data":{LEGACY_CODING_PLAN}}}"#);
    assert!(parse_remains(&nested).is_ok());
}

/// Answers by host and path: `accepting` serves usage on `path`, other
/// requests are rejected or missing.
struct RouteTransport {
    accepting: &'static str,
    path: &'static str,
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[async_trait]
impl UsageHttpTransport for RouteTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let host_ok = request.url.host_str() == Some(self.accepting);
        let path_ok = request.url.path() == self.path;
        self.requests.lock().unwrap().push(request);
        let (status_code, body) = match (host_ok, path_ok) {
            (true, true) => (200, LEGACY_CODING_PLAN),
            (true, false) => (404, "{}"),
            (false, _) => (401, "{}"),
        };
        Ok(UsageHttpResponse {
            status_code,
            body: body.to_owned(),
            headers: BTreeMap::new(),
        })
    }
}

struct RegionAuth(Option<&'static str>);

#[async_trait]
impl AccountAuthMaterialProvider for RegionAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("sk-cp-test".to_owned()),
            secondary_bearer_token: self.0.map(str::to_owned),
            ..AccountAuthMaterial::default()
        }))
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn routes(requests: &Mutex<Vec<UsageHttpRequest>>) -> Vec<String> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| format!("{}{}", request.url.host_str().unwrap(), request.url.path()))
        .collect()
}

#[test]
fn a_china_key_is_found_after_global_rejects_it() {
    let transport = Arc::new(RouteTransport {
        accepting: "api.minimaxi.com",
        path: "/v1/token_plan/remains",
        requests: Mutex::new(Vec::new()),
    });
    assert_eq!(
        block_on(detect_region(transport.as_ref(), "sk-cp-test")),
        Ok(MiniMaxRegion::ChinaMainland)
    );
    assert_eq!(
        routes(&transport.requests),
        [
            "api.minimax.io/v1/token_plan/remains",
            "api.minimax.io/v1/api/openplatform/coding_plan/remains",
            "api.minimaxi.com/v1/token_plan/remains",
        ]
    );
    assert_eq!(MiniMaxRegion::ChinaMainland.stored(), Some("cn"));

    transport.requests.lock().unwrap().clear();
    let adapter =
        MiniMaxUsageAdapter::new(transport.clone(), Arc::new(RegionAuth(Some("cn")))).unwrap();
    let result = block_on(adapter.probe(&account())).unwrap();
    assert!(result.snapshot.is_some());
    let request = transport.requests.lock().unwrap()[0].clone();
    assert_eq!(
        request.headers.get("Authorization").map(String::as_str),
        Some("Bearer sk-cp-test")
    );
}

#[test]
fn a_legacy_coding_plan_key_falls_back_to_the_older_endpoint() {
    let transport = Arc::new(RouteTransport {
        accepting: "api.minimax.io",
        path: "/v1/api/openplatform/coding_plan/remains",
        requests: Mutex::new(Vec::new()),
    });
    let adapter = MiniMaxUsageAdapter::new(transport.clone(), Arc::new(RegionAuth(None))).unwrap();
    let result = block_on(adapter.probe(&account())).unwrap();
    assert_eq!(result.snapshot.unwrap().primary.unwrap().used_percent, 75.0);
    assert_eq!(routes(&transport.requests).len(), 2);

    // A key no host accepts is reported as rejected.
    let nowhere = Arc::new(RouteTransport {
        accepting: "example.invalid",
        path: "/",
        requests: Mutex::new(Vec::new()),
    });
    let adapter = MiniMaxUsageAdapter::new(nowhere.clone(), Arc::new(RegionAuth(None))).unwrap();
    let rejected = block_on(adapter.probe(&account())).unwrap();
    assert_eq!(
        rejected.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );
    assert!(block_on(detect_region(nowhere.as_ref(), "sk-cp-test")).is_err());
}
