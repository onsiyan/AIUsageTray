
use super::*;
use crate::{accounts::AccountRecord, transport::UsageHttpRequest, usage::UsageAdapterErrorCode};
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Mutex};

#[test]
fn codex_oauth_definition_uses_openai_endpoints_and_loopback_callback() {
    let provider = oauth_definition();

    assert_eq!(
        provider.authorization_endpoint.as_str(),
        "https://auth.openai.com/oauth/authorize"
    );
    assert_eq!(
        provider.token_endpoint.as_str(),
        "https://auth.openai.com/oauth/token"
    );
    assert_eq!(
        provider.redirect_uri.as_str(),
        "http://localhost:1455/auth/callback"
    );
    assert_eq!(provider.client_id, CODEX_OAUTH_CLIENT_ID);
    assert!(provider.client_secret.is_none());
    assert_eq!(
        provider.scopes,
        [
            "openid",
            "profile",
            "email",
            "offline_access",
            "api.connectors.read",
            "api.connectors.invoke",
        ]
    );
    assert_eq!(
        provider
            .authorization_parameters
            .get("id_token_add_organizations"),
        Some(&"true".to_owned())
    );
    assert_eq!(
        provider
            .authorization_parameters
            .get("codex_cli_simplified_flow"),
        Some(&"true".to_owned())
    );
    assert_eq!(
        provider.authorization_parameters.get("originator"),
        Some(&"codex_usage_monitor_rust".to_owned())
    );
}

#[test]
fn parses_numeric_reset_and_stable_spark_windows() {
    let account = AccountRecord::create(
        "codex",
        "codex@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("acct-1".to_owned()),
    )
    .unwrap();
    let body = r#"{
            "account_id":"acct-1",
            "plan_type":"plus",
            "rate_limit":{"primary_window":{"used_percent":40,"reset_at":4102444800,"limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"GPT-5.3-Codex-Spark","metered_feature":"spark","rate_limit":{"primary_window":{"used_percent":12,"reset_at":4102444800,"limit_window_seconds":18000},"secondary_window":{"used_percent":4,"reset_at":4103049600,"limit_window_seconds":604800}}}]
        }"#;
    let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
    assert_eq!(
        snapshot.primary_window_kind,
        Some(UsagePrimaryWindowKind::Session)
    );
    assert_eq!(snapshot.additional_windows.len(), 2);
    assert_eq!(snapshot.additional_windows[0].key, "codex-spark");
    assert_eq!(snapshot.additional_windows[1].key, "codex-spark-weekly");
    assert_eq!(
        snapshot.primary.unwrap().reset_at_utc.unwrap().timestamp(),
        4102444800
    );
}

#[test]
fn spend_enrichment_does_not_merge_amounts_with_different_currencies() {
    let current = SpendSnapshot {
        monthly_usage: Some(10.0),
        monthly_limit: Some(100.0),
        used_percent: Some(10.0),
        limit_enabled: Some(true),
        currency_code: Some("USD".to_owned()),
    };
    let enrichment = SpendSnapshot {
        monthly_usage: Some(20.0),
        monthly_limit: None,
        used_percent: None,
        limit_enabled: None,
        currency_code: Some("EUR".to_owned()),
    };

    let merged = merge_spend(Some(current), enrichment);
    assert_eq!(merged.monthly_usage, Some(20.0));
    assert_eq!(merged.monthly_limit, None);
    assert_eq!(merged.currency_code.as_deref(), Some("EUR"));
}

#[test]
fn non_spark_extra_prefers_primary_and_uses_a_stable_slug() {
    let account = AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();
    let body = r#"{
            "rate_limit":{"primary_window":{"used_percent":1,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"Model Family / Pro","rate_limit":{"primary_window":{"used_percent":2,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000},"secondary_window":{"used_percent":8,"reset_at":"2030-01-02T00:00:00Z","limit_window_seconds":604800}}}]
        }"#;
    let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
    assert_eq!(snapshot.additional_windows.len(), 1);
    assert_eq!(snapshot.additional_windows[0].key, "codex-model-family-pro");
    assert_eq!(snapshot.additional_windows[0].window.used_percent, 2.0);
}

