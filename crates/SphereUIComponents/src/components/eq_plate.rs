//! The response graph EQUZ8's and EQUZX's plates share: its coordinate
//! model, the curves sampled from the DSP's own filter design, and the
//! overlay painter for everything on it that moves — the analyser, the
//! curves of bands riding their dynamics, the markers and the nodes.
//!
//! One coordinate model serves painting and hit-testing: frequency maps
//! across [`GRAPH_W`] design pixels on a log scale from 20 Hz to 20 kHz, gain
//! down the graph's height across ±its range, analyser level from +6 dB at
//! the top to −90 dB at the bottom. The nodes are painted here, above the
//! curves; the plates lay transparent hit targets over them.

use std::sync::Arc;

use gpui::{App, Bounds, FontWeight, Pixels, Rgba, Window};
use SpherePluginHost::spectrum::{MAX_HZ, MIN_HZ, SPECTRUM_BINS};

use crate::components::plate::{
    self, color, color_a, paint_dot, paint_polygon, paint_polyline, paint_text, Align, DYN, INK,
    WELL,
};
use crate::components::plugin_live::Live;

/// The graph's width at the design size.
pub const GRAPH_W: f32 = 940.0;
/// Frequencies the curves are sampled at.
pub const CURVE_POINTS: usize = 240;
pub const FREQ_MIN: f32 = 20.0;
pub const FREQ_MAX: f32 = 20_000.0;

/// The labelled frequencies and the grid's lines.
pub const FREQ_LABELS: [(f32, &str); 8] = [
    (50.0, "50"),
    (100.0, "100"),
    (200.0, "200"),
    (500.0, "500"),
    (1_000.0, "1k"),
    (2_000.0, "2k"),
    (5_000.0, "5k"),
    (10_000.0, "10k"),
];
const MINOR_FREQS: [f32; 18] = [
    30.0, 40.0, 60.0, 70.0, 80.0, 90.0, 300.0, 400.0, 600.0, 700.0, 800.0, 900.0, 3_000.0, 4_000.0,
    6_000.0, 7_000.0, 8_000.0, 9_000.0,
];

/// Where `hz` sits across the graph, in design pixels.
pub fn x_of(hz: f32) -> f32 {
    GRAPH_W * (hz.clamp(FREQ_MIN, FREQ_MAX) / FREQ_MIN).log10() / 3.0
}

/// The frequency at `fraction` across the graph.
pub fn freq_at(fraction: f32) -> f32 {
    FREQ_MIN * 1_000f32.powf(fraction.clamp(0.0, 1.0))
}

/// Curve sample `index`'s frequency.
pub fn sample_hz(index: usize) -> f32 {
    freq_at(index as f32 / (CURVE_POINTS - 1) as f32)
}

/// A graph `height` tall showing ±`range` dB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale {
    pub height: f32,
    pub range: f32,
}

impl Scale {
    pub fn mid(&self) -> f32 {
        self.height * 0.5
    }

    /// Where gain `db` sits down the graph.
    pub fn y_of(&self, db: f32) -> f32 {
        let mid = self.mid();
        (mid - db * mid / self.range.max(1.0)).clamp(-6.0, self.height + 6.0)
    }

    /// The gain at design `y` down the graph.
    pub fn db_at(&self, y: f32) -> f32 {
        let mid = self.mid();
        (mid - y.clamp(0.0, self.height)) / mid * self.range.max(1.0)
    }

    /// Where analyser level `db` sits.
    pub fn y_of_level(&self, db: f32) -> f32 {
        ((6.0 - db) / 96.0 * self.height).clamp(0.0, self.height)
    }

    /// The labelled dB lines: every step from −range to +range, exclusive.
    pub fn ticks(&self) -> (f32, Vec<f32>) {
        let r = self.range;
        let step = if r <= 6.0 {
            2.0
        } else if r <= 12.0 {
            3.0
        } else if r <= 18.0 {
            6.0
        } else {
            10.0
        };
        let mut ticks = Vec::new();
        let mut v = -r + step;
        while v < r - 0.01 {
            ticks.push(v);
            v += step;
        }
        (step, ticks)
    }
}

/// `+12`, `−6`, `0`: a dB scale label.
pub fn db_label(v: f32) -> String {
    if v.abs() < 0.05 {
        "0".into()
    } else {
        plate::signed(v).replace(".0", "")
    }
}

