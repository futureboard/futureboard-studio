//! The live half of a native built-in editor: the telemetry it polls from
//! the plug-in host's shared region, the history and ballistics kept between
//! polls, and the painters that draw it.
//!
//! Nothing here touches params or the panel. [`Live::poll`] runs on the
//! editor window's telemetry timer (pure atomic loads behind the host ops'
//! sources); the painters run in the window's overlay, into the bounds the
//! panel recorded, so a reading that moves redraws only the overlay.

use std::collections::VecDeque;
use std::time::Instant;

use gpui::{
    fill, point, px, App, Bounds, Hsla, Pixels, Rgba, SharedString, TextAlign, TextRun, Window,
};
use SpherePluginHost::audio_bridge::{
    BuiltinMeterFrame, StereoImageFrame, BUILTIN_RACK_SLOTS, IMAGE_SCOPE_POINTS, REDUCTION_BANDS,
};
use SpherePluginHost::spectrum::SPECTRUM_BINS;

use crate::components::builtin_plugin_editor_window::{
    BuiltinBandReductionSource, BuiltinEditorHostOps, BuiltinHostStatusSource, BuiltinMeterSource,
    BuiltinSpectrumSource, BuiltinStereoImageSource, PluginInstanceKey,
};
use crate::components::eq_graph::{freq_fraction, paint_area, paint_line};
use crate::components::quick_sampler_panel::rect;
use crate::theme::{typography, Colors};

/// Frames of level history the scrolling display keeps: ten seconds at the
/// telemetry rate.
pub const HISTORY: usize = 300;
/// Frames a meter's held peak covers: about 1.2 s.
const HOLD_FRAMES: usize = 36;
/// The level floor most meters and the history draw down to, in dBFS.
pub const LEVEL_FLOOR_DB: f32 = -48.0;
/// Vectorscope frames kept for persistence.
const SCOPE_FRAMES: usize = 6;
/// Angle bins across the polar level display's half-circle.
pub const POLAR_BINS: usize = 61;
/// How much of a polar level ray is kept from one scope frame to the next.
const POLAR_HOLD: f32 = 0.86;
/// The VU needle's time constant: a 300 ms rise to 99 %.
const NEEDLE_TAU_SEC: f32 = 0.3 / 4.6;
/// 0 VU on the output scale, in dBFS RMS.
pub const VU_REFERENCE_DBFS: f32 = -18.0;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HistoryPoint {
    pub(crate) in_db: f32,
    pub(crate) out_db: f32,
    pub(crate) reduction_db: f32,
    /// Rack position 0's input level. A single-stage built-in may carry its
    /// own reading there: WayGate's key (`waygate::KEY_SLOT`).
    pub(crate) slot_in_db: f32,
    /// Whether rack position 0's output level is lit: WayGate's detector
    /// state.
    pub(crate) slot_lit: bool,
}

fn to_db(level: f32) -> f32 {
    if level <= 1.0e-6 {
        -120.0
    } else {
        20.0 * level.log10()
    }
}

/// An instance's telemetry, as the editor last read it.
pub struct Live {
    key: PluginInstanceKey,
    meter_source: Option<BuiltinMeterSource>,
    spectrum_source: Option<BuiltinSpectrumSource>,
    image_source: Option<BuiltinStereoImageSource>,
    reduction_source: Option<BuiltinBandReductionSource>,
    status_source: Option<BuiltinHostStatusSource>,
    /// The newest meter frame; `None` until one arrives.
    pub frame: Option<BuiltinMeterFrame>,
    history: Box<[HistoryPoint; HISTORY]>,
    write: usize,
    count: usize,
    last_poll: Option<Instant>,
    /// The VU needle on each scale, in its own units: reduction in dB, and
    /// output in VU.
    needle_reduction: f32,
    needle_output: f32,
    spectrum: Option<(u32, [f32; SPECTRUM_BINS])>,
    reduction: Option<(u32, [f32; REDUCTION_BANDS])>,
    image_seq: Option<u32>,
    image: VecDeque<StereoImageFrame>,
    /// The vectorscope's auto-gain.
    scope_gain: f32,
    /// The polar level display's rays: the loudest sample at each angle,
    /// held and falling back.
    polar: [f32; POLAR_BINS],
    sample_rate: Option<f32>,
}

impl Live {
    pub fn new(key: PluginInstanceKey, host_ops: &BuiltinEditorHostOps) -> Self {
        Self {
            key,
            meter_source: host_ops.meter_source.clone(),
            spectrum_source: host_ops.spectrum_source.clone(),
            image_source: host_ops.stereo_image_source.clone(),
            reduction_source: host_ops.band_reduction_source.clone(),
            status_source: host_ops.host_status_source.clone(),
            frame: None,
            history: Box::new([HistoryPoint::default(); HISTORY]),
            write: 0,
            count: 0,
            last_poll: None,
            needle_reduction: 0.0,
            needle_output: -20.0,
            spectrum: None,
            reduction: None,
            image_seq: None,
            image: VecDeque::with_capacity(SCOPE_FRAMES),
            scope_gain: 1.0,
            polar: [0.0; POLAR_BINS],
            sample_rate: None,
        }
    }

    /// A reading for the preview tool and the tests, as if the host had
    /// published it.
    pub fn show(&mut self, frame: BuiltinMeterFrame) {
        self.push(frame, 1.0);
    }

    /// A stereo-image frame for the preview tool and the tests.
    pub fn show_image(&mut self, frame: StereoImageFrame) {
        self.take_image(frame);
    }

    /// A spectrum and band reductions for the preview tool and the tests.
    pub fn show_bands(
        &mut self,
        spectrum: Option<[f32; SPECTRUM_BINS]>,
        reduction: Option<[f32; REDUCTION_BANDS]>,
    ) {
        self.spectrum = spectrum.map(|bins| (0, bins));
        self.reduction = reduction.map(|bands| (0, bands));
    }

    pub fn sample_rate(&self) -> Option<f32> {
        self.sample_rate
    }

