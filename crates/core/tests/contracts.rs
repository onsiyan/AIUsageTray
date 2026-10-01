use async_trait::async_trait;
use std::{
    sync::{Arc, Mutex},
    time::Duration as StdDuration,
};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use url::Url;
use usage_monitor_core::{
    accounts::{
        ANTIGRAVITY, AccountId, AccountRecord, AccountStore, CLAUDE, InMemoryAccountStore, OPENAI,
        OPENCODE_GO, OPENROUTER,
    },
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AuthError, CookieValue,
        OAuthCallbackListener, OAuthPkcePair,
    },
    oauth_loopback::LoopbackOAuthCallbackListener,
    providers::antigravity::AntigravityUsageAdapter,
    providers::claude::ClaudeUsageAdapter,
    providers::openai::WhamUsageAdapter,
    providers::opencode_go::{OpenCodeGoSourceMode, OpenCodeGoUsageAdapter},
    providers::openrouter::OpenRouterUsageAdapter,
    storage::SqliteStore,
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{RateLimitWindow, UsageAdapter, UsageSnapshot, UsageSnapshotStore, UsageWindowKind},
};

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
        usage_monitor_core::accounts::OPENROUTER,
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

    // Copy the recorded requests so no lock is held across the probes below.
    let requests = transport.requests.lock().unwrap().clone();
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

    let opencode = OpenCodeGoUsageAdapter::new(transport.clone(), auth.clone())
        .unwrap()
        .with_source_mode(OpenCodeGoSourceMode::Api);
    let opencode_account = AccountRecord::create(
        "opencode",
        "opencode@example.com",
        None,
        usage_monitor_core::accounts::OPENCODE_GO,
        None,
    )
    .unwrap();
    let result = opencode.probe(&opencode_account).await.unwrap();
    assert_eq!(result.snapshot.unwrap().primary.unwrap().used_percent, 20.0);

    let openai = WhamUsageAdapter::new(transport.clone(), Arc::new(CodexOAuthAuth), false, false)
        .unwrap()
        .with_reset_credits(false);
    let openai_account = AccountRecord::create(
        "openai",
        "openai@example.com",
        Some("acct-1".to_owned()),
        usage_monitor_core::accounts::OPENAI,
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
    // A workspace id is shared by several keys and must not become the
    // account's provider identity.
    assert!(
        result
            .identity
            .as_ref()
            .is_some_and(|identity| identity.provider_account_id.is_none())
    );
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
                && diagnostic.code == usage_monitor_core::usage::UsageAdapterErrorCode::Forbidden)
    );
    assert!(
        snapshot
            .source_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.source == "activity"
                && diagnostic.code
                    == usage_monitor_core::usage::UsageAdapterErrorCode::InvalidPayload)
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
            && diagnostic.code == usage_monitor_core::usage::UsageAdapterErrorCode::TransientHttp
            && diagnostic.http_status_code == Some(503)
    }));
}

