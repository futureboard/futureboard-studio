//! The picture-in-picture shell a visualizer lives in.
//!
//! A small borderless window that floats above everything (`Floating`) and
//! can be dragged from anywhere on it. Every view shares one frame: a title
//! bar (title, a quiet line of view facts, close), one recessed panel with
//! the picture, the panel's value axis in a gutter to its right, its time or
//! frequency axis in a gutter below, and readouts on the window's own
//! surface. See [`Layout`].
//!
//! The picture is drawn by wgpu ([`super::gpu`]) and composited by GPUI; the
//! labels, readouts and controls are GPUI elements laid over it with the same
//! [`Layout`] the picture was built with.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, point, px, size, svg, App, AppContext, Bounds, Context, Corners, DevicePixels,
    FontFeatures, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    RenderImage, Rgba, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowControlArea,
    WindowHandle, WindowKind,
};
use smallvec::SmallVec;

use super::analysis::{Analyzer, SCOPE_WINDOW_SECONDS, SLOPE_DB_PER_OCTAVE};
use super::gpu::{self, Presented, Surface};
use super::scene::{
    self, is_major_freq, Layout, Palette, Rect, ViewState, FREQ_TICKS, LOUDNESS_GRID,
    LOUDNESS_RESET_H, LOUDNESS_TARGET, LOUDNESS_TIME_GRID, READOUT_CAPTION, READOUT_VALUE,
    SCOPE_DIVISION_MS, SPECTRUM_GRID_DB, STEREO_METER_CAPTION, TICK_H, TITLE_BAR,
};
use super::VisualizerKind;
use crate::components::title_bar::begin_titlebar_drag;
use crate::theme::{radius, space, typography, Colors};

/// The size a view opens at.
fn default_size(kind: VisualizerKind) -> (f32, f32) {
    match kind {
        VisualizerKind::Loudness => (460.0, 260.0),
        _ => (420.0, 260.0),
    }
}

/// The smallest a view's window goes: the loudness view's readout column
/// needs the height for its rows and the width beside its graph.
pub fn min_size(kind: VisualizerKind) -> (f32, f32) {
    match kind {
        VisualizerKind::Loudness => (320.0, 216.0),
        _ => (240.0, 180.0),
    }
}

/// Where the audio comes from.
enum Source {
    /// The master bus tap, as Studio runs.
    Tap { cursor: u64, listening: bool },
    /// The UI preview's test signal, generated a display frame at a time.
    #[cfg(feature = "ui-preview")]
    Signal(Box<dyn FnMut(&mut [f32], &mut [f32])>),
}

/// Everything that changes every frame, owned outside the view so the paint
/// closure can drive it without re-entering the view.
struct Engine {
    kind: VisualizerKind,
    analyzer: Analyzer,
    source: Source,
    last_frame: Option<Instant>,
    view: ViewState,
    palette: Palette,
    lut: [[u8; 4]; 256],
    trace: Vec<f32>,
    surface: Option<Surface>,
    /// Shared textures failed to import once; this window reads back.
    readback_only: bool,
    gpu_error: Option<String>,
    #[cfg(target_os = "windows")]
    external: Option<Arc<gpui::D3D11ExternalImage>>,
    image: Option<Arc<RenderImage>>,
    stale: Vec<Arc<RenderImage>>,
    /// Preview: spectrogram history analysed before the surface existed.
    #[cfg(feature = "ui-preview")]
    pending_columns: Vec<[[u8; 4]; super::analysis::DISPLAY_BINS]>,
}

impl Engine {
    fn new(kind: VisualizerKind) -> Self {
        let palette = Palette::from_theme();
        Self {
            kind,
            analyzer: Analyzer::new(kind.features()),
            source: Source::Tap {
                cursor: 0,
                listening: false,
            },
            last_frame: None,
            view: ViewState::default(),
            lut: scene::spectrogram_lut(&palette),
            palette,
            trace: Vec::new(),
            surface: None,
            readback_only: false,
            gpu_error: None,
            #[cfg(target_os = "windows")]
            external: None,
            image: None,
            stale: Vec::new(),
            #[cfg(feature = "ui-preview")]
            pending_columns: Vec::new(),
        }
    }

