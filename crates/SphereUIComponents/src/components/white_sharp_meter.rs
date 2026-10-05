//! WhiteSharp's live displays: the pitch correction meter and the
//! keyboard's sung and target notes.
//!
//! The DSP publishes its newest pitch readings through the per-insert level
//! block (`whitesharp::telemetry`); the editor window's root overlay polls it
//! once a frame and paints here, into bounds the cached panel recorded — so
//! a frame redraws the meter and the key lights and nothing else.
//!
//! The meter is drawn by wgpu off screen — the visualizers' renderer,
//! composited the same way (a shared texture on Windows, a read-back image
//! elsewhere). The key lights and the readout are GPUI quads and text over
//! the panel's keys, placed by the same [`key_frame`] the panel lays the
//! keys out with.

use gpui::{
    point, px, size, App, Bounds, Hsla, Pixels, Rgba, SharedString, TextAlign, TextRun, Window,
};

use crate::components::builtin_plugin_editor_window::{BuiltinPadLevelSource, PluginInstanceKey};
use crate::components::white_sharp_model::note_name;
use crate::theme::{typography, Colors};
use whitesharp::telemetry::{self, Reading};

/// The meter's reach either way, in cents.
pub const METER_CENTS: f32 = 100.0;
/// How fast the needle follows, per frame.
const FOLLOW: f32 = 0.35;
/// How fast it falls back to centre once the voice stops, per frame.
const RELEASE: f32 = 0.12;
/// Readings a pause in the voice may last before the lights go out.
const RECENT: usize = 6;

/// Where pitch class `class` sits on a one-octave keyboard: its left edge
/// and width as fractions of the keyboard's width, and whether it is a
/// black key (drawn shorter, on top).
pub fn key_frame(class: u8) -> (f32, f32, bool) {
    const WHITE: f32 = 1.0 / 7.0;
    const BLACK: f32 = WHITE * 0.62;
    // White keys by their index along the octave; black keys centred on
    // the boundary they straddle.
    match class % 12 {
        0 => (0.0, WHITE, false),
        2 => (WHITE, WHITE, false),
        4 => (2.0 * WHITE, WHITE, false),
        5 => (3.0 * WHITE, WHITE, false),
        7 => (4.0 * WHITE, WHITE, false),
        9 => (5.0 * WHITE, WHITE, false),
        11 => (6.0 * WHITE, WHITE, false),
        1 => (WHITE - BLACK * 0.5, BLACK, true),
        3 => (2.0 * WHITE - BLACK * 0.5, BLACK, true),
        6 => (4.0 * WHITE - BLACK * 0.5, BLACK, true),
        8 => (5.0 * WHITE - BLACK * 0.5, BLACK, true),
        _ => (6.0 * WHITE - BLACK * 0.5, BLACK, true),
    }
}

/// A black key's height, as a share of the keyboard's.
pub const BLACK_KEY_HEIGHT: f32 = 0.6;

/// The newest sung reading, if the voice is sounding.
fn newest_voiced(readings: &[Reading; telemetry::POINTS]) -> Option<Reading> {
    readings
        .iter()
        .rev()
        .take(RECENT)
        .find(|reading| reading.input.is_some())
        .copied()
}

/// The live displays' state across frames.
pub struct LiveDisplay {
    source: Option<BuiltinPadLevelSource>,
    key: Option<PluginInstanceKey>,
    block_seq: Option<u32>,
    /// What the voice is doing now.
    pub reading: Option<Reading>,
    /// Where the needle is, in cents.
    pub needle: f32,
    #[cfg(feature = "gpu-renderer")]
    presenter: presenter::Presenter,
}

impl Default for LiveDisplay {
    fn default() -> Self {
        Self {
            source: None,
            key: None,
            block_seq: None,
            reading: None,
            needle: 0.0,
            #[cfg(feature = "gpu-renderer")]
            presenter: presenter::Presenter::default(),
        }
    }
}

impl LiveDisplay {
    /// Points the displays at an insert's published readings.
    pub fn bind(&mut self, key: PluginInstanceKey, source: Option<BuiltinPadLevelSource>) {
        if self.key.as_ref() != Some(&key) {
            self.reading = None;
            self.needle = 0.0;
            self.block_seq = None;
        }
        self.key = Some(key);
        self.source = source;
    }

