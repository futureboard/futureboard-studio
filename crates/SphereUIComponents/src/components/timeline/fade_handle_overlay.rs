//! The fade handles of the audio clip under the pointer.
//!
//! Every audio clip keeps small corner hit areas for its fades, but only a
//! selected clip draws its squares itself. The hovered clip's squares are
//! painted here, in window coordinates, for the reason the Smart Tool's razor
//! line is its own entity: revealing them from the clip would rebuild its
//! whole lane each time the pointer crossed a clip edge, while notifying this
//! repaints two squares.
//!
//! The squares are not `deferred`: they paint in the timeline's own order,
//! after its lanes. A deferred element paints after every plain one, so it
//! would sit on top of the studio's command palette, settings and plug-in
//! picker, which are plain overlays painted after the timeline. Painted in
//! order they stay under those and under every deferred menu or popover, as
//! handles sit below menus and dialogs.

use gpui::{anchored, div, point, px, Context, IntoElement, ParentElement, Render, Window};

use crate::components::timeline::audio_clip::fade_handle_mark;

/// One square, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FadeHandleMark {
    pub left: f32,
    pub top: f32,
    pub size: f32,
}

/// The squares to draw. The timeline resolves them through the same layout
/// the clip's hit areas use (`audio_clip::clip_fade_handle_layout`), so a
/// square is always over the area that takes the press.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FadeHandleFrame {
    pub fade_in: Option<FadeHandleMark>,
    pub fade_out: Option<FadeHandleMark>,
    /// One of them is being dragged.
    pub active: bool,
    /// The clip is on an ARA track, where fades are not applied.
    pub disabled: bool,
}

pub type FadeHandleCell = std::rc::Rc<std::cell::Cell<Option<FadeHandleFrame>>>;

pub struct FadeHandleOverlay {
    frame: FadeHandleCell,
}

impl FadeHandleOverlay {
    pub fn new(frame: FadeHandleCell) -> Self {
        Self { frame }
    }
}

impl Render for FadeHandleOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(frame) = self.frame.get() else {
            return div().into_any_element();
        };
        div()
            .children(
                [frame.fade_in, frame.fade_out]
                    .into_iter()
                    .flatten()
                    .map(|mark| {
                        anchored()
                            .position(point(px(mark.left), px(mark.top)))
                            .child(fade_handle_mark(mark.size, frame.active, frame.disabled))
                    }),
            )
            .into_any_element()
    }
}