    /// Preview: an engine fed by `signal` instead of the tap, already run
    /// long enough that every view has history to show (the loudness graph's
    /// 30 s, the spectrogram's full width). Reads back, so the picture lands
    /// in the frame the preview copies.
    #[cfg(feature = "ui-preview")]
    fn with_signal(
        kind: VisualizerKind,
        mut signal: Box<dyn FnMut(&mut [f32], &mut [f32])>,
    ) -> Self {
        let mut engine = Self::new(kind);
        engine.readback_only = true;
        let dt = 1.0 / 60.0;
        let (mut left, mut right) = (vec![0.0; 800], vec![0.0; 800]);
        let frames = match kind {
            VisualizerKind::Loudness => 32 * 60,
            VisualizerKind::Spectrogram => gpu::SPECTROGRAM_COLUMNS as usize,
            _ => 90,
        };
        for _ in 0..frames {
            signal(&mut left, &mut right);
            engine.analyzer.feed(&left, &right, dt);
            if kind == VisualizerKind::StereoImage {
                engine.view.scope_gain =
                    scene::next_scope_gain(engine.view.scope_gain, &engine.analyzer, dt);
            }
            if let Some(column) = engine.analyzer.take_spectrogram_column() {
                let lut = &engine.lut;
                engine.pending_columns.push(
                    column.map(|level| lut[(level * 255.0).round().clamp(0.0, 255.0) as usize]),
                );
            }
        }
        engine.source = Source::Signal(signal);
        engine
    }