    /// Shows `reading` as if the DSP had just published it — the UI
    /// preview's synthetic voice.
    pub fn show(&mut self, reading: Reading) {
        self.reading = Some(reading);
        if let (Some(input), Some(output)) = (reading.input, reading.output) {
            self.needle = ((output - input) * 100.0).clamp(-METER_CENTS, METER_CENTS);
        }
    }

    /// Takes the newest block, and moves the needle a frame's worth.
    fn poll(&mut self) {
        if let (Some(source), Some(key)) = (self.source.as_ref(), self.key.as_ref()) {
            if let Some((seq, block)) = source(key) {
                if self.block_seq != Some(seq) {
                    self.block_seq = Some(seq);
                    if let Some(slots) = block.first_chunk::<{ telemetry::SLOTS }>() {
                        let (_, readings) = telemetry::decode(slots);
                        self.reading = newest_voiced(&readings);
                    }
                }
            }
        }
        let wanted = self
            .reading
            .and_then(|reading| Some((reading.output? - reading.input?) * 100.0))
            .map(|cents| cents.clamp(-METER_CENTS, METER_CENTS));
        self.needle += match wanted {
            Some(cents) => (cents - self.needle) * FOLLOW,
            None => -self.needle * RELEASE,
        };
    }

    /// Polls, then paints the meter into `meter` and the key lights onto the
    /// keyboard at `keyboard`.
    pub fn paint(
        &mut self,
        meter: Option<Bounds<Pixels>>,
        keyboard: Option<Bounds<Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.poll();
        if let Some(bounds) = meter {
            self.paint_meter(bounds, window, cx);
        }
        if let Some(bounds) = keyboard {
            self.paint_keys(bounds, window);
        }
    }

    fn paint_meter(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if w < 8.0 || h < 8.0 {
            return;
        }
        #[cfg(feature = "gpu-renderer")]
        {
            let scale = window.scale_factor();
            let scene = meter_scene(w * scale, h * scale, scale, self.needle);
            if let Err(error) = self.presenter.present(&scene, bounds, window, cx) {
                paint_text(
                    window,
                    cx,
                    &format!("Meter unavailable: {error}"),
                    Colors::text_muted().into(),
                    bounds.origin + point(px(8.0), px(h * 0.5 - 8.0)),
                );
            }
        }
        #[cfg(not(feature = "gpu-renderer"))]
        paint_text(
            window,
            cx,
            "The correction meter needs the GPU renderer",
            Colors::text_muted().into(),
            bounds.origin + point(px(8.0), px(h * 0.5 - 8.0)),
        );
    }

    /// The note the voice is pulled to washed in the accent, with a bar;
    /// the sung note, where it differs, ringed.
    fn paint_keys(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let Some(reading) = self.reading else {
            return;
        };
        let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let key_rect = |class: u8| {
            let (left, width, black) = key_frame(class);
            let height = if black { h * BLACK_KEY_HEIGHT } else { h };
            (x0 + left * w, y0, width * w, height)
        };
        let target = reading.target.map(|note| note.rem_euclid(12) as u8);
        if let Some(class) = target {
            let (x, y, kw, kh) = key_rect(class);
            // A white key shows only below the black keys; washing all of
            // it would tint its neighbours.
            let top = if key_frame(class).2 {
                y
            } else {
                y + h * BLACK_KEY_HEIGHT
            };
            window.paint_quad(gpui::fill(
                Bounds::new(point(px(x), px(top)), size(px(kw), px(y + kh - top))),
                Colors::with_alpha(Colors::accent_primary(), 0.45),
            ));
            window.paint_quad(gpui::fill(
                Bounds::new(
                    point(px(x + 3.0), px(y + kh - 6.0)),
                    size(px((kw - 6.0).max(2.0)), px(3.0)),
                ),
                Colors::accent_primary(),
            ));
        }
        if let Some(input) = reading.input {
            let class = input.round().rem_euclid(12.0) as u8;
            if Some(class) != target {
                let (x, y, kw, kh) = key_rect(class);
                let ring = Colors::accent_primary();
                for edge in [
                    Bounds::new(point(px(x), px(y)), size(px(kw), px(2.0))),
                    Bounds::new(point(px(x), px(y + kh - 2.0)), size(px(kw), px(2.0))),
                    Bounds::new(point(px(x), px(y)), size(px(2.0), px(kh))),
                    Bounds::new(point(px(x + kw - 2.0), px(y)), size(px(2.0), px(kh))),
                ] {
                    window.paint_quad(gpui::fill(edge, ring));
                }
            }
        }
    }
    /// What is being sung against what it is pulled to, for the meter's
    /// title row.
    pub fn readout(&self) -> Option<String> {
        let reading = self.reading?;
        let input = reading.input?;
        let nearest = input.round() as i32;
        let cents = ((input - nearest as f32) * 100.0).round();
        Some(match reading.target {
            Some(target) => format!(
                "{} {cents:+.0}¢  →  {}",
                note_name(nearest),
                note_name(target)
            ),
            None => format!("{} {cents:+.0}¢", note_name(nearest)),
        })
    }
}

