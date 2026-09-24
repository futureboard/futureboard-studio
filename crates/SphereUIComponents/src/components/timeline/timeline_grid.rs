//! Grid layer for the global conductor lanes (tempo, time signature, song text).
//!
//! Those lanes sit between the ruler and the track list as siblings, so the
//! arrangement's grid canvas — which lives inside the track list — never
//! reaches them. They were painting an opaque surface over nothing, which left
//! them as blank slabs with no musical time in them at all: you could not tell
//! which bar a tempo marker sat in without tracing down to the tracks below.
//!
//! This draws the same lines from the same source (`get_arrangement_grid_lines`)
//! with the same tokens the arrangement uses, so a bar line in the tempo lane is
//! the same pixel as the bar line in the track beneath it. Deriving them
//! independently would have let the two drift apart at fractional zoom.
//!
//! One canvas, not a div per line: at typical zoom this is a few hundred lines,
//! and the previous div-per-line version allocated an element for each.

use gpui::{canvas, fill, point, px, size, Bounds, IntoElement, Pixels, Styled};

use crate::components::timeline::timeline_state::{GridLineLevel, TimelineState};
use crate::theme::Colors;

/// Grid + alternating bar shading sized to a global lane's content area.
///
/// `grid_width` is the lane's content width (right of the header column), in the
/// same coordinate space the ruler and clips use.
pub fn timeline_grid(
    state: &TimelineState,
    grid_width: f32,
    _grid_height: f32,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("TimelineGrid");

    let lines: Vec<(f32, GridLineLevel)> = state
        .arrangement_grid_lines(grid_width)
        .iter()
        .map(|line| (line.x, line.level))
        .collect();
    crate::perf::count("grid_lines", lines.len() as u64);

    let (visible_start, visible_end) = state.visible_beat_range(grid_width);
    let shades: Vec<(f32, f32)> = state
        .time_signature_map
        .visible_bar_rects(visible_start as f64, visible_end as f64)
        .into_iter()
        .filter(|rect| rect.bar % 2 == 0)
        .filter_map(|rect| {
            // Clamped for the same reason as the arrangement's shades: the bar
            // under the left viewport edge starts at a negative x, and this
            // wash would otherwise bleed out of the lane. The lane clips, but
            // relying on the clip to hide wrong geometry is how it escapes the
            // moment a parent stops clipping.
            let x0 = state.beats_to_x(rect.start_beat as f32).max(0.0);
            let x1 = state.beats_to_x(rect.end_beat as f32);
            let w = x1 - x0;
            (w >= 2.0).then_some((x0, w))
        })
        .collect();

    canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let h = bounds.size.height;
            for (x, w) in &shades {
                window.paint_quad(fill(
                    Bounds::new(bounds.origin + point(px(*x), px(0.0)), size(px(*w), h)),
                    Colors::timeline_region_background(),
                ));
            }
            for (x, level) in &lines {
                let color = match level {
                    GridLineLevel::Bar => Colors::timeline_grid_bar(),
                    GridLineLevel::Beat => Colors::timeline_grid_major(),
                    GridLineLevel::Sub => Colors::timeline_grid_minor(),
                };
                window.paint_quad(fill(
                    Bounds::new(bounds.origin + point(px(*x), px(0.0)), size(px(1.0), h)),
                    color,
                ));
            }
        },
    )
    .absolute()
    .inset_0()
}