    /// Bring the analysis up to date with whatever arrived since last frame.
    fn pull(&mut self, dt: f32) {
        match &mut self.source {
            Source::Tap { cursor, listening } => {
                if !*listening {
                    *cursor = DirectAudio::visualizer_tap().listen();
                    *listening = true;
                }
                *cursor = self.analyzer.pull(*cursor, dt);
            }
            #[cfg(feature = "ui-preview")]
            Source::Signal(signal) => {
                let frames = ((self.analyzer.sample_rate() as f32 * dt) as usize).clamp(1, 4_800);
                let (mut left, mut right) = (vec![0.0; frames], vec![0.0; frames]);
                signal(&mut left, &mut right);
                self.analyzer.feed(&left, &right, dt);
            }
        }
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        for image in self.stale.drain(..) {
            cx.drop_image(image, Some(window));
        }
        let now = Instant::now();
        let dt = self
            .last_frame
            .map(|last| now.duration_since(last).as_secs_f32())
            .unwrap_or(1.0 / 60.0);
        self.last_frame = Some(now);
        self.pull(dt);
        match self.kind {
            VisualizerKind::StereoImage => {
                self.view.scope_gain =
                    scene::next_scope_gain(self.view.scope_gain, &self.analyzer, dt)
            }
            VisualizerKind::Oscilloscope => self.analyzer.oscilloscope(&mut self.trace),
            _ => {}
        }

        let scale = window.scale_factor();
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let (pw, ph) = (
            (w * scale).round().max(1.0) as u32,
            (h * scale).round().max(1.0) as u32,
        );
        if self.gpu_error.is_some() {
            return;
        }
        if self.surface.is_none() {
            #[cfg(target_os = "windows")]
            let luid = window.d3d11_adapter_luid();
            #[cfg(not(target_os = "windows"))]
            let luid = None;
            match gpu::context(luid).and_then(|context| {
                let shared = context.shares_textures() && !self.readback_only;
                Surface::new(context, pw, ph, shared)
            }) {
                Ok(surface) => self.surface = Some(surface),
                Err(error) => {
                    eprintln!("[visualizer] GPU unavailable: {error}");
                    self.gpu_error = Some(error);
                    return;
                }
            }
        }
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if let Err(error) = surface.resize(pw, ph) {
            self.gpu_error = Some(error);
            return;
        }
        #[cfg(feature = "ui-preview")]
        for texels in self.pending_columns.drain(..) {
            surface.push_spectrogram_column(&texels);
        }
        if self.kind == VisualizerKind::Spectrogram {
            if let Some(column) = self.analyzer.take_spectrogram_column() {
                let texels = column
                    .map(|level| self.lut[(level * 255.0).round().clamp(0.0, 255.0) as usize]);
                surface.push_spectrogram_column(&texels);
            }
        }
        let layout = Layout::new(self.kind, w, h);
        let frame = scene::build(
            &layout,
            scale,
            &self.analyzer,
            &self.view,
            &self.palette,
            &self.trace,
        );
        let presented = match surface.render(&frame) {
            Ok(presented) => presented,
            Err(error) => {
                self.gpu_error = Some(error);
                return;
            }
        };
        match presented {
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
                let imported = external.update_from_shared_texture(
                    handle,
                    point(DevicePixels(0), DevicePixels(0)),
                    size(DevicePixels(width as i32), DevicePixels(height as i32)),
                );
                match imported {
                    Ok(()) => {
                        let _ =
                            window.paint_d3d11_external_image(bounds, Corners::default(), external);
                    }
                    Err(error) => {
                        // Keep drawing: the next frame reads back instead.
                        eprintln!(
                            "[visualizer] shared texture import failed, reading back: {error}"
                        );
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
                    return;
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
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Source::Tap {
            listening: true, ..
        } = self.source
        {
            DirectAudio::visualizer_tap().unlisten();
        }
    }
}

/// The few composited state colours the controls need, resolved once rather
/// than on every frame's render.
#[derive(Clone, Copy)]
struct ControlPaint {
    close_hover: Rgba,
    close_pressed: Rgba,
    button: Rgba,
    button_hover: Rgba,
    button_pressed: Rgba,
}

impl ControlPaint {
    fn from_theme() -> Self {
        let chrome = Colors::surface_window();
        let button = Colors::button_bg();
        Self {
            close_hover: Colors::composite(chrome, Colors::state_hover()),
            close_pressed: Colors::composite(chrome, Colors::state_recessed()),
            button,
            button_hover: Colors::composite(button, Colors::state_hover()),
            button_pressed: Colors::composite(button, Colors::state_recessed()),
        }
    }
}

pub struct VisualizerWindow {
    kind: VisualizerKind,
    engine: Rc<RefCell<Engine>>,
    paint: ControlPaint,
    on_close: Arc<dyn Fn(VisualizerKind, &mut App) + Send + Sync>,
}

impl VisualizerWindow {
    fn new(
        kind: VisualizerKind,
        engine: Engine,
        on_close: Arc<dyn Fn(VisualizerKind, &mut App) + Send + Sync>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let handle = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_window, cx| {
            if let Some(view) = handle.upgrade() {
                let (kind, on_close) = {
                    let view = view.read(cx);
                    (view.kind, view.on_close.clone())
                };
                on_close(kind, cx);
            }
            true
        });
        Self {
            kind,
            engine: Rc::new(RefCell::new(engine)),
            paint: ControlPaint::from_theme(),
            on_close,
        }
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        (self.on_close)(self.kind, cx);
        window.remove_window();
    }
}

impl Render for VisualizerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A meter redraws every frame for as long as it is open.
        window.request_animation_frame();
        let bounds = window.viewport_size();
        let (w, h) = (f32::from(bounds.width), f32::from(bounds.height));
        let layout = Layout::new(self.kind, w, h);
        let engine = self.engine.clone();
        let gpu_error = engine.borrow().gpu_error.clone();
        let (overlay, meta) = overlay(self.kind, &layout, &engine.borrow(), self.paint, cx);

        div()
            .id("visualizer-root")
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(Colors::surface_window())
            .text_color(Colors::text_muted())
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| engine.borrow_mut().paint(bounds, window, cx),
                )
                .absolute()
                .size_full(),
            )
            // Drag from anywhere on the picture; the controls occlude it.
            .child(
                div()
                    .id("visualizer-drag")
                    .absolute()
                    .size_full()
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(gpui::MouseButton::Left, begin_titlebar_drag),
            )
            .child(overlay)
            .when_some(gpu_error, |root, error| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(space::SECTION))
                        .text_size(px(typography::UI_XS))
                        .child(format!("The GPU renderer is unavailable: {error}")),
                )
            })
            .child(self.title_bar(meta, cx))
    }
}

