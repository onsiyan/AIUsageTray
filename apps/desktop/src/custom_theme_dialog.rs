//! The dialog that edits the custom theme. Every change applies at once.

use super::*;
use custom_theme::{CustomTheme, Dim};
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;

impl App {
    /// Makes `theme` the custom theme, saves it, and shows it.
    pub(super) fn apply_custom_theme(&mut self, theme: CustomTheme, image_changed: bool) {
        custom_theme::apply(theme);
        if let Err(error) = custom_theme::save(theme) {
            preview_log(format!("custom theme save failed: {error}"));
        }
        let switched = self.theme_id != ThemeId::Custom;
        self.theme_id = ThemeId::Custom;
        if switched || image_changed {
            self.backdrop_image = backdrop_image_handle(ThemeId::Custom);
        }
        if switched && let Err(error) = save_theme(ThemeId::Custom) {
            preview_log(format!("theme preference save failed: {error}"));
        }
    }

    pub(super) fn update_custom_theme(&mut self, message: Message) -> Task<Message> {
        let mut theme = custom_theme::settings();
        match message {
            Message::OpenCustomTheme => {
                self.theme_menu_open = false;
                self.custom_theme_open = true;
                self.custom_image_error = None;
                self.custom_background_input = custom_theme::to_hex(theme.background);
                self.custom_accent_input = custom_theme::to_hex(theme.accent);
                // Opening the editor shows what is being edited.
                self.apply_custom_theme(theme, false);
            }
            Message::CloseCustomTheme => {
                if !self.custom_image_picking {
                    self.custom_theme_open = false;
                }
            }
            Message::CustomThemeLight(light) => {
                theme.light = light;
                self.apply_custom_theme(theme, false);
            }
            Message::CustomThemeBackground(color) => {
                theme.background = color;
                self.custom_background_input = custom_theme::to_hex(color);
                self.apply_custom_theme(theme, false);
            }
            Message::CustomThemeAccent(color) => {
                theme.accent = color;
                self.custom_accent_input = custom_theme::to_hex(color);
                self.apply_custom_theme(theme, false);
            }
            Message::CustomThemeBackgroundInput(input) => {
                if let Some(color) = custom_theme::parse_hex(&input) {
                    theme.background = color;
                    self.apply_custom_theme(theme, false);
                }
                self.custom_background_input = input;
            }
            Message::CustomThemeAccentInput(input) => {
                if let Some(color) = custom_theme::parse_hex(&input) {
                    theme.accent = color;
                    self.apply_custom_theme(theme, false);
                }
                self.custom_accent_input = input;
            }
            Message::CustomThemeDim(dim) => {
                theme.dim = dim;
                self.apply_custom_theme(theme, false);
            }
            Message::ChooseCustomImage => {
                if !self.custom_image_picking {
                    self.custom_image_picking = true;
                    self.custom_image_error = None;
                    return Task::perform(
                        async { custom_theme::choose_image() },
                        Message::CustomImageChosen,
                    );
                }
            }
            Message::CustomImageChosen(result) => {
                self.custom_image_picking = false;
                match result {
                    Ok(true) => {
                        theme.image = true;
                        self.apply_custom_theme(theme, true);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        preview_log(format!("custom image failed: {error}"));
                        self.custom_image_error = Some(error);
                    }
                }
            }
            Message::RemoveCustomImage => {
                if let Err(error) = custom_theme::remove_image() {
                    preview_log(format!("custom image removal failed: {error}"));
                }
                theme.image = false;
                self.apply_custom_theme(theme, true);
            }
            _ => {}
        }
        Task::none()
    }

    pub(super) fn custom_theme_dialog(&self) -> Element<'_, Message> {
        let active_theme = self.theme_id.definition();
        let language = self.language;
        let theme = custom_theme::settings();
        let label = |text_id: locale::Text| locale::text(language, text_id);

        let base = row![
            option_button(
                label(locale::Text::CustomThemeDark),
                !theme.light,
                Message::CustomThemeLight(false),
                active_theme,
            ),
            option_button(
                label(locale::Text::CustomThemeLight),
                theme.light,
                Message::CustomThemeLight(true),
                active_theme,
            ),
        ]
        .spacing(6);

        let background = color_choices(
            &custom_theme::BACKGROUND_PRESETS,
            theme.background,
            Message::CustomThemeBackground,
            &self.custom_background_input,
            Message::CustomThemeBackgroundInput,
            active_theme,
        );
        let accent = color_choices(
            &custom_theme::ACCENT_PRESETS,
            theme.accent,
            Message::CustomThemeAccent,
            &self.custom_accent_input,
            Message::CustomThemeAccentInput,
            active_theme,
        );

