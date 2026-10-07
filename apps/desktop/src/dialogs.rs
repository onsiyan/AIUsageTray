//! Account add, delete, and OpenRouter key dialogs, and the add-account status banner.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;

pub(super) fn account_add_dropdown(
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    // Two columns side by side, like the palette menu, so the list is wide
    // rather than tall.
    let half = PROVIDER_TABS.len().div_ceil(2);
    let provider_column = |providers: &[ProviderTab]| {
        column(
            providers
                .iter()
                .copied()
                .map(|provider| account_provider_choice(provider, active_theme)),
        )
        .spacing(2)
        .width(Fill)
    };

    container(
        column![
            text(locale::text(language, locale::Text::ChooseProvider))
                .size(typography::METADATA_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.muted_text()),
            row![
                provider_column(&PROVIDER_TABS[..half]),
                provider_column(&PROVIDER_TABS[half..]),
            ]
            .spacing(6),
        ]
        .spacing(6),
    )
    .width(400)
    .padding(8)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

pub(super) fn account_provider_choice(
    provider: ProviderTab,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    button(
        row![
            image(provider_logo_handle(
                provider.provider,
                active_theme.colors.is_light,
            ))
            .width(24)
            .height(24)
            .content_fit(ContentFit::Contain),
            text(provider.label)
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text())
                .width(Fill),
        ]
        .spacing(9)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ChooseAccountProvider(provider.provider))
    .width(Fill)
    .height(34)
    .padding([3, 7])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = matches!(status, button::Status::Hovered | button::Status::Pressed)
            .then(|| Background::Color(active_theme.colors.hover()));
        style.text_color = active_theme.colors.text();
        style.border = Border {
            radius: 7.0.into(),
            ..Border::default()
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

pub(super) fn account_deletion_picker_dialog(
    accounts: &[dashboard::AccountUsageEntry],
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let mut provider_groups = Vec::new();
    let mut account_count = 0;

    for provider_tab in PROVIDER_TABS.iter().copied() {
        let provider_accounts = accounts
            .iter()
            .filter(|entry| {
                dashboard::belongs_to_provider(&entry.account.provider_id, provider_tab.provider)
            })
            .collect::<Vec<_>>();
        if provider_accounts.is_empty() {
            continue;
        }

        account_count += provider_accounts.len();
        let mut account_rows = Vec::with_capacity(provider_accounts.len());
        for entry in provider_accounts {
            let account_id = entry.account.id;
            let display_name = dashboard::account_name(&entry.account);
            let email = if dashboard::name_is_email(&entry.account) {
                String::new()
            } else {
                dashboard::shown_email(&entry.account.email).to_owned()
            };
            account_rows.push(
                button(
                    row![
                        column![
                            text(display_name)
                                .size(typography::LABEL_SIZE)
                                .font(typography::MEDIUM)
                                .color(active_theme.colors.text()),
                            text(email)
                                .size(typography::METADATA_SIZE)
                                .font(typography::MEDIUM)
                                .color(active_theme.colors.muted_text()),
                        ]
                        .spacing(1)
                        .width(Fill),
                        icon_trash_2()
                            .size(14)
                            .color(active_theme.colors.danger_hover()),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .width(Fill),
                )
                .on_press(Message::SelectAccountForDeletion(account_id))
                .width(Fill)
                .height(42)
                .padding([4, 8])
                .style(move |framework_theme: &Theme, status| {
                    let mut style = button::text(framework_theme, status);
                    style.background =
                        matches!(status, button::Status::Hovered | button::Status::Pressed)
                            .then(|| Background::Color(active_theme.colors.hover()));
                    style.text_color = active_theme.colors.text();
                    style.border = Border {
                        color: active_theme.colors.border(0.12),
                        width: 1.0,
                        radius: 6.0.into(),
                    };
                    style.shadow = Shadow::default();
                    style
                })
                .into(),
            );
        }

        let heading = row![
            image(provider_logo_handle(
                provider_tab.provider,
                active_theme.colors.is_light,
            ))
            .width(20)
            .height(20)
            .content_fit(ContentFit::Contain),
            text(provider_tab.label)
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        provider_groups.push(
            column![heading, column(account_rows).spacing(2)]
                .spacing(5)
                .width(Fill)
                .into(),
        );
    }

    let list_height = if account_count == 0 {
        78.0
    } else {
        (account_count as f32 * 44.0 + provider_groups.len() as f32 * 28.0).clamp(140.0, 390.0)
    };
    let account_list: Element<'static, Message> = if provider_groups.is_empty() {
        container(
            text(locale::text(language, locale::Text::NoSavedAccounts))
                .size(typography::BODY_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
        )
        .width(Fill)
        .height(Length::Fixed(list_height))
        .center(Fill)
        .into()
    } else {
        crate::smooth_scroll::smooth_scroll(
            "manage-accounts",
            scrollable(column(provider_groups).spacing(10).width(Fill))
                .direction(iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::hidden(),
                ))
                .height(Length::Fixed(list_height)),
        )
    };

    container(
        column![
            text(locale::text(language, locale::Text::ManageAccounts))
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            text(locale::text(language, locale::Text::ChooseAccountToDelete))
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            account_list,
            row![
                Space::new().width(Fill).height(1),
                account_dialog_button(
                    locale::text(language, locale::Text::Cancel),
                    false,
                    true,
                    active_theme,
                    Message::DismissAccountDeleteDialog,
                ),
            ]
            .align_y(Alignment::Center)
            .width(Fill),
        ]
        .spacing(9)
        .width(Fill),
    )
    .width(380)
    .padding(15)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

pub(super) fn account_deletion_confirmation_dialog(
    pending: &PendingAccountDeletion,
    deleting: bool,
    queued: bool,
    error: Option<&str>,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let account_identity = row![
        image(provider_logo_handle(
            pending.provider,
            active_theme.colors.is_light,
        ))
        .width(28)
        .height(28)
        .content_fit(ContentFit::Contain),
        column![
            text(pending.provider.display_name())
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            text(pending.display_name.clone())
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            text(pending.email.clone())
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
        ]
        .spacing(2)
        .width(Fill),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .width(Fill);

    let error_message: Element<'static, Message> = error
        .map(|error| {
            column![
                text(locale::text(language, locale::Text::AccountDeletionFailed))
                    .size(typography::LABEL_SIZE)
                    .font(typography::MEDIUM)
                    .color(active_theme.colors.danger_hover()),
                text(error.to_owned())
                    .size(typography::METADATA_SIZE)
                    .font(typography::MEDIUM)
                    .color(active_theme.colors.muted_text()),
            ]
            .spacing(3)
            .width(Fill)
            .into()
        })
        .unwrap_or_else(|| Space::new().width(Fill).height(0).into());

    let refresh_notice: Element<'static, Message> = if queued {
        text(locale::text(language, locale::Text::WaitForUsageRefresh))
            .size(typography::METADATA_SIZE)
            .font(typography::MEDIUM)
            .color(active_theme.colors.muted_text())
            .into()
    } else {
        Space::new().width(Fill).height(0).into()
    };

    container(
        column![
            text(locale::text(language, locale::Text::ConfirmAccountDeletion))
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            account_identity,
            text(locale::text(language, locale::Text::AccountDeletionWarning))
                .size(typography::BODY_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            refresh_notice,
            error_message,
            row![
                account_dialog_button(
                    locale::text(language, locale::Text::Cancel),
                    false,
                    !deleting,
                    active_theme,
                    Message::CancelAccountDeletion,
                ),
                destructive_dialog_button(
                    locale::text(
                        language,
                        if deleting {
                            locale::Text::DeletingAccount
                        } else if queued {
                            locale::Text::Waiting
                        } else {
                            locale::Text::Delete
                        },
                    ),
                    !deleting && !queued,
                    active_theme,
                    Message::ConfirmAccountDeletion,
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill),
        ]
        .spacing(12)
        .width(Fill),
    )
    .width(372)
    .padding(16)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

pub(super) fn destructive_dialog_button(
    label: &'static str,
    enabled: bool,
    active_theme: &'static ThemeDefinition,
    message: Message,
) -> Element<'static, Message> {
    const DANGER: Color = Color::from_rgb8(166, 48, 48);

    button(
        text(label)
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS),
    )
    .on_press_maybe(enabled.then_some(message))
    .padding([7, 12])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = Some(Background::Color(
            if enabled && matches!(status, button::Status::Hovered | button::Status::Pressed) {
                active_theme.colors.danger_hover()
            } else if enabled {
                DANGER
            } else {
                active_theme.colors.control_surface()
            },
        ));
        style.text_color = if enabled {
            Color::WHITE
        } else {
            active_theme.colors.muted_text()
        };
        style.border = Border {
            color: active_theme.colors.border(0.24),
            width: 1.0,
            radius: 7.0.into(),
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

pub(super) fn api_key_dialog<'a>(
    provider: UsageProvider,
    api_key: &'a str,
    management_key: &'a str,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'a, Message> {
    let is_cursor = provider == UsageProvider::Cursor;
    let is_xai = provider == UsageProvider::Xai;
    let is_mimo = provider == UsageProvider::MiMo;
    let api_key_input = text_input(
        locale::text(
            language,
            if is_cursor {
                locale::Text::CursorSession
            } else if is_mimo {
                locale::Text::MiMoCookie
            } else if is_xai {
                locale::Text::XaiManagementKey
            } else {
                locale::Text::OpenRouterApiKey
            },
        ),
        api_key,
    )
    .secure(true)
    .size(typography::LABEL_SIZE)
    .font(typography::MEDIUM)
    .padding([8, 10])
    .on_input(Message::ApiKeyChanged)
    .width(Fill)
    .style(move |framework_theme, status| {
        account_key_input_style(framework_theme, status, active_theme)
    });

    // OpenRouter has a second, optional key (for Activity); xAI needs the
    // team the Management key reads, which is not a secret.
    let management_key_input = (provider == UsageProvider::OpenRouter || is_xai).then(|| {
        text_input(
            locale::text(
                language,
                if is_xai {
                    locale::Text::XaiTeamId
                } else {
                    locale::Text::OpenRouterManagementKey
                },
            ),
            management_key,
        )
        .secure(!is_xai)
        .size(typography::LABEL_SIZE)
        .font(typography::MEDIUM)
        .padding([8, 10])
        .on_input(Message::ManagementKeyChanged)
        .width(Fill)
        .style(move |framework_theme, status| {
            account_key_input_style(framework_theme, status, active_theme)
        })
    });
    let title = match provider {
        UsageProvider::DeepSeek => locale::text(language, locale::Text::DeepSeekTitle),
        UsageProvider::Cursor => locale::text(language, locale::Text::CursorTitle),
        UsageProvider::Kimi => locale::text(language, locale::Text::KimiTitle),
        UsageProvider::Zai => locale::text(language, locale::Text::ZaiTitle),
        UsageProvider::Xai => locale::text(language, locale::Text::XaiTitle),
        UsageProvider::MiniMax => locale::text(language, locale::Text::MiniMaxTitle),
        UsageProvider::MiMo => locale::text(language, locale::Text::MiMoTitle),
        _ => locale::text(language, locale::Text::OpenRouterTitle),
    };
    let hint = if is_cursor {
        locale::Text::CursorSessionHint
    } else if is_xai {
        locale::Text::XaiHint
    } else if is_mimo {
        locale::Text::MiMoHint
    } else {
        locale::Text::OpenRouterCredentialHint
    };
    // Cursor's and MiMo's cookies are copied from their signed-in sites.
    let open_site = if is_cursor {
        Some((locale::Text::OpenCursor, Message::OpenCursorSite))
    } else if is_mimo {
        Some((locale::Text::OpenMiMo, Message::OpenMiMoSite))
    } else {
        None
    }
    .map(|(label, message)| {
        account_dialog_button(
            locale::text(language, label),
            false,
            true,
            active_theme,
            message,
        )
    });

    let submit_enabled =
        !api_key.trim().is_empty() && (!is_xai || !management_key.trim().is_empty());
    container(
        column![
            text(title)
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            text(locale::text(language, hint))
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            api_key_input,
        ]
        .push(management_key_input)
        .push(
            row![]
                .push(open_site)
                .push(account_dialog_button(
                    locale::text(language, locale::Text::Cancel),
                    false,
                    true,
                    active_theme,
                    Message::CancelCredentials,
                ))
                .push(account_dialog_button(
                    locale::text(language, locale::Text::AddAccount),
                    true,
                    submit_enabled,
                    active_theme,
                    Message::SubmitCredentials,
                ))
                .spacing(8)
                .align_y(Alignment::Center)
                .width(Fill),
        )
        .spacing(10)
        .width(Fill),
    )
    .width(344)
    .padding(16)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

/// The one-time code of a Copilot or Codex sign-in, while the app waits for
/// the user to enter it on the provider's page.
pub(super) fn device_sign_in_dialog(
    code: &DeviceSignIn,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let texts = DeviceSignInTexts::of(code.provider);
    let code_box = container(
        row![
            text(code.user_code.clone())
                .size(26)
                .font(typography::STRONG)
                .color(active_theme.colors.text())
                .width(Fill)
                .align_x(Alignment::Center),
            button(container(lucide_icons::iced::icon_copy().size(15)).center(Fill))
                .on_press(Message::CopyDeviceCode)
                .width(30)
                .height(30)
                .padding(0)
                .style(move |framework_theme: &Theme, status| {
                    let mut style = button::text(framework_theme, status);
                    style.background =
                        matches!(status, button::Status::Hovered | button::Status::Pressed)
                            .then(|| Background::Color(active_theme.colors.hover()));
                    style.text_color = active_theme.colors.text();
                    style.border = Border {
                        radius: 6.0.into(),
                        ..Border::default()
                    };
                    style
                }),
        ]
        .align_y(Alignment::Center),
    )
    .padding([10, 10])
    .width(Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(active_theme.colors.control_surface())),
        border: Border {
            color: active_theme.colors.border(0.24),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    });

    container(
        column![
            row![
                image(provider_logo_handle(
                    code.provider,
                    active_theme.colors.is_light
                ))
                .width(22)
                .height(22)
                .content_fit(ContentFit::Contain),
                text(locale::text(language, texts.title))
                    .size(typography::ACCOUNT_NAME_SIZE)
                    .font(typography::EMPHASIS)
                    .color(active_theme.colors.text()),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text(locale::text(language, texts.enter_code))
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            code_box,
            text(locale::text(language, texts.waiting))
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            row![
                Space::new().width(Fill).height(1),
                account_dialog_button(
                    locale::text(language, locale::Text::Cancel),
                    false,
                    true,
                    active_theme,
                    Message::CancelAccountAdd,
                ),
                account_dialog_button(
                    locale::text(language, texts.open_page),
                    true,
                    true,
                    active_theme,
                    Message::OpenDeviceCodePage,
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill),
        ]
        .spacing(10)
        .width(Fill),
    )
    .width(344)
    .padding(16)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

struct DeviceSignInTexts {
    title: locale::Text,
    enter_code: locale::Text,
    waiting: locale::Text,
    open_page: locale::Text,
}

impl DeviceSignInTexts {
    fn of(provider: UsageProvider) -> Self {
        if provider == UsageProvider::Codex {
            Self {
                title: locale::Text::CodexCodeTitle,
                enter_code: locale::Text::CodexEnterCode,
                waiting: locale::Text::CodexWaiting,
                open_page: locale::Text::OpenOpenAI,
            }
        } else {
            Self {
                title: locale::Text::CopilotTitle,
                enter_code: locale::Text::CopilotEnterCode,
                waiting: locale::Text::CopilotWaiting,
                open_page: locale::Text::OpenGitHub,
            }
        }
    }
}

pub(super) fn account_key_input_style(
    framework_theme: &Theme,
    status: text_input::Status,
    active_theme: &'static ThemeDefinition,
) -> text_input::Style {
    let mut style = text_input::default(framework_theme, status);
    let border_color = match status {
        text_input::Status::Focused { .. } => active_theme.accent_color().scale_alpha(0.78),
        text_input::Status::Hovered => active_theme.colors.border(0.30),
        _ => active_theme.colors.border(0.18),
    };
    style.background = Background::Color(active_theme.colors.control_surface());
    style.border = Border {
        color: border_color,
        width: 1.0,
        radius: 7.0.into(),
    };
    style.icon = active_theme.colors.muted_text();
    style.placeholder = active_theme.colors.muted_text();
    style.value = active_theme.colors.text();
    style.selection = active_theme.accent_color().scale_alpha(0.42);
    style
}

pub(super) fn account_dialog_button(
    label: &'static str,
    primary: bool,
    enabled: bool,
    active_theme: &'static ThemeDefinition,
    message: Message,
) -> Element<'static, Message> {
    button(
        text(label)
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS),
    )
    .on_press_maybe(enabled.then_some(message))
    .padding([7, 12])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = if primary && enabled {
            Some(Background::Color(active_theme.accent_color()))
        } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            Some(Background::Color(active_theme.colors.hover()))
        } else {
            Some(Background::Color(active_theme.colors.control_surface()))
        };
        style.text_color = if primary && enabled {
            Color::WHITE
        } else if enabled {
            active_theme.colors.text()
        } else {
            active_theme.colors.muted_text()
        };
        style.border = Border {
            color: active_theme.colors.border(0.24),
            width: 1.0,
            radius: 7.0.into(),
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

pub(super) fn account_add_status_banner(
    status: &AccountAddStatus,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let content: Element<'static, Message> = match status {
        AccountAddStatus::Running(provider) => row![
            text(locale::text(language, locale::Text::AccountAddRunning))
                .size(typography::LABEL_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            text(provider.display_name())
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
        ]
        .spacing(5)
        .align_y(Alignment::Center)
        .into(),
        AccountAddStatus::Added(provider) => row![
            text(locale::text(language, locale::Text::AccountAdded))
                .size(typography::LABEL_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
            text(provider.display_name())
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
        ]
        .spacing(5)
        .align_y(Alignment::Center)
        .into(),
        AccountAddStatus::Failed(error) => column![
            text(locale::text(language, locale::Text::AccountAddFailed))
                .size(typography::LABEL_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.danger_hover()),
            text(error.clone())
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.muted_text()),
        ]
        .spacing(2)
        .width(Fill)
        .into(),
    };

    let close_button: Element<'static, Message> = if matches!(status, AccountAddStatus::Running(_))
    {
        button(
            text(locale::text(language, locale::Text::Cancel))
                .size(typography::METADATA_SIZE)
                .font(typography::EMPHASIS),
        )
        .on_press(Message::CancelAccountAdd)
        .padding([3, 9])
        .style(move |framework_theme: &Theme, state| {
            let mut style = button::text(framework_theme, state);
            style.background = Some(Background::Color(
                if matches!(state, button::Status::Hovered | button::Status::Pressed) {
                    active_theme.colors.hover()
                } else {
                    active_theme.colors.control_surface()
                },
            ));
            style.text_color = active_theme.colors.text();
            style.border = Border {
                color: active_theme.colors.border(0.20),
                width: 1.0,
                radius: 6.0.into(),
            };
            style
        })
        .into()
    } else {
        button(container(icon_x().size(13)).center(Fill))
            .on_press(Message::DismissAccountAddStatus)
            .width(22)
            .height(22)
            .padding(0)
            .style(move |framework_theme: &Theme, state| {
                let mut style = button::text(framework_theme, state);
                style.background =
                    matches!(state, button::Status::Hovered | button::Status::Pressed)
                        .then(|| Background::Color(active_theme.colors.hover()));
                style.text_color = active_theme.colors.muted_text();
                style.border = Border {
                    radius: 6.0.into(),
                    ..Border::default()
                };
                style
            })
            .into()
    };

    container(
        row![content, close_button]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([6, 12])
    .style(move |_| container::Style {
        background: Some(Background::Color(active_theme.colors.control_surface())),
        border: Border {
            color: active_theme.colors.border(0.20),
            width: 1.0,
            ..Border::default()
        },
        ..Default::default()
    })
    .into()
}

pub(super) fn account_menu_surface_style(
    active_theme: &'static ThemeDefinition,
) -> container::Style {
    container::Style {
        background: Some(Background::Color(active_theme.colors.window_surface())),
        border: Border {
            color: active_theme.colors.border(0.28),
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow::default(),
        ..Default::default()
    }
}
