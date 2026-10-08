use super::*;
use std::{collections::VecDeque, sync::Mutex};

struct QuotaSummaryFallbackTransport {
    statuses: Mutex<VecDeque<u16>>,
    requested_hosts: Mutex<Vec<String>>,
    requested_user_agents: Mutex<Vec<String>>,
}

#[async_trait]
impl UsageHttpTransport for QuotaSummaryFallbackTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        self.requested_hosts
            .lock()
            .unwrap()
            .push(request.url.host_str().unwrap().to_owned());
        self.requested_user_agents.lock().unwrap().push(
            request
                .headers
                .get("User-Agent")
                .expect("quota requests must identify their client")
                .clone(),
        );
        let status_code = self.statuses.lock().unwrap().pop_front().unwrap_or(500);
        Ok(UsageHttpResponse {
            status_code,
            body: "{}".to_owned(),
            headers: BTreeMap::new(),
        })
    }
}

struct StaticAntigravityAuth;

#[async_trait]
impl AccountAuthMaterialProvider for StaticAntigravityAuth {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some("test-token".to_owned()),
            ..AccountAuthMaterial::default()
        }))
    }
}

#[tokio::test]
async fn quota_summary_tries_next_host_after_forbidden_response() {
    let transport = Arc::new(QuotaSummaryFallbackTransport {
        statuses: Mutex::new(VecDeque::from([403, 403, 200])),
        requested_hosts: Mutex::new(Vec::new()),
        requested_user_agents: Mutex::new(Vec::new()),
    });
    let adapter =
        AntigravityUsageAdapter::new(transport.clone(), Arc::new(StaticAntigravityAuth)).unwrap();
    // An operation of its own: the answering endpoint is remembered
    // process-wide, and other tests must not see this one's.
    let response = adapter
        .post_remote_best_effort(
            "v1internal:fallbackOrderTest",
            json!({}),
            &AccountAuthMaterial {
                bearer_token: Some("test-token".to_owned()),
                ..AccountAuthMaterial::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(response.status_code, 200);
    assert_eq!(
        *transport.requested_hosts.lock().unwrap(),
        [
            "daily-cloudcode-pa.sandbox.googleapis.com",
            "daily-cloudcode-pa.googleapis.com",
            "cloudcode-pa.googleapis.com",
        ]
    );
    assert_eq!(
        *transport.requested_user_agents.lock().unwrap(),
        [QUOTA_SUMMARY_USER_AGENT; 3]
    );
}

#[tokio::test]
async fn the_endpoint_that_answered_is_tried_first_next_time() {
    let transport = Arc::new(QuotaSummaryFallbackTransport {
        statuses: Mutex::new(VecDeque::from([503, 200, 200])),
        requested_hosts: Mutex::new(Vec::new()),
        requested_user_agents: Mutex::new(Vec::new()),
    });
    let adapter =
        AntigravityUsageAdapter::new(transport.clone(), Arc::new(StaticAntigravityAuth)).unwrap();
    let material = AccountAuthMaterial {
        bearer_token: Some("test-token".to_owned()),
        ..AccountAuthMaterial::default()
    };
    for _ in 0..2 {
        let response = adapter
            .post_remote("v1internal:answeringEndpointTest", json!({}), &material)
            .await
            .unwrap();
        assert_eq!(response.status_code, 200);
    }

    assert_eq!(
        *transport.requested_hosts.lock().unwrap(),
        [
            "daily-cloudcode-pa.sandbox.googleapis.com",
            "daily-cloudcode-pa.googleapis.com",
            "daily-cloudcode-pa.googleapis.com",
        ]
    );
}

#[test]
fn quota_summary_diagnostics_report_status_without_including_response_body() {
    let response = UsageHttpResponse {
        status_code: 403,
        body: "private response content".to_owned(),
        headers: BTreeMap::from([("Retry-After".to_owned(), "17".to_owned())]),
    };

    let diagnostic = quota_summary_response_diagnostic(&response);

    assert_eq!(diagnostic.source, "retrieveUserQuotaSummary");
    assert_eq!(
        diagnostic.code,
        crate::usage::UsageAdapterErrorCode::Forbidden
    );
    assert_eq!(diagnostic.http_status_code, Some(403));
    assert_eq!(diagnostic.retry_after_seconds, Some(17));
    assert!(!diagnostic.message.contains("private response content"));

    let empty = quota_summary_empty_diagnostic(200);
    assert_eq!(empty.code, crate::usage::UsageAdapterErrorCode::Unknown);
    assert_eq!(empty.http_status_code, Some(200));
}

#[test]
fn quota_summary_accepts_shared_and_model_shaped_groups_with_real_quotas() {
    let grouped = parse_quota_summary(&json!({
        "groups": [
            {
                "displayName": "Gemini Models",
                "buckets": [
                    {"bucketId": "gemini-weekly", "remainingFraction": 0.43},
                    {"bucketId": "gemini-5h", "remainingFraction": 0.97}
                ]
            },
            {
                "displayName": "Claude and GPT models",
                "buckets": [
                    {"bucketId": "3p-weekly", "remainingFraction": 0.57},
                    {"bucketId": "3p-5h", "remainingFraction": 1.0}
                ]
            }
        ]
    }));
    assert!(has_usable_quota_summary(&grouped));

    let partial = parse_quota_summary(&json!({
        "groups": [{
            "displayName": "Gemini Models",
            "buckets": [{
                "bucketId": "gemini-5h",
                "remainingFraction": 0.43
            }]
        }]
    }));
    assert!(has_usable_quota_summary(&partial));

    let model_shaped = parse_quota_summary(&json!({
        "groups": [
            {
                "displayName": "Gemini 3.7 Flash",
                "buckets": [
                    {"bucketId": "weekly", "remainingFraction": 0.43},
                    {"bucketId": "5h", "remainingFraction": 0.97}
                ]
            },
            {
                "displayName": "Claude Opus 4.6",
                "buckets": [
                    {"bucketId": "weekly", "remainingFraction": 0.57},
                    {"bucketId": "5h", "remainingFraction": 1.0}
                ]
            }
        ]
    }));
    assert!(has_usable_quota_summary(&model_shaped));

    let invalid = parse_quota_summary(&json!({
        "groups": [{
            "displayName": "Gemini 3.7 Flash",
            "buckets": [{
                "bucketId": "weekly",
                "remainingFraction": 1.2
            }]
        }]
    }));
    assert!(!has_usable_quota_summary(&invalid));
}

#[test]
fn local_quota_summary_preserves_families_and_cadence_windows() {
    let root = json!({
        "response": {
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {
                            "bucketId": "gemini-weekly",
                            "displayName": "Weekly Limit Remaining",
                            "remainingFraction": 0.43,
                            "resetTime": "2030-01-08T00:00:00Z"
                        },
                        {
                            "bucketId": "gemini-5h",
                            "displayName": "Five Hour Limit Remaining",
                            "remainingFraction": 0.97,
                            "resetTime": "2030-01-01T05:00:00Z"
                        }
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {
                            "bucketId": "3p-weekly",
                            "displayName": "Weekly Limit Remaining",
                            "remainingFraction": 0.57
                        },
                        {
                            "bucketId": "3p-5h",
                            "displayName": "Five Hour Limit Remaining",
                            "remainingFraction": 1.0
                        }
                    ]
                }
            ]
        }
    });

    let groups = parse_quota_summary(&root);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].buckets.len(), 2);
    assert!(matches!(
        local_bucket_kind(&groups[0].buckets[0]),
        LocalBucketKind::Weekly
    ));
    assert!(matches!(
        local_bucket_kind(&groups[0].buckets[1]),
        LocalBucketKind::Session
    ));

    let account =
        AccountRecord::create("local", "local@example.com", None, ANTIGRAVITY, None).unwrap();
    let models = vec![to_quota(
        "MODEL_PLACEHOLDER_M1",
        "Gemini Pro".to_owned(),
        Some(0.75),
        None,
    )];
    let snapshot = snapshot_from_quota_summary(
        &account,
        &groups,
        &models,
        Some("local@example.com".to_owned()),
        Some("Google AI Pro".to_owned()),
        "local",
    );

    assert_eq!(snapshot.metrics.len(), 5);
    assert_eq!(snapshot.additional_windows.len(), 4);
    assert_eq!(snapshot.primary.as_ref().unwrap().name, "Gemini weekly");
    assert_eq!(
        snapshot.secondary.as_ref().unwrap().name,
        "Claude/GPT weekly"
    );
    assert!((snapshot.metrics[0].used_percent.unwrap() - 57.0).abs() < 1e-9);
    assert!((snapshot.metrics[1].used_percent.unwrap() - 3.0).abs() < 1e-9);
    assert!((snapshot.metrics[2].used_percent.unwrap() - 43.0).abs() < 1e-9);
    assert!((snapshot.metrics[3].used_percent.unwrap() - 0.0).abs() < 1e-9);
    assert_eq!(snapshot.metrics[4].name, "Gemini Pro");
    assert!((snapshot.metrics[4].used_percent.unwrap() - 57.0).abs() < 1e-9);
    assert_eq!(
        snapshot.additional_windows[0].window.limit_window_seconds,
        604_800
    );
    assert_eq!(
        snapshot.additional_windows[1].window.limit_window_seconds,
        18_000
    );
    assert_eq!(snapshot.source.as_deref(), Some("local"));
}

