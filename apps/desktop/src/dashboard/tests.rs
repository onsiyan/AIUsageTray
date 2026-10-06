use super::*;
use chrono::{Duration, TimeZone};
use usage_monitor_core::usage::{RateLimitWindow, UsageWindowKind};

fn model_metric(model_id: &str, name: &str, remaining: f64, reset_in_hours: i64) -> UsageMetric {
    UsageMetric {
        key: model_id.to_owned(),
        name: name.to_owned(),
        used_percent: Some(100.0 - remaining),
        used_amount: Some(100.0 - remaining),
        limit_amount: Some(100.0),
        remaining_amount: Some(remaining),
        unit: Some("percent".to_owned()),
        reset_at_utc: Some(Utc::now() + Duration::hours(reset_in_hours)),
        reset_label: None,
        metadata: [
            ("source".to_owned(), "model-quota".to_owned()),
            ("model_id".to_owned(), model_id.to_owned()),
        ]
        .into(),
    }
}

fn grouped_metric(
    key: &str,
    group: &str,
    bucket_id: &str,
    raw_bucket: &str,
    window_seconds: i64,
    remaining: Option<f64>,
) -> UsageMetric {
    let used_percent = remaining.map(|value| 100.0 - value);
    UsageMetric {
        key: key.to_owned(),
        name: format!("{group} {raw_bucket}"),
        used_percent,
        used_amount: used_percent,
        limit_amount: remaining.map(|_| 100.0),
        remaining_amount: remaining,
        unit: remaining.map(|_| "percent".to_owned()),
        reset_at_utc: None,
        reset_label: None,
        metadata: [
            ("source".to_owned(), "local-quota-summary".to_owned()),
            ("group".to_owned(), group.to_owned()),
            ("bucket_id".to_owned(), bucket_id.to_owned()),
            ("raw_bucket".to_owned(), raw_bucket.to_owned()),
            ("window_seconds".to_owned(), window_seconds.to_string()),
        ]
        .into(),
    }
}

#[test]
fn antigravity_group_mode_selects_real_pool_and_window_summary_metrics() {
    let metrics = vec![
        grouped_metric(
            "gemini-weekly",
            "Gemini",
            "gemini-weekly",
            "Weekly Limit Remaining",
            604_800,
            Some(71.0),
        ),
        grouped_metric(
            "gemini-weekly-constrained",
            "Gemini",
            "gemini-weekly",
            "Weekly Limit Remaining",
            604_800,
            Some(65.0),
        ),
        grouped_metric(
            "gemini-session",
            "Gemini",
            "gemini-5h",
            "Five Hour Limit Remaining",
            18_000,
            Some(39.0),
        ),
        grouped_metric(
            "3p-weekly",
            "Claude/GPT",
            "3p-weekly",
            "Weekly Limit Remaining",
            604_800,
            Some(100.0),
        ),
        grouped_metric(
            "3p-session",
            "Claude/GPT",
            "3p-5h",
            "Five Hour Limit Remaining",
            18_000,
            Some(100.0),
        ),
        model_metric("gemini-3.7-flash", "Gemini 3.7 Flash", 20.0, 5),
    ];

    let gemini_weekly = select_antigravity_quota_metric(
        &metrics,
        AntigravityQuotaGroup::Gemini,
        AntigravityQuotaPeriod::Weekly,
    )
    .unwrap();
    assert_eq!(gemini_weekly.key, "gemini-weekly-constrained");
    assert_eq!(gemini_weekly.remaining_percent(), Some(65.0));
    assert_eq!(
        select_antigravity_quota_metric(
            &metrics,
            AntigravityQuotaGroup::Gemini,
            AntigravityQuotaPeriod::FiveHour,
        )
        .unwrap()
        .remaining_percent(),
        Some(39.0)
    );
    assert_eq!(
        select_antigravity_quota_metric(
            &metrics,
            AntigravityQuotaGroup::ClaudeGpt,
            AntigravityQuotaPeriod::Weekly,
        )
        .unwrap()
        .remaining_percent(),
        Some(100.0)
    );
    assert!(
        select_antigravity_quota_metric(
            &[model_metric(
                "gemini-3.7-flash",
                "Gemini 3.7 Flash",
                20.0,
                5
            )],
            AntigravityQuotaGroup::Gemini,
            AntigravityQuotaPeriod::FiveHour,
        )
        .is_none()
    );
}