/// What the analyser draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Analyzer {
    /// The input and, over it, the input through the EQ.
    PrePost,
    #[default]
    Post,
    Pre,
    Off,
}

/// One band as the graph needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphBand {
    /// The band's slot in the params.
    pub slot: usize,
    /// The number its node shows.
    pub number: usize,
    pub on: bool,
    pub freq: f32,
    pub gain_db: f32,
    pub has_gain: bool,
    /// On, dynamic and with a gain stage: its gain rides its detector.
    pub dynamic: bool,
    pub range_db: f32,
    /// A stereo placement tag over the node (`M`, `S`, `L`, `R`).
    pub tag: Option<&'static str>,
}

impl GraphBand {
    /// The node's gain: its own for a shape with gain, 0 dB for the rest.
    pub fn node_db(&self) -> f32 {
        if self.has_gain {
            self.gain_db
        } else {
            0.0
        }
    }
}

/// A band's response at a gain: `(slot, gain_db) -> CURVE_POINTS samples`.
pub type BandCurve = Arc<dyn Fn(usize, f32) -> Vec<f32>>;

/// The curves that move only with an edit: each band's at its own gain and,
/// for dynamic bands, at its full range.
pub struct StaticCurves {
    pub bands: Vec<GraphBand>,
    pub at_gain: Vec<Option<Vec<f32>>>,
    pub at_range: Vec<Option<Vec<f32>>>,
}

impl StaticCurves {
    pub fn compute(bands: Vec<GraphBand>, curve: &BandCurve) -> Self {
        let at_gain = bands
            .iter()
            .map(|b| b.on.then(|| curve(b.slot, b.gain_db)))
            .collect();
        let at_range = bands
            .iter()
            .map(|b| b.dynamic.then(|| curve(b.slot, b.gain_db + b.range_db)))
            .collect();
        Self {
            bands,
            at_gain,
            at_range,
        }
    }

    pub fn any_dynamic(&self) -> bool {
        self.bands.iter().any(|b| b.dynamic)
    }
}

/// Everything the overlay needs to paint the graph, cloned out each frame.
#[derive(Clone)]
pub struct GraphView {
    pub scale: Scale,
    pub analyzer: Analyzer,
    pub bypass: bool,
    pub curves: Arc<StaticCurves>,
    pub curve: BandCurve,
    /// The selected band's position in `curves.bands`.
    pub selected: Option<usize>,
    pub accent: Rgba,
    /// The telemetry slot of band slot 0's dynamic gain.
    pub gain_base: usize,
    /// Under the pointer: frequency and gain.
    pub hover: Option<(f32, f32)>,
    pub hint: Option<&'static str>,
    /// The curve fill's strength, and the node number's size.
    pub fill_alpha: f32,
    pub node_text: f32,
}

fn sum(curves: &[&Vec<f32>]) -> Vec<f32> {
    (0..CURVE_POINTS)
        .map(|k| curves.iter().map(|c| c[k]).sum())
        .collect()
}

/// The curve at bin `hz`, interpolated between samples.
fn curve_at(curve: &[f32], hz: f32) -> f32 {
    let f = (hz / FREQ_MIN).log10() / 3.0 * (CURVE_POINTS - 1) as f32;
    let i = (f.floor().max(0.0) as usize).min(CURVE_POINTS - 2);
    let t = (f - i as f32).clamp(0.0, 1.0);
    curve[i] * (1.0 - t) + curve[i + 1] * t
}

