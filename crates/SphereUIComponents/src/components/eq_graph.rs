//! Frequency-axis geometry and the line and area painters the native
//! built-in editors' graphs share (VerbSpace/EchoSpace, the dynamics and
//! band families, the live displays).
//!
//! Frequency maps to a fraction of a plot's width on a log scale from
//! [`FREQ_MIN`] to [`FREQ_MAX`].

use gpui::{point, px, PathBuilder, Rgba, Window};

pub const FREQ_MIN: f32 = 20.0;
pub const FREQ_MAX: f32 = 20_000.0;

/// Grid frequencies.
pub const FREQ_GRID: [f32; 28] = [
    20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 200.0, 300.0, 400.0, 500.0, 600.0,
    700.0, 800.0, 900.0, 1_000.0, 2_000.0, 3_000.0, 4_000.0, 5_000.0, 6_000.0, 7_000.0, 8_000.0,
    9_000.0, 10_000.0, 20_000.0,
];

/// Fraction across the plot (0 left, 1 right) of `hz`.
pub fn freq_fraction(hz: f32) -> f32 {
    let hz = hz.clamp(FREQ_MIN, FREQ_MAX);
    (hz / FREQ_MIN).ln() / (FREQ_MAX / FREQ_MIN).ln()
}

/// The frequency at `fraction` across the plot.
pub fn freq_at_fraction(fraction: f32) -> f32 {
    FREQ_MIN * (FREQ_MAX / FREQ_MIN).powf(fraction.clamp(0.0, 1.0))
}

pub(crate) fn paint_line(window: &mut Window, points: &[(f32, f32)], width: f32, color: Rgba) {
    if points.len() < 2 {
        return;
    }
    let mut line = PathBuilder::stroke(px(width));
    line.move_to(point(px(points[0].0), px(points[0].1)));
    for &(x, y) in &points[1..] {
        line.line_to(point(px(x), px(y)));
    }
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
}

/// The area between `points` and the 0 dB line.
pub(crate) fn paint_area(window: &mut Window, points: &[(f32, f32)], zero_y: f32, color: Rgba) {
    if points.len() < 2 {
        return;
    }
    let mut area = PathBuilder::fill();
    area.move_to(point(px(points[0].0), px(zero_y)));
    for &(x, y) in points {
        area.line_to(point(px(x), px(y)));
    }
    area.line_to(point(px(points[points.len() - 1].0), px(zero_y)));
    area.close();
    if let Ok(path) = area.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_axis_round_trips() {
        assert_eq!(freq_fraction(FREQ_MIN), 0.0);
        assert!((freq_fraction(FREQ_MAX) - 1.0).abs() < 1e-6);
        assert!((freq_at_fraction(freq_fraction(1_000.0)) - 1_000.0).abs() < 0.1);
    }
}
