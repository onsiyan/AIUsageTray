//! The tab manager: show, hide, and order the tabs, and create, edit, or
//! delete custom tabs that gather several providers.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;
use lucide_icons::iced::{
    icon_chevron_down, icon_chevron_up, icon_eye, icon_eye_off, icon_layers, icon_panels_top_left,
    icon_pencil, icon_plus, icon_square, icon_square_check,
};
use tabs::{ProviderSet, TabEntry, TabKind};

/// A custom tab being created (`id: None`) or edited.
#[derive(Debug, Clone, Default)]
pub(super) struct TabEditor {
    pub id: Option<u32>,
    pub name: String,
    pub providers: ProviderSet,
}

impl TabEditor {
    fn can_save(&self) -> bool {
        !self.name.trim().is_empty() && !self.providers.is_empty()
    }
}

impl App {
    pub(super) fn save_tab_layout(&self) {
        if let Err(error) = tabs::save(&self.tab_layout) {
            preview_log(format!("tab layout save failed: {error}"));
        }
    }

    pub(super) fn close_tab_manager(&mut self) {
        self.tab_manager_open = false;
        self.tab_editor = None;
    }

    /// Handles every tab-manager message.
    pub(super) fn update_tabs(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ToggleTabManager => {
                if self.tab_manager_open {
                    self.close_tab_manager();
                } else if !self.account_add_running && self.credentials_provider.is_none() {
                    self.tab_manager_open = true;
                    self.theme_menu_open = false;
                    self.account_add_menu_open = false;
                    self.dismiss_account_delete_dialog();
                }
            }
            Message::DismissTabManager => self.close_tab_manager(),
            Message::ToggleTabVisible(index) => {
                if self.tab_layout.toggle_visible(index) {
                    self.selected_tab = self.tab_layout.resolve(self.selected_tab);
                    self.save_tab_layout();
                }
            }
            Message::MoveTab(index, offset) => {
                if self.tab_layout.move_tab(index, offset) {
                    self.save_tab_layout();
                }
            }
            Message::NewCustomTab => self.tab_editor = Some(TabEditor::default()),
            Message::EditCustomTab(id) => {
                self.tab_editor = self.tab_layout.custom_tab(id).map(|custom| TabEditor {
                    id: Some(id),
                    name: custom.name.clone(),
                    providers: custom.providers,
                });
            }
            Message::DeleteCustomTab(id) => {
                self.tab_layout.remove_custom(id);
                self.selected_tab = self.tab_layout.resolve(self.selected_tab);
                self.save_tab_layout();
            }
            Message::TabEditorNameChanged(name) => {
                if let Some(editor) = &mut self.tab_editor {
                    editor.name = name.chars().take(tabs::MAX_CUSTOM_TAB_NAME).collect();
                }
            }
            Message::TabEditorToggleProvider(provider) => {
                if let Some(editor) = &mut self.tab_editor {
                    editor.providers.toggle(provider);
                }
            }
            Message::SaveTabEditor => {
                let Some(editor) = self.tab_editor.take_if(|editor| editor.can_save()) else {
                    return Task::none();
                };
                let tab = match editor.id {
                    Some(id) => {
                        self.tab_layout
                            .update_custom(id, &editor.name, editor.providers);
                        DashboardTab::Custom {
                            id,
                            providers: editor.providers,
                        }
                    }
                    None => self.tab_layout.add_custom(&editor.name, editor.providers),
                };
                self.save_tab_layout();
                // Open the saved tab, so its accounts show right away.
                self.selected_tab = self.tab_layout.resolve(tab);
                return self.start_usage_refresh(usage_refresh::RefreshTrigger::Automatic);
            }
            Message::CancelTabEditor => self.tab_editor = None,
            _ => {}
        }
        Task::none()
    }
}

pub(super) fn tab_manager_button(
    open: bool,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let button = button(container(icon_panels_top_left().size(16)).center(Fill))
        .on_press(Message::ToggleTabManager)
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let mut style = button::text(theme, status);
            style.background =
                if open || matches!(status, button::Status::Hovered | button::Status::Pressed) {
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
        locale::text(language, locale::Text::Tabs),
        active_theme,
    )
}

