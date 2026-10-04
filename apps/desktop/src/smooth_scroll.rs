//! Smooth discrete wheel input while retaining Iced's layout, clipping and overlays.

use std::time::{Duration, Instant};

use iced::{
    Element, Event, Length, Rectangle, Size, Vector,
    advanced::{
        Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
        widget::{self, Operation, Tree, operation::scrollable},
    },
    keyboard, window,
};

use crate::Message;

const WHEEL_LINE_PIXELS: f32 = 60.0;
const TRANSITION_DURATION: Duration = Duration::from_millis(140);
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 120);
/// Wheel events closer together than this are one continuous gesture
/// (a precision touchpad or a free-spinning wheel), not separate notches.
const CONTINUOUS_INPUT_GAP: Duration = Duration::from_millis(40);

pub fn smooth_scroll(
    key: impl Into<String>,
    content: impl Into<Element<'static, Message>>,
) -> Element<'static, Message> {
    Element::new(SmoothScroll {
        key: key.into(),
        content: content.into(),
    })
}

struct SmoothScroll<'a> {
    key: String,
    content: Element<'a, Message>,
}

struct State {
    key: String,
    motion: Option<Motion>,
    modifiers: keyboard::Modifiers,
    last_wheel_at: Option<Instant>,
}

/// Only separate whole notches of a classic wheel are animated. Fractional
/// or rapid deltas already describe a continuous motion; easing each of them
/// restarted the animation every few milliseconds and made scrolling drag.
fn is_discrete_notch(lines: f32, since_previous: Option<Duration>) -> bool {
    (lines - lines.round()).abs() < 0.01
        && since_previous.is_none_or(|gap| gap >= CONTINUOUS_INPUT_GAP)
}

#[derive(Clone, Copy)]
struct Motion {
    from: f32,
    target: f32,
    started_at: Instant,
}

impl Motion {
    fn retarget(previous: Option<Self>, current: f32, delta: f32, max: f32, now: Instant) -> Self {
        // Accumulate rapid notches; reversing the wheel cancels the unfinished
        // distance immediately so the list follows the new direction.
        let base = previous
            .filter(|motion| (motion.target - current).signum() == delta.signum())
            .map_or(current, |motion| motion.target);
        Self {
            from: current,
            target: (base + delta).clamp(0.0, max),
            started_at: now,
        }
    }

    fn offset_at(self, now: Instant, max: f32) -> f32 {
        let progress = (now.saturating_duration_since(self.started_at).as_secs_f32()
            / TRANSITION_DURATION.as_secs_f32())
        .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        (self.from + (self.target - self.from) * eased).clamp(0.0, max)
    }

    fn is_active(self, now: Instant, max: f32) -> bool {
        now.saturating_duration_since(self.started_at) < TRANSITION_DURATION
            && (self.target.clamp(0.0, max) - self.offset_at(now, max)).abs() > 0.1
    }
}

/// Operate only on the wrapped scrollable, never a nested scrollable.
#[derive(Default)]
struct ScrollPosition {
    current: f32,
    max: f32,
    set_to: Option<f32>,
}

