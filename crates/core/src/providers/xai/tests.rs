//! Payload cases follow CodexBar's xAI plugin fixtures.

use super::*;
use crate::auth::AccountAuthMaterial;
use std::sync::Mutex;

const BALANCE: &str = r#"{"total":{"val":"-1000"}}"#;
const USAGE: &str = r#"{
  "timeSeries": [
    {"dataPoints": [
      {"timestamp":"2027-01-13T00:00:00Z","values":[0.75973725]},
      {"timestamp":"2027-01-14T00:00:00Z","values":[0.5]},
      {"timestamp":"2027-01-15T00:00:00Z","values":[0]}
    ]},
    {"dataPoints": [
      {"timestamp":"2027-01-13T00:00:00Z","values":[0.5]},
      {"timestamp":"2027-01-14T00:00:00Z","values":[0]},
      {"timestamp":"2027-01-15T00:00:00Z","values":[0]}
    ]}
  ],
  "limitReached": false
}"#;

fn account() -> AccountRecord {
    AccountRecord::create("xAI account", "xai@local.invalid", None, XAI, None).unwrap()
}

#[test]
fn ledger_balances_are_negated_cents() {
    assert_eq!(parse_balance(r#"{"total":{"val":"2500"}}"#), Ok(-25.0));
    assert_eq!(parse_balance(r#"{"total":{"val":"0"}}"#), Ok(0.0));
    assert_eq!(parse_balance(r#"{"total":{"val":"-333"}}"#), Ok(3.33));
    assert_eq!(
        parse_balance(r#"{"total":{"val":" -1000.5 "}}"#),
        Ok(10.005)
    );
    for body in [
        r#"{"total":{"val":"n/a"}}"#,
        r#"{"total":{"val":1000}}"#,
        r#"{"total":{"val":"1e3"}}"#,
        r#"{"total":{}}"#,
        "not json",
    ] {
        assert!(parse_balance(body).is_err(), "{body}");
    }
}

#[test]
fn the_history_request_covers_the_last_thirty_utc_days() {
    let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let request = usage_request(now);
    let range = &request["analyticsRequest"]["timeRange"];
    assert_eq!(range["startTime"], "2026-12-17 00:00:00");
    assert_eq!(range["endTime"], "2027-01-15 08:00:00");
    assert_eq!(range["timezone"], "Etc/GMT");
    assert_eq!(request["analyticsRequest"]["timeUnit"], "TIME_UNIT_DAY");
}

#[test]
fn history_sums_every_series_per_day() {
    let history = parse_history(USAGE).unwrap();
    assert_eq!(history.daily.len(), 3);
    assert!(!history.partial);
    let total: f64 = history.daily.values().sum();
    assert!((total - 1.75973725).abs() < 1e-9);

    let partial =
        parse_history(&USAGE.replace(r#""limitReached": false"#, r#""limitReached": true"#))
            .unwrap();
    assert!(partial.partial);

    // An empty history is a real zero, not a failure.
    let empty = parse_history(r#"{"timeSeries":[],"limitReached":false}"#).unwrap();
    assert!(empty.daily.is_empty());

    for body in [
        "{}",
        r#"{"timeSeries":null}"#,
        r#"{"timeSeries":[{}]}"#,
        r#"{"timeSeries":[{"dataPoints":[{"timestamp":"2027-01-15T00:00:00Z"}]}]}"#,
        r#"{"timeSeries":[{"dataPoints":[{"timestamp":"2027-01-15T00:00:00Z","values":[-1]}]}]}"#,
    ] {
        assert!(parse_history(body).is_err(), "{body}");
    }
}

#[test]
fn the_snapshot_shows_balance_today_and_thirty_days() {
    let now = DateTime::parse_from_rfc3339("2027-01-14T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let history = parse_history(USAGE).unwrap();
    let snapshot = snapshot(&account(), 10.0, Some(&history), now);
    let keys = snapshot
        .metrics
        .iter()
        .map(|metric| metric.key.as_str())
        .collect::<Vec<_>>();
    assert_eq!(keys, ["balance", "spend.today", "spend.30d"]);
    assert_eq!(snapshot.metrics[0].remaining_amount, Some(10.0));
    assert_eq!(snapshot.metrics[1].used_amount, Some(0.5));
    assert_eq!(snapshot.credits.as_ref().unwrap().balance, Some(10.0));
    assert_eq!(snapshot.data_confidence, "authoritative");

    // Without history only the balance shows.
    let balance_only = snapshot_without_history();
    assert_eq!(balance_only.metrics.len(), 1);
}

fn snapshot_without_history() -> UsageSnapshot {
    snapshot(&account(), 10.0, None, Utc::now())
}

#[test]
fn team_ids_must_be_one_path_segment() {
    assert!(valid_team_id("team-1234"));
    for team in ["", ".", "..", "team/../other"] {
        assert!(!valid_team_id(team), "{team}");
    }
}

struct KeyAuth(Option<&'static str>);

#[async_trait]
impl AccountAuthMaterialProvider for KeyAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("fixture-management-key".to_owned()),
            secondary_bearer_token: self.0.map(str::to_owned),
            ..AccountAuthMaterial::default()
        }))
    }
}

struct StubTransport {
    balance: (u16, &'static str),
    usage: (u16, &'static str),
    requests: Mutex<Vec<UsageHttpRequest>>,
}

impl StubTransport {
    fn new(balance: (u16, &'static str), usage: (u16, &'static str)) -> Arc<Self> {
        Arc::new(Self {
            balance,
            usage,
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl UsageHttpTransport for StubTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let (status_code, body) = if request.url.path().ends_with("/prepaid/balance") {
            self.balance
        } else {
            self.usage
        };
        self.requests.lock().unwrap().push(request);
        Ok(UsageHttpResponse {
            status_code,
            body: body.to_owned(),
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

fn probe(transport: Arc<StubTransport>, team: Option<&'static str>) -> UsageProbeResult {
    let adapter = XaiUsageAdapter::new(transport, Arc::new(KeyAuth(team))).unwrap();
    run(adapter.probe(&account())).unwrap()
}

#[test]
fn balance_and_usage_requests_match_codexbar() {
    let transport = StubTransport::new((200, BALANCE), (200, USAGE));
    let result = probe(transport.clone(), Some("team-1234"));
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.metrics[0].remaining_amount, Some(10.0));
    assert!(snapshot.source_diagnostics.is_empty());

    let requests = transport.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::GET);
    assert_eq!(
        requests[0].url.as_str(),
        "https://management-api.x.ai/v1/billing/teams/team-1234/prepaid/balance"
    );
    assert_eq!(requests[1].method, Method::POST);
    assert_eq!(
        requests[1].url.as_str(),
        "https://management-api.x.ai/v1/billing/teams/team-1234/usage"
    );
    for request in &requests {
        assert_eq!(
            request.headers.get("Authorization").map(String::as_str),
            Some("Bearer fixture-management-key")
        );
    }
}

#[test]
fn a_history_failure_keeps_the_balance() {
    let result = probe(
        StubTransport::new((200, BALANCE), (500, "{}")),
        Some("team-1234"),
    );
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.metrics.len(), 1);
    assert_eq!(snapshot.source_diagnostics.len(), 1);

    // A rejected key on either call is the account's error.
    let rejected = probe(
        StubTransport::new((200, BALANCE), (403, "{}")),
        Some("team-1234"),
    );
    assert_eq!(
        rejected.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );
}

#[test]
fn balance_errors_are_classified() {
    for (status, code) in [
        (401, UsageAdapterErrorCode::Unauthorized),
        (403, UsageAdapterErrorCode::Unauthorized),
        (404, UsageAdapterErrorCode::HttpError),
        (429, UsageAdapterErrorCode::RateLimited),
        (503, UsageAdapterErrorCode::TransientHttp),
    ] {
        let result = probe(
            StubTransport::new((status, "{}"), (200, USAGE)),
            Some("team-1234"),
        );
        assert_eq!(result.error.unwrap().code, code, "{status}");
    }
    let transport = StubTransport::new((200, BALANCE), (200, USAGE));
    let missing = probe(transport.clone(), None);
    assert_eq!(
        missing.error.unwrap().code,
        UsageAdapterErrorCode::AuthenticationUnavailable
    );
    let invalid = probe(transport.clone(), Some("team/../other"));
    assert!(invalid.error.is_some());
    assert!(transport.requests.lock().unwrap().is_empty());
}
