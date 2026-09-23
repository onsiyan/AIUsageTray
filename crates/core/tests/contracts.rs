use async_trait::async_trait;
use codex_usage_core::{
    accounts::{ANTIGRAVITY, AccountRecord, AccountStore, CLAUDE, OPENAI, OPENROUTER},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        AccountBrowserSessionRefresher, AuthError, CookieValue, InMemoryAuthMaterialStore,
        OAuthCallbackListener, OAuthPkcePair, StoredAuthMaterialProvider,
    },
    oauth_loopback::LoopbackOAuthCallbackListener,
    providers::antigravity::AntigravityUsageAdapter,
    providers::claude::{ClaudeSourceMode, ClaudeUsageAdapter},
    providers::openai::WhamUsageAdapter,
    providers::opencode_go::OpenCodeGoUsageAdapter,
    providers::openrouter::OpenRouterUsageAdapter,
    providers::registry::{ProviderRegistry, ProviderRegistryConfig},
    storage::SqliteStore,
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        RateLimitWindow, UsageAdapter, UsagePrimaryWindowKind, UsageSnapshot, UsageSnapshotStore,
        UsageWindowKind,
    },
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration as StdDuration,
};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use url::Url;

#[test]
fn pkce_matches_rfc7636_vector() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        OAuthPkcePair::compute_s256_challenge(verifier),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[tokio::test]
async fn loopback_callback_validates_state_and_returns_code() {
    let redirect_uri = Url::parse("http://localhost:0/oauth-callback").unwrap();
    let mut listener = LoopbackOAuthCallbackListener::new(redirect_uri).unwrap();
    listener.start().await.unwrap();
    let port = listener.redirect_uri().port().unwrap();
    let callback = tokio::spawn(async move {
        listener
            .wait("state-123", chrono::Duration::seconds(5))
            .await
            .unwrap()
    });

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            b"GET /oauth-callback?code=auth-code&state=state-123 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let result = callback.await.unwrap();
    assert_eq!(result.code.as_deref(), Some("auth-code"));
    assert!(result.succeeded());
}

#[tokio::test]
async fn antigravity_adapter_keeps_rpc_colon_in_https_path() {
    let transport = Arc::new(FakeTransport::default());
    let auth = Arc::new(StaticAuth);
    let adapter =
        AntigravityUsageAdapter::new_without_local_probe(transport.clone(), auth).unwrap();
    let account =
        AccountRecord::create("test", "test@example.com", None, ANTIGRAVITY, None).unwrap();

    let result = adapter.probe(&account).await.unwrap();
    assert!(result.succeeded());
    assert_eq!(result.snapshot.unwrap().primary.unwrap().used_percent, 50.0);

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].url.scheme(), "https");
    assert_eq!(requests[0].url.path(), "/v1internal:loadCodeAssist");
    assert_eq!(
        requests[0].url.host_str(),
        Some("daily-cloudcode-pa.sandbox.googleapis.com")
    );
}

#[tokio::test]
async fn antigravity_remote_keeps_project_id_out_of_google_identity() {
    let transport = Arc::new(FakeTransport::default());
    let auth = Arc::new(StaticAuth);
    let adapter = AntigravityUsageAdapter::new_without_local_probe(transport, auth).unwrap();
    let account = AccountRecord::create(
        "test",
        "test@example.com",
        Some("google-subject-1".to_owned()),
        ANTIGRAVITY,
        None,
    )
    .unwrap();

    let result = adapter.probe(&account).await.unwrap();
    let snapshot = result.snapshot.expect("quota snapshot");
    assert!(result.identity.is_none());
    assert_eq!(snapshot.response_account_id.as_deref(), Some("project-1"));
    assert_eq!(snapshot.observed_email, None);
    assert_eq!(
        account.provider_account_id.as_deref(),
        Some("google-subject-1")
    );
}

#[tokio::test]
async fn antigravity_remote_fallback_uses_authoritative_summary() {
    let transport = Arc::new(FallbackTransport::default());
    let auth = Arc::new(StaticAuth);
    let adapter =
        AntigravityUsageAdapter::new_without_local_probe(transport.clone(), auth).unwrap();
    let account =
        AccountRecord::create("test", "test@example.com", None, ANTIGRAVITY, None).unwrap();

    let result = adapter.probe(&account).await.unwrap();
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.primary.as_ref().unwrap().name, "Gemini weekly");
    assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 75.0);
    assert_eq!(
        snapshot
            .metrics
            .iter()
            .find(|metric| metric.name == "Gemini Pro")
            .and_then(|metric| metric.used_percent),
        Some(75.0)
    );

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 9);
    assert_eq!(
        requests[0].url.host_str(),
        Some("daily-cloudcode-pa.sandbox.googleapis.com")
    );
    assert_eq!(
        requests[2].url.host_str(),
        Some("cloudcode-pa.googleapis.com")
    );
    assert_eq!(
        requests[3].url.host_str(),
        Some("daily-cloudcode-pa.sandbox.googleapis.com")
    );
    assert_eq!(
        requests[5].url.host_str(),
        Some("cloudcode-pa.googleapis.com")
    );
    assert_eq!(
        requests[6].url.host_str(),
        Some("daily-cloudcode-pa.sandbox.googleapis.com")
    );
    assert_eq!(
        requests[8].url.host_str(),
        Some("cloudcode-pa.googleapis.com")
    );
}

#[tokio::test]
async fn antigravity_remote_verifies_an_all_full_model_catalog() {
    let transport = Arc::new(VerifiedQuotaTransport::default());
    let auth = Arc::new(StaticAuth);
    let adapter =
        AntigravityUsageAdapter::new_without_local_probe(transport.clone(), auth).unwrap();
    let account =
        AccountRecord::create("test", "test@example.com", None, ANTIGRAVITY, None).unwrap();

    let result = adapter.probe(&account).await.unwrap();
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.source.as_deref(), Some("api-verified-quota"));
    assert_eq!(snapshot.data_confidence, "authoritative");
    assert!((snapshot.primary.unwrap().used_percent - 55.0).abs() < 1e-9);

    let requests = transport.requests.lock().unwrap();
    assert!(requests.iter().any(|request| {
        request.url.path() == "/v1internal:retrieveUserQuota"
            && request.url.host_str() == Some("cloudcode-pa.googleapis.com")
    }));
}

