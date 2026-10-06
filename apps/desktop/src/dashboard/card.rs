//! The account card: header, identity, and per-account controls.

use super::*;

pub(super) fn account_card(
    entry: &AccountUsageEntry,
    (can_move_up, can_move_down): (bool, bool),
    alias_editor: Option<&AliasEditor>,
    usage_animation: &UsageAnimationState,
    account_name_hovered: bool,
    favorite: bool,
    model_visibility_menu_open: bool,
    model_visibility: &ModelVisibilityPreferences,
    show_all_model_quotas: bool,
    show_antigravity_quota_groups: bool,
    codex_desktop: &CodexDesktopState,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let account = &entry.account;
    let account_id = account.id;
    let editing = alias_editor.filter(|editor| editor.account_id == account_id);
    let mut identity_children: Vec<Element<'static, Message>> = Vec::with_capacity(4);
    let identity_provider = PROVIDER_TABS
        .iter()
        .find(|tab| belongs_to_provider(&account.provider_id, tab.provider))
        .map(|tab| tab.provider);
    if let Some(provider) = identity_provider {
        identity_children.push(
            image(crate::provider_logo_handle(provider, theme.colors.is_light))
                .width(20)
                .height(20)
                .content_fit(ContentFit::Contain)
                .into(),
        );
    }

    if let Some(editor) = editing {
        let input = text_input(locale::text(language, Text::NamePlaceholder), &editor.draft)
            .size(typography::BODY_SIZE)
            .padding([5, 8])
            .on_input(move |draft| Message::AliasDraftChanged(account_id, draft))
            .on_submit(Message::SaveAlias(account_id))
            .width(Length::Fixed(184.0))
            .style(move |framework_theme, status| {
                let mut style = text_input::default(framework_theme, status);
                let border_color = match status {
                    text_input::Status::Focused { .. } => theme.accent_color().scale_alpha(0.78),
                    text_input::Status::Hovered => theme.colors.border(0.30),
                    _ => theme.colors.border(0.18),
                };
                style.background = Background::Color(theme.colors.control_surface());
                style.border = Border {
                    color: border_color,
                    width: 1.0,
                    radius: 7.0.into(),
                };
                style.icon = muted_text(theme);
                style.placeholder = muted_text(theme);
                style.value = theme.colors.text();
                style.selection = theme.accent_color().scale_alpha(0.42);
                style
            });
        let save_label = if editor.is_saving {
            locale::text(language, Text::Saving)
        } else {
            locale::text(language, Text::Save)
        };
        identity_children.push(input.into());
        identity_children.push(
            button(text(save_label).size(typography::CONTROL_SIZE))
                .padding([5, 9])
                .on_press(Message::SaveAlias(account_id))
                .style(move |framework_theme, status| {
                    alias_action_style(framework_theme, theme, true, status)
                })
                .into(),
        );
        identity_children.push(
            button(text(locale::text(language, Text::Cancel)).size(typography::CONTROL_SIZE))
                .padding([5, 9])
                .on_press(Message::CancelAliasEdit(account_id))
                .style(move |framework_theme, status| {
                    alias_action_style(framework_theme, theme, false, status)
                })
                .into(),
        );
    } else {
        let edit_slot: Element<'static, Message> = if account_name_hovered {
            row![
                edit_name_button(account_id, theme, language),
                favorite_button(account_id, favorite, theme, language),
                move_account_button(account_id, -1, can_move_up, theme, language),
                move_account_button(account_id, 1, can_move_down, theme, language),
            ]
            .spacing(1)
            .align_y(Alignment::Center)
            .width(HOVER_CONTROLS_WIDTH)
            .into()
        } else {
            // Reserve the controls' width so hovering never reflows the header.
            space().width(HOVER_CONTROLS_WIDTH).height(24).into()
        };
        let name_and_edit = row![
            text(account_name(account))
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .wrapping(text::Wrapping::None),
            edit_slot,
        ]
        .spacing(5)
        .align_y(Alignment::Center);
        identity_children.push(
            mouse_area(name_and_edit)
                .on_enter(Message::AccountNameHovered(account_id))
                .on_exit(Message::AccountNameHoverEnded(account_id))
                .into(),
        );
    }
    let identity: Element<'static, Message> = iced::widget::Row::with_children(identity_children)
        .spacing(5)
        .align_y(Alignment::Center)
        .into();

    let visibility_snapshot = entry.snapshot.as_ref().filter(|snapshot| {
        providers_match(&account.provider_id, &snapshot.provider_id)
            && !snapshot
                .observed_email
                .as_deref()
                .is_some_and(|email| !email.trim().eq_ignore_ascii_case(account.email.trim()))
    });
    let model_visibility_entries = if model_visibility_menu_open {
        visibility_snapshot
            .map(|snapshot| model_quota_menu_entries(&snapshot.metrics))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let has_model_quotas = visibility_snapshot
        .is_some_and(|snapshot| snapshot.metrics.iter().any(is_model_quota_metric));
    let has_hidden_model_quotas = if model_visibility_menu_open {
        model_visibility_entries
            .iter()
            .any(|(model_id, _)| !model_visibility.is_visible(model_id))
    } else {
        visibility_snapshot
            .is_some_and(|snapshot| has_hidden_model_quota(&snapshot.metrics, model_visibility))
    };

    let spacer_width = if editing.is_some() {
        Length::Shrink
    } else {
        Fill
    };
    let mut header = row![identity, space().width(spacer_width)]
        .spacing(8)
        .align_y(Alignment::Center);
    if let Some(badge) = status_badge(account.status, theme, language) {
        header = header.push(badge);
    }
    let is_antigravity_account =
        belongs_to_provider(&account.provider_id, UsageProvider::Antigravity);
    let has_antigravity_summary = entry.snapshot.as_ref().is_some_and(|snapshot| {
        providers_match(&account.provider_id, &snapshot.provider_id)
            && !snapshot
                .observed_email
                .as_deref()
                .is_some_and(|email| !email.trim().eq_ignore_ascii_case(account.email.trim()))
            && snapshot.metrics.iter().any(is_antigravity_summary_metric)
    });
    if has_model_quotas || (is_antigravity_account && has_antigravity_summary) {
        header = header.push(model_visibility_button(
            account_id,
            model_visibility_menu_open,
            &model_visibility_entries,
            model_visibility,
            has_hidden_model_quotas,
            show_all_model_quotas,
            is_antigravity_account,
            is_antigravity_account && show_antigravity_quota_groups,
            theme,
            language,
        ));
    }
    let desktop_app = if belongs_to_provider(&account.provider_id, UsageProvider::Codex) {
        Some((
            DesktopApp::Codex,
            codex_desktop.active_account == Some(account_id),
        ))
    } else if is_antigravity_account {
        Some((
            DesktopApp::Antigravity,
            codex_desktop
                .antigravity_email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(account.email.trim())),
        ))
    } else {
        None
    };
    if let Some((app, is_active)) = desktop_app
        && editing.is_none()
    {
        header = header.push(desktop_app_button(
            account_id,
            app,
            is_active,
            codex_desktop,
            theme,
            language,
        ));
    }
    let mut rows: Vec<Element<'static, Message>> = vec![header.width(Fill).into()];

    if let Some((app, _)) = desktop_app
        && let Some((_, error)) = codex_desktop
            .failure
            .as_ref()
            .filter(|(failed_account, _)| *failed_account == account_id)
    {
        let message = format!("{}: {error}", locale::text(language, app.texts().failed));
        rows.push(warning_line(&message, theme));
    }

    if editing.is_some_and(|editor| editor.failed) {
        rows.push(warning_line(
            locale::text(language, Text::NameSaveFailed),
            theme,
        ));
    }

    let stored_snapshot = entry.snapshot.as_ref();
    let animated_snapshot = stored_snapshot
        .and_then(|snapshot| usage_animation.animated_snapshot(account_id, snapshot));
    let snapshot = animated_snapshot.as_ref().or(stored_snapshot);
    let plan_type = snapshot.and_then(|snapshot| snapshot.plan_type.clone());
    if crate::display_options::show_account_details()
        && (account.workspace_name.is_some()
            || (!shown_email(&account.email).is_empty() && !name_is_email(account))
            || plan_type.is_some())
    {
        // The email is not repeated when it is already the card's name.
        let email = if name_is_email(account) {
            ""
        } else {
            shown_email(&account.email)
        };
        let mut metadata = row![
            text(email.to_owned())
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(muted_text(theme)),
            space().width(Fill),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .width(Fill);
        if let Some(workspace_name) = account.workspace_name.clone() {
            metadata = metadata.push(
                text(workspace_name)
                    .size(typography::METADATA_SIZE)
                    .font(typography::EMPHASIS)
                    .color(muted_text(theme)),
            );
        }
        if let Some(plan_type) = plan_type {
            metadata = metadata.push(
                text(capitalize_first(&plan_type))
                    .size(typography::METADATA_SIZE)
                    .font(typography::EMPHASIS)
                    .color(theme.colors.text()),
            );
        }
        rows.push(metadata.into());
    }

    if let Some(snapshot) = snapshot {
        if !providers_match(&account.provider_id, &snapshot.provider_id) {
            rows.push(warning_line(
                locale::text(language, Text::ProviderMismatch),
                theme,
            ));
        } else if snapshot
            .observed_email
            .as_deref()
            .is_some_and(|email| !email.trim().eq_ignore_ascii_case(account.email.trim()))
        {
            let observed_email = snapshot.observed_email.as_deref().unwrap_or_default();
            let message = match language {
                Language::English => format!(
                    "{} ({observed_email}); details hidden",
                    locale::text(language, Text::IdentityMismatch)
                ),
                Language::Arabic => format!(
                    "{} ({observed_email})؛ لم نعرض تفاصيلها",
                    locale::text(language, Text::IdentityMismatch)
                ),
            };
            rows.push(warning_line(&message, theme));
        } else {
            append_snapshot_rows(
                &mut rows,
                snapshot,
                !belongs_to_provider(&account.provider_id, UsageProvider::Codex),
                model_visibility,
                show_all_model_quotas,
                is_antigravity_account && show_antigravity_quota_groups,
                theme,
                language,
            );
        }
    } else {
        rows.push(warning_line(
            locale::text(language, Text::NoSavedUsage),
            theme,
        ));
    }

    container(column(rows).spacing(5).width(Fill))
        .width(Fill)
        .padding([9, 10])
        .into()
}

