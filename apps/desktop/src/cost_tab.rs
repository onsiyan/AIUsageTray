//! The Cost page: what Codex and Claude Code usage would cost at API list
//! prices, read from this PC's logs and from other machines' over SSH.
//!
//! The page leads with what the user gets from their subscriptions: the API
//! value of the period against what their plans cost for it. Below come each
//! tool's share, the machines, the days, the token classes, and a table by
//! model.

mod machines;
mod plans;

pub(super) use machines::{MachineChange, Scope, SyncResults};

use std::collections::BTreeMap;

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::{column, stack};
use plans::Plan;
use usage_monitor_core::cost::{self, CostReport, CostTool, DayCost, LogRoots};

use crate::dashboard::AccountUsageEntry;

/// A report on screen is read again after this long.
const RESCAN_AFTER: Duration = Duration::from_secs(5 * 60);
const CHART_HEIGHT: f32 = 72.0;
/// Width of the number columns in the table.
const NUMBER_COLUMN: f32 = 62.0;

/// The days the page totals: today, a week, or the whole report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Period {
    Today,
    Week,
    #[default]
    Month,
}

impl Period {
    fn days(self) -> usize {
        match self {
            Self::Today => 1,
            Self::Week => 7,
            Self::Month => cost::REPORT_DAYS as usize,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum CostView {
    Period(Period),
    /// The chart day under the pointer, counted from the chart's first day.
    HoverDay(Option<usize>),
    /// Start editing a plan's price, with the text to start from, or stop.
    EditPlan(Option<(String, String)>),
    PlanDraft(String),
    SavePlan,
    Machine(machines::MachineChange),
    /// Count only this machine, or every machine.
    Scope(Scope),
    /// The machine row under the pointer.
    HoverMachine(Option<Scope>),
}

/// The price field, focused when editing starts.
const PLAN_PRICE_INPUT: &str = "cost-plan-price";

#[derive(Default)]
pub(super) struct CostTab {
    report: Option<CostReport>,
    scanning: bool,
    last_started: Option<Instant>,
    error: Option<String>,
    period: Period,
    hovered_day: Option<usize>,
    /// Monthly prices the user set, by plan key.
    plan_prices: BTreeMap<String, f64>,
    /// The plan whose price is being typed, and the text so far.
    editing_plan: Option<(String, String)>,
    /// A read was asked for while one was running: read again after it.
    rescan: bool,
    machines: machines::Machines,
    scope: Scope,
    /// The report narrowed to `scope`; `None` while every machine counts.
    shown: Option<CostReport>,
    hovered_machine: Option<Scope>,
}

impl CostTab {
    pub(super) fn load() -> Self {
        Self {
            plan_prices: plans::load_prices(),
            machines: machines::Machines::load(),
            ..Self::default()
        }
    }

    /// Reads the other machines if they were last read `every` ago.
    pub(super) fn sync_machines_if_due(&mut self, open: bool) -> Task<Message> {
        self.machines.sync_if_due(if open {
            machines::LIVE_SYNC
        } else {
            machines::BACKGROUND_SYNC
        })
    }

    pub(super) fn sync_machines(&mut self) -> Task<Message> {
        self.machines.sync()
    }

    /// Notes the machines' readings and builds the report again with them.
    pub(super) fn finish_machine_sync(&mut self, results: SyncResults) -> Task<Message> {
        let again = self.machines.finish_sync(results);
        let scan = self.scan();
        if again {
            Task::batch([scan, self.machines.sync()])
        } else {
            scan
        }
    }

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
            self.rescan = true;
            return Task::none();
        }
        self.scanning = true;
        self.last_started = Some(Instant::now());
        let machines = self.machines.names();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = sender.send_blocking(read_report(&machines));
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

    pub(super) fn finish(&mut self, result: Result<CostReport, String>) -> Task<Message> {
        self.scanning = false;
        match result {
            Ok(report) => {
                self.report = Some(report);
                self.error = None;
                self.rescope();
            }
            Err(error) => {
                preview_log(format!("cost scan failed: {error}"));
                self.error = Some(error);
            }
        }
        if std::mem::take(&mut self.rescan) {
            self.scan()
        } else {
            Task::none()
        }
    }

    /// Narrows the report to the scope, or goes back to every machine when
    /// the scope's machine is no longer in it.
    fn rescope(&mut self) {
        self.shown = None;
        let (Some(report), Some(name)) = (&self.report, self.scope.machine()) else {
            return;
        };
        match report
            .machines
            .iter()
            .find(|machine| machine.name.as_deref() == name)
        {
            Some(machine) => {
                let mut shown = report.clone();
                shown.tools = machine.tools.clone();
                self.shown = Some(shown);
            }
            None => self.scope = Scope::All,
        }
    }

    pub(super) fn change(&mut self, change: CostView) -> Task<Message> {
        match change {
            CostView::Period(period) => {
                self.period = period;
                self.hovered_day = None;
            }
            CostView::HoverDay(day) => self.hovered_day = day,
            CostView::EditPlan(editing) => {
                let started = editing.is_some();
                self.editing_plan = editing;
                if started {
                    return iced::widget::operation::focus(PLAN_PRICE_INPUT);
                }
            }
            CostView::PlanDraft(draft) => {
                if let Some((_, text)) = &mut self.editing_plan {
                    *text = draft;
                }
            }
            CostView::SavePlan => {
                if let Some((key, draft)) = self.editing_plan.take() {
                    // An empty field goes back to the list price.
                    if draft.trim().is_empty() {
                        self.plan_prices.remove(&key);
                    } else if let Some(price) = plans::parse_typed_price(&draft) {
                        self.plan_prices.insert(key, price);
                    } else {
                        self.editing_plan = Some((key, draft));
                        return Task::none();
                    }
                    if let Err(error) = plans::save_prices(&self.plan_prices) {
                        preview_log(format!("saving plan prices failed: {error}"));
                    }
                }
            }
            CostView::Scope(scope) => {
                self.scope = scope;
                self.hovered_day = None;
                self.rescope();
            }
            CostView::HoverMachine(scope) => self.hovered_machine = scope,
            CostView::Machine(change) => {
                if let MachineChange::Remove(name) = &change
                    && self.scope == Scope::Machine(name.clone())
                {
                    self.scope = Scope::All;
                    self.rescope();
                }
                let (task, rescan) = self.machines.change(change);
                if rescan {
                    return Task::batch([task, self.scan()]);
                }
                return task;
            }
        }
        Task::none()
    }
}

/// Updates prices once a day, then reads what is new in the logs and adds
/// the machines' last readings. Runs on its own thread: the first read of
/// large logs takes a few seconds.
fn read_report(machines: &[String]) -> Result<CostReport, String> {
    let cache_directory = cost::default_cache_directory();
    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        && let Err(error) = runtime.block_on(cost::refresh_prices_if_stale(&cache_directory))
    {
        preview_log(format!("price update failed: {error}"));
    }
    let readings = machines
        .iter()
        .filter_map(|name| {
            cost::remote::stored(&cache_directory, name).map(|reading| (name.clone(), reading))
        })
        .collect::<Vec<_>>();
    Ok(cost::scan_report_with(
        &LogRoots::from_environment(),
        &cache_directory,
        &readings,
    ))
}

/// English or Arabic, for the words only this page uses.
fn tr(language: locale::Language, english: &'static str, arabic: &'static str) -> &'static str {
    match language {
        locale::Language::English => english,
        locale::Language::Arabic => arabic,
    }
}

/// Codex in a neutral tone and Claude in its brand orange.
fn tool_color(tool: CostTool, theme: &'static ThemeDefinition) -> Color {
    match tool {
        CostTool::Claude => Color::from_rgb8(0xD9, 0x77, 0x57),
        CostTool::Codex if theme.colors.is_light => Color::from_rgb8(0x3A, 0x3A, 0x3A),
        CostTool::Codex => Color::from_rgb8(0xE6, 0xE6, 0xE6),
    }
}

/// The green of what the use is worth, the same on every theme.
fn value_color(theme: &'static ThemeDefinition) -> Color {
    if theme.colors.is_light {
        Color::from_rgb8(0x1F, 0x9D, 0x63)
    } else {
        Color::from_rgb8(0x3D, 0xD6, 0x8C)
    }
}

fn tool_provider(tool: CostTool) -> UsageProvider {
    match tool {
        CostTool::Codex => UsageProvider::Codex,
        CostTool::Claude => UsageProvider::Claude,
    }
}

pub(super) fn view(
    state: &CostTab,
    entries: &[AccountUsageEntry],
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let Some(full) = &state.report else {
        let note = if state.error.is_some() && !state.scanning {
            tr(
                language,
                "Couldn't read the local logs",
                "تعذّرت قراءة السجلات المحلية",
            )
        } else {
            tr(language, "Reading local logs…", "جارٍ قراءة السجلات المحلية…")
        };
        return container(
            text(note)
                .size(typography::BODY_SIZE)
                .color(theme.colors.muted_text()),
        )
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .into();
    };

    let report = state.shown.as_ref().unwrap_or(full);
    let days = state.period.days();
    let plans = plans::plans(entries, &state.plan_prices);
    let mut sections = vec![
        controls(state, full, theme, language),
        headline(report, state, &plans, theme, language),
    ];
    // The plans pay for every machine, so they are weighed only against all.
    if !plans.is_empty() && state.scope == Scope::All {
        sections.push(plan_list(state, &plans, theme, language));
    }
    sections.extend([
        tool_split(report, state, theme, language),
        machines::section(
            &state.machines,
            full,
            days,
            &state.scope,
            state.hovered_machine.as_ref(),
            theme,
            language,
        ),
        daily_chart(report, state, theme, language),
        token_metrics(report, days, theme, language),
        breakdown(report, state, theme, language),
        footer(report, state.scanning, theme, language),
    ]);

    let content = crate::smooth_scroll::smooth_scroll(
        DashboardTab::Cost.scroll_key(),
        scrollable(column(sections).spacing(16).width(Fill))
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(Fill),
    );
    container(content)
        .padding([10, 12])
        .width(Fill)
        .height(Fill)
        .into()
}

/// A row of choices with the picked one filled.
fn segmented(
    options: Vec<(&'static str, bool, Message)>,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let buttons = options.into_iter().map(|(label, selected, message)| {
        button(
            text(label)
                .size(typography::METADATA_SIZE)
                .font(if selected {
                    typography::EMPHASIS
                } else {
                    typography::MEDIUM
                })
                .color(if selected {
                    theme.colors.text()
                } else {
                    theme.colors.muted_text()
                }),
        )
        .on_press(message)
        .padding([4, 9])
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            style.background =
                (selected || hovered).then(|| Background::Color(theme.colors.hover()));
            style.border = Border {
                radius: 5.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
    });
    container(row(buttons).spacing(2))
        .padding(2)
        .style(move |_| container::Style {
            border: Border {
                color: theme.colors.border(0.45),
                width: 1.0,
                radius: 7.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn controls(
    state: &CostTab,
    full: &CostReport,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let periods = [
        (Period::Today, tr(language, "Today", "اليوم")),
        (Period::Week, tr(language, "7 days", "7 أيام")),
        (Period::Month, tr(language, "30 days", "30 يومًا")),
    ]
    .into_iter()
    .map(|(period, label)| {
        (
            label,
            state.period == period,
            Message::CostView(CostView::Period(period)),
        )
    })
    .collect();
    let mut line =
        row![segmented(periods, theme), Space::new().width(Fill)].align_y(Alignment::Center);
    if let Some(picker) = machines::picker(full, &state.scope, theme, language) {
        line = line.push(picker);
    }
    line.into()
}

/// The period's API value against what the plans cost for it.
fn headline(
    report: &CostReport,
    state: &CostTab,
    plans: &[Plan],
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let days = state.period.days();
    let value = report
        .tools
        .iter()
        .map(|tool| tool.last(days).cost_usd)
        .sum::<f64>();
    let english = language == locale::Language::English;
    let big = |figure: String| {
        text(figure)
            .size(30)
            .font(typography::STRONG)
            .color(theme.colors.text())
    };

    let paid = if state.scope == Scope::All {
        plans
            .iter()
            .filter_map(|plan| plan.paid_over(days))
            .sum::<f64>()
    } else {
        0.0
    };
    let unpriced = plans.iter().any(|plan| plan.monthly_usd.is_none());
    let mut figure =
        row![big(format_dollars(value)), Space::new().width(Fill)].align_y(Alignment::Center);
    if paid > 0.0 && value > 0.0 {
        figure = figure.push(return_badge(value / paid, theme, language));
    }

    let note = if state.scope != Scope::All {
        let name = state.scope.label(language);
        if english {
            format!("What the use on {name} would cost at API prices.")
        } else {
            format!("ما يكلّفه الاستخدام على {name} بأسعار API.")
        }
    } else if plans.is_empty() {
        tr(
            language,
            "What this PC's use would cost at API prices. Add your Codex or Claude account to weigh it against what you pay.",
            "ما يكلّفه استخدام هذا الجهاز بأسعار API. أضف حساب Codex أو Claude لتقارنه بما تدفعه.",
        )
        .to_owned()
    } else if paid <= 0.0 {
        tr(
            language,
            "What this PC's use would cost at API prices. Set your plan's price below to compare.",
            "ما يكلّفه استخدام هذا الجهاز بأسعار API. حدّد سعر اشتراكك في الأسفل لتقارن.",
        )
        .to_owned()
    } else {
        let mut note = match (english, value >= paid) {
            (true, true) => format!(
                "Your plans cost {} for this period. The same use at API prices: {}.",
                format_price(paid),
                format_dollars(value)
            ),
            (true, false) => format!(
                "Your plans cost {} for this period, more than this use at API prices.",
                format_price(paid)
            ),
            (false, true) => format!(
                "اشتراكاتك كلّفت {} لهذه الفترة، والاستخدام نفسه بأسعار API يكلّف {}.",
                format_price(paid),
                format_dollars(value)
            ),
            (false, false) => format!(
                "اشتراكاتك كلّفت {} لهذه الفترة، أكثر من هذا الاستخدام بأسعار API.",
                format_price(paid)
            ),
        };
        if unpriced {
            note.push_str(tr(
                language,
                " Plans without a price are left out.",
                " الاشتراكات بلا سعر غير محسوبة.",
            ));
        }
        note
    };

    let title = match (&state.scope, english) {
        (Scope::All, true) => "API VALUE OF YOUR USE".to_owned(),
        (Scope::All, false) => "قيمة استخدامك بأسعار API".to_owned(),
        (scope, true) => format!("API VALUE ON {}", scope.label(language).to_uppercase()),
        (scope, false) => format!("قيمة الاستخدام على {} بأسعار API", scope.label(language)),
    };
    let mut lines = column![
        text(title)
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.muted_text()),
        figure,
    ]
    .spacing(2);
    if paid > 0.0 {
        lines = lines.push(
            column![
                compare_bar(
                    tr(language, "Plans", "الاشتراك"),
                    paid,
                    paid.max(value),
                    theme.colors.muted_text(),
                    theme,
                ),
                compare_bar(
                    tr(language, "API", "API"),
                    value,
                    paid.max(value),
                    value_color(theme),
                    theme,
                ),
            ]
            .spacing(4)
            .padding([6, 0]),
        );
    }
    lines.push(muted_line(&note, theme)).into()
}

/// `14×`: how many times the plans' price the use is worth.
fn return_badge(
    times: f64,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let figure = if times >= 10.0 {
        format!("{times:.0}×")
    } else {
        format!("{times:.1}×")
    };
    let accent = value_color(theme);
    container(
        column![
            text(figure)
                .size(20)
                .font(typography::STRONG)
                .color(theme.colors.text()),
            text(tr(language, "your plans' worth", "قيمة اشتراكك"))
                .size(typography::COMPACT_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.muted_text()),
        ]
        .align_x(Alignment::Center),
    )
    .padding([4, 10])
    .style(move |_| container::Style {
        background: Some(Background::Color(accent.scale_alpha(0.16))),
        border: Border {
            color: accent.scale_alpha(0.55),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// A labelled bar sized against `largest`, with its amount at the end.
fn compare_bar(
    label: &'static str,
    amount: f64,
    largest: f64,
    color: Color,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let filled = if largest > 0.0 {
        ((amount / largest) * 1000.0).round().clamp(1.0, 1000.0) as u16
    } else {
        1
    };
    let bar = row![
        container(Space::new().width(Fill).height(8))
            .width(Length::FillPortion(filled))
            .style(move |_| container::Style {
                background: Some(Background::Color(color)),
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
                ..Default::default()
            }),
        Space::new()
            .width(Length::FillPortion(1000_u16.saturating_sub(filled).max(1)))
            .height(8),
    ]
    .width(Fill);
    row![
        text(label)
            .size(typography::METADATA_SIZE)
            .font(typography::MEDIUM)
            .color(theme.colors.muted_text())
            .width(58),
        bar,
        text(format_price(amount))
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text())
            .width(56)
            .align_x(iced::alignment::Horizontal::Right),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// The saved accounts' plans and their monthly prices, each price editable.
fn plan_list(
    state: &CostTab,
    plans: &[Plan],
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let english = language == locale::Language::English;
    let mut rows = column![
        text(tr(language, "Your plans", "اشتراكاتك"))
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text())
    ]
    .spacing(4);
    for plan in plans {
        let name = if plan.accounts > 1 {
            format!("{} {} × {}", plan.tool.label(), plan.name, plan.accounts)
        } else {
            format!("{} {}", plan.tool.label(), plan.name)
        };
        let editing = state
            .editing_plan
            .as_ref()
            .filter(|(key, _)| *key == plan.key)
            .map(|(_, draft)| draft.clone());
        let price: Element<'static, Message> = match editing {
            Some(draft) => {
                let valid = draft.trim().is_empty() || plans::parse_typed_price(&draft).is_some();
                row![
                    text_input(tr(language, "$ a month", "$ شهريًا"), &draft)
                        .id(PLAN_PRICE_INPUT)
                        .on_input(|draft| Message::CostView(CostView::PlanDraft(draft)))
                        .on_submit(Message::CostView(CostView::SavePlan))
                        .size(typography::METADATA_SIZE)
                        .padding([3, 6])
                        .width(76)
                        .style(move |framework_theme, status| {
                            let mut style = crate::dialogs::account_key_input_style(
                                framework_theme,
                                status,
                                theme,
                            );
                            if !valid {
                                style.border.color = Color::from_rgb8(0xD9, 0x4B, 0x4B);
                            }
                            style
                        }),
                    plan_button(tr(language, "Save", "حفظ"), true, CostView::SavePlan, theme),
                    plan_button("✕", false, CostView::EditPlan(None), theme),
                ]
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            }
            None => {
                let label = match (plan.monthly_usd, english) {
                    (Some(price), true) => format!("{}/mo", format_price(price)),
                    (Some(price), false) => format!("{} شهريًا", format_price(price)),
                    (None, _) => tr(language, "Set price", "حدّد السعر").to_owned(),
                };
                let draft = plan
                    .monthly_usd
                    .filter(|_| plan.custom)
                    .map(|price| format!("{price}"))
                    .unwrap_or_default();
                plan_button(
                    label,
                    plan.monthly_usd.is_none(),
                    CostView::EditPlan(Some((plan.key.clone(), draft))),
                    theme,
                )
            }
        };
        rows = rows.push(
            row![
                image(provider_logo_handle(
                    tool_provider(plan.tool),
                    theme.colors.is_light
                ))
                .width(14)
                .height(14)
                .content_fit(ContentFit::Contain),
                text(name)
                    .size(typography::LABEL_SIZE)
                    .font(typography::MEDIUM)
                    .color(theme.colors.text())
                    .width(Fill),
                price,
            ]
            .spacing(7)
            .align_y(Alignment::Center),
        );
    }
    let custom = plans.iter().any(|plan| plan.custom);
    rows.push(muted_line(
        match (custom, english) {
            (false, true) => "List prices on monthly billing. Click a price to enter what you pay.",
            (false, false) => "أسعار الاشتراك الشهري المعلنة. اضغط على السعر لتكتب ما تدفعه فعلًا.",
            (true, true) => {
                "Prices you entered, or list prices. Clear a price to go back to the list price."
            }
            (true, false) => "الأسعار التي كتبتها أو المعلنة. امسح السعر لتعود إلى المعلن.",
        },
        theme,
    ))
    .into()
}

/// A small text button for the plan prices; `strong` ones are filled.
fn plan_button(
    label: impl Into<String>,
    strong: bool,
    change: CostView,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let accent = value_color(theme);
    button(
        text(label.into())
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
    )
    .on_press(Message::CostView(change))
    .padding([3, 8])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        style.background = Some(Background::Color(if strong {
            accent.scale_alpha(if hovered { 0.34 } else { 0.22 })
        } else if hovered {
            theme.colors.hover()
        } else {
            Color::TRANSPARENT
        }));
        style.border = Border {
            color: theme.colors.border(0.35),
            width: 1.0,
            radius: 6.0.into(),
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

/// Each tool's amount and share, largest first, with a thin bar in its color.
fn tool_split(
    report: &CostReport,
    state: &CostTab,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let days = state.period.days();
    let measure = |total: &DayCost| total.cost_usd;
    let mut totals = report
        .tools
        .iter()
        .map(|tool| (tool, tool.last(days)))
        .collect::<Vec<_>>();
    let sum = totals.iter().map(|(_, total)| measure(total)).sum::<f64>();
    totals.sort_by(|left, right| measure(&right.1).total_cmp(&measure(&left.1)));

    let rows = totals.into_iter().map(|(tool, total)| {
        let share = if sum > 0.0 {
            measure(&total) / sum
        } else {
            0.0
        };
        let amount = format_dollars(total.cost_usd);
        let detail = if !tool.logs_found {
            tr(
                language,
                "No logs on this machine",
                "لا توجد سجلات على هذا الجهاز",
            )
            .to_owned()
        } else {
            match language {
                locale::Language::English => format!(
                    "{} of cost · {} tokens",
                    format_percent(share),
                    format_tokens(total.tokens.total())
                ),
                locale::Language::Arabic => format!(
                    "{} من التكلفة · {} رمز",
                    format_percent(share),
                    format_tokens(total.tokens.total())
                ),
            }
        };
        let color = tool_color(tool.tool, theme);
        let filled = (share * 1000.0).round() as u16;
        let bar = row![
            container(Space::new().width(Fill).height(4))
                .width(Length::FillPortion(filled.max(1)))
                .style(move |_| container::Style {
                    background: Some(Background::Color(color)),
                    border: Border {
                        radius: 2.0.into(),
                        ..Border::default()
                    },
                    ..Default::default()
                }),
            Space::new()
                .width(Length::FillPortion(1000_u16.saturating_sub(filled).max(1)))
                .height(4),
        ];
        let track = container(bar).width(Fill).style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.hover())),
            border: Border {
                radius: 2.0.into(),
                ..Border::default()
            },
            ..Default::default()
        });
        column![
            row![
                image(provider_logo_handle(
                    tool_provider(tool.tool),
                    theme.colors.is_light
                ))
                .width(16)
                .height(16)
                .content_fit(ContentFit::Contain),
                text(tool.tool.label())
                    .size(typography::BODY_SIZE)
                    .font(typography::EMPHASIS)
                    .color(theme.colors.text())
                    .width(Fill),
                text(amount)
                    .size(typography::BODY_SIZE)
                    .font(typography::EMPHASIS)
                    .color(theme.colors.text()),
            ]
            .spacing(7)
            .align_y(Alignment::Center),
            track,
            text(detail)
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.muted_text()),
        ]
        .spacing(4)
        .into()
    });
    column(rows).spacing(12).into()
}

/// The days the chart shows: the period, and at least a week.
fn chart_days(report: &CostReport, period: Period) -> usize {
    let available = report.tools.first().map_or(0, |tool| tool.days.len());
    period.days().max(7).min(available)
}

/// One column per day with both tools drawn from the same baseline, the
/// smaller in front, so neither looks bigger just for being on top.
fn daily_chart(
    report: &CostReport,
    state: &CostTab,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let count = chart_days(report, state.period);
    let value = |day: &DayCost| day.cost_usd;
    let series = report
        .tools
        .iter()
        .map(|tool| {
            let start = tool.days.len().saturating_sub(count);
            (tool.tool, &tool.days[start..])
        })
        .collect::<Vec<_>>();
    let highest = series
        .iter()
        .flat_map(|(_, days)| days.iter().map(value))
        .fold(0.0_f64, f64::max);
    let first_in_period = count.saturating_sub(state.period.days());

    let columns = (0..count).map(|index| {
        let mut bars = series
            .iter()
            .filter_map(|(tool, days)| days.get(index).map(|day| (*tool, value(day))))
            .filter(|(_, amount)| *amount > 0.0)
            .collect::<Vec<_>>();
        // The taller bar behind, the shorter in front.
        bars.sort_by(|left, right| right.1.total_cmp(&left.1));
        let dimmed = index < first_in_period;
        let layers = bars.into_iter().map(|(tool, amount)| {
            let height = ((amount / highest) as f32 * CHART_HEIGHT).max(2.0);
            let mut color = tool_color(tool, theme);
            if dimmed {
                color.a = 0.35;
            }
            container(
                container(Space::new().width(Fill).height(height))
                    .width(Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(color)),
                        border: Border {
                            radius: 2.0.into(),
                            ..Border::default()
                        },
                        ..Default::default()
                    }),
            )
            .width(Fill)
            .height(Fill)
            .align_y(Alignment::End)
            .into()
        });
        let baseline: Element<'static, Message> = container(
            container(Space::new().width(Fill).height(1)).style(move |_| container::Style {
                background: Some(Background::Color(theme.colors.border(0.6))),
                ..Default::default()
            }),
        )
        .width(Fill)
        .height(Fill)
        .align_y(Alignment::End)
        .into();
        let hovered = state.hovered_day == Some(index);
        let column_area = container(stack(std::iter::once(baseline).chain(layers)))
            .width(Fill)
            .height(CHART_HEIGHT)
            .padding([0, 1])
            .style(move |_| container::Style {
                background: hovered.then(|| Background::Color(theme.colors.hover())),
                border: Border {
                    radius: 3.0.into(),
                    ..Border::default()
                },
                ..Default::default()
            });
        mouse_area(column_area)
            .on_enter(Message::CostView(CostView::HoverDay(Some(index))))
            .on_exit(Message::CostView(CostView::HoverDay(None)))
            .into()
    });

    let days_shown = series.first().map(|(_, days)| *days).unwrap_or_default();
    // Under the pointer, the day's figures replace the heading.
    let heading: Element<'static, Message> = match state
        .hovered_day
        .and_then(|index| days_shown.get(index).map(|day| (index, day.day)))
    {
        Some((index, day)) => {
            let figures = series
                .iter()
                .filter_map(|(tool, days)| {
                    days.get(index)
                        .map(|day| format!("{} {}", tool.label(), format_dollars(day.cost_usd)))
                })
                .collect::<Vec<_>>()
                .join(" · ");
            text(format!("{} · {figures}", format_day(day, language)))
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .into()
        }
        None => text(tr(language, "API value by day", "القيمة يومًا بيوم"))
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text())
            .into(),
    };
    let axis = row![
        text(
            days_shown
                .first()
                .map(|day| format_day(day.day, language))
                .unwrap_or_default()
        )
        .size(typography::COMPACT_SIZE)
        .color(theme.colors.muted_text())
        .width(Fill),
        text(
            days_shown
                .last()
                .map(|day| format_day(day.day, language))
                .unwrap_or_default()
        )
        .size(typography::COMPACT_SIZE)
        .color(theme.colors.muted_text()),
    ];
    column![heading, row(columns).spacing(1).height(CHART_HEIGHT), axis]
        .spacing(6)
        .into()
}

