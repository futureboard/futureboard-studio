//! The Audio Processing Canvas.
//!
//! One full-height time-frequency surface carries the source audio and every
//! processor's effect on it. Its geometry comes from a single
//! [`CanvasTransform`]: the spectrogram tiles, the waveform, the frequency
//! ruler, each processing overlay, and pointer hit-testing all convert through
//! the same functions, so an overlay drawn at 60 Hz sits on the 60 Hz row of
//! the image behind it.
//!
//! Nothing here decodes audio, runs an FFT, or allocates per frame beyond the
//! bounded column list the viewport asks for. Analysis products arrive already
//! built from the window's background jobs.

use std::sync::Arc;

use gpui::{
    div, fill, img, point, px, size, Bounds, IntoElement, ParentElement, Pixels, RenderImage,
    Styled, StyledImage, Window,
};
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;
use sphere_audio_editor::{
    frequency_position, position_frequency, FrequencyScale, SpectrogramTileView,
};
use SphereAudioProcessor::NoiseGateMask;

use crate::theme::{radius, space, typography, Colors};

/// Vertical share of the canvas taken by the before/after lane when it is on.
const DIFF_LANE_FRACTION: f32 = 0.18;
const DIFF_LANE_MAX: f32 = 96.0;
/// Frequency ruler gutter. Inset over the content, not a separate column, so
/// the canvas keeps one coordinate space across its whole width.
const FREQ_GUTTER: f32 = 40.0;
const TIME_RULER_H: f32 = 16.0;
/// Below this spacing two event markers are indistinguishable, so the painter
/// collapses them instead of stacking sub-pixel quads.
const MIN_EVENT_SPACING: f32 = 2.0;

/// Which source representation the canvas shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CanvasView {
    Waveform,
    Spectrogram,
    #[default]
    Overlay,
}

impl CanvasView {
    pub const ALL: [Self; 3] = [Self::Waveform, Self::Spectrogram, Self::Overlay];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Waveform => "Wave",
            Self::Spectrogram => "Spectro",
            Self::Overlay => "Both",
        }
    }

    pub const fn shows_waveform(self) -> bool {
        matches!(self, Self::Waveform | Self::Overlay)
    }

    pub const fn shows_spectrogram(self) -> bool {
        matches!(self, Self::Spectrogram | Self::Overlay)
    }
}

/// Horizontal view state over the analyzed source window.
///
/// Frames are source frames, matching the selection and processing domain, so
/// no screen-space value ever reaches a DSP parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasViewport {
    pub start_frame: f64,
    pub frames_per_pixel: f64,
}

impl Default for CanvasViewport {
    fn default() -> Self {
        Self {
            start_frame: 0.0,
            frames_per_pixel: 256.0,
        }
    }
}

impl CanvasViewport {
    pub fn fit(window_frames: u64, width: f32) -> Self {
        Self {
            start_frame: 0.0,
            frames_per_pixel: (window_frames.max(1) as f64 / width.max(1.0) as f64).max(1.0e-3),
        }
    }

    /// Zoom around the frame beneath `anchor_x`, preserving it.
    pub fn zoom_around(&mut self, factor: f32, anchor_x: f32, window_frames: u64, width: f32) {
        let anchor_frame = self.start_frame + anchor_x.max(0.0) as f64 * self.frames_per_pixel;
        let fit = (window_frames.max(1) as f64 / width.max(1.0) as f64).max(1.0e-3);
        self.frames_per_pixel =
            (self.frames_per_pixel / factor.max(1.0e-3) as f64).clamp(1.0 / 64.0, fit.max(1.0e-3));
        self.start_frame = anchor_frame - anchor_x.max(0.0) as f64 * self.frames_per_pixel;
        self.clamp(window_frames, width);
    }

    pub fn clamp(&mut self, window_frames: u64, width: f32) {
        let span = self.frames_per_pixel * width.max(1.0) as f64;
        let max_start = (window_frames as f64 - span).max(0.0);
        self.start_frame = self.start_frame.clamp(0.0, max_start);
    }

    pub fn scroll_by(&mut self, delta_px: f32, window_frames: u64, width: f32) {
        self.start_frame += delta_px as f64 * self.frames_per_pixel;
        self.clamp(window_frames, width);
    }
}

/// The one coordinate model for the canvas.
#[derive(Debug, Clone, Copy)]
pub struct CanvasTransform {
    /// Source frame at the left edge of the window being edited.
    pub window_start_frame: u64,
    pub window_frames: u64,
    pub sample_rate: u32,
    pub max_hz: f32,
    pub frequency_scale: FrequencyScale,
    pub viewport: CanvasViewport,
    pub width: f32,
    /// Height of the time-frequency plane, excluding rulers and the diff lane.
    pub height: f32,
}