#[tokio::test]
async fn api_adapters_normalize_provider_specific_usage() {
    let transport = Arc::new(FakeTransport::default());
    let auth = Arc::new(StaticAuth);

    let openrouter = OpenRouterUsageAdapter::new(transport.clone(), auth.clone(), true)
        .unwrap()
        .with_activity_scope(Some("workspace-1".to_owned()), true);
    let openrouter_account = AccountRecord::create(
        "openrouter",
        "openrouter@example.com",
        None,
        codex_usage_core::accounts::OPENROUTER,
        None,
    )
    .unwrap();
    let result = openrouter.probe(&openrouter_account).await.unwrap();
    let snapshot = result.snapshot.unwrap();
    assert!((snapshot.primary.unwrap().used_percent - 9.0914810042).abs() < 1e-9);
    assert_eq!(snapshot.plan_type.as_deref(), Some("Paid"));
    assert_eq!(snapshot.credits.unwrap().balance, Some(75.0));
    assert!(snapshot.metrics.iter().any(|metric| {
        metric.key.starts_with("activity.")
            && metric.metadata.get("model").map(String::as_str) == Some("openai/gpt-5")
    }));
    let activity_summary = snapshot
        .metrics
        .iter()
        .find(|metric| metric.key == "activity.summary")
        .unwrap();
    assert_eq!(
        activity_summary
            .metadata
            .get("total_tokens")
            .map(String::as_str),
        Some("175")
    );
    assert!((activity_summary.used_amount.unwrap() - 0.018).abs() < 1e-12);
    assert!(snapshot.source_diagnostics.is_empty());
    let free_requests = snapshot
        .metrics
        .iter()
        .find(|metric| metric.key == "free-model.daily-requests")
        .unwrap();
    assert_eq!(free_requests.unit.as_deref(), Some("requests"));
    assert_eq!(free_requests.remaining_amount, Some(38.0));

    let requests = transport.requests.lock().unwrap();
    let authorization = |path: &str| {
        requests
            .iter()
            .find(|request| request.url.path() == path)
            .and_then(|request| request.headers.get("Authorization"))
            .map(String::as_str)
    };
    assert_eq!(authorization("/api/v1/key"), Some("Bearer test-token"));
    assert_eq!(authorization("/api/v1/credits"), Some("Bearer test-token"));
    assert_eq!(
        authorization("/api/v1/activity"),
        Some("Bearer management-token")
    );
    let activity_request = requests
        .iter()
        .find(|request| request.url.path() == "/api/v1/activity")
        .unwrap();
    assert_eq!(
        activity_request
            .url
            .query_pairs()
            .find(|(name, _)| name == "workspace_id")
            .map(|(_, value)| value),
        Some("workspace-1".into())
    );
    assert_eq!(
        activity_request
            .url
            .query_pairs()
            .find(|(name, _)| name == "group_by")
            .map(|(_, value)| value),
        Some("workspace".into())
    );
    let latest_activity_request = requests
        .iter()
        .find(|request| {
            request.url.path() == "/api/v1/activity"
                && request.url.query_pairs().any(|(name, _)| name == "date")
        })
        .unwrap();
    assert_eq!(
        latest_activity_request
            .url
            .query_pairs()
            .find(|(name, _)| name == "date")
            .map(|(_, value)| value),
        Some(
            (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
                .format("%Y-%m-%d")
                .to_string()
                .into()
        )
    );
    drop(requests);

    let opencode = OpenCodeGoUsageAdapter::new(transport.clone(), auth.clone()).unwrap();
    let opencode_account = AccountRecord::create(
        "opencode",
        "opencode@example.com",
        None,
        codex_usage_core::accounts::OPENCODE_GO,
        None,
    )
    .unwrap();
    let result = opencode.probe(&opencode_account).await.unwrap();
    assert_eq!(result.snapshot.unwrap().primary.unwrap().used_percent, 20.0);

    let claude = ClaudeUsageAdapter::new(transport.clone(), auth.clone(), false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web);
    let claude_account = AccountRecord::create(
        "claude",
        "claude@example.com",
        None,
        codex_usage_core::accounts::CLAUDE,
        None,
    )
    .unwrap();
    let result = claude.probe(&claude_account).await.unwrap();
    assert_eq!(result.snapshot.unwrap().primary.unwrap().used_percent, 30.0);

    let openai = WhamUsageAdapter::new(transport.clone(), Arc::new(CodexOAuthAuth), false, false)
        .unwrap()
        .with_reset_credits(false);
    let openai_account = AccountRecord::create(
        "openai",
        "openai@example.com",
        Some("acct-1".to_owned()),
        codex_usage_core::accounts::OPENAI,
        None,
    )
    .unwrap();
    let result = openai.probe(&openai_account).await.unwrap();
    assert_eq!(result.snapshot.unwrap().primary.unwrap().used_percent, 40.0);
}

#[tokio::test]
async fn openrouter_key_probe_has_a_bounded_deadline() {
    let transport = Arc::new(SlowTransport);
    let auth = Arc::new(StaticAuth);
    let adapter = OpenRouterUsageAdapter::new(transport, auth, false)
        .unwrap()
        .with_deadlines(StdDuration::from_millis(5), StdDuration::from_millis(5));
    let account = AccountRecord::create(
        "openrouter-timeout",
        "openrouter-timeout@example.com",
        None,
        OPENROUTER,
        None,
    )
    .unwrap();

    let error = adapter.probe(&account).await.unwrap_err();
    assert!(matches!(error, TransportError::Timeout(path) if path == "key"));
}

#[tokio::test]
async fn openrouter_optional_failures_preserve_key_data_and_record_diagnostics() {
    let transport = Arc::new(OptionalFailureTransport);
    let auth = Arc::new(StaticAuth);
    let adapter = OpenRouterUsageAdapter::new(transport, auth, true).unwrap();
    let account = AccountRecord::create(
        "openrouter-partial",
        "openrouter-partial@example.com",
        None,
        OPENROUTER,
        None,
    )
    .unwrap();

    let result = adapter.probe(&account).await.unwrap();
    let snapshot = result.snapshot.unwrap();
    assert!(snapshot.primary.is_some());
    assert!(snapshot.credits.is_none());
    assert!(
        snapshot
            .metrics
            .iter()
            .all(|metric| metric.key != "activity.summary")
    );
    assert_eq!(snapshot.source_diagnostics.len(), 2);
    assert!(
        snapshot
            .source_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.source == "credits"
                && diagnostic.code == codex_usage_core::usage::UsageAdapterErrorCode::Forbidden)
    );
    assert!(
        snapshot
            .source_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.source == "activity"
                && diagnostic.code
                    == codex_usage_core::usage::UsageAdapterErrorCode::InvalidPayload)
    );
}

#[tokio::test]
async fn openrouter_credits_survive_a_failed_key_quota_request() {
    let transport = Arc::new(KeyUnavailableCreditsTransport);
    let auth = Arc::new(StaticAuth);
    let adapter = OpenRouterUsageAdapter::new(transport, auth, true)
        .unwrap()
        .with_activity(false);
    let account = AccountRecord::create(
        "openrouter-partial",
        "openrouter-partial@example.com",
        None,
        OPENROUTER,
        None,
    )
    .unwrap();

    let result = adapter.probe(&account).await.unwrap();
    let snapshot = result.snapshot.expect("credits-only usage snapshot");
    assert!(snapshot.primary.is_none());
    assert_eq!(
        snapshot
            .credits
            .as_ref()
            .and_then(|credits| credits.balance),
        Some(75.0)
    );
    assert!(snapshot.source_diagnostics.iter().any(|diagnostic| {
        diagnostic.source == "key"
            && diagnostic.code == codex_usage_core::usage::UsageAdapterErrorCode::TransientHttp
            && diagnostic.http_status_code == Some(503)
    }));
}