pub(super) fn account_separator(
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    container(space().height(Length::Fill))
        .width(Fill)
        .height(1)
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.text().scale_alpha(0.24))),
            ..Default::default()
        })
        .into()
}

/// "Use in Codex" for a saved Codex account, or a marker on the account the
/// Codex desktop app is currently signed in with.
/// A desktop app a saved account can be switched into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DesktopApp {
    Codex,
    Antigravity,
}

pub(super) struct DesktopAppTexts {
    use_label: Text,
    use_hint: Text,
    active_label: Text,
    active_hint: Text,
    failed: Text,
}

impl DesktopApp {
    fn texts(self) -> DesktopAppTexts {
        match self {
            DesktopApp::Codex => DesktopAppTexts {
                use_label: Text::UseInCodex,
                use_hint: Text::UseInCodexHint,
                active_label: Text::InCodex,
                active_hint: Text::InCodexHint,
                failed: Text::CodexSwitchFailed,
            },
            DesktopApp::Antigravity => DesktopAppTexts {
                use_label: Text::UseInAntigravity,
                use_hint: Text::UseInAntigravityHint,
                active_label: Text::InAntigravity,
                active_hint: Text::InAntigravityHint,
                failed: Text::AntigravitySwitchFailed,
            },
        }
    }

    fn switch_message(self, account_id: AccountId) -> Message {
        match self {
            DesktopApp::Codex => Message::SwitchCodexDesktopAccount(account_id),
            DesktopApp::Antigravity => Message::SwitchAntigravityAppAccount(account_id),
        }
    }
}

