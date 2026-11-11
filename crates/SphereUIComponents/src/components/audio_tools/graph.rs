//! Display-space analyzer graphs.
//!
//! Analysis bins stay in the DSP snapshot. This module remaps them onto the
//! current plot width, optionally smooths for drawing, and paints an
//! anti-aliased filled contour. Nothing here runs on the audio callback.

use gpui::{
    canvas, fill, point, px, size, App, Bounds, IntoElement, PathBuilder, PathStyle, Pixels, Point,
    ShapedLine, StrokeOptions, Styled, TextAlign, TextRun, Window,
};

use crate::theme::{typography, Colors};

pub const DB_FLOOR: f32 = -96.0;
pub const DB_CEIL: f32 = 12.0;
pub const MIN_HZ: f32 = 20.0;

const PAD_L: f32 = 30.0;
const PAD_R: f32 = 8.0;
const PAD_T: f32 = 10.0;
const PAD_B: f32 = 16.0;

const FREQ_TICKS: [f32; 10] = [
    20.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0,
];
const DB_TICKS: [f32; 6] = [0.0, -12.0, -24.0, -48.0, -72.0, -96.0];

const MIN_DISPLAY_POINTS: usize = 256;
const MAX_DISPLAY_POINTS: usize = 2048;
const STROKE_W: f32 = 1.6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DisplaySmoothing {
    Off,
    #[default]
    Light,
    Medium,
    Heavy,
}

impl DisplaySmoothing {
    pub const ALL: [Self; 4] = [Self::Off, Self::Light, Self::Medium, Self::Heavy];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Light => "Light",
            Self::Medium => "Med",
            Self::Heavy => "Heavy",
        }
    }

    fn radius(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Light => 2,
            Self::Medium => 4,
            Self::Heavy => 8,
        }
    }

    fn mix(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Light => 0.35,
            Self::Medium => 0.55,
            Self::Heavy => 0.72,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GraphStyle {
    Fill,
    Line,
    #[default]
    FillAndLine,
    Overlay,
}

impl GraphStyle {
    pub const ALL: [Self; 4] = [Self::Fill, Self::Line, Self::FillAndLine, Self::Overlay];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Fill => "Fill",
            Self::Line => "Line",
            Self::FillAndLine => "Both",
            Self::Overlay => "Cmp",
        }
    }

    fn fill(self) -> bool {
        matches!(self, Self::Fill | Self::FillAndLine | Self::Overlay)
    }

    fn line(self) -> bool {
        matches!(self, Self::Line | Self::FillAndLine | Self::Overlay)
    }

    fn overlay(self) -> bool {
        matches!(self, Self::Overlay)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GraphDraw {
    pub smoothing: DisplaySmoothing,
    pub style: GraphStyle,
    pub hover: Option<Point<Pixels>>,
}

#[derive(Clone)]
pub struct SpectrumLayer {
    pub label: &'static str,
    pub db: Vec<f32>,
    pub color: gpui::Rgba,
    pub fill_alpha: f32,
    pub line: bool,
    pub fill: bool,
}

#[derive(Clone)]
pub struct AnalyzerPlot {
    pub sample_rate: u32,
    pub min_hz: f32,
    pub max_hz: f32,
    pub db_floor: f32,
    pub db_ceil: f32,
    pub layers: Vec<SpectrumLayer>,
    pub threshold_db: Option<f32>,
    pub harmonic_hz: Vec<f32>,
    pub band: Option<(f32, f32)>,
    pub draw: GraphDraw,
}

impl Default for AnalyzerPlot {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            min_hz: MIN_HZ,
            max_hz: 0.0,
            db_floor: DB_FLOOR,
            db_ceil: DB_CEIL,
            layers: Vec::new(),
            threshold_db: None,
            harmonic_hz: Vec::new(),
            band: None,
            draw: GraphDraw::default(),
        }
    }
}

pub fn display_point_count(width: f32, scale: f32) -> usize {
    let scaled = (width.max(1.0) * scale.max(1.0)).round() as usize;
    scaled.clamp(MIN_DISPLAY_POINTS, MAX_DISPLAY_POINTS)
}

pub fn log_hz(t: f32, min_hz: f32, max_hz: f32) -> f32 {
    let min_hz = min_hz.max(1.0);
    let max_hz = max_hz.max(min_hz + 1.0);
    let t = t.clamp(0.0, 1.0);
    min_hz * (max_hz / min_hz).powf(t)
}