#[tokio::test]
async fn claude_oauth_adapter_uses_authoritative_oauth_usage_route() {
    let transport = Arc::new(FakeTransport::default());
    let auth = Arc::new(ClaudeOAuthAuth);
    let adapter = ClaudeUsageAdapter::new(transport.clone(), auth, false).unwrap();
    let account = AccountRecord::create(
        "claude-oauth",
        "claude-oauth@example.com",
        None,
        codex_usage_core::accounts::CLAUDE,
        None,
    )
    .unwrap();

    let result = adapter.probe(&account).await.unwrap();
    let snapshot = result.snapshot.unwrap();
    assert_eq!(snapshot.source.as_deref(), Some("oauth"));
    assert_eq!(snapshot.primary.unwrap().used_percent, 22.0);
    assert_eq!(snapshot.secondary.unwrap().used_percent, 11.0);
    assert_eq!(snapshot.additional_windows.len(), 2);
    assert_eq!(snapshot.additional_windows[0].name, "Daily Routines");
    assert_eq!(snapshot.additional_windows[1].name, "Sonnet");

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.as_str(),
        "https://api.anthropic.com/api/oauth/usage"
    );
    assert_eq!(
        requests[0].headers.get("Authorization").map(String::as_str),
        Some("Bearer sk-ant-oat-test")
    );
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-beta")
            .map(String::as_str),
        Some("oauth-2025-04-20")
    );
}

#[tokio::test]
async fn claude_web_missing_five_hour_keeps_weekly_secondary_and_marks_placeholder() {
    let transport = Arc::new(NoFiveHourTransport::default());
    let auth = Arc::new(StaticAuth);
    let adapter = ClaudeUsageAdapter::new(transport, auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web);
    let account =
        AccountRecord::create("claude-web", "claude-web@example.com", None, CLAUDE, None).unwrap();

    let result = adapter.probe(&account).await.unwrap();
    let snapshot = result.snapshot.unwrap();
    let primary = snapshot.primary.unwrap();
    assert_eq!(primary.used_percent, 0.0);
    assert_eq!(
        snapshot.primary_window_kind,
        Some(UsagePrimaryWindowKind::Session)
    );
    assert!(snapshot.primary_window_is_synthetic);
    assert_eq!(snapshot.secondary.unwrap().used_percent, 41.0);
}

#[tokio::test]
async fn claude_rotated_session_cookie_is_persisted_per_verified_account() {
    let transport = Arc::new(ClaudeRotationTransport::default());
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let account_a = AccountRecord::create(
        "claude-a",
        "a@example.com",
        Some("org-a".to_owned()),
        CLAUDE,
        None,
    )
    .unwrap();
    let account_b = AccountRecord::create(
        "claude-b",
        "b@example.com",
        Some("org-b".to_owned()),
        CLAUDE,
        None,
    )
    .unwrap();
    store
        .save(
            account_a.id,
            &claude_auth_material("sk-ant-initial-a", "a-private-cookie"),
        )
        .await
        .unwrap();
    store
        .save(
            account_b.id,
            &claude_auth_material("sk-ant-initial-b", "b-private-cookie"),
        )
        .await
        .unwrap();

    let auth_store: Arc<dyn AccountAuthMaterialStore> = store.clone();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let registry = ProviderRegistry::from_dependencies_with_auth_store(
        transport.clone(),
        auth,
        auth_store,
        ProviderRegistryConfig {
            claude_source_mode: ClaudeSourceMode::Web,
            fetch_claude_prepaid_credits: false,
            fetch_claude_account_identity: false,
            ..ProviderRegistryConfig::default()
        },
    )
    .unwrap();
    let adapter = registry.get(CLAUDE).unwrap();

    let (result_a, result_b) = tokio::join!(adapter.probe(&account_a), adapter.probe(&account_b));
    let result_a = result_a.unwrap();
    let result_b = result_b.unwrap();
    assert!(result_a.succeeded());
    assert!(result_b.succeeded());
    assert!(result_a.session_token_was_refreshed);
    assert!(result_b.session_token_was_refreshed);

    let material_a = store.get(account_a.id).await.unwrap().unwrap();
    let material_b = store.get(account_b.id).await.unwrap().unwrap();
    assert_eq!(claude_session_cookie(&material_a), Some("sk-ant-renewed-a"));
    assert_eq!(claude_session_cookie(&material_b), Some("sk-ant-renewed-b"));
    assert!(
        material_a
            .cookies
            .iter()
            .any(|cookie| { cookie.name == "extra" && cookie.value == "a-private-cookie" })
    );
    assert!(
        material_b
            .cookies
            .iter()
            .any(|cookie| { cookie.name == "extra" && cookie.value == "b-private-cookie" })
    );
    assert_eq!(
        material_a.bearer_token.as_deref(),
        Some("Bearer sk-ant-oat-test")
    );
    assert_eq!(
        material_b.oauth_access_token.as_deref(),
        Some("claude-preserved-oauth-token")
    );

    let requests = transport.requests.lock().unwrap();
    for request in requests.iter().filter(|request| {
        request.url.path().ends_with("/usage") || request.url.path() == "/api/account"
    }) {
        let cookie = request.headers.get("Cookie").unwrap();
        if request.url.path().contains("org-a") || cookie.contains("renewed-a") {
            assert_eq!(cookie, "sessionKey=sk-ant-renewed-a");
        } else {
            assert_eq!(cookie, "sessionKey=sk-ant-renewed-b");
        }
    }
}

#[tokio::test]
async fn claude_rotated_session_cookie_is_not_saved_for_a_different_identity() {
    let transport = Arc::new(ClaudeRotationTransport {
        wrong_identity: true,
        ..ClaudeRotationTransport::default()
    });
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let account = AccountRecord::create(
        "claude-mismatch",
        "expected@example.com",
        Some("org-a".to_owned()),
        CLAUDE,
        None,
    )
    .unwrap();
    store
        .save(
            account.id,
            &claude_auth_material("sk-ant-initial-a", "private-cookie"),
        )
        .await
        .unwrap();
    let auth_store: Arc<dyn AccountAuthMaterialStore> = store.clone();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let adapter = ClaudeUsageAdapter::new(transport, auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web)
        .with_auth_material_store(auth_store);

    let result = adapter.probe(&account).await.unwrap();
    assert_eq!(
        result.error.as_ref().map(|error| error.code),
        Some(codex_usage_core::usage::UsageAdapterErrorCode::AccountMismatch)
    );
    let material = store.get(account.id).await.unwrap().unwrap();
    assert_eq!(claude_session_cookie(&material), Some("sk-ant-initial-a"));
}

