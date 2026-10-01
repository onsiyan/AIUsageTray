//! Per-model quota selection, grouping into families, and quota tiles.

use super::*;

pub(super) fn model_quota_menu_entries(metrics: &[UsageMetric]) -> Vec<(String, String)> {
    let mut families = BTreeMap::new();
    metrics
        .iter()
        .filter(|metric| is_model_quota_metric(metric))
        .for_each(|metric| {
            let family_id = model_quota_family_id(model_quota_id(metric));
            families.entry(family_id.clone()).or_insert_with(|| {
                (
                    family_id.clone(),
                    model_family_display_name(&family_id, &[(*metric).clone()]),
                )
            });
        });
    families.into_values().collect()
}

pub(super) fn has_hidden_model_quota(
    metrics: &[UsageMetric],
    model_visibility: &ModelVisibilityPreferences,
) -> bool {
    metrics
        .iter()
        .filter(|metric| is_model_quota_metric(metric))
        .any(|metric| {
            let family_id = model_quota_family_id(model_quota_id(metric));
            !model_visibility.is_visible(&family_id)
        })
}

pub(super) fn is_model_quota_metric(metric: &UsageMetric) -> bool {
    metric
        .metadata
        .get("source")
        .is_some_and(|source| source == "model-quota")
}

pub(super) fn model_quota_id(metric: &UsageMetric) -> &str {
    metric
        .metadata
        .get("model_id")
        .map(String::as_str)
        .unwrap_or(&metric.key)
}

pub(super) fn normalize_model_id(model_id: &str) -> String {
    model_id
        .trim()
        .strip_prefix("models/")
        .or_else(|| model_id.trim().strip_prefix("Models/"))
        .unwrap_or(model_id.trim())
        .to_ascii_lowercase()
}

pub(super) fn visible_model_quota_metrics(
    metrics: &[&UsageMetric],
    model_visibility: &ModelVisibilityPreferences,
    show_all: bool,
) -> Vec<UsageMetric> {
    let mut by_family = BTreeMap::<String, Vec<UsageMetric>>::new();
    for metric in metrics {
        let family_id = model_quota_family_id(model_quota_id(metric));
        by_family
            .entry(family_id)
            .or_default()
            .push((*metric).clone());
    }

    let mut visible = Vec::new();
    for (family_id, entries) in by_family {
        let is_pinned = model_visibility.is_visible(&family_id)
            || entries
                .iter()
                .any(|metric| model_visibility.is_visible(model_quota_id(metric)));
        if show_all || is_pinned {
            visible.push(aggregate_model_family(&family_id, &entries));
        }
    }
    sort_model_metrics(&mut visible);
    visible
}

pub(super) fn aggregate_model_family(family_id: &str, entries: &[UsageMetric]) -> UsageMetric {
    let mut aggregate = entries[0].clone();
    aggregate.name = model_family_display_name(family_id, entries);
    if let Some(min_remaining) = entries
        .iter()
        .filter_map(UsageMetric::remaining_percent)
        .min_by(f64::total_cmp)
    {
        let used_percent = 100.0 - min_remaining;
        aggregate.used_percent = Some(used_percent);
        aggregate.used_amount = Some(used_percent);
        aggregate.limit_amount = Some(100.0);
        aggregate.remaining_amount = Some(min_remaining);
        aggregate.unit = Some("percent".to_owned());
    } else {
        aggregate.used_percent = None;
        aggregate.used_amount = None;
        aggregate.limit_amount = None;
        aggregate.remaining_amount = None;
        aggregate.unit = None;
    }
    aggregate.reset_at_utc = entries
        .iter()
        .filter_map(|metric| metric.reset_at_utc.as_ref().cloned())
        .min();
    aggregate
}

pub(super) fn sort_model_metrics(metrics: &mut [UsageMetric]) {
    metrics.sort_by(|left, right| {
        right
            .remaining_percent()
            .unwrap_or(-1.0)
            .total_cmp(&left.remaining_percent().unwrap_or(-1.0))
            .then_with(|| left.name.cmp(&right.name))
    });
}

pub(super) fn split_thinking_level_suffix(model_id: &str) -> Option<(&str, &str)> {
    ["extra-low", "minimal", "low", "medium", "high", "tiered"]
        .into_iter()
        .find_map(|level| {
            let suffix = format!("-{level}");
            model_id
                .strip_suffix(&suffix)
                .map(|base_id| (base_id, level))
        })
}

pub(super) fn model_quota_family_id(model_id: &str) -> String {
    let normalized_id = normalize_model_id(model_id);
    if let Some((base_id, _)) = split_thinking_level_suffix(&normalized_id) {
        base_id.to_owned()
    } else if normalized_id.ends_with("-low/high") {
        normalized_id
            .strip_suffix("-low/high")
            .unwrap_or(&normalized_id)
            .to_owned()
    } else if normalized_id.starts_with("claude-") && normalized_id.ends_with("-thinking") {
        normalized_id
            .strip_suffix("-thinking")
            .unwrap_or(&normalized_id)
            .to_owned()
    } else {
        normalized_id
    }
}

