//! Shared row widgets, colors, and number/date formatting.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;

pub(super) fn compact_values_line(
    values: &[String],
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    text(values.join(" · "))
        .size(typography::METADATA_SIZE)
        .color(muted_text(theme))
        .width(Fill)
        .into()
}

pub(super) fn info_line(
    label: &str,
    value: &str,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    row![
        text(label.to_owned())
            .size(typography::METADATA_SIZE)
            .color(muted_text(theme)),
        space().width(Fill),
        text(value.to_owned())
            .size(typography::VALUE_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .width(Fill)
    .into()
}

pub(super) fn reset_credit_info_line(
    label: &str,
    value: &str,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let value: Element<'static, Message> =
        if let Some((prefix, countdown)) = countdown_label_parts(value, language) {
            rich_text::<(), Message, iced::Theme, iced::Renderer>([
                span::<(), iced::Font>(prefix.to_owned()).color(muted_text(theme)),
                span::<(), iced::Font>(countdown.to_owned())
                    .font(typography::STRONG)
                    .color(reset_time_accent(theme)),
            ])
            .size(typography::RESET_TIME_SIZE)
            .font(typography::MEDIUM)
            .into()
        } else {
            text(value.to_owned())
                .size(typography::RESET_TIME_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.text())
                .into()
        };

    row![
        text(label.to_owned())
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
        space().width(Fill),
        value,
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .width(Fill)
    .into()
}

pub(super) fn rate_window_row(
    window: &RateLimitWindow,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let remaining = window.remaining_percent();
    let mut children: Vec<Element<'static, Message>> = vec![percent_line(
        &display_window_name(&window.name, language),
        remaining,
        theme,
        language,
    )];
    if let Some(reset_at) = window.reset_at_utc {
        children.push(reset_time_label(reset_at, Utc::now(), theme, language));
    }
    column(children).spacing(1).width(Fill).into()
}

pub(super) fn metric_row(
    metric: &UsageMetric,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    if let Some(remaining) = metric.remaining_percent() {
        let mut children = vec![percent_line(&metric.name, remaining, theme, language)];
        if let Some(reset_at) = metric.reset_at_utc {
            children.push(reset_time_label(reset_at, Utc::now(), theme, language));
        }
        return column(children).spacing(1).width(Fill).into();
    }

    let value = if let Some(remaining) = metric.remaining_amount {
        Some(format_amount(
            remaining,
            metric.unit.as_deref(),
            None,
            language,
        ))
    } else if let Some(used) = metric.used_amount {
        Some(format_amount(used, metric.unit.as_deref(), None, language))
    } else {
        metric
            .reset_at_utc
            .map(|reset| format_local_reset(reset.with_timezone(&Local), language))
    };

    if let Some(value) = value {
        info_line(&metric.name, &value, theme)
    } else {
        text(metric.name.clone())
            .size(typography::METADATA_SIZE)
            .color(muted_text(theme))
            .into()
    }
}

pub(super) fn percent_line(
    label: &str,
    remaining: f64,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let Some(remaining) = valid_percent(remaining) else {
        return info_line(
            label,
            locale::text(language, Text::InvalidPercentage),
            theme,
        );
    };
    let accent = usage_color(remaining, theme);
    let shown = crate::percent_display::current().displayed(remaining);
    column![
        row![
            text(label.to_owned())
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text()),
            space().width(Fill),
            text(format!("{shown:.0}%"))
                .size(typography::PERCENTAGE_SIZE)
                .font(typography::STRONG)
                .color(accent),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .width(Fill),
        progress_bar(0.0..=100.0, shown as f32)
            .girth(5)
            .style(move |_| progress_bar::Style {
                // The empty part of the bar must stay visible on white.
                background: Background::Color(theme.colors.border(if theme.colors.is_light {
                    0.26
                } else {
                    0.16
                })),
                bar: Background::Color(accent),
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
            }),
    ]
    .spacing(4)
    .width(Fill)
    .into()
}

pub(super) fn warning_line(
    message: &str,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    let (text_color, background_color, border_color) = if theme.colors.is_light {
        (
            Color::from_rgb8(146, 64, 14),
            Color::from_rgb8(255, 248, 230),
            Color::from_rgb8(224, 190, 127),
        )
    } else {
        (
            Color::from_rgb8(235, 190, 114),
            Color::from_rgba(0.62, 0.36, 0.10, 0.18),
            Color::from_rgba(0.83, 0.57, 0.24, 0.25),
        )
    };
    container(
        text(message.to_owned())
            .size(typography::LABEL_SIZE)
            .font(typography::MEDIUM)
            .color(text_color),
    )
    .width(Fill)
    .padding([5, 7])
    .style(move |_| container::Style {
        background: Some(Background::Color(background_color)),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    })
    .into()
}

pub(super) fn centered_note(
    message: &str,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    container(
        text(message.to_owned())
            .size(typography::BODY_SIZE)
            .color(muted_text(theme)),
    )
    .width(Fill)
    .height(Fill)
    .center(Fill)
    .style(move |_| container::Style {
        text_color: Some(theme.colors.text()),
        ..Default::default()
    })
    .into()
}

pub(super) fn muted_text(theme: &'static crate::theme::ThemeDefinition) -> Color {
    theme.colors.muted_text()
}

pub(super) fn valid_percent(value: f64) -> Option<f64> {
    if value.is_finite() {
        Some(value.clamp(0.0, 100.0))
    } else {
        None
    }
}

pub(super) fn displayable_spend_percent(remaining: Option<f64>) -> Option<f64> {
    remaining.filter(|value| !value.is_finite() || format!("{value:.0}") != "0")
}

pub(super) fn usage_color(remaining: f64, theme: &'static crate::theme::ThemeDefinition) -> Color {
    if theme.colors.is_light {
        if remaining <= 15.0 {
            Color::from_rgb8(176, 48, 43)
        } else if remaining <= 40.0 {
            Color::from_rgb8(180, 83, 9)
        } else {
            Color::from_rgb8(27, 116, 69)
        }
    } else if remaining <= 15.0 {
        Color::from_rgb8(237, 119, 105)
    } else if remaining <= 40.0 {
        Color::from_rgb8(229, 190, 102)
    } else {
        Color::from_rgb8(139, 205, 164)
    }
}

pub(super) fn format_amount(
    amount: f64,
    unit: Option<&str>,
    currency: Option<&str>,
    language: Language,
) -> String {
    if !amount.is_finite() {
        return locale::text(language, Text::Unavailable).to_owned();
    }
    let amount = if amount.abs() >= 100.0 {
        format!("{amount:.0}")
    } else {
        format!("{amount:.2}")
    };

    if let Some(currency) = currency {
        return match currency_symbol(currency) {
            Some(symbol) => format!("{symbol}{amount}"),
            None => format!("{amount} {currency}"),
        };
    }
    // A metric whose unit is a currency code reads as money too.
    if let Some(symbol) = unit.and_then(currency_symbol) {
        return format!("{symbol}{amount}");
    }

    match unit {
        Some("tokens") => format!("{amount} token"),
        Some(unit) => format!("{amount} {unit}"),
        None => amount,
    }
}

fn currency_symbol(code: &str) -> Option<&'static str> {
    match code.trim().to_ascii_uppercase().as_str() {
        "USD" => Some("$"),
        "EUR" => Some("€"),
        "GBP" => Some("£"),
        "JPY" | "CNY" => Some("¥"),
        _ => None,
    }
}

