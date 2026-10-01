use super::*;
use std::{collections::VecDeque, sync::Mutex};
use tokio::sync::Notify;

struct StaticAuthMaterialProvider(AccountAuthMaterial);

#[async_trait]
impl AccountAuthMaterialProvider for StaticAuthMaterialProvider {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(self.0.clone()))
    }
}

struct ConsoleBillingTransport {
    status_body: String,
    billing_body: String,
    billing_delay: StdDuration,
    wait_for_billing_before_status: bool,
    billing_started: Notify,
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[async_trait]
impl UsageHttpTransport for ConsoleBillingTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let path = request.url.path().to_owned();
        self.requests.lock().unwrap().push(request);

        if path.ends_with(CONSOLE_BILLING_PATH) {
            self.billing_started.notify_one();
            tokio::time::sleep(self.billing_delay).await;
            return Ok(json_response(200, &self.billing_body));
        }
        if path.ends_with(CONSOLE_STATUS_PATH) {
            if self.wait_for_billing_before_status {
                tokio::time::timeout(StdDuration::from_secs(1), self.billing_started.notified())
                    .await
                    .expect("billing request should start before status completes");
            }
            return Ok(json_response(200, &self.status_body));
        }

        Err(TransportError::InvalidUrl(format!(
            "unexpected test request path: {path}"
        )))
    }
}

fn json_response(status_code: u16, body: &str) -> UsageHttpResponse {
    UsageHttpResponse {
        status_code,
        body: body.to_owned(),
        headers: Default::default(),
    }
}

fn console_probe_adapter(transport: Arc<dyn UsageHttpTransport>) -> OpenCodeGoUsageAdapter {
    OpenCodeGoUsageAdapter::new(
        transport,
        Arc::new(StaticAuthMaterialProvider(
            AccountAuthMaterial::from_cookie_header("session=test-session", None),
        )),
    )
    .unwrap()
    .with_source_mode(OpenCodeGoSourceMode::Web)
}

fn console_probe_account() -> AccountRecord {
    AccountRecord::create(
        "go",
        "go@example.com",
        None,
        OPENCODE_GO,
        Some("wrk_123".to_owned()),
    )
    .unwrap()
}

fn active_go_status() -> String {
    json!({
        "access": {"meters": {
            "fiveHour": {"usedMicroCents": 1_000_000, "limitMicroCents": 10_000_000},
            "week": {"usedMicroCents": 3_000_000, "limitMicroCents": 10_000_000}
        }}
    })
    .to_string()
}

#[tokio::test]
async fn console_balance_enrichment_is_parallel_and_bounded() {
    let transport = Arc::new(ConsoleBillingTransport {
        status_body: active_go_status(),
        billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
        billing_delay: StdDuration::from_millis(500),
        wait_for_billing_before_status: true,
        billing_started: Notify::new(),
        requests: Mutex::new(Vec::new()),
    });
    let adapter = console_probe_adapter(transport.clone());

    let result = adapter.probe(&console_probe_account()).await.unwrap();
    let snapshot = result.snapshot.unwrap();

    assert!(snapshot.primary.is_some());
    assert!(snapshot.credits.is_none());
    assert!(snapshot.source_diagnostics.iter().any(|item| {
        item.source == "web.console.billing" && item.message.contains("wait bound")
    }));
    let paths = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert!(paths.iter().any(|path| path.ends_with(CONSOLE_STATUS_PATH)));
    assert!(
        paths
            .iter()
            .any(|path| path.ends_with(CONSOLE_BILLING_PATH))
    );
    let requests = transport.requests.lock().unwrap();
    for request in requests.iter() {
        assert_eq!(
            request.headers.get("Cookie").map(String::as_str),
            Some("session=test-session")
        );
        assert_eq!(
            request.headers.get("x-org-id").map(String::as_str),
            Some("wrk_123")
        );
    }
}