#[test]
fn antigravity_group_fallback_selects_the_lowest_real_model_quota() {
    let metrics = vec![
        model_metric("gemini-3.7-flash", "Gemini 3.7 Flash", 82.0, 120),
        model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 43.0, 168),
        model_metric("claude-sonnet-4-6", "Claude Sonnet 4.6", 72.0, 168),
        model_metric("claude-opus-4-6", "Claude Opus 4.6", 57.0, 168),
        model_metric("gpt-oss-120b", "GPT OSS 120B", 88.0, 168),
        model_metric("chat_20706", "Unknown model", 1.0, 168),
    ];

    let gemini =
        select_antigravity_model_quota_metric(&metrics, AntigravityQuotaGroup::Gemini).unwrap();
    assert_eq!(gemini.key, "gemini-2.5-pro");
    assert_eq!(gemini.remaining_percent(), Some(43.0));
    assert_eq!(
        gemini.reset_at_utc, metrics[1].reset_at_utc,
        "the fallback must preserve the selected model's actual reset time"
    );

    let claude_gpt =
        select_antigravity_model_quota_metric(&metrics, AntigravityQuotaGroup::ClaudeGpt).unwrap();
    assert_eq!(claude_gpt.key, "claude-opus-4-6");
    assert_eq!(claude_gpt.remaining_percent(), Some(57.0));

    assert!(
            select_antigravity_model_quota_metric(
                &[metrics[5].clone()],
                AntigravityQuotaGroup::Gemini,
            )
            .is_none()
        );
    assert!(antigravity_quota_period(&metrics[0]).is_none());
}

#[test]
fn antigravity_model_fallback_is_used_only_when_group_windows_are_absent() {
    let metrics = vec![
        grouped_metric(
            "gemini-weekly",
            "Gemini",
            "gemini-weekly",
            "Weekly Limit Remaining",
            604_800,
            Some(65.0),
        ),
        model_metric("gemini-3.7-flash", "Gemini 3.7 Flash", 20.0, 5),
    ];

    let grouped_weekly = select_antigravity_quota_metric(
        &metrics,
        AntigravityQuotaGroup::Gemini,
        AntigravityQuotaPeriod::Weekly,
    );
    assert!(
        select_antigravity_model_quota_fallback(
            &metrics,
            AntigravityQuotaGroup::Gemini,
            grouped_weekly,
            None,
        )
        .is_none()
    );

    assert_eq!(
        select_antigravity_model_quota_fallback(
            &metrics,
            AntigravityQuotaGroup::Gemini,
            None,
            None,
        )
        .map(|metric| metric.key.as_str()),
        Some("gemini-3.7-flash")
    );
}

#[test]
fn returning_to_all_or_pinned_view_closes_antigravity_group_mode() {
    let mut state = DashboardState::loading();
    state.set_show_antigravity_quota_groups(true);

    state.set_show_all_model_quotas(false);

    assert!(!state.show_antigravity_quota_groups);
    assert!(!state.show_all_model_quotas);
}

#[test]
fn antigravity_group_mode_is_the_default() {
    let state = DashboardState::loading();

    assert!(state.show_antigravity_quota_groups);
    assert!(!state.show_all_model_quotas);
}

#[test]
fn incremental_account_usage_replaces_only_the_completed_account() {
    let mut state = DashboardState::loading();
    let first = AccountRecord::create(
        "First Codex account",
        "first@example.com",
        None,
        "openai",
        None,
    )
    .unwrap();
    let second = AccountRecord::create(
        "Second Codex account",
        "second@example.com",
        None,
        "openai",
        None,
    )
    .unwrap();
    let first_id = first.id;
    let second_id = second.id;
    state.set_accounts(vec![
        AccountUsageEntry {
            account: first,
            snapshot: None,
        },
        AccountUsageEntry {
            account: second,
            snapshot: None,
        },
    ]);

    let mut refreshed_first = state
        .account_entries()
        .iter()
        .find(|entry| entry.account.id == first_id)
        .unwrap()
        .clone();
    refreshed_first.account.label = "Personal Codex".to_owned();
    state.update_account_usage(refreshed_first);

    assert_eq!(state.account_entries().len(), 2);
    assert_eq!(
        state
            .account_entries()
            .iter()
            .find(|entry| entry.account.id == first_id)
            .unwrap()
            .account
            .label,
        "Personal Codex"
    );
    assert!(
        state
            .account_entries()
            .iter()
            .any(|entry| entry.account.id == second_id)
    );
}