    /// Reads every source. True when anything worth a redraw moved, so an
    /// idle editor stays idle.
    pub fn poll(&mut self) -> bool {
        let now = Instant::now();
        let dt = self
            .last_poll
            .map_or(0.033, |at| now.duration_since(at).as_secs_f32())
            .min(0.25);
        self.last_poll = Some(now);
        let mut moved = false;

        let frame = self
            .meter_source
            .as_ref()
            .and_then(|source| source(&self.key));
        if let Some(frame) = frame {
            let before = (self.needle_reduction, self.needle_output, self.frame);
            self.push(frame, dt);
            moved |= before.2 != Some(frame)
                || (before.0 - self.needle_reduction).abs() > 0.01
                || (before.1 - self.needle_output).abs() > 0.01;
        } else if self.frame.take().is_some() {
            moved = true;
        }

        if let Some((seq, bins)) = self.spectrum_source.as_ref().and_then(|s| s(&self.key)) {
            if self.spectrum.is_none_or(|(old, _)| old != seq) {
                self.spectrum = Some((seq, bins));
                moved = true;
            }
        }
        if let Some((seq, bands)) = self.reduction_source.as_ref().and_then(|s| s(&self.key)) {
            if self.reduction.is_none_or(|(old, _)| old != seq) {
                self.reduction = Some((seq, bands));
                moved = true;
            }
        }
        if let Some((seq, image)) = self.image_source.as_ref().and_then(|s| s(&self.key)) {
            if self.image_seq != Some(seq) {
                self.image_seq = Some(seq);
                self.take_image(image);
                moved = true;
            }
        }
        if let Some((rate, ..)) = self.status_source.as_ref().and_then(|s| s(&self.key)) {
            if rate > 0 {
                self.sample_rate = Some(rate as f32);
            }
        }
        moved
    }

    fn push(&mut self, frame: BuiltinMeterFrame, dt: f32) {
        self.history[self.write] = HistoryPoint {
            in_db: to_db(frame.in_peak),
            out_db: to_db(frame.out_peak),
            reduction_db: frame.gain_reduction_db.max(0.0),
            slot_in_db: to_db(frame.slot_in_peak[0]),
            slot_lit: frame.slot_out_peak[0] >= 0.5,
        };
        self.write = (self.write + 1) % HISTORY;
        self.count = (self.count + 1).min(HISTORY);
        let follow = 1.0 - (-dt / NEEDLE_TAU_SEC).exp();
        let output = to_db(frame.out_rms) - VU_REFERENCE_DBFS;
        self.needle_reduction +=
            (frame.gain_reduction_db.max(0.0) - self.needle_reduction) * follow;
        self.needle_output += (output.max(-30.0) - self.needle_output) * follow;
        self.frame = Some(frame);
    }

    fn take_image(&mut self, frame: StereoImageFrame) {
        if self.image.len() == SCOPE_FRAMES {
            self.image.pop_front();
        }
        let peak = frame
            .scope
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let target = if peak > 1.0e-4 {
            (0.82 / peak).min(16.0)
        } else {
            self.scope_gain
        };
        // Quick to back off a loud passage, slow to grow into a quiet one.
        let rate = if target < self.scope_gain { 0.5 } else { 0.04 };
        self.scope_gain = (self.scope_gain + (target - self.scope_gain) * rate).clamp(1.0, 16.0);
        let mut fresh = [0.0f32; POLAR_BINS];
        for pair in frame.scope.chunks_exact(2) {
            let (angle, radius) = polar_of(pair[0], pair[1]);
            let bin = ((angle / std::f32::consts::PI + 0.5) * (POLAR_BINS - 1) as f32).round();
            let bin = (bin.max(0.0) as usize).min(POLAR_BINS - 1);
            fresh[bin] = fresh[bin].max(radius);
        }
        for (held, now) in self.polar.iter_mut().zip(fresh) {
            *held = now.max(*held * POLAR_HOLD);
        }
        self.image.push_back(frame);
    }

    /// The point `age` frames before the newest.
    fn point(&self, age: usize) -> HistoryPoint {
        self.history[(self.write + HISTORY - 1 - age) % HISTORY]
    }

    /// How many history points there are, up to [`HISTORY`].
    pub(crate) fn history_len(&self) -> usize {
        self.count
    }

    /// The history point `age` frames before the newest, for a family's own
    /// history painter.
    pub(crate) fn history_at(&self, age: usize) -> HistoryPoint {
        self.point(age)
    }

    /// The highest of a reading over the hold window.
    pub(crate) fn held(&self, read: impl Fn(&HistoryPoint) -> f32) -> f32 {
        (0..self.count.min(HOLD_FRAMES))
            .map(|age| read(&self.point(age)))
            .fold(f32::NEG_INFINITY, f32::max)
    }

    pub fn spectrum_bins(&self) -> Option<&[f32; SPECTRUM_BINS]> {
        self.spectrum.as_ref().map(|(_, bins)| bins)
    }

    pub fn band_reduction(&self) -> Option<[f32; REDUCTION_BANDS]> {
        self.reduction.map(|(_, bands)| bands)
    }

    pub fn image(&self) -> Option<&StereoImageFrame> {
        self.image.back()
    }

    pub fn slot_peaks(&self) -> Option<([f32; BUILTIN_RACK_SLOTS], [f32; BUILTIN_RACK_SLOTS])> {
        self.frame
            .map(|frame| (frame.slot_in_peak, frame.slot_out_peak))
    }
}

// ── Painting helpers ────────────────────────────────────────────────────────