#[test]
fn reset_credit_inventory_keeps_expiry_and_available_count() {
    let inventory = parse_credit_inventory(
            r#"{"available_count":2,"credits":[{"id":"c1","status":"available","granted_at":"2030-01-01T00:00:00Z","expires_at":4102444800,"title":"Five hour","description":"reset"}]}"#,
        )
        .unwrap();
    assert_eq!(inventory.available_count, 2);
    assert_eq!(inventory.credits[0].id.as_deref(), Some("c1"));
    assert_eq!(
        inventory.credits[0].expires_at_utc.unwrap().timestamp(),
        4102444800
    );
}

#[test]
fn individual_limit_precedence_is_exposed_with_credit_reset() {
    let account = AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();
    let body = r#"{
            "individual_limit":{"limit":100,"used":25,"remaining_percent":75,"reset_at":4102444800},
            "rate_limit":{"primary_window":{"used_percent":10,"reset_at":4102444800,"limit_window_seconds":18000}},
            "credits":{"has_credits":true,"balance":0}
        }"#;
    let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
    let credits = snapshot.credits.unwrap();
    let limit = credits.limit.unwrap();
    assert_eq!(limit.limit, Some(100.0));
    assert_eq!(limit.used, Some(25.0));
    assert_eq!(limit.remaining, Some(75.0));
    assert_eq!(limit.used_percent, Some(25.0));
    assert_eq!(snapshot.data_confidence, "authoritative");
}

#[test]
fn over_quota_primary_window_is_preserved_and_secondary_remains_available() {
    let account = AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();
    let body = r#"{
            "rate_limit":{
                "primary_window":{"used_percent":101,"reset_at":4102444800,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":20,"reset_at":4103049600,"limit_window_seconds":604800}
            }
        }"#;
    let snapshot = parse_wham_usage(&account, body).unwrap().unwrap().unwrap();
    assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 101.0);
    assert_eq!(snapshot.primary.as_ref().unwrap().remaining_percent(), 0.0);
    assert!(snapshot.secondary.is_some());
    assert_eq!(snapshot.data_confidence, "authoritative");
}

#[test]
fn configured_root_uses_codex_usage_fallback_and_backend_base_keeps_wham_path() {
    let root = resolve_base_url(&HashMap::from([(
        "CODEX_CHATGPT_BASE_URL".to_owned(),
        "https://example.test".to_owned(),
    )]))
    .unwrap();
    assert_eq!(root.as_str(), "https://example.test/");
    assert!(!is_backend_api_base(&root));

    let backend = resolve_base_url(&HashMap::from([(
        "CODEX_CHATGPT_BASE_URL".to_owned(),
        "https://example.test/backend-api".to_owned(),
    )]))
    .unwrap();
    assert!(is_backend_api_base(&backend));
}

#[tokio::test]
async fn account_oauth_bearer_queries_wham_without_cookies_or_session_preflight() {
    let transport = Arc::new(CodexUsageTransport::default());
    let auth = Arc::new(StaticOAuthAuth(AccountAuthMaterial {
        bearer_token: Some("account-scoped-oauth-token".to_owned()),
        secondary_bearer_token: Some("unverified-secondary-token".to_owned()),
        oauth_access_token: Some("unverified-oauth-token".to_owned()),
        cookies: vec![crate::auth::CookieValue {
            name: "legacy-session-cookie".to_owned(),
            value: "must-not-be-sent".to_owned(),
        }],
        ..AccountAuthMaterial::default()
    }));
    let adapter = WhamUsageAdapter::new(transport.clone(), auth, false, false)
        .unwrap()
        .with_reset_credits(false);
    let account = AccountRecord::create(
        "codex",
        "codex@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("acct-1".to_owned()),
    )
    .unwrap();

    let result = adapter.probe(&account).await.unwrap();

    assert!(result.succeeded());
    assert_eq!(
        result
            .identity
            .as_ref()
            .and_then(|identity| identity.provider_account_id.as_deref()),
        Some("chatgpt-user-1")
    );
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.source.as_deref(), Some("codex-oauth"));
    assert_eq!(
        snapshot.observed_email.as_deref(),
        Some("codex@example.com")
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/backend-api/wham/usage");
    assert_eq!(
        requests[0].headers.get("Authorization").map(String::as_str),
        Some("Bearer account-scoped-oauth-token")
    );
    assert!(!requests[0].headers.contains_key("Cookie"));
    assert_eq!(
        requests[0]
            .headers
            .get("ChatGPT-Account-Id")
            .map(String::as_str),
        Some("acct-1")
    );
}