impl VisualizerWindow {
    /// Title, the view's facts and close across the top: part of the
    /// window's surface, draggable like the rest of it. The title lines up
    /// with the panel's left edge.
    fn title_bar(&self, meta: gpui::AnyElement, cx: &mut Context<Self>) -> impl IntoElement {
        let paint = self.paint;
        div()
            .id("visualizer-title-bar")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TITLE_BAR))
            .flex()
            .items_center()
            .gap(px(space::BASE))
            .pl(px(scene::EDGE))
            .pr(px(space::TIGHT))
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(gpui::MouseButton::Left, begin_titlebar_drag)
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(typography::UI_SM))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Colors::text_secondary())
                    .child(self.kind.title()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .child(meta),
            )
            .child(
                div()
                    .id("visualizer-close")
                    .group("visualizer-close")
                    .occlude()
                    .flex_shrink_0()
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(radius::CONTROL_SM))
                    .cursor_pointer()
                    .hover(move |style| style.bg(paint.close_hover))
                    .active(move |style| style.bg(paint.close_pressed))
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx)))
                    .child(
                        svg()
                            .path(crate::assets::ICON_CLOSE_SMALL_PATH)
                            .size(px(12.0))
                            .text_color(Colors::text_muted())
                            .group_hover("visualizer-close", |style| {
                                style.text_color(Colors::text_primary())
                            }),
                    ),
            )
    }
}

fn tabular() -> FontFeatures {
    static FEATURES: OnceLock<FontFeatures> = OnceLock::new();
    FEATURES
        .get_or_init(|| FontFeatures(Arc::new(vec![("tnum".to_string(), 1)])))
        .clone()
}

/// Numbers with a real minus sign, which lines up with the plus and the
/// figures where a hyphen does not.
fn minus(text: String) -> String {
    text.replace('-', "\u{2212}")
}

fn number(value: f32, decimals: usize) -> String {
    minus(format!("{value:.decimals$}"))
}

fn freq_text(hz: f32) -> String {
    if hz >= 1_000.0 {
        format!("{}k", hz / 1_000.0)
    } else {
        format!("{hz}")
    }
}

/// Small print: axis ticks, captions, units.
fn small(text: impl Into<String>) -> gpui::Div {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_faint())
        .font_features(tabular())
        .whitespace_nowrap()
        .child(text.into())
}

/// How wide a run of small print is, near enough to keep ticks apart.
fn small_width(text: &str) -> f32 {
    text.chars().count() as f32 * typography::DENSE_CAPTION * 0.6 + 2.0
}

/// Ticks along one axis, placed in priority order and dropped where they
/// would collide with one already placed, so a small window thins its axes
/// instead of overprinting them.
#[derive(Default)]
struct Spacing(SmallVec<[(f32, f32); 12]>);

impl Spacing {
    fn claim(&mut self, start: f32, end: f32) -> bool {
        let gap = space::TIGHT;
        if self.0.iter().any(|(a, b)| start < b + gap && end > a - gap) {
            return false;
        }
        self.0.push((start, end));
        true
    }
}

/// The value axis: labels in the gutter right of the panel, each centred on
/// its line and kept inside the panel's height.
fn value_ticks(
    layout: &Layout,
    ticks: impl IntoIterator<Item = (String, f32)>,
    unit: &str,
) -> Vec<gpui::AnyElement> {
    let p = layout.panel;
    let mut spacing = Spacing::default();
    let mut out = Vec::new();
    for (text, y) in ticks {
        let top = (y - TICK_H / 2.0).clamp(p.y - 1.0, p.bottom() - TICK_H + 1.0);
        if !spacing.claim(top, top + TICK_H) {
            continue;
        }
        out.push(
            small(text)
                .absolute()
                .left(px(p.right() + space::SNUG))
                .top(px(top))
                .h(px(TICK_H))
                .flex()
                .items_center()
                .into_any_element(),
        );
    }
    // The axis's unit sits where the two gutters meet, under its figures.
    if !unit.is_empty() {
        out.push(
            small(unit.to_string())
                .absolute()
                .left(px(p.right() + space::SNUG))
                .top(px(p.bottom() + 3.0))
                .h(px(TICK_H))
                .flex()
                .items_center()
                .into_any_element(),
        );
    }
    out
}