pub(super) fn family_display_name(family_id: &str) -> Option<&'static str> {
    match family_id {
        "gemini-3.1-pro" => Some("Gemini 3.1 Pro"),
        "gemini-3.7-flash" => Some("Gemini 3.7 Flash"),
        "gemini-3.5-flash" => Some("Gemini 3.5 Flash"),
        "gemini-3-flash" => Some("Gemini 3 Flash"),
        "gemini-3.1-flash-image" => Some("Gemini 3.1 Flash Image"),
        "claude-sonnet-4-6" => Some("Claude Sonnet 4.6"),
        "claude-opus-4-6" => Some("Claude Opus 4.6"),
        "claude-opus-4-5" => Some("Claude Opus 4.5"),
        "gpt-oss-120b" => Some("GPT OSS 120B"),
        _ => None,
    }
}

pub(super) fn model_family_display_name(family_id: &str, entries: &[UsageMetric]) -> String {
    if let Some(name) = family_display_name(family_id) {
        return name.to_owned();
    }
    let normalized = normalize_model_id(family_id);
    if let Some(rest) = normalized.strip_prefix("claude-") {
        let parts = rest.split('-').collect::<Vec<_>>();
        if parts.len() >= 3
            && parts[parts.len() - 2].chars().all(|ch| ch.is_ascii_digit())
            && parts[parts.len() - 1].chars().all(|ch| ch.is_ascii_digit())
        {
            let family = parts[..parts.len() - 2]
                .iter()
                .map(|part| capitalize_first(part))
                .collect::<Vec<_>>()
                .join(" ");
            return format!(
                "Claude {family} {}.{}",
                parts[parts.len() - 2],
                parts[parts.len() - 1]
            );
        }
    }
    let base_name = entries
        .iter()
        .find(|metric| {
            let id = normalize_model_id(model_quota_id(metric));
            split_thinking_level_suffix(&id).is_none() && !id.ends_with("-thinking")
        })
        .or_else(|| entries.first())
        .map(|metric| metric.name.trim())
        .unwrap_or(family_id);
    let lower_name = base_name.to_ascii_lowercase();
    [
        " low/high",
        " extra-low",
        " minimal",
        " low",
        " medium",
        " high",
        " tiered",
    ]
    .iter()
    .find_map(|suffix| {
        lower_name.ends_with(suffix).then(|| {
            base_name[..base_name.len() - suffix.len()]
                .trim()
                .to_owned()
        })
    })
    .unwrap_or_else(|| base_name.to_owned())
}

pub(super) fn is_default_pinned_model_family(family_id: &str) -> bool {
    matches!(
        family_id,
        "gemini-3.1-pro" | "gemini-3.1-flash-image" | "gemini-3-flash" | "claude-opus-4-6"
    )
}

pub(super) fn append_model_quota_metrics(
    rows: &mut Vec<Element<'static, Message>>,
    metrics: Vec<UsageMetric>,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    for pair in metrics.chunks(2) {
        let mut quota_row = row![].spacing(6).width(Fill);
        for metric in pair {
            quota_row = quota_row.push(model_quota_tile(metric, theme, language));
        }
        if pair.len() == 1 {
            quota_row = quota_row.push(space().width(Fill));
        }
        rows.push(quota_row.into());
    }
}

pub(super) fn model_quota_tile(
    metric: &UsageMetric,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let remaining = metric.remaining_percent().and_then(valid_percent);
    let accent = remaining
        .map(|remaining| usage_color(remaining, theme))
        .unwrap_or_else(|| muted_text(theme));
    let mut tile_rows = vec![
        row![
            text(metric.name.clone())
                .size(typography::COMPACT_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .width(Fill),
            text(remaining.map_or_else(
                || locale::text(language, Text::Unavailable).to_owned(),
                |value| format!("{:.0}%", crate::percent_display::current().displayed(value))
            ))
            .size(typography::COMPACT_SIZE)
            .font(typography::STRONG)
            .color(accent),
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .width(Fill)
        .into(),
    ];
    if let Some(remaining) = remaining {
        let shown = crate::percent_display::current().displayed(remaining);
        tile_rows.push(
            progress_bar(0.0..=100.0, shown as f32)
                .girth(4)
                .style(move |_| progress_bar::Style {
                    background: Background::Color(theme.colors.border(0.16)),
                    bar: Background::Color(accent),
                    border: Border {
                        radius: 4.0.into(),
                        ..Border::default()
                    },
                })
                .into(),
        );
    }
    if let Some(reset_at) = metric.reset_at_utc {
        tile_rows.push(
            text(short_reset_countdown(reset_at, Utc::now(), language))
                .size(typography::COMPACT_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
    container(column(tile_rows).spacing(3).width(Fill))
        .padding([5, 6])
        .width(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(accent.scale_alpha(0.16))),
            border: Border {
                color: accent.scale_alpha(0.25),
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .into()
}

pub(super) fn short_reset_countdown(
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
    language: Language,
) -> String {
    let seconds = (reset_at - now).num_seconds();
    if seconds <= 0 {
        return locale::text(language, Text::ResetReached).to_owned();
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    match language {
        Language::English if hours >= 24 => format!("{}d {}h", hours / 24, hours % 24),
        Language::English if hours > 0 => format!("{hours}h {minutes}m"),
        Language::English => format!("{}m", minutes.max(1)),
        Language::Arabic if hours >= 24 => format!("{}ي {}س", hours / 24, hours % 24),
        Language::Arabic if hours > 0 => format!("{hours}س {minutes}د"),
        Language::Arabic => format!("{}د", minutes.max(1)),
    }
}