#[tokio::test]
async fn claude_rotated_session_cookie_is_not_saved_without_verified_email() {
    let transport = Arc::new(ClaudeRotationTransport {
        missing_identity: true,
        ..ClaudeRotationTransport::default()
    });
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let account = AccountRecord::create(
        "claude-unverified",
        "a@example.com",
        Some("org-a".to_owned()),
        CLAUDE,
        None,
    )
    .unwrap();
    store
        .save(
            account.id,
            &claude_auth_material("sk-ant-initial-a", "private-cookie"),
        )
        .await
        .unwrap();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let auth_store: Arc<dyn AccountAuthMaterialStore> = store.clone();
    let adapter = ClaudeUsageAdapter::new(transport, auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web)
        .with_auth_material_store(auth_store);

    let result = adapter.probe(&account).await.unwrap();
    assert!(result.succeeded());
    assert!(!result.session_token_was_refreshed);
    assert!(
        result
            .snapshot
            .as_ref()
            .unwrap()
            .source_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.source == "auth.session-key"
                && diagnostic.code == codex_usage_core::usage::UsageAdapterErrorCode::Unknown)
    );
    let material = store.get(account.id).await.unwrap().unwrap();
    assert_eq!(claude_session_cookie(&material), Some("sk-ant-initial-a"));
}

#[tokio::test]
async fn claude_web_session_rotation_is_reported_when_merged_into_oauth_usage() {
    let transport = Arc::new(ClaudeRotationTransport::default());
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let account = AccountRecord::create(
        "claude-oauth-web",
        "a@example.com",
        Some("org-a".to_owned()),
        CLAUDE,
        None,
    )
    .unwrap();
    store
        .save(
            account.id,
            &claude_auth_material("sk-ant-initial-a", "private-cookie"),
        )
        .await
        .unwrap();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let auth_store: Arc<dyn AccountAuthMaterialStore> = store.clone();
    let adapter = ClaudeUsageAdapter::new(transport, auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::OAuth)
        .with_auth_material_store(auth_store);

    let result = adapter.probe(&account).await.unwrap();
    assert!(result.succeeded());
    assert_eq!(
        result.snapshot.as_ref().unwrap().source.as_deref(),
        Some("oauth")
    );
    assert!(result.session_token_was_refreshed);
    let material = store.get(account.id).await.unwrap().unwrap();
    assert_eq!(claude_session_cookie(&material), Some("sk-ant-renewed-a"));
}

#[tokio::test]
async fn claude_401_reimports_and_verifies_the_bound_browser_session_once() {
    let transport = Arc::new(ClaudeSessionRecoveryTransport {
        rotate_browser_session_on_organizations: true,
        ..ClaudeSessionRecoveryTransport::default()
    });
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let refresher = Arc::new(StaticBrowserSessionRefresher::new("sk-ant-browser-a"));
    let account = claude_recovery_account("claude-recovery", "a@example.com", "org-a");
    store
        .save(
            account.id,
            &claude_auth_material("sk-ant-initial-a", "private-cookie"),
        )
        .await
        .unwrap();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let adapter = ClaudeUsageAdapter::new(transport.clone(), auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web)
        .with_auth_material_store(store.clone())
        .with_browser_session_refresher(refresher.clone());

    let result = adapter.probe(&account).await.unwrap();
    assert!(result.succeeded());
    assert!(result.session_token_was_refreshed);
    assert_eq!(
        result
            .snapshot
            .as_ref()
            .unwrap()
            .response_account_id
            .as_deref(),
        Some("org-a")
    );
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        claude_session_cookie(&store.get(account.id).await.unwrap().unwrap()),
        Some("sk-ant-browser-a-rotated")
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request
                .headers
                .get("Cookie")
                .is_some_and(|cookie| { cookie == "sessionKey=sk-ant-initial-a" }))
            .count(),
        1,
        "the expired session must not be retried"
    );
    assert!(requests.iter().all(|request| {
        request
            .headers
            .get("Cookie")
            .is_none_or(|cookie| !cookie.contains("sk-ant-browser-b"))
    }));
    assert!(requests.iter().any(|request| {
        request.url.path() == "/api/account"
            && request
                .headers
                .get("Cookie")
                .is_some_and(|cookie| cookie == "sessionKey=sk-ant-browser-a-rotated")
    }));
    assert!(requests.iter().any(|request| {
        request.url.path().ends_with("/usage")
            && request
                .headers
                .get("Cookie")
                .is_some_and(|cookie| cookie == "sessionKey=sk-ant-browser-a-rotated")
    }));
}

#[tokio::test]
async fn claude_401_does_not_persist_a_reimported_session_for_another_account() {
    let transport = Arc::new(ClaudeSessionRecoveryTransport::default());
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let refresher = Arc::new(StaticBrowserSessionRefresher::new("sk-ant-browser-b"));
    let account = claude_recovery_account("claude-recovery-mismatch", "a@example.com", "org-a");
    store
        .save(
            account.id,
            &claude_auth_material("sk-ant-initial-a", "private-cookie"),
        )
        .await
        .unwrap();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let adapter = ClaudeUsageAdapter::new(transport.clone(), auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web)
        .with_auth_material_store(store.clone())
        .with_browser_session_refresher(refresher.clone());

    let result = adapter.probe(&account).await.unwrap();
    assert_eq!(
        result.error.as_ref().map(|error| error.code),
        Some(codex_usage_core::usage::UsageAdapterErrorCode::AccountMismatch)
    );
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        claude_session_cookie(&store.get(account.id).await.unwrap().unwrap()),
        Some("sk-ant-initial-a")
    );
    assert!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("/usage"))
    );
}

#[tokio::test]
async fn claude_401_retries_usage_only_once_with_the_reimported_session() {
    let transport = Arc::new(ClaudeSessionRecoveryTransport {
        replacement_session_unauthorized: true,
        ..ClaudeSessionRecoveryTransport::default()
    });
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let refresher = Arc::new(StaticBrowserSessionRefresher::new("sk-ant-browser-a"));
    let account = claude_recovery_account("claude-recovery-second-401", "a@example.com", "org-a");
    store
        .save(
            account.id,
            &claude_auth_material("sk-ant-initial-a", "private-cookie"),
        )
        .await
        .unwrap();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let adapter = ClaudeUsageAdapter::new(transport.clone(), auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web)
        .with_auth_material_store(store.clone())
        .with_browser_session_refresher(refresher.clone());

    let result = adapter.probe(&account).await.unwrap();
    assert_eq!(
        result.error.as_ref().map(|error| error.code),
        Some(codex_usage_core::usage::UsageAdapterErrorCode::Unauthorized)
    );
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        claude_session_cookie(&store.get(account.id).await.unwrap().unwrap()),
        Some("sk-ant-browser-a")
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/organizations"
                && request
                    .headers
                    .get("Cookie")
                    .is_some_and(|cookie| { cookie == "sessionKey=sk-ant-browser-a" }))
            .count(),
        2,
        "one identity preflight and one usage retry, with no recursive recovery"
    );
}

