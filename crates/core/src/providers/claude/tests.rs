use super::*;

#[test]
fn oauth_weekly_lane_is_selected_when_five_hour_is_missing() {
    let root: Value = serde_json::json!({
        "five_hour": null,
        "seven_day": {
            "utilization": 41,
            "resets_at": "2030-01-02T00:00:00Z"
        },
        "seven_day_cowork": {
            "utilization": 9,
            "resets_at": "2030-01-03T00:00:00Z"
        },
        "limits": [{
            "kind": "weekly_scoped",
            "group": "weekly",
            "percent": 12,
            "scope": {
                "model": {
                    "id": "fable",
                    "display_name": "Fable"
                }
            }
        }]
    });
    let now = Utc::now();
    let five_hour = parse_window(
        root.get("five_hour"),
        UsageWindowKind::Primary,
        "Session",
        now,
    );
    let weekly = parse_window(
        root.get("seven_day"),
        UsageWindowKind::Secondary,
        "Weekly",
        now,
    );
    let primary = five_hour.clone().or_else(|| weekly.clone());
    let secondary = weekly.clone();
    assert_eq!(primary.unwrap().used_percent, 41.0);
    assert_eq!(
        primary_window_kind(secondary.as_ref()),
        Some(UsagePrimaryWindowKind::Weekly)
    );
    let secondary = secondary.unwrap();
    assert_eq!(secondary.used_percent, 41.0);
    assert_eq!(secondary.limit_window_seconds, 7 * 24 * 60 * 60);

    let additional = parse_claude_extra_windows(&root, now);
    assert_eq!(additional.len(), 2);
    assert_eq!(additional[0].name, "Daily Routines");
    assert_eq!(additional[1].key, "claude-weekly-scoped-fable");
}

#[test]
fn web_missing_five_hour_uses_a_synthetic_session_lane() {
    let primary = synthetic_session_window();
    assert_eq!(primary.name, "Session");
    assert_eq!(primary.used_percent, 0.0);
    assert_eq!(primary.reset_at_utc, None);
    assert_eq!(
        primary_window_kind(Some(&primary)),
        Some(UsagePrimaryWindowKind::Session)
    );
}

#[test]
fn spend_limit_becomes_primary_only_when_no_usage_lane_exists() {
    let root = serde_json::json!({
        "extra_usage": {
            "is_enabled": true,
            "used_credits": 250,
            "monthly_limit": 1000,
            "utilization": 25
        }
    });
    let (spend, _) = parse_extra_usage(&root);
    let primary = spend.as_ref().and_then(spend_limit_window).unwrap();
    assert_eq!(primary.name, "Spend limit");
    assert_eq!(primary.used_percent, 25.0);
    assert_eq!(spend.unwrap().monthly_usage, Some(2.5));
}

#[test]
fn extra_usage_rejects_disabled_or_invalid_values_and_preserves_currency() {
    let root = serde_json::json!({
        "extra_usage": {
            "is_enabled": true,
            "used_credits": 1250,
            "monthly_limit": 5000,
            "currency": " usd ",
            "utilization": 25
        }
    });
    let (spend, _) = parse_extra_usage(&root);
    let spend = spend.unwrap();
    assert_eq!(spend.monthly_usage, Some(12.5));
    assert_eq!(spend.monthly_limit, Some(50.0));
    assert_eq!(spend.currency_code.as_deref(), Some("USD"));

    let disabled = serde_json::json!({
        "extra_usage": { "is_enabled": false, "used_credits": 1, "monthly_limit": 100 }
    });
    assert!(parse_extra_usage(&disabled).0.is_none());

    let invalid = serde_json::json!({
        "extra_usage": { "used_credits": "NaN", "monthly_limit": 100 }
    });
    assert!(parse_extra_usage(&invalid).0.is_none());

    let negative = serde_json::json!({
        "extra_usage": { "used_credits": -1, "monthly_limit": 100 }
    });
    assert!(parse_extra_usage(&negative).0.is_none());
}

#[test]
fn overage_spend_requires_enabled_state_currency_and_finite_amounts() {
    let valid =
        r#"{"is_enabled":true,"used_credits":125,"monthly_credit_limit":1000,"currency":"USD"}"#;
    let spend = parse_overage_spend(valid).unwrap();
    assert_eq!(spend.monthly_usage, Some(1.25));
    assert_eq!(spend.monthly_limit, Some(10.0));
    assert_eq!(spend.currency_code.as_deref(), Some("USD"));

    assert!(parse_overage_spend(
            r#"{"is_enabled":false,"used_credits":125,"monthly_credit_limit":1000,"currency":"USD"}"#
        )
        .is_none());
    assert!(
        parse_overage_spend(
            r#"{"is_enabled":true,"used_credits":125,"monthly_credit_limit":1000}"#
        )
        .is_none()
    );
    assert!(parse_overage_spend(
            r#"{"is_enabled":true,"used_credits":"NaN","monthly_credit_limit":1000,"currency":"USD"}"#
        )
        .is_none());
}