/// One figure with its label above and a note below.
fn metric(
    label: &'static str,
    value: String,
    detail: String,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    container(
        column![
            text(label)
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.muted_text()),
            text(value)
                .size(typography::PERCENTAGE_SIZE + 2.0)
                .font(typography::EMPHASIS)
                .color(theme.colors.text()),
            text(detail)
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.muted_text()),
        ]
        .spacing(1),
    )
    .width(Fill)
    .padding([7, 9])
    .style(move |_| container::Style {
        background: Some(Background::Color(theme.colors.hover())),
        border: Border {
            radius: 7.0.into(),
            ..Border::default()
        },
        ..Default::default()
    })
    .into()
}

/// The token classes behind the total, and what the cache saved.
fn token_metrics(
    report: &CostReport,
    days: usize,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let mut total = DayCost::default();
    let mut active_days = 0;
    let longest = report
        .tools
        .iter()
        .map(|tool| tool.days.len())
        .max()
        .unwrap_or(0);
    for offset in 0..days.min(longest) {
        let mut day_tokens = 0;
        for tool in &report.tools {
            if let Some(day) = tool.days.iter().rev().nth(offset) {
                total.cost_usd += day.cost_usd;
                total.cache_savings_usd += day.cache_savings_usd;
                total.tokens.add(&day.tokens);
                day_tokens += day.tokens.total();
            }
        }
        active_days += u64::from(day_tokens > 0);
    }
    let tokens = &total.tokens;
    let input = tokens.input + tokens.cache_read;
    let cached_share = if input > 0 {
        tokens.cache_read as f64 / input as f64
    } else {
        0.0
    };
    let per_day = tokens.total() / active_days.max(1);
    let english = language == locale::Language::English;
    let processed = metric(
        tr(language, "All tokens", "كل الرموز"),
        format_tokens(tokens.total()),
        if english {
            format!("about {} on a working day", format_tokens(per_day))
        } else {
            format!("نحو {} في يوم العمل", format_tokens(per_day))
        },
        theme,
    );
    let cached = metric(
        tr(language, "Read from cache", "مقروء من الكاش"),
        format_tokens(tokens.cache_read),
        if english {
            format!("{} of everything read", format_percent(cached_share))
        } else {
            format!("{} من كل ما قُرئ", format_percent(cached_share))
        },
        theme,
    );
    let uncached = metric(
        tr(language, "Fresh input", "إدخال جديد"),
        format_tokens(tokens.input),
        if english {
            format!("{} of it kept in cache", format_tokens(tokens.cache_write))
        } else {
            format!("{} منه حُفظ في الكاش", format_tokens(tokens.cache_write))
        },
        theme,
    );
    let output = metric(
        tr(language, "Written by the model", "ما كتبه النموذج"),
        format_tokens(tokens.output),
        if english {
            format!("{} of it thinking", format_tokens(tokens.reasoning))
        } else {
            format!("{} منه تفكير", format_tokens(tokens.reasoning))
        },
        theme,
    );
    let savings = metric(
        tr(language, "Saved by the cache", "ما وفّره الكاش"),
        format_dollars(total.cache_savings_usd),
        if total.cost_usd > 0.0 {
            let times = total.cache_savings_usd / total.cost_usd;
            if english {
                format!("{times:.1}× the API value, had every read been new input")
            } else {
                format!("{times:.1}× القيمة، لو قُرئ كل شيء كإدخال جديد")
            }
        } else {
            tr(
                language,
                "had every read been new input",
                "لو قُرئ كل شيء كإدخال جديد",
            )
            .to_owned()
        },
        theme,
    );
    column![
        row![processed, cached].spacing(6),
        row![uncached, output].spacing(6),
        savings,
    ]
    .spacing(6)
    .into()
}