#[test]
fn summary_bucket_window_uses_absolute_reset_and_fixed_duration() {
    let root = json!({
        "groups": [{
            "displayName": "Gemini Models",
            "buckets": [{
                "bucketId": "gemini-5h",
                "window": "FIVE_HOUR",
                "remainingFraction": 0.8,
                "resetTime": 1_704_067_200_000_i64
            }]
        }]
    });
    let groups = parse_quota_summary(&root);
    let bucket = &groups[0].buckets[0];
    assert_eq!(
        bucket.reset_at_utc,
        Some(DateTime::from_timestamp(1_704_067_200, 0).unwrap())
    );
    let window = local_bucket_window(bucket, UsageWindowKind::Primary, "Gemini 5-hour".to_owned());
    assert_eq!(window.limit_window_seconds, 18_000);
    assert_eq!(
        window.seconds_until_reset(DateTime::from_timestamp(1_704_063_600, 0).unwrap()),
        Some(3_600)
    );
}

#[test]
fn unknown_or_disabled_summary_buckets_keep_reset_context_without_fake_windows() {
    let root = json!({
        "groups": [{
            "displayName": "Gemini Models",
            "buckets": [
                {
                    "bucketId": "gemini-5h-limit",
                    "displayName": "5h limit",
                    "remainingFraction": 0.8,
                    "resetTime": "2030-01-01T05:00:00Z"
                },
                {
                    "bucketId": "gemini-weekly",
                    "displayName": "Weekly Limit",
                    "remainingFraction": 0.25,
                    "disabled": true,
                    "resetTime": "2030-01-08T00:00:00Z"
                },
                {
                    "bucketId": "3p-5h",
                    "displayName": "Five Hour Limit",
                    "resetTime": "2030-01-01T05:00:00Z"
                }
            ]
        }]
    });
    let groups = parse_quota_summary(&root);
    let account =
        AccountRecord::create("local", "local@example.com", None, ANTIGRAVITY, None).unwrap();

    let snapshot = snapshot_from_quota_summary(
        &account,
        &groups,
        &[],
        Some("local@example.com".to_owned()),
        None,
        "local",
    );

    assert_eq!(
        snapshot.primary.as_ref().unwrap().limit_window_seconds,
        18_000
    );
    assert_eq!(snapshot.additional_windows.len(), 1);
    for bucket_id in ["gemini-weekly", "3p-5h"] {
        let metric = snapshot
            .metrics
            .iter()
            .find(|metric| metric.metadata.get("bucket_id").map(String::as_str) == Some(bucket_id))
            .unwrap();
        assert_eq!(metric.used_percent, None);
        assert_eq!(metric.remaining_amount, None);
        assert_eq!(metric.unit, None);
        assert_eq!(
            metric.metadata.get("usage_known").map(String::as_str),
            Some("false")
        );
        assert!(metric.reset_at_utc.is_some());
    }
}

