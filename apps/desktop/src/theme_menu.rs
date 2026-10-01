//! The palette menu: theme choice and usage percentage display.

use super::*;

pub(super) fn theme_dropdown(
    current_theme: ThemeId,
    language: locale::Language,
) -> Element<'static, Message> {
    let mut items = THEME_MANIFEST
        .iter()
        .copied()
        .map(|theme| theme_choice_row(theme, current_theme))
        .collect::<Vec<_>>();

    items.push(
        container(
            text(locale::text(language, locale::Text::PercentDisplayTitle))
                .size(typography::METADATA_SIZE)
                .color(Color::from_rgba(1.0, 1.0, 1.0, 0.6)),
        )
        .padding([6, 9])
        .into(),
    );
    let current_display = percent_display::current();
    for (mode, label) in [
        (PercentDisplay::Remaining, locale::Text::ShowRemaining),
        (PercentDisplay::Used, locale::Text::ShowUsed),
    ] {
        items.push(percent_display_choice_row(
            mode,
            locale::text(language, label),
            mode == current_display,
        ));
    }

    container(column(items).spacing(2))
        .width(156)
        .padding(6)
        .style(theme_dropdown_surface_style)
        .into()
}

pub(super) fn theme_choice_row(
    theme_choice: ThemeDefinition,
    current_theme: ThemeId,
) -> Element<'static, Message> {
    let selected = theme_choice.id == current_theme;
    let check: Element<'static, Message> = if selected {
        icon_check::<Theme>().size(15).color(Color::WHITE).into()
    } else {
        Space::new().width(16).height(15).into()
    };
    let swatch_color = theme_choice.swatch_color();

    let swatch = container(Space::new().width(Fill).height(Fill))
        .width(16)
        .height(16)
        .style(move |_| container::Style {
            background: Some(Background::Color(swatch_color)),
            border: Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.22),
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
                .color(Color::WHITE)
                .width(Fill),
            check,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::SelectTheme(theme_choice.id))
    .width(Fill)
    .height(34)
    .padding([4, 9])
    .style(move |theme: &Theme, status| theme_menu_item_style(theme, selected, status))
    .into()
}

pub(super) fn percent_display_choice_row(
    mode: PercentDisplay,
    label: &'static str,
    selected: bool,
) -> Element<'static, Message> {
    let check: Element<'static, Message> = if selected {
        icon_check::<Theme>().size(15).color(Color::WHITE).into()
    } else {
        Space::new().width(16).height(15).into()
    };

    button(
        row![
            text(label)
                .size(typography::LABEL_SIZE)
                .color(Color::WHITE)
                .width(Fill),
            check,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::SelectPercentDisplay(mode))
    .width(Fill)
    .height(34)
    .padding([4, 9])
    .style(move |theme: &Theme, status| theme_menu_item_style(theme, selected, status))
    .into()
}

pub(super) fn theme_dropdown_surface_style(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::BLACK)),
        border: Border {
            radius: 14.0.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
        ..Default::default()
    }
}

pub(super) fn theme_menu_item_style(
    framework_theme: &Theme,
    selected: bool,
    status: button::Status,
) -> button::Style {
    let mut style = button::text(framework_theme, status);
    style.background =
        if selected || matches!(status, button::Status::Hovered | button::Status::Pressed) {
            Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.14)))
        } else {
            None
        };
    style.text_color = Color::WHITE;
    style.border = Border {
        radius: 8.0.into(),
        ..Border::default()
    };
    style.shadow = Shadow::default();
    style
}