#[tokio::test]
async fn console_usage_accepts_an_account_scoped_oauth_bearer_without_cookies() {
    let transport = Arc::new(ConsoleBillingTransport {
        status_body: active_go_status(),
        billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
        billing_delay: StdDuration::from_millis(1),
        wait_for_billing_before_status: false,
        billing_started: Notify::new(),
        requests: Mutex::new(Vec::new()),
    });
    let adapter = OpenCodeGoUsageAdapter::new(
        transport.clone(),
        Arc::new(StaticAuthMaterialProvider(AccountAuthMaterial {
            bearer_token: Some("opencode-access-token".to_owned()),
            oauth_refresh_token: Some("opencode-refresh-token".to_owned()),
            ..AccountAuthMaterial::default()
        })),
    )
    .unwrap()
    .with_source_mode(OpenCodeGoSourceMode::Automatic);

    let result = adapter.probe(&console_probe_account()).await.unwrap();

    assert!(result.succeeded(), "{result:?}");
    assert!(result.snapshot.unwrap().primary.is_some());
    let requests = transport.requests.lock().unwrap();
    assert!(requests.iter().any(|request| {
        request.url.path().ends_with(CONSOLE_STATUS_PATH)
            && request.headers.get("Authorization").map(String::as_str)
                == Some("Bearer opencode-access-token")
            && !request.headers.contains_key("Cookie")
    }));
}

#[tokio::test]
async fn console_balance_is_required_when_account_has_no_go_subscription() {
    let transport = Arc::new(ConsoleBillingTransport {
        status_body: json!({"access": null}).to_string(),
        billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
        billing_delay: StdDuration::ZERO,
        wait_for_billing_before_status: true,
        billing_started: Notify::new(),
        requests: Mutex::new(Vec::new()),
    });
    let adapter = console_probe_adapter(transport);

    let snapshot = adapter
        .probe(&console_probe_account())
        .await
        .unwrap()
        .snapshot
        .unwrap();

    assert_eq!(snapshot.source.as_deref(), Some("web-console"));
    assert_eq!(
        snapshot
            .credits
            .as_ref()
            .and_then(|credits| credits.balance),
        Some(1.25)
    );
    assert!(snapshot.primary.is_none());
    assert!(snapshot.source_diagnostics.iter().any(|item| {
        item.source == "web.console.status" && item.code == UsageAdapterErrorCode::NoSubscription
    }));
}

#[tokio::test]
async fn console_balance_is_required_when_subscription_usage_fields_are_missing() {
    let transport = Arc::new(ConsoleBillingTransport {
        status_body: json!({"access": {"meters": {"week": {"usagePercent": 35.0}}}}).to_string(),
        billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
        billing_delay: StdDuration::ZERO,
        wait_for_billing_before_status: true,
        billing_started: Notify::new(),
        requests: Mutex::new(Vec::new()),
    });
    let adapter = console_probe_adapter(transport);

    let snapshot = adapter
        .probe(&console_probe_account())
        .await
        .unwrap()
        .snapshot
        .unwrap();

    assert_eq!(snapshot.source.as_deref(), Some("web-console"));
    assert_eq!(
        snapshot
            .credits
            .as_ref()
            .and_then(|credits| credits.balance),
        Some(1.25)
    );
    assert!(snapshot.primary.is_none());
    assert!(snapshot.source_diagnostics.iter().any(|item| {
        item.source == "web.console.status" && item.code == UsageAdapterErrorCode::InvalidPayload
    }));
}

struct RedirectSequenceTransport {
    responses: Mutex<VecDeque<UsageHttpResponse>>,
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[async_trait]
impl UsageHttpTransport for RedirectSequenceTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("test response should be queued"))
    }
}

fn redirect_response(status_code: u16, location: &str) -> UsageHttpResponse {
    UsageHttpResponse {
        status_code,
        body: String::new(),
        headers: [("Location".to_owned(), location.to_owned())].into(),
    }
}

fn success_response() -> UsageHttpResponse {
    UsageHttpResponse {
        status_code: 200,
        body: "ok".to_owned(),
        headers: Default::default(),
    }
}

#[test]
fn api_direct_percent_one_is_one_percent_not_one_hundred() {
    let value = json!({"usagePercent": 1.0, "resetInSec": 60});
    let parsed = parse_window(&value, UsageWindowKind::Primary, "Rolling", true, false).unwrap();
    assert_eq!(parsed.window.used_percent, 1.0);
}

