//! The Smart Tool's razor line.
//!
//! Hovering the lower half of an audio clip with the Pointer shows where a
//! click would split it. The line follows the pointer at the display rate, so
//! it is its own entity for the same reason the playhead is: notifying
//! `Timeline` for it would rebuild every lane on every mouse move, while
//! notifying this repaints one line.

use gpui::{
    anchored, deferred, div, point, px, Context, IntoElement, ParentElement, Render, Styled, Window,
};

use crate::theme::Colors;

/// Where the line is, in window coordinates. The clip that owns the hover
/// measures its own bounds, so this needs no second copy of the arrangement's
/// geometry.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CutGuideFrame {
    /// Snapped split position.
    pub x: f32,
    pub top: f32,
    pub height: f32,
}

pub type CutGuideCell = std::rc::Rc<std::cell::Cell<Option<CutGuideFrame>>>;

pub struct CutGuideOverlay {
    frame: CutGuideCell,
}

impl CutGuideOverlay {
    pub fn new(frame: CutGuideCell) -> Self {
        Self { frame }
    }
}

impl Render for CutGuideOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(frame) = self.frame.get() else {
            return div().into_any_element();
        };
        deferred(
            anchored()
                .position(point(px(frame.x.round() - 0.5), px(frame.top)))
                .child(
                    div()
                        .w(px(1.0))
                        .h(px(frame.height.max(1.0)))
                        .bg(Colors::text_primary()),
                ),
        )
        .into_any_element()
    }
}