pub fn hz_to_t(hz: f32, min_hz: f32, max_hz: f32) -> f32 {
    let min_hz = min_hz.max(1.0);
    let max_hz = max_hz.max(min_hz + 1.0);
    let hz = hz.clamp(min_hz, max_hz);
    (hz / min_hz).log10() / (max_hz / min_hz).log10()
}

/// Peak-preserving remap of FFT bins onto a log-frequency display axis.
///
/// Low frequencies collapse many bins into one display sample (max of the
/// range, so resonances survive). High frequencies stretch a bin across
/// several samples (linear interpolation, so the contour is not a step).
pub fn resample_spectrum_log(
    magnitudes_db: &[f32],
    sample_rate: u32,
    count: usize,
    min_hz: f32,
    max_hz: f32,
) -> Vec<f32> {
    if magnitudes_db.is_empty() || count == 0 {
        return vec![DB_FLOOR; count.max(1)];
    }
    let bins = magnitudes_db.len();
    let fft = (bins * 2).max(2) as f32;
    let sr = sample_rate.max(1) as f32;
    let min_hz = min_hz.max(MIN_HZ);
    let max_hz = max_hz.max(min_hz + 1.0);
    let mut out = vec![DB_FLOOR; count];
    for (i, slot) in out.iter_mut().enumerate() {
        let t0 = i as f32 / count as f32;
        let t1 = (i + 1) as f32 / count as f32;
        let hz0 = log_hz(t0, min_hz, max_hz);
        let hz1 = log_hz(t1, min_hz, max_hz);
        let b0 = ((hz0 * fft / sr).floor() as usize).min(bins.saturating_sub(1));
        let b1 = ((hz1 * fft / sr).ceil() as usize).min(bins);
        if b1 <= b0 + 1 {
            let bin_f = (hz0 + hz1) * 0.5 * fft / sr;
            *slot = interp_bin(magnitudes_db, bin_f);
        } else {
            let mut peak = DB_FLOOR;
            for mag in magnitudes_db.iter().take(b1).skip(b0) {
                peak = peak.max(*mag);
            }
            *slot = peak;
        }
    }
    out
}

fn interp_bin(mags: &[f32], bin_f: f32) -> f32 {
    if mags.is_empty() {
        return DB_FLOOR;
    }
    let last_i = mags.len() - 1;
    let bin_f = bin_f.clamp(0.0, last_i as f32);
    let i = bin_f.floor() as usize;
    let t = bin_f - i as f32;
    let p0 = mags[i.saturating_sub(1)];
    let p1 = mags[i];
    let p2 = mags.get(i + 1).copied().unwrap_or(p1);
    let p3 = mags.get((i + 2).min(last_i)).copied().unwrap_or(p2);
    let t2 = t * t;
    let t3 = t2 * t;
    let y = 0.5
        * (2.0 * p1
            + (-p0 + p2) * t
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3);
    y.clamp(p1.min(p2), p1.max(p2))
}

/// Light Gaussian blur that keeps local maxima at their original height.
pub fn peak_preserving_smooth(values: &[f32], smoothing: DisplaySmoothing) -> Vec<f32> {
    let radius = smoothing.radius();
    let mix = smoothing.mix();
    if radius == 0 || values.len() < 3 || mix <= 0.0 {
        return values.to_vec();
    }
    let n = values.len();
    let sigma = radius as f32 * 0.6 + 0.4;
    let mut kernel = Vec::with_capacity(radius * 2 + 1);
    let mut ksum = 0.0f32;
    for d in 0..=radius * 2 {
        let x = d as f32 - radius as f32;
        let w = (-0.5 * (x / sigma) * (x / sigma)).exp();
        kernel.push(w);
        ksum += w;
    }
    for w in &mut kernel {
        *w /= ksum.max(1.0e-6);
    }
    let mut out = vec![0.0f32; n];
    for i in 0..n {
        let mut acc = 0.0f32;
        for (k, w) in kernel.iter().enumerate() {
            let j = i as i32 + k as i32 - radius as i32;
            let j = j.clamp(0, (n - 1) as i32) as usize;
            acc += values[j] * *w;
        }
        let left = if i == 0 { values[i] } else { values[i - 1] };
        let right = values.get(i + 1).copied().unwrap_or(values[i]);
        let is_peak = values[i] >= left && values[i] >= right && values[i] > acc;
        out[i] = if is_peak {
            values[i]
        } else {
            values[i] + (acc - values[i]) * mix
        };
    }
    out
}