/// A table row: a wide first cell, then right-aligned number cells.
fn table_row(
    first: Element<'static, Message>,
    cells: Vec<(String, bool)>,
    theme: &'static ThemeDefinition,
    header: bool,
) -> Element<'static, Message> {
    let mut cells_row = row![first].spacing(4).align_y(Alignment::Center);
    for (value, strong) in cells {
        let color = if header || !strong {
            theme.colors.muted_text()
        } else {
            theme.colors.text()
        };
        cells_row = cells_row.push(
            text(value)
                .size(if header {
                    typography::METADATA_SIZE
                } else {
                    typography::LABEL_SIZE
                })
                .font(if strong && !header {
                    typography::EMPHASIS
                } else {
                    typography::MEDIUM
                })
                .color(color)
                .width(Length::Fixed(NUMBER_COLUMN))
                .align_x(iced::alignment::Horizontal::Right),
        );
    }
    let line = container(Space::new().width(Fill).height(1)).style(move |_| container::Style {
        background: Some(Background::Color(theme.colors.border(if header {
            0.6
        } else {
            0.25
        }))),
        ..Default::default()
    });
    column![container(cells_row).padding([5, 0]), line].into()
}

fn header_cell(label: &'static str, theme: &'static ThemeDefinition) -> Element<'static, Message> {
    text(label)
        .size(typography::METADATA_SIZE)
        .font(typography::MEDIUM)
        .color(theme.colors.muted_text())
        .width(Fill)
        .into()
}

