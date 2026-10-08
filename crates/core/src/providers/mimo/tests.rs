//! Payload cases follow MiMo's observed responses.

use super::*;
use crate::auth::AccountAuthMaterial;
use std::sync::Mutex;

const BALANCE: &str = r#"{"code":0,"message":"","data":{"balance":"25.51","currency":"USD","cashBalance":"20","giftBalance":"5.51"}}"#;
const DETAIL: &str = r#"{"code":0,"message":"","data":{"planCode":"standard","currentPeriodEnd":"2026-05-04 23:59:59","expired":false}}"#;
const USAGE: &str = r#"{"code":0,"message":"","data":{"monthUsage":{"percent":0.0505,"items":[{"name":"month_total_token","used":10100158,"limit":200000000,"percent":0.0505}]}}}"#;
const COOKIE: &str = "api-platform_serviceToken=secret-token; userId=42";

fn account() -> AccountRecord {
    AccountRecord::create(
        "Xiaomi MiMo account",
        "mimo@local.invalid",
        None,
        MIMO,
        None,
    )
    .unwrap()
}

#[test]
fn only_the_mimo_cookies_are_kept_from_a_pasted_header() {
    assert_eq!(
        cookie_header(
            "Cookie: _ga=GA1; userId=42; api-platform_serviceToken=secret-token; api-platform_ph=x"
        )
        .as_deref(),
        Some("api-platform_ph=x; api-platform_serviceToken=secret-token; userId=42")
    );
    // A copied curl argument works too.
    assert_eq!(
        cookie_header("-H 'cookie: userId=42; api-platform_serviceToken=secret-token'").as_deref(),
        Some("api-platform_serviceToken=secret-token; userId=42")
    );
    assert_eq!(cookie_header("userId=42; other=1"), None);
    assert_eq!(cookie_header("api-platform_serviceToken=; userId=42"), None);
}

#[test]
fn balance_and_token_plan_become_a_card() {
    let balance = parse_balance(BALANCE).unwrap();
    let detail: Value = serde_json::from_str(DETAIL).unwrap();
    let usage: Value = serde_json::from_str(USAGE).unwrap();
    let snapshot = MiMoUsage {
        balance,
        plan: parse_plan_detail(&detail["data"]),
        tokens: parse_token_usage(&usage["data"]),
    }
    .snapshot(&account(), Utc::now());

    let window = snapshot.primary.unwrap();
    assert_eq!(window.name, TOKEN_PLAN_WINDOW_NAME);
    assert!((window.used_percent - 5.05).abs() < 1e-9);
    assert_eq!(
        window.reset_at_utc,
        DateTime::parse_from_rfc3339("2026-05-04T23:59:59Z")
            .ok()
            .map(|time| time.with_timezone(&Utc))
    );
    assert_eq!(snapshot.plan_type.as_deref(), Some("Standard"));
    let keys = snapshot
        .metrics
        .iter()
        .map(|metric| (metric.key.as_str(), metric.remaining_amount))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            ("balance", Some(25.51)),
            ("balance.topped_up", Some(20.0)),
            ("balance.granted", Some(5.51)),
        ]
    );
}

#[test]
fn balance_without_a_plan_is_a_spend_card() {
    let balance =
        parse_balance(r#"{"code":0,"data":{"balance":"10.00","currency":"CNY"}}"#).unwrap();
    let snapshot = MiMoUsage {
        balance,
        plan: PlanDetail::default(),
        tokens: None,
    }
    .snapshot(&account(), Utc::now());
    assert!(snapshot.primary.is_none());
    assert_eq!(
        snapshot.primary_window_kind,
        Some(UsagePrimaryWindowKind::Spend)
    );
    assert_eq!(snapshot.metrics.len(), 1);
    assert_eq!(snapshot.metrics[0].unit.as_deref(), Some("CNY"));
}

#[test]
fn errors_and_signed_out_sessions_are_told_apart() {
    assert!(matches!(
        parse_balance(r#"{"code":401,"message":"login"}"#),
        Err(Failure::SignedOut(_))
    ));
    assert!(matches!(
        parse_balance("<html>sign in</html>"),
        Err(Failure::SignedOut(_))
    ));
    assert!(matches!(
        parse_balance(r#"{"code":500,"message":"busy"}"#),
        Err(Failure::Payload(message)) if message == "busy"
    ));
    assert!(matches!(
        parse_balance(r#"{"code":0,"data":{"balance":"x","currency":"USD"}}"#),
        Err(Failure::Payload(_))
    ));
    // An expired plan has no reset to show.
    let expired: Value = serde_json::from_str(
        r#"{"planCode":"standard","currentPeriodEnd":"2026-05-04 23:59:59","expired":true}"#,
    )
    .unwrap();
    assert_eq!(parse_plan_detail(&expired).period_end, None);
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

struct CookieAuth(&'static str);

#[async_trait]
impl AccountAuthMaterialProvider for CookieAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some(self.0.to_owned()),
            ..AccountAuthMaterial::default()
        }))
    }
}

fn probe(
    routes: Vec<(&'static str, u16, &'static str)>,
) -> (UsageProbeResult, Vec<UsageHttpRequest>) {
    let transport = Arc::new(RoutedTransport {
        routes,
        requests: Mutex::new(Vec::new()),
    });
    let adapter = MiMoUsageAdapter::new(transport.clone(), Arc::new(CookieAuth(COOKIE))).unwrap();
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
fn the_probe_reads_the_console_with_the_session_cookie() {
    let (result, requests) = probe(vec![
        ("/api/v1/balance", 200, BALANCE),
        ("/api/v1/tokenPlan/detail", 200, DETAIL),
        ("/api/v1/tokenPlan/usage", 200, USAGE),
    ]);
    let snapshot = result.snapshot.unwrap();
    assert!(snapshot.primary.is_some());
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| {
        request.url.host_str() == Some("platform.xiaomimimo.com")
            && request.headers.get("Cookie").map(String::as_str) == Some(COOKIE)
    }));
}

#[test]
fn a_missing_plan_never_hides_the_balance_and_a_redirect_asks_to_sign_in() {
    let (result, _) = probe(vec![("/api/v1/balance", 200, BALANCE)]);
    let snapshot = result.snapshot.unwrap();
    assert!(snapshot.primary.is_none());
    assert_eq!(snapshot.credits.unwrap().balance, Some(25.51));

    let (result, _) = probe(vec![("/api/v1/balance", 302, "")]);
    assert_eq!(
        result.error.unwrap().code,
        UsageAdapterErrorCode::Unauthorized
    );
}