fn flattened_models(models: &[UsageMetric]) -> &[UsageMetric] {
    models
}

fn account_entry_with_usage(used_percent: f64) -> AccountUsageEntry {
    let account =
        AccountRecord::create("Codex test", "codex-test@example.com", None, "openai", None)
            .unwrap();
    let snapshot = UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id: None,
        plan_type: Some("pro".to_owned()),
        primary: Some(RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: "Primary".to_owned(),
            used_percent,
            reset_at_utc: None,
            limit_window_seconds: 5 * 60 * 60,
        }),
        primary_window_kind: Some(UsagePrimaryWindowKind::Session),
        primary_window_is_synthetic: false,
        secondary: None,
        additional_windows: Vec::new(),
        credits: None,
        credit_inventory: None,
        spend: None,
        observed_email: None,
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics: Vec::new(),
        source_diagnostics: Vec::new(),
        provider_id: "openai".to_owned(),
        source: Some("api".to_owned()),
        data_confidence: "authoritative".to_owned(),
    };

    AccountUsageEntry {
        account,
        snapshot: Some(snapshot),
    }
}

#[test]
fn changed_usage_starts_a_transition_from_the_previous_visible_value() {
    let previous = account_entry_with_usage(20.0);
    let mut next = previous.clone();
    next.snapshot
        .as_mut()
        .unwrap()
        .primary
        .as_mut()
        .unwrap()
        .used_percent = 42.0;

    let now = Instant::now();
    let mut animation = UsageAnimationState::default();
    animation.update(
        std::slice::from_ref(&previous),
        std::slice::from_ref(&next),
        now,
    );

    let key = UsagePercentKey {
        account_id: next.account.id,
        field: "window:primary".to_owned(),
    };
    let transition = animation.transitions.get(&key).unwrap();
    assert_eq!(transition.from_remaining, 80.0);
    assert_eq!(transition.to_remaining, 58.0);
    assert!(animation.is_active());
}

#[test]
fn single_account_update_keeps_other_accounts_running_transitions() {
    let with_usage = |entry: &AccountUsageEntry, used: f64| {
        let mut next = entry.clone();
        next.snapshot
            .as_mut()
            .unwrap()
            .primary
            .as_mut()
            .unwrap()
            .used_percent = used;
        next
    };
    let first = account_entry_with_usage(20.0);
    let second = account_entry_with_usage(30.0);
    let now = Instant::now();
    let mut animation = UsageAnimationState::default();
    animation.update(
        &[first.clone(), second.clone()],
        &[with_usage(&first, 40.0), second.clone()],
        now,
    );
    assert!(animation.has_transitions_for(first.account.id));

    animation.update_account(&second, &with_usage(&second, 50.0), now);

    assert!(
        animation.has_transitions_for(first.account.id),
        "updating one account must not cancel another's animation"
    );
    assert!(animation.has_transitions_for(second.account.id));
}

#[test]
fn usage_percent_transition_eases_to_the_new_value() {
    let started_at = Instant::now();
    let transition = UsagePercentTransition {
        from_remaining: 82.0,
        to_remaining: 67.0,
        started_at,
    };

    assert_eq!(transition.value_at(started_at), 82.0);
    let midpoint = transition.value_at(started_at + USAGE_CHANGE_ANIMATION_DURATION / 2);
    assert!(midpoint < 82.0 && midpoint > 67.0);
    assert_eq!(
        transition.value_at(started_at + USAGE_CHANGE_ANIMATION_DURATION),
        67.0
    );
    assert!(!transition.is_active(started_at + USAGE_CHANGE_ANIMATION_DURATION));
}

#[test]
fn reset_label_uses_date_and_day_hour_count_after_24_hours() {
    let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 0).unwrap();
    let reset_at = now + Duration::hours(51) + Duration::minutes(20);

    let label = reset_label(reset_at, now, Language::English);
    let formatted_reset = format_local_reset(reset_at.with_timezone(&Local), Language::English);

    assert!(label.starts_with("Resets on "));
    assert!(label.contains(&formatted_reset));
    assert!(label.contains(" at "));
    assert!(label.contains("· in 2d 3h"));
    assert!(!label.contains("51h"));
}

