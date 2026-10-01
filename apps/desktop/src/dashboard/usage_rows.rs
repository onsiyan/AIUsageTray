//! Usage rows for a snapshot: rate windows, Antigravity groups, spend, and reset credits.

use super::*;

pub(super) fn append_snapshot_rows(
    rows: &mut Vec<Element<'static, Message>>,
    snapshot: &UsageSnapshot,
    show_spend_summary: bool,
    model_visibility: &ModelVisibilityPreferences,
    show_all_model_quotas: bool,
    show_antigravity_quota_groups: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    if snapshot.is_stale {
        rows.push(warning_line(
            locale::text(language, Text::StaleUsage),
            theme,
        ));
    }

    if show_antigravity_quota_groups && snapshot.provider_id.eq_ignore_ascii_case("antigravity") {
        append_antigravity_quota_groups(
            rows,
            &snapshot.metrics,
            &snapshot.data_confidence,
            theme,
            language,
        );
        if let Some(inventory) = &snapshot.credit_inventory {
            append_reset_credit_inventory(rows, inventory, theme, language);
        }
        if show_spend_summary {
            append_spend_summary(rows, snapshot.spend.as_ref(), theme, language);
        }
        append_snapshot_source_diagnostics(rows, snapshot, theme, language);
        return;
    }

    let model_metrics = snapshot
        .metrics
        .iter()
        .filter(|metric| is_model_quota_metric(metric))
        .collect::<Vec<_>>();
    let windows = snapshot
        .all_rate_windows()
        .filter(|window| {
            !model_metrics
                .iter()
                .any(|metric| metric.name.trim().eq_ignore_ascii_case(window.name.trim()))
        })
        .collect::<Vec<_>>();
    let metrics = snapshot
        .metrics
        .iter()
        .filter(|metric| !is_openrouter_activity_metric(&snapshot.provider_id, metric))
        .filter(|metric| !is_model_quota_metric(metric))
        .filter(|metric| !duplicates_rate_window(metric, &windows))
        .collect::<Vec<_>>();

    // Reset credits sit under the last weekly lane: the additional weekly
    // window when there is one, otherwise the regular weekly window.
    let reset_inventory_anchor = windows.iter().rposition(|window| {
        let is_primary = snapshot
            .primary
            .as_ref()
            .is_some_and(|primary| std::ptr::eq(primary, *window));
        is_weekly_usage_window(window, is_primary, snapshot.primary_window_kind)
            || is_additional_weekly_window(window)
    });
    let mut reset_inventory_rendered = false;
    if !windows.is_empty() || !metrics.is_empty() {
        for (index, window) in windows.into_iter().enumerate() {
            rows.push(rate_window_row(window, theme, language));
            if !reset_inventory_rendered
                && reset_inventory_anchor == Some(index)
                && let Some(inventory) = &snapshot.credit_inventory
            {
                if inventory.available_count > 0 {
                    rows.push(space().height(Length::Fixed(8.0)).into());
                }
                append_reset_credit_inventory(rows, inventory, theme, language);
                reset_inventory_rendered = true;
            }
        }
        for metric in metrics {
            rows.push(metric_row(metric, theme, language));
        }
    }

    if !reset_inventory_rendered && let Some(inventory) = &snapshot.credit_inventory {
        append_reset_credit_inventory(rows, inventory, theme, language);
    }

    let visible_models =
        visible_model_quota_metrics(&model_metrics, model_visibility, show_all_model_quotas);
    if !model_metrics.is_empty() && visible_models.is_empty() {
        rows.push(
            text(locale::text(language, Text::NoModelsVisible))
                .size(typography::BODY_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
    append_model_quota_metrics(rows, visible_models, theme, language);

    if show_spend_summary {
        append_spend_summary(rows, snapshot.spend.as_ref(), theme, language);
    }

    append_snapshot_source_diagnostics(rows, snapshot, theme, language);
}

pub(super) fn append_snapshot_source_diagnostics(
    rows: &mut Vec<Element<'static, Message>>,
    snapshot: &UsageSnapshot,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let visible_diagnostic_count = snapshot
        .source_diagnostics
        .iter()
        .filter(|diagnostic| {
            !is_openrouter_activity_source(&snapshot.provider_id, &diagnostic.source)
        })
        .count();
    if visible_diagnostic_count == 0 {
        return;
    }
    let message = match language {
        Language::English => format!(
            "{} data sources could not be completed",
            visible_diagnostic_count
        ),
        Language::Arabic => format!("تعذر إكمال {} من مصادر البيانات", visible_diagnostic_count),
    };
    rows.push(warning_line(&message, theme));
}

pub(super) fn is_openrouter_activity_metric(provider_id: &str, metric: &UsageMetric) -> bool {
    provider_id.eq_ignore_ascii_case("openrouter") && metric.key.starts_with("activity.")
}

pub(super) fn is_openrouter_activity_source(provider_id: &str, source: &str) -> bool {
    provider_id.eq_ignore_ascii_case("openrouter") && source.eq_ignore_ascii_case("activity")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AntigravityQuotaGroup {
    Gemini,
    ClaudeGpt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AntigravityQuotaPeriod {
    Weekly,
    FiveHour,
}

pub(super) fn is_antigravity_summary_metric(metric: &UsageMetric) -> bool {
    metric
        .metadata
        .get("source")
        .is_some_and(|source| source == "local-quota-summary")
}

pub(super) fn antigravity_quota_group(metric: &UsageMetric) -> Option<AntigravityQuotaGroup> {
    let group = metric
        .metadata
        .get("group")
        .map(String::as_str)
        .unwrap_or(&metric.name)
        .to_ascii_lowercase();
    if group.contains("gemini") {
        Some(AntigravityQuotaGroup::Gemini)
    } else if group.contains("claude") || group.contains("gpt") || group.contains("3p") {
        Some(AntigravityQuotaGroup::ClaudeGpt)
    } else {
        None
    }
}

pub(super) fn antigravity_model_quota_group(metric: &UsageMetric) -> Option<AntigravityQuotaGroup> {
    if !is_model_quota_metric(metric) {
        return None;
    }

    let model_id = normalize_model_id(model_quota_id(metric));
    let model_name = metric.name.trim().to_ascii_lowercase();
    if model_id.starts_with("gemini-") || model_name.starts_with("gemini ") {
        Some(AntigravityQuotaGroup::Gemini)
    } else if model_id.starts_with("claude-")
        || model_id.starts_with("gpt-")
        || model_name.starts_with("claude ")
        || model_name.starts_with("gpt ")
    {
        Some(AntigravityQuotaGroup::ClaudeGpt)
    } else {
        None
    }
}

pub(super) fn antigravity_quota_period(metric: &UsageMetric) -> Option<AntigravityQuotaPeriod> {
    let labels = [
        metric.metadata.get("bucket_id").map(String::as_str),
        metric.metadata.get("raw_bucket").map(String::as_str),
        Some(metric.name.as_str()),
    ];
    for label in labels.into_iter().flatten() {
        let normalized = label.trim().to_ascii_lowercase().replace('_', "-");
        if normalized.contains("week") {
            return Some(AntigravityQuotaPeriod::Weekly);
        }
        if normalized.contains("session")
            || normalized.contains("5-hour")
            || normalized.contains("5 hour")
            || normalized.contains("5h")
            || normalized.contains("five hour")
        {
            return Some(AntigravityQuotaPeriod::FiveHour);
        }
    }

    match metric
        .metadata
        .get("window_seconds")
        .and_then(|value| value.parse::<i64>().ok())
    {
        Some(14_400..=21_600) => Some(AntigravityQuotaPeriod::FiveHour),
        Some(518_400..=691_200) => Some(AntigravityQuotaPeriod::Weekly),
        _ => None,
    }
}

pub(super) fn select_antigravity_quota_metric(
    metrics: &[UsageMetric],
    group: AntigravityQuotaGroup,
    period: AntigravityQuotaPeriod,
) -> Option<&UsageMetric> {
    let candidates = metrics
        .iter()
        .filter(|metric| {
            is_antigravity_summary_metric(metric)
                && antigravity_quota_group(metric) == Some(group)
                && antigravity_quota_period(metric) == Some(period)
        })
        .collect::<Vec<_>>();
    let known = candidates
        .iter()
        .copied()
        .filter(|metric| metric.remaining_percent().is_some_and(f64::is_finite))
        .collect::<Vec<_>>();

    known
        .into_iter()
        .min_by(|left, right| {
            left.remaining_percent()
                .unwrap_or_default()
                .total_cmp(&right.remaining_percent().unwrap_or_default())
        })
        .or_else(|| candidates.first().copied())
}

pub(super) fn select_antigravity_model_quota_metric(
    metrics: &[UsageMetric],
    group: AntigravityQuotaGroup,
) -> Option<&UsageMetric> {
    let candidates = metrics
        .iter()
        .filter(|metric| antigravity_model_quota_group(metric) == Some(group))
        .collect::<Vec<_>>();
    let known = candidates
        .iter()
        .copied()
        .filter(|metric| metric.remaining_percent().is_some_and(f64::is_finite));

    known
        .min_by(|left, right| {
            left.remaining_percent()
                .unwrap_or_default()
                .total_cmp(&right.remaining_percent().unwrap_or_default())
        })
        .or_else(|| candidates.first().copied())
}

pub(super) fn select_antigravity_model_quota_fallback<'a>(
    metrics: &'a [UsageMetric],
    group: AntigravityQuotaGroup,
    weekly: Option<&'a UsageMetric>,
    five_hour: Option<&'a UsageMetric>,
) -> Option<&'a UsageMetric> {
    if weekly.is_some() || five_hour.is_some() {
        return None;
    }

    select_antigravity_model_quota_metric(metrics, group)
}

pub(super) fn append_antigravity_quota_groups(
    rows: &mut Vec<Element<'static, Message>>,
    metrics: &[UsageMetric],
    data_confidence: &str,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let model_quota_is_authoritative = data_confidence.eq_ignore_ascii_case("authoritative");
    let mut displayed_any_group = false;

    for (group, title) in [
        (AntigravityQuotaGroup::Gemini, Text::GeminiModels),
        (AntigravityQuotaGroup::ClaudeGpt, Text::ClaudeGptModels),
    ] {
        if group == AntigravityQuotaGroup::ClaudeGpt && antigravity_claude_gpt_hidden() {
            continue;
        }
        let weekly =
            select_antigravity_quota_metric(metrics, group, AntigravityQuotaPeriod::Weekly);
        let five_hour =
            select_antigravity_quota_metric(metrics, group, AntigravityQuotaPeriod::FiveHour);
        // Prefer provider-supplied grouped windows. When they are absent, show
        // the actual most-constrained per-model quota with its reset time; the
        // model catalogue does not identify it as a shared 5-hour or weekly
        // window, so never label this fallback as one.
        let model_quota =
            select_antigravity_model_quota_fallback(metrics, group, weekly, five_hour);
        if weekly.is_none() && five_hour.is_none() && model_quota.is_none() {
            continue;
        }

        if displayed_any_group {
            rows.push(space().height(Length::Fixed(8.0)).into());
        }
        rows.push(
            text(locale::text(language, title))
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .into(),
        );
        // Put the shorter reset window first, followed by the weekly limit.
        if let Some(metric) = five_hour {
            rows.push(antigravity_quota_row(
                metric,
                Text::FiveHourLimit,
                theme,
                language,
            ));
        }
        if let Some(metric) = weekly {
            rows.push(antigravity_quota_row(metric, Text::Weekly, theme, language));
        }
        if let Some(metric) = model_quota {
            let confidence_note = if model_quota_is_authoritative {
                ""
            } else {
                match language {
                    Language::English => " · unverified model quota",
                    Language::Arabic => " · حصة نموذج غير مؤكدة",
                }
            };
            let label = format!(
                "{} · {}{}",
                locale::text(
                    language,
                    match crate::percent_display::current() {
                        crate::percent_display::PercentDisplay::Remaining => Text::QuotaRemaining,
                        crate::percent_display::PercentDisplay::Used => Text::QuotaUsed,
                    }
                ),
                metric.name,
                confidence_note
            );
            rows.push(antigravity_quota_row_with_label(
                metric, label, theme, language,
            ));
        }
        displayed_any_group = true;
    }

    if !displayed_any_group {
        rows.push(
            text(locale::text(language, Text::GroupedQuotasUnavailable))
                .size(typography::METADATA_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
}

pub(super) fn antigravity_quota_row(
    metric: &UsageMetric,
    period_label: Text,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    antigravity_quota_row_with_label(
        metric,
        locale::text(language, period_label).to_owned(),
        theme,
        language,
    )
}

pub(super) fn antigravity_quota_row_with_label(
    metric: &UsageMetric,
    label: String,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let remaining = metric
        .remaining_percent()
        .filter(|remaining| remaining.is_finite());
    let mut children = if let Some(remaining) = remaining {
        vec![percent_line(&label, remaining, theme, language)]
    } else {
        vec![info_line(
            &label,
            locale::text(language, Text::Unavailable),
            theme,
        )]
    };
    if let Some(reset_at) = metric.reset_at_utc {
        children.push(reset_time_label(reset_at, Utc::now(), theme, language));
    }
    column(children).spacing(1).width(Fill).into()
}

pub(super) fn append_spend_summary(
    rows: &mut Vec<Element<'static, Message>>,
    spend: Option<&SpendSnapshot>,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let Some(spend) = spend else {
        return;
    };

    let value = match (spend.monthly_usage, spend.monthly_limit) {
        (Some(usage), Some(limit)) => format!(
            "{} / {}",
            format_amount(usage, None, spend.currency_code.as_deref(), language),
            format_amount(limit, None, spend.currency_code.as_deref(), language),
        ),
        (Some(usage), None) => format_amount(usage, None, spend.currency_code.as_deref(), language),
        _ => String::new(),
    };
    if let Some(remaining) = displayable_spend_percent(spend.remaining_percent()) {
        rows.push(percent_line(&value, remaining, theme, language));
    } else if !value.is_empty() {
        rows.push(compact_values_line(&[value], theme));
    }
}

pub(super) fn append_reset_credit_inventory(
    rows: &mut Vec<Element<'static, Message>>,
    inventory: &UsageCreditInventory,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let mut available_credits = available_reset_credits(inventory);
    available_credits.sort_by_key(|credit| credit.expires_at_utc);

    for credit in &available_credits {
        let label = reset_credit_label(credit, language);
        let expiration = credit.expires_at_utc.map_or_else(
            || locale::text(language, Text::NoExpiryDate).to_owned(),
            |expires_at| credit_expiration_label(expires_at, Utc::now(), language),
        );
        rows.push(reset_credit_info_line(&label, &expiration, theme, language));
    }

    if available_credits.len() < inventory.available_count as usize {
        rows.push(
            text(locale::text(language, Text::ResetExpiryUnavailable))
                .size(typography::METADATA_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
}

pub(super) fn available_reset_credits(inventory: &UsageCreditInventory) -> Vec<&UsageCreditRecord> {
    if inventory.available_count == 0 {
        return Vec::new();
    }

    inventory
        .credits
        .iter()
        .filter(|credit| {
            credit
                .status
                .as_deref()
                .is_none_or(|status| status.eq_ignore_ascii_case("available"))
        })
        .collect::<Vec<_>>()
}

pub(super) fn is_weekly_usage_window(
    window: &RateLimitWindow,
    is_primary: bool,
    primary_window_kind: Option<UsagePrimaryWindowKind>,
) -> bool {
    if is_primary && primary_window_kind == Some(UsagePrimaryWindowKind::Weekly) {
        return true;
    }

    if window.kind != UsageWindowKind::Additional && window.limit_window_seconds >= 6 * 24 * 60 * 60
    {
        return true;
    }

    is_weekly_usage_name(&window.name)
}

pub(super) fn is_additional_weekly_window(window: &RateLimitWindow) -> bool {
    window.kind == UsageWindowKind::Additional
        && (window.limit_window_seconds >= 6 * 24 * 60 * 60
            || window.name.to_ascii_lowercase().contains("weekly"))
}

pub(super) fn is_weekly_usage_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "weekly" | "week" | "7-day" | "7 days" | "secondary"
    )
}

pub(super) fn reset_credit_label(credit: &UsageCreditRecord, language: Language) -> String {
    credit
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            credit
                .reset_type
                .as_deref()
                .filter(|reset_type| !reset_type.trim().is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| locale::text(language, Text::ResetCredit).to_owned())
}

pub(super) fn credit_expiration_label(
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
    language: Language,
) -> String {
    let local_expiration = format_local_reset(expires_at.with_timezone(&Local), language);
    let seconds = (expires_at - now).num_seconds();
    if seconds <= 0 {
        return format!(
            "{} {local_expiration}",
            locale::text(language, Text::Expired)
        );
    }

    if seconds > 24 * 60 * 60 {
        let days = seconds / (24 * 60 * 60);
        let hours = (seconds % (24 * 60 * 60)) / (60 * 60);
        return match language {
            Language::English => format!(
                "{} {local_expiration} · in {days}d {hours}h",
                locale::text(language, Text::Expires)
            ),
            Language::Arabic => format!(
                "{} {local_expiration} · بعد {} و{}",
                locale::text(language, Text::Expires),
                arabic_day_count(days),
                arabic_hour_count(hours)
            ),
        };
    }

    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    match language {
        Language::English => format!(
            "{} {local_expiration} · in {hours}h {}m",
            locale::text(language, Text::Expires),
            minutes.max(1)
        ),
        Language::Arabic => format!(
            "{} {local_expiration} · بعد {hours} س و{} د",
            locale::text(language, Text::Expires),
            minutes.max(1)
        ),
    }
}

pub(super) fn duplicates_rate_window(metric: &UsageMetric, windows: &[&RateLimitWindow]) -> bool {
    windows.iter().any(|window| {
        let same_name = metric.name.trim().eq_ignore_ascii_case(window.name.trim());
        let same_percent = metric
            .used_percent
            .is_some_and(|used| (used - window.used_percent).abs() < 0.01);
        let same_reset = metric.reset_at_utc.as_ref() == window.reset_at_utc.as_ref();
        same_name && same_percent && same_reset
    })
}

pub(super) fn providers_match(account_provider: &str, snapshot_provider: &str) -> bool {
    account_provider == snapshot_provider
        || matches!(
            (account_provider, snapshot_provider),
            ("codex", "openai") | ("openai", "codex")
        )
}

pub(super) fn status_badge(
    status: AccountStatus,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Option<Element<'static, Message>> {
    let (label, color) = match status {
        AccountStatus::Active => return None,
        AccountStatus::NeedsReauthentication => (
            locale::text(language, Text::NeedsSignIn),
            Color::from_rgb8(230, 116, 101),
        ),
        AccountStatus::Paused => (
            locale::text(language, Text::Paused),
            Color::from_rgb8(224, 182, 92),
        ),
        AccountStatus::Disabled => (
            locale::text(language, Text::Disabled),
            Color::from_rgb8(163, 169, 180),
        ),
    };

    Some(
        container(
            text(label)
                .size(typography::METADATA_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text()),
        )
        .padding([4, 7])
        .style(move |_| container::Style {
            background: Some(Background::Color(color.scale_alpha(0.20))),
            border: Border {
                color: color.scale_alpha(0.38),
                width: 1.0,
                radius: 7.0.into(),
            },
            ..Default::default()
        })
        .into(),
    )
}
