//! The picture-in-picture shell a visualizer lives in.
//!
//! A small borderless window that floats above everything (`Floating`), is
//! all content, and can be dragged from anywhere on it. The title and close
//! control appear only while the pointer is over the window, the way a PIP
//! video player shows its controls, so a meter parked in a corner of the
//! screen is nothing but the meter.
//!
//! The picture is drawn by wgpu ([`super::gpu`]) and composited by GPUI; the
//! labels, readouts and controls are GPUI elements laid over it with the same
//! [`Layout`] the picture was built with.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Corners, DevicePixels, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, RenderImage, StatefulInteractiveElement, Styled, Window,
    WindowBounds, WindowControlArea, WindowHandle, WindowKind, canvas, div, point, px, size, svg,
};
use smallvec::SmallVec;

use super::VisualizerKind;
use super::analysis::Analyzer;
use super::gpu::{self, Presented, Surface};
use super::scene::{
    self, FREQ_TICKS, LOUDNESS_GRID, LOUDNESS_PANEL, LOUDNESS_ROWS, LOUDNESS_TARGET, Layout,
    Palette, SPECTRUM_GRID_DB, STEREO_METER_CAPTION, TITLE_BAR, ViewState,
};
use crate::theme::{Colors, typography};

const DEFAULT_SIZE: (f32, f32) = (420.0, 260.0);
const MIN_SIZE: (f32, f32) = (240.0, 180.0);

/// Everything that changes every frame, owned outside the view so the paint
/// closure can drive it without re-entering the view.
struct Engine {
    kind: VisualizerKind,
    analyzer: Analyzer,
    cursor: u64,
    listening: bool,
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
}

impl Engine {
    fn new(kind: VisualizerKind) -> Self {
        let palette = Palette::from_theme();
        Self {
            kind,
            analyzer: Analyzer::new(kind.features()),
            cursor: 0,
            listening: false,
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
        if !self.listening {
            self.cursor = DirectAudio::visualizer_tap().listen();
            self.listening = true;
        }
        self.cursor = self.analyzer.pull(self.cursor, dt);
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
        if self.listening {
            DirectAudio::visualizer_tap().unlisten();
        }
    }
}

pub struct VisualizerWindow {
    kind: VisualizerKind,
    engine: Rc<RefCell<Engine>>,
    on_close: Arc<dyn Fn(VisualizerKind, &mut App) + Send + Sync>,
}

impl VisualizerWindow {
    fn new(
        kind: VisualizerKind,
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
            engine: Rc::new(RefCell::new(Engine::new(kind))),
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
        let overlay = overlay(self.kind, &layout, &engine.borrow(), cx);

        div()
            .id("visualizer-root")
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(Colors::surface_canvas())
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
                    .window_control_area(WindowControlArea::Drag),
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
                        .p(px(16.0))
                        .text_size(px(typography::UI_XS))
                        .child(format!("The GPU renderer is unavailable: {error}")),
                )
            })
            .child(self.title_bar(cx))
    }
}

impl VisualizerWindow {
    /// Title and close across the top: part of the picture, no surface of its
    /// own, and draggable like the rest of it.
    fn title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("visualizer-title-bar")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TITLE_BAR))
            .flex()
            .items_center()
            .justify_between()
            .pl(px(10.0))
            .pr(px(4.0))
            .window_control_area(WindowControlArea::Drag)
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_secondary())
                    .child(self.kind.title()),
            )
            .child(
                div()
                    .id("visualizer-close")
                    .group("visualizer-close")
                    .occlude()
                    .p(px(6.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx)))
                    .child(
                        svg()
                            .path(crate::assets::ICON_CLOSE_SMALL_PATH)
                            .w(px(12.0))
                            .h(px(12.0))
                            .text_color(Colors::text_muted())
                            .group_hover("visualizer-close", |style| {
                                style.text_color(Colors::text_primary())
                            }),
                    ),
            )
    }
}

fn label(text: impl Into<String>, x: f32, y: f32) -> gpui::Div {
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_faint())
        .child(text.into())
}

fn freq_text(hz: f32) -> String {
    if hz >= 1_000.0 {
        format!("{}k", hz / 1_000.0)
    } else {
        format!("{hz}")
    }
}

fn lufs_text(value: Option<f32>, unit: &str) -> String {
    match value {
        Some(v) => format!("{v:.1} {unit}"),
        None => format!("— {unit}"),
    }
}

