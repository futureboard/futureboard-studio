//! The samplers' shared state and helpers: Quick Sampler's panel state and
//! callbacks, the sample display's frame and waveform painting, the filter
//! and envelope curves, and the value formats. Quick Sampler and Slicer are
//! drawn by [`crate::components::quick_sampler_plate`] and
//! [`crate::components::slicer_plate`]; Drum Sampler reuses the display
//! painting.

use std::sync::Arc;

use gpui::{
    div, fill, linear_color_stop, linear_gradient, point, px, size, AnyElement, App, Bounds,
    IntoElement, ParentElement, PathBuilder, Pixels, Rgba, Styled, Window,
};

use crate::theme::{space, typography, Colors};
use quicksampler::{
    FilterMode as QuickSamplerFilter, LoopMode as QuickSamplerLoop, QuickSamplerParams,
};

pub const QUICK_SAMPLER_TITLE: &str = "Quick Sampler";

/// A loaded sample, as the panel describes and draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleSummary {
    pub file_name: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub seconds: f64,
    /// `(min, max)` per column bucket, for the waveform.
    pub peaks: Arc<Vec<(f32, f32)>>,
}

/// The four markers on the waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleMarker {
    Start,
    End,
    LoopStart,
    LoopEnd,
}

impl SampleMarker {
    /// `params` with this marker moved to `fraction` of the sample, kept on
    /// its side of its partner.
    pub fn moved(self, params: QuickSamplerParams, fraction: f32) -> QuickSamplerParams {
        let f = fraction.clamp(0.0, 1.0);
        let mut next = params;
        match self {
            SampleMarker::Start => next.start = f.min(params.end),
            SampleMarker::End => next.end = f.max(params.start),
            SampleMarker::LoopStart => next.loop_start = f.min(params.loop_end),
            SampleMarker::LoopEnd => next.loop_end = f.max(params.loop_start),
        }
        next
    }

    /// The marker nearest `fraction` among those drawn for `params`.
    pub fn nearest(params: &QuickSamplerParams, fraction: f32) -> SampleMarker {
        let mut candidates = vec![
            (SampleMarker::Start, params.start),
            (SampleMarker::End, params.end),
        ];
        if params.loop_mode != QuickSamplerLoop::Off {
            candidates.push((SampleMarker::LoopStart, params.loop_start));
            candidates.push((SampleMarker::LoopEnd, params.loop_end));
        }
        candidates
            .into_iter()
            .min_by(|a, b| (a.1 - fraction).abs().total_cmp(&(b.1 - fraction).abs()))
            .map(|(marker, _)| marker)
            .unwrap_or(SampleMarker::Start)
    }
}

/// What the panel draws: the track's sampler, the loaded sample's summary, and
/// the window's transient state.
#[derive(Clone)]
pub struct QuickSamplerPanelState {
    /// The insert's params — what its DSP in the plug-in host plays with.
    pub params: QuickSamplerParams,
    pub sample: Option<SampleSummary>,
    pub loading: bool,
    pub status: Option<String>,
    pub keyboard_root: u8,
    pub active_notes: Vec<u8>,
    pub dragging: Option<SampleMarker>,
}

impl Default for QuickSamplerPanelState {
    fn default() -> Self {
        Self {
            params: QuickSamplerParams::default(),
            sample: None,
            loading: false,
            status: None,
            keyboard_root: KEYBOARD_DEFAULT_ROOT,
            active_notes: Vec::new(),
            dragging: None,
        }
    }
}

impl QuickSamplerPanelState {
    pub fn is_playable(&self) -> bool {
        self.sample.is_some() && !self.loading
    }

    pub fn shift_keyboard_octave(&mut self, delta: i32) {
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root = root.clamp(0, KEYBOARD_HIGHEST_ROOT as i32) as u8;
    }
}