#[derive(Clone, Copy)]
enum Anchor {
    Start,
    Centre,
    End,
}

/// The time or frequency axis: labels in the gutter under the panel. The
/// first and last may anchor to the panel's ends rather than centre on them.
fn axis_ticks(
    layout: &Layout,
    ticks: impl IntoIterator<Item = (String, f32, Anchor)>,
) -> Vec<gpui::AnyElement> {
    let p = layout.panel;
    let mut spacing = Spacing::default();
    let mut out = Vec::new();
    for (text, x, anchor) in ticks {
        let width = small_width(&text);
        let start = match anchor {
            Anchor::Start => x,
            Anchor::Centre => x - width / 2.0,
            Anchor::End => x - width,
        };
        if start < p.x - 1.0 || start + width > p.right() + 1.0 {
            continue;
        }
        if !spacing.claim(start, start + width) {
            continue;
        }
        let slot = 64.0;
        let left = match anchor {
            Anchor::Start => x,
            Anchor::Centre => x - slot / 2.0,
            Anchor::End => x - slot,
        };
        out.push(
            small(text)
                .absolute()
                .left(px(left))
                .w(px(slot))
                .top(px(p.bottom() + 3.0))
                .h(px(TICK_H))
                .flex()
                .items_center()
                .map(|row| match anchor {
                    Anchor::Start => row.justify_start(),
                    Anchor::Centre => row.justify_center(),
                    Anchor::End => row.justify_end(),
                })
                .into_any_element(),
        );
    }
    out
}

/// Frequencies on a log axis, decades first so they survive a narrow window.
fn freq_ticks() -> impl Iterator<Item = f32> {
    let ticks = &FREQ_TICKS[..FREQ_TICKS.len() - 1];
    let majors = ticks.iter().copied().filter(|hz| is_major_freq(*hz));
    let minors = ticks.iter().copied().filter(|hz| !is_major_freq(*hz));
    majors.chain(minors)
}

/// A line of the title bar's facts: a quiet caption and its value.
fn meta_pair(caption: &str, value: String, color: Rgba) -> gpui::Div {
    div()
        .flex()
        .items_baseline()
        .gap(px(space::TIGHT))
        .whitespace_nowrap()
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_faint())
                .child(caption.to_string()),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_features(tabular())
                .text_color(color)
                .child(value),
        )
}

fn meta_row() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(space::LOOSE))
        .overflow_hidden()
}

fn tilt_meta() -> gpui::Div {
    meta_row().child(meta_pair(
        "Tilt",
        format!("+{SLOPE_DB_PER_OCTAVE} dB/oct"),
        Colors::text_muted(),
    ))
}

/// A loudness reading, or a dash before there is one.
fn reading(value: Option<f32>, decimals: usize) -> Option<String> {
    value.map(|v| number(v, decimals))
}