pub(super) fn tab_manager_dialog(
    layout: &tabs::TabLayout,
    editor: Option<&TabEditor>,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let content = match editor {
        Some(editor) => tab_editor_content(editor, language, active_theme),
        None => tab_list_content(layout, language, active_theme),
    };
    container(content)
        .width(380)
        .padding(16)
        .style(move |_| account_menu_surface_style(active_theme))
        .into()
}

fn tab_list_content(
    layout: &tabs::TabLayout,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let entries = layout.entries();
    let visible_count = layout.visible_tabs().count();
    let rows = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            tab_row(
                index,
                entry,
                (index > 0, index + 1 < entries.len()),
                entry.visible && visible_count == 1,
                language,
                active_theme,
            )
        })
        .collect::<Vec<_>>();
    let list_height = (entries.len() as f32 * 38.0).clamp(120.0, 400.0);

    column![
        dialog_title(locale::text(language, locale::Text::Tabs), active_theme),
        dialog_note(locale::text(language, locale::Text::TabsHint), active_theme),
        crate::smooth_scroll::smooth_scroll(
            "tab-manager",
            scrollable(column(rows).spacing(2).width(Fill))
                .direction(iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::hidden(),
                ))
                .height(Length::Fixed(list_height)),
        ),
        row![
            icon_text_button(
                icon_plus().size(14).into(),
                locale::text(language, locale::Text::NewTab),
                Message::NewCustomTab,
                active_theme,
            ),
            Space::new().width(Fill).height(1),
            account_dialog_button(
                locale::text(language, locale::Text::Done),
                true,
                true,
                active_theme,
                Message::DismissTabManager,
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .width(Fill),
    ]
    .spacing(10)
    .width(Fill)
    .into()
}