pub(crate) type VoidCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;
pub(crate) type ParamsCb = Arc<dyn Fn(&QuickSamplerParams, &mut Window, &mut App) + 'static>;
pub(crate) type NoteCb = Arc<dyn Fn(&u8, &mut Window, &mut App) + 'static>;
pub(crate) type I32Cb = Arc<dyn Fn(&i32, &mut Window, &mut App) + 'static>;
pub(crate) type F32Cb = Arc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;
pub(crate) type PointCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct QuickSamplerCallbacks {
    pub on_browse: VoidCb,
    /// One complete params value, so a knob drag cannot land a partial edit.
    pub on_set_params: ParamsCb,
    /// A press on the waveform, at this window-space x. The window picks the
    /// nearest marker and drags it until the button comes up.
    pub on_waveform_press: F32Cb,
    /// A right-click on the waveform, at this window-space point: the
    /// window opens its menu there.
    pub on_waveform_menu: PointCb,
    pub on_note_on: NoteCb,
    pub on_note_off: NoteCb,
    pub on_all_notes_off: VoidCb,
    pub on_shift_octave: I32Cb,
}

/// Four octaves from C2: a bass line and a lead both fit under the hands.
const KEYBOARD_DEFAULT_ROOT: u8 = 36;
const KEYBOARD_OCTAVES: usize = 4;
const KEYBOARD_WHITE_KEYS: usize = KEYBOARD_OCTAVES * 7;
pub(crate) const KEYBOARD_HIGHEST_ROOT: u8 = 127 - (KEYBOARD_OCTAVES as u8 * 12);
/// Strip along the bottom of the display holding the time ruler.
pub(crate) const RULER_H: f32 = 18.0;
/// Flags on the markers: the grab handle, and where their letters sit.
pub(crate) const FLAG_W: f32 = 14.0;
pub(crate) const FLAG_H: f32 = 14.0;
pub(crate) const KNOB_SIZE: f32 = 34.0;
const KNOB_CELL_W: f32 = 58.0;
pub(crate) const ENVELOPE_KNOB_MAX_MS: f32 = 4_000.0;

/// The group a sampler panel's root carries, so its drop overlay shows while
/// a file is dragged anywhere over the panel.
pub(crate) const SAMPLE_DROP_GROUP: &str = "sampler-sample-drop";

pub(crate) fn format_seconds(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{:.0} ms", seconds * 1_000.0)
    } else if seconds < 60.0 {
        format!("{seconds:.2} s")
    } else {
        format!("{}:{:05.2}", (seconds / 60.0).floor(), seconds % 60.0)
    }
}

// ── Sample display ─────────────────────────────────────────────────────────

/// Colours the display paints with, resolved once per render (theme lookups
/// stay out of the paint closure).
#[derive(Clone, Copy)]
pub(crate) struct DisplayColors {
    pub(crate) background: Rgba,
    pub(crate) grid: Rgba,
    pub(crate) ruler: Rgba,
    pub(crate) wave_edge: Rgba,
    pub(crate) wave_peak: Rgba,
    pub(crate) wave_core: Rgba,
    pub(crate) skipped: Rgba,
    pub(crate) loop_band: Rgba,
    pub(crate) marker: Rgba,
    pub(crate) loop_marker: Rgba,
}

impl DisplayColors {
    pub(crate) fn resolve() -> Self {
        let accent = Colors::accent_primary();
        Self {
            background: Colors::surface_canvas(),
            grid: Colors::with_alpha(Colors::text_primary(), 0.05),
            ruler: Colors::surface_panel_alt(),
            wave_edge: accent,
            wave_peak: Colors::with_alpha(accent, 0.62),
            wave_core: Colors::with_alpha(accent, 0.16),
            skipped: Colors::with_alpha(Colors::surface_canvas(), 0.72),
            loop_band: Colors::with_alpha(accent, 0.09),
            marker: Colors::text_primary(),
            loop_marker: accent,
        }
    }
}

/// A tick every "nice" step that leaves room for its label.
fn ruler_step(seconds: f64) -> f64 {
    const STEPS: [f64; 16] = [
        0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0,
    ];
    let target = seconds / 8.0;
    STEPS
        .into_iter()
        .find(|step| *step >= target)
        .unwrap_or(300.0)
}

/// `(fraction of the sample, label)` for each ruler tick after zero.
pub(crate) fn ruler_labels(seconds: f64) -> Vec<(f32, String)> {
    if seconds <= 0.0 {
        return Vec::new();
    }
    let step = ruler_step(seconds);
    let mut out = Vec::new();
    let mut t = step;
    while t < seconds * 0.97 && out.len() < 24 {
        out.push(((t / seconds) as f32, format_seconds(t)));
        t += step;
    }
    out
}