#[test]
fn weekly_zero_bucket_wins_over_five_hour_bucket() {
    let root = json!({
        "groups": [{
            "displayName": "Claude and GPT models",
            "buckets": [
                {"bucketId": "3p-5h", "remainingFraction": 0.01},
                {"bucketId": "3p-weekly", "remainingFraction": 0.0}
            ]
        }]
    });
    let groups = parse_quota_summary(&root);
    let model = to_quota("claude-sonnet", "Claude Sonnet".to_owned(), Some(1.0), None);
    let effective = apply_summary_quota(&model, &groups);
    assert_eq!(effective.remaining_fraction, Some(0.0));
    assert_eq!(effective.used_percent, 100.0);
    assert_eq!(effective.window.limit_window_seconds, 604_800);
}

#[test]
fn subscription_name_prefers_paid_google_tier_over_legacy_plan_info() {
    let root = json!({
        "cloudaicompanionProject": "project-1",
        "currentTier": {"id": "free-tier", "name": "Antigravity Starter Quota"},
        "paidTier": {"id": "g1-ultra-tier", "name": "Google AI Ultra"},
        "planInfo": {"planName": "Pro", "planType": "PAID"}
    });

    assert_eq!(find_plan_type(&root).as_deref(), Some("Google AI Ultra"));
}

