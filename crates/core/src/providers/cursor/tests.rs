//! Payload cases follow Cursor's observed responses.

use super::*;
use crate::auth::AccountAuthMaterial;
use std::sync::Mutex;

const USER_ID: &str = "user_01J8ABCDEF";

/// A JWT whose payload is `payload`; the signature is never checked here.
fn jwt(payload: &str) -> String {
    let encode = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
    format!(
        "{}.{}.signature",
        encode(r#"{"alg":"HS256"}"#),
        encode(payload)
    )
}

fn token_expiring_in(seconds: i64) -> String {
    let exp = Utc::now().timestamp() + seconds;
    jwt(&format!(
        r#"{{"sub":"auth0|{USER_ID}","email":"dev@example.com","exp":{exp}}}"#
    ))
}

fn account() -> AccountRecord {
    AccountRecord::create(
        "dev@example.com",
        "dev@example.com",
        Some(USER_ID.to_owned()),
        CURSOR,
        None,
    )
    .unwrap()
}

fn snapshot_of(summary: &str) -> UsageSnapshot {
    CursorUsage {
        summary: parse_summary(summary).unwrap(),
        requests: None,
        grok_bot: None,
    }
    .snapshot(&account(), Utc::now())
}

#[test]
fn a_session_reads_its_user_and_expiry_from_the_token() {
    let token = token_expiring_in(3600);
    let session = CursorSession::from_access_token(&token).unwrap();
    assert_eq!(session.user_id, USER_ID);
    assert_eq!(session.email.as_deref(), Some("dev@example.com"));
    assert!(session.is_usable(Utc::now()));
    assert_eq!(
        session.cookie_header(),
        format!("WorkosCursorSessionToken={USER_ID}%3A%3A{token}")
    );

    // Within a minute of expiry the token no longer counts.
    let expiring = CursorSession::from_access_token(&token_expiring_in(30)).unwrap();
    assert!(!expiring.is_usable(Utc::now()));
    assert!(CursorSession::from_access_token("not-a-token").is_err());
    // The user id goes into a cookie, so odd characters are refused.
    assert!(
        CursorSession::from_access_token(&jwt(r#"{"sub":"auth0|a;b","exp":9999999999}"#)).is_err()
    );
}

#[test]
fn the_token_is_found_in_any_pasted_form() {
    let token = token_expiring_in(3600);
    for input in [
        token.clone(),
        format!("{USER_ID}::{token}"),
        format!("{USER_ID}%3A%3A{token}"),
        format!("WorkosCursorSessionToken={USER_ID}%3A%3A{token}"),
        format!("Cookie: other=1; WorkosCursorSessionToken={USER_ID}%3A%3A{token}; theme=dark"),
        format!("  \"{token}\"  "),
    ] {
        assert_eq!(
            access_token_from_input(&input).as_deref(),
            Some(token.as_str()),
            "{input}"
        );
    }
    assert_eq!(access_token_from_input("hello"), None);
    assert_eq!(
        access_token_from_input("WorkosCursorSessionToken=abc"),
        None
    );
}

#[test]
fn plan_percentages_become_total_auto_and_api_bars() {
    let snapshot = snapshot_of(
        r#"{"billingCycleStart":"2025-01-01T00:00:00.000Z","billingCycleEnd":"2025-02-01T00:00:00.000Z",
            "membershipType":"pro",
            "individualUsage":{"plan":{"enabled":true,"used":1500,"limit":2000,"remaining":500,
              "autoPercentUsed":12.5,"apiPercentUsed":40,"totalPercentUsed":30}}}"#,
    );
    let total = snapshot.primary.unwrap();
    assert_eq!(total.name, TOTAL_WINDOW_NAME);
    assert_eq!(total.used_percent, 30.0);
    assert_eq!(
        total.reset_at_utc.unwrap().to_rfc3339(),
        "2025-02-01T00:00:00+00:00"
    );
    assert_eq!(total.limit_window_seconds, 31 * 24 * 60 * 60);
    let auto = snapshot.secondary.unwrap();
    assert_eq!(auto.name, AUTO_WINDOW_NAME);
    assert_eq!(auto.used_percent, 12.5);
    assert_eq!(snapshot.additional_windows.len(), 1);
    assert_eq!(snapshot.additional_windows[0].name, API_WINDOW_NAME);
    assert_eq!(snapshot.additional_windows[0].window.used_percent, 40.0);
    assert_eq!(snapshot.plan_type.as_deref(), Some("Pro"));
    assert!(snapshot.metrics.is_empty());
}

#[test]
fn the_total_falls_back_through_the_other_fields() {
    // Average of Auto and API when there is no total.
    let averaged =
        snapshot_of(r#"{"individualUsage":{"plan":{"autoPercentUsed":10,"apiPercentUsed":30}}}"#);
    assert_eq!(averaged.primary.unwrap().used_percent, 20.0);

    // Fractional percents are percents already: 0.36 is 0.36%.
    let fractional = snapshot_of(r#"{"individualUsage":{"plan":{"apiPercentUsed":0.36}}}"#);
    assert_eq!(fractional.primary.unwrap().used_percent, 0.36);

    // Cents used of the plan limit.
    let from_cents = snapshot_of(r#"{"individualUsage":{"plan":{"used":500,"limit":2000}}}"#);
    assert_eq!(from_cents.primary.unwrap().used_percent, 25.0);

    // Enterprise members' personal cap, then the team pool.
    let overall = snapshot_of(
        r#"{"membershipType":"enterprise","individualUsage":{"overall":{"used":3000,"limit":10000}}}"#,
    );
    assert_eq!(overall.primary.unwrap().used_percent, 30.0);
    let pooled = snapshot_of(r#"{"teamUsage":{"pooled":{"used":"7500","limit":"10000"}}}"#);
    assert_eq!(pooled.primary.unwrap().used_percent, 75.0);

    // Over-quota values are clamped for the bar.
    let over = snapshot_of(r#"{"individualUsage":{"plan":{"totalPercentUsed":135}}}"#);
    assert_eq!(over.primary.unwrap().used_percent, 100.0);
}

#[test]
fn on_demand_spend_prefers_a_personal_cap_over_the_team_budget() {
    let personal = snapshot_of(
        r#"{"individualUsage":{"plan":{"totalPercentUsed":100},"onDemand":{"used":1234,"limit":5000}},
            "teamUsage":{"onDemand":{"used":90000,"limit":100000}}}"#,
    );
    assert_eq!(personal.metrics.len(), 1);
    assert_eq!(personal.metrics[0].key, "on_demand");
    assert_eq!(personal.metrics[0].used_amount, Some(12.34));
    assert_eq!(personal.metrics[0].limit_amount, Some(50.0));
    assert_eq!(personal.metrics[0].unit.as_deref(), Some("USD"));

    let team = snapshot_of(
        r#"{"membershipType":"team","individualUsage":{"onDemand":{"used":700}},
            "teamUsage":{"onDemand":{"used":25000,"limit":100000}}}"#,
    );
    let keys = team
        .metrics
        .iter()
        .map(|metric| metric.key.as_str())
        .collect::<Vec<_>>();
    assert_eq!(keys, ["on_demand.team", "on_demand.yours"]);
    assert_eq!(team.metrics[0].used_amount, Some(250.0));
    assert_eq!(team.metrics[0].limit_amount, Some(1000.0));
    assert_eq!(team.metrics[1].used_amount, Some(7.0));

    // No spend and no budget: no row.
    let none = snapshot_of(r#"{"individualUsage":{"onDemand":{"used":0}}}"#);
    assert!(none.metrics.is_empty());
}

#[test]
fn a_legacy_request_plan_shows_only_its_request_quota() {
    let usage = CursorUsage {
        summary: parse_summary(
            r#"{"billingCycleEnd":"2025-02-01T00:00:00Z","individualUsage":{"plan":{"autoPercentUsed":5,"apiPercentUsed":7}}}"#,
        )
        .unwrap(),
        requests: parse_request_usage(&serde_json::json!({
            "gpt-4": {"numRequests": 120, "numRequestsTotal": 125, "maxRequestUsage": 500}
        })),
        grok_bot: None,
    };
    let snapshot = usage.snapshot(&account(), Utc::now());
    let requests = snapshot.primary.unwrap();
    assert_eq!(requests.name, REQUESTS_WINDOW_NAME);
    assert_eq!(requests.used_percent, 25.0);
    assert!(snapshot.secondary.is_none());
    assert!(snapshot.additional_windows.is_empty());

    // Token-based plans report no request ceiling.
    assert!(parse_request_usage(&serde_json::json!({"gpt-4": {"numRequests": 3}})).is_none());
}

#[test]
fn grok_bot_shows_for_a_paid_allowance_or_an_unexpired_trial() {
    let now = Utc::now();
    let paid = GrokBotUsage::parse(&serde_json::json!({
        "usagePercent": 42.5, "includedLimitZero": false, "hasNonZeroIncludedLimit": false,
        "currentPeriodStart": "2025-01-06T00:00:00Z", "nextResetTimestampUtc": "2025-01-13T00:00:00Z",
    }))
    .window(now)
    .unwrap();
    assert_eq!(paid.name, GROK_BOT_WINDOW_NAME);
    assert_eq!(paid.used_percent, 42.5);
    assert_eq!(paid.limit_window_seconds, 7 * 24 * 60 * 60);
    assert!(paid.reset_at_utc.is_some());

    // A trial has no recurring reset.
    let trial_end = (now + chrono::Duration::days(3)).to_rfc3339();
    let trial = GrokBotUsage::parse(&serde_json::json!({
        "usagePercent": 100, "includedLimitZero": true, "sandTrialExpiresAt": trial_end,
        "nextResetTimestampUtc": "2025-01-13T00:00:00Z",
    }))
    .window(now)
    .unwrap();
    assert_eq!(trial.reset_at_utc, None);

    let expired_trial = (now - chrono::Duration::days(1)).to_rfc3339();
    assert!(
        GrokBotUsage::parse(&serde_json::json!({
            "usagePercent": 10, "includedLimitZero": true, "sandTrialExpiresAt": expired_trial,
        }))
        .window(now)
        .is_none()
    );
    // The older flag still counts when the newer one is missing.
    assert!(
        GrokBotUsage::parse(&serde_json::json!({
            "usagePercent": 10, "hasNonZeroIncludedLimit": true,
        }))
        .window(now)
        .is_some()
    );
}

#[test]
fn the_app_token_is_read_from_cursors_state_database() {
    let directory = std::env::temp_dir().join(format!("cursor-state-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let database = directory.join("state.vscdb");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
            )
            .unwrap();
        // Stored as UTF-16LE, which also reads as UTF-8 with NULs.
        let utf16 = "abc.def.ghi"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                rusqlite::params![APP_TOKEN_KEY, utf16],
            )
            .unwrap();
    }
    assert_eq!(
        read_app_token(&database).unwrap().as_deref(),
        Some("abc.def.ghi")
    );

    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![APP_TOKEN_KEY, "jkl.mno.pqr"],
        )
        .unwrap();
    assert_eq!(
        read_app_token(&database).unwrap().as_deref(),
        Some("jkl.mno.pqr")
    );
    assert_eq!(
        read_app_token(&directory.join("missing.vscdb")).unwrap(),
        None
    );
    std::fs::remove_dir_all(&directory).unwrap();
}

struct TokenAuth(String);

#[async_trait]
impl AccountAuthMaterialProvider for TokenAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some(self.0.clone()),
            ..AccountAuthMaterial::default()
        }))
    }
}