pub(super) fn display_window_name(name: &str, language: Language) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "primary" | "session" | "5-hour" | "5 hours" | "five-hour" => {
            locale::text(language, Text::FiveHourLimit).to_owned()
        }
        "secondary" | "weekly" | "7-day" | "7 days" | "week" => {
            locale::text(language, Text::Weekly).to_owned()
        }
        "daily" | "day" => locale::text(language, Text::Daily).to_owned(),
        "monthly" | "month" => locale::text(language, Text::Monthly).to_owned(),
        _ => name.to_owned(),
    }
}

pub(super) fn format_local_reset(reset_at: DateTime<Local>, language: Language) -> String {
    match language {
        Language::English => reset_at.format("%b %-d, %Y at %-I:%M %p").to_string(),
        Language::Arabic => reset_at.format("%Y-%m-%d %H:%M").to_string(),
    }
}

pub(super) fn reset_time_label(
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let label = reset_label(reset_at, now, language);
    // On the shaded pill of an image theme, secondary grey would read dim.
    let prefix_color = if theme.backdrop.is_some() {
        theme.colors.text()
    } else {
        muted_text(theme)
    };
    let label: Element<'static, Message> =
        if let Some((prefix, countdown)) = countdown_label_parts(&label, language) {
            rich_text::<(), Message, iced::Theme, iced::Renderer>([
                span::<(), iced::Font>(prefix.to_owned()).color(prefix_color),
                span::<(), iced::Font>(countdown.to_owned())
                    .font(typography::STRONG)
                    .color(reset_time_accent(theme)),
            ])
            .size(typography::RESET_TIME_SIZE)
            .font(typography::MEDIUM)
            .into()
        } else {
            text(label)
                .size(typography::RESET_TIME_SIZE)
                .font(typography::STRONG)
                .color(prefix_color)
                .into()
        };
    container(shade_over_backdrop(label, theme))
        .width(Fill)
        .into()
}