/// Labels and readouts over the picture, placed with the picture's layout.
fn overlay(
    kind: VisualizerKind,
    layout: &Layout,
    engine: &Engine,
    cx: &mut Context<VisualizerWindow>,
) -> gpui::AnyElement {
    let analyzer = &engine.analyzer;
    let p = layout.plot;
    let mut root = div().absolute().inset_0();
    match kind {
        VisualizerKind::Spectrum => {
            for hz in FREQ_TICKS.iter().take(FREQ_TICKS.len() - 1) {
                root = root.child(label(
                    freq_text(*hz),
                    layout.freq_x(*hz) - 8.0,
                    p.bottom() + 3.0,
                ));
            }
            for db in SPECTRUM_GRID_DB {
                root = root.child(label(
                    format!("{db}"),
                    p.right() + 4.0,
                    layout.spectrum_y(db) - 6.0,
                ));
            }
            root = root.child(label("4.5 dB/oct", p.x + 4.0, p.y + 2.0));
        }
        VisualizerKind::Spectrogram => {
            for hz in [100.0, 1_000.0, 10_000.0] {
                root = root.child(label(
                    freq_text(hz),
                    p.right() + 4.0,
                    layout.freq_y(hz) - 6.0,
                ));
            }
            root = root.child(label("now →", p.right() - 34.0, p.bottom() + 3.0));
        }
        VisualizerKind::StereoImage => {
            let (cx0, base, r) = (p.x + p.w / 2.0, p.bottom(), p.h);
            let d = r * std::f32::consts::FRAC_1_SQRT_2;
            root = root
                .child(label("L", cx0 - d - 12.0, base - d - 14.0))
                .child(label("R", cx0 + d + 5.0, base - d - 14.0))
                .child(label("M", cx0 - 4.0, p.y - 14.0))
                .child(label(
                    format!("×{:.1}", engine.view.scope_gain),
                    p.x,
                    base - 14.0,
                ));
            let correlation = if analyzer.live {
                format!("{:+.2}", analyzer.correlation)
            } else {
                "—".to_string()
            };
            let balance = if !analyzer.live {
                "—".to_string()
            } else if analyzer.balance_db.abs() < 0.05 {
                "C".to_string()
            } else if analyzer.balance_db > 0.0 {
                format!("R {:.1} dB", analyzer.balance_db)
            } else {
                format!("L {:.1} dB", -analyzer.balance_db)
            };
            for (track, caption, value) in layout
                .stereo_meters()
                .into_iter()
                .zip([("CORRELATION", correlation), ("BALANCE", balance)])
                .map(|(track, (caption, value))| (track, caption, value))
            {
                root = root.child(
                    div()
                        .absolute()
                        .left(px(track.x))
                        .top(px(layout.strip.y))
                        .w(px(track.w))
                        .h(px(STEREO_METER_CAPTION))
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(px(typography::DENSE_CAPTION))
                        .child(div().text_color(Colors::text_faint()).child(caption))
                        .child(div().text_color(Colors::text_secondary()).child(value)),
                );
            }
        }
        VisualizerKind::Loudness => {
            let row = |caption: &str, value: String, row: usize| {
                div()
                    .absolute()
                    .left(px(10.0))
                    .top(px(layout.top + LOUDNESS_ROWS[row]))
                    .w(px(LOUDNESS_PANEL - 20.0))
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_faint())
                            .child(caption.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(typography::UI_MD))
                            .text_color(Colors::text_primary())
                            .child(value),
                    )
            };
            root = root
                .child(row(
                    "MOMENTARY",
                    lufs_text(analyzer.momentary_lufs, "LUFS"),
                    0,
                ))
                .child(row(
                    "SHORT-TERM",
                    lufs_text(analyzer.short_term_lufs, "LUFS"),
                    1,
                ))
                .child(row(
                    "INTEGRATED",
                    lufs_text(analyzer.integrated_lufs, "LUFS"),
                    2,
                ))
                .child(row("RANGE", lufs_text(analyzer.range_lu, "LU"), 3))
                .child(row(
                    "TRUE PEAK",
                    lufs_text(analyzer.true_peak_db, "dBTP"),
                    4,
                ));
            for lufs in LOUDNESS_GRID {
                root = root.child(label(
                    format!("{lufs}"),
                    p.right() + 4.0,
                    layout.loudness_y(lufs) - 6.0,
                ));
            }
            root = root.child(
                label(
                    format!("{LOUDNESS_TARGET} target"),
                    p.x + 4.0,
                    layout.loudness_y(LOUDNESS_TARGET) - 13.0,
                )
                .text_color(Colors::accent_warning()),
            );
            let engine_handle = cx.entity().downgrade();
            root = root.child(
                div()
                    .id("visualizer-loudness-reset")
                    .absolute()
                    .left(px(10.0))
                    .top(px(layout.top + LOUDNESS_ROWS[4] + 24.0))
                    .occlude()
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(Colors::border_normal())
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_secondary())
                    .hover(|style| style.bg(Colors::state_hover()))
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
                    format!("{:.1}", 20.0 * value.log10())
                } else {
                    "−∞".to_string()
                }
            };
            root = root
                .child(label(
                    format!(
                        "{} ms",
                        (super::analysis::SCOPE_WINDOW_SECONDS * 1_000.0).round()
                    ),
                    p.x + 4.0,
                    p.bottom() - 14.0,
                ))
                .child(label(
                    format!(
                        "Peak L {}  R {} dBFS",
                        peak(analyzer.peak[0]),
                        peak(analyzer.peak[1])
                    ),
                    p.x + 4.0,
                    p.y + 2.0,
                ));
        }
    }
    root.into_any_element()
}

/// Open a visualizer window near the bottom-right of the Studio window, the
/// way a picture-in-picture player opens in a corner.
pub fn open_visualizer_window(
    kind: VisualizerKind,
    owner_bounds: Option<Bounds<Pixels>>,
    on_close: Arc<dyn Fn(VisualizerKind, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<VisualizerWindow>, String> {
    let (w, h) = DEFAULT_SIZE;
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.kind = WindowKind::Floating;
    options.is_minimizable = false;
    options.titlebar = Some(gpui::TitlebarOptions {
        title: Some(crate::platform_chrome::branded_window_title(kind.title()).into()),
        appears_transparent: true,
        traffic_light_position: None,
    });
    options.window_min_size = Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1)));
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
        cx.new(|cx| VisualizerWindow::new(kind, on_close, window, cx))
    })
    .map_err(|error| error.to_string())
}