impl CanvasTransform {
    /// `frame` is relative to the start of the edited window.
    pub fn frame_to_x(&self, frame: f64) -> f32 {
        ((frame - self.viewport.start_frame) / self.viewport.frames_per_pixel.max(1.0e-9)) as f32
    }

    pub fn x_to_frame(&self, x: f32) -> f64 {
        self.viewport.start_frame + x as f64 * self.viewport.frames_per_pixel
    }

    /// `frame` is an absolute source frame.
    pub fn source_frame_to_x(&self, frame: i64) -> f32 {
        self.frame_to_x(frame as f64 - self.window_start_frame as f64)
    }

    pub fn x_to_source_frame(&self, x: f32) -> i64 {
        (self.window_start_frame as f64 + self.x_to_frame(x)).round() as i64
    }

    pub fn seconds_to_x(&self, seconds: f32) -> f32 {
        self.frame_to_x(seconds as f64 * self.sample_rate.max(1) as f64)
    }

    pub fn hz_to_y(&self, hz: f32) -> f32 {
        self.height * (1.0 - frequency_position(hz, self.max_hz, self.frequency_scale))
    }

    pub fn y_to_hz(&self, y: f32) -> f32 {
        let position = 1.0 - (y / self.height.max(1.0)).clamp(0.0, 1.0);
        position_frequency(position, self.max_hz, self.frequency_scale)
    }

    pub fn visible_frames(&self) -> (u64, u64) {
        let start = self.viewport.start_frame.max(0.0) as u64;
        let end = (self.x_to_frame(self.width).ceil().max(0.0) as u64).min(self.window_frames);
        (start.min(end), end)
    }

    pub fn frame_label(&self, frame: f64) -> String {
        let seconds = frame.max(0.0) / self.sample_rate.max(1) as f64;
        let minutes = (seconds / 60.0).floor();
        let rest = seconds - minutes * 60.0;
        format!("{minutes:.0}:{rest:06.3}")
    }
}

/// Min/max peaks for the edited window, over the interleaved PCM they came
/// from.
///
/// Zoomed out, columns read from a fixed-size mip; zoomed in past one mip
/// bucket per column, they read the samples directly. Either path is bounded
/// by the canvas width, so scrolling never rescans the whole clip.
#[derive(Clone)]
pub struct WaveformPeaks {
    samples: Arc<[f32]>,
    channels: usize,
    frames: usize,
    mip: Arc<[(f32, f32)]>,
    bucket_frames: f64,
}

impl WaveformPeaks {
    pub const MIP_BUCKETS: usize = 65_536;