/// Icon button that signs a desktop app in with this account, or marks the
/// account the app currently uses.
pub(super) fn desktop_app_button(
    account_id: AccountId,
    app: DesktopApp,
    is_active: bool,
    codex_desktop: &CodexDesktopState,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let texts = app.texts();
    let is_switching = codex_desktop.switching == Some(account_id);
    let (glyph, label, tip) = if is_switching {
        (
            icon_arrow_left_right(),
            Text::CodexSwitching,
            Text::CodexSwitching,
        )
    } else if is_active {
        (icon_check(), texts.active_label, texts.active_hint)
    } else {
        (icon_arrow_left_right(), texts.use_label, texts.use_hint)
    };
    let accent = theme.accent_color();
    let glyph_color = if is_active {
        accent
    } else if is_switching {
        muted_text(theme)
    } else {
        theme.colors.text()
    };
    // Icon-only so it always fits beside long account names; the tooltip
    // names the action.
    let mut control = button(container(glyph.size(14).color(glyph_color)).center(26))
        .width(26)
        .height(26)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            style.background = Some(Background::Color(if is_active {
                accent.scale_alpha(0.18)
            } else if hovered {
                theme.colors.hover()
            } else {
                theme.colors.control_surface()
            }));
            style.text_color = theme.colors.text();
            style.border = Border {
                color: if is_active {
                    accent.scale_alpha(0.62)
                } else {
                    theme.colors.border(0.18)
                },
                width: 1.0,
                radius: 7.0.into(),
            };
            style.shadow = Default::default();
            style
        });
    // The active account can be pressed again to restart Codex on it.
    if codex_desktop.switching.is_none() {
        control = control.on_press(app.switch_message(account_id));
    }

    let tip = if is_switching {
        locale::text(language, tip).to_owned()
    } else {
        format!(
            "{} · {}",
            locale::text(language, label),
            locale::text(language, tip)
        )
    };
    crate::hint::hint(control, tip, theme)
}