/// One line of text at `origin`.
pub fn paint_text(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    color: Hsla,
    origin: gpui::Point<Pixels>,
) {
    let font = window.text_style().font();
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(typography::UI_SM),
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
    let _ = line.paint(origin, px(16.0), TextAlign::Left, None, window, cx);
}

#[cfg(feature = "gpu-renderer")]
use crate::components::visualizer::scene::{Scene, Vertex};

#[cfg(feature = "gpu-renderer")]
fn rgba(color: Rgba) -> [f32; 4] {
    [color.r, color.g, color.b, color.a]
}

#[cfg(feature = "gpu-renderer")]
fn quad(tris: &mut Vec<Vertex>, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
    if w <= 0.0 || h <= 0.0 || color.a <= 0.0 {
        return;
    }
    let c = rgba(color);
    let (x1, y1) = (x + w, y + h);
    for position in [[x, y], [x1, y], [x1, y1], [x, y], [x1, y1], [x, y1]] {
        tris.push(Vertex { position, color: c });
    }
}

/// The meter, in physical pixels: a scale of ticks every ten cents, a bar
/// from the centre to the correction, and the needle at its end. Left of
/// centre the voice is pulled down, right of it up.
#[cfg(feature = "gpu-renderer")]
fn meter_scene(w: f32, h: f32, scale: f32, needle: f32) -> Scene {
    let ink = Colors::text_primary();
    let accent = Colors::accent_primary();
    let mut scene = Scene {
        clear: Some(rgba(Colors::surface_canvas())),
        ..Default::default()
    };
    let t = &mut scene.triangles;
    let inset = 10.0 * scale;
    let (left, right) = (inset, w - inset);
    let span = right - left;
    let mid = left + span * 0.5;
    let x_of = |cents: f32| mid + cents / METER_CENTS * span * 0.5;
    let line = scale.max(1.0);

    // The bar's channel.
    let channel_y = h * 0.42;
    let channel_h = h * 0.30;
    quad(
        t,
        left,
        channel_y,
        span,
        channel_h,
        Colors::with_alpha(ink, 0.06),
    );

    // Ticks: every ten cents, longer every fifty, the centre longest.
    let mut cents = -METER_CENTS;
    while cents <= METER_CENTS + 0.5 {
        let major = (cents % 50.0).abs() < 0.5;
        let centre = cents.abs() < 0.5;
        let (top, alpha) = if centre {
            (h * 0.10, 0.55)
        } else if major {
            (h * 0.20, 0.32)
        } else {
            (h * 0.30, 0.16)
        };
        quad(
            t,
            x_of(cents) - line * 0.5,
            top,
            line,
            channel_y - top - 2.0 * scale,
            Colors::with_alpha(ink, alpha),
        );
        cents += 10.0;
    }

    // The correction: a bar from the centre, its depth in the accent,
    // and a needle at its end.
    let end = x_of(needle);
    let (from, to) = if end < mid { (end, mid) } else { (mid, end) };
    quad(
        t,
        from,
        channel_y,
        to - from,
        channel_h,
        Colors::with_alpha(accent, 0.55),
    );
    quad(
        t,
        end - line,
        channel_y - 4.0 * scale,
        2.0 * line,
        channel_h + 8.0 * scale,
        accent,
    );
    quad(
        t,
        mid - line * 0.5,
        channel_y,
        line,
        channel_h,
        Colors::with_alpha(ink, 0.6),
    );
    scene
}

#[cfg(feature = "gpu-renderer")]
mod presenter {
    //! Renders a scene off screen with the visualizers' wgpu renderer and
    //! puts the result in the window.

    use std::sync::Arc;

    use gpui::{point, size, App, Bounds, Corners, DevicePixels, Pixels, RenderImage, Window};
    use smallvec::SmallVec;

    use crate::components::visualizer::gpu::{self, Presented, Surface};
    use crate::components::visualizer::scene::Scene;