    pub fn build(samples: Arc<[f32]>, channels: usize) -> Self {
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        let buckets = Self::MIP_BUCKETS.min(frames.max(1));
        let bucket_frames = frames.max(1) as f64 / buckets as f64;
        let mut mip = Vec::with_capacity(buckets);
        for bucket in 0..buckets {
            let start = (bucket as f64 * bucket_frames).floor() as usize;
            let end = (((bucket + 1) as f64 * bucket_frames).ceil() as usize).min(frames);
            let (lo, hi) = extrema(&samples, channels, start.min(end), end);
            mip.push((lo, hi));
        }
        Self {
            samples,
            channels,
            frames,
            mip: mip.into(),
            bucket_frames,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// One min/max pair per pixel column for the current viewport.
    pub fn columns(&self, transform: &CanvasTransform) -> Vec<(f32, f32)> {
        let width = transform.width.max(1.0) as usize;
        let mut columns = Vec::with_capacity(width);
        let from_samples = transform.viewport.frames_per_pixel < self.bucket_frames;
        for x in 0..width {
            let f0 = transform.x_to_frame(x as f32).max(0.0);
            let f1 = transform.x_to_frame(x as f32 + 1.0).max(f0);
            let (lo, hi) = if from_samples {
                let start = (f0 as usize).min(self.frames);
                let end = ((f1.ceil() as usize).max(start + 1)).min(self.frames);
                extrema(&self.samples, self.channels, start, end)
            } else {
                let b0 = (f0 / self.bucket_frames) as usize;
                let b1 = ((f1 / self.bucket_frames).ceil() as usize)
                    .max(b0 + 1)
                    .min(self.mip.len());
                let mut lo = 0.0_f32;
                let mut hi = 0.0_f32;
                for (bucket_lo, bucket_hi) in &self.mip[b0.min(self.mip.len())..b1] {
                    lo = lo.min(*bucket_lo);
                    hi = hi.max(*bucket_hi);
                }
                (lo, hi)
            };
            columns.push((lo, hi));
        }
        columns
    }
}

fn extrema(samples: &[f32], channels: usize, start_frame: usize, end_frame: usize) -> (f32, f32) {
    let start = start_frame.saturating_mul(channels).min(samples.len());
    let end = end_frame.saturating_mul(channels).min(samples.len());
    let mut lo = 0.0_f32;
    let mut hi = 0.0_f32;
    for sample in &samples[start.min(end)..end] {
        lo = lo.min(*sample);
        hi = hi.max(*sample);
    }
    (lo, hi)
}

/// A time-domain event drawn as a vertical mark.
#[derive(Debug, Clone, Copy)]
pub struct CanvasEvent {
    /// Frame relative to the start of the edited window.
    pub frame: u64,
    pub width_frames: u32,
    /// 0..=1, drives mark opacity.
    pub strength: f32,
}

/// A horizontal frequency band, used for notches and fixed-band attenuation.
#[derive(Debug, Clone, Copy)]
pub struct CanvasBand {
    pub center_hz: f32,
    pub half_width_hz: f32,
    /// Attenuation in dB, drives the band's weight.
    pub depth_db: f32,
}

/// The before/after lane under the time-frequency plane.
#[derive(Debug, Clone, Default)]
pub struct CanvasDiff {
    /// Level envelope of the source over the previewed range.
    pub before: Vec<f32>,
    /// Level envelope after the active processor ran over the same range.
    pub after: Vec<f32>,
    /// Frames the preview actually covers, relative to the edited window.
    pub start_frame: u64,
    pub end_frame: u64,
    pub stale: bool,
}

impl CanvasDiff {
    pub fn is_empty(&self) -> bool {
        self.before.is_empty() || self.after.is_empty()
    }
}

/// Everything the active module contributes to the canvas.
#[derive(Clone, Default)]
pub struct CanvasOverlays {
    /// Gated time-frequency cells, pre-rendered against this transform's
    /// frequency scale by the analysis job.
    pub mask: Option<CanvasMask>,
    pub bands: Vec<CanvasBand>,
    pub events: Vec<CanvasEvent>,
    pub event_tone: EventTone,
    pub selected_event: Option<usize>,
    pub transients: Vec<CanvasEvent>,
    /// Time-frequency region the module targets.
    pub region: Option<CanvasRegion>,
    pub region_gain_db: f32,
    pub diff: Option<CanvasDiff>,
}

/// A pre-rendered attenuation map aligned to a frame range of the window.
#[derive(Clone)]
pub struct CanvasMask {
    pub image: Arc<RenderImage>,
    pub start_frame: u64,
    pub end_frame: u64,
}

/// Width/height of the attenuation image. Fixed so the job cost does not track
/// clip length, and stretched to the frame range it covers at paint time.
const MASK_IMAGE_WIDTH: usize = 512;
const MASK_IMAGE_HEIGHT: usize = 192;

/// Turn a gate map into an image aligned to `scale`, so the shaded cells land
/// on the same rows as the spectrogram tiles underneath.
///
/// Runs on the analysis worker, never on the render path.
pub fn build_mask_image(
    mask: &NoiseGateMask,
    sample_rate: u32,
    frequency_scale: FrequencyScale,
) -> Option<Arc<RenderImage>> {
    if mask.columns == 0 || mask.bins < 2 {
        return None;
    }
    let max_hz = sample_rate.max(1) as f32 * 0.5;
    let width = MASK_IMAGE_WIDTH.min(mask.columns.max(1));
    let height = MASK_IMAGE_HEIGHT;
    let tint = Colors::accent_purple();
    let (r, g, b) = (
        (tint.r * 255.0) as u8,
        (tint.g * 255.0) as u8,
        (tint.b * 255.0) as u8,
    );
    let mut pixels = Vec::with_capacity(width * height * 4);
    let mut row_bins = Vec::with_capacity(height);
    for row in 0..height {
        let position = 1.0 - row as f32 / (height - 1) as f32;
        let hz = position_frequency(position, max_hz, frequency_scale);
        let bin = ((hz / max_hz) * (mask.bins - 1) as f32).round() as usize;
        row_bins.push(bin.min(mask.bins - 1));
    }
    for &bin in &row_bins {
        for x in 0..width {
            let column = x * mask.columns / width;
            let gain = mask.gain(column, bin).clamp(0.0, 1.0);
            // Depth of the cut drives opacity; an untouched cell is invisible.
            let attenuation = 1.0 - gain;
            let alpha = (attenuation * 0.55 * 255.0) as u8;
            pixels.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width as u32, height as u32, pixels)?;
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        Frame::new(buffer),
        1,
    ))))
}