#[test]
fn moving_an_account_reorders_only_its_own_tab() {
    let make = |provider: &str, email: &str| AccountUsageEntry {
        account: AccountRecord::create(email, email, None, provider, None).unwrap(),
        snapshot: None,
    };
    let entries = vec![
        make(usage_monitor_core::accounts::OPENAI, "a@example.com"),
        make(usage_monitor_core::accounts::CLAUDE, "c@example.com"),
        make(usage_monitor_core::accounts::OPENAI, "b@example.com"),
    ];
    let ids = entries
        .iter()
        .map(|entry| entry.account.id)
        .collect::<Vec<_>>();
    let mut state = DashboardState::loading();
    state.entries = entries;
    state.account_order = Vec::new();

    let codex_order = |state: &DashboardState| {
        state
            .ordered_entries(DashboardTab::Provider(UsageProvider::Codex))
            .iter()
            .map(|entry| entry.account.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(codex_order(&state), vec![ids[0], ids[2]]);

    let codex = DashboardTab::Provider(UsageProvider::Codex);
    // Moving the first account up does nothing.
    assert!(!state.move_account(codex, ids[0], -1));
    assert_eq!(codex_order(&state), vec![ids[0], ids[2]]);

    assert!(state.move_account(codex, ids[2], -1));
    assert_eq!(codex_order(&state), vec![ids[2], ids[0]]);
    assert_eq!(
        state.account_order,
        vec![ids[2], ids[1], ids[0]],
        "the Claude account keeps its slot"
    );
}

#[test]
fn favorites_gather_starred_accounts_in_their_own_order() {
    let make = |provider: &str, email: &str| AccountUsageEntry {
        account: AccountRecord::create(email, email, None, provider, None).unwrap(),
        snapshot: None,
    };
    let entries = vec![
        make(usage_monitor_core::accounts::OPENAI, "a@example.com"),
        make(usage_monitor_core::accounts::CLAUDE, "c@example.com"),
        make(usage_monitor_core::accounts::OPENAI, "b@example.com"),
    ];
    let ids = entries
        .iter()
        .map(|entry| entry.account.id)
        .collect::<Vec<_>>();
    let mut state = DashboardState::loading();
    state.entries = entries;
    state.account_order = Vec::new();
    state.favorites = Vec::new();

    let favorites = |state: &DashboardState| {
        state
            .ordered_entries(DashboardTab::Favorites)
            .iter()
            .map(|entry| entry.account.id)
            .collect::<Vec<_>>()
    };
    assert!(favorites(&state).is_empty());

    // Starred accounts from different providers, in the order starred.
    state.toggle_favorite(ids[2]);
    state.toggle_favorite(ids[1]);
    assert_eq!(favorites(&state), vec![ids[2], ids[1]]);

    // Reordering the Favorites tab leaves the provider order alone.
    assert!(state.move_account(DashboardTab::Favorites, ids[1], -1));
    assert_eq!(favorites(&state), vec![ids[1], ids[2]]);
    assert!(state.account_order.is_empty());

    // A starred account that was deleted is not shown.
    let deleted = AccountId::new();
    state.favorites.push(deleted);
    assert_eq!(favorites(&state), vec![ids[1], ids[2]]);

    state.toggle_favorite(ids[1]);
    assert_eq!(favorites(&state), vec![ids[2]]);
}

#[test]
fn reset_label_keeps_hour_format_at_exactly_24_hours() {
    let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 0).unwrap();
    let reset_at = now + Duration::hours(24);
    let label = reset_label(reset_at, now, Language::English);

    let clock = reset_at.with_timezone(&Local).format("%-I:%M %p");
    assert_eq!(label, format!("Resets tomorrow at {clock} · in 24h 0m"));
}

#[test]
fn stale_label_says_when_the_shown_reading_was_taken() {
    let now = Local
        .with_ymd_and_hms(2026, 9, 24, 15, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        stale_label(now - Duration::minutes(11), now, Language::English),
        "Couldn't update · showing the reading from 2:49 PM"
    );
    assert_eq!(
        stale_label(now - Duration::minutes(11), now, Language::Arabic),
        "تعذّر التحديث · هذه القراءة من 14:49"
    );
    assert_eq!(
        stale_label(now - Duration::days(2), now, Language::English),
        "Couldn't update · showing the reading from Sep 22, 2026 at 3:00 PM"
    );
}

#[test]
fn reset_within_a_day_shows_its_clock_time_and_countdown() {
    // Midday local time keeps a 3h reset on the same day in any time zone.
    let now = Local
        .with_ymd_and_hms(2026, 9, 24, 12, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let reset_at = now + Duration::minutes(3 * 60 + 25);

    assert_eq!(
        reset_label(reset_at, now, Language::English),
        "Resets at 3:25 PM · in 3h 25m"
    );
    assert_eq!(
        reset_label(reset_at, now, Language::Arabic),
        "يتجدد الساعة 15:25 · بعد 3 س و25 د"
    );
    assert_eq!(
        countdown_label_parts("Resets at 3:25 PM · in 3h 25m", Language::English),
        Some(("Resets at 3:25 PM · ", "in 3h 25m"))
    );
}

#[test]
fn countdown_label_parts_highlight_only_relative_time_in_both_languages() {
    let english = "Resets on Sep 29, 2026 at 10:20 PM · in 4d 11h";
    assert_eq!(
        countdown_label_parts(english, Language::English),
        Some(("Resets on Sep 29, 2026 at 10:20 PM · ", "in 4d 11h"))
    );

    let arabic = "يتجدد في 2026-09-29 22:20 · بعد ٤ أيام و١١ ساعة";
    assert_eq!(
        countdown_label_parts(arabic, Language::Arabic),
        Some(("يتجدد في 2026-09-29 22:20 · ", "بعد ٤ أيام و١١ ساعة"))
    );

    assert_eq!(
        countdown_label_parts("Resets in 24h 0m", Language::English),
        Some(("Resets ", "in 24h 0m"))
    );
    assert_eq!(
        countdown_label_parts("يتجدد بعد 4 س و15 د", Language::Arabic),
        Some(("يتجدد ", "بعد 4 س و15 د"))
    );
    assert_eq!(
        countdown_label_parts(
            "Expires Sep 29, 2026 at 10:20 PM · in 4d 11h",
            Language::English
        ),
        Some(("Expires Sep 29, 2026 at 10:20 PM · ", "in 4d 11h"))
    );
    assert_eq!(
        countdown_label_parts("Reset time reached", Language::English),
        None
    );
}

#[test]
fn reset_credit_expiry_shows_local_date_and_remaining_days_or_hours() {
    let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 0).unwrap();
    let expiry = now + Duration::hours(51) + Duration::minutes(20);
    let label = credit_expiration_label(expiry, now, Language::English);
    let local_date = format_local_reset(expiry.with_timezone(&Local), Language::English);

    assert!(label.starts_with("Expires "));
    assert!(label.contains(&local_date));
    assert!(label.contains("· in 2d 3h"));
    assert!(!label.contains("51h"));

    let under_one_day = credit_expiration_label(
        now + Duration::hours(4) + Duration::minutes(15),
        now,
        Language::English,
    );
    assert!(under_one_day.contains("· in 4h 15m"));

    let expired = credit_expiration_label(now - Duration::minutes(1), now, Language::English);
    assert!(expired.starts_with("Expired "));
}

#[test]
fn spend_summary_hides_percentages_that_render_as_zero() {
    assert_eq!(displayable_spend_percent(Some(0.0)), None);
    assert_eq!(displayable_spend_percent(Some(0.49)), None);
    assert_eq!(displayable_spend_percent(Some(1.0)), Some(1.0));
    assert_eq!(displayable_spend_percent(None), None);
}

#[test]
fn session_window_uses_the_official_five_hour_limit_label() {
    assert_eq!(
        display_window_name("session", Language::English),
        "5 hours limit"
    );
    assert_eq!(
        display_window_name("5-hour", Language::Arabic),
        "حد 5 ساعات"
    );
}

#[test]
fn account_alias_can_be_reset_to_original_or_saved_trimmed() {
    assert_eq!(normalized_alias("   ", "Provider name"), None);
    assert_eq!(normalized_alias(" Provider name ", "Provider name"), None);
    assert_eq!(
        normalized_alias("  Work account  ", "Provider name"),
        Some("Work account".to_owned())
    );
}

#[test]
fn plan_name_starts_with_an_uppercase_letter() {
    assert_eq!(capitalize_first("plus"), "Plus");
    assert_eq!(capitalize_first(""), "");
}

#[test]
fn reset_credit_expiry_lists_only_credits_still_counted_as_available() {
    let record = |id: &str, status: Option<&str>| UsageCreditRecord {
        id: Some(id.to_owned()),
        reset_type: None,
        status: status.map(str::to_owned),
        granted_at_utc: None,
        expires_at_utc: None,
        redeem_started_at_utc: None,
        redeemed_at_utc: None,
        title: None,
        description: None,
    };
    let inventory = UsageCreditInventory {
        available_count: 2,
        credits: vec![
            record("available", Some("available")),
            record("legacy-without-status", None),
            record("redeemed", Some("redeemed")),
        ],
    };

    let listed = available_reset_credits(&inventory);
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|credit| {
        credit
            .status
            .as_deref()
            .is_none_or(|status| status.eq_ignore_ascii_case("available"))
    }));

    let empty_inventory = UsageCreditInventory {
        available_count: 0,
        credits: vec![record("stale", None)],
    };
    assert!(available_reset_credits(&empty_inventory).is_empty());
}