/// Labels, readouts and the title bar's facts over the picture, placed with
/// the picture's layout.
fn overlay(
    kind: VisualizerKind,
    layout: &Layout,
    engine: &Engine,
    paint: ControlPaint,
    cx: &mut Context<VisualizerWindow>,
) -> (gpui::AnyElement, gpui::AnyElement) {
    let analyzer = &engine.analyzer;
    let p = layout.plot;
    let mut root = div().absolute().inset_0();
    let meta: gpui::Div;
    match kind {
        VisualizerKind::Spectrum => {
            meta = tilt_meta();
            root = root
                .children(axis_ticks(
                    layout,
                    freq_ticks().map(|hz| (freq_text(hz), layout.freq_x(hz), Anchor::Centre)),
                ))
                .children(value_ticks(
                    layout,
                    SPECTRUM_GRID_DB
                        .iter()
                        .map(|db| (number(*db, 0), layout.spectrum_y(*db))),
                    "dB",
                ));
        }
        VisualizerKind::Spectrogram => {
            meta = tilt_meta();
            root = root
                .children(value_ticks(
                    layout,
                    freq_ticks().map(|hz| (freq_text(hz), layout.freq_y(hz))),
                    "Hz",
                ))
                .children(axis_ticks(
                    layout,
                    [("now".to_string(), p.right(), Anchor::End)],
                ));
        }
        VisualizerKind::StereoImage => {
            meta = meta_row().child(meta_pair(
                "Zoom",
                format!("×{:.1}", engine.view.scope_gain),
                Colors::text_muted(),
            ));
            let (cx0, base, r) = (p.x + p.w / 2.0, p.bottom(), p.h);
            let d = r * std::f32::consts::FRAC_1_SQRT_2;
            let at = |text: &str, x: f32, y: f32| {
                small(text.to_string())
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .h(px(TICK_H))
                    .flex()
                    .items_center()
            };
            root = root
                .child(at("L", cx0 - d - 10.0, base - d - TICK_H))
                .child(at("R", cx0 + d + 4.0, base - d - TICK_H))
                .child(at("M", cx0 - 3.0, p.y - TICK_H - 2.0));

            let correlation = analyzer.live.then(|| analyzer.correlation);
            let balance = if !analyzer.live {
                None
            } else if analyzer.balance_db.abs() < 0.05 {
                Some("C".to_string())
            } else if analyzer.balance_db > 0.0 {
                Some(format!("R {:.1} dB", analyzer.balance_db))
            } else {
                Some(format!("L {:.1} dB", -analyzer.balance_db))
            };
            let meters = layout.stereo_meters();
            let rows = [
                (
                    "CORRELATION",
                    correlation.map(|c| minus(format!("{c:+.2}"))),
                    if correlation.is_some_and(|c| c < 0.0) {
                        Colors::accent_warning()
                    } else {
                        Colors::text_primary()
                    },
                    ["\u{2212}1", "0", "+1"],
                ),
                ("BALANCE", balance, Colors::text_primary(), ["L", "C", "R"]),
            ];
            for (track, (caption, value, color, scale)) in meters.into_iter().zip(rows) {
                root = root
                    .child(
                        div()
                            .absolute()
                            .left(px(track.x))
                            .top(px(layout.strip.y))
                            .w(px(track.w))
                            .h(px(STEREO_METER_CAPTION))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(small(caption))
                            .child(readout_value(value, color, typography::DENSE_LABEL)),
                    )
                    .child(meter_scale(track, scale));
            }
        }
        VisualizerKind::Loudness => {
            meta = meta_row().child(
                div()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_faint())
                    .whitespace_nowrap()
                    .child("EBU R128"),
            );
            let side = layout.side;
            for (index, (caption, value)) in [
                ("MOMENTARY", analyzer.momentary_lufs),
                ("SHORT-TERM", analyzer.short_term_lufs),
            ]
            .into_iter()
            .enumerate()
            {
                root = root.child(big_readout(
                    side,
                    layout.loudness_block(index),
                    caption,
                    reading(value, 1),
                    "LUFS",
                ));
            }
            let true_peak_color = if analyzer.true_peak_db.is_some_and(|db| db > 0.0) {
                Colors::accent_danger()
            } else {
                Colors::text_secondary()
            };
            for (index, (caption, value, unit, color)) in [
                (
                    "INTEGRATED",
                    reading(analyzer.integrated_lufs, 1),
                    "LUFS",
                    Colors::text_secondary(),
                ),
                (
                    "RANGE",
                    reading(analyzer.range_lu, 1),
                    "LU",
                    Colors::text_secondary(),
                ),
                (
                    "TRUE PEAK",
                    reading(analyzer.true_peak_db, 1),
                    "dBTP",
                    true_peak_color,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                root = root.child(
                    div()
                        .absolute()
                        .left(px(side.x))
                        .top(px(layout.loudness_row(index)))
                        .w(px(side.w))
                        .h(px(scene::LOUDNESS_ROW))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(space::BASE))
                        .child(small(caption))
                        .child(
                            div()
                                .flex()
                                .items_end()
                                .gap(px(space::HAIR + 1.0))
                                .child(
                                    readout_value(value, color, typography::UI_SM)
                                        .line_height(px(typography::UI_SM + 2.0)),
                                )
                                .child(small(unit).line_height(px(typography::UI_SM + 2.0))),
                        ),
                );
            }

            root = root
                .children(value_ticks(
                    layout,
                    LOUDNESS_GRID
                        .iter()
                        .map(|lufs| (number(*lufs, 0), layout.loudness_y(*lufs))),
                    "LUFS",
                ))
                .children(axis_ticks(
                    layout,
                    [("now".to_string(), p.right(), Anchor::End)]
                        .into_iter()
                        .chain(LOUDNESS_TIME_GRID.iter().map(|seconds| {
                            (
                                minus(format!("-{seconds} s")),
                                layout.loudness_x(*seconds),
                                Anchor::Centre,
                            )
                        })),
                ))
                .child(
                    small(format!("Target {}", number(LOUDNESS_TARGET, 0)))
                        .absolute()
                        .left(px(p.x))
                        .w(px((p.w - space::SNUG).max(0.0)))
                        .top(px(layout.loudness_y(LOUDNESS_TARGET) - TICK_H - 1.0))
                        .h(px(TICK_H))
                        .flex()
                        .items_center()
                        .justify_end()
                        .text_color(Colors::accent_warning()),
                );

            let engine_handle = cx.entity().downgrade();
            root = root.child(
                div()
                    .id("visualizer-loudness-reset")
                    .absolute()
                    .left(px(side.x))
                    .top(px(layout.loudness_reset()))
                    .h(px(LOUDNESS_RESET_H))
                    .occlude()
                    .flex()
                    .items_center()
                    .px(px(space::BASE))
                    .rounded(px(radius::CONTROL_SM))
                    .border_1()
                    .border_color(Colors::button_border())
                    .bg(paint.button)
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_secondary())
                    .hover(move |style| style.bg(paint.button_hover))
                    .active(move |style| style.bg(paint.button_pressed))
                    .cursor_pointer()
                    .child("Reset integrated")
                    .on_click(move |_, _, cx| {
                        if let Some(view) = engine_handle.upgrade() {
                            view.update(cx, |view, cx| {
                                view.engine.borrow_mut().analyzer.reset_loudness();
                                cx.notify();
                            });
                        }
                    }),
            );
        }
        VisualizerKind::Oscilloscope => {
            let peak = |value: f32| {
                if value > 0.0 {
                    number(20.0 * value.log10(), 1)
                } else {
                    "\u{2212}\u{221e}".to_string()
                }
            };
            let peak_color = |value: f32| {
                if value > 1.0 {
                    Colors::accent_danger()
                } else {
                    Colors::text_secondary()
                }
            };
            // A narrow window drops the caption, not the figures.
            let wide = layout.size[0] >= 320.0;
            meta = meta_row()
                .child(meta_pair(
                    if wide { "Peak L" } else { "L" },
                    peak(analyzer.peak[0]),
                    peak_color(analyzer.peak[0]),
                ))
                .child(meta_pair(
                    "R",
                    format!("{} dBFS", peak(analyzer.peak[1])),
                    peak_color(analyzer.peak[1]),
                ));
            let span_ms = (SCOPE_WINDOW_SECONDS * 1_000.0).round();
            let divisions = (span_ms / SCOPE_DIVISION_MS).round() as usize;
            root = root
                .children(axis_ticks(
                    layout,
                    [
                        ("0".to_string(), p.x, Anchor::Start),
                        (format!("{span_ms} ms"), p.right(), Anchor::End),
                    ]
                    .into_iter()
                    .chain((1..divisions).map(|i| {
                        let ms = i as f32 * SCOPE_DIVISION_MS;
                        (format!("{ms}"), layout.scope_x(ms), Anchor::Centre)
                    })),
                ))
                .children(value_ticks(
                    layout,
                    [
                        ("0", 0.0),
                        ("+1", 1.0),
                        ("\u{2212}1", -1.0),
                        ("+.5", 0.5),
                        ("\u{2212}.5", -0.5),
                    ]
                    .map(|(text, level)| (text.to_string(), layout.scope_y(level))),
                    "",
                ));
        }
    }
    (root.into_any_element(), meta.into_any_element())
}