#[derive(Debug, Clone, Copy)]
pub struct CanvasRegion {
    pub start_frame: i64,
    pub end_frame: i64,
    pub min_hz: f32,
    pub max_hz: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventTone {
    #[default]
    Repair,
    Neutral,
}

impl EventTone {
    fn color(self) -> gpui::Rgba {
        match self {
            Self::Repair => Colors::status_error(),
            Self::Neutral => Colors::text_secondary(),
        }
    }
}

/// Immutable description of one canvas paint.
#[derive(Clone)]
pub struct ProcessingCanvas {
    pub transform: CanvasTransform,
    pub view: CanvasView,
    pub peaks: Option<WaveformPeaks>,
    pub tiles: Vec<SpectrogramTileView>,
    /// Offset of the edited window inside the analyzed file, in seconds.
    pub tile_offset_seconds: f32,
    pub overlays: CanvasOverlays,
    pub status: Option<String>,
    pub status_is_error: bool,
}

impl ProcessingCanvas {
    fn diff_height(&self) -> f32 {
        if self
            .overlays
            .diff
            .as_ref()
            .is_some_and(|diff| !diff.is_empty())
        {
            (self.transform.height * DIFF_LANE_FRACTION).min(DIFF_LANE_MAX)
        } else {
            0.0
        }
    }

    /// Height of the time-frequency plane once the diff lane is subtracted.
    pub fn plane_height(&self) -> f32 {
        (self.transform.height - self.diff_height()).max(1.0)
    }

    /// A transform whose `height` matches the plane the overlays draw on.
    pub fn plane_transform(&self) -> CanvasTransform {
        CanvasTransform {
            height: self.plane_height(),
            ..self.transform
        }
    }
}

fn quad(
    window: &mut Window,
    origin: gpui::Point<Pixels>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: gpui::Rgba,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    window.paint_quad(fill(
        Bounds::new(origin + point(px(x), px(y)), size(px(w), px(h))),
        color,
    ));
}

/// Spectrogram tiles positioned by the shared time transform.
fn tile_layer(canvas: &ProcessingCanvas) -> impl IntoElement {
    let transform = canvas.plane_transform();
    let height = transform.height;
    let tiles = canvas
        .tiles
        .iter()
        .filter_map(|tile| {
            let start = tile.start_seconds - canvas.tile_offset_seconds;
            let x = transform.seconds_to_x(start);
            let width = transform.seconds_to_x(start + tile.duration_seconds) - x;
            if width <= 0.0 || x + width < 0.0 || x > transform.width {
                return None;
            }
            Some(
                img(Arc::clone(&tile.image))
                    .absolute()
                    .left(px(x))
                    .top(px(0.0))
                    .w(px(width))
                    .h(px(height))
                    .object_fit(gpui::ObjectFit::Fill),
            )
        })
        .collect::<Vec<_>>();
    div()
        .absolute()
        .left_0()
        .top_0()
        .w(px(transform.width))
        .h(px(height))
        .overflow_hidden()
        .children(tiles)
}

/// The attenuation map, stretched across the frame range it was computed for.
fn mask_layer(canvas: &ProcessingCanvas) -> Option<impl IntoElement> {
    let mask = canvas.overlays.mask.clone()?;
    let transform = canvas.plane_transform();
    let x0 = transform.frame_to_x(mask.start_frame as f64);
    let x1 = transform.frame_to_x(mask.end_frame as f64);
    if x1 - x0 <= 0.0 || x1 < 0.0 || x0 > transform.width {
        return None;
    }
    Some(
        div()
            .absolute()
            .left_0()
            .top_0()
            .w(px(transform.width))
            .h(px(transform.height))
            .overflow_hidden()
            .child(
                img(mask.image)
                    .absolute()
                    .left(px(x0))
                    .top(px(0.0))
                    .w(px(x1 - x0))
                    .h(px(transform.height))
                    .object_fit(gpui::ObjectFit::Fill),
            ),
    )
}

/// Waveform, notch bands, events, region and the diff lane share one painter
/// so they cannot drift apart or reorder between frames.
fn paint_layer(canvas: ProcessingCanvas) -> impl IntoElement {
    gpui::canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let origin = bounds.origin;
            let transform = CanvasTransform {
                width: f32::from(bounds.size.width).max(1.0),
                height: f32::from(bounds.size.height).max(1.0),
                ..canvas.transform
            };
            let plane = ProcessingCanvas {
                transform,
                ..canvas.clone()
            };
            let plane_h = plane.plane_height();
            let t = plane.plane_transform();

            if plane.view.shows_waveform() {
                paint_waveform(window, origin, &plane, &t, plane_h);
            }
            paint_bands(window, origin, &plane.overlays.bands, &t);
            paint_region(
                window,
                origin,
                plane.overlays.region,
                plane.overlays.region_gain_db,
                &t,
            );
            paint_events(
                window,
                origin,
                &plane.overlays.transients,
                EventTone::Neutral,
                None,
                &t,
            );
            paint_events(
                window,
                origin,
                &plane.overlays.events,
                plane.overlays.event_tone,
                plane.overlays.selected_event,
                &t,
            );
            if let Some(diff) = plane.overlays.diff.as_ref() {
                paint_diff(window, origin, diff, &t, plane_h, transform.height);
            }
        },
    )
    .size_full()
}