#[test]
fn weekly_reset_inventory_anchor_recognizes_semantic_and_named_weekly_windows() {
    let primary = RateLimitWindow {
        kind: UsageWindowKind::Primary,
        name: "primary".to_owned(),
        used_percent: 25.0,
        reset_at_utc: None,
        limit_window_seconds: 5 * 60 * 60,
    };
    assert!(is_weekly_usage_window(
        &primary,
        true,
        Some(UsagePrimaryWindowKind::Weekly)
    ));
    assert!(!is_weekly_usage_window(
        &primary,
        false,
        Some(UsagePrimaryWindowKind::Weekly)
    ));
    assert!(is_weekly_usage_name("Weekly"));
    assert!(is_weekly_usage_name("7-day"));
}

#[test]
fn pinned_mode_defaults_to_curated_families_and_all_mode_shows_every_family() {
    let metrics = [
        model_metric("gemini-3.1-pro-low", "Gemini 3.1 Pro Low", 80.0, 6),
        model_metric("gemini-3.1-pro-high", "Gemini 3.1 Pro High", 60.0, 6),
        model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 91.0, 6),
    ];
    let default_visibility = ModelVisibilityPreferences::default();
    let input = metrics.iter().collect::<Vec<_>>();
    let pinned = visible_model_quota_metrics(&input, &default_visibility, false);
    assert_eq!(flattened_models(&pinned).len(), 1);
    assert_eq!(pinned[0].name, "Gemini 3.1 Pro");
    assert_eq!(pinned[0].remaining_percent(), Some(60.0));

    let mut visibility = ModelVisibilityPreferences::default();
    visibility.set_visible("gemini-3.1-pro", false);
    let pinned = visible_model_quota_metrics(&input, &visibility, false);
    assert!(pinned.is_empty());

    let all = visible_model_quota_metrics(&input, &visibility, true);
    assert_eq!(flattened_models(&all).len(), 2);
    assert_eq!(all[0].name, "Gemini 2.5 Pro");
}