/// Paints the graph into `b`: grid, analyser, the dynamic band's swing, the
/// selected band, the whole EQ, the hover line, the dynamics markers and the
/// nodes.
pub fn paint_graph(
    window: &mut Window,
    cx: &mut App,
    live: &Live,
    view: &GraphView,
    b: Bounds<Pixels>,
    s: f32,
) {
    let at = plate::at(b, s);
    let scale = view.scale;
    let h = scale.height;
    let mid = scale.mid();
    let xs: Vec<f32> = (0..CURVE_POINTS).map(|i| x_of(sample_hz(i))).collect();
    let line_of = |curve: &[f32], y: &dyn Fn(f32) -> f32| -> Vec<(f32, f32)> {
        xs.iter().zip(curve).map(|(&x, &db)| at(x, y(db))).collect()
    };
    let yd = |db: f32| scale.y_of(db);

    // Grid, analyser and curves stay inside the graph: a cut drives its
    // curve past the bottom edge, and that must not show as a line under it.
    let curves = &view.curves;
    let mask = gpui::ContentMask { bounds: b };
    let offsets = window.with_content_mask(Some(mask), |window| {
        // Grid.
        let (step, ticks) = scale.ticks();
        for f in MINOR_FREQS {
            paint_polyline(
                window,
                &[at(x_of(f), 0.0), at(x_of(f), h)],
                1.0,
                color(0x1b1b1b),
                None,
            );
        }
        for v in ticks.iter().chain(std::iter::once(&scale.range)) {
            let y = yd(v - step * 0.5);
            paint_polyline(
                window,
                &[at(0.0, y), at(GRAPH_W, y)],
                1.0,
                color(0x1b1b1b),
                None,
            );
        }
        for (f, _) in FREQ_LABELS {
            paint_polyline(
                window,
                &[at(x_of(f), 0.0), at(x_of(f), h)],
                1.0,
                color(0x262626),
                None,
            );
        }
        for &v in ticks.iter().filter(|v| v.abs() > 0.01) {
            let y = yd(v);
            paint_polyline(
                window,
                &[at(0.0, y), at(GRAPH_W, y)],
                1.0,
                color(0x262626),
                None,
            );
        }
        paint_polyline(
            window,
            &[at(0.0, mid), at(GRAPH_W, mid)],
            1.0,
            color(0x3a3a3a),
            None,
        );

        // The curves as they play now: each dynamic band at its gain plus what
        // its detector is taking.
        let block = live.pads();
        let offsets: Vec<f32> = curves
            .bands
            .iter()
            .map(|band| {
                if !band.dynamic || view.bypass {
                    return 0.0;
                }
                block.map_or(0.0, |block| {
                    block
                        .get(view.gain_base + band.slot)
                        .copied()
                        .filter(|v| v.is_finite())
                        .unwrap_or(0.0)
                })
            })
            .collect();
        let live_curves: Vec<Option<Vec<f32>>> = curves
            .bands
            .iter()
            .zip(&curves.at_gain)
            .zip(&offsets)
            .map(|((band, stat), &offset)| {
                if band.dynamic && offset.abs() >= 0.05 {
                    Some((view.curve)(band.slot, band.gain_db + offset))
                } else {
                    stat.clone()
                }
            })
            .collect();
        let total_static = sum(&curves.at_gain.iter().flatten().collect::<Vec<_>>());
        let total = sum(&live_curves.iter().flatten().collect::<Vec<_>>());

        // Analyser: the input as the host measures it, and through the EQ.
        if view.analyzer != Analyzer::Off {
            if let Some(bins) = live.spectrum_bins() {
                let bin_hz = |i: usize| {
                    MIN_HZ * (MAX_HZ / MIN_HZ).powf(i as f32 / (SPECTRUM_BINS - 1) as f32)
                };
                let pre: Vec<(f32, f32)> = bins
                    .iter()
                    .enumerate()
                    .map(|(i, &db)| at(x_of(bin_hz(i)), scale.y_of_level(db)))
                    .collect();
                let post: Vec<(f32, f32)> = bins
                    .iter()
                    .enumerate()
                    .map(|(i, &db)| {
                        let eq = if view.bypass {
                            0.0
                        } else {
                            curve_at(&total, bin_hz(i))
                        };
                        at(x_of(bin_hz(i)), scale.y_of_level(db + eq))
                    })
                    .collect();
                let filled = |line: &[(f32, f32)]| {
                    let mut area = line.to_vec();
                    area.push(at(GRAPH_W, h));
                    area.push(at(0.0, h));
                    area
                };
                match view.analyzer {
                    Analyzer::PrePost => {
                        paint_polygon(window, &filled(&pre), color_a(0xffffff, 0.03));
                        paint_polyline(window, &pre, 1.0, color(0x3a3a3a), None);
                        paint_polyline(window, &post, 1.0, color(0x6a6a6a), None);
                    }
                    Analyzer::Post => {
                        paint_polygon(window, &filled(&post), color_a(0xffffff, 0.045));
                        paint_polyline(window, &post, 1.0, color(0x4d4d4d), None);
                    }
                    Analyzer::Pre => {
                        paint_polygon(window, &filled(&pre), color_a(0xffffff, 0.045));
                        paint_polyline(window, &pre, 1.0, color(0x4d4d4d), None);
                    }
                    Analyzer::Off => {}
                }
            }
        }

        // How far the dynamic bands have moved, and how far they can.
        if curves.any_dynamic() {
            let mut swing = line_of(&total_static, &yd);
            swing.extend(line_of(&total, &yd).into_iter().rev());
            paint_polygon(window, &swing, color_a(DYN, 0.25));
            let reach: Vec<&Vec<f32>> = curves
                .at_range
                .iter()
                .zip(&curves.at_gain)
                .filter_map(|(range, gain)| range.as_ref().or(gain.as_ref()))
                .collect();
            paint_polyline(
                window,
                &line_of(&sum(&reach), &yd),
                1.0,
                color_a(DYN, 0.7),
                Some((3.0 * s, 3.0 * s)),
            );
        }

        let close_to_zero = |line: Vec<(f32, f32)>| {
            let mut area = line;
            area.push(at(GRAPH_W, mid));
            area.push(at(0.0, mid));
            area
        };
        if let Some(sel) = view.selected {
            if let Some(curve) = live_curves.get(sel).and_then(Option::as_ref) {
                let line = line_of(curve, &yd);
                paint_polygon(
                    window,
                    &close_to_zero(line.clone()),
                    with_alpha(view.accent, 0.16),
                );
                paint_polyline(window, &line, 1.2 * s, with_alpha(view.accent, 0.8), None);
            }
        }
        let line = line_of(&total, &yd);
        let fill = if view.bypass { 0.02 } else { view.fill_alpha };
        paint_polygon(
            window,
            &close_to_zero(line.clone()),
            color_a(0xffffff, fill),
        );
        paint_polyline(
            window,
            &line,
            2.0 * s,
            color_a(0xf2f2f2, if view.bypass { 0.3 } else { 1.0 }),
            None,
        );

        if let Some((hz, _)) = view.hover {
            let x = x_of(hz);
            paint_polyline(
                window,
                &[at(x, 0.0), at(x, h)],
                1.0,
                color_a(0xffffff, 0.18),
                None,
            );
        }
        if let Some(hint) = view.hint {
            paint_text(
                window,
                cx,
                hint,
                plate::outfit(FontWeight::NORMAL),
                11.5 * s,
                color(plate::FAINT),
                at(10.0, 8.0),
                Align::Left,
            );
        }

        offsets
    });

    // The dynamics markers: where each riding band's gain has gone.
    for (band, &offset) in curves.bands.iter().zip(&offsets) {
        if !band.dynamic || offset.abs() < 0.05 {
            continue;
        }
        let x = x_of(band.freq);
        let (y1, y2) = (yd(band.gain_db), yd(band.gain_db + offset));
        let (lx, ly) = at(x - 1.0, y1.min(y2));
        plate::paint_rect(window, (lx, ly, 2.0 * s, (y2 - y1).abs() * s), color(DYN));
        let (dx, dy) = at(x, y2);
        paint_dot(window, dx, dy, 3.5 * s, color(DYN));
    }

    // Nodes, the selected one last so it sits on top.
    let mono = plate::mono(FontWeight::SEMIBOLD);
    let order: Vec<usize> = (0..curves.bands.len())
        .filter(|i| Some(*i) != view.selected)
        .chain(view.selected)
        .collect();
    for index in order {
        let band = &curves.bands[index];
        let selected = Some(index) == view.selected;
        let (cx0, cy0) = at(x_of(band.freq), yd(band.node_db()));
        if band.dynamic {
            paint_dot(window, cx0, cy0, 14.5 * s, color(DYN));
            paint_dot(window, cx0, cy0, 13.0 * s, color(WELL));
        }
        let (fill_ink, edge, text) = if !band.on {
            (color(WELL), color(0x5a5a5a), color(plate::FAINT))
        } else if selected {
            (view.accent, view.accent, color(WELL))
        } else {
            (color(INK), color(INK), color(WELL))
        };
        paint_dot(window, cx0, cy0, 11.0 * s, edge);
        paint_dot(window, cx0, cy0, 9.5 * s, fill_ink);
        paint_text(
            window,
            cx,
            &band.number.to_string(),
            mono.clone(),
            view.node_text * s,
            text,
            (cx0, cy0 - view.node_text * 0.65 * s),
            Align::Center,
        );
        if let Some(tag) = band.tag {
            let (w, th) = (6.0 + 6.0 * tag.len() as f32, 12.0);
            let (tx, ty) = (cx0 + 21.0 * s - w * s, cy0 - 19.0 * s);
            window.paint_quad(
                gpui::fill(plate::rect(tx, ty, w * s, (th + 2.0) * s), color(0x353535))
                    .corner_radii(gpui::px(2.0 * s))
                    .border_widths(gpui::px(1.0))
                    .border_color(color(0x555555)),
            );
            paint_text(
                window,
                cx,
                tag,
                mono.clone(),
                9.0 * s,
                color(plate::WHITE),
                (tx + w * 0.5 * s, ty + 1.0 * s),
                Align::Center,
            );
        }
    }
}

