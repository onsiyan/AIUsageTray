//! Title-bar controls, provider tabs, and the window frame.

use super::*;

pub(super) fn add_account_button(
    adding: bool,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let button = button(container(icon_user_round_plus().size(17)).center(Fill))
        .on_press_maybe((!adding).then_some(Message::ToggleAccountAddMenu))
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let mut style = button::text(theme, status);
            style.background =
                if adding || matches!(status, button::Status::Hovered | button::Status::Pressed) {
                    Some(Background::Color(active_theme.colors.hover()))
                } else {
                    None
                };
            style.text_color = active_theme.colors.text();
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });

    hint::hint(
        button,
        locale::text(language, locale::Text::AddAccount),
        active_theme,
    )
}

pub(super) fn delete_account_button(
    open: bool,
    disabled: bool,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let delete_color = active_theme.colors.danger_hover();
    let button = button(container(icon_trash_2().size(16)).center(Fill))
        .on_press_maybe((!disabled).then_some(Message::ToggleAccountDeleteDialog))
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let highlighted =
                open || matches!(status, button::Status::Hovered | button::Status::Pressed);
            let mut style = button::text(theme, status);
            style.background = highlighted.then(|| {
                Background::Color(delete_color.scale_alpha(if active_theme.colors.is_light {
                    0.14
                } else {
                    0.24
                }))
            });
            style.text_color = if highlighted {
                delete_color
            } else {
                active_theme.colors.text()
            };
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });

    hint::hint(
        button,
        locale::text(language, locale::Text::DeleteAccount),
        active_theme,
    )
}

pub(super) fn theme_button(active_theme: &'static ThemeDefinition) -> Element<'static, Message> {
    button(container(icon_palette().size(16)).center(Fill))
        .on_press(Message::ToggleThemeMenu)
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let mut style = button::text(theme, status);
            style.background =
                if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                    Some(Background::Color(active_theme.colors.hover()))
                } else {
                    None
                };
            style.text_color = active_theme.colors.text();
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
}

pub(super) fn refresh_button(
    refreshing: bool,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let refresh_icon =
        crate::spinner::spinning_icon(refresh_icon_handle(active_theme), 16.0, refreshing);

    let button = button(container(refresh_icon).center(Fill))
        .on_press_maybe((!refreshing).then_some(Message::RefreshAllUsage))
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let mut style = button::text(theme, status);
            style.background = if refreshing
                || matches!(status, button::Status::Hovered | button::Status::Pressed)
            {
                Some(Background::Color(active_theme.colors.hover()))
            } else {
                None
            };
            style.text_color = active_theme.colors.text();
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });

    hint::hint(
        button,
        locale::text(
            language,
            if refreshing {
                locale::Text::RefreshingUsage
            } else {
                locale::Text::RefreshUsage
            },
        ),
        active_theme,
    )
}

pub(super) fn provider_tab_bar(
    layout: &tabs::TabLayout,
    icons: &std::collections::HashMap<u32, image::Handle>,
    selected_tab: DashboardTab,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let tabs = layout
        .visible_tabs()
        .map(|entry| match &entry.kind {
            tabs::TabKind::Provider(provider) => {
                let tab = PROVIDER_TABS
                    .iter()
                    .copied()
                    .find(|tab| tab.provider == *provider)
                    .expect("every provider has a tab");
                provider_tab(tab, selected_tab, active_theme)
            }
            tabs::TabKind::Favorites => favorites_tab(selected_tab, active_theme, language),
            tabs::TabKind::Cost => cost_tab_button(selected_tab, active_theme, language),
            tabs::TabKind::Custom(custom) => custom_tab(
                custom,
                icons.get(&custom.id),
                entry.dashboard_tab(),
                selected_tab,
                active_theme,
            ),
        })
        .collect::<Vec<_>>();

    container(row(tabs).spacing(2).width(Fill))
        .width(Fill)
        .height(37)
        .padding([3, 8])
        .style(move |_| container::Style {
            background: Some(Background::Color(if active_theme.colors.is_light {
                active_theme.colors.control_surface()
            } else {
                Color::from_rgba(0.0, 0.0, 0.0, PROVIDER_TAB_SHADE_OPACITY)
            })),
            ..Default::default()
        })
        .into()
}

pub(super) fn provider_tab(
    tab: ProviderTab,
    selected_tab: DashboardTab,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let logo = image(provider_logo_handle(
        tab.provider,
        active_theme.colors.is_light,
    ))
    .width(24)
    .height(24)
    .content_fit(ContentFit::Contain);
    tab_button(
        logo.into(),
        DashboardTab::Provider(tab.provider),
        selected_tab,
        tab.label,
        Length::FillPortion(2),
        active_theme,
    )
}

/// The tab of accounts the user starred, from any provider.
fn favorites_tab(
    selected_tab: DashboardTab,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let star = icon_star::<Theme>()
        .size(20)
        .color(active_theme.colors.text());
    tab_button(
        star.into(),
        DashboardTab::Favorites,
        selected_tab,
        locale::text(language, locale::Text::Favorites),
        Length::FillPortion(2),
        active_theme,
    )
}

/// The tab of what local Codex and Claude Code usage would cost.
fn cost_tab_button(
    selected_tab: DashboardTab,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let icon = icon_circle_dollar_sign::<Theme>()
        .size(20)
        .color(active_theme.colors.text());
    tab_button(
        icon.into(),
        DashboardTab::Cost,
        selected_tab,
        locale::text(language, locale::Text::CostTab),
        Length::FillPortion(2),
        active_theme,
    )
}