#[test]
fn subscription_name_falls_back_to_current_tier_and_allowed_free_tier() {
    let current = json!({
        "currentTier": {"id": "g1-pro-tier", "name": "Google AI Pro"}
    });
    assert_eq!(find_plan_type(&current).as_deref(), Some("Google AI Pro"));

    let free = json!({
        "allowedTiers": [
            {"id": "free-tier", "name": "Antigravity Starter Quota", "isDefault": true}
        ]
    });
    assert_eq!(
        find_plan_type(&free).as_deref(),
        Some("Antigravity Starter Quota")
    );
}

#[test]
fn model_quota_snapshot_uses_constrained_gemini_and_claude_gpt_representatives() {
    let account = AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
    let quotas = vec![
        to_quota(
            "gemini-pro",
            "Gemini Pro".to_owned(),
            Some(0.4),
            Some(
                DateTime::parse_from_rfc3339("2030-01-01T05:00:00Z")
                    .unwrap()
                    .into(),
            ),
        ),
        to_quota(
            "gemini-flash",
            "Gemini Flash".to_owned(),
            Some(0.2),
            Some(
                DateTime::parse_from_rfc3339("2030-01-01T06:00:00Z")
                    .unwrap()
                    .into(),
            ),
        ),
        to_quota("claude-sonnet", "Claude Sonnet".to_owned(), Some(0.6), None),
        to_quota("gpt-oss-120b", "GPT-OSS 120B".to_owned(), Some(0.3), None),
        to_quota("gemini-image", "Gemini Image".to_owned(), Some(0.0), None),
        to_quota(
            "tab_gemini_autocomplete",
            "Gemini autocomplete".to_owned(),
            Some(0.0),
            None,
        ),
    ];

    let snapshot = snapshot_from_model_quotas(
        &account,
        &quotas,
        Some(account.email.clone()),
        None,
        "local-legacy",
        "authoritative",
    );

    let primary = snapshot.primary.as_ref().unwrap();
    assert_eq!(primary.name, "Gemini Flash");
    assert_eq!(primary.used_percent, 80.0);
    assert_eq!(primary.reset_at_utc, quotas[1].window.reset_at_utc);
    let secondary = snapshot.secondary.as_ref().unwrap();
    assert_eq!(secondary.name, "GPT-OSS 120B");
    assert_eq!(secondary.used_percent, 70.0);
    let mut additional_keys = snapshot
        .additional_windows
        .iter()
        .map(|window| window.key.as_str())
        .collect::<Vec<_>>();
    additional_keys.sort_unstable();
    assert_eq!(additional_keys, ["gemini-image", "tab_gemini_autocomplete"]);
    assert_eq!(snapshot.metrics.len(), quotas.len());
}