pub(crate) fn frame_of(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    (
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// One line of text at `(x, y)` (its top), aligned on `x`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn label(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    size: f32,
    color: Rgba,
    x: f32,
    y: f32,
    align: Align,
) {
    if text.is_empty() {
        return;
    }
    let font = window.text_style().font();
    let color: Hsla = color.into();
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
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
    );
    let width = f32::from(line.width);
    let left = match align {
        Align::Left => x,
        Align::Center => x - width * 0.5,
        Align::Right => x - width,
    };
    let _ = line.paint(
        point(px(left), px(y)),
        px(size * 1.3),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// A line of short dashes from `(x0, y0)` to `(x1, y1)`.
pub(crate) fn dashed(
    window: &mut Window,
    (x0, y0): (f32, f32),
    (x1, y1): (f32, f32),
    width: f32,
    color: Rgba,
) {
    let length = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
    if length < 1.0 {
        return;
    }
    let steps = (length / 8.0).ceil() as usize;
    for i in 0..steps {
        let a = i as f32 / steps as f32;
        let b = (a + 0.5 / steps as f32).min(1.0);
        paint_line(
            window,
            &[
                (x0 + (x1 - x0) * a, y0 + (y1 - y0) * a),
                (x0 + (x1 - x0) * b, y0 + (y1 - y0) * b),
            ],
            width,
            color,
        );
    }
}

/// A filled dot of radius `r` at `(x, y)`.
pub(crate) fn dot(window: &mut Window, x: f32, y: f32, r: f32, color: Rgba) {
    window.paint_quad(fill(rect(x - r, y - r, 2.0 * r, 2.0 * r), color).corner_radii(px(r)));
}

fn db_text(db: f32) -> String {
    if db <= -119.0 {
        "−∞".to_string()
    } else if db.abs() < 0.05 {
        "0.0".to_string()
    } else {
        format!("{db:.1}")
    }
}

// ── The history ─────────────────────────────────────────────────────────────

/// A level line across the history, with its tag.
#[derive(Debug, Clone)]
pub struct Marker {
    pub db: f32,
    pub label: String,
    pub color: Rgba,
}

/// The last ten seconds, newest at the right: input peaks as a shaded area,
/// the reduction hanging from the top, the output peak as a line, and
/// `markers` across it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_history(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    live: &Live,
    markers: &[Marker],
    reduction_label: &str,
    bypassed: bool,
) {
    const GUTTER: f32 = 34.0;
    const PAD_Y: f32 = 10.0;
    let (x0, y0, w, h) = frame_of(bounds);
    let plot_w = (w - GUTTER).max(1.0);
    let plot_h = (h - 2.0 * PAD_Y).max(1.0);
    let ink = Colors::text_primary();
    let y_at = |db: f32| y0 + PAD_Y + (db / LEVEL_FLOOR_DB).clamp(0.0, 1.0) * plot_h;
    let x_at = |age: usize| x0 + plot_w - age as f32 * plot_w / (HISTORY - 1) as f32;

    for db in [0.0, -6.0, -12.0, -18.0, -24.0, -36.0] {
        let y = y_at(db);
        window.paint_quad(fill(
            rect(x0, y, plot_w, 1.0),
            Colors::with_alpha(ink, 0.06),
        ));
        label(
            window,
            cx,
            &format!("{db:.0}"),
            typography::DENSE_CAPTION,
            Colors::text_faint(),
            x0 + plot_w + 6.0,
            y - 6.0,
            Align::Left,
        );
    }

    let alpha = if bypassed { 0.45 } else { 1.0 };
    if live.count > 1 {
        let ages = 0..live.count;
        let input: Vec<(f32, f32)> = ages
            .clone()
            .map(|age| (x_at(age), y_at(live.point(age).in_db)))
            .collect();
        paint_area(
            window,
            &input,
            y0 + PAD_Y + plot_h,
            Colors::with_alpha(Colors::text_muted(), 0.22 * alpha),
        );
        paint_line(
            window,
            &input,
            1.0,
            Colors::with_alpha(Colors::text_muted(), 0.7 * alpha),
        );
        let reduction: Vec<(f32, f32)> = ages
            .clone()
            .map(|age| (x_at(age), y_at(-live.point(age).reduction_db)))
            .collect();
        if reduction.iter().any(|(_, y)| *y > y_at(0.0) + 0.5) {
            paint_area(
                window,
                &reduction,
                y_at(0.0),
                Colors::with_alpha(Colors::accent_primary(), 0.30 * alpha),
            );
            paint_line(
                window,
                &reduction,
                1.25,
                Colors::with_alpha(Colors::accent_primary(), alpha),
            );
        }
        let output: Vec<(f32, f32)> = ages
            .map(|age| (x_at(age), y_at(live.point(age).out_db)))
            .collect();
        paint_line(window, &output, 1.25, Colors::with_alpha(ink, 0.9 * alpha));
    }

    for marker in markers {
        let y = y_at(marker.db);
        // A tag near the top goes under its line, clear of the display's
        // own tags.
        let tag_y = if y - 14.0 < y0 + 4.0 {
            y + 3.0
        } else {
            y - 14.0
        };
        dashed(
            window,
            (x0, y),
            (x0 + plot_w, y),
            1.0,
            Colors::with_alpha(marker.color, 0.85),
        );
        label(
            window,
            cx,
            &marker.label,
            typography::DENSE_CAPTION,
            marker.color,
            x0 + 8.0,
            tag_y,
            Align::Left,
        );
    }
    label(
        window,
        cx,
        &format!("In · Out · {reduction_label}"),
        typography::DENSE_CAPTION,
        Colors::text_faint(),
        x0 + 8.0,
        y0 + h - 16.0,
        Align::Left,
    );
}

// ── Level meters ────────────────────────────────────────────────────────────

/// One column of the meter bay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MeterColumn {
    Input,
    Output,
    /// The reduction, filling down from the top over 24 dB, its readout
    /// prefixed with `sign`.
    Reduction(&'static str, &'static str),
}