#[test]
fn api_usage_renewal_precedes_root_and_root_remains_a_fallback() {
    let nested_renewal = (Utc::now() + Duration::days(5)).to_rfc3339();
    let root_renewal = (Utc::now() + Duration::days(10)).to_rfc3339();
    let nested_root = json!({
        "renewAt": root_renewal,
        "usage": {
            "renewAt": nested_renewal,
            "rolling": {"usagePercent": 45.0}
        }
    });
    let root_fallback = json!({
        "renewAt": root_renewal,
        "usage": {"rolling": {"usagePercent": 45.0}}
    });
    let account = AccountRecord::create(
        "go",
        "go@example.com",
        None,
        OPENCODE_GO,
        Some("wrk_123".to_owned()),
    )
    .unwrap();
    let renewal_for = |root: &Value| {
        parse_api_snapshot(root, &account)
            .snapshot
            .unwrap()
            .metrics
            .into_iter()
            .find(|metric| metric.key == "subscription-renewal")
            .and_then(|metric| metric.reset_at_utc)
    };

    assert_eq!(
        renewal_for(&nested_root),
        parse_date_value(&json!(nested_renewal), Utc::now())
    );
    assert_eq!(
        renewal_for(&root_fallback),
        parse_date_value(&json!(root_renewal), Utc::now())
    );
}

#[test]
fn redirect_policy_requires_https_and_the_same_origin() {
    let source = Url::parse("https://opencode.ai/console/api/orgs").unwrap();
    assert!(is_allowed_redirect_target(
        &source,
        &Url::parse("https://opencode.ai/console/api/orgs-v2").unwrap()
    ));
    assert!(!is_allowed_redirect_target(
        &source,
        &Url::parse("http://opencode.ai/console/api/orgs").unwrap()
    ));
    assert!(!is_allowed_redirect_target(
        &source,
        &Url::parse("https://opencode.ai:8443/console/api/orgs").unwrap()
    ));
    assert!(!is_allowed_redirect_target(
        &source,
        &Url::parse("https://user@opencode.ai/console/api/orgs").unwrap()
    ));
}

#[tokio::test]
async fn same_origin_https_redirect_is_followed_with_session_headers() {
    let transport = RedirectSequenceTransport {
        responses: Mutex::new(VecDeque::from([
            redirect_response(302, "/console/api/orgs-v2"),
            success_response(),
        ])),
        requests: Mutex::new(Vec::new()),
    };
    let request = UsageHttpRequest {
        method: Method::GET,
        url: Url::parse("https://opencode.ai/console/api/orgs").unwrap(),
        headers: [("Cookie".to_owned(), "session=secret".to_owned())].into(),
        body: None,
    };

    let response = send_with_guarded_redirects(&transport, request)
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(response.status_code, 200);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].url.path(), "/console/api/orgs-v2");
    assert_eq!(requests[1].headers.get("Cookie").unwrap(), "session=secret");
}

#[tokio::test]
async fn cross_origin_redirect_is_not_followed_with_account_credentials() {
    let transport = RedirectSequenceTransport {
        responses: Mutex::new(VecDeque::from([
            redirect_response(302, "https://attacker.example/collect"),
            success_response(),
        ])),
        requests: Mutex::new(Vec::new()),
    };
    let request = UsageHttpRequest {
        method: Method::GET,
        url: Url::parse("https://opencode.ai/console/api/orgs").unwrap(),
        headers: [("Cookie".to_owned(), "session=secret".to_owned())].into(),
        body: None,
    };

    let response = send_with_guarded_redirects(&transport, request)
        .await
        .unwrap();
    assert_eq!(response.status_code, 302);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn post_redirect_rewrites_to_get_and_drops_body_headers() {
    let transport = RedirectSequenceTransport {
        responses: Mutex::new(VecDeque::from([
            redirect_response(303, "/server-v2"),
            success_response(),
        ])),
        requests: Mutex::new(Vec::new()),
    };
    let request = UsageHttpRequest {
        method: Method::POST,
        url: Url::parse("https://opencode.ai/_server").unwrap(),
        headers: [
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Authorization".to_owned(), "Bearer secret".to_owned()),
        ]
        .into(),
        body: Some("[]".to_owned()),
    };

    let response = send_with_guarded_redirects(&transport, request)
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(response.status_code, 200);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, Method::GET);
    assert!(requests[1].body.is_none());
    assert!(
        !requests[1]
            .headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("content-type"))
    );
    assert_eq!(
        requests[1].headers.get("Authorization").unwrap(),
        "Bearer secret"
    );
}