fn paint_waveform(
    window: &mut Window,
    origin: gpui::Point<Pixels>,
    canvas: &ProcessingCanvas,
    transform: &CanvasTransform,
    plane_h: f32,
) {
    let Some(peaks) = canvas.peaks.as_ref() else {
        return;
    };
    if peaks.is_empty() {
        return;
    }
    let columns = peaks.columns(transform);
    let center = plane_h * 0.5;
    // Over the spectrogram the waveform is a reference layer, not the subject,
    // so it drops to a weight that keeps spectral detail readable.
    let alpha = if canvas.view == CanvasView::Overlay {
        0.5
    } else {
        0.9
    };
    let body = Colors::with_alpha(Colors::accent_primary(), alpha * 0.45);
    let edge = Colors::with_alpha(Colors::accent_primary(), alpha);
    quad(
        window,
        origin,
        0.0,
        center,
        transform.width,
        1.0,
        Colors::with_alpha(Colors::text_faint(), 0.3),
    );
    for (x, (lo, hi)) in columns.iter().enumerate() {
        if *lo == 0.0 && *hi == 0.0 {
            continue;
        }
        let top = center - hi.clamp(-1.0, 1.0) * center;
        let bottom = center - lo.clamp(-1.0, 1.0) * center;
        let h = (bottom - top).max(1.0);
        quad(window, origin, x as f32, top, 1.0, h, body);
        quad(window, origin, x as f32, top, 1.0, 1.0, edge);
        quad(window, origin, x as f32, bottom - 1.0, 1.0, 1.0, edge);
    }
}

fn paint_bands(
    window: &mut Window,
    origin: gpui::Point<Pixels>,
    bands: &[CanvasBand],
    transform: &CanvasTransform,
) {
    for band in bands {
        let depth = (band.depth_db.abs() / 48.0).clamp(0.05, 1.0);
        let y_low = transform.hz_to_y(band.center_hz + band.half_width_hz);
        let y_high = transform.hz_to_y((band.center_hz - band.half_width_hz).max(0.0));
        let top = y_low.min(y_high);
        let h = (y_high - y_low).abs().max(1.0);
        quad(
            window,
            origin,
            0.0,
            top,
            transform.width,
            h,
            Colors::with_alpha(Colors::status_warning(), 0.10 + depth * 0.18),
        );
        let center_y = transform.hz_to_y(band.center_hz);
        quad(
            window,
            origin,
            0.0,
            center_y,
            transform.width,
            1.0,
            Colors::with_alpha(Colors::status_warning(), 0.35 + depth * 0.5),
        );
    }
}