/// Vertical bars side by side, each with its caption, a held-peak tick and
/// the held value.
pub(crate) fn paint_meters(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    live: &Live,
    columns: &[MeterColumn],
    bypassed: bool,
) {
    const CAPTION_H: f32 = 16.0;
    const READOUT_H: f32 = 18.0;
    const BAR_W: f32 = 12.0;
    let (x0, y0, w, h) = frame_of(bounds);
    let column_w = w / columns.len().max(1) as f32;
    let bar_top = y0 + CAPTION_H;
    let bar_h = (h - CAPTION_H - READOUT_H).max(1.0);
    let has = live.frame.is_some() && live.count > 0;
    for (i, column) in columns.iter().enumerate() {
        let centre = x0 + column_w * (i as f32 + 0.5);
        let caption = match column {
            MeterColumn::Input => "In",
            MeterColumn::Output => "Out",
            MeterColumn::Reduction(caption, _) => caption,
        };
        label(
            window,
            cx,
            caption,
            typography::DENSE_CAPTION,
            Colors::text_muted(),
            centre,
            y0,
            Align::Center,
        );
        let bar = rect(centre - BAR_W * 0.5, bar_top, BAR_W, bar_h);
        window.paint_quad(fill(bar, Colors::meter_bg()));
        let (unit, hold, text) = match column {
            MeterColumn::Input | MeterColumn::Output => {
                let read = |p: &HistoryPoint| {
                    if *column == MeterColumn::Input {
                        p.in_db
                    } else {
                        p.out_db
                    }
                };
                let now = if has { read(&live.point(0)) } else { -120.0 };
                let held = if has { live.held(read) } else { -120.0 };
                let unit = |db: f32| ((db - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB).clamp(0.0, 1.0);
                (
                    unit(now),
                    unit(held),
                    if has { db_text(held) } else { "—".into() },
                )
            }
            MeterColumn::Reduction(_, sign) => {
                let read = |p: &HistoryPoint| if bypassed { 0.0 } else { p.reduction_db };
                let now = if has { read(&live.point(0)) } else { 0.0 };
                let held = if has { live.held(read) } else { 0.0 };
                let text = if !has {
                    "—".to_string()
                } else if held >= 0.05 {
                    format!("{sign}{held:.1}")
                } else {
                    "0.0".to_string()
                };
                (
                    (now / 24.0).clamp(0.0, 1.0),
                    (held / 24.0).clamp(0.0, 1.0),
                    text,
                )
            }
        };
        let (x, bar_w) = (centre - BAR_W * 0.5, BAR_W);
        match column {
            MeterColumn::Reduction(..) => {
                window.paint_quad(fill(
                    rect(x, bar_top, bar_w, unit * bar_h),
                    Colors::accent_primary(),
                ));
                if hold > 0.0 {
                    window.paint_quad(fill(
                        rect(x, bar_top + hold * bar_h - 1.0, bar_w, 1.5),
                        Colors::accent_primary_hover(),
                    ));
                }
            }
            _ => {
                let top = bar_top + (1.0 - unit) * bar_h;
                let color = if unit >= 1.0 {
                    Colors::meter_high()
                } else {
                    Colors::text_muted()
                };
                window.paint_quad(fill(rect(x, top, bar_w, bar_top + bar_h - top), color));
                if hold > 0.0 {
                    window.paint_quad(fill(
                        rect(x, bar_top + (1.0 - hold) * bar_h, bar_w, 1.5),
                        Colors::text_primary(),
                    ));
                }
            }
        }
        label(
            window,
            cx,
            &text,
            typography::UI_XS,
            Colors::text_secondary(),
            centre,
            bar_top + bar_h + 3.0,
            Align::Center,
        );
    }
}

// ── The VU meter ────────────────────────────────────────────────────────────

/// A VU face's colours: paper, ink, the red zone, and the needle.
#[derive(Debug, Clone, Copy)]
pub struct VuFace {
    pub top: Rgba,
    pub bottom: Rgba,
    pub ink: Rgba,
    pub hot: Rgba,
    pub needle: Rgba,
}

pub(crate) fn rgb(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

impl VuFace {
    /// Warm cream paper with brown ink: the optical leveller.
    pub fn cream() -> Self {
        Self {
            top: rgb(0xfff7e5),
            bottom: rgb(0xe7d2aa),
            ink: rgb(0x403629),
            hot: rgb(0xa92f1c),
            needle: rgb(0x160f08),
        }
    }

    /// Deep blue with white ink: the FET limiter.
    pub fn blue() -> Self {
        Self {
            top: rgb(0x3a72ad),
            bottom: rgb(0x1a4574),
            ink: rgb(0xf2f6fa),
            hot: rgb(0xd8492f),
            needle: rgb(0xf5f2ec),
        }
    }

    /// A tinted face with dark ink, from `top` to `bottom`.
    pub fn tinted(top: u32, bottom: u32, ink: u32, hot: u32) -> Self {
        Self {
            top: rgb(top),
            bottom: rgb(bottom),
            ink: rgb(ink),
            hot: rgb(hot),
            needle: rgb(ink),
        }
    }
}

/// What the VU reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VuScale {
    /// Gain reduction, 0 dB resting at the right, `full_scale` dB at the left.
    Reduction(f32),
    /// Output level in VU, −20 to +3, 0 VU at [`VU_REFERENCE_DBFS`].
    Output,
}

/// The needle's position, −1 (left stop) to 1 (right stop).
fn vu_position(scale: VuScale, value: f32) -> f32 {
    let unit = match scale {
        VuScale::Reduction(full) => 1.0 - (value / full).clamp(0.0, 1.0).powf(0.6),
        VuScale::Output => ((value + 20.0) / 23.0).clamp(0.0, 1.0),
    };
    unit * 2.0 - 1.0
}

/// A moving-coil meter: its face, the scale for `scale`, and the needle on
/// the live reading — parked at rest while no telemetry arrives.
pub(crate) fn paint_vu(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    live: &Live,
    face: VuFace,
    scale: VuScale,
    title: &str,
) {
    const SWEEP: f32 = 0.70; // radians either side of vertical: 40°
    let (x0, y0, w, h) = frame_of(bounds);
    window.paint_quad(
        fill(
            bounds,
            gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(face.top, 0.0),
                gpui::linear_color_stop(face.bottom, 1.0),
            ),
        )
        .corner_radii(px(6.0)),
    );
    // The arc's crown a fifth of the way down; the pivot below it, off the
    // face when the face is wide and short.
    let radius = (w * 0.60).min(h * 0.95);
    let pivot = (x0 + w * 0.5, y0 + h * 0.20 + radius);
    let at = |position: f32, r: f32| {
        let angle = position * SWEEP;
        (pivot.0 + r * angle.sin(), pivot.1 - r * angle.cos())
    };

    let (ticks, labelled, hot_from): (&[f32], &[f32], Option<f32>) = match scale {
        VuScale::Reduction(full) if full > 20.0 => (
            &[0.0, 1.5, 3.0, 4.5, 6.0, 9.0, 12.0, 18.0, 24.0],
            &[0.0, 3.0, 6.0, 12.0, 24.0],
            None,
        ),
        VuScale::Reduction(_) => (
            &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 15.0, 20.0],
            &[0.0, 2.0, 4.0, 6.0, 10.0, 20.0],
            None,
        ),
        VuScale::Output => (
            &[
                -20.0, -10.0, -7.0, -5.0, -3.0, -2.0, -1.0, 0.0, 1.0, 2.0, 3.0,
            ],
            &[-20.0, -10.0, -7.0, -5.0, -3.0, 0.0, 3.0],
            Some(0.0),
        ),
    };

    // The scale arc, red past 0 VU.
    let arc: Vec<(f32, f32)> = (0..=40)
        .map(|i| at(i as f32 / 20.0 - 1.0, radius))
        .collect();
    paint_line(window, &arc, 1.2, Colors::with_alpha(face.ink, 0.8));
    if let Some(from) = hot_from {
        let start = vu_position(scale, from);
        let hot: Vec<(f32, f32)> = (0..=12)
            .map(|i| at(start + (1.0 - start) * i as f32 / 12.0, radius + 2.0))
            .collect();
        paint_line(window, &hot, 4.0, face.hot);
    }
    for tick in ticks {
        let position = vu_position(scale, *tick);
        let major = labelled.contains(tick);
        let length = if major { 9.0 } else { 5.0 };
        let color = if hot_from.is_some_and(|from| *tick > from) {
            face.hot
        } else {
            face.ink
        };
        paint_line(
            window,
            &[at(position, radius), at(position, radius + length)],
            if major { 1.4 } else { 1.0 },
            color,
        );
        if major {
            let (x, y) = at(position, radius + length + 10.0);
            let text = if matches!(scale, VuScale::Output) && *tick > 0.0 {
                format!("+{tick:.0}")
            } else {
                format!("{tick:.0}")
            };
            label(
                window,
                cx,
                &text,
                typography::DENSE_CAPTION,
                color,
                x,
                y - 7.0,
                Align::Center,
            );
        }
    }
    label(
        window,
        cx,
        title,
        typography::DENSE_CAPTION,
        Colors::with_alpha(face.ink, 0.85),
        x0 + w * 0.5,
        pivot.1 - radius * 0.46,
        Align::Center,
    );

    let live_now = live.frame.is_some();
    let value = match scale {
        VuScale::Reduction(_) => live.needle_reduction,
        VuScale::Output => live.needle_output,
    };
    let position = if live_now {
        vu_position(scale, value)
    } else {
        vu_position(
            scale,
            match scale {
                VuScale::Reduction(_) => 0.0,
                VuScale::Output => -20.0,
            },
        )
    };
    let needle_color = if live_now {
        face.needle
    } else {
        Colors::with_alpha(face.needle, 0.45)
    };
    let tip = at(position, radius + 6.0);
    // From just inside the face's bottom edge when the pivot sits below it.
    let below = pivot.1 - (y0 + h - 4.0);
    let base = at(
        position,
        (radius * 0.16).max(below / (position * SWEEP).cos()),
    );
    paint_line(window, &[base, tip], 1.8, needle_color);
    if !live_now {
        label(
            window,
            cx,
            "no signal",
            typography::DENSE_CAPTION,
            Colors::with_alpha(face.ink, 0.6),
            x0 + w * 0.5,
            pivot.1 - radius * 0.46 + 16.0,
            Align::Center,
        );
    }
    if live.frame.is_some_and(|frame| frame.out_clip) {
        let r = rect(x0 + w - 42.0, y0 + 8.0, 34.0, 15.0);
        window.paint_quad(fill(r, face.hot).corner_radii(px(3.0)));
        label(
            window,
            cx,
            "CLIP",
            typography::DENSE_CAPTION,
            rgb(0xffffff),
            x0 + w - 25.0,
            y0 + 9.0,
            Align::Center,
        );
    }
}

