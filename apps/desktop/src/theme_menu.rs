//! The palette menu: theme, usage percentage display, what account cards
//! show, and memory saver.

use super::*;
use display_options::ResetCreditVisibility;

pub(super) fn theme_dropdown(
    current_theme: ThemeId,
    language: locale::Language,
    memory_saver: bool,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let mut items = THEME_MANIFEST
        .iter()
        .copied()
        .map(|theme| theme_choice_row(theme, current_theme, active_theme))
        .collect::<Vec<_>>();

    items.push(menu_section_title(
        locale::text(language, locale::Text::PercentDisplayTitle),
        active_theme,
    ));
    let current_display = percent_display::current();
    for (mode, label) in [
        (PercentDisplay::Remaining, locale::Text::ShowRemaining),
        (PercentDisplay::Used, locale::Text::ShowUsed),
    ] {
        items.push(choice_row(
            locale::text(language, label),
            mode == current_display,
            Message::SelectPercentDisplay(mode),
            active_theme,
        ));
    }

    items.push(menu_section_title(
        locale::text(language, locale::Text::CardDetailsTitle),
        active_theme,
    ));
    let details_shown = display_options::show_account_details();
    items.push(choice_row(
        locale::text(language, locale::Text::ShowEmailAndPlan),
        details_shown,
        Message::SetShowAccountDetails(!details_shown),
        active_theme,
    ));
    let team_budgets_shown = display_options::show_team_budgets();
    items.push(choice_row(
        locale::text(language, locale::Text::ShowTeamBudgets),
        team_budgets_shown,
        Message::SetShowTeamBudgets(!team_budgets_shown),
        active_theme,
    ));

    items.push(menu_section_title(
        locale::text(language, locale::Text::ResetCreditsTitle),
        active_theme,
    ));
    let current_reset_credits = display_options::reset_credits();
    for (mode, label) in [
        (ResetCreditVisibility::All, locale::Text::ResetCreditsAll),
        (
            ResetCreditVisibility::ExpiringSoon,
            locale::Text::ResetCreditsSoon,
        ),
        (ResetCreditVisibility::None, locale::Text::ResetCreditsNone),
    ] {
        items.push(choice_row(
            locale::text(language, label),
            mode == current_reset_credits,
            Message::SelectResetCredits(mode),
            active_theme,
        ));
    }

    items.push(menu_section_title(
        locale::text(language, locale::Text::MemoryTitle),
        active_theme,
    ));
    items.push(choice_row(
        locale::text(language, locale::Text::MemorySaver),
        memory_saver,
        Message::SetMemorySaver(!memory_saver),
        active_theme,
    ));
    items.push(
        container(
            text(locale::text(language, locale::Text::MemorySaverTradeoff))
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
        )
        .padding([2, 9])
        .into(),
    );

    container(column(items).spacing(1))
        .width(200)
        .padding(6)
        .style(move |_| theme_dropdown_surface_style(active_theme))
        .into()
}

const MENU_ROW_HEIGHT: f32 = 30.0;

pub(super) fn theme_choice_row(
    theme_choice: ThemeDefinition,
    current_theme: ThemeId,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let selected = theme_choice.id == current_theme;
    let swatch_color = theme_choice.swatch_color();

    let swatch = container(Space::new().width(Fill).height(Fill))
        .width(16)
        .height(16)
        .style(move |_| container::Style {
            background: Some(Background::Color(swatch_color)),
            border: Border {
                color: active_theme.colors.border(0.45),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        });

    button(
        row![
            swatch,
            text(theme_choice.label)
                .size(typography::LABEL_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.text())
                .width(Fill),
            check_mark(selected, active_theme),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::SelectTheme(theme_choice.id))
    .width(Fill)
    .height(MENU_ROW_HEIGHT)
    .padding([4, 9])
    .style(move |theme: &Theme, status| {
        theme_menu_item_style(theme, selected, status, active_theme)
    })
    .into()
}

fn check_mark(selected: bool, active_theme: &'static ThemeDefinition) -> Element<'static, Message> {
    if selected {
        icon_check::<Theme>()
            .size(15)
            .color(active_theme.colors.text())
            .into()
    } else {
        Space::new().width(16).height(15).into()
    }
}

fn menu_section_title(
    title: &'static str,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    container(
        text(title)
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(active_theme.colors.muted_text()),
    )
    .padding(iced::Padding {
        top: 8.0,
        right: 9.0,
        bottom: 3.0,
        left: 9.0,
    })
    .into()
}

fn choice_row(
    label: &'static str,
    selected: bool,
    message: Message,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    button(
        row![
            text(label)
                .size(typography::LABEL_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.text())
                .width(Fill),
            check_mark(selected, active_theme),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(message)
    .width(Fill)
    .height(MENU_ROW_HEIGHT)
    .padding([4, 9])
    .style(move |theme: &Theme, status| {
        theme_menu_item_style(theme, selected, status, active_theme)
    })
    .into()
}

pub(super) fn theme_dropdown_surface_style(
    active_theme: &'static ThemeDefinition,
) -> container::Style {
    container::Style {
        // Opaque even on themes with a backdrop image, so the menu stays
        // readable over it.
        background: Some(Background::Color(active_theme.colors.window_surface())),
        border: Border {
            color: active_theme.colors.border(if active_theme.colors.is_light {
                0.45
            } else {
                0.22
            }),
            width: 1.0,
            radius: 12.0.into(),
        },
        shadow: Shadow::default(),
        ..Default::default()
    }
}

pub(super) fn theme_menu_item_style(
    framework_theme: &Theme,
    selected: bool,
    status: button::Status,
    active_theme: &'static ThemeDefinition,
) -> button::Style {
    let mut style = button::text(framework_theme, status);
    style.background =
        if selected || matches!(status, button::Status::Hovered | button::Status::Pressed) {
            Some(Background::Color(active_theme.colors.hover()))
        } else {
            None
        };
    style.text_color = active_theme.colors.text();
    style.border = Border {
        radius: 7.0.into(),
        ..Border::default()
    };
    style.shadow = Shadow::default();
    style
}