#[tokio::test]
async fn claude_cloudflare_challenge_does_not_trigger_browser_session_reimport() {
    let transport = Arc::new(ClaudeSessionRecoveryTransport {
        stale_session_challenge: true,
        ..ClaudeSessionRecoveryTransport::default()
    });
    let store = Arc::new(InMemoryAuthMaterialStore::default());
    let refresher = Arc::new(StaticBrowserSessionRefresher::new("sk-ant-browser-a"));
    let account = claude_recovery_account("claude-recovery-challenge", "a@example.com", "org-a");
    store
        .save(
            account.id,
            &claude_auth_material("sk-ant-initial-a", "private-cookie"),
        )
        .await
        .unwrap();
    let auth: Arc<dyn AccountAuthMaterialProvider> =
        Arc::new(StoredAuthMaterialProvider::new(store.clone()));
    let adapter = ClaudeUsageAdapter::new(transport, auth, false)
        .unwrap()
        .with_source_mode(ClaudeSourceMode::Web)
        .with_auth_material_store(store.clone())
        .with_browser_session_refresher(refresher.clone());

    let result = adapter.probe(&account).await.unwrap();
    assert_eq!(
        result.error.as_ref().map(|error| error.code),
        Some(codex_usage_core::usage::UsageAdapterErrorCode::CloudflareChallenge)
    );
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        claude_session_cookie(&store.get(account.id).await.unwrap().unwrap()),
        Some("sk-ant-initial-a")
    );
}

#[tokio::test]
async fn sqlite_store_round_trips_account_and_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("accounts.db")).unwrap();
    let mut account = AccountRecord::create("Test", "Test@Example.com", None, ANTIGRAVITY, None)
        .unwrap()
        .with_codex_home(Some(r"C:\managed\codex"));
    account.browser_kind = Some("chrome".to_owned());
    account.browser_profile_id = Some("Profile 2".to_owned());
    store.upsert(&account).await.unwrap();
    let loaded = store.get(account.id).await.unwrap().unwrap();
    assert_eq!(loaded.email, "test@example.com");
    assert_eq!(loaded.provider_id, ANTIGRAVITY);
    assert_eq!(loaded.browser_kind.as_deref(), Some("chrome"));
    assert_eq!(loaded.browser_profile_id.as_deref(), Some("Profile 2"));
    assert_eq!(loaded.codex_home.as_deref(), Some(r"C:\managed\codex"));

    let snapshot = UsageSnapshot {
        account_id: account.id,
        observed_at_utc: chrono::Utc::now(),
        response_account_id: Some("project-1".to_owned()),
        plan_type: Some("Antigravity".to_owned()),
        primary: Some(RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: "primary".to_owned(),
            used_percent: 25.0,
            reset_at_utc: None,
            limit_window_seconds: 3600,
        }),
        primary_window_kind: None,
        primary_window_is_synthetic: false,
        secondary: None,
        additional_windows: Vec::new(),
        credits: None,
        credit_inventory: None,
        spend: None,
        observed_email: Some(account.email.clone()),
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics: Vec::new(),
        source_diagnostics: Vec::new(),
        provider_id: ANTIGRAVITY.to_owned(),
        source: Some("api".to_owned()),
        data_confidence: "authoritative".to_owned(),
    };
    store.save(snapshot.clone()).await.unwrap();
    let loaded_snapshot = store.get_latest(account.id).await.unwrap().unwrap();
    assert_eq!(
        loaded_snapshot.response_account_id,
        snapshot.response_account_id
    );
    assert_eq!(loaded_snapshot.primary.unwrap().used_percent, 25.0);
}

#[tokio::test]
async fn sqlite_account_identity_upsert_reuses_only_the_same_provider_and_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("accounts.db")).unwrap();
    let first = AccountRecord::create(
        "Antigravity personal",
        "user@example.com",
        Some("google-subject-1".to_owned()),
        ANTIGRAVITY,
        None,
    )
    .unwrap();
    let saved_first = store
        .upsert_or_get_by_provider_identity(&first)
        .await
        .unwrap();
    assert_eq!(saved_first.id, first.id);

    let duplicate = AccountRecord::create(
        "Antigravity duplicate",
        "user@example.com",
        Some("google-subject-1".to_owned()),
        ANTIGRAVITY,
        None,
    )
    .unwrap();
    let saved_duplicate = store
        .upsert_or_get_by_provider_identity(&duplicate)
        .await
        .unwrap();
    assert_eq!(saved_duplicate.id, first.id);
    assert_eq!(saved_duplicate.label, "Antigravity personal");
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert!(matches!(
        store.upsert(&duplicate).await,
        Err(codex_usage_core::accounts::AccountStoreError::DuplicateProviderIdentity)
    ));

    let second_antigravity_identity = AccountRecord::create(
        "Antigravity second Google account",
        "user@example.com",
        Some("google-subject-2".to_owned()),
        ANTIGRAVITY,
        None,
    )
    .unwrap();
    let saved_second = store
        .upsert_or_get_by_provider_identity(&second_antigravity_identity)
        .await
        .unwrap();
    assert_eq!(saved_second.id, second_antigravity_identity.id);

    let other_provider_same_subject = AccountRecord::create(
        "OpenAI account",
        "user@example.com",
        Some("google-subject-1".to_owned()),
        OPENAI,
        None,
    )
    .unwrap();
    let saved_other_provider = store
        .upsert_or_get_by_provider_identity(&other_provider_same_subject)
        .await
        .unwrap();
    assert_eq!(saved_other_provider.id, other_provider_same_subject.id);

    let openai_workspace_one = AccountRecord::create(
        "OpenAI account in workspace one",
        "user@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("workspace-1".to_owned()),
    )
    .unwrap();
    let saved_openai_workspace_one = store
        .upsert_or_get_by_provider_identity(&openai_workspace_one)
        .await
        .unwrap();
    assert_eq!(saved_openai_workspace_one.id, openai_workspace_one.id);

    let same_user_other_workspace = AccountRecord::create(
        "OpenAI account in another workspace",
        "user@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("workspace-2".to_owned()),
    )
    .unwrap();
    let saved_other_workspace = store
        .upsert_or_get_by_provider_identity(&same_user_other_workspace)
        .await
        .unwrap();
    assert_eq!(saved_other_workspace.id, same_user_other_workspace.id);
    assert_eq!(store.list().await.unwrap().len(), 5);
}