// ── Transfer displays ───────────────────────────────────────────────────────

/// The square plot a transfer curve sits in: `(x0, y0, side)` within
/// `bounds`, leaving room for the axis labels.
pub(crate) fn transfer_plot(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    let (x0, y0, w, h) = frame_of(bounds);
    let (left, right, top, bottom) = (30.0, 10.0, 26.0, 20.0);
    (
        x0 + left,
        y0 + top,
        (w - left - right).max(1.0),
        (h - top - bottom).max(1.0),
    )
}

/// The operating point on a transfer curve over `range_db` to 0 on both
/// axes: the input peak against the output peak, with a trace up from the
/// floor.
///
/// `input_offset_db` is how far the metered input sits above what enters the
/// plug-in (a limiter that meters after its drive).
pub(crate) fn paint_operating_point(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    live: &Live,
    range_db: f32,
    input_offset_db: f32,
    output_db: impl Fn(f32) -> f32,
) {
    let Some(frame) = live.frame else {
        return;
    };
    if frame.in_peak <= 1.0e-5 {
        return;
    }
    let (px0, py0, pw, ph) = transfer_plot(bounds);
    let in_db = (to_db(frame.in_peak) - input_offset_db).clamp(range_db, 0.0);
    let out_db = output_db(in_db).clamp(range_db, 0.0);
    let x = px0 + (in_db - range_db) / -range_db * pw;
    let y = py0 + ph - (out_db - range_db) / -range_db * ph;
    let color = Colors::accent_primary_hover();
    dashed(
        window,
        (x, py0 + ph),
        (x, y),
        1.0,
        Colors::with_alpha(color, 0.55),
    );
    dashed(
        window,
        (px0, y),
        (x, y),
        1.0,
        Colors::with_alpha(color, 0.55),
    );
    dot(window, x, y, 4.0, color);
}

// ── Spectrum ────────────────────────────────────────────────────────────────

/// The insert's input spectrum behind a frequency display, as a soft fill
/// across `plot` (`x0, y0, w, h`). Silent until a frame arrives.
pub(crate) fn paint_spectrum(window: &mut Window, live: &Live, plot: (f32, f32, f32, f32)) {
    use SpherePluginHost::spectrum::{CEIL_DB, FLOOR_DB, MAX_HZ, MIN_HZ};
    let Some(bins) = live.spectrum_bins() else {
        return;
    };
    let (x0, y0, w, h) = plot;
    let points: Vec<(f32, f32)> = bins
        .iter()
        .enumerate()
        .map(|(i, db)| {
            let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf(i as f32 / (SPECTRUM_BINS - 1) as f32);
            let level = ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0);
            (x0 + freq_fraction(hz) * w, y0 + h - level * h)
        })
        .collect();
    paint_area(
        window,
        &points,
        y0 + h,
        Colors::with_alpha(Colors::text_primary(), 0.06),
    );
    paint_line(
        window,
        &points,
        1.0,
        Colors::with_alpha(Colors::text_primary(), 0.16),
    );
}

// ── Stereo image ────────────────────────────────────────────────────────────