/// The period by model: cost, share and tokens.
fn breakdown(
    report: &CostReport,
    state: &CostTab,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let title = text(tr(language, "Where it went", "أين ذهب الاستخدام"))
        .size(typography::LABEL_SIZE)
        .font(typography::EMPHASIS)
        .color(theme.colors.text());
    let days = state.period.days();
    let mut rows = vec![title.into()];
    let empty = tr(
        language,
        "No use in this period.",
        "لا استخدام في هذه الفترة.",
    );

    rows.push(table_row(
        header_cell(tr(language, "Model", "النموذج"), theme),
        vec![
            (tr(language, "Cost", "التكلفة").to_owned(), false),
            (tr(language, "Share", "الحصة").to_owned(), false),
            (tr(language, "Tokens", "الرموز").to_owned(), false),
        ],
        theme,
        true,
    ));
    let mut models = report
        .tools
        .iter()
        .flat_map(|tool| {
            tool.models(days)
                .into_iter()
                .map(move |model| (tool.tool, model))
        })
        .filter(|(_, model)| model.tokens.total() > 0)
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .1
            .cost_usd
            .unwrap_or(-1.0)
            .total_cmp(&left.1.cost_usd.unwrap_or(-1.0))
    });
    let total = models
        .iter()
        .filter_map(|(_, model)| model.cost_usd)
        .sum::<f64>();
    if models.is_empty() {
        rows.push(muted_line(empty, theme));
    }
    for (tool, model) in models {
        let (cost, share) = match model.cost_usd {
            Some(cost) => (
                format_dollars(cost),
                format_percent(if total > 0.0 { cost / total } else { 0.0 }),
            ),
            None => (
                tr(language, "no price", "بلا سعر").to_owned(),
                "–".to_owned(),
            ),
        };
        let name = row![
            image(provider_logo_handle(
                tool_provider(tool),
                theme.colors.is_light
            ))
            .width(13)
            .height(13)
            .content_fit(ContentFit::Contain),
            text(model.model.clone())
                .size(typography::LABEL_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.text()),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .width(Fill);
        rows.push(table_row(
            name.into(),
            vec![
                (cost, true),
                (share, false),
                (format_tokens(model.tokens.total()), false),
            ],
            theme,
            false,
        ));
    }
    column(rows).spacing(2).into()
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
        muted_line(
            if report.machines.is_empty() {
                tr(
                    language,
                    "Read from this PC's Codex and Claude Code logs, all accounts together.",
                    "مقروءة من سجلات Codex وClaude Code على هذا الجهاز، لكل الحسابات معًا.",
                )
            } else {
                tr(
                    language,
                    "Read from the Codex and Claude Code logs of this PC and your machines, all accounts together.",
                    "مقروءة من سجلات Codex وClaude Code على هذا الجهاز وأجهزتك، لكل الحسابات معًا.",
                )
            },
            theme,
        ),
        muted_line(&prices, theme),
    ];
    if scanning {
        lines.push(muted_line(
            tr(language, "Reading local logs…", "جارٍ قراءة السجلات المحلية…"),
            theme,
        ));
    }
    column(lines).spacing(2).into()
}