#[tokio::test]
async fn sqlite_open_migrates_legacy_identity_triggers_to_include_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("accounts.db");
    let store = SqliteStore::open(&database_path).unwrap();
    let first_workspace = AccountRecord::create(
        "OpenAI first workspace",
        "user@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("workspace-1".to_owned()),
    )
    .unwrap();
    store.upsert(&first_workspace).await.unwrap();
    drop(store);

    let legacy_connection = rusqlite::Connection::open(&database_path).unwrap();
    legacy_connection
        .execute_batch(
            r#"
            DROP TRIGGER IF EXISTS trg_accounts_provider_identity_unique_insert;
            DROP TRIGGER IF EXISTS trg_accounts_provider_identity_unique_update;

            CREATE TRIGGER trg_accounts_provider_identity_unique_insert
            BEFORE INSERT ON accounts
            WHEN NEW.provider_account_id IS NOT NULL
                AND EXISTS (
                    SELECT 1 FROM accounts
                    WHERE provider_id = NEW.provider_id
                        AND provider_account_id = NEW.provider_account_id
                        AND account_id <> NEW.account_id
                )
            BEGIN
                SELECT RAISE(ABORT, 'duplicate provider identity');
            END;

            CREATE TRIGGER trg_accounts_provider_identity_unique_update
            BEFORE UPDATE OF provider_id, provider_account_id ON accounts
            WHEN NEW.provider_account_id IS NOT NULL
                AND EXISTS (
                    SELECT 1 FROM accounts
                    WHERE provider_id = NEW.provider_id
                        AND provider_account_id = NEW.provider_account_id
                        AND account_id <> NEW.account_id
                )
            BEGIN
                SELECT RAISE(ABORT, 'duplicate provider identity');
            END;
            "#,
        )
        .unwrap();
    drop(legacy_connection);

    let migrated_store = SqliteStore::open(&database_path).unwrap();
    let second_workspace = AccountRecord::create(
        "OpenAI second workspace",
        "user@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("workspace-2".to_owned()),
    )
    .unwrap();
    let saved_second_workspace = migrated_store
        .upsert_or_get_by_provider_identity(&second_workspace)
        .await
        .unwrap();

    assert_eq!(saved_second_workspace.id, second_workspace.id);
    assert_eq!(migrated_store.list().await.unwrap().len(), 2);
}

#[derive(Default)]
struct FakeTransport {
    requests: Mutex<Vec<UsageHttpRequest>>,
}

struct SlowTransport;

#[async_trait]
impl UsageHttpTransport for SlowTransport {
    async fn send(&self, _request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        unreachable!("the OpenRouter adapter must cancel the request before it completes")
    }
}

struct OptionalFailureTransport;

#[async_trait]
impl UsageHttpTransport for OptionalFailureTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let (status_code, body) = match request.url.path() {
            "/api/v1/key" => (
                200,
                r#"{"data":{"limit":100,"limit_remaining":90,"limit_reset":"monthly","usage_monthly":10,"workspace_id":"workspace-1"}}"#,
            ),
            "/api/v1/credits" => (403, r#"{"error":{"code":403}}"#),
            "/api/v1/activity" => {
                let date = (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
                    .format("%Y-%m-%d");
                return Ok(UsageHttpResponse {
                    status_code: 200,
                    body: format!(
                        r#"{{"data":[{{"date":"{date}","model":"openai/gpt-5","usage":0.01,"requests":1,"prompt_tokens":9007199254740992,"completion_tokens":10}}]}}"#
                    ),
                    headers: Default::default(),
                });
            }
            _ => return Err(TransportError::InvalidUrl(request.url.to_string())),
        };
        Ok(UsageHttpResponse {
            status_code,
            body: body.to_owned(),
            headers: Default::default(),
        })
    }
}

struct KeyUnavailableCreditsTransport;

#[async_trait]
impl UsageHttpTransport for KeyUnavailableCreditsTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let (status_code, body) = match request.url.path() {
            "/api/v1/key" => (503, "temporary quota failure"),
            "/api/v1/credits" => (200, r#"{"data":{"total_credits":100,"total_usage":25}}"#),
            _ => return Err(TransportError::InvalidUrl(request.url.to_string())),
        };
        Ok(UsageHttpResponse {
            status_code,
            body: body.to_owned(),
            headers: Default::default(),
        })
    }
}

#[async_trait]
impl UsageHttpTransport for FakeTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        let body = match request.url.path() {
            "/v1internal:loadCodeAssist" => {
                r#"{"cloudaicompanionProject":"project-1","planInfo":{"planType":"PAID"}}"#
            }
            "/v1internal:fetchAvailableModels" => {
                r#"{"models":{"gemini-flash":{"displayName":"Gemini Flash","quotaInfo":{"remainingFraction":0.75,"resetTime":"2030-01-01T00:00:00Z"}},"gemini-pro":{"displayName":"Gemini Pro","quotaInfo":{"remainingFraction":0.50,"resetTime":"2030-01-01T00:00:00Z"}}}}"#
            }
            "/api/v1/key" => {
                r#"{"data":{"usage":433.286754736,"limit":500,"limit_remaining":454.542594979,"limit_reset":"monthly","workspace_id":"workspace-1","usage_daily":3.404645509,"usage_weekly":3.404645509,"usage_monthly":45.457405021,"is_free_tier":false,"is_management_key":false,"free_model_daily_requests":{"limit":50,"remaining":38,"used":12},"include_byok_in_limit":false,"expires_at":"2030-12-31T23:59:59Z"}}"#
            }
            "/api/v1/credits" => r#"{"data":{"total_credits":100,"total_usage":25}}"#,
            "/api/v1/activity" => {
                let date = (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
                    .format("%Y-%m-%d");
                return Ok(UsageHttpResponse {
                    status_code: 200,
                    body: format!(
                        r#"{{"data":[{{"date":"{date}","endpoint_id":"endpoint-1","model":"openai/gpt-5","model_permaslug":"openai/gpt-5-2029-01-01","provider_name":"OpenAI","prompt_tokens":50,"completion_tokens":125,"reasoning_tokens":25,"requests":5,"usage":0.015,"byok_usage_inference":0.003}}]}}"#
                    ),
                    headers: Default::default(),
                });
            }
            "/zen/go/v1/usage" => {
                r#"{"usage":{"rolling":{"usagePercent":20,"resetInSec":3600},"weekly":{"usagePercent":10,"resetInSec":7200},"monthly":{"usagePercent":5,"resetInSec":86400}}}"#
            }
            "/api/organizations" => r#"[{"uuid":"org-1","has_chat_capability":true}]"#,
            "/api/organizations/org-1/usage" => {
                r#"{"five_hour":{"utilization":30,"resets_at":"2030-01-01T00:00:00Z"},"seven_day":{"utilization":15,"resets_at":"2030-01-02T00:00:00Z"}}"#
            }
            "/api/oauth/usage" => {
                r#"{"rate_limit_tier":"pro","five_hour":{"utilization":22,"resets_at":"2030-01-01T00:00:00Z"},"seven_day":{"utilization":11,"resets_at":"2030-01-02T00:00:00Z"},"seven_day_cowork":{"utilization":4,"resets_at":"2030-01-04T00:00:00Z"},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":7,"resets_at":"2030-01-03T00:00:00Z","scope":{"model":{"id":"sonnet","display_name":"Sonnet"}}},{"kind":"weekly_scoped","group":"weekly","percent":99,"scope":{"model":{"id":"all-models","display_name":"All models"}}}]}"#
            }
            "/backend-api/wham/usage" => {
                r#"{"account_id":"acct-1","plan_type":"plus","rate_limit":{"primary_window":{"used_percent":40,"reset_at":"2030-01-01T00:00:00Z","limit_window_seconds":18000},"secondary_window":{"used_percent":10,"reset_at":"2030-01-02T00:00:00Z","limit_window_seconds":604800}}}"#
            }
            _ => return Err(TransportError::InvalidUrl(request.url.to_string())),
        };
        Ok(UsageHttpResponse {
            status_code: 200,
            body: body.to_owned(),
            headers: Default::default(),
        })
    }
}