pub fn resample_series(values: &[f32], count: usize) -> Vec<f32> {
    if values.is_empty() || count == 0 {
        return vec![0.0; count.max(1)];
    }
    if values.len() == count {
        return values.to_vec();
    }
    let last = (values.len() - 1) as f32;
    (0..count)
        .map(|i| {
            let t = i as f32 / (count - 1).max(1) as f32;
            interp_bin(values, t * last)
        })
        .collect()
}

fn plot_rect(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    let width = f32::from(bounds.size.width).max(0.0);
    let height = f32::from(bounds.size.height).max(0.0);
    let plot_w = (width - PAD_L - PAD_R).max(2.0);
    let plot_h = (height - PAD_T - PAD_B).max(2.0);
    (PAD_L, PAD_T, plot_w, plot_h)
}

pub fn db_to_y(db: f32, plot_h: f32, floor: f32, ceil: f32) -> f32 {
    let span = (ceil - floor).max(1.0);
    let t = ((db - floor) / span).clamp(0.0, 1.0);
    plot_h * (1.0 - t)
}

fn hz_x(hz: f32, plot_w: f32, min_hz: f32, max_hz: f32) -> f32 {
    hz_to_t(hz, min_hz, max_hz) * plot_w
}

fn format_hz(hz: f32) -> String {
    if hz >= 1000.0 {
        let k = hz / 1000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{:.0}k", k)
        } else {
            format!("{:.2} kHz", k)
        }
    } else {
        format!("{:.0}", hz)
    }
}

