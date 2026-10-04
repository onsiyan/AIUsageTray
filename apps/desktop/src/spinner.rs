//! An icon that spins on its own while a refresh runs.
//!
//! Driving the rotation from app messages rebuilt and laid out the whole
//! window on every frame, which starved scrolling for the length of a
//! refresh. This widget instead rotates the icon at draw time and schedules
//! its own next frame, so only the icon is redrawn.

use std::{
    f32::consts::TAU,
    time::{Duration, Instant},
};

use iced::{
    Element, Event, Length, Radians, Rectangle, Size,
    advanced::{
        Clipboard, Layout, Shell, Widget,
        image::{self, Renderer as _},
        layout, mouse, renderer,
        widget::{self, Tree},
    },
    window,
};

use crate::Message;

const TURNS_PER_SECOND: f32 = 0.8;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

pub fn spinning_icon(
    handle: image::Handle,
    size: f32,
    spinning: bool,
) -> Element<'static, Message> {
    Element::new(SpinningIcon {
        handle,
        size,
        spinning,
    })
}

struct SpinningIcon {
    handle: image::Handle,
    size: f32,
    spinning: bool,
}

#[derive(Default)]
struct State {
    started_at: Option<Instant>,
    now: Option<Instant>,
}

impl State {
    fn angle(&self) -> f32 {
        match (self.started_at, self.now) {
            (Some(started_at), Some(now)) => {
                let elapsed = now.saturating_duration_since(started_at).as_secs_f32();
                (elapsed * TURNS_PER_SECOND * TAU).rem_euclid(TAU)
            }
            _ => 0.0,
        }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for SpinningIcon {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(Size::new(self.size, self.size))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        if !self.spinning {
            *state = State::default();
            return;
        }
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            state.started_at.get_or_insert(*now);
            state.now = Some(*now);
            shell.request_redraw_at(*now + FRAME_INTERVAL);
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let angle = if self.spinning {
            tree.state.downcast_ref::<State>().angle()
        } else {
            0.0
        };
        renderer.draw_image(
            image::Image::new(self.handle.clone()).rotation(Radians(angle)),
            layout.bounds(),
            *viewport,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angle_advances_with_time_and_wraps() {
        let started_at = Instant::now();
        let at = |millis| State {
            started_at: Some(started_at),
            now: Some(started_at + Duration::from_millis(millis)),
        };
        assert_eq!(at(0).angle(), 0.0);
        assert!(at(100).angle() > 0.0);
        assert!(at(100).angle() < at(200).angle());
        let one_turn = (1000.0 / TURNS_PER_SECOND) as u64;
        assert!(
            at(one_turn + 10).angle() < at(100).angle(),
            "wraps after a turn"
        );
        assert_eq!(State::default().angle(), 0.0);
    }
}