    #[derive(Default)]
    pub struct Presenter {
        surface: Option<Surface>,
        /// Shared textures failed to import once; read back from now on.
        readback_only: bool,
        error: Option<String>,
        #[cfg(target_os = "windows")]
        external: Option<Arc<gpui::D3D11ExternalImage>>,
        image: Option<Arc<RenderImage>>,
        stale: Vec<Arc<RenderImage>>,
    }

    impl Presenter {
        pub fn present(
            &mut self,
            scene: &Scene,
            bounds: Bounds<Pixels>,
            window: &mut Window,
            cx: &mut App,
        ) -> Result<(), String> {
            for image in self.stale.drain(..) {
                cx.drop_image(image, Some(window));
            }
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            let scale = window.scale_factor();
            let (pw, ph) = (
                (f32::from(bounds.size.width) * scale).round().max(1.0) as u32,
                (f32::from(bounds.size.height) * scale).round().max(1.0) as u32,
            );
            if self.surface.is_none() {
                #[cfg(target_os = "windows")]
                let luid = window.d3d11_adapter_luid();
                #[cfg(not(target_os = "windows"))]
                let luid = None;
                let readback_only = self.readback_only;
                match gpu::context(luid).and_then(|context| {
                    let shared = context.shares_textures() && !readback_only;
                    Surface::new(context, pw, ph, shared)
                }) {
                    Ok(surface) => self.surface = Some(surface),
                    Err(error) => {
                        eprintln!("[whitesharp] GPU unavailable: {error}");
                        self.error = Some(error.clone());
                        return Err(error);
                    }
                }
            }
            let surface = self.surface.as_mut().expect("created above");
            surface.resize(pw, ph)?;
            match surface.render(scene)? {
                #[cfg(target_os = "windows")]
                Presented::Shared {
                    handle,
                    width,
                    height,
                } => {
                    let external = self
                        .external
                        .get_or_insert_with(|| window.create_d3d11_external_image())
                        .clone();
                    match external.update_from_shared_texture(
                        handle,
                        point(DevicePixels(0), DevicePixels(0)),
                        size(DevicePixels(width as i32), DevicePixels(height as i32)),
                    ) {
                        Ok(()) => {
                            let _ = window.paint_d3d11_external_image(
                                bounds,
                                Corners::default(),
                                external,
                            );
                        }
                        Err(error) => {
                            // Keep drawing: the next frame reads back.
                            eprintln!("[whitesharp] shared texture import failed: {error}");
                            self.readback_only = true;
                            self.surface = None;
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                Presented::Shared { .. } => {}
                Presented::Pixels {
                    bgra,
                    width,
                    height,
                } => {
                    let Some(buffer) = image::ImageBuffer::from_raw(width, height, bgra) else {
                        return Ok(());
                    };
                    let next = Arc::new(RenderImage::new(SmallVec::from_elem(
                        image::Frame::new(buffer),
                        1,
                    )));
                    if let Some(previous) = self.image.replace(next.clone()) {
                        self.stale.push(previous);
                    }
                    let _ = window.paint_image(bounds, Corners::default(), next, 0, false);
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keys_tile_one_octave() {
        let mut whites: Vec<(f32, f32)> = (0..12u8)
            .map(key_frame)
            .filter(|(_, _, black)| !black)
            .map(|(left, width, _)| (left, width))
            .collect();
        whites.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(whites.len(), 7);
        let mut edge = 0.0;
        for (left, width) in whites {
            assert!((left - edge).abs() < 1.0e-6);
            edge = left + width;
        }
        assert!((edge - 1.0).abs() < 1.0e-6);
        // Each black key straddles the boundary between its neighbours.
        let (left, width, black) = key_frame(1);
        assert!(black && left < 1.0 / 7.0 && left + width > 1.0 / 7.0);
    }

    #[test]
    fn the_newest_voiced_reading_wins_and_a_pause_clears_it() {
        let mut history = whitesharp::telemetry::History::default();
        history.push(Reading {
            input: Some(60.2),
            output: Some(60.0),
            target: Some(60),
        });
        let (_, readings) = telemetry::decode(&history.encode());
        assert_eq!(newest_voiced(&readings).and_then(|r| r.target), Some(60));
        for _ in 0..RECENT {
            history.push(Reading::SILENT);
        }
        let (_, readings) = telemetry::decode(&history.encode());
        assert_eq!(newest_voiced(&readings), None);
    }
}