/// Answers each endpoint by its path.
struct RoutedTransport {
    routes: Vec<(&'static str, u16, &'static str)>,
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[async_trait]
impl UsageHttpTransport for RoutedTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let path = request.url.path().to_owned();
        self.requests.lock().unwrap().push(request);
        let (status_code, body) = self
            .routes
            .iter()
            .find(|(route, _, _)| *route == path)
            .map_or((404, "{}"), |(_, status, body)| (*status, *body));
        Ok(UsageHttpResponse {
            status_code,
            body: body.to_owned(),
            headers: BTreeMap::new(),
        })
    }
}

fn probe(
    token: String,
    routes: Vec<(&'static str, u16, &'static str)>,
) -> (UsageProbeResult, Vec<UsageHttpRequest>) {
    let transport = Arc::new(RoutedTransport {
        routes,
        requests: Mutex::new(Vec::new()),
    });
    let adapter = CursorUsageAdapter::new(transport.clone(), Arc::new(TokenAuth(token)))
        .unwrap()
        .with_app_database(None);
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
fn the_probe_reads_the_dashboard_endpoints_with_the_session_cookie() {
    let token = token_expiring_in(3600);
    let (result, requests) = probe(
        token.clone(),
        vec![
            (
                "/api/usage-summary",
                200,
                r#"{"membershipType":"pro_plus","individualUsage":{"plan":{"totalPercentUsed":55,"autoPercentUsed":20}}}"#,
            ),
            (
                "/api/auth/me",
                200,
                r#"{"email":"dev@example.com","sub":"auth0|user_01J8ABCDEF","name":"Dev"}"#,
            ),
            (
                "/api/dashboard/get-sand-usage-status",
                200,
                r#"{"usagePercent":5,"includedLimitZero":false,"nextResetTimestampUtc":"2099-01-01T00:00:00Z"}"#,
            ),
            ("/api/usage", 200, r#"{"gpt-4":{"numRequests":0}}"#),
        ],
    );
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.provider_id, CURSOR);
    assert_eq!(snapshot.primary.unwrap().used_percent, 55.0);
    assert_eq!(snapshot.secondary.unwrap().used_percent, 20.0);
    assert_eq!(snapshot.additional_windows[0].name, GROK_BOT_WINDOW_NAME);
    assert_eq!(snapshot.plan_type.as_deref(), Some("Pro+"));
    assert_eq!(snapshot.observed_email.as_deref(), Some("dev@example.com"));
    let identity = result.identity.unwrap();
    assert_eq!(identity.provider_account_id.as_deref(), Some(USER_ID));

    let cookie = format!("WorkosCursorSessionToken={USER_ID}%3A%3A{token}");
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("Cookie") == Some(&cookie))
    );
    let sand = requests
        .iter()
        .find(|request| request.url.path() == "/api/dashboard/get-sand-usage-status")
        .unwrap();
    assert_eq!(sand.method, Method::POST);
    assert_eq!(
        sand.headers.get("Origin").map(String::as_str),
        Some("https://cursor.com")
    );
    let legacy = requests
        .iter()
        .find(|request| request.url.path() == "/api/usage")
        .unwrap();
    assert_eq!(legacy.url.query(), Some("user=auth0%7Cuser_01J8ABCDEF"));
}