#[test]
fn default_pins_match_the_compact_antigravity_selection() {
    let metrics = [
        model_metric("gemini-3.1-pro-high", "Gemini 3.1 Pro High", 67.0, 4),
        model_metric("gemini-3.1-flash-image", "Gemini 3.1 Flash Image", 67.0, 4),
        model_metric("gemini-3-flash", "Gemini 3 Flash", 67.0, 4),
        model_metric(
            "claude-opus-4-6-thinking",
            "Claude Opus 4.6 Thinking",
            67.0,
            4,
        ),
        model_metric("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite", 67.0, 4),
    ];
    let pinned = visible_model_quota_metrics(
        &metrics.iter().collect::<Vec<_>>(),
        &ModelVisibilityPreferences::default(),
        false,
    );
    assert_eq!(
        pinned
            .iter()
            .map(|metric| metric.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Claude Opus 4.6",
            "Gemini 3 Flash",
            "Gemini 3.1 Flash Image",
            "Gemini 3.1 Pro",
        ]
    );
}

#[test]
fn model_visibility_preferences_keep_legacy_lines_and_allow_visible_overrides() {
    let preferences =
        parse_model_visibility_preferences("gemini-3.1-pro-high\nvisible:Models/gemini-2.5-pro\n");

    assert!(!preferences.is_visible("gemini-3.1-pro-high"));
    assert!(!preferences.is_visible("gemini-2.5-flash"));
    assert!(preferences.is_visible("gemini-2.5-pro"));
    assert!(preferences.is_visible("gemini-3.1-pro-low"));
}