fn paint_region(
    window: &mut Window,
    origin: gpui::Point<Pixels>,
    region: Option<CanvasRegion>,
    gain_db: f32,
    transform: &CanvasTransform,
) {
    let Some(region) = region else {
        return;
    };
    let x0 = transform.source_frame_to_x(region.start_frame).max(0.0);
    let x1 = transform
        .source_frame_to_x(region.end_frame)
        .min(transform.width);
    if x1 <= x0 {
        return;
    }
    let y0 = transform.hz_to_y(region.max_hz).max(0.0);
    let y1 = transform.hz_to_y(region.min_hz).min(transform.height);
    let h = (y1 - y0).max(1.0);
    let w = x1 - x0;
    // A cut and a boost are both legitimate here, so the wash carries the sign
    // while the frame keeps the region readable either way.
    let wash = if gain_db < -0.05 {
        Colors::with_alpha(Colors::status_error(), 0.16)
    } else if gain_db > 0.05 {
        Colors::with_alpha(Colors::status_success(), 0.16)
    } else {
        Colors::with_alpha(Colors::accent_primary(), 0.14)
    };
    quad(window, origin, x0, y0, w, h, wash);
    let edge = Colors::with_alpha(Colors::accent_primary(), 0.85);
    quad(window, origin, x0, y0, w, 1.0, edge);
    quad(window, origin, x0, y1 - 1.0, w, 1.0, edge);
    quad(window, origin, x0, y0, 1.0, h, edge);
    quad(window, origin, x1 - 1.0, y0, 1.0, h, edge);
}

fn paint_events(
    window: &mut Window,
    origin: gpui::Point<Pixels>,
    events: &[CanvasEvent],
    tone: EventTone,
    selected: Option<usize>,
    transform: &CanvasTransform,
) {
    let color = tone.color();
    let mut last_x = f32::NEG_INFINITY;
    for (index, event) in events.iter().enumerate() {
        let x = transform.frame_to_x(event.frame as f64);
        if x < -2.0 || x > transform.width + 2.0 {
            continue;
        }
        let is_selected = selected == Some(index);
        if !is_selected && x - last_x < MIN_EVENT_SPACING {
            continue;
        }
        last_x = x;
        let w = transform
            .frame_to_x((event.frame + event.width_frames as u64) as f64)
            .max(x + 1.0)
            - x;
        let alpha = if is_selected {
            0.95
        } else {
            0.30 + event.strength.clamp(0.0, 1.0) * 0.45
        };
        quad(
            window,
            origin,
            x,
            0.0,
            w.max(1.0),
            transform.height,
            Colors::with_alpha(color, alpha * 0.45),
        );
        quad(
            window,
            origin,
            x,
            0.0,
            if is_selected { 2.0 } else { 1.0 },
            transform.height,
            Colors::with_alpha(color, alpha),
        );
        if is_selected {
            let cap = 5.0;
            quad(window, origin, x - 2.0, 0.0, cap, 2.0, color);
            quad(
                window,
                origin,
                x - 2.0,
                transform.height - 2.0,
                cap,
                2.0,
                color,
            );
        }
    }
}

fn paint_diff(
    window: &mut Window,
    origin: gpui::Point<Pixels>,
    diff: &CanvasDiff,
    transform: &CanvasTransform,
    lane_top: f32,
    total_height: f32,
) {
    let lane_h = (total_height - lane_top).max(1.0);
    quad(
        window,
        origin,
        0.0,
        lane_top,
        transform.width,
        lane_h,
        Colors::with_alpha(Colors::surface_panel_alt(), 0.92),
    );
    quad(
        window,
        origin,
        0.0,
        lane_top,
        transform.width,
        1.0,
        Colors::border_subtle(),
    );
    if diff.is_empty() {
        return;
    }
    let x0 = transform.frame_to_x(diff.start_frame as f64);
    let x1 = transform.frame_to_x(diff.end_frame as f64);
    let span = x1 - x0;
    if span <= 1.0 {
        return;
    }
    let dim = if diff.stale { 0.45 } else { 1.0 };
    let peak = diff
        .before
        .iter()
        .chain(diff.after.iter())
        .copied()
        .fold(1.0e-4_f32, f32::max);
    let inner_h = (lane_h - 4.0).max(1.0);
    let base_y = lane_top + lane_h - 2.0;
    let columns = span.min(transform.width * 2.0).max(1.0) as usize;
    for column in 0..columns {
        let t = column as f32 / columns as f32;
        let x = x0 + t * span;
        if x < 0.0 || x > transform.width {
            continue;
        }
        let before = sample_envelope(&diff.before, t) / peak;
        let after = sample_envelope(&diff.after, t) / peak;
        let before_h = before.clamp(0.0, 1.0) * inner_h;
        let after_h = after.clamp(0.0, 1.0) * inner_h;
        quad(
            window,
            origin,
            x,
            base_y - before_h,
            1.0,
            before_h,
            Colors::with_alpha(Colors::text_muted(), 0.45 * dim),
        );
        // What the processor removes is the readable part of a before/after
        // comparison, so the difference gets the strong colour, not the result.
        if before_h > after_h + 0.5 {
            quad(
                window,
                origin,
                x,
                base_y - before_h,
                1.0,
                before_h - after_h,
                Colors::with_alpha(Colors::status_error(), 0.55 * dim),
            );
        }
        quad(
            window,
            origin,
            x,
            base_y - after_h,
            1.0,
            1.0,
            Colors::with_alpha(Colors::accent_primary(), 0.95 * dim),
        );
    }
}