pub(crate) fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(y)), size(px(w.max(0.0)), px(h.max(0.0))))
}

pub(crate) fn vertical(top: Rgba, bottom: Rgba) -> gpui::Background {
    linear_gradient(
        180.0,
        linear_color_stop(top, 0.0),
        linear_color_stop(bottom, 1.0),
    )
}

/// Where a sample display's waveform sits, in window pixels.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DisplayFrame {
    pub(crate) x0: f32,
    pub(crate) y0: f32,
    pub(crate) w: f32,
    /// Height above the ruler.
    pub(crate) wave_h: f32,
    pub(crate) mid: f32,
    /// Pixels from the centre line to full scale.
    pub(crate) half: f32,
}

impl DisplayFrame {
    /// The x of `fraction` of the sample.
    pub(crate) fn at(&self, fraction: f32) -> f32 {
        self.x0 + fraction.clamp(0.0, 1.0) * self.w
    }
}

/// A sample display's background, ruler strip and grid: centre line, ±6 dB
/// guides, one line per ruler tick.
pub(crate) fn paint_display_frame(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    seconds: f64,
    c: &DisplayColors,
) -> DisplayFrame {
    window.paint_quad(fill(bounds, c.background));
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width).max(1.0);
    let h = f32::from(bounds.size.height).max(1.0);
    let wave_h = h - RULER_H;
    let mid = y0 + wave_h * 0.5;
    let frame = DisplayFrame {
        x0,
        y0,
        w,
        wave_h,
        mid,
        half: wave_h * 0.5 - FLAG_H * 0.5 - 4.0,
    };

    window.paint_quad(fill(rect(x0, y0 + wave_h, w, RULER_H), c.ruler));
    window.paint_quad(fill(rect(x0, mid, w, 1.0), c.grid));
    for level in [0.5_f32, -0.5] {
        window.paint_quad(fill(rect(x0, mid - level * frame.half, w, 1.0), c.grid));
    }
    if seconds > 0.0 {
        let step = ruler_step(seconds);
        let mut t = step;
        while t < seconds && t / step < 64.0 {
            let x = frame.at((t / seconds) as f32);
            window.paint_quad(fill(rect(x, y0, 1.0, wave_h), c.grid));
            window.paint_quad(fill(rect(x, y0 + wave_h, 1.0, 5.0), c.marker));
            t += step;
        }
    }
    frame
}

/// The waveform, scaled to the sample's own peak. `false` when there is
/// nothing to draw.
pub(crate) fn paint_waveform(
    window: &mut Window,
    frame: &DisplayFrame,
    peaks: Option<&Vec<(f32, f32)>>,
    c: &DisplayColors,
) -> bool {
    let DisplayFrame {
        x0, w, mid, half, ..
    } = *frame;
    let Some(peaks) = peaks.filter(|peaks| !peaks.is_empty()) else {
        return false;
    };
    // Scaled to the sample's own peak: a quiet one-shot fills the display
    // like a loud one, the way a sampler shows its zone.
    let loudest = peaks.iter().fold(0.0_f32, |peak, (low, high)| {
        peak.max(low.abs()).max(high.abs())
    });
    let scale = if loudest > 1.0e-5 {
        (1.0 / loudest).min(64.0)
    } else {
        1.0
    };
    let columns = (w.floor() as usize).max(2);
    let mut tops = Vec::with_capacity(columns);
    let mut bottoms = Vec::with_capacity(columns);
    for column in 0..columns {
        let fraction = column as f32 / (columns - 1) as f32;
        let bucket = ((fraction * peaks.len() as f32) as usize).min(peaks.len() - 1);
        let (low, high) = peaks[bucket];
        let x = x0 + column as f32 * w / (columns - 1) as f32;
        tops.push((x, mid - (high * scale).clamp(-1.0, 1.0) * half));
        bottoms.push((x, mid - (low * scale).clamp(-1.0, 1.0) * half));
    }

    // Two halves, each a gradient from the peak to the centre line: bright at
    // the edge of the waveform, quiet at its core.
    let half_path = |edge: &[(f32, f32)]| {
        let mut path = PathBuilder::fill();
        path.move_to(point(px(edge[0].0), px(mid)));
        for &(x, y) in edge {
            path.line_to(point(px(x), px(y)));
        }
        path.line_to(point(px(edge[edge.len() - 1].0), px(mid)));
        path.close();
        path.build().ok()
    };
    if let Some(path) = half_path(&tops) {
        window.paint_path(path, vertical(c.wave_peak, c.wave_core));
    }
    if let Some(path) = half_path(&bottoms) {
        window.paint_path(path, vertical(c.wave_core, c.wave_peak));
    }
    for edge in [&tops, &bottoms] {
        let mut path = PathBuilder::stroke(px(1.2));
        path.move_to(point(px(edge[0].0), px(edge[0].1)));
        for &(x, y) in edge.iter().skip(1) {
            path.line_to(point(px(x), px(y)));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, c.wave_edge);
        }
    }
    true
}