#[test]
fn hidden_model_indicator_matches_the_available_model_families() {
    let metrics = [
        model_metric("gemini-3.1-pro-high", "Gemini 3.1 Pro High", 82.0, 12),
        model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 70.0, 8),
    ];
    let mut visibility = ModelVisibilityPreferences::default();

    assert!(has_hidden_model_quota(&metrics, &visibility));

    visibility.set_visible("gemini-2.5-pro", true);
    assert!(!has_hidden_model_quota(&metrics, &visibility));
}

#[test]
fn thinking_variants_render_once_without_thinking_names_and_use_conservative_quota() {
    let metrics = [
        model_metric(
            "gemini-3.7-flash-medium",
            "Gemini 3.7 Flash Medium",
            82.0,
            12,
        ),
        model_metric("gemini-3.7-flash-high", "Gemini 3.7 Flash High", 61.0, 8),
        model_metric("claude-sonnet-4-5", "Claude Sonnet 4.5", 75.0, 6),
        model_metric(
            "claude-sonnet-4-5-thinking",
            "Claude Sonnet 4.5 Thinking",
            24.0,
            3,
        ),
    ];
    let models = visible_model_quota_metrics(
        &metrics.iter().collect::<Vec<_>>(),
        &ModelVisibilityPreferences::default(),
        true,
    );
    let flash = models
        .iter()
        .find(|metric| metric.name == "Gemini 3.7 Flash")
        .unwrap();
    assert_eq!(flash.remaining_percent(), Some(61.0));
    assert!(
        !models
            .iter()
            .any(|metric| metric.name == "High" || metric.name == "Medium")
    );

    let sonnet = models
        .iter()
        .find(|metric| metric.name == "Claude Sonnet 4.5")
        .unwrap();
    assert_eq!(sonnet.remaining_percent(), Some(24.0));
    assert!(sonnet.reset_at_utc.unwrap() <= Utc::now() + Duration::hours(3));
}

#[test]
fn thinking_variants_and_base_model_collapse_into_one_named_row() {
    let metrics = [
        model_metric("gemini-3.8-flash", "Gemini 3.8 Flash", 91.0, 12),
        model_metric("gemini-3.8-flash-high", "Gemini 3.8 Flash High", 61.0, 8),
        model_metric(
            "gemini-3.8-flash-medium",
            "Gemini 3.8 Flash Medium",
            82.0,
            10,
        ),
    ];
    let models = visible_model_quota_metrics(
        &metrics.iter().collect::<Vec<_>>(),
        &ModelVisibilityPreferences::default(),
        true,
    );
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "Gemini 3.8 Flash");
    assert_eq!(models[0].remaining_percent(), Some(61.0));
}

#[test]
fn distinct_base_models_remain_separate_while_thinking_levels_merge() {
    let metrics = [
        model_metric("gemini-3-flash", "Gemini 3 Flash", 90.0, 8),
        model_metric("gemini-3.5-flash-high", "Gemini 3.5 Flash High", 80.0, 8),
        model_metric(
            "gemini-3.5-flash-medium",
            "Gemini 3.5 Flash Medium",
            70.0,
            8,
        ),
    ];
    let models = visible_model_quota_metrics(
        &metrics.iter().collect::<Vec<_>>(),
        &ModelVisibilityPreferences::default(),
        true,
    );
    assert_eq!(models.len(), 2);
    assert!(models.iter().any(|metric| metric.name == "Gemini 3 Flash"));
    let flash_35 = models
        .iter()
        .find(|metric| metric.name == "Gemini 3.5 Flash")
        .unwrap();
    assert_eq!(flash_35.remaining_percent(), Some(70.0));
}

#[test]
fn a_single_thinking_variant_displays_its_base_model_name() {
    let metrics = [
        model_metric(
            "gemini-3.7-flash-medium",
            "Gemini 3.7 Flash Medium",
            82.0,
            12,
        ),
        model_metric("gemini-3.7-flash-high", "Gemini 3.7 Flash High", 61.0, 8),
    ];
    let mut visibility = ModelVisibilityPreferences::default();
    visibility.set_visible("gemini-3.7-flash", true);
    let models =
        visible_model_quota_metrics(&metrics.iter().collect::<Vec<_>>(), &visibility, true);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "Gemini 3.7 Flash");
}