fn shape_label(
    window: &mut Window,
    text: &str,
    color: impl Into<gpui::Hsla>,
    size: f32,
) -> ShapedLine {
    let color = color.into();
    let font = window.text_style().font();
    window.text_system().shape_line(
        text.to_string().into(),
        px(size),
        &[TextRun {
            len: text.len(),
            font,
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    )
}

struct TickGlyph {
    line: ShapedLine,
    x: f32,
    y: f32,
}

pub struct PlotLabels {
    ticks: Vec<TickGlyph>,
    hover: Option<(ShapedLine, f32, f32)>,
}

impl AnalyzerPlot {
    fn axis(&self) -> (f32, f32, f32, f32) {
        let min_hz = if self.min_hz > 0.0 {
            self.min_hz
        } else {
            MIN_HZ
        };
        let nyquist = self.sample_rate.max(1) as f32 * 0.5;
        let max_hz = if self.max_hz > min_hz {
            self.max_hz.min(nyquist)
        } else {
            nyquist.max(min_hz + 1.0)
        };
        let floor = if self.db_floor < self.db_ceil {
            self.db_floor
        } else {
            DB_FLOOR
        };
        let ceil = if self.db_ceil > floor {
            self.db_ceil
        } else {
            DB_CEIL
        };
        (min_hz, max_hz, floor, ceil)
    }

    pub fn prepare_labels(&self, bounds: Bounds<Pixels>, window: &mut Window) -> PlotLabels {
        let (ox, oy, plot_w, plot_h) = plot_rect(bounds);
        let (min_hz, max_hz, floor, ceil) = self.axis();
        let tick_color = Colors::text_faint();
        let mut ticks = Vec::new();
        for hz in FREQ_TICKS {
            if hz < min_hz || hz > max_hz * 1.01 {
                continue;
            }
            let x = ox + hz_x(hz, plot_w, min_hz, max_hz);
            let line = shape_label(
                window,
                &format_hz(hz),
                tick_color,
                typography::DENSE_CAPTION,
            );
            let w = f32::from(line.width());
            ticks.push(TickGlyph {
                line,
                x: x - w * 0.5,
                y: oy + plot_h + 2.0,
            });
        }
        for db in DB_TICKS {
            if db < floor - 0.5 || db > ceil + 0.5 {
                continue;
            }
            let y = oy + db_to_y(db, plot_h, floor, ceil);
            let label = if db > 0.0 {
                format!("+{db:.0}")
            } else {
                format!("{db:.0}")
            };
            let line = shape_label(window, &label, tick_color, typography::DENSE_CAPTION);
            ticks.push(TickGlyph {
                line,
                x: 2.0,
                y: y - 5.0,
            });
        }

        let hover = self.hover_readout(bounds, ox, plot_w, min_hz, max_hz, window);
        PlotLabels { ticks, hover }
    }

    fn hover_readout(
        &self,
        bounds: Bounds<Pixels>,
        ox: f32,
        plot_w: f32,
        min_hz: f32,
        max_hz: f32,
        window: &mut Window,
    ) -> Option<(ShapedLine, f32, f32)> {
        let mouse = self.draw.hover?;
        if !bounds.contains(&mouse) {
            return None;
        }
        let lx = f32::from(mouse.x - bounds.origin.x) - ox;
        if lx < 0.0 || lx > plot_w {
            return None;
        }
        let t = (lx / plot_w).clamp(0.0, 1.0);
        let hz = log_hz(t, min_hz, max_hz);
        let scale = window.scale_factor().max(1.0);
        let n = display_point_count(plot_w, scale);
        let mut parts = vec![format_hz_full(hz)];
        for layer in &self.layers {
            if layer.db.is_empty() {
                continue;
            }
            let curve = self.display_curve(&layer.db, n, min_hz, max_hz);
            let idx = ((t * (n - 1) as f32).round() as usize).min(n.saturating_sub(1));
            let db = curve.get(idx).copied().unwrap_or(DB_FLOOR);
            parts.push(format!("{} {:+.1} dB", layer.label, db));
        }
        if let Some(th) = self.threshold_db {
            parts.push(format!("Thr {:+.1} dB", th));
        }
        let text = parts.join("   ");
        let line = shape_label(
            window,
            &text,
            Colors::text_secondary(),
            typography::DENSE_LABEL,
        );
        Some((line, ox + lx, 0.0))
    }

    fn display_curve(&self, db: &[f32], n: usize, min_hz: f32, max_hz: f32) -> Vec<f32> {
        let resampled = resample_spectrum_log(db, self.sample_rate, n, min_hz, max_hz);
        peak_preserving_smooth(&resampled, self.draw.smoothing)
    }

    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        labels: PlotLabels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let width = f32::from(bounds.size.width).max(0.0);
        let height = f32::from(bounds.size.height).max(0.0);
        if width < 8.0 || height < 8.0 {
            return;
        }
        window.paint_quad(fill(bounds, Colors::surface_canvas()));
        let (ox, oy, plot_w, plot_h) = plot_rect(bounds);
        let (min_hz, max_hz, floor, ceil) = self.axis();
        let origin = bounds.origin;
        let scale = window.scale_factor().max(1.0);
        let n = display_point_count(plot_w, scale);

        self.paint_grid(
            window, origin, ox, oy, plot_w, plot_h, min_hz, max_hz, floor, ceil,
        );

        if let Some((lo, hi)) = self.band {
            let x0 = ox + hz_x(lo, plot_w, min_hz, max_hz);
            let x1 = ox + hz_x(hi, plot_w, min_hz, max_hz);
            window.paint_quad(fill(
                Bounds::new(
                    origin + point(px(x0), px(oy)),
                    size(px((x1 - x0).max(2.0)), px(plot_h)),
                ),
                Colors::with_alpha(Colors::accent_primary(), 0.10),
            ));
        }

        for hz in &self.harmonic_hz {
            if *hz <= min_hz || *hz >= max_hz {
                continue;
            }
            let half = (*hz * 0.018).max(0.8);
            let x0 = ox + hz_x((*hz - half).max(min_hz), plot_w, min_hz, max_hz);
            let x1 = ox + hz_x((*hz + half).min(max_hz), plot_w, min_hz, max_hz);
            window.paint_quad(fill(
                Bounds::new(
                    origin + point(px(x0), px(oy)),
                    size(px((x1 - x0).max(1.5)), px(plot_h)),
                ),
                Colors::with_alpha(Colors::status_warning(), 0.12),
            ));
            let x = ox + hz_x(*hz, plot_w, min_hz, max_hz);
            window.paint_quad(fill(
                Bounds::new(origin + point(px(x), px(oy)), size(px(1.0), px(plot_h))),
                Colors::with_alpha(Colors::status_warning(), 0.48),
            ));
        }

        let overlay = self.draw.style.overlay();
        for (index, layer) in self.layers.iter().enumerate() {
            if layer.db.is_empty() {
                continue;
            }
            let curve = self.display_curve(&layer.db, n, min_hz, max_hz);
            let fill_layer = layer.fill && self.draw.style.fill() && (index == 0 || overlay);
            let line_layer = layer.line && (self.draw.style.line() || overlay || index > 0);
            let fill_alpha = if fill_layer {
                if overlay && index > 0 {
                    layer.fill_alpha.max(0.08) * 0.7
                } else {
                    layer.fill_alpha
                }
            } else {
                0.0
            };
            paint_contour(
                window,
                origin,
                ox,
                oy,
                plot_w,
                plot_h,
                floor,
                ceil,
                &curve,
                layer.color,
                fill_alpha,
                line_layer,
            );
        }

        if let Some(th) = self.threshold_db {
            let y = oy + db_to_y(th, plot_h, floor, ceil);
            paint_dashed_h(window, origin, ox, y, plot_w, Colors::status_warning());
        }

        if let Some((line, hx, _)) = &labels.hover {
            let x = *hx;
            window.paint_quad(fill(
                Bounds::new(origin + point(px(x), px(oy)), size(px(1.0), px(plot_h))),
                Colors::with_alpha(Colors::text_primary(), 0.28),
            ));
            let text_w = f32::from(line.width());
            let tx = (x + 6.0).min(ox + plot_w - text_w - 4.0).max(ox);
            let ty = oy + 2.0;
            window.paint_quad(fill(
                Bounds::new(
                    origin + point(px(tx - 3.0), px(ty)),
                    size(px(text_w + 6.0), px(14.0)),
                ),
                Colors::with_alpha(Colors::surface_canvas(), 0.82),
            ));
            let _ = line.paint(
                origin + point(px(tx), px(ty)),
                px(12.0),
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }

        for tick in &labels.ticks {
            let _ = tick.line.paint(
                origin + point(px(tick.x), px(tick.y)),
                px(11.0),
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }

    fn paint_grid(
        &self,
        window: &mut Window,
        origin: Point<Pixels>,
        ox: f32,
        oy: f32,
        plot_w: f32,
        plot_h: f32,
        min_hz: f32,
        max_hz: f32,
        floor: f32,
        ceil: f32,
    ) {
        let major = Colors::with_alpha(Colors::text_faint(), 0.16);
        let minor = Colors::with_alpha(Colors::text_faint(), 0.08);
        for db in DB_TICKS {
            if db < floor - 0.5 || db > ceil + 0.5 {
                continue;
            }
            let y = oy + db_to_y(db, plot_h, floor, ceil);
            let color = if db == 0.0 || db == -24.0 || db == -48.0 {
                major
            } else {
                minor
            };
            window.paint_quad(fill(
                Bounds::new(origin + point(px(ox), px(y)), size(px(plot_w), px(1.0))),
                color,
            ));
        }
        for hz in FREQ_TICKS {
            if hz < min_hz || hz > max_hz * 1.01 {
                continue;
            }
            let x = ox + hz_x(hz, plot_w, min_hz, max_hz);
            window.paint_quad(fill(
                Bounds::new(origin + point(px(x), px(oy)), size(px(1.0), px(plot_h))),
                minor,
            ));
        }
    }
}

fn format_hz_full(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{:.2} kHz", hz / 1000.0)
    } else {
        format!("{:.0} Hz", hz)
    }
}

pub fn plot_canvas(plot: AnalyzerPlot) -> impl IntoElement {
    canvas(
        {
            let plot = plot.clone();
            move |bounds, window, _cx| plot.prepare_labels(bounds, window)
        },
        move |bounds, labels, window, cx| plot.paint(bounds, labels, window, cx),
    )
    .size_full()
}

fn paint_contour(
    window: &mut Window,
    origin: Point<Pixels>,
    ox: f32,
    oy: f32,
    plot_w: f32,
    plot_h: f32,
    floor: f32,
    ceil: f32,
    curve: &[f32],
    color: gpui::Rgba,
    fill_alpha: f32,
    stroke: bool,
) {
    if curve.len() < 2 {
        return;
    }
    let n = curve.len();
    let denom = (n - 1) as f32;
    let pts: Vec<(f32, f32)> = curve
        .iter()
        .enumerate()
        .map(|(i, db)| {
            let x = ox + (i as f32 / denom) * plot_w;
            let y = oy + db_to_y(*db, plot_h, floor, ceil);
            (x, y)
        })
        .collect();
    let floor_y = oy + plot_h;

    if fill_alpha > 0.01 {
        let mut body = PathBuilder::fill();
        body.move_to(origin + point(px(pts[0].0), px(floor_y)));
        body.line_to(origin + point(px(pts[0].0), px(pts[0].1)));
        for &(x, y) in &pts[1..] {
            body.line_to(origin + point(px(x), px(y)));
        }
        let last = pts[n - 1];
        body.line_to(origin + point(px(last.0), px(floor_y)));
        body.close();
        if let Ok(path) = body.build() {
            window.paint_path(path, Colors::with_alpha(color, fill_alpha));
        }
    }

    if stroke {
        let options = StrokeOptions::default()
            .with_line_width(STROKE_W)
            .with_miter_limit(2.0);
        let mut line = PathBuilder::stroke(px(STROKE_W)).with_style(PathStyle::Stroke(options));
        line.move_to(origin + point(px(pts[0].0), px(pts[0].1)));
        for &(x, y) in &pts[1..] {
            line.line_to(origin + point(px(x), px(y)));
        }
        if let Ok(path) = line.build() {
            window.paint_path(path, Colors::with_alpha(color, 0.92));
        }
    }
}

fn paint_dashed_h(
    window: &mut Window,
    origin: Point<Pixels>,
    x: f32,
    y: f32,
    width: f32,
    color: gpui::Rgba,
) {
    let mut cursor = x;
    let end = x + width;
    let on: f32 = 5.0;
    let off: f32 = 4.0;
    while cursor < end {
        let w = on.min(end - cursor);
        window.paint_quad(fill(
            Bounds::new(origin + point(px(cursor), px(y)), size(px(w), px(1.0))),
            Colors::with_alpha(color, 0.7),
        ));
        cursor += on + off;
    }
}

pub fn paint_series_contour(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    values: &[f32],
    color: gpui::Rgba,
    fill_alpha: f32,
) {
    let width = f32::from(bounds.size.width).max(0.0);
    let height = f32::from(bounds.size.height).max(0.0);
    if width < 4.0 || height < 4.0 || values.len() < 2 {
        return;
    }
    let scale = window.scale_factor().max(1.0);
    let n = display_point_count(width, scale);
    let samples = resample_series(values, n);
    let origin = bounds.origin;
    let denom = (samples.len() - 1) as f32;
    let peak = samples.iter().copied().fold(1.0e-6_f32, f32::max);
    let pts: Vec<(f32, f32)> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = (i as f32 / denom) * width;
            let y = height - (*v / peak).clamp(0.0, 1.0) * height * 0.92;
            (x, y)
        })
        .collect();
    if fill_alpha > 0.01 {
        let mut body = PathBuilder::fill();
        body.move_to(origin + point(px(0.0), px(height)));
        body.line_to(origin + point(px(pts[0].0), px(pts[0].1)));
        for &(x, y) in &pts[1..] {
            body.line_to(origin + point(px(x), px(y)));
        }
        body.line_to(origin + point(px(pts[pts.len() - 1].0), px(height)));
        body.close();
        if let Ok(path) = body.build() {
            window.paint_path(path, Colors::with_alpha(color, fill_alpha));
        }
    }
    let options = StrokeOptions::default()
        .with_line_width(STROKE_W)
        .with_miter_limit(2.0);
    let mut line = PathBuilder::stroke(px(STROKE_W)).with_style(PathStyle::Stroke(options));
    line.move_to(origin + point(px(pts[0].0), px(pts[0].1)));
    for &(x, y) in &pts[1..] {
        line.line_to(origin + point(px(x), px(y)));
    }
    if let Ok(path) = line.build() {
        window.paint_path(path, Colors::with_alpha(color, 0.9));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        display_point_count, hz_to_t, log_hz, peak_preserving_smooth, resample_spectrum_log,
        DisplaySmoothing, MIN_HZ,
    };

    #[test]
    fn log_mapping_round_trips() {
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let hz = log_hz(t, MIN_HZ, 20_000.0);
            let back = hz_to_t(hz, MIN_HZ, 20_000.0);
            assert!((back - t).abs() < 1.0e-4, "{t} -> {hz} -> {back}");
        }
    }

    #[test]
    fn display_count_tracks_width_not_bin_count() {
        let narrow = display_point_count(320.0, 1.0);
        let wide = display_point_count(1200.0, 2.0);
        assert!(narrow >= 256);
        assert!(wide > narrow);
        assert!(wide <= 2048);
    }

    #[test]
    fn resample_does_not_follow_bin_count() {
        let bins: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let a = resample_spectrum_log(&bins, 48_000, 400, 20.0, 20_000.0);
        let b = resample_spectrum_log(&bins, 48_000, 800, 20.0, 20_000.0);
        assert_eq!(a.len(), 400);
        assert_eq!(b.len(), 800);
    }

    #[test]
    fn peak_preserving_keeps_a_spike() {
        let mut values = vec![-60.0f32; 64];
        values[20] = -6.0;
        let out = peak_preserving_smooth(&values, DisplaySmoothing::Medium);
        assert!(out[20] > -8.0, "peak flattened to {}", out[20]);
        assert!(out[10] < -40.0);
    }
}