#[tokio::test]
async fn post_307_redirect_preserves_method_and_body() {
    let transport = RedirectSequenceTransport {
        responses: Mutex::new(VecDeque::from([
            redirect_response(307, "/server-preserved"),
            success_response(),
        ])),
        requests: Mutex::new(Vec::new()),
    };
    let request = UsageHttpRequest {
        method: Method::POST,
        url: Url::parse("https://opencode.ai/_server").unwrap(),
        headers: [("Content-Type".to_owned(), "application/json".to_owned())].into(),
        body: Some("[]".to_owned()),
    };

    let response = send_with_guarded_redirects(&transport, request)
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(response.status_code, 200);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, Method::POST);
    assert_eq!(requests[1].body.as_deref(), Some("[]"));
}

#[tokio::test]
async fn redirect_chain_is_bounded() {
    let transport = RedirectSequenceTransport {
        responses: Mutex::new(
            (0..=MAX_OPEN_CODE_REDIRECTS)
                .map(|_| redirect_response(302, "/loop"))
                .collect(),
        ),
        requests: Mutex::new(Vec::new()),
    };
    let request = UsageHttpRequest {
        method: Method::GET,
        url: Url::parse("https://opencode.ai/loop").unwrap(),
        headers: Default::default(),
        body: None,
    };

    let response = send_with_guarded_redirects(&transport, request)
        .await
        .unwrap();
    assert_eq!(response.status_code, 302);
    assert_eq!(
        transport.requests.lock().unwrap().len(),
        MAX_OPEN_CODE_REDIRECTS + 1
    );
}

#[test]
fn dashboard_fraction_is_converted_but_console_micro_cents_are_amounts() {
    let dashboard = json!({"usagePercent": 0.43, "resetInSec": 60});
    let parsed = parse_window(
        &dashboard,
        UsageWindowKind::Primary,
        "Rolling",
        false,
        false,
    )
    .unwrap();
    assert_eq!(parsed.window.used_percent, 43.0);

    let console = json!({"usedMicroCents": 6_000_000.0, "limitMicroCents": 12_000_000.0});
    let parsed = parse_window(&console, UsageWindowKind::Primary, "Rolling", false, true).unwrap();
    assert_eq!(parsed.window.used_percent, 50.0);
    assert_eq!(parsed.used_amount, Some(0.06));
    assert_eq!(parsed.limit_amount, Some(0.12));
}

#[test]
fn relative_reset_parser_accepts_codexbar_field_aliases() {
    for key in ["reset_sec", "resetsInSeconds", "resetIn", "resetSec"] {
        let mut value = serde_json::Map::new();
        value.insert("usagePercent".to_owned(), json!(25.0));
        value.insert(key.to_owned(), json!(3600));

        let parsed = parse_window(
            &Value::Object(value),
            UsageWindowKind::Primary,
            "Rolling",
            true,
            false,
        )
        .unwrap();
        assert!(parsed.window.reset_at_utc.is_some(), "missing {key}");
        assert_eq!(
            parsed.window.limit_window_seconds,
            5 * 60 * 60,
            "the window length must not be a countdown for {key}"
        );
    }
}

#[test]
fn workspace_ids_are_normalized_from_dashboard_urls() {
    assert_eq!(
        normalize_workspace_id("https://opencode.ai/workspace/wrk_123/go"),
        Some("wrk_123".to_owned())
    );
    assert_eq!(
        normalize_workspace_id("org_123"),
        Some("org_123".to_owned())
    );
}

#[test]
fn console_workspace_discovery_ignores_nested_and_unrecognized_ids() {
    let orgs = json!([
        {"id": "user_123", "workspace": {"id": "wrk_nested"}},
        {"id": "org_selected"},
        {"id": "wrk_later"}
    ]);
    assert_eq!(
        select_console_workspace_id(&orgs, None),
        Some("org_selected".to_owned())
    );
}

#[test]
fn console_status_diagnostic_reports_schema_without_response_values() {
    let root = json!({"access": {"meters": {"weekly": {"used": 123}}}});
    let message = console_status_shape_error(&root);
    assert!(message.contains("fiveHour"));
    assert!(message.contains("weekly"));
    assert!(!message.contains("123"));
    assert_eq!(
        console_status_shape_error(&json!({"access": null})),
        "the signed-in account has no active OpenCode Go subscription"
    );
}