#[test]
fn optional_endpoints_failing_never_hide_the_plan_usage() {
    let (result, _) = probe(
        token_expiring_in(3600),
        vec![(
            "/api/usage-summary",
            200,
            r#"{"individualUsage":{"plan":{"totalPercentUsed":10}}}"#,
        )],
    );
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.primary.unwrap().used_percent, 10.0);
    assert!(snapshot.additional_windows.is_empty());
    // The token's own email names the account when /auth/me fails.
    assert_eq!(snapshot.observed_email.as_deref(), Some("dev@example.com"));
}

#[test]
fn a_rejected_or_expired_session_asks_for_a_new_sign_in() {
    let (rejected, _) = probe(
        token_expiring_in(3600),
        vec![("/api/usage-summary", 401, r#"{"error":"unauthorized"}"#)],
    );
    assert_eq!(
        rejected.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );

    let (expired, requests) = probe(token_expiring_in(-10), Vec::new());
    assert_eq!(
        expired.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );
    assert!(requests.is_empty());
}

const TEAM_SUMMARY: &str = r#"{"membershipType":"enterprise","billingCycleEnd":"2099-02-01T00:00:00.000Z","individualUsage":{"plan":{"totalPercentUsed":10}}}"#;
const ME: &str = r#"{"email":"Dev@Example.com","sub":"auth0|user_01J8ABCDEF"}"#;

#[test]
fn a_team_member_sees_their_own_budget() {
    let (result, requests) = probe(
        token_expiring_in(3600),
        vec![
            ("/api/usage-summary", 200, TEAM_SUMMARY),
            ("/api/auth/me", 200, ME),
            ("/api/dashboard/teams", 200, r#"{"teams":[{"id":42}]}"#),
            (
                "/api/dashboard/get-team-spend",
                200,
                r#"{"totalPages":1,"teamMemberSpend":[
                    {"email":"other@example.com","overallSpendCents":9000,"monthlyLimitDollars":100},
                    {"email":"dev@example.com","overallSpendCents":1234,"effectivePerUserLimitDollars":50,"monthlyLimitDollars":500}
                ]}"#,
            ),
        ],
    );
    let snapshot = result.snapshot.unwrap();
    let budget = snapshot
        .metrics
        .iter()
        .find(|metric| metric.key == TEAM_BUDGET_KEY)
        .unwrap();
    assert_eq!(budget.used_amount, Some(12.34));
    assert_eq!(budget.limit_amount, Some(50.0));
    assert!(budget.reset_at_utc.is_some());
    let spend = requests
        .iter()
        .find(|request| request.url.path() == "/api/dashboard/get-team-spend")
        .unwrap();
    let body: Value = serde_json::from_str(spend.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["teamId"], 42);
    assert_eq!(body["pageSize"], 50);
    assert_eq!(
        spend.headers.get("Referer").map(String::as_str),
        Some("https://cursor.com/dashboard")
    );
}

#[test]
fn an_unclear_team_budget_is_left_out() {
    let budget_of = |teams: &'static str, spend: &'static str| {
        let (result, _) = probe(
            token_expiring_in(3600),
            vec![
                ("/api/usage-summary", 200, TEAM_SUMMARY),
                ("/api/auth/me", 200, ME),
                ("/api/dashboard/teams", 200, teams),
                ("/api/dashboard/get-team-spend", 200, spend),
            ],
        );
        result
            .snapshot
            .unwrap()
            .metrics
            .iter()
            .any(|metric| metric.key == TEAM_BUDGET_KEY)
    };
    let one_team = r#"{"teams":[{"id":42}]}"#;
    let member = r#"{"totalPages":1,"teamMemberSpend":[{"email":"dev@example.com","overallSpendCents":100,"monthlyLimitDollars":20}]}"#;
    assert!(budget_of(one_team, member));
    // Two teams and no selection: which one is unknown.
    assert!(!budget_of(r#"{"teams":[{"id":42},{"id":7}]}"#, member));
    // An explicit zero per-user limit is no budget, not the monthly one.
    assert!(!budget_of(
        one_team,
        r#"{"totalPages":1,"teamMemberSpend":[{"email":"dev@example.com","overallSpendCents":100,"effectivePerUserLimitDollars":0,"monthlyLimitDollars":20}]}"#
    ));
    // The same email twice is ambiguous.
    assert!(!budget_of(
        one_team,
        r#"{"totalPages":1,"teamMemberSpend":[
            {"email":"dev@example.com","overallSpendCents":100,"monthlyLimitDollars":20},
            {"email":"DEV@example.com","overallSpendCents":5,"monthlyLimitDollars":20}
        ]}"#
    ));
    // A short page that claims more pages follow does not add up.
    assert!(!budget_of(
        one_team,
        r#"{"totalPages":2,"teamMemberSpend":[{"email":"dev@example.com","overallSpendCents":100,"monthlyLimitDollars":20}]}"#
    ));
}

#[test]
fn personal_plans_never_ask_for_team_spend() {
    let (_, requests) = probe(
        token_expiring_in(3600),
        vec![
            (
                "/api/usage-summary",
                200,
                r#"{"membershipType":"pro","individualUsage":{"plan":{"totalPercentUsed":10}}}"#,
            ),
            ("/api/auth/me", 200, ME),
        ],
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.url.path().starts_with("/api/dashboard/teams"))
    );
}
