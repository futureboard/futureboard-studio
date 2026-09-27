use gpui::{div, px, Empty, IntoElement, ParentElement, Render, Styled, Window};

use crate::components::reorder::{DragRefusal, RefusableDrag};
use crate::theme::Colors;

/// Zero-sized GPUI drag payload for the mixer's horizontal scrollbar thumb.
/// Same pattern as [`super::MixerSplitDrag`]: `on_drag` registers it and
/// `on_drag_move` on the scrollbar track maps the captured pointer to a scroll
/// offset for as long as the pointer is held.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MixerScrollDrag;

impl Render for MixerScrollDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone)]
pub(super) struct SendSlotDrag {
    pub(super) track_id: String,
    pub(super) send_id: String,
    pub(super) target_name: String,
    /// List index the drag started from, as rendered — picks the drop line's
    /// edge; the commit resolves the drop against the live send order.
    pub(super) source_index: usize,
    /// The target currently showing "not allowed" for this drag.
    pub(super) refusal: DragRefusal,
}

impl RefusableDrag for SendSlotDrag {
    fn refusal(&self) -> &DragRefusal {
        &self.refusal
    }
}

impl Render for SendSlotDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .h(px(20.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_overlay())
            .border(px(1.0))
            .border_color(Colors::accent_primary())
            .text_size(px(10.0))
            .text_color(Colors::text_primary())
            .child(format!("Send -> {}", self.target_name))
    }
}