/// A readout's figure, or a dash before there is one.
fn readout_value(value: Option<String>, color: Rgba, text_size: f32) -> gpui::Div {
    let (text, color) = match value {
        Some(text) => (text, color),
        None => ("\u{2014}".to_string(), Colors::text_faint()),
    };
    div()
        .text_size(px(text_size))
        .font_features(tabular())
        .text_color(color)
        .whitespace_nowrap()
        .child(text)
}

/// A headline reading in the loudness column: caption, then the figure with
/// its unit, with the scene's bar under it ([`Layout::loudness_bar`]).
fn big_readout(
    side: Rect,
    top: f32,
    caption: &'static str,
    value: Option<String>,
    unit: &'static str,
) -> gpui::Div {
    div()
        .absolute()
        .left(px(side.x))
        .top(px(top))
        .w(px(side.w))
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(READOUT_CAPTION))
                .flex()
                .items_center()
                .child(small(caption)),
        )
        .child(
            // One line height for both runs, bottoms aligned: the figure and
            // its unit then share a baseline (GPUI's baseline alignment
            // follows each run's own line box, which sits the unit low).
            div()
                .h(px(READOUT_VALUE))
                .flex()
                .items_end()
                .gap(px(space::TIGHT))
                .child(
                    readout_value(value, Colors::text_primary(), typography::UI_LG)
                        .font_weight(FontWeight::SEMIBOLD)
                        .line_height(px(typography::UI_LG + 2.0)),
                )
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .line_height(px(typography::UI_LG + 2.0))
                        .text_color(Colors::text_muted())
                        .child(unit),
                ),
        )
}

