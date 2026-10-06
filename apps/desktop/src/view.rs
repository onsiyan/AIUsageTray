//! Builds the popup window from the app state.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;

impl App {
    pub(super) fn view(&self) -> Element<'_, Message> {
        let active_theme = self.theme_id.definition();
        let provider_tab_bar = provider_tab_bar(
            &self.tab_layout,
            self.selected_tab,
            active_theme,
            self.language,
        );
        let title_bar = container(
            row![
                mouse_area(Space::new().width(Fill).height(Length::Fill))
                    .on_press(Message::DragWindow),
                add_account_button(self.account_add_running, active_theme, self.language),
                delete_account_button(
                    self.account_delete_dialog_open,
                    self.account_add_running
                        || self.credentials_provider.is_some()
                        || self.account_delete_running,
                    active_theme,
                    self.language,
                ),
                refresh_button(self.dashboard_refresh_running, active_theme, self.language,),
                tab_manager_button(self.tab_manager_open, active_theme, self.language),
                theme_button(active_theme),
                close_window_button(active_theme),
            ]
            .spacing(4)
            .align_y(Alignment::Center)
            .width(Fill),
        )
        .width(Fill)
        .height(44)
        .padding([4, 10])
        .style(move |_| container::Style {
            background: Some(Background::Color(if active_theme.colors.is_light {
                active_theme.colors.control_surface()
            } else {
                Color::from_rgba(0.0, 0.0, 0.0, TITLE_BAR_SHADE_OPACITY)
            })),
            border: Border {
                radius: iced::border::Radius::default().top(WINDOW_FRAME_RADIUS),
                ..Border::default()
            },
            ..Default::default()
        });

        let title_bar_separator = container(Space::new().width(Fill).height(Length::Fill))
            .width(Fill)
            .height(1)
            .style(move |_| container::Style {
                background: Some(Background::Color(active_theme.colors.border(
                    if active_theme.colors.is_light {
                        0.38
                    } else {
                        0.16
                    },
                ))),
                ..Default::default()
            });

        let account_add_status: Element<'_, Message> = self
            .account_add_status
            .as_ref()
            .map(|status| account_add_status_banner(status, active_theme, self.language))
            .unwrap_or_else(|| Space::new().width(Fill).height(0).into());

        let foreground = column![
            title_bar,
            title_bar_separator,
            provider_tab_bar,
            account_add_status,
            dashboard::view(
                &self.dashboard,
                self.selected_tab,
                active_theme,
                self.language,
            )
        ]
        .spacing(0)
        .width(Fill)
        .height(Fill);

        let backdrop: Element<'_, Message> = if let Some(backdrop_image) = &self.backdrop_image {
            image(backdrop_image.clone())
                .width(Fill)
                .height(Fill)
                .content_fit(ContentFit::Cover)
                .opacity(
                    active_theme
                        .backdrop
                        .map_or(1.0, |backdrop| backdrop.image_opacity),
                )
                .into()
        } else {
            Space::new().width(Fill).height(Fill).into()
        };

        let shade: Element<'_, Message> = if let Some(backdrop) = active_theme.backdrop {
            container(Space::new().width(Fill).height(Fill))
                .width(Fill)
                .height(Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(Color::from_rgba(
                        0.0,
                        0.0,
                        0.0,
                        backdrop.shade_opacity,
                    ))),
                    border: Border {
                        radius: WINDOW_FRAME_RADIUS.into(),
                        ..Border::default()
                    },
                    ..Default::default()
                })
                .into()
        } else {
            Space::new().width(Fill).height(Fill).into()
        };

        let page: Element<'_, Message> = stack![backdrop, shade, foreground]
            .width(Fill)
            .height(Fill)
            .into();

        let content = if self.tab_manager_open {
            let dismiss_area = mouse_area(Space::new().width(Fill).height(Fill))
                .on_press(Message::DismissTabManager);
            let dialog = container(tab_manager_dialog(
                &self.tab_layout,
                self.tab_editor.as_ref(),
                self.language,
                active_theme,
            ))
            .width(Fill)
            .height(Fill)
            .center(Fill);

            stack![page, dialog_scrim(), dismiss_area, dialog]
                .width(Fill)
                .height(Fill)
                .into()
        } else if let Some(credentials_provider) = self.credentials_provider {
            let dismiss_area = mouse_area(Space::new().width(Fill).height(Fill))
                .on_press(Message::CancelCredentials);
            let credentials_dialog = container(api_key_dialog(
                credentials_provider,
                &self.api_key_input,
                &self.management_key_input,
                self.language,
                active_theme,
            ))
            .width(Fill)
            .height(Fill)
            .center(Fill);

            stack![page, dialog_scrim(), dismiss_area, credentials_dialog]
                .width(Fill)
                .height(Fill)
                .into()
        } else if self.account_delete_dialog_open {
            let scrim = dialog_scrim();
            let dismiss_area = mouse_area(Space::new().width(Fill).height(Fill))
                .on_press(Message::DismissAccountDeleteDialog);
            let dialog = if let Some(pending) = &self.pending_account_deletion {
                account_deletion_confirmation_dialog(
                    pending,
                    self.account_delete_running,
                    self.account_delete_queued,
                    self.account_delete_error.as_deref(),
                    self.language,
                    active_theme,
                )
            } else {
                account_deletion_picker_dialog(
                    self.dashboard.account_entries(),
                    self.language,
                    active_theme,
                )
            };
            let dialog_layer = container(dialog).width(Fill).height(Fill).center(Fill);

            stack![page, scrim, dismiss_area, dialog_layer]
                .width(Fill)
                .height(Fill)
                .into()
        } else if self.account_add_menu_open {
            let dismiss_area = column![
                Space::new().width(Fill).height(45),
                mouse_area(Space::new().width(Fill).height(Fill))
                    .on_press(Message::DismissAccountAddMenu),
            ]
            .width(Fill)
            .height(Fill);

            let account_menu_layer = container(account_add_dropdown(self.language, active_theme))
                .width(Fill)
                .height(Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::Start)
                .padding([44, 110]);

            stack![page, dismiss_area, account_menu_layer]
                .width(Fill)
                .height(Fill)
                .into()
        } else if self.theme_menu_open {
            let dismiss_area = column![
                Space::new().width(Fill).height(45),
                mouse_area(Space::new().width(Fill).height(Fill))
                    .on_press(Message::DismissThemeMenu),
            ]
            .width(Fill)
            .height(Fill);

            let theme_menu_layer = container(theme_dropdown(
                self.theme_id,
                self.language,
                self.memory_saver,
                active_theme,
            ))
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::Start)
            .padding([44, 50]);

            stack![page, dismiss_area, theme_menu_layer]
                .width(Fill)
                .height(Fill)
                .into()
        } else {
            page
        };

        // Draw the frame last so backdrop images cannot cover its rim.
        let frame_outline = container(Space::new().width(Fill).height(Fill))
            .width(Fill)
            .height(Fill)
            .style(move |_| window_frame_outline_style(active_theme));
        let content = stack![content, frame_outline].width(Fill).height(Fill);

        container(content)
            .width(Fill)
            .height(Fill)
            .style(move |_| window_frame_style(active_theme))
            .into()
    }
}

/// Dims the popup behind a dialog so the dialog stands apart from it.
fn dialog_scrim<'a>() -> Element<'a, Message> {
    container(Space::new().width(Fill).height(Fill))
        .width(Fill)
        .height(Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.54))),
            ..Default::default()
        })
        .into()
}
