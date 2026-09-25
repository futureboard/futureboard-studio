//! The arrangement marquee's rectangle.
//!
//! The rectangle follows the pointer at the display rate while a marquee is
//! dragged, so it is its own entity for the same reason the playhead and the
//! razor line are: notifying `Timeline` for it would rebuild every lane on
//! every mouse move (and on every synthetic drag repeat macOS sends while the
//! button is held still), while notifying this repaints one rectangle.
//! `Timeline` itself is notified only when what the rectangle encloses
//! changes.

use gpui::{div, px, Context, IntoElement, ParentElement, Render, Styled, Window};

use crate::theme::Colors;

/// Where the rectangle is, relative to the visible track area: `left` in lane
/// x, `top` from the top of the visible track area. The overlay's parent clips
/// it to that area.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MarqueeFrame {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

pub type MarqueeFrameCell = std::rc::Rc<std::cell::Cell<Option<MarqueeFrame>>>;

pub struct MarqueeOverlay {
    frame: MarqueeFrameCell,
}

impl MarqueeOverlay {
    pub fn new(frame: MarqueeFrameCell) -> Self {
        Self { frame }
    }
}

impl Render for MarqueeOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(frame) = self.frame.get() else {
            return div().into_any_element();
        };
        let accent = Colors::accent_primary();
        div()
            .absolute()
            .left(px(frame.left))
            .top(px(frame.top))
            .w(px(frame.width.max(1.0)))
            .h(px(frame.height.max(1.0)))
            .bg(Colors::with_alpha(accent, 0.14))
            .border(px(1.0))
            .border_color(Colors::with_alpha(accent, 0.7))
            .into_any_element()
    }
}
