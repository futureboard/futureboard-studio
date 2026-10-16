use crate::theme::Colors;
use gpui::{div, px, svg, IntoElement, Styled};

/// Content-viewport playhead line (no head/marker) at a precomputed x.
pub fn playhead_line_at(x: f32) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("PlayheadLine");
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(x))
        .w(px(1.0))
        .bg(Colors::timeline_playhead())
}

/// Ruler-only playhead head/marker (no vertical line) at a precomputed x.
pub fn playhead_head_at(x: f32) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("PlayheadHead");
    svg()
        .path(crate::assets::ICON_PLAYHEAD_HANDLE_PATH)
        .absolute()
        .top_0()
        .left(px(x - 5.5))
        .w(px(12.0))
        .h(px(12.0))
        .text_color(Colors::timeline_playhead())
}

/// Where the playhead is this frame, shared between the arrangement and the
/// overlay that draws it (`arrangement_overlay::PlayheadOverlay`).
///
/// A cell rather than an entity field so `Timeline::render` can refresh it
/// without leasing the overlay: the arrangement recomputes x whenever it lays
/// itself out (a scroll, a zoom, a resize), and the playback poll recomputes it
/// between those.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PlayheadFrame {
    /// x within the lane column, in the same space `beats_to_x` returns.
    pub x: f32,
}

pub type PlayheadFrameCell = std::rc::Rc<std::cell::Cell<PlayheadFrame>>;
