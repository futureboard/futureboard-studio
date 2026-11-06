use crate::theme::Colors;
use gpui::{
    div, fill, point, px, size, Bounds, IntoElement, ParentElement, PathBuilder, Rgba, Styled,
    Window,
};

/// Width of the stroke on each side of the playhead line, in pixels.
pub const PLAYHEAD_STROKE: f32 = 1.0;

/// The playhead's line and stroke colours at `alpha` (1 for a playing
/// transport; views that dim a parked playhead pass less).
pub fn playhead_colors(alpha: f32) -> (Rgba, Rgba) {
    let line = Colors::playhead_line();
    let stroke = Colors::playhead_stroke();
    (
        Colors::with_alpha(line, line.a * alpha),
        Colors::with_alpha(stroke, stroke.a * alpha),
    )
}

/// Paint the playhead line, 1 px wide with its left edge at `x`, from `top` to
/// `bottom`, with the stroke on both sides. Paint-time only.
pub fn paint_playhead_line(window: &mut Window, x: f32, top: f32, bottom: f32, alpha: f32) {
    let height = bottom - top;
    if height <= 0.0 {
        return;
    }
    let (line, stroke) = playhead_colors(alpha);
    window.paint_quad(fill(
        Bounds::new(
            point(px(x - PLAYHEAD_STROKE), px(top)),
            size(px(1.0 + 2.0 * PLAYHEAD_STROKE), px(height)),
        ),
        stroke,
    ));
    window.paint_quad(fill(
        Bounds::new(point(px(x), px(top)), size(px(1.0), px(height))),
        line,
    ));
}

/// Paint the ruler's playhead head: a 12 px triangle pointing down at the line
/// whose left edge is at `x`, its top at `top`, outlined by the stroke.
pub fn paint_playhead_head(window: &mut Window, x: f32, top: f32, alpha: f32) {
    let (line, stroke) = playhead_colors(alpha);
    let center = x + 0.5;
    let triangle = |grow: f32| {
        let mut path = PathBuilder::fill();
        path.move_to(point(px(center - 6.0 - grow), px(top - grow)));
        path.line_to(point(px(center + 6.0 + grow), px(top - grow)));
        path.line_to(point(px(center), px(top + 12.0 + grow * 1.6)));
        path.close();
        path.build().ok()
    };
    if let Some(outline) = triangle(PLAYHEAD_STROKE) {
        window.paint_path(outline, stroke);
    }
    if let Some(body) = triangle(0.0) {
        window.paint_path(body, line);
    }
}

/// Content-viewport playhead line (no head/marker) at a precomputed x, with
/// its stroke, for views that place it as an element.
pub fn playhead_line_at(x: f32) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("PlayheadLine");
    playhead_line_element(x, 1.0)
}

/// [`playhead_line_at`] at `alpha`, its line's left edge at `x`.
pub fn playhead_line_element(x: f32, alpha: f32) -> gpui::Div {
    let (line, stroke) = playhead_colors(alpha);
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(x - PLAYHEAD_STROKE))
        .w(px(1.0 + 2.0 * PLAYHEAD_STROKE))
        .bg(stroke)
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(PLAYHEAD_STROKE))
                .w(px(1.0))
                .bg(line),
        )
}

/// Ruler-only playhead head/marker (no vertical line) at a precomputed x.
pub fn playhead_head_at(x: f32) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("PlayheadHead");
    gpui::canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<gpui::Pixels>, (), window, _cx| {
            let origin_x: f32 = bounds.origin.x.into();
            let origin_y: f32 = bounds.origin.y.into();
            paint_playhead_head(window, origin_x + x, origin_y + 1.0, 1.0);
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
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
