//! The MIDI editor's grid, painted in one canvas.
//!
//! The note grid used to be a `div` per pitch row, per row line and per timing
//! line — a few hundred elements rebuilt on every scroll and zoom — each at a
//! fractional position, so every 1 px line was antialiased across two device
//! pixels and the grid read soft and uneven. This paints the same geometry as
//! batched quads, with every edge snapped to the device pixel grid.
//!
//! Geometry and colours are resolved by the caller (control path); the paint
//! closure only walks prepared lists.

use gpui::{canvas, fill, point, px, size, AnyElement, Bounds, IntoElement, Pixels, Rgba, Styled};

/// Everything the note grid paints, in grid-local logical pixels.
#[derive(Debug, Clone, Default)]
pub(super) struct NoteGridSnapshot {
    /// Device pixels per logical pixel.
    pub scale: f32,
    /// Row fills: `(top, bottom, colour)`. Top and bottom rather than a height
    /// so adjacent rows share an edge exactly after snapping.
    pub rows: Vec<(f32, f32, Rgba)>,
    /// Horizontal row separators: `(edge, colour)`. The line is painted just
    /// above `edge`, inside the row that ends there — the same place the key
    /// lane draws its key borders.
    pub row_lines: Vec<(f32, Rgba)>,
    /// Vertical timing lines: `(x, colour)`.
    pub columns: Vec<(f32, Rgba)>,
}

/// Round a logical coordinate onto the device pixel grid.
#[inline]
pub(super) fn snap(v: f32, scale: f32) -> f32 {
    let scale = scale.max(0.5);
    (v * scale).round() / scale
}

/// A one-device-pixel-aligned hairline: one logical pixel rounded to whole
/// device pixels, so a 1.5× display draws 2 device px instead of a smeared
/// 1.5.
#[inline]
pub(super) fn hairline(scale: f32) -> f32 {
    let scale = scale.max(0.5);
    scale.round().max(1.0) / scale
}

pub(super) fn render_note_grid(snapshot: NoteGridSnapshot) -> AnyElement {
    canvas(
        |_b, _w, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let NoteGridSnapshot {
                scale,
                rows,
                row_lines,
                columns,
            } = &snapshot;
            let origin = bounds.origin;
            let width = bounds.size.width;
            let height = bounds.size.height;
            let line = hairline(*scale);
            for (top, bottom, color) in rows {
                let top = snap(*top, *scale);
                let bottom = snap(*bottom, *scale);
                if bottom <= top {
                    continue;
                }
                window.paint_quad(fill(
                    Bounds::new(
                        origin + point(px(0.0), px(top)),
                        size(width, px(bottom - top)),
                    ),
                    *color,
                ));
            }
            for (edge, color) in row_lines {
                window.paint_quad(fill(
                    Bounds::new(
                        origin + point(px(0.0), px(snap(*edge, *scale) - line)),
                        size(width, px(line)),
                    ),
                    *color,
                ));
            }
            for (x, color) in columns {
                window.paint_quad(fill(
                    Bounds::new(
                        origin + point(px(snap(*x, *scale)), px(0.0)),
                        size(px(line), height),
                    ),
                    *color,
                ));
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// Timing lines only, for the controller / velocity / articulation lanes
/// under the grid. Same snapping, so a lane's bar line continues the grid's.
pub(super) fn render_lane_grid(scale: f32, columns: Vec<(f32, Rgba)>) -> AnyElement {
    render_note_grid(NoteGridSnapshot {
        scale,
        columns,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapping_lands_on_device_pixels() {
        assert_eq!(snap(10.3, 1.0), 10.0);
        assert_eq!(snap(10.3, 2.0), 10.5);
        assert_eq!(snap(10.2, 2.0), 10.0);
        assert_eq!(hairline(1.0), 1.0);
        assert_eq!(hairline(2.0), 1.0);
        // 1.5×: two whole device pixels, not a blurred one and a half.
        assert!((hairline(1.5) * 1.5 - 2.0).abs() < 1e-6);
    }
}