/// A left/right pair as a polar point folded into the upper half-plane:
/// `(angle, radius)`, the angle from −π/2 (all left-minus-right, out of
/// phase) through −π/4 (left only), 0 (mono) and π/4 (right only) to π/2.
fn polar_of(left: f32, right: f32) -> (f32, f32) {
    let mut mid = (left + right) * std::f32::consts::FRAC_1_SQRT_2;
    let mut side = (right - left) * std::f32::consts::FRAC_1_SQRT_2;
    // A point below the axis is the same direction reached with the polarity
    // flipped: fold it up, as a polar scope does.
    if mid < 0.0 {
        mid = -mid;
        side = -side;
    }
    (side.atan2(mid), (mid * mid + side * side).sqrt())
}

/// Which picture of the stereo image a vectorscope draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopeMode {
    /// Every sample as a dot on a half-circle, by direction and level.
    #[default]
    PolarSample,
    /// A ray per direction, as long as the loudest sound that way.
    PolarLevel,
    /// The classic goniometer.
    Lissajous,
}

impl ScopeMode {
    pub const ALL: [ScopeMode; 3] = [
        ScopeMode::PolarSample,
        ScopeMode::PolarLevel,
        ScopeMode::Lissajous,
    ];

    /// The mode's full name.
    pub fn title(self) -> &'static str {
        match self {
            Self::PolarSample => "Polar Sample",
            Self::PolarLevel => "Polar Level",
            Self::Lissajous => "Lissajous",
        }
    }

    /// Its name on a switch.
    pub fn label(self) -> &'static str {
        match self {
            Self::PolarSample => "Sample",
            Self::PolarLevel => "Level",
            Self::Lissajous => "Lissajous",
        }
    }
}

/// The stereo image in `mode`.
pub(crate) fn paint_vectorscope(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    live: &Live,
    mode: ScopeMode,
) {
    match mode {
        ScopeMode::Lissajous => paint_scope(window, cx, bounds, live),
        ScopeMode::PolarSample | ScopeMode::PolarLevel => {
            paint_polar(window, cx, bounds, live, mode == ScopeMode::PolarLevel)
        }
    }
}

/// The half-circle polar scope: mono straight up, left and right on the
/// diagonals, out-of-phase energy down at the baseline either side.
fn paint_polar(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    live: &Live,
    levels: bool,
) {
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
    let (x0, y0, w, h) = frame_of(bounds);
    let centre = (x0 + w * 0.5, y0 + h - 18.0);
    let radius = (w * 0.5 - 16.0).min(h - 40.0).max(8.0);
    let at = |angle: f32, r: f32| (centre.0 + r * angle.sin(), centre.1 - r * angle.cos());
    let ink = Colors::text_primary();
    for r in [radius, radius * 0.5] {
        let arc: Vec<(f32, f32)> = (0..=48)
            .map(|i| at(-FRAC_PI_2 + PI * i as f32 / 48.0, r))
            .collect();
        paint_line(window, &arc, 1.0, Colors::with_alpha(ink, 0.08));
    }
    paint_line(
        window,
        &[at(-FRAC_PI_2, radius), at(FRAC_PI_2, radius)],
        1.0,
        Colors::with_alpha(ink, 0.12),
    );
    for angle in [-FRAC_PI_4, 0.0, FRAC_PI_4] {
        paint_line(
            window,
            &[centre, at(angle, radius)],
            1.0,
            Colors::with_alpha(ink, 0.12),
        );
    }
    // The safe zone: inside the diagonals, nothing is out of phase.
    let size = typography::DENSE_CAPTION;
    let faint = Colors::text_faint();
    let (mx, my) = at(0.0, radius + 4.0);
    label(window, cx, "M", size, faint, mx, my - 12.0, Align::Center);
    let (lx, ly) = at(-FRAC_PI_4, radius + 6.0);
    label(
        window,
        cx,
        "L",
        size,
        faint,
        lx - 4.0,
        ly - 12.0,
        Align::Center,
    );
    let (rx, ry) = at(FRAC_PI_4, radius + 6.0);
    label(
        window,
        cx,
        "R",
        size,
        faint,
        rx + 4.0,
        ry - 12.0,
        Align::Center,
    );
    label(
        window,
        cx,
        "+S",
        size,
        faint,
        centre.0 + radius + 4.0,
        centre.1 - 6.0,
        Align::Left,
    );
    label(
        window,
        cx,
        "−S",
        size,
        faint,
        centre.0 - radius - 4.0,
        centre.1 - 6.0,
        Align::Right,
    );
    if live.image.is_empty() {
        return;
    }
    label(
        window,
        cx,
        &format!("×{:.1}", live.scope_gain),
        size,
        faint,
        x0 + w - 8.0,
        y0 + 8.0,
        Align::Right,
    );
    let accent = Colors::accent_primary_hover();
    if levels {
        for (bin, level) in live.polar.iter().enumerate() {
            let length = (level * live.scope_gain).min(1.0) * radius;
            if length < 0.5 {
                continue;
            }
            let angle = (bin as f32 / (POLAR_BINS - 1) as f32 - 0.5) * PI;
            paint_line(
                window,
                &[centre, at(angle, length)],
                2.0,
                Colors::with_alpha(accent, 0.85),
            );
        }
        return;
    }
    let frames = live.image.len();
    for (age, frame) in live.image.iter().enumerate() {
        let alpha = 0.18 + 0.72 * (age + 1) as f32 / frames as f32;
        let color = Colors::with_alpha(accent, alpha);
        for pair in frame.scope.chunks_exact(2).take(IMAGE_SCOPE_POINTS) {
            let (angle, r) = polar_of(pair[0], pair[1]);
            let (x, y) = at(angle, (r * live.scope_gain).min(1.0) * radius);
            window.paint_quad(fill(rect(x - 1.0, y - 1.0, 2.0, 2.0), color));
        }
    }
}

/// A correlation bar standing up, −1 at the foot to +1 at the head, growing
/// from the middle. `None` reads as no signal.
pub(crate) fn paint_correlation_vertical(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    value: Option<f32>,
) {
    let (x0, y0, w, h) = frame_of(bounds);
    window.paint_quad(fill(bounds, Colors::meter_bg()));
    let centre = y0 + h * 0.5;
    if let Some(value) = value {
        let v = value.clamp(-1.0, 1.0);
        let end = centre - v * h * 0.5;
        let (top, bottom) = if v >= 0.0 {
            (end, centre)
        } else {
            (centre, end)
        };
        let color = if v >= 0.0 {
            Colors::accent_primary()
        } else {
            Colors::accent_warning()
        };
        window.paint_quad(fill(rect(x0, top, w, (bottom - top).max(1.0)), color));
    }
    window.paint_quad(fill(
        rect(x0 - 2.0, centre - 0.5, w + 4.0, 1.0),
        Colors::text_secondary(),
    ));
}