struct CodexOAuthAuth;

#[async_trait]
impl AccountAuthMaterialProvider for CodexOAuthAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("codex-oauth-token".to_owned()),
            ..AccountAuthMaterial::default()
        }))
    }
}

#[derive(Default)]
struct FallbackTransport {
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[derive(Default)]
struct NoFiveHourTransport;

#[async_trait]
impl UsageHttpTransport for NoFiveHourTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let body = match request.url.path() {
            "/api/organizations" => r#"[{"uuid":"org-1","has_chat_capability":true}]"#,
            "/api/organizations/org-1/usage" => {
                r#"{"five_hour":null,"seven_day":{"utilization":41,"resets_at":"2030-01-02T00:00:00Z"}}"#
            }
            _ => return Err(TransportError::InvalidUrl(request.url.to_string())),
        };
        Ok(UsageHttpResponse {
            status_code: 200,
            body: body.to_owned(),
            headers: Default::default(),
        })
    }
}

fn claude_recovery_account(label: &str, email: &str, organization_id: &str) -> AccountRecord {
    let mut account =
        AccountRecord::create(label, email, Some(organization_id.to_owned()), CLAUDE, None)
            .unwrap();
    account.browser_kind = Some("chrome".to_owned());
    account.browser_profile_id = Some("Default".to_owned());
    account
}

struct StaticBrowserSessionRefresher {
    session_key: String,
    calls: AtomicUsize,
}

impl StaticBrowserSessionRefresher {
    fn new(session_key: &str) -> Self {
        Self {
            session_key: session_key.to_owned(),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AccountBrowserSessionRefresher for StaticBrowserSessionRefresher {
    async fn reimport(
        &self,
        account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        assert_eq!(account.browser_kind.as_deref(), Some("chrome"));
        assert_eq!(account.browser_profile_id.as_deref(), Some("Default"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(AccountAuthMaterial {
            cookies: vec![CookieValue {
                name: "sessionKey".to_owned(),
                value: self.session_key.clone(),
            }],
            ..AccountAuthMaterial::default()
        }))
    }
}

#[derive(Default)]
struct ClaudeSessionRecoveryTransport {
    requests: Mutex<Vec<UsageHttpRequest>>,
    stale_session_challenge: bool,
    replacement_session_unauthorized: bool,
    rotate_browser_session_on_organizations: bool,
}

#[async_trait]
impl UsageHttpTransport for ClaudeSessionRecoveryTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        let session_key = request
            .headers
            .get("Cookie")
            .and_then(|header| header.strip_prefix("sessionKey="))
            .ok_or_else(|| TransportError::InvalidUrl("missing Claude cookie".to_owned()))?;
        let path = request.url.path();
        if path == "/api/organizations" && session_key == "sk-ant-initial-a" {
            if self.stale_session_challenge {
                return Ok(UsageHttpResponse {
                    status_code: 403,
                    body: "Just a moment".to_owned(),
                    headers: [("cf-mitigated".to_owned(), "challenge".to_owned())]
                        .into_iter()
                        .collect(),
                });
            }
            return Ok(UsageHttpResponse {
                status_code: 401,
                body: String::new(),
                headers: Default::default(),
            });
        }
        let replacement_organization_requests = self
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request.url.path() == "/api/organizations"
                    && request
                        .headers
                        .get("Cookie")
                        .is_some_and(|cookie| cookie.starts_with("sessionKey=sk-ant-browser-"))
            })
            .count();
        if path == "/api/organizations"
            && self.replacement_session_unauthorized
            && session_key.starts_with("sk-ant-browser-")
            && replacement_organization_requests > 1
        {
            return Ok(UsageHttpResponse {
                status_code: 401,
                body: String::new(),
                headers: Default::default(),
            });
        }

        let (organization_id, email) = match session_key {
            "sk-ant-browser-a" | "sk-ant-browser-a-rotated" => ("org-a", "a@example.com"),
            "sk-ant-browser-b" => ("org-b", "b@example.com"),
            _ => {
                return Err(TransportError::InvalidUrl(
                    "unexpected Claude recovery session".to_owned(),
                ));
            }
        };
        let body = if path == "/api/organizations" {
            format!(r#"[{{"uuid":"{organization_id}","has_chat_capability":true}}]"#)
        } else if path == "/api/account" {
            format!(
                r#"{{"email_address":"{email}","memberships":[{{"organization":{{"uuid":"{organization_id}","rate_limit_tier":"default_claude_pro"}}}}]}}"#
            )
        } else if path == format!("/api/organizations/{organization_id}/usage") {
            r#"{"five_hour":{"utilization":30,"resets_at":"2030-01-01T00:00:00Z"},"seven_day":{"utilization":15,"resets_at":"2030-01-02T00:00:00Z"}}"#.to_owned()
        } else {
            return Err(TransportError::InvalidUrl(request.url.to_string()));
        };
        let mut headers = std::collections::BTreeMap::new();
        if path == "/api/organizations"
            && self.rotate_browser_session_on_organizations
            && session_key == "sk-ant-browser-a"
        {
            headers.insert(
                "Set-Cookie".to_owned(),
                "sessionKey=sk-ant-browser-a-rotated; Path=/; HttpOnly".to_owned(),
            );
        }
        Ok(UsageHttpResponse {
            status_code: 200,
            body,
            headers,
        })
    }
}

#[derive(Default)]
struct ClaudeRotationTransport {
    requests: Mutex<Vec<UsageHttpRequest>>,
    wrong_identity: bool,
    missing_identity: bool,
}

#[async_trait]
impl UsageHttpTransport for ClaudeRotationTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        if request.url.path() == "/api/oauth/usage" {
            return Ok(UsageHttpResponse {
                status_code: 200,
                body: r#"{"rate_limit_tier":"pro","five_hour":{"utilization":22,"resets_at":"2030-01-01T00:00:00Z"},"seven_day":{"utilization":11,"resets_at":"2030-01-02T00:00:00Z"}}"#.to_owned(),
                headers: Default::default(),
            });
        }
        let session_key = request
            .headers
            .get("Cookie")
            .and_then(|header| {
                header
                    .split(';')
                    .find_map(|pair| pair.trim().split_once('='))
            })
            .filter(|(name, _)| name.eq_ignore_ascii_case("sessionKey"))
            .map(|(_, value)| value)
            .ok_or_else(|| {
                TransportError::InvalidUrl("Claude session cookie missing".to_owned())
            })?;
        let (organization_id, email, renewed_session_key) = match session_key {
            "sk-ant-initial-a" | "sk-ant-renewed-a" => {
                ("org-a", "a@example.com", "sk-ant-renewed-a")
            }
            "sk-ant-initial-b" | "sk-ant-renewed-b" => {
                ("org-b", "b@example.com", "sk-ant-renewed-b")
            }
            _ => {
                return Err(TransportError::InvalidUrl(
                    "unexpected Claude session".to_owned(),
                ));
            }
        };