pub(super) fn move_account_button(
    account_id: AccountId,
    offset: isize,
    enabled: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let glyph = if offset < 0 {
        icon_chevron_up()
    } else {
        icon_chevron_down()
    };
    let color = if enabled {
        theme.colors.text()
    } else {
        muted_text(theme).scale_alpha(0.45)
    };
    let mut control = button(container(glyph.size(13).color(color)).center(22))
        .width(22)
        .height(24)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = (enabled
                && matches!(status, button::Status::Hovered | button::Status::Pressed))
            .then(|| Background::Color(theme.colors.hover()));
            style.text_color = theme.colors.text();
            style.border = Border {
                radius: 6.0.into(),
                ..Border::default()
            };
            style.shadow = Default::default();
            style
        });
    if enabled {
        control = control.on_press(Message::MoveAccount(account_id, offset));
    }
    let tip = if offset < 0 {
        Text::MoveAccountUp
    } else {
        Text::MoveAccountDown
    };
    crate::hint::hint(control, locale::text(language, tip), theme)
}

/// Stars the account for the Favorites tab, or removes its star.
pub(super) fn favorite_button(
    account_id: AccountId,
    favorite: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let color = if favorite {
        FAVORITE_STAR_COLOR
    } else {
        theme.colors.text()
    };
    let control = button(container(icon_star().size(13).color(color)).center(24))
        .on_press(Message::ToggleFavorite(account_id))
        .width(24)
        .height(24)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = matches!(status, button::Status::Hovered | button::Status::Pressed)
                .then(|| Background::Color(theme.colors.hover()));
            style.text_color = theme.colors.text();
            style.border = Border {
                radius: 6.0.into(),
                ..Border::default()
            };
            style.shadow = Default::default();
            style
        });
    let tip = if favorite {
        Text::RemoveFromFavorites
    } else {
        Text::AddToFavorites
    };
    crate::hint::hint(control, locale::text(language, tip), theme)
}

/// Gold, so a starred account reads as starred on every theme.
const FAVORITE_STAR_COLOR: Color = Color::from_rgb(0.98, 0.76, 0.18);

pub(super) fn edit_name_button(
    account_id: AccountId,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let edit_button =
        button(container(icon_pencil().size(13).color(theme.colors.text())).center(24))
            .on_press(Message::BeginAliasEdit(account_id))
            .width(24)
            .height(24)
            .padding(0)
            .style(move |framework_theme, status| {
                let mut style = button::text(framework_theme, status);
                style.background =
                    matches!(status, button::Status::Hovered | button::Status::Pressed)
                        .then(|| Background::Color(theme.colors.hover()));
                style.text_color = theme.colors.text();
                style.border = Border {
                    radius: 6.0.into(),
                    ..Border::default()
                };
                style.shadow = Default::default();
                style
            });

    crate::hint::hint(edit_button, locale::text(language, Text::EditName), theme)
}

pub(super) fn alias_action_style(
    framework_theme: &iced::Theme,
    theme: &'static crate::theme::ThemeDefinition,
    primary: bool,
    status: button::Status,
) -> button::Style {
    let mut style = button::text(framework_theme, status);
    let is_hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let background = if is_hovered {
        theme.colors.hover()
    } else if primary {
        theme.accent_color().scale_alpha(0.14)
    } else {
        theme.colors.control_surface()
    };
    style.background = Some(Background::Color(background));
    style.text_color = theme.colors.text();
    style.border = Border {
        color: if primary {
            theme.accent_color().scale_alpha(0.62)
        } else {
            theme.colors.border(0.18)
        },
        width: 1.0,
        radius: 7.0.into(),
    };
    style.shadow = Default::default();
    style
}

pub(super) fn model_visibility_button(
    account_id: AccountId,
    menu_open: bool,
    model_entries: &[(String, String)],
    model_visibility: &ModelVisibilityPreferences,
    has_hidden_model_quotas: bool,
    show_all_model_quotas: bool,
    is_antigravity_account: bool,
    show_antigravity_quota_groups: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let glyph = if has_hidden_model_quotas {
        icon_eye_off()
    } else {
        icon_eye()
    };
    let button = button(container(glyph.size(14).color(theme.colors.text())).center(26))
        .on_press(Message::ToggleModelVisibilityMenu(account_id))
        .width(26)
        .height(26)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = (menu_open
                || matches!(status, button::Status::Hovered | button::Status::Pressed))
            .then(|| Background::Color(theme.colors.hover()));
            style.text_color = theme.colors.text();
            style.border = Border {
                radius: 7.0.into(),
                ..Border::default()
            };
            style.shadow = Default::default();
            style
        });

    let trigger = crate::hint::hint(button, locale::text(language, Text::ModelVisibility), theme);
    let menu: Element<'static, Message> = if menu_open {
        model_visibility_menu(
            account_id,
            model_entries,
            model_visibility,
            show_all_model_quotas,
            is_antigravity_account,
            show_antigravity_quota_groups,
            theme,
            language,
        )
    } else {
        space().into()
    };

    ModelVisibilityTrigger {
        trigger,
        menu,
        menu_open,
        dismiss_message: Message::CloseModelVisibilityMenu(account_id),
    }
    .into()
}