/// The goniometer: mid up, side across, the last frames fading in, scaled by
/// the auto-gain.
pub(crate) fn paint_scope(window: &mut Window, cx: &mut App, bounds: Bounds<Pixels>, live: &Live) {
    let (x0, y0, w, h) = frame_of(bounds);
    let centre = (x0 + w * 0.5, y0 + h * 0.5 + 6.0);
    let radius = ((w).min(h - 24.0) * 0.5 - 10.0).max(8.0);
    let guide = Colors::with_alpha(Colors::text_primary(), 0.12);
    for r in [radius, radius * 0.5] {
        let ring: Vec<(f32, f32)> = (0..=48)
            .map(|i| {
                let a = i as f32 / 48.0 * std::f32::consts::TAU;
                (centre.0 + r * a.cos(), centre.1 + r * a.sin())
            })
            .collect();
        paint_line(
            window,
            &ring,
            1.0,
            Colors::with_alpha(Colors::text_primary(), 0.07),
        );
    }
    let diagonal = radius * std::f32::consts::FRAC_1_SQRT_2;
    for (dx, dy) in [
        (0.0, radius),
        (radius, 0.0),
        (diagonal, diagonal),
        (diagonal, -diagonal),
    ] {
        paint_line(
            window,
            &[
                (centre.0 - dx, centre.1 - dy),
                (centre.0 + dx, centre.1 + dy),
            ],
            1.0,
            guide,
        );
    }
    let text = Colors::text_faint();
    let size = typography::DENSE_CAPTION;
    label(
        window,
        cx,
        "M",
        size,
        text,
        centre.0,
        centre.1 - radius - 13.0,
        Align::Center,
    );
    label(
        window,
        cx,
        "L",
        size,
        text,
        centre.0 - diagonal - 6.0,
        centre.1 - diagonal - 12.0,
        Align::Center,
    );
    label(
        window,
        cx,
        "R",
        size,
        text,
        centre.0 + diagonal + 6.0,
        centre.1 - diagonal - 12.0,
        Align::Center,
    );
    label(
        window,
        cx,
        "S",
        size,
        text,
        centre.0 + radius + 8.0,
        centre.1 - 6.0,
        Align::Center,
    );
    if live.image.is_empty() {
        return;
    }
    label(
        window,
        cx,
        &format!("×{:.1}", live.scope_gain),
        size,
        text,
        x0 + w - 8.0,
        y0 + 8.0,
        Align::Right,
    );
    let frames = live.image.len();
    let accent = Colors::accent_primary_hover();
    for (age, frame) in live.image.iter().enumerate() {
        let alpha = 0.18 + 0.72 * (age + 1) as f32 / frames as f32;
        let color = Colors::with_alpha(accent, alpha);
        for pair in frame.scope.chunks_exact(2).take(IMAGE_SCOPE_POINTS) {
            let (l, r) = (pair[0] * live.scope_gain, pair[1] * live.scope_gain);
            let side = ((r - l) * std::f32::consts::FRAC_1_SQRT_2).clamp(-1.0, 1.0);
            let mid = ((l + r) * std::f32::consts::FRAC_1_SQRT_2).clamp(-1.0, 1.0);
            let x = centre.0 + side * radius;
            let y = centre.1 - mid * radius;
            window.paint_quad(fill(rect(x - 1.0, y - 1.0, 2.0, 2.0), color));
        }
    }
}

/// A correlation bar, −1 to +1, growing from the centre: accent toward mono,
/// warning toward out of phase. `None` reads as no signal.
pub(crate) fn paint_correlation(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    value: Option<f32>,
    with_scale: bool,
) {
    let (x0, y0, w, h) = frame_of(bounds);
    let bar_h = if with_scale {
        (h - 30.0).max(4.0)
    } else {
        h.min(8.0)
    };
    let bar_y = y0 + if with_scale { 14.0 } else { (h - bar_h) * 0.5 };
    window.paint_quad(fill(rect(x0, bar_y, w, bar_h), Colors::meter_bg()));
    let centre = x0 + w * 0.5;
    if let Some(value) = value {
        let v = value.clamp(-1.0, 1.0);
        let end = centre + v * w * 0.5;
        let (left, right) = if v >= 0.0 {
            (centre, end)
        } else {
            (end, centre)
        };
        let color = if v >= 0.0 {
            Colors::accent_primary()
        } else {
            Colors::accent_warning()
        };
        window.paint_quad(fill(
            rect(left, bar_y, (right - left).max(1.0), bar_h),
            color,
        ));
    }
    window.paint_quad(fill(
        rect(centre - 0.5, bar_y - 2.0, 1.0, bar_h + 4.0),
        Colors::text_secondary(),
    ));
    if with_scale {
        let size = typography::DENSE_CAPTION;
        let faint = Colors::text_faint();
        label(
            window,
            cx,
            "−1",
            size,
            faint,
            x0,
            bar_y + bar_h + 2.0,
            Align::Left,
        );
        label(
            window,
            cx,
            "0",
            size,
            faint,
            centre,
            bar_y + bar_h + 2.0,
            Align::Center,
        );
        label(
            window,
            cx,
            "+1",
            size,
            faint,
            x0 + w,
            bar_y + bar_h + 2.0,
            Align::Right,
        );
        let text = value.map_or("—".to_string(), |v| format!("{v:+.2}"));
        label(
            window,
            cx,
            &text,
            typography::UI_XS,
            Colors::text_secondary(),
            x0 + w,
            y0,
            Align::Right,
        );
        label(
            window,
            cx,
            "Correlation",
            size,
            Colors::text_muted(),
            x0,
            y0 + 1.0,
            Align::Left,
        );
    }
}