// ── Modules ────────────────────────────────────────────────────────────────

/// One knob with its caption and readout.
pub(crate) fn knob_cell(
    caption: &'static str,
    readout: String,
    control: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(KNOB_CELL_W))
        .gap(px(space::HAIR))
        .child(control)
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(readout),
        )
        .into_any_element()
}

pub(crate) fn percent(value: f32) -> String {
    format!("{:.0}%", value * 100.0)
}

pub(crate) fn format_ms(ms: f32) -> String {
    if ms <= 0.0 {
        "0 ms".to_string()
    } else if ms < 1_000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1_000.0)
    }
}

pub(crate) fn with(
    p: QuickSamplerParams,
    apply: impl FnOnce(&mut QuickSamplerParams),
) -> QuickSamplerParams {
    let mut next = p;
    apply(&mut next);
    next.sanitized()
}

// Filter ----------------------------------------------------------------------

/// Cutoff knob position (0..1) ↔ Hz, logarithmic: each tenth of the sweep is
/// the same musical interval.
pub(crate) fn cutoff_from_position(position: f32) -> f32 {
    let (lo, hi) = (quicksampler::CUTOFF_MIN_HZ, quicksampler::CUTOFF_MAX_HZ);
    lo * (hi / lo).powf(position.clamp(0.0, 1.0))
}