fn tab_row(
    index: usize,
    entry: &TabEntry,
    (can_move_up, can_move_down): (bool, bool),
    is_last_shown: bool,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let (icon, name): (Element<'static, Message>, String) = match &entry.kind {
        TabKind::Provider(provider) => (
            image(provider_logo_handle(
                *provider,
                active_theme.colors.is_light,
            ))
            .width(20)
            .height(20)
            .content_fit(ContentFit::Contain)
            .into(),
            provider.display_name().to_owned(),
        ),
        TabKind::Favorites => (
            icon_star::<Theme>()
                .size(17)
                .color(active_theme.colors.text())
                .into(),
            locale::text(language, locale::Text::Favorites).to_owned(),
        ),
        TabKind::Custom(custom) => (
            icon_layers::<Theme>()
                .size(17)
                .color(active_theme.colors.text())
                .into(),
            custom.name.clone(),
        ),
    };
    let name_color = if entry.visible {
        active_theme.colors.text()
    } else {
        active_theme.colors.muted_text()
    };

    let visibility = small_icon_button(
        if entry.visible {
            icon_eye().size(15).into()
        } else {
            icon_eye_off().size(15).into()
        },
        (!is_last_shown).then_some(Message::ToggleTabVisible(index)),
        active_theme,
    );
    let mut controls = row![visibility].spacing(2).align_y(Alignment::Center);
    let mut trailing = row![].spacing(2).align_y(Alignment::Center);
    if let TabKind::Custom(custom) = &entry.kind {
        trailing = trailing
            .push(small_icon_button(
                icon_pencil().size(14).into(),
                Some(Message::EditCustomTab(custom.id)),
                active_theme,
            ))
            .push(small_icon_button(
                icon_trash_2().size(14).into(),
                Some(Message::DeleteCustomTab(custom.id)),
                active_theme,
            ));
    }
    trailing = trailing
        .push(small_icon_button(
            icon_chevron_up().size(15).into(),
            can_move_up.then_some(Message::MoveTab(index, -1)),
            active_theme,
        ))
        .push(small_icon_button(
            icon_chevron_down().size(15).into(),
            can_move_down.then_some(Message::MoveTab(index, 1)),
            active_theme,
        ));
    controls = controls.push(container(icon).width(24).height(24).center(24));

    container(
        row![
            controls,
            text(name)
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(name_color)
                .width(Fill),
            trailing,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([3, 6])
    .width(Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(active_theme.colors.control_surface())),
        border: Border {
            color: active_theme.colors.border(0.12),
            width: 1.0,
            radius: 7.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn tab_editor_content(
    editor: &TabEditor,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let title = locale::text(
        language,
        if editor.id.is_some() {
            locale::Text::EditTab
        } else {
            locale::Text::NewTab
        },
    );
    let name_input = text_input(locale::text(language, locale::Text::TabName), &editor.name)
        .on_input(Message::TabEditorNameChanged)
        .on_submit(Message::SaveTabEditor)
        .size(typography::LABEL_SIZE)
        .font(typography::MEDIUM)
        .padding([8, 10])
        .width(Fill)
        .style(move |framework_theme, status| {
            account_key_input_style(framework_theme, status, active_theme)
        });

    let providers = PROVIDER_TABS
        .iter()
        .map(|tab| {
            let checked = editor.providers.contains(tab.provider);
            let check: Element<'static, Message> = if checked {
                icon_square_check::<Theme>()
                    .size(17)
                    .color(active_theme.colors.text())
                    .into()
            } else {
                icon_square::<Theme>()
                    .size(17)
                    .color(active_theme.colors.muted_text())
                    .into()
            };
            button(
                row![
                    check,
                    image(provider_logo_handle(
                        tab.provider,
                        active_theme.colors.is_light
                    ))
                    .width(20)
                    .height(20)
                    .content_fit(ContentFit::Contain),
                    text(tab.label)
                        .size(typography::LABEL_SIZE)
                        .font(typography::MEDIUM)
                        .color(active_theme.colors.text())
                        .width(Fill),
                ]
                .spacing(9)
                .align_y(Alignment::Center),
            )
            .on_press(Message::TabEditorToggleProvider(tab.provider))
            .width(Fill)
            .height(32)
            .padding([3, 8])
            .style(move |framework_theme: &Theme, status| {
                let mut style = button::text(framework_theme, status);
                style.background = (checked
                    || matches!(status, button::Status::Hovered | button::Status::Pressed))
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
        })
        .collect::<Vec<_>>();

    column![
        dialog_title(title, active_theme),
        name_input,
        dialog_note(
            locale::text(language, locale::Text::TabProviders),
            active_theme
        ),
        column(providers).spacing(2),
        row![
            Space::new().width(Fill).height(1),
            account_dialog_button(
                locale::text(language, locale::Text::Cancel),
                false,
                true,
                active_theme,
                Message::CancelTabEditor,
            ),
            account_dialog_button(
                locale::text(language, locale::Text::Save),
                true,
                editor.can_save(),
                active_theme,
                Message::SaveTabEditor,
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .width(Fill),
    ]
    .spacing(10)
    .width(Fill)
    .into()
}

fn dialog_title(
    title: &'static str,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    text(title)
        .size(typography::ACCOUNT_NAME_SIZE)
        .font(typography::EMPHASIS)
        .color(active_theme.colors.text())
        .into()
}

fn dialog_note(
    note: &'static str,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    text(note)
        .size(typography::METADATA_SIZE)
        .font(typography::MEDIUM)
        .color(active_theme.colors.muted_text())
        .into()
}

fn small_icon_button(
    icon: Element<'static, Message>,
    message: Option<Message>,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let enabled = message.is_some();
    button(container(icon).center(Fill))
        .on_press_maybe(message)
        .width(26)
        .height(26)
        .padding(0)
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = (enabled
                && matches!(status, button::Status::Hovered | button::Status::Pressed))
            .then(|| Background::Color(active_theme.colors.hover()));
            style.text_color = if enabled {
                active_theme.colors.text()
            } else {
                active_theme.colors.muted_text().scale_alpha(0.4)
            };
            style.border = Border {
                radius: 6.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
}

fn icon_text_button(
    icon: Element<'static, Message>,
    label: &'static str,
    message: Message,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    button(
        row![
            icon,
            text(label)
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(message)
    .padding([7, 12])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = Some(Background::Color(
            if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                active_theme.colors.hover()
            } else {
                active_theme.colors.control_surface()
            },
        ));
        style.text_color = active_theme.colors.text();
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