/// On themes with a background image, lays a light shade behind small text
/// so bright parts of the picture cannot wash it out. Plain themes are
/// left as they are.
fn shade_over_backdrop(
    content: Element<'static, Message>,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    if theme.backdrop.is_none() {
        return content;
    }
    container(content)
        .padding([1, 6])
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.48))),
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..Default::default()
        })
        .into()
}

pub(super) fn countdown_label_parts(label: &str, language: Language) -> Option<(&str, &str)> {
    let countdown_word = match language {
        Language::English => "in ",
        Language::Arabic => "بعد ",
    };

    if let Some((_, countdown)) = label.split_once(" · ")
        && countdown.starts_with(countdown_word)
    {
        let prefix_end = label.len() - countdown.len();
        return Some((&label[..prefix_end], &label[prefix_end..]));
    }

    let immediate_reset_prefix = match language {
        Language::English => "Resets ",
        Language::Arabic => "يتجدد ",
    };
    let countdown = label.strip_prefix(immediate_reset_prefix)?;
    countdown
        .starts_with(countdown_word)
        .then_some((immediate_reset_prefix, countdown))
}

pub(super) fn reset_time_accent(theme: &'static crate::theme::ThemeDefinition) -> Color {
    if theme.colors.is_light {
        Color::from_rgb8(194, 82, 0)
    } else {
        Color::from_rgb8(246, 183, 83)
    }
}

/// Says the reading could not be updated and when it was taken.
pub(super) fn stale_label(
    observed_at: DateTime<Utc>,
    now: DateTime<Utc>,
    language: Language,
) -> String {
    let observed = observed_at.with_timezone(&Local);
    let today = observed.date_naive() == now.with_timezone(&Local).date_naive();
    let when = match (language, today) {
        (Language::English, true) => observed.format("%-I:%M %p").to_string(),
        (Language::Arabic, true) => observed.format("%H:%M").to_string(),
        _ => format_local_reset(observed, language),
    };
    match language {
        Language::English => format!("Couldn't update · showing the reading from {when}"),
        Language::Arabic => format!("تعذّر التحديث · هذه القراءة من {when}"),
    }
}

pub(super) fn reset_label(
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
    language: Language,
) -> String {
    let seconds = (reset_at - now).num_seconds();
    if seconds <= 0 {
        return locale::text(language, Text::ResetReached).to_owned();
    }

    let local_reset = format_local_reset(reset_at.with_timezone(&Local), language);
    if seconds > 24 * 60 * 60 {
        let days = seconds / (24 * 60 * 60);
        let hours = (seconds % (24 * 60 * 60)) / (60 * 60);
        return match language {
            Language::English => format!("Resets on {local_reset} · in {days}d {hours}h"),
            Language::Arabic => format!(
                "يتجدد في {local_reset} · بعد {} و{}",
                arabic_day_count(days),
                arabic_hour_count(hours)
            ),
        };
    }

    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let countdown = match language {
        Language::English if hours > 0 => format!("in {hours}h {minutes}m"),
        Language::English => format!("in {}m", minutes.max(1)),
        Language::Arabic if hours > 0 => format!("بعد {hours} س و{minutes} د"),
        Language::Arabic => format!("بعد {} د", minutes.max(1)),
    };
    let local_reset = reset_at.with_timezone(&Local);
    let tomorrow = local_reset.date_naive() != now.with_timezone(&Local).date_naive();
    match language {
        Language::English => {
            let day = if tomorrow { "tomorrow " } else { "" };
            format!(
                "Resets {day}at {} · {countdown}",
                local_reset.format("%-I:%M %p")
            )
        }
        Language::Arabic => {
            let day = if tomorrow { "غدًا " } else { "" };
            format!(
                "يتجدد {day}الساعة {} · {countdown}",
                local_reset.format("%H:%M")
            )
        }
    }
}

pub(super) fn arabic_day_count(days: i64) -> String {
    match days {
        1 => "يوم".to_owned(),
        2 => "يومين".to_owned(),
        3..=10 => format!("{days} أيام"),
        _ => format!("{days} يوم"),
    }
}

pub(super) fn arabic_hour_count(hours: i64) -> String {
    match hours {
        1 => "ساعة".to_owned(),
        2 => "ساعتين".to_owned(),
        3..=10 => format!("{hours} ساعات"),
        _ => format!("{hours} ساعة"),
    }
}