#[test]
fn prepaid_credits_requires_a_valid_amount_and_currency() {
    let credits = parse_prepaid_credits(r#"{"amount":0,"currency":"usd"}"#).unwrap();
    assert_eq!(credits.balance, Some(0.0));
    assert_eq!(credits.currency_code.as_deref(), Some("USD"));
    assert_eq!(credits.credits_available, Some(false));

    assert!(parse_prepaid_credits(r#"{"amount":100}"#).is_none());
    assert!(parse_prepaid_credits(r#"{"amount":-1,"currency":"USD"}"#).is_none());
    assert!(parse_prepaid_credits(r#"{"amount":"Infinity","currency":"USD"}"#).is_none());
}

#[test]
fn organization_selection_prefers_chat_capability_arrays() {
    let organizations = r#"[
            {"uuid":"api-org","capabilities":["api"]},
            {"uuid":"chat-org","capabilities":["chat"]}
        ]"#;
    assert_eq!(
        select_organization(organizations, None).as_deref(),
        Some("chat-org")
    );
}

#[test]
fn organization_selection_honors_bound_org_and_supports_legacy_flags() {
    let organizations = r#"[
            {"uuid":"api-org","capabilities":["api"]},
            {"uuid":"legacy-chat-org","has_chat_capability":true,"is_api_only":false}
        ]"#;
    assert_eq!(
        select_organization(organizations, Some("api-org")).as_deref(),
        Some("api-org")
    );
    assert_eq!(
        select_organization(organizations, None).as_deref(),
        Some("legacy-chat-org")
    );
}

#[test]
fn organization_selection_does_not_fallback_when_bound_org_is_missing() {
    let organizations = r#"[
            {"uuid":"another-chat-org","capabilities":["chat"]}
        ]"#;
    assert_eq!(
        select_organization(organizations, Some("missing-org")),
        None
    );
    assert_eq!(
        select_organization(organizations, Some(" another-chat-org ")).as_deref(),
        Some("another-chat-org")
    );
}

#[test]
fn claude_profile_accepts_nested_account_and_organization_shape() {
    let profile = parse_claude_profile(
            r#"{"account":{"uuid":"acct-1","email_address":"user@example.com"},"organization":{"uuid":"org-1"}}"#,
        )
        .unwrap();
    assert_eq!(profile.account_id.as_deref(), Some("acct-1"));
    assert_eq!(profile.organization_id.as_deref(), Some("org-1"));
    assert_eq!(profile.email.as_deref(), Some("user@example.com"));
}

#[test]
fn admin_usage_parser_normalizes_cost_and_model_tokens() {
    let costs = parse_admin_usage(
            r#"{"data":[{"starting_at":"2030-01-01T00:00:00Z","ending_at":"2030-01-02T00:00:00Z","results":[{"amount":"1250","description":"Claude"}]}]}"#,
        )
        .unwrap();
    assert_eq!(costs.cost_usd, Some(12.5));

    let messages = parse_admin_usage(
            r#"{"data":[{"starting_at":"2030-01-01T00:00:00Z","ending_at":"2030-01-02T00:00:00Z","results":[{"uncached_input_tokens":10,"cache_creation":{"ephemeral_5m_input_tokens":2},"cache_read_input_tokens":3,"output_tokens":5,"model":"claude-sonnet"}]}]}"#,
        )
        .unwrap();
    assert_eq!(messages.total_tokens, 20);
    assert_eq!(messages.models[0].name, "claude-sonnet");
}

#[test]
fn web_session_key_requires_a_claude_session_cookie() {
    let invalid = crate::auth::AccountAuthMaterial {
        bearer_token: Some("sk-ant-api-invalid".to_owned()),
        cookies: vec![crate::auth::CookieValue {
            name: "sessionKey".to_owned(),
            value: "not-a-session".to_owned(),
        }],
        ..Default::default()
    };
    assert!(claude_session_key(&invalid).is_none());

    let valid = crate::auth::AccountAuthMaterial {
        cookies: vec![crate::auth::CookieValue {
            name: "sessionKey".to_owned(),
            value: "sk-ant-sid-test".to_owned(),
        }],
        ..Default::default()
    };
    assert_eq!(
        claude_session_key(&valid).as_deref(),
        Some("sk-ant-sid-test")
    );
}

#[test]
fn rotated_web_session_key_requires_success_and_a_valid_cookie() {
    let response = |status_code, set_cookie: &str| crate::transport::UsageHttpResponse {
        status_code,
        body: String::new(),
        headers: [("Set-Cookie".to_owned(), set_cookie.to_owned())]
            .into_iter()
            .collect(),
    };

    let renewed = response(
        200,
        "__cf_bm=cloudflare; Expires=Wed, 21 Oct 2030 07:28:00 GMT\nsessionKey=sk-ant-sid-renewed; Path=/; HttpOnly",
    );
    assert_eq!(
        rotated_claude_session_key(&renewed).as_deref(),
        Some("sk-ant-sid-renewed")
    );
    assert!(rotated_claude_session_key(&response(401, "sessionKey=sk-ant-sid-new")).is_none());
    assert!(rotated_claude_session_key(&response(200, "sessionKey=not-a-claude-cookie")).is_none());
}

#[test]
fn plan_inference_preserves_max_usage_multiplier() {
    assert_eq!(
        claude_plan_label(None, Some("default_claude_max_5x"), None, None).as_deref(),
        Some("Claude Max 5x")
    );
}

#[test]
fn cloudflare_challenge_is_distinct_from_a_rejected_claude_session() {
    let challenge = crate::transport::UsageHttpResponse {
        status_code: 403,
        body: "Just a moment...".to_owned(),
        headers: std::collections::BTreeMap::new(),
    };
    let error = map_claude_http_error(&challenge, "Claude").error.unwrap();
    assert_eq!(
        error.code,
        crate::usage::UsageAdapterErrorCode::CloudflareChallenge
    );

    let unauthorized = crate::transport::UsageHttpResponse {
        status_code: 401,
        body: String::new(),
        headers: std::collections::BTreeMap::new(),
    };
    let error = map_claude_http_error(&unauthorized, "Claude")
        .error
        .unwrap();
    assert_eq!(
        error.code,
        crate::usage::UsageAdapterErrorCode::Unauthorized
    );
}
#[test]
fn automatic_cli_fallback_requires_the_same_signed_in_account() {
    let account = AccountRecord::create("Claude", "me@example.com", None, CLAUDE, None).unwrap();
    let automatic = ClaudeSourceMode::Automatic;
    assert!(unverified_cli_fallback(automatic, &account, None).is_some());
    assert!(unverified_cli_fallback(automatic, &account, Some("other@example.com")).is_some());
    assert!(unverified_cli_fallback(automatic, &account, Some("ME@example.com")).is_none());
    assert!(unverified_cli_fallback(ClaudeSourceMode::Cli, &account, None).is_none());
}

#[test]
fn oauth_profile_names_the_subscription_plan() {
    let pro = parse_claude_profile(
            r#"{"account":{"email":"a@example.com","has_claude_pro":true,"has_claude_max":false},
                "organization":{"organization_type":"claude_pro","rate_limit_tier":"default_claude_ai","billing_type":"stripe_subscription","seat_tier":null}}"#,
        )
        .unwrap();
    assert_eq!(pro.plan_type.as_deref(), Some("Claude Pro"));

    let max = parse_claude_profile(
            r#"{"account":{"has_claude_max":true},
                "organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x"}}"#,
        )
        .unwrap();
    assert_eq!(max.plan_type.as_deref(), Some("Claude Max 20x"));

    let team = parse_claude_profile(
        r#"{"organization":{"organization_type":"claude_team","seat_tier":"team_standard"}}"#,
    )
    .unwrap();
    assert_eq!(team.plan_type.as_deref(), Some("Claude Team Standard"));

    let flags_only = parse_claude_profile(r#"{"account":{"has_claude_pro":true}}"#).unwrap();
    assert_eq!(flags_only.plan_type.as_deref(), Some("Claude Pro"));

    let unknown = parse_claude_profile(r#"{"account":{"email":"a@example.com"}}"#).unwrap();
    assert_eq!(unknown.plan_type, None);
}

#[test]
fn reset_grants_become_reset_credits() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-30T06:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let root: Value = serde_json::from_str(
            r#"{"cedar_ember":{"eligible":true,"grants":[
                {"id":"launch","label":"Launch reset","resets_total":1,"resets_left":1,
                 "starts_at":"2026-09-22T19:00:00+03:00","ends_at":"2026-10-22T19:00:00+03:00",
                 "clears":["five_hour","seven_day"],"paused":false},
                {"id":"session","resets_left":2,"ends_at":"2026-10-01T00:00:00Z",
                 "clears":["five_hour"],"paused":true},
                {"id":"used","resets_left":0,"clears":["five_hour"]},
                {"id":"expired","resets_left":1,"ends_at":"2026-09-01T00:00:00Z","clears":["seven_day"]}
            ]}}"#,
        )
        .unwrap();

    let inventory = parse_claude_reset_grants(&root, now).unwrap();

    assert_eq!(inventory.available_count, 1);
    assert_eq!(inventory.credits.len(), 3);
    let full = &inventory.credits[0];
    assert_eq!(full.title.as_deref(), Some("Full reset"));
    assert_eq!(full.status.as_deref(), Some("available"));
    assert_eq!(full.description.as_deref(), Some("Launch reset"));
    assert_eq!(
        full.expires_at_utc.unwrap().to_rfc3339(),
        "2026-10-22T16:00:00+00:00"
    );
    assert!(inventory.credits[1..].iter().all(|credit| {
        credit.title.as_deref() == Some("5-hour reset")
            && credit.status.as_deref() == Some("paused")
    }));

    let ineligible: Value =
        serde_json::from_str(r#"{"cedar_ember":{"eligible":false,"grants":[]}}"#).unwrap();
    let empty = parse_claude_reset_grants(&ineligible, now).unwrap();
    assert_eq!(empty.available_count, 0);
    let absent: Value = serde_json::from_str(r#"{"cedar_ember":null}"#).unwrap();
    assert!(parse_claude_reset_grants(&absent, now).is_none());
}
