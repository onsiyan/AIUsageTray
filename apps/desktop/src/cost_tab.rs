//! The Cost tab: what Codex and Claude Code usage on this PC would cost at
//! API list prices, read from the tools' local logs.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;
use usage_monitor_core::cost::{self, CostReport, CostTool, DayCost, LogRoots, ToolCost};

/// A report on screen is read again after this long.
const RESCAN_AFTER: Duration = Duration::from_secs(5 * 60);
const MODELS_SHOWN: usize = 4;
const CHART_HEIGHT: f32 = 46.0;

#[derive(Default)]
pub(super) struct CostTab {
    report: Option<CostReport>,
    scanning: bool,
    last_started: Option<Instant>,
    error: Option<String>,
}

impl CostTab {
    /// Reads the logs if nothing is being read and the last report is old.
    pub(super) fn scan_if_due(&mut self) -> Task<Message> {
        let due = self
            .last_started
            .is_none_or(|started| started.elapsed() >= RESCAN_AFTER);
        if self.scanning || !due {
            return Task::none();
        }
        self.scan()
    }

    /// Reads the logs now, unless a read is already running.
    pub(super) fn scan(&mut self) -> Task<Message> {
        if self.scanning {
            return Task::none();
        }
        self.scanning = true;
        self.last_started = Some(Instant::now());
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = sender.send_blocking(read_report());
        });
        Task::perform(
            async move {
                receiver
                    .recv()
                    .await
                    .unwrap_or_else(|_| Err("the cost reader stopped".to_owned()))
            },
            |result| Message::CostScanned(Box::new(result)),
        )
    }

    pub(super) fn finish(&mut self, result: Result<CostReport, String>) {
        self.scanning = false;
        match result {
            Ok(report) => {
                self.report = Some(report);
                self.error = None;
            }
            Err(error) => {
                preview_log(format!("cost scan failed: {error}"));
                self.error = Some(error);
            }
        }
    }
}

/// Updates prices once a day, then reads what is new in the logs. Runs on
/// its own thread: the first read of large logs takes a few seconds.
fn read_report() -> Result<CostReport, String> {
    let cache_directory = cost::default_cache_directory();
    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        && let Err(error) = runtime.block_on(cost::refresh_prices_if_stale(&cache_directory))
    {
        preview_log(format!("price update failed: {error}"));
    }
    Ok(cost::scan_report(
        &LogRoots::from_environment(),
        &cache_directory,
    ))
}

pub(super) fn view(
    state: &CostTab,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let Some(report) = &state.report else {
        let note = if state.error.is_some() && !state.scanning {
            locale::Text::CostFailed
        } else {
            locale::Text::CostReading
        };
        return container(
            text(locale::text(language, note))
                .size(typography::BODY_SIZE)
                .color(theme.colors.muted_text()),
        )
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .into();
    };

    let mut sections = Vec::new();
    for tool in CostTool::ALL {
        if let Some(cost) = report.tool(tool) {
            sections.push(tool_card(cost, theme, language));
        }
    }
    sections.push(footer(report, state.scanning, theme, language));

    let content = crate::smooth_scroll::smooth_scroll(
        DashboardTab::Cost.scroll_key(),
        scrollable(column(sections).spacing(10).width(Fill))
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(Fill),
    );
    container(content)
        .padding([10, 10])
        .width(Fill)
        .height(Fill)
        .into()
}