#[test]
fn unpinned_models_are_hidden_until_selected_or_all_models_is_enabled() {
    let metrics = [
        model_metric("gemini-1.5-pro", "Gemini 1.5 Pro", 96.0, 4),
        model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 70.0, 4),
        model_metric("gemini-2.5-flash", "Gemini 2.5 Flash", 95.0, 4),
        model_metric("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite", 67.0, 4),
    ];
    let mut visibility = ModelVisibilityPreferences::default();
    assert!(!visibility.is_visible("gemini-1.5-pro"));
    assert!(!visibility.is_visible("gemini-2.5-pro"));

    let input = metrics.iter().collect::<Vec<_>>();
    assert!(visible_model_quota_metrics(&input, &visibility, false).is_empty());

    let all = visible_model_quota_metrics(&input, &visibility, true);
    assert_eq!(all.len(), 4);
    assert!(
        all.iter()
            .any(|metric| metric.name == "Gemini 3.1 Flash Lite")
    );

    visibility.set_visible("gemini-2.5-pro", true);
    assert!(visibility.is_visible("gemini-2.5-pro"));
    let models = visible_model_quota_metrics(&input, &visibility, false);
    assert!(models.iter().any(|metric| metric.name == "Gemini 2.5 Pro"));
}

#[test]
fn duplicate_window_metrics_are_hidden_without_hiding_provider_specific_metrics() {
    let window = RateLimitWindow {
        kind: UsageWindowKind::Primary,
        name: "Session".to_owned(),
        used_percent: 25.0,
        reset_at_utc: None,
        limit_window_seconds: 3600,
    };
    let duplicate = UsageMetric {
        key: "primary".to_owned(),
        name: "Session".to_owned(),
        used_percent: Some(25.0),
        used_amount: None,
        limit_amount: None,
        remaining_amount: None,
        unit: None,
        reset_at_utc: None,
        reset_label: None,
        metadata: Default::default(),
    };
    let specific = UsageMetric {
        name: "Model A".to_owned(),
        ..duplicate.clone()
    };

    assert!(duplicates_rate_window(&duplicate, &[&window]));
    assert!(!duplicates_rate_window(&specific, &[&window]));
}

#[test]
fn openrouter_activity_is_hidden_without_hiding_other_provider_data() {
    let activity = UsageMetric {
        key: "activity.summary".to_owned(),
        name: "Activity summary".to_owned(),
        used_percent: None,
        used_amount: Some(12.0),
        limit_amount: None,
        remaining_amount: None,
        unit: Some("USD".to_owned()),
        reset_at_utc: None,
        reset_label: None,
        metadata: Default::default(),
    };
    let credits = UsageMetric {
        key: "credits.balance".to_owned(),
        name: "Credits".to_owned(),
        ..activity.clone()
    };

    assert!(is_openrouter_activity_metric("openrouter", &activity));
    assert!(!is_openrouter_activity_metric("openrouter", &credits));
    assert!(!is_openrouter_activity_metric(
        "another-provider",
        &activity
    ));
    assert!(is_openrouter_activity_source("openrouter", "activity"));
    assert!(!is_openrouter_activity_source("openrouter", "credits"));
    assert!(!is_openrouter_activity_source(
        "another-provider",
        "activity"
    ));
}

#[test]
fn remaining_percentage_is_clamped_and_non_finite_values_do_not_reach_ui() {
    assert_eq!(valid_percent(-10.0), Some(0.0));
    assert_eq!(valid_percent(140.0), Some(100.0));
    assert_eq!(valid_percent(f64::NAN), None);
}

#[test]
fn amounts_in_a_currency_unit_read_as_money() {
    use super::widgets::format_amount;
    assert_eq!(
        format_amount(7.25, Some("USD"), None, Language::English),
        "$7.25"
    );
    assert_eq!(
        format_amount(8.5, Some("CNY"), None, Language::English),
        "¥8.50"
    );
    assert_eq!(
        format_amount(3.0, Some("requests"), None, Language::English),
        "3.00 requests"
    );
    assert_eq!(
        format_amount(1.0, None, Some("CHF"), Language::English),
        "1.00 CHF"
    );
}

#[test]
fn placeholder_emails_of_key_based_accounts_are_not_shown() {
    assert_eq!(shown_email("deepseek@local.invalid"), "");
    assert_eq!(shown_email("openrouter@LOCAL.INVALID"), "");
    assert_eq!(shown_email("someone@example.invalid"), "");
    assert_eq!(shown_email("someone@example.com"), "someone@example.com");
    assert_eq!(shown_email("invalid@gmail.com"), "invalid@gmail.com");
}
