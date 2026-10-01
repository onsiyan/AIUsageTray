//! The model visibility menu anchored to an account card.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;

pub(super) struct ModelVisibilityTrigger<'a> {
    pub(super) trigger: Element<'a, Message>,
    pub(super) menu: Element<'a, Message>,
    pub(super) menu_open: bool,
    pub(super) dismiss_message: Message,
}

impl<'content> Widget<Message, iced::Theme, iced::Renderer> for ModelVisibilityTrigger<'content> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::stateless()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.trigger), Tree::new(&self.menu)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.trigger, &self.menu]);
    }

    fn size(&self) -> Size<Length> {
        self.trigger.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.trigger
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.trigger.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.trigger.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.trigger
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.trigger.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'overlay>(
        &'overlay mut self,
        tree: &'overlay mut Tree,
        layout: Layout<'overlay>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'overlay, Message, iced::Theme, iced::Renderer>> {
        let (trigger_tree, menu_tree) = tree.children.split_at_mut(1);
        let trigger_overlay = self.trigger.as_widget_mut().overlay(
            &mut trigger_tree[0],
            layout,
            renderer,
            viewport,
            translation,
        );
        let menu_overlay = if self.menu_open {
            Some(overlay::Element::new(Box::new(ModelVisibilityPopup {
                target_bounds: layout.bounds() + translation,
                viewport: *viewport,
                menu: &mut self.menu,
                tree: &mut menu_tree[0],
                dismiss_message: self.dismiss_message.clone(),
            })))
        } else {
            None
        };
        let overlays = trigger_overlay
            .into_iter()
            .chain(menu_overlay)
            .collect::<Vec<_>>();

        (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
    }
}

impl<'a> From<ModelVisibilityTrigger<'a>> for Element<'a, Message> {
    fn from(trigger: ModelVisibilityTrigger<'a>) -> Self {
        Element::new(trigger)
    }
}

pub(super) struct ModelVisibilityPopup<'b, 'a> {
    target_bounds: Rectangle,
    viewport: Rectangle,
    menu: &'b mut Element<'a, Message>,
    tree: &'b mut Tree,
    dismiss_message: Message,
}

impl<'borrow, 'content> overlay::Overlay<Message, iced::Theme, iced::Renderer>
    for ModelVisibilityPopup<'borrow, 'content>
where
    'content: 'borrow,
{
    fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> layout::Node {
        let viewport = if self.viewport.width > 0.0 && self.viewport.height > 0.0 {
            self.viewport
        } else {
            Rectangle::with_size(bounds)
        };
        let node = self.menu.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let size = node.size();
        let below = viewport.y + viewport.height - self.target_bounds.y - self.target_bounds.height;
        let above = self.target_bounds.y - viewport.y;
        let y = if below >= size.height || below >= above {
            self.target_bounds.y + self.target_bounds.height
        } else {
            self.target_bounds.y - size.height
        }
        .clamp(
            viewport.y,
            (viewport.y + viewport.height - size.height).max(viewport.y),
        );
        let x = (self.target_bounds.x + self.target_bounds.width - size.width).clamp(
            viewport.x,
            (viewport.x + viewport.width - size.width).max(viewport.x),
        );
        node.move_to(Point::new(x, y))
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ) && !cursor.is_over(layout.bounds())
        {
            shell.publish(self.dismiss_message.clone());
            shell.capture_event();
            return;
        }

        self.menu.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &self.viewport,
        );
    }

    fn draw(
        &self,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.menu.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &self.viewport,
        );
    }

    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.menu
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.menu
            .as_widget()
            .mouse_interaction(self.tree, layout, cursor, &self.viewport, renderer)
    }

    fn overlay<'overlay>(
        &'overlay mut self,
        layout: Layout<'overlay>,
        renderer: &iced::Renderer,
    ) -> Option<overlay::Element<'overlay, Message, iced::Theme, iced::Renderer>> {
        self.menu
            .as_widget_mut()
            .overlay(self.tree, layout, renderer, &self.viewport, Vector::ZERO)
    }
}