fn sample_envelope(values: &[f32], t: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let index = (t.clamp(0.0, 1.0) * (values.len() - 1) as f32).round() as usize;
    values[index.min(values.len() - 1)]
}

/// Frequency gutter. Only drawn when the spectrogram is visible, because on a
/// waveform-only canvas the vertical axis is amplitude.
fn frequency_ruler(canvas: &ProcessingCanvas) -> impl IntoElement {
    let transform = canvas.plane_transform();
    let spectral = canvas.view.shows_spectrogram();
    let entries: Vec<(f32, String)> = if spectral {
        [20.0_f32, 100.0, 500.0, 1_000.0, 5_000.0, 10_000.0, 20_000.0]
            .into_iter()
            .filter(|hz| *hz <= transform.max_hz)
            .map(|hz| {
                let label = if hz >= 1000.0 {
                    format!("{:.0}k", hz / 1000.0)
                } else {
                    format!("{hz:.0}")
                };
                (transform.hz_to_y(hz), label)
            })
            .collect()
    } else {
        [
            (0.0, "+1.0".to_string()),
            (transform.height * 0.5, "0".to_string()),
            (transform.height - 1.0, "−1.0".to_string()),
        ]
        .into_iter()
        .collect()
    };
    div()
        .absolute()
        .left_0()
        .top_0()
        .w(px(FREQ_GUTTER))
        .h(px(transform.height))
        .bg(Colors::with_alpha(Colors::surface_canvas(), 0.55))
        .children(entries.into_iter().map(|(y, label)| {
            div()
                .absolute()
                .left(px(space::TIGHT))
                .top(px((y - 6.0).clamp(0.0, (transform.height - 12.0).max(0.0))))
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(label)
        }))
}

/// Time ruler under the plane. Tick spacing follows the zoom so labels never
/// collide, and every tick converts through the shared transform.
fn time_ruler(canvas: &ProcessingCanvas) -> impl IntoElement {
    let transform = canvas.transform;
    let seconds_per_pixel =
        transform.viewport.frames_per_pixel / transform.sample_rate.max(1) as f64;
    let target_seconds = seconds_per_pixel * 110.0;
    let step = [
        0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
    ]
    .into_iter()
    .find(|candidate| *candidate >= target_seconds)
    .unwrap_or(600.0);
    let (first_frame, last_frame) = transform.visible_frames();
    let sr = transform.sample_rate.max(1) as f64;
    let first_tick = ((first_frame as f64 / sr) / step).floor() * step;
    let last_second = last_frame as f64 / sr;
    let mut ticks = Vec::new();
    let mut second = first_tick;
    while second <= last_second && ticks.len() < 64 {
        let x = transform.frame_to_x(second * sr);
        if x >= 0.0 && x <= transform.width {
            ticks.push((x, format_seconds(second, step)));
        }
        second += step;
    }
    div()
        .absolute()
        .left_0()
        .bottom_0()
        .w(px(transform.width))
        .h(px(TIME_RULER_H))
        .bg(Colors::surface_panel_alt())
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .children(ticks.into_iter().map(|(x, label)| {
            div()
                .absolute()
                .left(px(x + 3.0))
                .top(px(1.0))
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(label)
        }))
}

fn format_seconds(seconds: f64, step: f64) -> String {
    let minutes = (seconds / 60.0).floor();
    let rest = seconds - minutes * 60.0;
    if step < 0.1 {
        format!("{minutes:.0}:{rest:06.3}")
    } else if step < 1.0 {
        format!("{minutes:.0}:{rest:04.1}")
    } else {
        format!("{minutes:.0}:{rest:02.0}")
    }
}

fn status_badge(text: String, is_error: bool) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .px(px(space::BASE))
                .py(px(space::TIGHT))
                .rounded(px(radius::CONTROL))
                .border(px(1.0))
                .border_color(if is_error {
                    Colors::status_error()
                } else {
                    Colors::border_subtle()
                })
                .bg(Colors::with_alpha(Colors::surface_panel(), 0.9))
                .text_size(px(typography::DENSE_LABEL))
                .text_color(if is_error {
                    Colors::status_error()
                } else {
                    Colors::text_muted()
                })
                .child(text),
        )
}