#[test]
fn console_status_uses_access_meters() {
    let root = json!({
        "access": {"meters": {
            "fiveHour": {"usedMicroCents": 6_000_000, "limitMicroCents": 12_000_000, "resetInSec": 300},
            "week": {"usedMicroCents": 9_000_000, "limitMicroCents": 30_000_000, "resetInSec": 900}
        }}
    });
    let account = AccountRecord::create(
        "go",
        "go@example.com",
        None,
        OPENCODE_GO,
        Some("wrk_123".to_owned()),
    )
    .unwrap();
    let result = parse_console_snapshot(&root, &account, "wrk_123").unwrap();
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.primary.unwrap().used_percent, 50.0);
    assert_eq!(snapshot.secondary.unwrap().used_percent, 30.0);
}

#[test]
fn console_status_uses_access_end_as_monthly_reset_fallback() {
    let renews_at = (Utc::now() + Duration::days(30)).to_rfc3339();
    let root = json!({
        "access": {
            "endsAt": renews_at,
            "meters": {
                "fiveHour": {"usedMicroCents": 6_000_000, "limitMicroCents": 12_000_000},
                "month": {"usedMicroCents": 30_000_000, "limitMicroCents": 100_000_000}
            }
        }
    });
    let account = AccountRecord::create(
        "go",
        "go@example.com",
        None,
        OPENCODE_GO,
        Some("wrk_123".to_owned()),
    )
    .unwrap();

    let snapshot = parse_console_snapshot(&root, &account, "wrk_123")
        .unwrap()
        .snapshot
        .unwrap();
    let monthly = snapshot
        .additional_windows
        .iter()
        .find(|window| window.key == "monthly")
        .unwrap();
    let expected = parse_date_value(&json!(renews_at), Utc::now()).unwrap();
    assert_eq!(monthly.window.reset_at_utc, Some(expected));
    assert!(monthly.window.limit_window_seconds > 0);
    assert!(snapshot.metrics.iter().any(|metric| {
        metric.key == "subscription-renewal" && metric.reset_at_utc == Some(expected)
    }));
}

#[test]
fn console_status_prefers_month_meter_reset_over_access_end() {
    let month_reset = (Utc::now() + Duration::days(5)).to_rfc3339();
    let renews_at = (Utc::now() + Duration::days(10)).to_rfc3339();
    let root = json!({
        "access": {
            "endsAt": renews_at,
            "meters": {
                "fiveHour": {"usedMicroCents": 6_000_000, "limitMicroCents": 12_000_000},
                "month": {
                    "usedMicroCents": 30_000_000,
                    "limitMicroCents": 100_000_000,
                    "resetsAt": month_reset
                }
            }
        }
    });
    let account = AccountRecord::create(
        "go",
        "go@example.com",
        None,
        OPENCODE_GO,
        Some("wrk_123".to_owned()),
    )
    .unwrap();

    let snapshot = parse_console_snapshot(&root, &account, "wrk_123")
        .unwrap()
        .snapshot
        .unwrap();
    let monthly = snapshot
        .additional_windows
        .iter()
        .find(|window| window.key == "monthly")
        .unwrap();
    assert_eq!(
        monthly.window.reset_at_utc,
        parse_date_value(&json!(month_reset), Utc::now())
    );
}

#[test]
fn legacy_billing_balance_is_scaled_from_provider_units() {
    let response = json!({"customerID": "cus_test", "balance": 125_000_000});
    assert_eq!(find_legacy_billing_balance(&response), Some(1.25));
}

#[test]
fn console_billing_uses_signed_balance_and_not_available_credits() {
    let response = json!({
        "balanceMicroCents": "125000000",
        "availableMicroCents": "999000000"
    });
    assert_eq!(find_console_billing_balance(&response), Some(1.25));

    let negative_balance = json!({"balanceMicroCents": -75_000_000});
    assert_eq!(find_console_billing_balance(&negative_balance), Some(-0.75));

    let absent_balance = json!({
        "balanceMicroCents": null,
        "availableMicroCents": "999000000"
    });
    assert_eq!(find_console_billing_balance(&absent_balance), None);
}

#[test]
fn workspace_preference_only_selects_among_the_accounts_own_workspaces() {
    let orgs = json!([{ "id": "wrk_first" }, { "id": "wrk_second" }]);
    assert_eq!(
        select_console_workspace_id(&orgs, Some("wrk_second")).as_deref(),
        Some("wrk_second")
    );
    assert_eq!(
        select_console_workspace_id(&orgs, Some("wrk_someone_else")).as_deref(),
        Some("wrk_first")
    );
}