        let path = request.url.path();
        let mut headers = std::collections::BTreeMap::new();
        let body = if path == "/api/organizations" {
            headers.insert(
                "Set-Cookie".to_owned(),
                format!(
                    "__cf_bm=account-local; Path=/\nsessionKey={renewed_session_key}; Path=/; HttpOnly"
                ),
            );
            format!(r#"[{{"uuid":"{organization_id}","has_chat_capability":true}}]"#)
        } else if path == format!("/api/organizations/{organization_id}/usage") {
            r#"{"five_hour":{"utilization":30,"resets_at":"2030-01-01T00:00:00Z"},"seven_day":{"utilization":15,"resets_at":"2030-01-02T00:00:00Z"}}"#.to_owned()
        } else if path == "/api/account" {
            if self.missing_identity {
                format!(
                    r#"{{"memberships":[{{"organization":{{"uuid":"{organization_id}","rate_limit_tier":"default_claude_pro"}}}}]}}"#
                )
            } else {
                let email = if self.wrong_identity {
                    "different@example.com"
                } else {
                    email
                };
                format!(
                    r#"{{"email_address":"{email}","memberships":[{{"organization":{{"uuid":"{organization_id}","rate_limit_tier":"default_claude_pro"}}}}]}}"#
                )
            }
        } else {
            return Err(TransportError::InvalidUrl(request.url.to_string()));
        };

        Ok(UsageHttpResponse {
            status_code: 200,
            body,
            headers,
        })
    }
}

fn claude_auth_material(session_key: &str, extra_cookie: &str) -> AccountAuthMaterial {
    AccountAuthMaterial {
        bearer_token: Some("Bearer sk-ant-oat-test".to_owned()),
        cookies: vec![
            CookieValue {
                name: "sessionKey".to_owned(),
                value: session_key.to_owned(),
            },
            CookieValue {
                name: "extra".to_owned(),
                value: extra_cookie.to_owned(),
            },
        ],
        oauth_access_token: Some("claude-preserved-oauth-token".to_owned()),
        ..AccountAuthMaterial::default()
    }
}

fn claude_session_cookie(material: &AccountAuthMaterial) -> Option<&str> {
    material
        .cookies
        .iter()
        .find(|cookie| cookie.name.eq_ignore_ascii_case("sessionKey"))
        .map(|cookie| cookie.value.as_str())
}

#[async_trait]
impl UsageHttpTransport for FallbackTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        if request.url.host_str() != Some("cloudcode-pa.googleapis.com") {
            return Ok(UsageHttpResponse {
                status_code: 503,
                body: "temporary endpoint failure".to_owned(),
                headers: Default::default(),
            });
        }
        let body = match request.url.path() {
            "/v1internal:loadCodeAssist" => {
                r#"{"cloudaicompanionProject":"project-1","planInfo":{"planType":"PAID"}}"#
            }
            "/v1internal:fetchAvailableModels" => {
                r#"{"models":{"gemini-pro":{"displayName":"Gemini Pro","quotaInfo":{"remainingFraction":0.99,"resetTime":"2030-01-01T00:00:00Z"}}}}"#
            }
            "/v1internal:retrieveUserQuotaSummary" => {
                r#"{"groups":[{"displayName":"Gemini Models","buckets":[{"bucketId":"gemini-5h","window":"FIVE_HOUR","remainingFraction":0.90,"resetTime":"2030-01-01T05:00:00Z"},{"bucketId":"gemini-weekly","window":"WEEKLY","remainingFraction":0.25,"resetTime":"2030-01-08T00:00:00Z"}]}]}"#
            }
            _ => return Err(TransportError::InvalidUrl(request.url.to_string())),
        };
        Ok(UsageHttpResponse {
            status_code: 200,
            body: body.to_owned(),
            headers: Default::default(),
        })
    }
}

#[derive(Default)]
struct VerifiedQuotaTransport {
    requests: Mutex<Vec<UsageHttpRequest>>,
}

#[async_trait]
impl UsageHttpTransport for VerifiedQuotaTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        if request.url.host_str() != Some("cloudcode-pa.googleapis.com") {
            return Ok(UsageHttpResponse {
                status_code: 503,
                body: "temporary endpoint failure".to_owned(),
                headers: Default::default(),
            });
        }
        let response = match request.url.path() {
            "/v1internal:loadCodeAssist" => UsageHttpResponse {
                status_code: 200,
                body: r#"{"cloudaicompanionProject":"project-1","paidTier":{"id":"g1-pro-tier","name":"Google AI Pro"}}"#.to_owned(),
                headers: Default::default(),
            },
            "/v1internal:fetchAvailableModels" => UsageHttpResponse {
                status_code: 200,
                body: r#"{"models":{"gemini-pro":{"displayName":"Gemini Pro","quotaInfo":{"remainingFraction":1.0,"resetTime":"2030-01-01T00:00:00Z"}},"claude-sonnet":{"displayName":"Claude Sonnet","quotaInfo":{"remainingFraction":1.0,"resetTime":"2030-01-01T00:00:00Z"}}}}"#.to_owned(),
                headers: Default::default(),
            },
            "/v1internal:retrieveUserQuota" => UsageHttpResponse {
                status_code: 200,
                body: r#"{"buckets":[{"modelId":"gemini-pro","remainingFraction":0.45,"resetTime":"2030-01-01T05:00:00Z"},{"modelId":"claude-sonnet","remainingFraction":0.80,"resetTime":"2030-01-01T05:00:00Z"}]}"#.to_owned(),
                headers: Default::default(),
            },
            "/v1internal:retrieveUserQuotaSummary" => UsageHttpResponse {
                status_code: 404,
                body: "not available".to_owned(),
                headers: Default::default(),
            },
            _ => return Err(TransportError::InvalidUrl(request.url.to_string())),
        };
        Ok(response)
    }
}

struct StaticAuth;

#[async_trait]
impl AccountAuthMaterialProvider for StaticAuth {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        if account.provider_id == CLAUDE {
            return Ok(Some(AccountAuthMaterial {
                cookies: vec![CookieValue {
                    name: "sessionKey".to_owned(),
                    value: "sk-ant-sid-test".to_owned(),
                }],
                ..Default::default()
            }));
        }
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("test-token".to_owned()),
            secondary_bearer_token: (account.provider_id == OPENROUTER)
                .then_some("management-token".to_owned()),
            ..Default::default()
        }))
    }
}

struct ClaudeOAuthAuth;

#[async_trait]
impl AccountAuthMaterialProvider for ClaudeOAuthAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("Bearer sk-ant-oat-test".to_owned()),
            ..Default::default()
        }))
    }
}