/// A custom tab shows its image like a provider tab, or else its name,
/// which needs more room.
fn custom_tab(
    custom: &tabs::CustomTab,
    icon: Option<&image::Handle>,
    tab: DashboardTab,
    selected_tab: DashboardTab,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let providers = custom
        .providers
        .providers()
        .map(UsageProvider::display_name)
        .collect::<Vec<_>>()
        .join(", ");
    let (content, label, width): (Element<'static, Message>, String, Length) = match icon {
        Some(handle) => (
            image(handle.clone())
                .width(24)
                .height(24)
                .content_fit(ContentFit::Contain)
                .into(),
            if providers.is_empty() {
                custom.name.clone()
            } else {
                format!("{}: {providers}", custom.name)
            },
            Length::FillPortion(2),
        ),
        None => (
            text(custom.name.clone())
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text())
                .wrapping(text::Wrapping::None)
                .into(),
            providers,
            Length::FillPortion(4),
        ),
    };
    tab_button(content, tab, selected_tab, label, width, active_theme)
}

fn tab_button(
    content: Element<'static, Message>,
    tab: DashboardTab,
    selected_tab: DashboardTab,
    label: impl Into<String>,
    width: Length,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let selected = tab.same_tab(selected_tab);
    let tab_button = button(container(content).center(Fill))
        .on_press(Message::SelectTab(tab))
        .width(width)
        .height(31)
        .padding(0)
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            let highlighted =
                selected || matches!(status, button::Status::Hovered | button::Status::Pressed);
            style.background = highlighted.then(|| Background::Color(active_theme.colors.hover()));
            style.text_color = active_theme.colors.text();
            style.border = Border {
                radius: 7.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });

    hint::hint(tab_button, label, active_theme)
}

pub(super) fn provider_logo_handle(provider: UsageProvider, light_theme: bool) -> image::Handle {
    if light_theme {
        let logo_index = match provider {
            UsageProvider::Codex => Some(0),
            UsageProvider::OpenRouter => Some(1),
            UsageProvider::Copilot => Some(2),
            UsageProvider::Cursor => Some(3),
            UsageProvider::Zai => Some(4),
            UsageProvider::Xai => Some(5),
            UsageProvider::MiMo => Some(6),
            UsageProvider::Kimi => Some(7),
            _ => None,
        };
        if let Some(logo_index) = logo_index {
            let logos = LIGHT_THEME_PROVIDER_LOGOS.get_or_init(|| {
                [
                    decode_provider_logo(include_bytes!("../assets/providers/chatgpt.png"), true),
                    decode_provider_logo(
                        include_bytes!("../assets/providers/openrouter.png"),
                        true,
                    ),
                    decode_provider_logo(include_bytes!("../assets/providers/copilot.png"), true),
                    decode_provider_logo(include_bytes!("../assets/providers/cursor.png"), true),
                    decode_provider_logo(include_bytes!("../assets/providers/zai.png"), true),
                    decode_provider_logo(include_bytes!("../assets/providers/xai.png"), true),
                    decode_provider_logo(include_bytes!("../assets/providers/mimo.png"), true),
                    // Already black, and its dot must stay blue.
                    decode_provider_logo(
                        include_bytes!("../assets/providers/kimi-light-theme.png"),
                        false,
                    ),
                ]
            });
            return logos[logo_index].clone();
        }
    }

    let logos = PROVIDER_LOGOS.get_or_init(|| {
        [
            decode_provider_logo(include_bytes!("../assets/providers/chatgpt.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/claude.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/antigravity.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/opencode-go.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/openrouter.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/deepseek.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/copilot.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/cursor.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/kimi.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/zai.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/xai.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/minimax.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/mimo.png"), false),
        ]
    });

    logos[match provider {
        UsageProvider::Codex => 0,
        UsageProvider::Claude => 1,
        UsageProvider::Antigravity => 2,
        UsageProvider::OpenCodeGo => 3,
        UsageProvider::OpenRouter => 4,
        UsageProvider::DeepSeek => 5,
        UsageProvider::Copilot => 6,
        UsageProvider::Cursor => 7,
        UsageProvider::Kimi => 8,
        UsageProvider::Zai => 9,
        UsageProvider::Xai => 10,
        UsageProvider::MiniMax => 11,
        UsageProvider::MiMo => 12,
    }]
    .clone()
}

pub(super) fn close_window_button(
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    button(container(icon_x().size(16)).center(Fill))
        .on_press(Message::CloseButton)
        .width(30)
        .height(29)
        .padding(0)
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            style.background =
                hovered.then(|| Background::Color(active_theme.colors.danger_hover()));
            style.text_color = if hovered {
                Color::WHITE
            } else {
                active_theme.colors.text()
            };
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
}

pub(super) fn window_frame_style(theme: &'static ThemeDefinition) -> container::Style {
    let surface = if theme.backdrop.is_some() {
        Color::TRANSPARENT
    } else {
        theme.colors.window_surface()
    };

    container::Style {
        background: Some(Background::Color(surface)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: WINDOW_FRAME_RADIUS.into(),
        },
        text_color: None,
        shadow: Shadow::default(),
        snap: false,
    }
}

pub(super) fn window_frame_outline_style(theme: &'static ThemeDefinition) -> container::Style {
    container::Style {
        border: Border {
            color: if theme.colors.is_light {
                theme.colors.border(0.58)
            } else {
                Color::WHITE
            },
            width: WINDOW_FRAME_BORDER_WIDTH,
            radius: WINDOW_FRAME_RADIUS.into(),
        },
        ..Default::default()
    }
}