impl Operation for ScrollPosition {
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _: Option<&widget::Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        state: &mut dyn scrollable::Scrollable,
    ) {
        self.max = (content_bounds.height - bounds.height).max(0.0);
        self.current = translation.y.clamp(0.0, self.max);
        if let Some(offset) = self.set_to {
            state.scroll_to(scrollable::AbsoluteOffset {
                x: None,
                y: Some(offset.clamp(0.0, self.max)),
            });
        }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for SmoothScroll<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State {
            key: self.key.clone(),
            motion: None,
            modifiers: keyboard::Modifiers::default(),
            last_wheel_at: None,
        })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.key != self.key {
            state.key.clone_from(&self.key);
            state.motion = None;
            tree.children = self.children();
        } else {
            tree.diff_children(&[&self.content]);
        }
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
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
        let state = tree.state.downcast_mut::<State>();
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) = event {
            state.modifiers = *modifiers;
        }

        match event {
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x, y },
            }) if *x == 0.0
                && *y != 0.0
                && y.is_finite()
                && !state.modifiers.shift()
                && {
                    let now = Instant::now();
                    let since_previous = state
                        .last_wheel_at
                        .replace(now)
                        .map(|previous| now.saturating_duration_since(previous));
                    is_discrete_notch(*y, since_previous)
                }
                && layout
                    .bounds()
                    .intersection(viewport)
                    .is_some_and(|bounds| cursor.is_over(bounds)) =>
            {
                let mut position = ScrollPosition::default();
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut position,
                );
                if position.max > 0.0 {
                    let motion = Motion::retarget(
                        state.motion,
                        position.current,
                        -y * WHEEL_LINE_PIXELS,
                        position.max,
                        Instant::now(),
                    );
                    state.motion = (motion.target != motion.from).then_some(motion);
                    if state.motion.is_some() {
                        shell.request_redraw();
                    }
                    shell.capture_event();
                    return;
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let Some(motion) = state.motion {
                    let mut position = ScrollPosition::default();
                    self.content.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut position,
                    );
                    let active = motion.is_active(*now, position.max);
                    position.set_to = Some(if active {
                        motion.offset_at(*now, position.max)
                    } else {
                        motion.target.clamp(0.0, position.max)
                    });
                    self.content.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut position,
                    );
                    if active {
                        shell.request_redraw_at(*now + FRAME_INTERVAL);
                    } else {
                        state.motion = None;
                    }
                }
            }
            // Precision touchpads already supply continuous pixels. Touch,
            // dragging, and keyboard input take over from wheel animation.
            Event::Mouse(mouse::Event::WheelScrolled { .. } | mouse::Event::ButtonPressed(_))
            | Event::Touch(_)
            | Event::Keyboard(keyboard::Event::KeyPressed { .. })
            | Event::Window(window::Event::Unfocused) => state.motion = None,
            _ => {}
        }

        self.content.as_widget_mut().update(
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
        self.content.as_widget().draw(
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
        operation: &mut dyn Operation,
    ) {
        tree.state.downcast_mut::<State>().motion = None;
        self.content
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::renderer::Headless;

    #[test]
    fn only_separate_whole_notches_are_animated() {
        let slow = Some(Duration::from_millis(120));
        let fast = Some(Duration::from_millis(7));
        assert!(is_discrete_notch(-1.0, None));
        assert!(is_discrete_notch(3.0, slow));
        assert!(
            !is_discrete_notch(-1.2916666, slow),
            "fractional deltas are continuous"
        );
        assert!(
            !is_discrete_notch(-1.0, fast),
            "rapid deltas are continuous"
        );
    }

    #[test]
    fn widget_animates_real_scroll_state_and_keeps_pixel_input_direct() {
        let renderer = iced::futures::executor::block_on(<iced::Renderer as Headless>::new(
            iced::Font::DEFAULT,
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("software renderer");
        let mut content = smooth_scroll(
            "accounts",
            iced::widget::scrollable(iced::widget::Space::new().width(200).height(1200))
                .width(200)
                .height(100),
        );
        let mut tree = Tree::new(&content);
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 100.0)),
        );
        let layout = Layout::new(&node);
        let viewport = Rectangle::with_size(Size::new(200.0, 100.0));
        let mut messages = Vec::new();
        let mut dispatch = |tree: &mut Tree, event: Event| {
            let mut shell = Shell::new(&mut messages);
            content.as_widget_mut().update(
                tree,
                &event,
                layout,
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &viewport,
            );
            (shell.is_event_captured(), shell.redraw_request())
        };
        dispatch(
            &mut tree,
            Event::Window(window::Event::RedrawRequested(Instant::now())),
        );
        let (captured, redraw) = dispatch(
            &mut tree,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
            }),
        );
        assert!(captured);
        assert_eq!(redraw, window::RedrawRequest::NextFrame);
        let started = tree
            .state
            .downcast_ref::<State>()
            .motion
            .unwrap()
            .started_at;
        // The original Iced widget would already have jumped to 60 pixels.
        let mut previous = 0.0;
        let mut probe: Element<'_, Message> =
            iced::widget::scrollable(iced::widget::Space::new().width(200).height(1200))
                .width(200)
                .height(100)
                .into();
        for milliseconds in [8, 24, 48, 80, 140] {
            let (_, redraw) = dispatch(
                &mut tree,
                Event::Window(window::Event::RedrawRequested(
                    started + Duration::from_millis(milliseconds),
                )),
            );
            let mut position = ScrollPosition::default();
            probe
                .as_widget_mut()
                .operate(&mut tree.children[0], layout, &renderer, &mut position);
            assert!(position.current > previous);
            if milliseconds < 140 {
                assert!(position.current < 60.0);
                assert!(matches!(redraw, window::RedrawRequest::At(_)));
            } else {
                assert_eq!(position.current, 60.0);
                assert_eq!(redraw, window::RedrawRequest::Wait);
            }
            previous = position.current;
        }
        assert!(tree.state.downcast_ref::<State>().motion.is_none());
        dispatch(
            &mut tree,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
            }),
        );
        dispatch(
            &mut tree,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -12.0 },
            }),
        );
        assert!(tree.state.downcast_ref::<State>().motion.is_none());
        let mut position = ScrollPosition::default();
        probe
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, &renderer, &mut position);
        // The second notch arrived right after the first, so it is part of a
        // continuous gesture and moves directly (60 px), like the pixel input.
        assert_eq!(
            position.current, 132.0,
            "rapid and pixel input should move directly by their exact distance"
        );
        assert!(
            messages.is_empty(),
            "scroll animation must not rebuild the app each frame"
        );
    }

    #[test]
    fn wheel_moves_gradually_and_finishes_without_idle_frames() {
        let now = Instant::now();
        let motion = Motion::retarget(None, 0.0, 60.0, 500.0, now);
        assert_eq!(motion.offset_at(now, 500.0), 0.0);
        let mid = motion.offset_at(now + TRANSITION_DURATION / 2, 500.0);
        assert!(mid > 0.0 && mid < 60.0);
        assert_eq!(motion.offset_at(now + TRANSITION_DURATION, 500.0), 60.0);
        assert!(!motion.is_active(now + TRANSITION_DURATION, 500.0));
    }

    #[test]
    fn rapid_notches_accumulate_but_reversal_follows_the_new_direction() {
        let now = Instant::now();
        let first = Motion::retarget(None, 100.0, 60.0, 500.0, now);
        let next = Motion::retarget(Some(first), 130.0, 60.0, 500.0, now);
        assert_eq!(next.target, 220.0);
        let reversed = Motion::retarget(Some(next), 150.0, -60.0, 500.0, now);
        assert_eq!(reversed.target, 90.0);
        assert!(reversed.offset_at(now + Duration::from_millis(8), 500.0) < 150.0);
    }

    #[test]
    fn limits_and_content_shrink_do_not_leave_scroll_debt() {
        let now = Instant::now();
        let down = Motion::retarget(None, 480.0, 60.0, 500.0, now);
        assert_eq!(down.target, 500.0);
        assert_eq!(
            Motion::retarget(Some(down), 500.0, -60.0, 500.0, now).target,
            440.0
        );
        let up = Motion::retarget(None, 20.0, -60.0, 500.0, now);
        assert_eq!(up.target, 0.0);
        assert_eq!(down.offset_at(now + Duration::from_millis(8), 200.0), 200.0);
        assert!(!down.is_active(now, 200.0));
        assert!(!down.is_active(now, 0.0));
    }
}