#[tokio::test]
async fn claude_oauth_adapter_uses_authoritative_oauth_usage_route() {
    let transport = Arc::new(FakeTransport::default());
    let auth = Arc::new(ClaudeOAuthAuth);
    let adapter = ClaudeUsageAdapter::new(transport.clone(), auth).unwrap();
    let account = AccountRecord::create(
        "claude-oauth",
        "claude-oauth@example.com",
        None,
        usage_monitor_core::accounts::CLAUDE,
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
        "https://api.anthropic.com/api/oauth/usage?cedar_ember=1"
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
        Err(usage_monitor_core::accounts::AccountStoreError::DuplicateProviderIdentity)
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
    .unwrap()
    .with_workspace_name(Some("  Team North  "));
    let saved_openai_workspace_one = store
        .upsert_or_get_by_provider_identity(&openai_workspace_one)
        .await
        .unwrap();
    assert_eq!(saved_openai_workspace_one.id, openai_workspace_one.id);
    assert_eq!(
        saved_openai_workspace_one.workspace_name.as_deref(),
        Some("Team North")
    );
    assert_eq!(
        store
            .get(saved_openai_workspace_one.id)
            .await
            .unwrap()
            .unwrap()
            .workspace_name
            .as_deref(),
        Some("Team North")
    );

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
async fn account_alias_is_optional_and_does_not_change_provider_identity() {
    let store = InMemoryAccountStore::default();
    let account = AccountRecord::create(
        "Codex — user@example.com",
        "user@example.com",
        Some("provider-user-1".to_owned()),
        OPENAI,
        Some("workspace-1".to_owned()),
    )
    .unwrap()
    .with_workspace_name(Some("Team North"));
    store.upsert(&account).await.unwrap();

    let renamed = store
        .set_alias(account.id, Some("  Work  "))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.alias.as_deref(), Some("Work"));
    assert_eq!(renamed.display_name(), "Work");
    assert_eq!(renamed.label, "Codex — user@example.com");
    assert_eq!(renamed.email, account.email);
    assert_eq!(renamed.provider_account_id, account.provider_account_id);
    assert_eq!(renamed.workspace_id, account.workspace_id);
    assert_eq!(renamed.workspace_name, account.workspace_name);

    store
        .upsert(&account.with_workspace_name(Some("Team South")))
        .await
        .unwrap();
    let after_metadata_update = store.get(account.id).await.unwrap().unwrap();
    assert_eq!(after_metadata_update.alias.as_deref(), Some("Work"));
    assert_eq!(
        after_metadata_update.workspace_name.as_deref(),
        Some("Team South")
    );

    let cleared = store
        .set_alias(account.id, Some("  "))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.alias, None);
    assert_eq!(cleared.display_name(), "Codex — user@example.com");
    assert!(
        store
            .set_alias(AccountId::new(), Some("Missing account"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn account_references_use_stable_provider_prefixes_and_skip_deleted_numbers() {
    let store = InMemoryAccountStore::default();
    let providers = [
        (OPENAI, "ch1"),
        (CLAUDE, "cc1"),
        (OPENROUTER, "or1"),
        (OPENCODE_GO, "oc1"),
        (ANTIGRAVITY, "ag1"),
    ];
    let mut saved_accounts = Vec::new();

    for (index, (provider_id, expected_reference)) in providers.iter().enumerate() {
        let account = AccountRecord::create(
            format!("Account {index}"),
            format!("user{index}@example.com"),
            None,
            provider_id,
            None,
        )
        .unwrap();
        let saved = store
            .upsert_or_get_by_provider_identity(&account)
            .await
            .unwrap();
        assert_eq!(saved.account_ref.as_deref(), Some(*expected_reference));
        saved_accounts.push(saved);
    }

    let codex_two =
        AccountRecord::create("Codex two", "codex-two@example.com", None, OPENAI, None).unwrap();
    let codex_two = store
        .upsert_or_get_by_provider_identity(&codex_two)
        .await
        .unwrap();
    assert_eq!(codex_two.account_ref.as_deref(), Some("ch2"));

    let renamed = store
        .set_alias(saved_accounts[0].id, Some("Personal"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.account_ref.as_deref(), Some("ch1"));
    store.remove(codex_two.id).await.unwrap();

    let codex_three =
        AccountRecord::create("Codex three", "codex-three@example.com", None, OPENAI, None)
            .unwrap();
    let codex_three = store
        .upsert_or_get_by_provider_identity(&codex_three)
        .await
        .unwrap();
    assert_eq!(codex_three.account_ref.as_deref(), Some("ch3"));
}

#[tokio::test]
async fn sqlite_account_references_survive_updates_reopen_and_identity_deduplication() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("accounts.db");
    let first = AccountRecord::create(
        "Codex one",
        "codex@example.com",
        Some("provider-codex-1".to_owned()),
        OPENAI,
        None,
    )
    .unwrap();
    let second =
        AccountRecord::create("Codex two", "codex-two@example.com", None, OPENAI, None).unwrap();

    let store = SqliteStore::open(&database_path).unwrap();
    store.upsert(&first).await.unwrap();
    store.upsert(&second).await.unwrap();
    assert_eq!(
        store
            .get(first.id)
            .await
            .unwrap()
            .unwrap()
            .account_ref
            .as_deref(),
        Some("ch1")
    );
    assert_eq!(
        store
            .get(second.id)
            .await
            .unwrap()
            .unwrap()
            .account_ref
            .as_deref(),
        Some("ch2")
    );
    drop(store);

    let reopened = SqliteStore::open(&database_path).unwrap();
    let renamed = reopened
        .set_alias(first.id, Some("Main"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.account_ref.as_deref(), Some("ch1"));

    let duplicate = AccountRecord::create(
        "Same Codex identity",
        "codex@example.com",
        Some("provider-codex-1".to_owned()),
        OPENAI,
        None,
    )
    .unwrap();
    let deduplicated = reopened
        .upsert_or_get_by_provider_identity(&duplicate)
        .await
        .unwrap();
    assert_eq!(deduplicated.id, first.id);
    assert_eq!(deduplicated.account_ref.as_deref(), Some("ch1"));

    reopened.remove(second.id).await.unwrap();
    let third = AccountRecord::create("Codex three", "codex-three@example.com", None, OPENAI, None)
        .unwrap();
    reopened.upsert(&third).await.unwrap();
    assert_eq!(
        reopened
            .get(third.id)
            .await
            .unwrap()
            .unwrap()
            .account_ref
            .as_deref(),
        Some("ch3")
    );
}

#[tokio::test]
async fn sqlite_open_backfills_references_for_an_older_accounts_database() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("accounts.db");
    let codex = AccountRecord::create("Codex", "codex@example.com", None, OPENAI, None).unwrap();
    let antigravity =
        AccountRecord::create("Antigravity", "google@example.com", None, ANTIGRAVITY, None)
            .unwrap();
    let store = SqliteStore::open(&database_path).unwrap();
    store.upsert(&codex).await.unwrap();
    store.upsert(&antigravity).await.unwrap();
    drop(store);

    let legacy = rusqlite::Connection::open(&database_path).unwrap();
    legacy
        .execute_batch(
            "DROP INDEX ux_accounts_account_ref; ALTER TABLE accounts DROP COLUMN account_ref; DROP TABLE account_ref_sequences;",
        )
        .unwrap();
    drop(legacy);

    let migrated = SqliteStore::open(&database_path).unwrap();
    assert_eq!(
        migrated
            .get(codex.id)
            .await
            .unwrap()
            .unwrap()
            .account_ref
            .as_deref(),
        Some("ch1")
    );
    assert_eq!(
        migrated
            .get(antigravity.id)
            .await
            .unwrap()
            .unwrap()
            .account_ref
            .as_deref(),
        Some("ag1")
    );
}

#[tokio::test]
async fn sqlite_account_alias_persists_across_reopen_and_can_be_cleared() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("accounts.db");
    let account = AccountRecord::create(
        "Codex — user@example.com",
        "user@example.com",
        Some("provider-user-1".to_owned()),
        OPENAI,
        Some("workspace-1".to_owned()),
    )
    .unwrap()
    .with_workspace_name(Some("Team North"));

    let store = SqliteStore::open(&database_path).unwrap();
    store.upsert(&account).await.unwrap();
    let renamed = store
        .set_alias(account.id, Some("Work"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.display_name(), "Work");
    drop(store);

    let reopened = SqliteStore::open(&database_path).unwrap();
    let loaded = reopened.get(account.id).await.unwrap().unwrap();
    assert_eq!(loaded.alias.as_deref(), Some("Work"));
    assert_eq!(loaded.display_name(), "Work");
    assert_eq!(loaded.label, account.label);
    assert_eq!(loaded.email, account.email);
    assert_eq!(loaded.provider_account_id, account.provider_account_id);
    assert_eq!(loaded.workspace_id, account.workspace_id);
    assert_eq!(loaded.workspace_name, account.workspace_name);

    reopened
        .upsert(&account.with_workspace_name(Some("Team South")))
        .await
        .unwrap();
    let after_metadata_update = reopened.get(account.id).await.unwrap().unwrap();
    assert_eq!(after_metadata_update.alias.as_deref(), Some("Work"));
    assert_eq!(
        after_metadata_update.workspace_name.as_deref(),
        Some("Team South")
    );

    let cleared = reopened.set_alias(account.id, None).await.unwrap().unwrap();
    assert_eq!(cleared.alias, None);
    assert_eq!(cleared.display_name(), "Codex — user@example.com");
}

#[tokio::test]
async fn sqlite_open_adds_alias_to_an_existing_accounts_table() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("accounts.db");
    let store = SqliteStore::open(&database_path).unwrap();
    let account = AccountRecord::create(
        "Codex account",
        "user@example.com",
        Some("provider-user-1".to_owned()),
        OPENAI,
        None,
    )
    .unwrap();
    store.upsert(&account).await.unwrap();
    drop(store);

    let legacy_connection = rusqlite::Connection::open(&database_path).unwrap();
    legacy_connection
        .execute("ALTER TABLE accounts DROP COLUMN alias", [])
        .unwrap();
    drop(legacy_connection);

    let migrated_store = SqliteStore::open(&database_path).unwrap();
    let migrated = migrated_store.get(account.id).await.unwrap().unwrap();
    assert_eq!(migrated.alias, None);
    assert_eq!(migrated.display_name(), "Codex account");
    let renamed = migrated_store
        .set_alias(account.id, Some("Work"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.display_name(), "Work");
}

#[tokio::test]
async fn sqlite_open_adds_workspace_name_to_an_existing_accounts_table() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("accounts.db");
    let store = SqliteStore::open(&database_path).unwrap();
    let account = AccountRecord::create(
        "Codex account",
        "user@example.com",
        Some("chatgpt-user-1".to_owned()),
        OPENAI,
        Some("workspace-1".to_owned()),
    )
    .unwrap();
    store.upsert(&account).await.unwrap();
    drop(store);

    let legacy_connection = rusqlite::Connection::open(&database_path).unwrap();
    legacy_connection
        .execute("ALTER TABLE accounts DROP COLUMN workspace_name", [])
        .unwrap();
    drop(legacy_connection);

    let migrated_store = SqliteStore::open(&database_path).unwrap();
    let migrated = migrated_store.get(account.id).await.unwrap().unwrap();
    assert_eq!(migrated.workspace_id.as_deref(), Some("workspace-1"));
    assert_eq!(migrated.workspace_name, None);

    let named = migrated.with_workspace_name(Some("Team North"));
    migrated_store.upsert(&named).await.unwrap();
    assert_eq!(
        migrated_store
            .get(account.id)
            .await
            .unwrap()
            .unwrap()
            .workspace_name
            .as_deref(),
        Some("Team North")
    );
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