pub(super) fn model_visibility_menu(
    account_id: AccountId,
    model_entries: &[(String, String)],
    model_visibility: &ModelVisibilityPreferences,
    show_all_model_quotas: bool,
    is_antigravity_account: bool,
    show_antigravity_quota_groups: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let model_rows = model_entries
        .iter()
        .map(|(model_id, display_name)| {
            let model_id = model_id.clone();
            checkbox(model_visibility.is_visible(&model_id))
                .label(display_name.clone())
                .size(12)
                .spacing(7)
                .text_size(typography::METADATA_SIZE)
                .font(typography::BODY)
                .on_toggle(move |is_visible| {
                    Message::SetModelVisibility(model_id.clone(), is_visible)
                })
                .style(move |framework_theme, status| {
                    if theme.colors.is_light {
                        checkbox_style(theme, status)
                    } else {
                        iced::widget::checkbox::primary(framework_theme, status)
                    }
                })
                .into()
        })
        .collect::<Vec<Element<'static, Message>>>();

    let close_button = button(container(icon_x().size(12).color(muted_text(theme))).center(22))
        .on_press(Message::CloseModelVisibilityMenu(account_id))
        .width(22)
        .height(22)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = matches!(status, button::Status::Hovered | button::Status::Pressed)
                .then(|| Background::Color(theme.colors.hover()));
            style.text_color = theme.colors.text();
            style.border = Border {
                radius: 5.0.into(),
                ..Border::default()
            };
            style.shadow = Default::default();
            style
        });
    let mut mode_buttons = row![
        model_quota_mode_button(
            locale::text(language, Text::AllModels),
            show_all_model_quotas && !show_antigravity_quota_groups,
            true,
            theme,
        ),
        model_quota_mode_button(
            locale::text(language, Text::PinnedModels),
            !show_all_model_quotas && !show_antigravity_quota_groups,
            false,
            theme,
        ),
    ]
    .spacing(4)
    .width(Fill);
    if is_antigravity_account {
        mode_buttons = mode_buttons.push(antigravity_group_mode_button(
            locale::text(language, Text::AntigravityGroups),
            show_antigravity_quota_groups,
            theme,
        ));
    }

    let mut menu_content = column![
        row![
            text(locale::text(language, Text::ModelVisibility))
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text()),
            space().width(Fill),
            close_button,
        ]
        .align_y(Alignment::Center)
        .width(Fill),
        mode_buttons,
    ]
    .spacing(5)
    .width(Fill);
    if is_antigravity_account && show_antigravity_quota_groups {
        menu_content = menu_content.push(
            checkbox(!antigravity_claude_gpt_hidden())
                .label(locale::text(language, Text::ShowClaudeGptGroup))
                .size(12)
                .spacing(7)
                .text_size(typography::METADATA_SIZE)
                .font(typography::BODY)
                .on_toggle(|show| Message::SetAntigravityClaudeGptHidden(!show))
                .style(move |framework_theme, status| {
                    if theme.colors.is_light {
                        checkbox_style(theme, status)
                    } else {
                        iced::widget::checkbox::primary(framework_theme, status)
                    }
                }),
        );
    }
    if !model_entries.is_empty() {
        let model_list = scrollable(column(model_rows).spacing(2))
            .height(Length::Fixed(176.0))
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::hidden(),
            ));
        menu_content = menu_content.push(model_list);
    }

    container(menu_content)
        .width(250)
        .padding([7, 8])
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.window_surface())),
            border: Border {
                color: theme.colors.border(0.28),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

pub(super) fn checkbox_style(
    theme: &'static crate::theme::ThemeDefinition,
    status: iced::widget::checkbox::Status,
) -> iced::widget::checkbox::Style {
    use iced::widget::checkbox::{Status, Style};

    let (is_checked, is_hovered, is_disabled) = match status {
        Status::Active { is_checked } => (is_checked, false, false),
        Status::Hovered { is_checked } => (is_checked, true, false),
        Status::Disabled { is_checked } => (is_checked, false, true),
    };
    let fill = if is_checked {
        theme.accent_color()
    } else if is_hovered {
        Color::from_rgb8(232, 239, 248)
    } else {
        theme.colors.control_surface()
    };
    let border = if is_checked {
        theme.accent_color()
    } else {
        theme.colors.border(0.58)
    };

    Style {
        background: Background::Color(if is_disabled {
            fill.scale_alpha(0.55)
        } else {
            fill
        }),
        icon_color: Color::WHITE,
        border: Border {
            color: border,
            width: 1.0,
            radius: 3.0.into(),
        },
        text_color: Some(theme.colors.text()),
    }
}

pub(super) fn model_quota_mode_button(
    label: &'static str,
    selected: bool,
    show_all: bool,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    button(
        text(label)
            .size(typography::COMPACT_SIZE)
            .font(if selected {
                typography::EMPHASIS
            } else {
                typography::BODY
            }),
    )
    .on_press(Message::SetModelQuotaDisplay(show_all))
    .padding([4, 6])
    .width(Fill)
    .style(move |framework_theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = Some(Background::Color(if selected {
            theme.accent_color().scale_alpha(0.14)
        } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            theme.colors.hover()
        } else {
            theme.colors.control_surface()
        }));
        style.text_color = theme.colors.text();
        style.border = Border {
            color: if selected {
                theme.accent_color().scale_alpha(0.55)
            } else {
                theme.colors.border(0.18)
            },
            width: 1.0,
            radius: 5.0.into(),
        };
        style.shadow = Default::default();
        style
    })
    .into()
}

pub(super) fn antigravity_group_mode_button(
    label: &'static str,
    selected: bool,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    button(
        text(label)
            .size(typography::COMPACT_SIZE)
            .font(if selected {
                typography::EMPHASIS
            } else {
                typography::BODY
            }),
    )
    .on_press(Message::SetAntigravityQuotaGroups(true))
    .padding([4, 6])
    .width(Fill)
    .style(move |framework_theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = Some(Background::Color(if selected {
            theme.accent_color().scale_alpha(0.14)
        } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            theme.colors.hover()
        } else {
            theme.colors.control_surface()
        }));
        style.text_color = theme.colors.text();
        style.border = Border {
            color: if selected {
                theme.accent_color().scale_alpha(0.55)
            } else {
                theme.colors.border(0.18)
            },
            width: 1.0,
            radius: 5.0.into(),
        };
        style.shadow = Default::default();
        style
    })
    .into()
}