fn muted_line(message: &str, theme: &'static ThemeDefinition) -> Element<'static, Message> {
    text(message.to_owned())
        .size(typography::METADATA_SIZE)
        .font(typography::MEDIUM)
        .color(theme.colors.muted_text())
        .into()
}

/// `Oct 3`, or `3 أكتوبر`.
fn format_day(day: chrono::NaiveDate, language: locale::Language) -> String {
    use chrono::Datelike;
    const ENGLISH: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const ARABIC: [&str; 12] = [
        "يناير",
        "فبراير",
        "مارس",
        "أبريل",
        "مايو",
        "يونيو",
        "يوليو",
        "أغسطس",
        "سبتمبر",
        "أكتوبر",
        "نوفمبر",
        "ديسمبر",
    ];
    let month = day.month0() as usize;
    match language {
        locale::Language::English => format!("{} {}", ENGLISH[month], day.day()),
        locale::Language::Arabic => format!("{} {}", day.day(), ARABIC[month]),
    }
}

fn format_percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
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

/// `$30`, `$2.67`, `$1,769`: whole dollars without cents.
fn format_price(amount: f64) -> String {
    if amount > 0.0 && amount < 1_000.0 && (amount - amount.round()).abs() < 0.005 {
        format!("${:.0}", amount.round())
    } else {
        format_dollars(amount)
    }
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
    use super::{format_day, format_dollars, format_percent, format_price, format_tokens};
    use crate::locale::Language;

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
        assert_eq!(format_percent(0.684), "68.4%");
        assert_eq!(format_price(30.0), "$30");
        assert_eq!(format_price(2.666), "$2.67");
    }

    #[test]
    fn days_read_in_each_language() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        assert_eq!(format_day(day, Language::English), "Oct 3");
        assert_eq!(format_day(day, Language::Arabic), "3 أكتوبر");
    }
}