pub(crate) fn cutoff_position(hz: f32) -> f32 {
    let (lo, hi) = (quicksampler::CUTOFF_MIN_HZ, quicksampler::CUTOFF_MAX_HZ);
    ((hz.clamp(lo, hi) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

pub(crate) fn format_hz(hz: f32) -> String {
    if hz >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// Magnitude in dB of the sampler's state-variable filter at `hz`: the
/// analog prototype of the same topology, with the same resonance mapping as
/// the DSP (`k = 2 - 1.94 · resonance`).
pub(crate) fn filter_response_db(
    mode: QuickSamplerFilter,
    cutoff: f32,
    resonance: f32,
    hz: f32,
) -> f32 {
    let k = 2.0 - 1.94 * resonance.clamp(0.0, 1.0);
    let x = hz / cutoff.max(1.0);
    let denominator = ((1.0 - x * x).powi(2) + (k * x).powi(2)).sqrt().max(1.0e-9);
    let magnitude = match mode {
        QuickSamplerFilter::Off => 1.0,
        QuickSamplerFilter::LowPass => 1.0 / denominator,
        QuickSamplerFilter::HighPass => x * x / denominator,
        QuickSamplerFilter::BandPass => k * x / denominator,
    };
    20.0 * magnitude.max(1.0e-6).log10()
}

// Amp -------------------------------------------------------------------------

/// The envelope's corner points across `width`: attack, decay and release
/// each take a share of the width that grows with their time, and the
/// sustain holds a fixed stretch so it is always visible.
pub(crate) fn envelope_points(p: &QuickSamplerParams, width: f32, height: f32) -> [(f32, f32); 5] {
    // Square-root time so a 10 ms attack is visible next to a 4 s release.
    let span = |ms: f32| (ms.max(0.0) / ENVELOPE_KNOB_MAX_MS).sqrt().min(1.0);
    let sustain_w = 0.22;
    let (a, d, r) = (span(p.attack_ms), span(p.decay_ms), span(p.release_ms));
    let total = (a + d + r).max(1.0e-6);
    let share = (1.0 - sustain_w) * (a + d + r).min(2.4) / 2.4;
    let unit = share / total;
    let xa = a * unit;
    let xd = xa + d * unit;
    let xs = xd + sustain_w;
    let xr = xs + r * unit;
    let level = |value: f32| height * (1.0 - value.clamp(0.0, 1.0));
    [
        (0.0, height),
        (xa * width, level(1.0)),
        (xd * width, level(p.sustain)),
        (xs * width, level(p.sustain)),
        (xr * width, height),
    ]
}

// Graph painting ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_never_crosses_its_partner() {
        let p = QuickSamplerParams {
            start: 0.2,
            end: 0.6,
            ..QuickSamplerParams::default()
        };
        assert_eq!(SampleMarker::Start.moved(p, 0.9).start, 0.6);
        assert_eq!(SampleMarker::End.moved(p, 0.1).end, 0.2);
        assert_eq!(SampleMarker::End.moved(p, 2.0).end, 1.0);
    }

    #[test]
    fn the_nearest_marker_ignores_a_loop_that_is_off() {
        let p = QuickSamplerParams {
            start: 0.1,
            end: 0.9,
            loop_start: 0.5,
            loop_end: 0.55,
            ..QuickSamplerParams::default()
        };
        assert_eq!(SampleMarker::nearest(&p, 0.45), SampleMarker::Start);
        let looping = QuickSamplerParams {
            loop_mode: QuickSamplerLoop::Forward,
            ..p
        };
        assert_eq!(
            SampleMarker::nearest(&looping, 0.49),
            SampleMarker::LoopStart
        );
        assert_eq!(SampleMarker::nearest(&looping, 0.56), SampleMarker::LoopEnd);
    }

    #[test]
    fn the_cutoff_knob_is_logarithmic_end_to_end() {
        assert!((cutoff_from_position(0.0) - 20.0).abs() < 1.0e-3);
        assert!((cutoff_from_position(1.0) - 20_000.0).abs() < 0.5);
        assert!((cutoff_from_position(cutoff_position(1_000.0)) - 1_000.0).abs() < 0.5);
    }

    #[test]
    fn the_filter_curve_matches_the_filter() {
        let lp = |hz| filter_response_db(QuickSamplerFilter::LowPass, 1_000.0, 0.0, hz);
        // Flat below the cutoff, -6 dB at it (k = 2), falling 12 dB/octave.
        assert!(lp(50.0).abs() < 0.1);
        assert!((lp(1_000.0) + 6.02).abs() < 0.1);
        assert!((lp(8_000.0) - lp(16_000.0) - 12.0).abs() < 0.5);
        let hp = |hz| filter_response_db(QuickSamplerFilter::HighPass, 1_000.0, 0.0, hz);
        assert!(hp(16_000.0).abs() < 0.1 && hp(50.0) < -40.0);
        // Resonance lifts the peak at the cutoff.
        assert!(filter_response_db(QuickSamplerFilter::LowPass, 1_000.0, 0.9, 1_000.0) > 10.0);
    }

    #[test]
    fn the_envelope_graph_keeps_its_order_and_sustain() {
        let p = QuickSamplerParams {
            attack_ms: 10.0,
            decay_ms: 300.0,
            sustain: 0.5,
            release_ms: 4_000.0,
            ..QuickSamplerParams::default()
        };
        let points = envelope_points(&p, 200.0, 100.0);
        assert!(points.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(points[2].1, 50.0);
        assert_eq!(points[3].1, 50.0);
        assert!(points[4].0 <= 200.0 + 1.0e-3);
    }

    #[test]
    fn the_ruler_ticks_at_a_readable_step() {
        assert_eq!(ruler_step(3.69), 0.5);
        assert_eq!(ruler_step(0.2), 0.05);
        let labels = ruler_labels(3.69);
        assert_eq!(labels.len(), 7);
        assert_eq!(labels[0].1, "500 ms");
        assert_eq!(labels[1].1, "1.00 s");
    }
}