/// Render the canvas body. The caller owns the element's id, pointer handlers
/// and measured size; this function owns only what is drawn inside it.
pub fn processing_canvas(canvas: ProcessingCanvas) -> impl IntoElement {
    let status = canvas.status.clone();
    let status_is_error = canvas.status_is_error;
    let show_tiles = canvas.view.shows_spectrogram() && !canvas.tiles.is_empty();
    let mask = canvas
        .view
        .shows_spectrogram()
        .then(|| mask_layer(&canvas))
        .flatten();
    let tiles = show_tiles.then(|| tile_layer(&canvas));
    let ruler_freq = frequency_ruler(&canvas);
    let ruler_time = time_ruler(&canvas);
    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .bg(Colors::surface_canvas())
        .children(tiles)
        .children(mask)
        .child(div().absolute().inset_0().child(paint_layer(canvas)))
        .child(ruler_freq)
        .child(ruler_time)
        .children(status.map(|text| status_badge(text, status_is_error)))
}

/// Legend entry describing one overlay currently on the canvas.
pub fn canvas_legend(entries: Vec<(&'static str, gpui::Rgba)>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .children(entries.into_iter().map(|(label, color)| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(
                    div()
                        .w(px(8.0))
                        .h(px(8.0))
                        .rounded(px(radius::MICRO))
                        .bg(color),
                )
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(label),
                )
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform(width: f32) -> CanvasTransform {
        CanvasTransform {
            window_start_frame: 1_000,
            window_frames: 96_000,
            sample_rate: 48_000,
            max_hz: 24_000.0,
            frequency_scale: FrequencyScale::Logarithmic,
            viewport: CanvasViewport::fit(96_000, width),
            width,
            height: 400.0,
        }
    }

    #[test]
    fn time_and_frequency_round_trip_through_one_transform() {
        let t = transform(800.0);
        for frame in [0.0, 1_234.0, 95_999.0] {
            let x = t.frame_to_x(frame);
            assert!((t.x_to_frame(x) - frame).abs() < 1.0);
        }
        for hz in [50.0, 440.0, 8_000.0] {
            let y = t.hz_to_y(hz);
            assert!((t.y_to_hz(y) - hz).abs() / hz < 0.01, "{hz} -> {y}");
        }
    }

    #[test]
    fn source_frames_account_for_the_window_offset() {
        let t = transform(800.0);
        let x = t.source_frame_to_x(1_000);
        assert!(x.abs() < 0.5);
        assert_eq!(t.x_to_source_frame(0.0), 1_000);
    }

    #[test]
    fn zoom_keeps_the_frame_under_the_anchor() {
        let mut viewport = CanvasViewport::fit(96_000, 800.0);
        let mut t = transform(800.0);
        let anchor = 300.0;
        let before = t.x_to_frame(anchor);
        viewport.zoom_around(4.0, anchor, 96_000, 800.0);
        t.viewport = viewport;
        assert!((t.x_to_frame(anchor) - before).abs() < 1.0);
        assert!(viewport.frames_per_pixel < 96_000.0 / 800.0);
    }

    #[test]
    fn zoom_out_never_exceeds_the_window() {
        let mut viewport = CanvasViewport::fit(96_000, 800.0);
        viewport.zoom_around(0.01, 0.0, 96_000, 800.0);
        assert!(viewport.frames_per_pixel <= 96_000.0 / 800.0 + 1.0e-6);
        assert_eq!(viewport.start_frame, 0.0);
    }

    #[test]
    fn peak_columns_agree_between_the_mip_and_the_samples() {
        let mono: Arc<[f32]> = (0..200_000)
            .map(|i| ((i as f32) * 0.01).sin() * 0.8)
            .collect::<Vec<_>>()
            .into();
        let peaks = WaveformPeaks::build(Arc::clone(&mono), 1);
        let mut wide = transform(400.0);
        wide.window_frames = mono.len() as u64;
        wide.viewport = CanvasViewport::fit(mono.len() as u64, 400.0);
        let coarse = peaks.columns(&wide);
        assert_eq!(coarse.len(), 400);
        assert!(coarse.iter().any(|(lo, hi)| *lo < -0.5 && *hi > 0.5));

        let mut tight = wide;
        tight.viewport.frames_per_pixel = 1.0;
        let fine = peaks.columns(&tight);
        assert_eq!(fine.len(), 400);
        assert!(fine.iter().all(|(lo, hi)| *lo <= *hi));
    }
}
