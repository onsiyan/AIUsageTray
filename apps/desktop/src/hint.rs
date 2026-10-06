//! Hover hints drawn as a readable bubble over whatever is underneath.

use iced::{
    Background, Border, Element, Shadow,
    widget::{container, text, tooltip},
};
use std::time::Duration;

use crate::{Message, theme::ThemeDefinition, typography};

const HINT_DELAY: Duration = Duration::from_millis(350);
const HINT_MAX_WIDTH: f32 = 260.0;

/// Wraps `content` with a hint shown below it on hover.
pub fn hint(
    content: impl Into<Element<'static, Message>>,
    label: impl Into<String>,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let label = container(
        text(label.into())
            .size(typography::LABEL_SIZE)
            .font(typography::MEDIUM)
            .color(theme.colors.text()),
    )
    .max_width(HINT_MAX_WIDTH);
    tooltip(content, label, tooltip::Position::Bottom)
        .padding(8)
        .gap(5)
        .delay(HINT_DELAY)
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.control_surface())),
            text_color: Some(theme.colors.text()),
            border: Border {
                color: theme
                    .colors
                    .border(if theme.colors.is_light { 0.35 } else { 0.22 }),
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: Shadow::default(),
            ..Default::default()
        })
        .into()
}