/// The three marks under a centre-zero meter: its two ends and its centre.
fn meter_scale(track: Rect, marks: [&'static str; 3]) -> gpui::Div {
    let mut row = div()
        .absolute()
        .left(px(track.x))
        .top(px(track.bottom() + 3.0))
        .w(px(track.w))
        .h(px(TICK_H))
        .flex()
        .items_center()
        .justify_between();
    for mark in marks {
        row = row.child(small(mark));
    }
    row
}

/// Open a visualizer window near the bottom-right of the Studio window, the
/// way a picture-in-picture player opens in a corner.
pub fn open_visualizer_window(
    kind: VisualizerKind,
    owner_bounds: Option<Bounds<Pixels>>,
    on_close: Arc<dyn Fn(VisualizerKind, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<VisualizerWindow>, String> {
    let (w, h) = default_size(kind);
    let (min_w, min_h) = min_size(kind);
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.kind = WindowKind::Floating;
    options.is_minimizable = false;
    options.titlebar = Some(gpui::TitlebarOptions {
        title: Some(crate::platform_chrome::branded_window_title(kind.title()).into()),
        appears_transparent: true,
        traffic_light_position: None,
    });
    options.window_min_size = Some(size(px(min_w), px(min_h)));
    let offset = VisualizerKind::ALL
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(0) as f32
        * 28.0;
    options.window_bounds = Some(WindowBounds::Windowed(match owner_bounds {
        Some(owner) => Bounds::new(
            point(
                owner.origin.x + owner.size.width - px(w + 32.0 + offset),
                owner.origin.y + owner.size.height - px(h + 72.0 + offset),
            ),
            size(px(w), px(h)),
        ),
        None => crate::window_position::centered_window_bounds(None, size(px(w), px(h)), cx),
    }));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);
    cx.open_window(options, move |window, cx| {
        cx.new(|cx| VisualizerWindow::new(kind, Engine::new(kind), on_close, window, cx))
    })
    .map_err(|error| error.to_string())
}

/// Preview: a visualizer window in `options`, fed by `signal` (stereo blocks
/// at 48 kHz) rather than the master bus.
#[cfg(feature = "ui-preview")]
pub fn open_preview_window(
    kind: VisualizerKind,
    options: gpui::WindowOptions,
    signal: Box<dyn FnMut(&mut [f32], &mut [f32])>,
    cx: &mut App,
) -> gpui::Result<WindowHandle<VisualizerWindow>> {
    let on_close: Arc<dyn Fn(VisualizerKind, &mut App) + Send + Sync> = Arc::new(|_, _| {});
    cx.open_window(options, move |window, cx| {
        cx.new(|cx| {
            VisualizerWindow::new(
                kind,
                Engine::with_signal(kind, signal),
                on_close,
                window,
                cx,
            )
        })
    })
}