        let mut image_section = column![
            row![account_dialog_button(
                label(if theme.image {
                    locale::Text::CustomThemeChangeImage
                } else {
                    locale::Text::CustomThemeChooseImage
                }),
                false,
                !self.custom_image_picking,
                active_theme,
                Message::ChooseCustomImage,
            ),]
            .push(theme.image.then(|| {
                account_dialog_button(
                    label(locale::Text::CustomThemeRemoveImage),
                    false,
                    !self.custom_image_picking,
                    active_theme,
                    Message::RemoveCustomImage,
                )
            }))
            .spacing(6),
        ]
        .spacing(8);
        if theme.image {
            image_section = image_section.push(section_label(
                label(locale::Text::CustomThemeDim),
                active_theme,
            ));
            image_section = image_section.push(
                row(Dim::ALL.into_iter().map(|dim| {
                    option_button(
                        label(match dim {
                            Dim::Light => locale::Text::CustomThemeDimLight,
                            Dim::Medium => locale::Text::CustomThemeDimMedium,
                            Dim::Strong => locale::Text::CustomThemeDimStrong,
                        }),
                        theme.dim == dim,
                        Message::CustomThemeDim(dim),
                        active_theme,
                    )
                }))
                .spacing(6),
            );
        }
        if let Some(error) = &self.custom_image_error {
            image_section = image_section.push(
                text(error.clone())
                    .size(typography::METADATA_SIZE)
                    .color(active_theme.colors.danger_hover()),
            );
        }

        let content = column![
            text(label(locale::Text::CustomThemeTitle))
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            section_label(label(locale::Text::CustomThemeBase), active_theme),
            base,
            section_label(label(locale::Text::CustomThemeBackground), active_theme),
            background,
            section_label(label(locale::Text::CustomThemeAccent), active_theme),
            accent,
            section_label(label(locale::Text::CustomThemeImage), active_theme),
            image_section,
            row![
                Space::new().width(Fill),
                account_dialog_button(
                    label(locale::Text::CustomThemeDone),
                    true,
                    !self.custom_image_picking,
                    active_theme,
                    Message::CloseCustomTheme,
                ),
            ],
        ]
        .spacing(9);

        container(content)
            .width(340)
            .padding(16)
            .style(move |_| account_menu_surface_style(active_theme))
            .into()
    }
}

fn section_label(
    label: &'static str,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    text(label)
        .size(typography::METADATA_SIZE)
        .font(typography::EMPHASIS)
        .color(active_theme.colors.muted_text())
        .into()
}

fn option_button(
    label: &'static str,
    selected: bool,
    message: Message,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    button(
        text(label)
            .size(typography::LABEL_SIZE)
            .font(typography::MEDIUM)
            .color(active_theme.colors.text()),
    )
    .on_press(message)
    .padding([5, 12])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        style.background = Some(Background::Color(if selected {
            active_theme.colors.text().scale_alpha(0.16)
        } else if hovered {
            active_theme.colors.hover()
        } else {
            active_theme.colors.control_surface()
        }));
        style.border = Border {
            color: active_theme
                .colors
                .text()
                .scale_alpha(if selected { 0.6 } else { 0.18 }),
            width: 1.0,
            radius: 7.0.into(),
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

fn color_choices<'a>(
    presets: &[[u8; 3]],
    current: [u8; 3],
    on_pick: fn([u8; 3]) -> Message,
    input: &'a str,
    on_input: fn(String) -> Message,
    active_theme: &'static ThemeDefinition,
) -> Element<'a, Message> {
    let swatches = row(presets.iter().copied().map(|color| {
        let selected = color == current;
        button(Space::new().width(Fill).height(Fill))
            .on_press(on_pick(color))
            .width(24)
            .height(24)
            .padding(0)
            .style(move |_, _| button::Style {
                background: Some(Background::Color(Color::from_rgb8(
                    color[0], color[1], color[2],
                ))),
                border: Border {
                    color: if selected {
                        active_theme.colors.text()
                    } else {
                        active_theme.colors.border(0.35)
                    },
                    width: if selected { 2.5 } else { 1.0 },
                    radius: 6.0.into(),
                },
                ..button::Style::default()
            })
            .into()
    }))
    .spacing(6);
    let hex = text_input("#000000", input)
        .on_input(on_input)
        .size(typography::LABEL_SIZE)
        .padding([4, 8])
        .width(92)
        .style(move |framework_theme, status| {
            account_key_input_style(framework_theme, status, active_theme)
        });
    column![swatches, hex].spacing(6).into()
}