#[test]
fn retired_gemini_flash_ids_canonicalize_and_keep_best_known_duplicate() {
    let retired_ids = [
        "gemini-3.6-flash",
        "gemini-3.6-flash-low",
        "gemini-3.6-flash-medium",
        "gemini-3.6-flash-high",
        "gemini-3.5-flash-extra-low",
        "gemini-3.5-flash-low",
        "gemini-3.5-flash-mid",
        "gemini-3.5-flash-high",
        "gemini-3-flash-agent",
    ];
    for retired_id in retired_ids {
        assert_eq!(canonical_model_id(retired_id), "gemini-3.7-flash");
    }
    assert_eq!(canonical_model_id(" GEMINI-3.6-FLASH "), "gemini-3.7-flash");
    assert_eq!(canonical_model_id("gemini-3.6-pro"), "gemini-3.6-pro");

    let reset = Some(
        DateTime::parse_from_rfc3339("2030-01-01T05:00:00Z")
            .unwrap()
            .into(),
    );
    let flash_variants = vec![
        to_quota(
            "gemini-3.6-flash",
            "Gemini 3.6 Flash".to_owned(),
            Some(0.5),
            reset,
        ),
        to_quota(
            "gemini-3.5-flash-high",
            "Gemini 3.5 Flash High".to_owned(),
            Some(0.2),
            reset,
        ),
        to_quota(
            "gemini-3.5-flash-extra-low",
            "Gemini 3.5 Flash Extra Low".to_owned(),
            None,
            reset,
        ),
    ];
    let deduplicated = canonicalize_and_deduplicate_model_quotas(&flash_variants);
    assert_eq!(deduplicated.len(), 1);
    assert_eq!(deduplicated[0].key, "gemini-3.7-flash");
    assert_eq!(deduplicated[0].name, "Gemini 3.5 Flash High");
    assert_eq!(deduplicated[0].remaining_fraction, Some(0.2));

    let account = AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
    let quotas = [
        flash_variants,
        vec![to_quota(
            "gpt-oss-120b",
            "GPT-OSS 120B".to_owned(),
            Some(0.3),
            reset,
        )],
    ]
    .concat();
    let snapshot = snapshot_from_model_quotas(
        &account,
        &quotas,
        Some(account.email.clone()),
        None,
        "api-model-catalog",
        "authoritative",
    );
    let primary = snapshot.primary.as_ref().unwrap();
    assert_eq!(primary.name, "Gemini 3.5 Flash High");
    assert_eq!(primary.used_percent, 80.0);
    assert_eq!(snapshot.metrics.len(), quotas.len());
    for retired_id in [
        "gemini-3.6-flash",
        "gemini-3.5-flash-high",
        "gemini-3.5-flash-extra-low",
    ] {
        assert!(
            snapshot
                .metrics
                .iter()
                .any(|metric| metric.key == retired_id)
        );
    }
}