#[tokio::test]
async fn codex_independent_usage_enrichment_requests_run_concurrently() {
    let transport = Arc::new(ParallelCodexRequestTransport {
        usage_and_reset_barrier: tokio::sync::Barrier::new(2),
        workspace_enrichment_barrier: tokio::sync::Barrier::new(2),
    });
    let auth = Arc::new(StaticOAuthAuth(oauth_material()));
    let adapter = WhamUsageAdapter::new(transport, auth, true, true)
        .unwrap()
        .with_reset_credits(true);
    let account = AccountRecord::create(
        "codex",
        "codex@example.com",
        None,
        OPENAI,
        Some("workspace-1".to_owned()),
    )
    .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), adapter.probe(&account))
        .await
        .expect("independent Codex usage requests should not wait on each other")
        .unwrap();

    assert!(result.succeeded());
}

#[tokio::test]
async fn optional_endpoint_failures_are_reported_without_failing_codex_usage() {
    let transport = Arc::new(OptionalEndpointFailureTransport {
        requests: Mutex::new(Vec::new()),
    });
    let auth = Arc::new(StaticOAuthAuth(oauth_material()));
    let adapter = WhamUsageAdapter::new(transport.clone(), auth, true, true).unwrap();
    let account = AccountRecord::create(
        "codex",
        "codex@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("acct-1".to_owned()),
    )
    .unwrap();

    let result = adapter.probe(&account).await.unwrap();

    assert!(result.succeeded());
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 40.0);
    assert_eq!(
        snapshot
            .source_diagnostics
            .iter()
            .map(|diagnostic| diagnostic.source.as_str())
            .collect::<Vec<_>>(),
        [
            "wham.reset-credits",
            "workspace.monthly-usage",
            "workspace.remaining-balance",
        ]
    );
    for diagnostic in &snapshot.source_diagnostics {
        assert_eq!(diagnostic.code, UsageAdapterErrorCode::Forbidden);
        assert_eq!(diagnostic.http_status_code, Some(403));
        assert_eq!(diagnostic.retry_after_seconds, Some(17));
        assert!(!diagnostic.message.contains("private response body"));
    }
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let paths = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert!(paths.contains(
        &"/backend-api/accounts/acct-1/spend-controls/current-user/monthly-usage".to_owned()
    ));
    assert!(paths.contains(&"/backend-api/accounts/acct-1/remaining_balance".to_owned()));
}

#[tokio::test]
async fn cookie_only_material_is_rejected_without_a_browser_session_request() {
    let transport = Arc::new(CodexUsageTransport::default());
    let auth = Arc::new(StaticOAuthAuth(AccountAuthMaterial {
        cookies: vec![crate::auth::CookieValue {
            name: "session".to_owned(),
            value: "browser-cookie".to_owned(),
        }],
        ..AccountAuthMaterial::default()
    }));
    let adapter = WhamUsageAdapter::new(transport.clone(), auth, false, false).unwrap();
    let account = AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();

    let result = adapter.probe(&account).await.unwrap();

    assert_eq!(
        result.error.unwrap().code,
        UsageAdapterErrorCode::AuthenticationUnavailable
    );
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[test]
fn wham_account_id_is_validated_against_workspace_not_user_identity() {
    let account = AccountRecord::create(
        "codex",
        "codex@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("workspace-1".to_owned()),
    )
    .unwrap();

    let matching = parse_wham_usage(
            &account,
            r#"{"account_id":"workspace-1","plan_type":"team","rate_limit":{"primary_window":{"used_percent":10,"reset_at":4102444800,"limit_window_seconds":18000}}}"#,
        )
        .unwrap();
    assert!(matches!(matching, Ok(Some(_))));

    let mismatched = parse_wham_usage(
            &account,
            r#"{"account_id":"other-workspace","plan_type":"team","rate_limit":{"primary_window":{"used_percent":10,"reset_at":4102444800,"limit_window_seconds":18000}}}"#,
        )
        .unwrap();
    assert!(matches!(
        mismatched,
        Err(error) if error.code == UsageAdapterErrorCode::AccountMismatch
    ));
}

fn oauth_material() -> AccountAuthMaterial {
    AccountAuthMaterial {
        bearer_token: Some("account-scoped-oauth-token".to_owned()),
        ..AccountAuthMaterial::default()
    }
}

struct StaticOAuthAuth(AccountAuthMaterial);

#[async_trait]
impl AccountAuthMaterialProvider for StaticOAuthAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(self.0.clone()))
    }
}

