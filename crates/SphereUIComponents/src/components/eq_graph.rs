//! The response graph both built-in EQs share: geometry, the curve set, and
//! painting.
//!
//! One coordinate model serves drawing, the overlaid nodes and labels, and
//! hit-testing: frequency maps to a fraction of the plot's width on a log
//! scale from [`FREQ_MIN`] to [`FREQ_MAX`], gain to a fraction of its height
//! across ±the editor's dB range. The labels and numbered nodes are divs
//! placed by those fractions; the canvas paints by them; the window hit-tests
//! by them against the canvas bounds captured at prepaint.
//!
//! Curves are sampled once per edit ([`Curves::compute`]) at
//! [`CURVE_POINTS`] log-spaced frequencies, not per paint: the analyser
//! repaints the graph about thirty times a second, the curves only move
//! when a parameter does.

use std::sync::Arc;

use gpui::{fill, point, px, Bounds, PathBuilder, Pixels, Rgba, Window};

use crate::components::eq_model::{
    EqParams, Placement, FREQ_MAX, FREQ_MIN, GAIN_MAX_DB, GAIN_MIN_DB,
};
use crate::components::quick_sampler_panel::rect;
use crate::theme::Colors;

/// Frequencies the curves are sampled at.
pub const CURVE_POINTS: usize = 256;
/// The dB spans the graph can show, ± each.
pub const DB_RANGES: [f32; 4] = [6.0, 12.0, 18.0, 30.0];
pub const DEFAULT_DB_RANGE: f32 = 18.0;
/// How near the pointer must be to a node to take it, in pixels.
pub const NODE_HIT_RADIUS: f32 = 12.0;
/// The whole EQ's fill under its curve: a wash, so the band curves and the
/// analyser read through it.
const TOTAL_FILL_ALPHA: f32 = 0.12;
/// Grid frequencies; the labelled ones are listed in [`FREQ_LABELS`].
pub const FREQ_GRID: [f32; 28] = [
    20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 200.0, 300.0, 400.0, 500.0, 600.0,
    700.0, 800.0, 900.0, 1_000.0, 2_000.0, 3_000.0, 4_000.0, 5_000.0, 6_000.0, 7_000.0, 8_000.0,
    9_000.0, 10_000.0, 20_000.0,
];
pub const FREQ_LABELS: [(f32, &str); 9] = [
    (30.0, "30"),
    (60.0, "60"),
    (100.0, "100"),
    (300.0, "300"),
    (600.0, "600"),
    (1_000.0, "1k"),
    (3_000.0, "3k"),
    (6_000.0, "6k"),
    (10_000.0, "10k"),
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

/// Fraction down the plot (0 top) of `db` on a ±`range` scale.
pub fn db_fraction(db: f32, range: f32) -> f32 {
    0.5 - db / (2.0 * range.max(1.0))
}

/// The gain at `fraction` down the plot, on a ±`range` scale.
pub fn db_at_fraction(fraction: f32, range: f32) -> f32 {
    (0.5 - fraction) * 2.0 * range.max(1.0)
}

/// The dB lines a ±`range` grid draws, top to bottom, each with its label.
pub fn db_grid(range: f32) -> Vec<f32> {
    let step = if range <= 12.0 { 3.0 } else { 6.0 };
    let mut lines = Vec::new();
    let mut db = (range / step).floor() * step;
    while db >= -range {
        lines.push(db);
        db -= step;
    }
    lines
}

/// The frequency of curve sample `index`.
pub fn sample_hz(index: usize) -> f32 {
    freq_at_fraction(index as f32 / (CURVE_POINTS - 1) as f32)
}

/// What the graph draws as lines, sampled at [`CURVE_POINTS`] frequencies.
#[derive(Clone, Debug, Default)]
pub struct Curves {
    /// The whole EQ, for the view shown.
    pub total: Vec<f32>,
    /// Each switched-on band heard in the view, by slot.
    pub bands: Vec<(usize, Vec<f32>)>,
    /// The selected band with its dynamics fully engaged — where it can
    /// move to. `None` without live dynamics.
    pub ghost: Option<(usize, Vec<f32>)>,
}

impl Curves {
    pub fn compute(
        params: &EqParams,
        view: Placement,
        selected: Option<usize>,
        sample_rate: f32,
    ) -> Self {
        let total = (0..CURVE_POINTS)
            .map(|i| params.response_db(view, sample_hz(i), sample_rate))
            .collect();
        let band_curve = |index: usize, gain: f32| -> Vec<f32> {
            (0..CURVE_POINTS)
                .map(|i| params.band_response_db(index, gain, sample_hz(i), sample_rate))
                .collect()
        };
        let bands = params
            .listed()
            .into_iter()
            .filter(|index| {
                let band = params.band(*index);
                band.active && band.placement.heard_in(view)
            })
            .map(|index| (index, band_curve(index, params.band(index).gain_db)))
            .collect();
        let ghost = selected.and_then(|index| {
            let band = params.band(index);
            (band.active
                && band.dynamics_live()
                && band.range_db.abs() >= 0.01
                && band.placement.heard_in(view))
            .then(|| (index, band_curve(index, band.gain_db + band.range_db)))
        });
        Self {
            total,
            bands,
            ghost,
        }
    }
}

/// The colour a band is known by: on its node, its strip cell and its curve.
/// Cycles through the theme's categorical hues; the accent stays reserved
/// for selection.
pub fn band_color(index: usize) -> Rgba {
    match index % 6 {
        0 => Colors::track_bus(),
        1 => Colors::track_audio(),
        2 => Colors::track_instrument(),
        3 => Colors::accent_warning(),
        4 => Colors::accent_danger(),
        _ => Colors::accent_purple(),
    }
}

/// Every colour the canvas needs, resolved before painting.
#[derive(Clone, Copy)]
pub struct GraphPalette {
    pub background: Rgba,
    pub grid: Rgba,
    pub grid_major: Rgba,
    pub zero: Rgba,
    pub spectrum_top: Rgba,
    pub spectrum_bottom: Rgba,
    pub total: Rgba,
    pub total_fill: Rgba,
    pub hover: Rgba,
}

impl GraphPalette {
    pub fn resolve() -> Self {
        let ink = Colors::text_primary();
        Self {
            background: Colors::surface_canvas(),
            grid: Colors::with_alpha(ink, 0.045),
            grid_major: Colors::with_alpha(ink, 0.09),
            zero: Colors::with_alpha(ink, 0.18),
            spectrum_top: Colors::with_alpha(Colors::text_secondary(), 0.30),
            spectrum_bottom: Colors::with_alpha(Colors::text_secondary(), 0.04),
            total: Colors::accent_primary(),
            total_fill: Colors::accent_primary(),
            hover: Colors::with_alpha(ink, 0.22),
        }
    }
}

/// Everything one paint of the graph reads.
#[derive(Clone)]
pub struct GraphPaint {
    pub curves: Arc<Curves>,
    pub db_range: f32,
    /// The analyser's latest frame, dB per bin, `None` while off or silent.
    pub spectrum: Option<Arc<[f32; SpherePluginHost::spectrum::SPECTRUM_BINS]>>,
    pub show_band_curves: bool,
    pub selected: Option<usize>,
    pub bypassed: bool,
    /// The pointer's fraction across the plot, for the hover line.
    pub hover: Option<f32>,
    pub palette: GraphPalette,
    /// Each drawn band's colour, by slot.
    pub band_colors: Arc<Vec<Rgba>>,
}

/// Paints the plot — grid, analyser, curves, hover line — into `bounds`.
/// Square edges: it is a data canvas (see DESIGN.md's must-stay-square list).
pub fn paint_graph(window: &mut Window, bounds: Bounds<Pixels>, g: &GraphPaint) {
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let p = &g.palette;
    window.paint_quad(fill(bounds, p.background));

    for hz in FREQ_GRID {
        let major = FREQ_LABELS.iter().any(|(label, _)| *label == hz);
        let x = x0 + freq_fraction(hz) * w;
        window.paint_quad(fill(
            rect(x, y0, 1.0, h),
            if major { p.grid_major } else { p.grid },
        ));
    }
    for db in db_grid(g.db_range) {
        let y = y0 + db_fraction(db, g.db_range) * h;
        let color = if db == 0.0 { p.zero } else { p.grid_major };
        window.paint_quad(fill(rect(x0, y, w, 1.0), color));
    }

    // The analyser, behind the curves: the signal arriving at the insert.
    if let (Some(bins), false) = (g.spectrum.as_ref(), g.bypassed) {
        paint_spectrum(window, bounds, bins.as_ref(), p);
    }

    let dim = if g.bypassed { 0.3 } else { 1.0 };
    let zero_y = y0 + db_fraction(0.0, g.db_range) * h;
    let to_points = |curve: &[f32]| -> Vec<(f32, f32)> {
        curve
            .iter()
            .enumerate()
            .map(|(i, db)| {
                let x = x0 + i as f32 / (CURVE_POINTS - 1) as f32 * w;
                let y =
                    y0 + db_fraction(db.clamp(-g.db_range * 1.4, g.db_range * 1.4), g.db_range) * h;
                (x, y.clamp(y0, y0 + h))
            })
            .collect()
    };
    let color_of = |index: usize| {
        g.band_colors
            .get(index)
            .copied()
            .unwrap_or_else(Colors::text_secondary)
    };

    for (index, curve) in &g.curves.bands {
        let selected = g.selected == Some(*index);
        if !g.show_band_curves && !selected {
            continue;
        }
        let color = color_of(*index);
        let points = to_points(curve);
        if selected {
            paint_area(
                window,
                &points,
                zero_y,
                Colors::with_alpha(color, 0.14 * dim),
            );
            paint_line(window, &points, 1.4, Colors::with_alpha(color, 0.9 * dim));
        } else {
            paint_line(window, &points, 1.0, Colors::with_alpha(color, 0.38 * dim));
        }
    }
    if let Some((index, curve)) = &g.curves.ghost {
        paint_line(
            window,
            &to_points(curve),
            1.0,
            Colors::with_alpha(color_of(*index), 0.5 * dim),
        );
    }

    let total = to_points(&g.curves.total);
    // `with_alpha` sets the alpha outright, so the fill's own is scaled here.
    paint_area(
        window,
        &total,
        zero_y,
        Colors::with_alpha(p.total_fill, TOTAL_FILL_ALPHA * dim),
    );
    paint_line(
        window,
        &total,
        2.0,
        Colors::with_alpha(p.total, if g.bypassed { 0.35 } else { 1.0 }),
    );

    if let Some(fraction) = g.hover {
        window.paint_quad(fill(rect(x0 + fraction * w, y0, 1.0, h), p.hover));
    }
}

fn paint_spectrum(window: &mut Window, bounds: Bounds<Pixels>, bins: &[f32], p: &GraphPalette) {
    use SpherePluginHost::spectrum::{CEIL_DB, FLOOR_DB, MAX_HZ, MIN_HZ};
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let bottom = y0 + h;
    let count = bins.len().max(1) as f32;
    let mut area = PathBuilder::fill();
    area.move_to(point(px(x0), px(bottom)));
    for (i, db) in bins.iter().enumerate() {
        // Each bin's centre on the analyser's own log axis.
        let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf((i as f32 + 0.5) / count);
        let level = ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0) * 0.92;
        area.line_to(point(
            px(x0 + freq_fraction(hz) * w),
            px(bottom - level * h),
        ));
    }
    area.line_to(point(px(x0 + w), px(bottom)));
    area.close();
    if let Ok(path) = area.build() {
        window.paint_path(
            path,
            crate::components::quick_sampler_panel::vertical(p.spectrum_top, p.spectrum_bottom),
        );
    }
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

/// Where band `index`'s node sits, as fractions across and down the plot.
pub fn node_fractions(params: &EqParams, index: usize, db_range: f32) -> (f32, f32) {
    let band = params.band(index);
    (
        freq_fraction(band.freq),
        db_fraction(band.node_gain_db(), db_range).clamp(0.0, 1.0),
    )
}

/// The band whose node is under window-space `(x, y)` in a plot at
/// `bounds`: the nearest within [`NODE_HIT_RADIUS`], the selected band
/// winning a tie so a stack of nodes can still be dragged apart.
pub fn node_at(
    params: &EqParams,
    view: Placement,
    db_range: f32,
    bounds: Bounds<Pixels>,
    (x, y): (f32, f32),
    selected: Option<usize>,
) -> Option<usize> {
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let mut best: Option<(usize, f32)> = None;
    for index in params.listed() {
        if !params.band(index).placement.heard_in(view) {
            continue;
        }
        let (fx, fy) = node_fractions(params, index, db_range);
        let distance = ((x0 + fx * w - x).powi(2) + (y0 + fy * h - y).powi(2)).sqrt();
        if distance > NODE_HIT_RADIUS {
            continue;
        }
        let distance = if selected == Some(index) {
            distance - 0.5
        } else {
            distance
        };
        if best.is_none_or(|(_, nearest)| distance < nearest) {
            best = Some((index, distance));
        }
    }
    best.map(|(index, _)| index)
}

/// Window-space `(x, y)` as a frequency and gain in a plot at `bounds`.
pub fn point_value(bounds: Bounds<Pixels>, (x, y): (f32, f32), db_range: f32) -> (f32, f32) {
    let fx = (x - f32::from(bounds.origin.x)) / f32::from(bounds.size.width).max(1.0);
    let fy = (y - f32::from(bounds.origin.y)) / f32::from(bounds.size.height).max(1.0);
    (
        freq_at_fraction(fx),
        db_at_fraction(fy, db_range).clamp(GAIN_MIN_DB, GAIN_MAX_DB),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::eq_model::{new_band, EqKind, Shape};

    #[test]
    fn the_axes_map_both_ways() {
        assert!(freq_fraction(FREQ_MIN).abs() < 1.0e-6);
        assert!((freq_fraction(FREQ_MAX) - 1.0).abs() < 1.0e-6);
        for hz in [25.0, 440.0, 3_000.0, 18_000.0] {
            assert!((freq_at_fraction(freq_fraction(hz)) - hz).abs() / hz < 1.0e-4);
        }
        assert_eq!(db_fraction(0.0, 18.0), 0.5);
        assert_eq!(db_fraction(18.0, 18.0), 0.0);
        assert!((db_at_fraction(db_fraction(-7.5, 12.0), 12.0) + 7.5).abs() < 1.0e-4);
        assert_eq!(
            db_grid(12.0),
            vec![12.0, 9.0, 6.0, 3.0, 0.0, -3.0, -6.0, -9.0, -12.0]
        );
        assert_eq!(db_grid(18.0).len(), 7);
    }

    #[test]
    fn a_node_is_found_where_it_is_drawn() {
        let mut params = EqParams::defaults(EqKind::Zx);
        params.set_band(3, new_band(EqKind::Zx, 1_000.0, 6.0, Placement::Stereo));
        let bounds = Bounds {
            origin: point(px(100.0), px(50.0)),
            size: gpui::size(px(600.0), px(300.0)),
        };
        let (fx, fy) = node_fractions(&params, 3, 18.0);
        let at = (100.0 + fx * 600.0, 50.0 + fy * 300.0);
        assert_eq!(
            node_at(&params, Placement::Stereo, 18.0, bounds, at, None),
            Some(3)
        );
        assert_eq!(
            node_at(
                &params,
                Placement::Stereo,
                18.0,
                bounds,
                (at.0 + 40.0, at.1),
                None
            ),
            None
        );
        // Its own place reads back as its own values.
        let (hz, db) = point_value(bounds, at, 18.0);
        assert!((hz - 1_000.0).abs() < 1.0);
        assert!((db - 6.0).abs() < 0.01);
        // A side band is not there in a mid view.
        let mut band = params.band(3);
        band.placement = Placement::Side;
        params.set_band(3, band);
        assert_eq!(
            node_at(&params, Placement::Mid, 18.0, bounds, at, None),
            None
        );
    }

    #[test]
    fn curves_follow_the_bands_and_the_view() {
        let mut params = EqParams::defaults(EqKind::Zx);
        params.set_band(0, new_band(EqKind::Zx, 1_000.0, 6.0, Placement::Mid));
        let mut dynamic = new_band(EqKind::Zx, 4_000.0, 0.0, Placement::Side);
        dynamic.dynamic = true;
        dynamic.range_db = -6.0;
        dynamic.shape = Shape::Bell;
        params.set_band(1, dynamic);
        let mid = Curves::compute(&params, Placement::Mid, Some(1), 48_000.0);
        assert_eq!(mid.bands.len(), 1, "the side band is not in the mid view");
        assert!(mid.ghost.is_none());
        let side = Curves::compute(&params, Placement::Side, Some(1), 48_000.0);
        let ghost = side.ghost.expect("a dynamic band shows where it can go");
        let at_4k = (freq_fraction(4_000.0) * (CURVE_POINTS - 1) as f32).round() as usize;
        assert!(ghost.1[at_4k] < -5.0);
        let peak = mid.total.iter().fold(f32::MIN, |a, b| a.max(*b));
        assert!((peak - 6.0).abs() < 0.2, "{peak}");
    }
}