/// A rack position's two level bars: what enters above, what leaves below,
/// over −48 to 0 dBFS.
pub(crate) fn paint_stage_bars(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    input: f32,
    output: f32,
) {
    let (x0, y0, w, h) = frame_of(bounds);
    let bar_h = ((h - 2.0) * 0.5).clamp(1.0, 3.0);
    let unit = |level: f32| ((to_db(level) - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB).clamp(0.0, 1.0);
    for (row, level, color) in [
        (0.0, input, Colors::text_faint()),
        (bar_h + 2.0, output, Colors::accent_primary()),
    ] {
        window.paint_quad(fill(rect(x0, y0 + row, w, bar_h), Colors::meter_bg()));
        window.paint_quad(fill(rect(x0, y0 + row, w * unit(level), bar_h), color));
    }
}

/// A horizontal level bar over −60 to 0 dBFS: RMS filled, the peak a tick.
pub(crate) fn paint_level_bar(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    caption: &str,
    peak: Option<f32>,
    rms: Option<f32>,
) {
    let (x0, y0, w, h) = frame_of(bounds);
    let bar_y = y0 + 14.0;
    let bar_h = (h - 14.0).clamp(3.0, 6.0);
    window.paint_quad(fill(rect(x0, bar_y, w, bar_h), Colors::meter_bg()));
    let unit = |level: f32| ((to_db(level) + 60.0) / 60.0).clamp(0.0, 1.0);
    if let Some(rms) = rms {
        window.paint_quad(fill(
            rect(x0, bar_y, w * unit(rms), bar_h),
            Colors::text_muted(),
        ));
    }
    if let Some(peak) = peak {
        let color = if peak >= 1.0 {
            Colors::meter_high()
        } else {
            Colors::text_primary()
        };
        window.paint_quad(fill(
            rect(x0 + w * unit(peak) - 1.0, bar_y, 2.0, bar_h),
            color,
        ));
    }
    let size = typography::DENSE_CAPTION;
    label(
        window,
        cx,
        caption,
        size,
        Colors::text_muted(),
        x0,
        y0,
        Align::Left,
    );
    let text = peak.map_or("—".to_string(), |peak| db_text(to_db(peak)));
    label(
        window,
        cx,
        &text,
        size,
        Colors::text_secondary(),
        x0 + w,
        y0,
        Align::Right,
    );
}

/// A reduction bar growing from the right over 24 dB, its readout over it.
pub(crate) fn paint_reduction_bar(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    caption: &str,
    reduction_db: Option<f32>,
) {
    let (x0, y0, w, h) = frame_of(bounds);
    let bar_y = y0 + 14.0;
    let bar_h = (h - 14.0).clamp(3.0, 6.0);
    window.paint_quad(fill(rect(x0, bar_y, w, bar_h), Colors::meter_bg()));
    if let Some(db) = reduction_db {
        let width = w * (db / 24.0).clamp(0.0, 1.0);
        window.paint_quad(fill(
            rect(x0 + w - width, bar_y, width, bar_h),
            Colors::accent_primary(),
        ));
    }
    let size = typography::DENSE_CAPTION;
    label(
        window,
        cx,
        caption,
        size,
        Colors::text_muted(),
        x0,
        y0,
        Align::Left,
    );
    let text = match reduction_db {
        None => "—".to_string(),
        Some(db) if db >= 0.05 => format!("−{db:.1}"),
        Some(_) => "0.0".to_string(),
    };
    label(
        window,
        cx,
        &text,
        size,
        Colors::text_secondary(),
        x0 + w,
        y0,
        Align::Right,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PluginInstanceKey {
        PluginInstanceKey {
            track_id: "t".into(),
            insert_id: "i".into(),
        }
    }

    #[test]
    fn an_idle_instance_asks_for_no_redraw() {
        let mut live = Live::new(key(), &BuiltinEditorHostOps::default());
        assert!(!live.poll());
        assert!(live.frame.is_none());
    }

    #[test]
    fn the_held_peak_outlasts_the_reading() {
        let mut live = Live::new(key(), &BuiltinEditorHostOps::default());
        live.show(BuiltinMeterFrame {
            in_peak: 1.0,
            gain_reduction_db: 6.0,
            ..Default::default()
        });
        for _ in 0..10 {
            live.show(BuiltinMeterFrame::default());
        }
        assert_eq!(live.held(|p| p.reduction_db), 6.0);
        assert_eq!(live.held(|p| p.in_db), 0.0);
        for _ in 0..HOLD_FRAMES {
            live.show(BuiltinMeterFrame::default());
        }
        assert_eq!(live.held(|p| p.reduction_db), 0.0);
    }

    #[test]
    fn the_needle_rests_right_at_no_reduction_and_left_at_full_scale() {
        assert_eq!(vu_position(VuScale::Reduction(20.0), 0.0), 1.0);
        assert_eq!(vu_position(VuScale::Reduction(20.0), 20.0), -1.0);
        assert_eq!(vu_position(VuScale::Output, -20.0), -1.0);
        assert_eq!(vu_position(VuScale::Output, 3.0), 1.0);
    }

    #[test]
    fn the_polar_scope_reads_left_mono_right_and_out_of_phase() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
        let close = |a: f32, b: f32| (a - b).abs() < 1.0e-5;
        assert!(close(polar_of(1.0, 1.0).0, 0.0));
        assert!(close(polar_of(1.0, 0.0).0, -FRAC_PI_4));
        assert!(close(polar_of(0.0, 1.0).0, FRAC_PI_4));
        assert!(close(polar_of(1.0, -1.0).0.abs(), FRAC_PI_2));
        // Polarity does not change the direction.
        assert!(close(polar_of(-1.0, 0.0).0, -FRAC_PI_4));
    }

    #[test]
    fn polar_levels_hold_and_fall_back() {
        let mut live = Live::new(key(), &BuiltinEditorHostOps::default());
        let mut frame = StereoImageFrame::default();
        frame.scope[0] = 0.5;
        frame.scope[1] = 0.5;
        live.show_image(frame);
        let mono = POLAR_BINS / 2;
        assert!(live.polar[mono] > 0.7);
        live.show_image(StereoImageFrame::default());
        assert!(live.polar[mono] < 0.7 && live.polar[mono] > 0.5);
    }

    #[test]
    fn the_scope_backs_off_a_loud_passage_at_once() {
        let mut live = Live::new(key(), &BuiltinEditorHostOps::default());
        let mut frame = StereoImageFrame::default();
        frame.scope[0] = 0.01;
        for _ in 0..200 {
            live.show_image(frame);
        }
        assert!(live.scope_gain > 15.0);
        frame.scope[0] = 0.9;
        live.show_image(frame);
        assert!(live.scope_gain < 9.0);
        assert_eq!(live.image.len(), SCOPE_FRAMES);
    }
}