#[test]
fn remote_model_windows_hide_only_exact_pool_mirrors() {
    let account = AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
    let reset = Some(
        DateTime::parse_from_rfc3339("2030-01-01T05:00:00Z")
            .unwrap()
            .into(),
    );
    let quotas = vec![
        to_quota("gemini-flash", "Gemini Flash".to_owned(), Some(0.7), reset),
        to_quota("gpt-oss-120b", "GPT-OSS 120B".to_owned(), Some(0.8), reset),
        to_quota(
            "gemini-3.7-flash-image",
            "Gemini 3.7 Flash Image".to_owned(),
            Some(0.7),
            reset,
        ),
        to_quota(
            "gemini-3.7-flash-image-no-reset",
            "Gemini 3.7 Flash Image No Reset".to_owned(),
            Some(0.7),
            None,
        ),
        to_quota(
            "gemini-3.7-flash-image-different",
            "Gemini 3.7 Flash Image Different".to_owned(),
            Some(0.6),
            reset,
        ),
        to_quota(
            "gemini-3.7-flash-image-reset-only",
            "Gemini 3.7 Flash Image Reset Only".to_owned(),
            None,
            reset,
        ),
        to_quota(
            "gemini-3.7-flash-image-full",
            "Gemini 3.7 Flash Image Full".to_owned(),
            Some(1.0),
            reset,
        ),
        to_quota(
            "claude-sonnet-image",
            "Claude Sonnet Image".to_owned(),
            Some(0.8),
            reset,
        ),
        to_quota(
            "claude-sonnet-image-different",
            "Claude Sonnet Image Different".to_owned(),
            Some(0.6),
            reset,
        ),
        to_quota(
            "tab_gemini_autocomplete",
            "Gemini autocomplete".to_owned(),
            Some(0.7),
            reset,
        ),
    ];

    let local = snapshot_from_model_quotas(
        &account,
        &quotas,
        Some(account.email.clone()),
        None,
        "local-legacy",
        "authoritative",
    );
    let remote = snapshot_from_model_quotas(
        &account,
        &quotas,
        Some(account.email.clone()),
        None,
        "api-model-catalog",
        "authoritative",
    );

    let mut local_keys = local
        .additional_windows
        .iter()
        .map(|window| window.key.as_str())
        .collect::<Vec<_>>();
    local_keys.sort_unstable();
    assert_eq!(
        local_keys,
        [
            "claude-sonnet-image",
            "claude-sonnet-image-different",
            "gemini-3.7-flash-image",
            "gemini-3.7-flash-image-different",
            "gemini-3.7-flash-image-no-reset",
            "gemini-3.7-flash-image-reset-only",
            "tab_gemini_autocomplete",
        ]
    );

    let mut remote_keys = remote
        .additional_windows
        .iter()
        .map(|window| window.key.as_str())
        .collect::<Vec<_>>();
    remote_keys.sort_unstable();
    assert_eq!(
        remote_keys,
        [
            "claude-sonnet-image-different",
            "gemini-3.7-flash-image-different",
            "gemini-3.7-flash-image-no-reset",
            "gemini-3.7-flash-image-reset-only",
        ]
    );
    assert_eq!(local.metrics.len(), quotas.len());
    assert_eq!(remote.metrics.len(), quotas.len());
}

#[test]
fn unknown_model_fallback_is_local_only() {
    let account = AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
    let quotas = vec![to_quota(
        "experimental-text-model",
        "Experimental text model".to_owned(),
        Some(0.7),
        None,
    )];

    let local = snapshot_from_model_quotas(
        &account,
        &quotas,
        Some(account.email.clone()),
        None,
        "local-legacy",
        "authoritative",
    );
    let remote = snapshot_from_model_quotas(
        &account,
        &quotas,
        Some(account.email.clone()),
        None,
        "api-model-catalog",
        "degraded",
    );

    assert_eq!(
        local.primary.as_ref().map(|window| window.name.as_str()),
        Some("Experimental text model")
    );
    assert!(remote.primary.is_none());
    assert_eq!(remote.metrics.len(), 1);
}

#[test]
fn remote_quota_buckets_are_deduplicated_by_lowest_remaining_fraction() {
    let root = json!({
        "buckets": [
            {"modelId": "gemini-pro", "remainingFraction": 0.9},
            {"modelId": "gemini-pro", "remainingFraction": 0.4},
            {"modelId": "claude-sonnet", "remainingFraction": 0.8}
        ]
    });
    let quotas = parse_remote_quota_buckets(&root);
    assert_eq!(quotas.len(), 2);
    assert_eq!(
        quotas
            .iter()
            .find(|quota| quota.key == "gemini-pro")
            .and_then(|quota| quota.remaining_fraction),
        Some(0.4)
    );
    assert!(should_verify_remote_quotas(&[
        to_quota("one", "One".to_owned(), Some(1.0), None),
        to_quota("two", "Two".to_owned(), Some(0.999), None),
    ]));
}

#[test]
fn onboarding_prefers_default_allowed_tier() {
    let root = json!({
        "allowedTiers": [
            {"id": "paid-tier", "isDefault": false},
            {"id": "free-tier", "isDefault": true}
        ],
        "paidTier": {"id": "paid-tier"}
    });
    assert_eq!(find_onboard_tier(&root).as_deref(), Some("free-tier"));
}