fn tool_card(
    cost: &ToolCost,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let provider = match cost.tool {
        CostTool::Codex => UsageProvider::Codex,
        CostTool::Claude => UsageProvider::Claude,
    };
    let header = row![
        image(provider_logo_handle(provider, theme.colors.is_light))
            .width(20)
            .height(20)
            .content_fit(ContentFit::Contain),
        text(cost.tool.label())
            .size(typography::ACCOUNT_NAME_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text())
            .width(Fill),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut body = vec![header.into()];
    if !cost.logs_found {
        body.push(muted_line(
            locale::text(language, locale::Text::CostNoLogs),
            theme,
        ));
    } else if !cost.has_usage() {
        body.push(muted_line(
            locale::text(language, locale::Text::CostNoUsage),
            theme,
        ));
    } else {
        body.push(
            row![
                period_tile(locale::Text::CostToday, &cost.last(1), theme, language),
                period_tile(locale::Text::CostLast7, &cost.last(7), theme, language),
                period_tile(
                    locale::Text::CostLast30,
                    &cost.last(cost.days.len()),
                    theme,
                    language
                ),
            ]
            .spacing(6)
            .into(),
        );
        body.push(daily_chart(&cost.days, theme));
        body.push(models_list(cost, theme, language));
    }

    container(column(body).spacing(10))
        .width(Fill)
        .padding(12)
        .style(move |_| card_style(theme))
        .into()
}

fn card_style(theme: &'static ThemeDefinition) -> container::Style {
    container::Style {
        background: Some(Background::Color(if theme.colors.is_light {
            Color::from_rgba(1.0, 1.0, 1.0, 0.55)
        } else {
            Color::from_rgba(0.0, 0.0, 0.0, 0.62)
        })),
        border: Border {
            color: theme.colors.border(0.35),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

/// One period's cost, with its tokens under it.
fn period_tile(
    label: locale::Text,
    total: &DayCost,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let tokens = format!(
        "{} {}",
        format_tokens(total.tokens.total()),
        locale::text(language, locale::Text::CostTokens)
    );
    container(
        column![
            text(locale::text(language, label))
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.muted_text()),
            text(format_dollars(total.cost_usd))
                .size(typography::PERCENTAGE_SIZE + 2.0)
                .font(typography::STRONG)
                .color(theme.colors.text()),
            text(tokens)
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.muted_text()),
        ]
        .spacing(1),
    )
    .width(Fill)
    .padding([6, 8])
    .style(move |_| container::Style {
        background: Some(Background::Color(if theme.colors.is_light {
            theme.colors.hover()
        } else {
            Color::from_rgba(1.0, 1.0, 1.0, 0.07)
        })),
        border: Border {
            radius: 7.0.into(),
            ..Border::default()
        },
        ..Default::default()
    })
    .into()
}

/// A bar for each day, today on the right.
fn daily_chart(days: &[DayCost], theme: &'static ThemeDefinition) -> Element<'static, Message> {
    let highest = days.iter().map(|day| day.cost_usd).fold(0.0_f64, f64::max);
    // The green of the usage bars, so the chart reads the same in every theme.
    let accent = if theme.colors.is_light {
        Color::from_rgb8(27, 116, 69)
    } else {
        Color::from_rgb8(139, 205, 164)
    };
    let last = days.len().saturating_sub(1);
    let bars = days.iter().enumerate().map(|(index, day)| {
        let share = if highest > 0.0 {
            (day.cost_usd / highest) as f32
        } else {
            0.0
        };
        let height = if day.cost_usd > 0.0 {
            (share * CHART_HEIGHT).max(3.0)
        } else {
            2.0
        };
        let color = if day.cost_usd <= 0.0 {
            theme.colors.border(0.5)
        } else if index == last {
            accent
        } else {
            Color { a: 0.55, ..accent }
        };
        container(Space::new().width(Fill).height(height))
            .width(Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(color)),
                border: Border {
                    radius: 2.0.into(),
                    ..Border::default()
                },
                ..Default::default()
            })
            .into()
    });
    container(row(bars).spacing(2).align_y(Alignment::End))
        .width(Fill)
        .height(CHART_HEIGHT)
        .align_y(Alignment::End)
        .into()
}

fn models_list(
    cost: &ToolCost,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let mut lines = vec![
        text(locale::text(language, locale::Text::CostModels))
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.muted_text())
            .into(),
    ];
    for model in cost.models.iter().take(MODELS_SHOWN) {
        let price = model.cost_usd.map_or_else(
            || locale::text(language, locale::Text::CostUnpriced).to_owned(),
            format_dollars,
        );
        lines.push(
            row![
                text(model.model.clone())
                    .size(typography::LABEL_SIZE)
                    .font(typography::MEDIUM)
                    .color(theme.colors.text())
                    .width(Fill),
                text(format_tokens(model.tokens.total()))
                    .size(typography::METADATA_SIZE)
                    .font(typography::MEDIUM)
                    .color(theme.colors.muted_text()),
                text(price)
                    .size(typography::LABEL_SIZE)
                    .font(typography::EMPHASIS)
                    .color(theme.colors.text())
                    .width(Length::Fixed(72.0))
                    .align_x(iced::alignment::Horizontal::Right),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    column(lines).spacing(4).into()
}

fn footer(
    report: &CostReport,
    scanning: bool,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let updated = report
        .prices
        .updated_at
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    let prices = match (language, report.prices.downloaded) {
        (locale::Language::English, true) => format!("Prices from models.dev, updated {updated}."),
        (locale::Language::English, false) => format!("Built-in prices from {updated}."),
        (locale::Language::Arabic, true) => format!("الأسعار من models.dev، حُدّثت {updated}."),
        (locale::Language::Arabic, false) => format!("أسعار مدمجة بتاريخ {updated}."),
    };
    let mut lines = vec![
        muted_line(locale::text(language, locale::Text::CostNote), theme),
        muted_line(&prices, theme),
    ];
    if scanning {
        lines.push(muted_line(
            locale::text(language, locale::Text::CostReading),
            theme,
        ));
    }
    // On image themes, a shade keeps the small print off the picture.
    let shaded = theme.backdrop.is_some();
    container(column(lines).spacing(2))
        .width(Fill)
        .padding([6, 8])
        .style(move |_| container::Style {
            background: shaded.then(|| Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.5))),
            border: Border {
                radius: 7.0.into(),
                ..Border::default()
            },
            ..Default::default()
        })
        .into()
}

fn muted_line(message: &str, theme: &'static ThemeDefinition) -> Element<'static, Message> {
    text(message.to_owned())
        .size(typography::METADATA_SIZE)
        .font(typography::MEDIUM)
        .color(theme.colors.muted_text())
        .into()
}

/// `$0.12`, `$11.01`, `$1,205`.
pub(super) fn format_dollars(amount: f64) -> String {
    if !amount.is_finite() || amount <= 0.0 {
        return "$0".to_owned();
    }
    if amount < 1_000.0 {
        return format!("${amount:.2}");
    }
    let whole = format!("{:.0}", amount.round());
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("${grouped}")
}

/// `820`, `5.2K`, `131M`, `1.47B`.
pub(super) fn format_tokens(tokens: u64) -> String {
    let value = tokens as f64;
    let (scaled, suffix) = if tokens >= 1_000_000_000 {
        (value / 1e9, "B")
    } else if tokens >= 1_000_000 {
        (value / 1e6, "M")
    } else if tokens >= 1_000 {
        (value / 1e3, "K")
    } else {
        return tokens.to_string();
    };
    let digits = if scaled >= 100.0 {
        0
    } else if scaled >= 10.0 {
        1
    } else {
        2
    };
    format!("{scaled:.digits$}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::{format_dollars, format_tokens};

    #[test]
    fn amounts_and_token_counts_read_short() {
        assert_eq!(format_dollars(0.0), "$0");
        assert_eq!(format_dollars(0.123), "$0.12");
        assert_eq!(format_dollars(999.994), "$999.99");
        assert_eq!(format_dollars(1_204.59), "$1,205");
        assert_eq!(format_dollars(1_234_567.0), "$1,234,567");
        assert_eq!(format_tokens(820), "820");
        assert_eq!(format_tokens(5_157_518), "5.16M");
        assert_eq!(format_tokens(131_054_244), "131M");
        assert_eq!(format_tokens(1_466_920_434), "1.47B");
    }
}