fn with_alpha(ink: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha, ..ink }
}

/// Where a node drag at window `(x, y)` puts a band in a graph at `b`:
/// its frequency, and its gain on a ±`range` scale.
pub fn drag_target(b: Bounds<Pixels>, scale: Scale, (x, y): (f32, f32)) -> (f32, f32) {
    let (bx, by) = plate::xy(b.origin);
    let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
    let fx = ((x - bx) / bw.max(1.0)).clamp(0.0, 1.0);
    let fy = ((y - by) / bh.max(1.0)).clamp(0.0, 1.0) * scale.height;
    (freq_at(fx), scale.db_at(fy))
}

/// Note name and cents of `hz`: `A4 +0c`.
pub fn note_of(hz: f32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let n = 12.0 * (hz.max(1.0) / 440.0).log2() + 69.0;
    let r = n.round();
    let cents = ((n - r) * 100.0).round() as i32;
    let name = NAMES[(r as i32).rem_euclid(12) as usize];
    let octave = (r / 12.0).floor() as i32 - 1;
    let sign = if cents >= 0 { "+" } else { "\u{2212}" };
    format!("{name}{octave} {sign}{}c", cents.abs())
}

/// The design's band shape icons, in a 20 × 14 box.
pub const ICON_LOW_CUT: &str = "M1 12 C4 12 5 3 9 3 L19 3";
pub const ICON_LOW_SHELF: &str = "M1 3 L5 3 C9 3 9 10 13 10 L19 10";
pub const ICON_BELL: &str = "M1 10 L6 10 C8 10 8.5 2 10 2 C11.5 2 12 10 14 10 L19 10";
pub const ICON_NOTCH: &str = "M1 4 L8 4 C9 4 9.4 12 10 12 C10.6 12 11 4 12 4 L19 4";
pub const ICON_HIGH_SHELF: &str = "M1 10 L7 10 C11 10 11 3 15 3 L19 3";
pub const ICON_HIGH_CUT: &str = "M1 3 L11 3 C15 3 16 12 19 12";
pub const ICON_BAND_PASS: &str = "M1 12 C6 12 8 2 10 2 C12 2 14 12 19 12";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_axis_spans_three_decades() {
        assert!(x_of(20.0).abs() < 1e-4);
        assert!((x_of(20_000.0) - GRAPH_W).abs() < 1e-3);
        assert!((x_of(1_000.0) - 532.3).abs() < 0.5);
        assert!((freq_at(x_of(632.0) / GRAPH_W) - 632.0).abs() < 0.5);
    }

    #[test]
    fn gain_maps_both_ways() {
        let scale = Scale {
            height: 262.0,
            range: 18.0,
        };
        assert!((scale.y_of(0.0) - 131.0).abs() < 1e-4);
        assert!((scale.db_at(scale.y_of(-6.0)) + 6.0).abs() < 1e-3);
        let (step, ticks) = scale.ticks();
        assert_eq!(step, 6.0);
        assert_eq!(ticks, vec![-12.0, -6.0, 0.0, 6.0, 12.0]);
    }

    #[test]
    fn notes_are_named_with_cents() {
        assert_eq!(note_of(440.0), "A4 +0c");
        assert_eq!(note_of(261.63), "C4 +0c");
    }
}