#[derive(Default)]
struct CodexUsageTransport {
    requests: Mutex<Vec<UsageHttpRequest>>,
}

struct OptionalEndpointFailureTransport {
    requests: Mutex<Vec<UsageHttpRequest>>,
}

struct ParallelCodexRequestTransport {
    usage_and_reset_barrier: tokio::sync::Barrier,
    workspace_enrichment_barrier: tokio::sync::Barrier,
}

#[async_trait]
impl UsageHttpTransport for ParallelCodexRequestTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let path = request.url.path();
        let (status_code, body) = if path.ends_with("/wham/usage") {
            self.usage_and_reset_barrier.wait().await;
            (
                    200,
                    r#"{"account_id":"workspace-1","plan_type":"team","rate_limit":{"primary_window":{"used_percent":40,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}}}"#.to_owned(),
                )
        } else if path.ends_with("/wham/rate-limit-reset-credits") {
            self.usage_and_reset_barrier.wait().await;
            (404, String::new())
        } else {
            self.workspace_enrichment_barrier.wait().await;
            (404, String::new())
        };
        Ok(UsageHttpResponse {
            status_code,
            body,
            headers: BTreeMap::new(),
        })
    }
}

#[async_trait]
impl UsageHttpTransport for OptionalEndpointFailureTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let path = request.url.path().to_owned();
        self.requests.lock().unwrap().push(request);
        let (status_code, body, headers) = match path.as_str() {
                "/backend-api/wham/usage" => (
                    200,
                    r#"{"account_id":"acct-1","plan_type":"team","rate_limit":{"primary_window":{"used_percent":40,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}}}"#.to_owned(),
                    BTreeMap::new(),
                ),
                _ => (
                    403,
                    "private response body".to_owned(),
                    BTreeMap::from([("Retry-After".to_owned(), "17".to_owned())]),
                ),
            };
        Ok(UsageHttpResponse {
            status_code,
            body,
            headers,
        })
    }
}

#[async_trait]
impl UsageHttpTransport for CodexUsageTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let path = request.url.path().to_owned();
        self.requests.lock().unwrap().push(request);
        let (status_code, body) = if path == "/backend-api/wham/usage" {
            (
                    200,
                    r#"{"account_id":"acct-1","plan_type":"plus","rate_limit":{"primary_window":{"used_percent":40,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000}}}"#.to_owned(),
                )
        } else {
            (404, String::new())
        };
        Ok(UsageHttpResponse {
            status_code,
            body,
            headers: BTreeMap::new(),
        })
    }
}
struct HtmlUsageTransport;

#[async_trait]
impl UsageHttpTransport for HtmlUsageTransport {
    async fn send(&self, _request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        Ok(UsageHttpResponse {
            status_code: 200,
            body: "<html>maintenance</html>".to_owned(),
            headers: BTreeMap::new(),
        })
    }
}

#[tokio::test]
async fn non_json_usage_body_is_an_invalid_payload_not_a_network_failure() {
    let auth = Arc::new(StaticOAuthAuth(oauth_material()));
    let adapter = WhamUsageAdapter::new(Arc::new(HtmlUsageTransport), auth, false, false)
        .unwrap()
        .with_reset_credits(false);
    let account = AccountRecord::create("codex", "codex@example.com", None, OPENAI, None).unwrap();

    let result = adapter.probe(&account).await.unwrap();
    assert_eq!(
        result.error.unwrap().code,
        UsageAdapterErrorCode::InvalidPayload
    );
}
