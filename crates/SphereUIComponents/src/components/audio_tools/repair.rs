//! The Audio Repair workspace.
//!
//! Three regions, one job each:
//!
//! ```txt
//! [ tool rail ][      Audio Processing Canvas      ][ inspector ]
//!    modules      source audio + the active effect    parameters
//! ```
//!
//! The centre is always the source material. A module never replaces it with
//! its own plot; it contributes overlays that are drawn on the same
//! time-frequency plane through [`CanvasTransform`]. Spectrum analysis is a
//! floating secondary read-out, off by default.
//!
//! Ownership: this module owns the canvas viewport, the drawn region, the
//! analysis products shown on the canvas, and the preview worker's lifecycle.
//! `window.rs` owns the session, the processor parameters, and the commit path.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, svg, App, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    ScrollWheelEvent, StatefulInteractiveElement, Styled, Window,
};
use sphere_audio_editor::{
    AudioRepairModule, FrequencyScale, SpectralSelection, SpectrogramTileView,
};
use SphereAudioProcessor::{
    apply_spectral_gain, db_to_lin, declick_interleaved, detect_clicks, downmix_interleaved,
    lin_to_db, noise_gate_mask, peak_amplitude, reduce_noise_stft, AudioClipProcessor, ClickEvent,
    DeclickParams, DehumParams, DehumProcessor, SpectralDenoiseParams, SpectralGainParams,
    StftSettings,
};

use crate::assets;
use crate::components::audio_editor_spectrogram::{cached_or_analyze, SpectrogramJobParams};
use crate::components::controls::{fb_button, fb_icon_button, fb_section_label, FbButtonKind};
use crate::theme::{elevation, radius, size, space, typography, Colors};

use super::canvas::{
    build_mask_image, canvas_legend, processing_canvas, CanvasBand, CanvasDiff, CanvasEvent,
    CanvasMask, CanvasOverlays, CanvasRegion, CanvasTransform, CanvasView, CanvasViewport,
    EventTone, ProcessingCanvas, WaveformPeaks,
};
use super::module_list::{self, repair_module_list, MODULES};
use super::viz;
use super::window::AudioToolWindow;
use super::workspace;

/// Inspector width. Fixed so the canvas never reflows while a value changes.
const INSPECTOR_WIDTH: f32 = 288.0;
/// The inspector in a narrow window, beside the collapsed navigator.
const INSPECTOR_WIDTH_COMPACT: f32 = 256.0;
/// Below this window width the navigator collapses to its icon rail and the
/// inspector narrows, so the canvas keeps a usable width.
const COMPACT_BELOW: f32 = 1_000.0;
const CANVAS_HEADER_H: f32 = 36.0;
/// The view switch. Wide enough that "Spectrogram" never truncates in its
/// third of the track.
const VIEW_TRACK_W: f32 = 312.0;
const VIEW_TRACK_W_COMPACT: f32 = 200.0;
/// Number disc of an inspector step.
const STEP_DISC: f32 = 20.0;
/// Icon plate in the inspector header.
const HEADER_TILE: f32 = 32.0;
/// Upper bound on the frames one preview pass processes. The worker follows the
/// viewport, so this caps latency rather than truncating the clip.
const PREVIEW_MAX_FRAMES: u64 = 20 * 96_000;
const ENVELOPE_HOPS: usize = 512;
const AUDITION_WINDOW: usize = 2048;

/// Everything the repair workspace owns on top of the shared tool session.
pub(super) struct RepairSurface {
    pub module: AudioRepairModule,
    pub view: CanvasView,
    pub frequency_scale: FrequencyScale,
    pub viewport: CanvasViewport,
    pub fitted: bool,
    pub show_transients: bool,
    pub show_spectrum: bool,
    /// Region drawn on the canvas, in absolute source frames.
    pub region: Option<CanvasRegion>,
    drag: Option<RegionDrag>,
    pub peaks: Option<WaveformPeaks>,
    tiles: Vec<SpectrogramTileView>,
    tile_offset_seconds: f32,
    spectrogram_status: Option<String>,
    spectrogram_generation: u64,
    mask: Option<CanvasMask>,
    clicks: Vec<ClickEvent>,
    pub selected_click: usize,
    diff: Option<CanvasDiff>,
    stats: Option<RepairStats>,
    pub profile_frames: u64,
    preview_generation: u64,
    preview_busy: bool,
    preview_dirty: bool,
    audition_peak_db: f32,
    /// One focus handle per navigator row, so the keyboard position *is* GPUI
    /// focus rather than a second cursor that can disagree with it.
    pub module_focus: Vec<FocusHandle>,
    canvas_width: Rc<Cell<f32>>,
    canvas_height: Rc<Cell<f32>>,
    canvas_origin: Rc<Cell<(f32, f32)>>,
}

impl RepairSurface {
    pub fn new(cx: &mut App) -> Self {
        Self {
            module: AudioRepairModule::Denoise,
            view: CanvasView::Overlay,
            frequency_scale: FrequencyScale::Logarithmic,
            viewport: CanvasViewport::default(),
            fitted: false,
            show_transients: false,
            show_spectrum: false,
            region: None,
            drag: None,
            peaks: None,
            tiles: Vec::new(),
            tile_offset_seconds: 0.0,
            spectrogram_status: None,
            spectrogram_generation: 0,
            mask: None,
            clicks: Vec::new(),
            selected_click: 0,
            diff: None,
            stats: None,
            profile_frames: 0,
            preview_generation: 0,
            preview_busy: false,
            preview_dirty: false,
            audition_peak_db: -120.0,
            module_focus: MODULES.iter().map(|_| cx.focus_handle()).collect(),
            canvas_width: Rc::new(Cell::new(720.0)),
            canvas_height: Rc::new(Cell::new(360.0)),
            canvas_origin: Rc::new(Cell::new((0.0, 0.0))),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RegionDrag {
    start_frame: i64,
    start_hz: f32,
    current_frame: i64,
    current_hz: f32,
}

/// Measured before/after figures for the previewed range.
#[derive(Debug, Clone, Copy, Default)]
struct RepairStats {
    peak_before_db: f32,
    peak_after_db: f32,
    rms_before_db: f32,
    rms_after_db: f32,
    /// Share of analyzed time-frequency cells the de-noiser gates.
    coverage: Option<f32>,
    events: usize,
}

/// Result of one preview pass, produced entirely on the background executor.
struct PreviewResult {
    start_frame: u64,
    end_frame: u64,
    diff: Option<CanvasDiff>,
    mask: Option<CanvasMask>,
    clicks: Vec<ClickEvent>,
    stats: RepairStats,
}

/// Modules the engine can audition through the realtime clip preview path.
/// The others are offline-only; their honest feedback is the canvas, not a
/// latch that pretends to change playback.
fn has_engine_audition(module: AudioRepairModule) -> bool {
    matches!(
        module,
        AudioRepairModule::Denoise | AudioRepairModule::DeHum
    )
}

impl AudioToolWindow {
    // ---------------------------------------------------------------- layout

    pub(super) fn repair_surface(
        &mut self,
        viewport_width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let compact = viewport_width < COMPACT_BELOW;
        div()
            .flex()
            .flex_row()
            .size_full()
            .min_h(px(0.0))
            .child(repair_module_list(
                self.repair.module,
                &self.repair.module_focus,
                compact,
                cx,
            ))
            .child(self.repair_canvas_region(compact, cx))
            .child(self.repair_inspector(compact, cx))
    }

    // ------------------------------------------------------------ navigator

    /// Switch modules.
    ///
    /// The previous module's analysis products are dropped rather than left on
    /// the canvas: a mask or an event list from Noise Reduction says nothing true
    /// about De-Hum.
    pub(super) fn activate_repair_module(
        &mut self,
        module: AudioRepairModule,
        cx: &mut Context<Self>,
    ) {
        if self.repair.module == module {
            return;
        }
        self.repair.module = module;
        self.repair.mask = None;
        self.repair.clicks.clear();
        self.repair.stats = None;
        self.repair.diff = None;
        self.emit_preview(cx);
        self.invalidate_repair_preview(cx);
        cx.notify();
    }

    /// Move the keyboard position to a navigator row without activating it.
    pub(super) fn focus_repair_module(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.repair.module_focus.get(index).cloned() {
            window.focus(&handle, cx);
            cx.notify();
        }
    }

    /// Arrow-key navigation between selectable rows.
    pub(super) fn step_repair_module_focus(
        &mut self,
        from: usize,
        delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(next) = module_list::step_selectable(from, delta) {
            self.focus_repair_module(next, window, cx);
        }
    }

    fn repair_canvas_region(&mut self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let transform = self.repair_transform();
        let overlays = self.repair_overlays();
        let legend = self.repair_legend();
        let canvas_model = ProcessingCanvas {
            transform,
            view: self.repair.view,
            peaks: self.repair.peaks.clone(),
            tiles: self.repair.tiles.clone(),
            tile_offset_seconds: self.repair.tile_offset_seconds,
            overlays,
            status: self.repair_canvas_status(),
            status_is_error: false,
        };
        let width_cell = self.repair.canvas_width.clone();
        let height_cell = self.repair.canvas_height.clone();
        let origin_cell = self.repair.canvas_origin.clone();
        let spectrum = self
            .repair
            .show_spectrum
            .then(|| self.repair_spectrum_overlay());

        div()
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex_col()
            .child(self.repair_canvas_header(compact, cx))
            .child(
                div()
                    .id("repair-canvas")
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .bg(Colors::surface_canvas())
                    .child(
                        // Measurement only. The canvas is the layout owner for
                        // its region, so every overlay and every pointer event
                        // resolves against this one rectangle.
                        canvas(
                            move |bounds: Bounds<Pixels>, _window, cx| {
                                let w = f32::from(bounds.size.width).max(1.0);
                                let h = f32::from(bounds.size.height).max(1.0);
                                let ox = f32::from(bounds.origin.x);
                                let oy = f32::from(bounds.origin.y);
                                if (width_cell.get() - w).abs() > 0.5
                                    || (height_cell.get() - h).abs() > 0.5
                                    || (origin_cell.get().0 - ox).abs() > 0.5
                                    || (origin_cell.get().1 - oy).abs() > 0.5
                                {
                                    width_cell.set(w);
                                    height_cell.set(h);
                                    origin_cell.set((ox, oy));
                                    cx.refresh_windows();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(processing_canvas(canvas_model))
                    .child(
                        div()
                            .absolute()
                            .left(px(space::SNUG))
                            .bottom(px(20.0))
                            .child(legend),
                    )
                    .child(self.repair_zoom_cluster(cx))
                    .children(spectrum)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.repair_pointer_down(event.position, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        this.repair_pointer_move(event.position, event.dragging(), cx);
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, _, cx| {
                            this.repair_pointer_up(event.position, cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, _, cx| {
                            this.repair_pointer_up(event.position, cx);
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                        this.repair_wheel(event, window, cx);
                    })),
            )
    }

    /// The canvas toolbar: what to show on the left, overlays on the right.
    /// Zoom lives on the canvas itself ([`Self::repair_zoom_cluster`]), so the
    /// row fits beside the collapsed navigator in a narrow window.
    fn repair_canvas_header(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.repair.view;
        let log = self.repair.frequency_scale == FrequencyScale::Logarithmic;
        let count = CanvasView::ALL.len();
        div()
            .flex_none()
            .h(px(CANVAS_HEADER_H))
            .px(px(space::BASE))
            .gap(px(space::SNUG))
            .flex()
            .flex_row()
            .items_center()
            .overflow_hidden()
            .bg(Colors::surface_panel())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                workspace::segment_track()
                    .flex_none()
                    .w(px(if compact {
                        VIEW_TRACK_W_COMPACT
                    } else {
                        VIEW_TRACK_W
                    }))
                    .children(
                        CanvasView::ALL
                            .into_iter()
                            .enumerate()
                            .map(|(index, candidate)| {
                                workspace::compact_segment(
                                    format!("repair-view-{}", candidate.label()),
                                    if compact {
                                        candidate.label()
                                    } else {
                                        candidate.long_label()
                                    },
                                    view == candidate,
                                    workspace::segment_position(index, count),
                                    cx.listener(move |this, _, _, cx| {
                                        this.repair.view = candidate;
                                        cx.notify();
                                    }),
                                )
                            }),
                    ),
            )
            .child(div().flex_1())
            .child(workspace::latch(
                "repair-log-hz",
                "Log Hz",
                log,
                true,
                cx.listener(|this, _, _, cx| {
                    this.repair.frequency_scale =
                        if this.repair.frequency_scale == FrequencyScale::Logarithmic {
                            FrequencyScale::Linear
                        } else {
                            FrequencyScale::Logarithmic
                        };
                    // Tiles are baked against a frequency scale, so the shared
                    // cache key changes with it.
                    this.spawn_repair_spectrogram(cx);
                    this.invalidate_repair_preview(cx);
                    cx.notify();
                }),
            ))
            .child(workspace::latch(
                "repair-transients",
                "Transients",
                self.repair.show_transients,
                true,
                cx.listener(|this, _, _, cx| {
                    this.repair.show_transients = !this.repair.show_transients;
                    if this.repair.show_transients && this.transients.is_empty() {
                        this.detect_repair_transients();
                    }
                    cx.notify();
                }),
            ))
            .child(workspace::latch(
                "repair-spectrum",
                "Spectrum",
                self.repair.show_spectrum,
                true,
                cx.listener(|this, _, _, cx| {
                    this.repair.show_spectrum = !this.repair.show_spectrum;
                    cx.notify();
                }),
            ))
    }

    /// Zoom out, zoom in and fit, floating in the canvas's lower right corner.
    /// The wheel zooms too; these are the visible, clickable form of it.
    fn repair_zoom_cluster(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .right(px(space::BASE))
            .bottom(px(space::SECTION + space::TIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::HAIR))
            .p(px(space::HAIR))
            .rounded(px(radius::CONTROL))
            .bg(Colors::with_alpha(Colors::surface_raised(), 0.94))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .shadow(elevation::shadow(elevation::OVERLAY))
            // The canvas under it draws regions on mouse down; a press here
            // belongs to the buttons.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(fb_icon_button(
                "repair-zoom-out",
                assets::ICON_ZOOM_OUT_PATH,
                "Zoom out",
                size::MICRO,
                None,
                cx.listener(|this, _, _, cx| this.repair_zoom(0.5, cx)),
            ))
            .child(fb_icon_button(
                "repair-zoom-in",
                assets::ICON_ZOOM_IN_PATH,
                "Zoom in",
                size::MICRO,
                None,
                cx.listener(|this, _, _, cx| this.repair_zoom(2.0, cx)),
            ))
            .child(workspace::ghost_action(
                "repair-fit",
                "Fit",
                true,
                cx.listener(|this, _, _, cx| {
                    let width = this.repair.canvas_width.get();
                    this.repair.viewport = CanvasViewport::fit(this.repair_window_frames(), width);
                    this.invalidate_repair_preview(cx);
                    cx.notify();
                }),
            ))
    }

    /// Spectrum analysis stays available, as a floating read-out anchored to
    /// the canvas rather than as the workspace itself.
    fn repair_spectrum_overlay(&self) -> impl IntoElement {
        let snapshot = self.spectrum.as_ref();
        let magnitudes = snapshot.map(|s| s.magnitudes_db.as_slice()).unwrap_or(&[]);
        let averaged = self.spectrum_avg.as_deref().unwrap_or(&[]);
        let hold = snapshot.map(|s| s.peak_hold_db.as_slice()).unwrap_or(&[]);
        let sample_rate = snapshot
            .map(|s| s.sample_rate)
            .unwrap_or(self.session.target.sample_rate);
        div()
            .absolute()
            .right(px(space::BASE))
            .top(px(space::BASE))
            .w(px(320.0))
            .h(px(180.0))
            .rounded(px(radius::SURFACE))
            .border(px(1.0))
            .border_color(Colors::border_normal())
            .bg(Colors::with_alpha(Colors::surface_panel(), 0.94))
            .shadow(elevation::shadow(elevation::OVERLAY))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .h(px(size::DENSE))
                    .px(px(space::SNUG))
                    .flex()
                    .items_center()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_secondary())
                    .child("SPECTRUM"),
            )
            .child(div().flex_1().min_h(px(0.0)).child(viz::spectrum_view(
                magnitudes,
                averaged,
                hold,
                sample_rate,
                self.repair_graph_draw(),
            )))
    }

    fn repair_legend(&self) -> impl IntoElement {
        let mut entries: Vec<(&'static str, gpui::Rgba)> = Vec::new();
        match self.repair.module {
            AudioRepairModule::Denoise => {
                if self.repair.mask.is_some() {
                    entries.push(("Gated", Colors::accent_purple()));
                }
            }
            AudioRepairModule::DeHum => entries.push(("Notch", Colors::status_warning())),
            AudioRepairModule::DeClick => entries.push(("Click", Colors::status_error())),
            AudioRepairModule::SpectralRepair => entries.push(("Region", Colors::accent_primary())),
            _ => {}
        }
        if self.repair.show_transients && !self.transients.is_empty() {
            entries.push(("Transient", Colors::text_secondary()));
        }
        if self
            .repair
            .diff
            .as_ref()
            .is_some_and(|diff| !diff.is_empty())
        {
            entries.push(("Removed", Colors::status_error()));
            entries.push(("After", Colors::accent_primary()));
        }
        canvas_legend(entries)
    }

    // ------------------------------------------------------------ inspector

    /// The inspector: what the module is for, its steps in order, the range it
    /// works on, and the measured result.
    fn repair_inspector(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let module = self.repair.module;
        let icon = MODULES[module_list::index_of(module)].icon;
        div()
            .id("repair-inspector")
            .flex_none()
            .w(px(if compact {
                INSPECTOR_WIDTH_COMPACT
            } else {
                INSPECTOR_WIDTH
            }))
            .h_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_panel())
            .border_l(px(1.0))
            .border_color(Colors::border_subtle())
            .overflow_y_scroll()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(space::BASE))
                    .px(px(space::LOOSE))
                    .pt(px(space::LOOSE))
                    .pb(px(space::LOOSE))
                    .border_b(px(1.0))
                    .border_color(Colors::border_subtle())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(space::BASE))
                            .child(
                                div()
                                    .size(px(HEADER_TILE))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(radius::CONTROL))
                                    .bg(Colors::with_alpha(Colors::accent_primary(), 0.16))
                                    .child(
                                        svg()
                                            .path(icon)
                                            .size(px(18.0))
                                            .text_color(Colors::accent_primary()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(typography::UI_MD))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Colors::text_primary())
                                    .child(module.label()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_secondary())
                            .child(module.summary()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SECTION + space::TIGHT))
                    .px(px(space::LOOSE))
                    .py(px(space::LOOSE))
                    .children(match module {
                        AudioRepairModule::Denoise => {
                            Some(self.denoise_inspector(cx).into_any_element())
                        }
                        AudioRepairModule::DeClick => {
                            Some(self.declick_inspector(cx).into_any_element())
                        }
                        AudioRepairModule::DeHum => {
                            Some(self.dehum_inspector(cx).into_any_element())
                        }
                        AudioRepairModule::SpectralRepair => {
                            Some(self.spectral_inspector(cx).into_any_element())
                        }
                        _ => None,
                    })
                    .when(!module.uses_spectral_selection(), |body| {
                        body.child(self.region_section(cx))
                    })
                    .child(self.result_section()),
            )
    }

    fn denoise_inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let learned = self.learned_noise.is_some();
        let sample_rate = self.session.target.sample_rate.max(1) as f32;
        let from_region = self.repair.region.is_some();
        let status = if learned {
            format!(
                "Learned from {:.2} s of audio.",
                self.repair.profile_frames as f32 / sample_rate
            )
        } else if from_region {
            "It will learn from your selection. Make sure only noise is in it.".to_string()
        } else {
            "Select a moment with only noise in it, like a pause between \
             phrases, then press Learn Noise."
                .to_string()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(space::SECTION + space::TIGHT))
            .child(repair_step(
                1,
                "Teach it the noise",
                learned,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::BASE))
                    .child(step_note(status))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(space::SNUG))
                            .child(fb_button(
                                "repair-learn",
                                if learned {
                                    "Learn Again"
                                } else {
                                    "Learn Noise"
                                },
                                // Commit is the window's one primary action.
                                FbButtonKind::Default,
                                self.source_pcm.is_some(),
                                cx.listener(|this, _, _, cx| this.spawn_learn_noise(cx)),
                            ))
                            .when(learned, |row| {
                                row.child(fb_button(
                                    "repair-forget",
                                    "Forget",
                                    FbButtonKind::Ghost,
                                    true,
                                    cx.listener(|this, _, _, cx| {
                                        this.learned_noise = None;
                                        this.repair.profile_frames = 0;
                                        this.repair.mask = None;
                                        this.invalidate_repair_preview(cx);
                                        cx.notify();
                                    }),
                                ))
                            }),
                    )
                    .into_any_element(),
            ))
            .child(repair_step(
                2,
                "Choose how much to remove",
                false,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::LOOSE))
                    .child(repair_param(
                        "Reduction",
                        format!("{:.1} dB", self.denoise.reduction_db),
                        workspace::unipolar_slider(
                            "repair-dn-reduction",
                            self.denoise.reduction_db,
                            0.0,
                            24.0,
                            bind_repair(cx, |this, value, cx| {
                                this.denoise.reduction_db = value;
                                this.emit_repair_preview(cx);
                                this.invalidate_repair_preview(cx);
                            }),
                            Some(12.0),
                        ),
                    ))
                    .child(repair_param(
                        "Threshold",
                        format!("{:.0} dB", self.denoise.threshold_db),
                        workspace::unipolar_slider(
                            "repair-dn-threshold",
                            self.denoise.threshold_db,
                            -80.0,
                            -12.0,
                            bind_repair(cx, |this, value, cx| {
                                this.denoise.threshold_db = value;
                                this.emit_repair_preview(cx);
                                this.invalidate_repair_preview(cx);
                            }),
                            Some(-48.0),
                        ),
                    ))
                    .into_any_element(),
            ))
    }

    fn declick_inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.repair.clicks.len();
        let selected = self.repair.selected_click.min(total.saturating_sub(1));
        let sample_rate = self.session.target.sample_rate.max(1) as f32;
        let found = match total {
            0 => "No clicks found".to_string(),
            1 => "1 click found".to_string(),
            n => format!("{n} clicks found"),
        };
        let showing = self.repair.clicks.get(selected).map(|event| {
            let seconds = (self.repair_window_start() + event.frame as u64) as f32 / sample_rate;
            format!("Showing click {} at {seconds:.3} s", selected + 1)
        });
        let width = self.declick.max_click_width;
        div()
            .flex()
            .flex_col()
            .gap(px(space::SECTION + space::TIGHT))
            .child(repair_step(
                1,
                "Find the clicks",
                total > 0,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::LOOSE))
                    .child(repair_param(
                        "Sensitivity",
                        format!("{:.0}%", self.declick.sensitivity * 100.0),
                        workspace::unipolar_slider(
                            "repair-dc-sensitivity",
                            self.declick.sensitivity,
                            0.05,
                            1.0,
                            bind_repair(cx, |this, value, cx| {
                                this.declick.sensitivity = value;
                                this.invalidate_repair_preview(cx);
                            }),
                            Some(0.65),
                        ),
                    ))
                    .child(repair_param(
                        "Longest click",
                        format!(
                            "{width} smp · {:.2} ms",
                            width as f32 / sample_rate * 1000.0
                        ),
                        workspace::unipolar_slider(
                            "repair-dc-width",
                            width as f32,
                            2.0,
                            64.0,
                            bind_repair(cx, |this, value, cx| {
                                this.declick.max_click_width = value.round() as usize;
                                this.invalidate_repair_preview(cx);
                            }),
                            Some(12.0),
                        ),
                    ))
                    .into_any_element(),
            ))
            .child(repair_step(
                2,
                "Check what it found",
                false,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .text_size(px(typography::UI_SM))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(Colors::text_primary())
                            .child(found),
                    )
                    .child(step_note(showing.unwrap_or_else(|| {
                        "Raise the sensitivity if you can still hear clicks.".to_string()
                    })))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(space::SNUG))
                            .child(fb_button(
                                "repair-dc-prev",
                                "Previous",
                                FbButtonKind::Default,
                                total > 0,
                                cx.listener(|this, _, _, cx| this.step_click(-1, cx)),
                            ))
                            .child(fb_button(
                                "repair-dc-next",
                                "Next",
                                FbButtonKind::Default,
                                total > 0,
                                cx.listener(|this, _, _, cx| this.step_click(1, cx)),
                            )),
                    )
                    .into_any_element(),
            ))
    }

    fn dehum_inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let options = [(50.0_f32, "50 Hz"), (60.0, "60 Hz")];
        let count = options.len();
        div()
            .flex()
            .flex_col()
            .gap(px(space::SECTION + space::TIGHT))
            .child(repair_step(
                1,
                "Pick the mains frequency",
                false,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::BASE))
                    .child(workspace::segment_track().children(
                        options.into_iter().enumerate().map(|(index, (hz, label))| {
                            workspace::compact_segment(
                                format!("repair-hum-{label}"),
                                label,
                                (self.dehum.base_hz - hz).abs() < 1.0,
                                workspace::segment_position(index, count),
                                cx.listener(move |this, _, _, cx| {
                                    this.dehum.base_hz = hz;
                                    this.emit_repair_preview(cx);
                                    this.invalidate_repair_preview(cx);
                                    cx.notify();
                                }),
                            )
                        }),
                    ))
                    .child(step_note(
                        "50 Hz in most of the world, 60 Hz in North America and a few \
                         other countries. The yellow lines show where it cuts."
                            .to_string(),
                    ))
                    .into_any_element(),
            ))
            .child(repair_step(
                2,
                "Choose how much to remove",
                false,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::LOOSE))
                    .child(repair_param(
                        "Overtones",
                        format!("{}", self.dehum.harmonics),
                        workspace::unipolar_slider(
                            "repair-hum-harmonics",
                            self.dehum.harmonics as f32,
                            1.0,
                            10.0,
                            bind_repair(cx, |this, value, cx| {
                                this.dehum.harmonics = value.round().clamp(1.0, 10.0) as u8;
                                this.emit_repair_preview(cx);
                                this.invalidate_repair_preview(cx);
                            }),
                            Some(4.0),
                        ),
                    ))
                    .child(repair_param(
                        "Reduction",
                        format!("{:.1} dB", self.dehum.reduction_db),
                        workspace::unipolar_slider(
                            "repair-hum-reduction",
                            self.dehum.reduction_db,
                            0.0,
                            48.0,
                            bind_repair(cx, |this, value, cx| {
                                this.dehum.reduction_db = value;
                                this.emit_repair_preview(cx);
                                this.invalidate_repair_preview(cx);
                            }),
                            Some(18.0),
                        ),
                    ))
                    .into_any_element(),
            ))
    }

    fn spectral_inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sample_rate = self.session.target.sample_rate.max(1) as f32;
        let region = self.repair.region;
        let drawn = match region {
            Some(region) => div()
                .flex()
                .flex_col()
                .gap(px(space::BASE))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(space::HAIR))
                        .child(readout_row(
                            "Time",
                            format!(
                                "{:.3} – {:.3} s",
                                region.start_frame as f32 / sample_rate,
                                region.end_frame as f32 / sample_rate
                            ),
                        ))
                        .child(readout_row(
                            "Pitch range",
                            format!("{:.0} – {:.0} Hz", region.min_hz, region.max_hz),
                        )),
                )
                .child(div().flex().child(fb_button(
                    "repair-region-clear",
                    "Draw Again",
                    FbButtonKind::Default,
                    true,
                    cx.listener(|this, _, _, cx| {
                        this.repair.region = None;
                        this.invalidate_repair_preview(cx);
                        cx.notify();
                    }),
                )))
                .into_any_element(),
            None => step_note(
                "Drag on the spectrogram to draw a box around the sound you want \
                 to change."
                    .to_string(),
            )
            .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(space::SECTION + space::TIGHT))
            .child(repair_step(
                1,
                "Draw around the sound",
                region.is_some(),
                drawn,
            ))
            .child(repair_step(
                2,
                "Turn it down",
                false,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::LOOSE))
                    .child(repair_param(
                        "Gain",
                        if self.spectral_gain_db <= -119.0 {
                            "Silent".to_string()
                        } else {
                            format!("{:+.1} dB", self.spectral_gain_db)
                        },
                        workspace::unipolar_slider(
                            "repair-sp-gain",
                            self.spectral_gain_db,
                            -48.0,
                            24.0,
                            bind_repair(cx, |this, value, cx| {
                                this.spectral_gain_db = value;
                                this.invalidate_repair_preview(cx);
                            }),
                            Some(0.0),
                        ),
                    ))
                    .child(div().flex().child(fb_button(
                        "repair-sp-silence",
                        "Silence It",
                        FbButtonKind::Default,
                        region.is_some(),
                        cx.listener(|this, _, _, cx| {
                            this.spectral_gain_db = -120.0;
                            this.invalidate_repair_preview(cx);
                            cx.notify();
                        }),
                    )))
                    .into_any_element(),
            ))
    }

    /// Which part of the clip the module works on: all of it, or the part
    /// drawn on the canvas.
    fn region_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sample_rate = self.session.target.sample_rate.max(1) as f32;
        let region = self.repair.region;
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(fb_section_label("WORKING ON"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .h(px(size::PROMINENT))
                    .pl(px(space::BASE))
                    .pr(px(space::TIGHT))
                    .rounded(px(radius::CONTROL))
                    .bg(Colors::surface_input())
                    .border(px(1.0))
                    .border_color(Colors::border_subtle())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(Colors::text_primary())
                            .child(match region {
                                Some(region) => format!(
                                    "Selection · {:.3} – {:.3} s",
                                    region.start_frame as f32 / sample_rate,
                                    region.end_frame as f32 / sample_rate
                                ),
                                None => "Whole clip".to_string(),
                            }),
                    )
                    .when(region.is_none(), |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(typography::DENSE_CAPTION))
                                .text_color(Colors::text_faint())
                                .child("Drag on the canvas to pick a part"),
                        )
                    })
                    .when(region.is_some(), |row| {
                        row.child(fb_icon_button(
                            "repair-region-clear",
                            assets::ICON_CLOSE_SMALL_PATH,
                            "Clear selection",
                            size::MICRO,
                            None,
                            cx.listener(|this, _, _, cx| {
                                this.repair.region = None;
                                this.invalidate_repair_preview(cx);
                                cx.notify();
                            }),
                        ))
                    }),
            )
    }

    /// Before/after figures measured on the previewed range, never estimated.
    fn result_section(&self) -> impl IntoElement {
        let stats = self.repair.stats;
        let stale =
            self.repair.diff.as_ref().is_some_and(|diff| diff.stale) || self.repair.preview_busy;
        let audition_hint = if !has_engine_audition(self.repair.module) {
            "Audition is not available for this module yet. The canvas shows \
             what it changes."
        } else if self.session.preview_enabled {
            "You are hearing the repair in the project now."
        } else {
            "Turn on Audition below to hear it in the project."
        };
        let change = stats.map(|stats| {
            let change = stats.rms_after_db - stats.rms_before_db;
            if change.abs() < 0.005 {
                0.0
            } else {
                change
            }
        });
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .p(px(space::LOOSE))
            .rounded(px(radius::SURFACE))
            .bg(Colors::surface_raised())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(fb_section_label("RESULT"))
                    .when(stale, |row| {
                        row.child(
                            div()
                                .text_size(px(typography::DENSE_CAPTION))
                                .text_color(Colors::text_faint())
                                .child("Updating…"),
                        )
                    }),
            )
            .child(match change {
                Some(change) => div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .text_size(px(typography::UI_LG))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if change < -0.05 {
                                Colors::accent_primary()
                            } else {
                                Colors::text_primary()
                            })
                            .child(format!("{change:+.2} dB")),
                    )
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child("change in level"),
                    )
                    .into_any_element(),
                None => div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child("Nothing to compare yet.")
                    .into_any_element(),
            })
            .children(stats.map(|stats| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(readout_row(
                        "Peak",
                        format!(
                            "{:.1} → {:.1} dB",
                            stats.peak_before_db, stats.peak_after_db
                        ),
                    ))
                    .child(readout_row(
                        "Average (RMS)",
                        format!("{:.1} → {:.1} dB", stats.rms_before_db, stats.rms_after_db),
                    ))
                    .children(stats.coverage.map(|coverage| {
                        readout_row("Treated as noise", format!("{:.1}%", coverage * 100.0))
                    }))
                    .children(
                        (stats.events > 0)
                            .then(|| readout_row("Clicks repaired", format!("{}", stats.events))),
                    )
            }))
            .child(
                div()
                    .pt(px(space::TIGHT))
                    .border_t(px(1.0))
                    .border_color(Colors::border_subtle())
                    .child(step_note(audition_hint.to_string())),
            )
    }

    pub(super) fn audition_meter(&self) -> impl IntoElement {
        let db = self.repair.audition_peak_db;
        let filled = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .child(
                div()
                    .w(px(64.0))
                    .h(px(4.0))
                    .rounded(px(radius::MICRO))
                    .bg(Colors::meter_rail())
                    .child(
                        div()
                            .w(px(64.0 * filled))
                            .h(px(4.0))
                            .rounded(px(radius::MICRO))
                            .bg(if db > -1.0 {
                                Colors::meter_high()
                            } else {
                                Colors::meter_low()
                            }),
                    ),
            )
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_muted())
                    .child(if db <= -119.0 {
                        "silent".to_string()
                    } else {
                        format!("{db:.0} dB")
                    }),
            )
    }

    // ----------------------------------------------------------- coordinates

    pub(super) fn repair_window_start(&self) -> u64 {
        self.session
            .target
            .time_selection
            .map(|selection| selection.start_frame.max(0) as u64)
            .unwrap_or(0)
    }

    pub(super) fn repair_window_frames(&self) -> u64 {
        self.repair
            .peaks
            .as_ref()
            .map(|peaks| peaks.frames() as u64)
            .unwrap_or_else(|| {
                self.session
                    .target
                    .time_selection
                    .map(|selection| (selection.end_frame - selection.start_frame).max(0) as u64)
                    .unwrap_or(self.session.target.source_frames.max(0) as u64)
            })
            .max(1)
    }

    fn repair_transform(&self) -> CanvasTransform {
        let sample_rate = self.session.target.sample_rate.max(1);
        CanvasTransform {
            window_start_frame: self.repair_window_start(),
            window_frames: self.repair_window_frames(),
            sample_rate,
            max_hz: sample_rate as f32 * 0.5,
            frequency_scale: self.repair.frequency_scale,
            viewport: self.repair.viewport,
            width: self.repair.canvas_width.get(),
            height: self.repair.canvas_height.get(),
        }
    }

    fn repair_local_point(&self, position: gpui::Point<Pixels>) -> (f32, f32) {
        let (ox, oy) = self.repair.canvas_origin.get();
        (f32::from(position.x) - ox, f32::from(position.y) - oy)
    }

    // -------------------------------------------------------------- gestures

    fn repair_pointer_down(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let (x, y) = self.repair_local_point(position);
        let transform = self.repair_plane_transform();
        if x < 0.0 || x > transform.width || y < 0.0 || y > transform.height {
            return;
        }
        let frame = transform.x_to_source_frame(x);
        let hz = transform.y_to_hz(y);
        self.repair.drag = Some(RegionDrag {
            start_frame: frame,
            start_hz: hz,
            current_frame: frame,
            current_hz: hz,
        });
        cx.notify();
    }

    fn repair_pointer_move(
        &mut self,
        position: gpui::Point<Pixels>,
        dragging: bool,
        cx: &mut Context<Self>,
    ) {
        if !dragging {
            return;
        }
        let Some(mut drag) = self.repair.drag else {
            return;
        };
        let (x, y) = self.repair_local_point(position);
        let transform = self.repair_plane_transform();
        drag.current_frame = transform.x_to_source_frame(x.clamp(0.0, transform.width));
        drag.current_hz = transform.y_to_hz(y.clamp(0.0, transform.height));
        self.repair.drag = Some(drag);
        self.repair.region = Some(self.region_from_drag(drag));
        cx.notify();
    }

    fn repair_pointer_up(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut drag) = self.repair.drag.take() else {
            return;
        };
        let (x, y) = self.repair_local_point(position);
        let transform = self.repair_plane_transform();
        drag.current_frame = transform.x_to_source_frame(x.clamp(0.0, transform.width));
        drag.current_hz = transform.y_to_hz(y.clamp(0.0, transform.height));
        let region = self.region_from_drag(drag);
        // A click without movement clears the region instead of leaving a
        // zero-width one that silently processes nothing.
        self.repair.region = (region.end_frame - region.start_frame > 0).then_some(region);
        self.invalidate_repair_preview(cx);
        cx.notify();
    }

    fn region_from_drag(&self, drag: RegionDrag) -> CanvasRegion {
        let nyquist = self.session.target.sample_rate.max(1) as f32 * 0.5;
        let spectral = self.repair.module.uses_spectral_selection();
        let selection = SpectralSelection::new(
            drag.start_frame,
            drag.current_frame,
            if spectral { drag.start_hz } else { 0.0 },
            if spectral { drag.current_hz } else { nyquist },
        )
        .normalized();
        CanvasRegion {
            start_frame: selection.start_frame,
            end_frame: selection.end_frame,
            min_hz: selection.min_hz,
            max_hz: selection.max_hz,
        }
    }

    fn repair_plane_transform(&self) -> CanvasTransform {
        let model = ProcessingCanvas {
            transform: self.repair_transform(),
            view: self.repair.view,
            peaks: None,
            tiles: Vec::new(),
            tile_offset_seconds: 0.0,
            overlays: CanvasOverlays {
                diff: self.repair.diff.clone(),
                ..CanvasOverlays::default()
            },
            status: None,
            status_is_error: false,
        };
        model.plane_transform()
    }

    fn repair_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (dx, dy) = match event.delta {
            gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            gpui::ScrollDelta::Lines(p) => (p.x * 36.0, p.y * 36.0),
        };
        let width = self.repair.canvas_width.get();
        let frames = self.repair_window_frames();
        if event.modifiers.control || event.modifiers.platform {
            let (anchor_x, _) = self.repair_local_point(event.position);
            self.repair
                .viewport
                .zoom_around((1.0015_f32).powf(dy), anchor_x, frames, width);
        } else {
            self.repair.viewport.scroll_by(-(dx + dy), frames, width);
        }
        self.invalidate_repair_preview(cx);
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    fn repair_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        let width = self.repair.canvas_width.get();
        let frames = self.repair_window_frames();
        self.repair
            .viewport
            .zoom_around(factor, width * 0.5, frames, width);
        self.invalidate_repair_preview(cx);
        cx.notify();
    }

    fn step_click(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.repair.clicks.is_empty() {
            return;
        }
        let last = self.repair.clicks.len() - 1;
        let next = (self.repair.selected_click as i32 + delta).clamp(0, last as i32) as usize;
        self.repair.selected_click = next;
        // Following an event is a navigation gesture: centre it so the canvas
        // stays the place the user works, instead of reading a number here.
        let frame = self.repair.clicks[next].frame as f64;
        let width = self.repair.canvas_width.get();
        let span = self.repair.viewport.frames_per_pixel * width as f64;
        self.repair.viewport.start_frame = frame - span * 0.5;
        self.repair
            .viewport
            .clamp(self.repair_window_frames(), width);
        self.invalidate_repair_preview(cx);
        cx.notify();
    }

    /// Frames the profile learns from: the drawn region when there is one,
    /// otherwise the whole analyzed window.
    pub(super) fn repair_learn_range(&self, total_frames: usize) -> (usize, usize) {
        let window_start = self.repair_window_start() as i64;
        match self.repair.region {
            Some(region) => {
                let start = (region.start_frame - window_start).max(0) as usize;
                let end = ((region.end_frame - window_start).max(0) as usize).min(total_frames);
                (start.min(end), end)
            }
            None => (0, total_frames),
        }
    }

    // ---------------------------------------------------------------- render

    fn repair_overlays(&self) -> CanvasOverlays {
        let window_start = self.repair_window_start() as i64;
        let mut overlays = CanvasOverlays {
            region: self.repair.region,
            region_gain_db: if self.repair.module == AudioRepairModule::SpectralRepair {
                self.spectral_gain_db
            } else {
                0.0
            },
            diff: self.repair.diff.clone(),
            ..CanvasOverlays::default()
        };
        match self.repair.module {
            AudioRepairModule::Denoise => overlays.mask = self.repair.mask.clone(),
            AudioRepairModule::DeHum => {
                let params = self.dehum.sanitized();
                // Q follows the processor's own formula, so the drawn band is
                // the band the notch actually removes.
                let q = 18.0 + params.reduction_db * 0.4;
                overlays.bands = (1..=params.harmonics.max(1))
                    .map(|harmonic| {
                        let center = params.base_hz * harmonic as f32;
                        CanvasBand {
                            center_hz: center,
                            half_width_hz: center / (2.0 * q.max(0.1)),
                            depth_db: params.reduction_db,
                        }
                    })
                    .collect();
            }
            AudioRepairModule::DeClick => {
                overlays.event_tone = EventTone::Repair;
                overlays.selected_event = (!self.repair.clicks.is_empty())
                    .then(|| self.repair.selected_click.min(self.repair.clicks.len() - 1));
                overlays.events = self
                    .repair
                    .clicks
                    .iter()
                    .map(|event| CanvasEvent {
                        frame: event.frame as u64,
                        width_frames: event.width as u32,
                        strength: event.strength,
                    })
                    .collect();
            }
            _ => {}
        }
        if self.repair.show_transients {
            let sample_rate = self.session.target.sample_rate.max(1) as f64;
            overlays.transients = self
                .transients
                .iter()
                .filter_map(|marker| {
                    let frame = marker.source_frame as i64 - window_start;
                    (frame >= 0).then_some(CanvasEvent {
                        frame: frame as u64,
                        width_frames: (sample_rate * 0.002) as u32,
                        strength: marker.strength,
                    })
                })
                .collect();
        }
        overlays
    }

    fn repair_canvas_status(&self) -> Option<String> {
        if self.repair.peaks.is_none() {
            return Some(
                self.repair
                    .spectrogram_status
                    .clone()
                    .unwrap_or_else(|| "Analyzing source…".to_string()),
            );
        }
        if self.repair.view.shows_spectrogram() && self.repair.tiles.is_empty() {
            return self.repair.spectrogram_status.clone();
        }
        None
    }

    fn repair_graph_draw(&self) -> super::viz::GraphDraw {
        super::viz::GraphDraw {
            smoothing: self.display_smoothing,
            style: self.graph_style,
            hover: self.graph_hover,
        }
    }

    // ------------------------------------------------------------------ jobs

    /// Called once the analysis pass has decoded the window.
    pub(super) fn on_repair_source_ready(&mut self, cx: &mut Context<Self>) {
        let Some(pcm) = self.source_pcm.clone() else {
            return;
        };
        let channels = self.source_channels.max(1);
        let peaks = WaveformPeaks::build(pcm, channels);
        let frames = peaks.frames() as u64;
        self.repair.peaks = Some(peaks);
        if !self.repair.fitted {
            self.repair.viewport = CanvasViewport::fit(frames, self.repair.canvas_width.get());
            self.repair.fitted = true;
        }
        self.repair
            .viewport
            .clamp(frames, self.repair.canvas_width.get());
        if self.repair.region.is_none() {
            self.repair.region = self.session.target.spectral_selection.map(|selection| {
                let selection = selection.normalized();
                CanvasRegion {
                    start_frame: selection.start_frame,
                    end_frame: selection.end_frame,
                    min_hz: selection.min_hz,
                    max_hz: selection.max_hz,
                }
            });
        }
        if self.repair.show_transients {
            self.detect_repair_transients();
        }
        self.spawn_repair_spectrogram(cx);
        self.invalidate_repair_preview(cx);
    }

    fn detect_repair_transients(&mut self) {
        let Some(pcm) = self.source_pcm.as_ref() else {
            return;
        };
        let channels = self.source_channels.max(1);
        let mono = downmix_interleaved(pcm, channels);
        let nyquist = self.session.target.sample_rate as f32 * 0.5;
        let (low, high) = (20.0_f32, nyquist);
        self.transients = SphereAudioProcessor::detect_transients(
            &mono,
            self.session.target.sample_rate.max(1),
            SphereAudioProcessor::TransientDetectParams {
                freq_low_hz: low,
                freq_high_hz: high,
                ..self.transient_params
            },
        );
    }

    /// Reuse the audio editor's shared spectrogram cache: the same clip opened
    /// in both surfaces analyzes once.
    pub(super) fn spawn_repair_spectrogram(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.session.target.source_path.clone() else {
            self.repair.spectrogram_status = Some("Clip has no source file".to_string());
            return;
        };
        let params = SpectrogramJobParams {
            frequency_scale: self.repair.frequency_scale,
            pitch_semitones: 0.0,
            reverse: false,
        };
        self.repair.spectrogram_generation = self.repair.spectrogram_generation.wrapping_add(1);
        let generation = self.repair.spectrogram_generation;
        self.repair.spectrogram_status = Some("Building spectrogram…".to_string());
        let host = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { cached_or_analyze(&path, params) })
                .await;
            let _ = host.update(cx, |this, cx| {
                if this.repair.spectrogram_generation != generation {
                    return;
                }
                match result {
                    Ok(rendered) => {
                        this.repair.tiles = rendered.tiles.clone();
                        this.repair.tile_offset_seconds = this.repair_window_start() as f32
                            / this.session.target.sample_rate.max(1) as f32;
                        this.repair.spectrogram_status = None;
                    }
                    Err(error) => this.repair.spectrogram_status = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Mark the canvas preview as out of date and start a pass when the worker
    /// is free. In-flight passes are never cancelled mid-FFT; the generation
    /// guard drops their result instead.
    pub(super) fn invalidate_repair_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(diff) = self.repair.diff.as_mut() {
            diff.stale = true;
        }
        if self.repair.preview_busy {
            self.repair.preview_dirty = true;
            return;
        }
        self.spawn_repair_preview(cx);
    }

    fn spawn_repair_preview(&mut self, cx: &mut Context<Self>) {
        let Some(pcm) = self.source_pcm.clone() else {
            return;
        };
        let channels = self.source_channels.max(1);
        let total_frames = pcm.len() / channels;
        let transform = self.repair_transform();
        let (visible_start, visible_end) = transform.visible_frames();
        let start = (visible_start as usize).min(total_frames);
        let end = ((visible_end as usize).max(start + 1)).min(total_frames);
        let end = end.min(start + PREVIEW_MAX_FRAMES as usize);
        if end <= start {
            return;
        }

        let module = self.repair.module;
        let sample_rate = self.session.target.sample_rate.max(1);
        let profile = self.learned_noise.clone();
        let denoise = self.denoise;
        let declick = self.declick;
        let dehum = self.dehum;
        let gain_db = self.spectral_gain_db;
        let region = self.repair.region;
        let window_start = self.repair_window_start() as i64;
        let frequency_scale = self.repair.frequency_scale;

        self.repair.preview_busy = true;
        self.repair.preview_dirty = false;
        self.repair.preview_generation = self.repair.preview_generation.wrapping_add(1);
        let generation = self.repair.preview_generation;
        let host = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    compute_preview(PreviewRequest {
                        module,
                        pcm,
                        channels,
                        start,
                        end,
                        sample_rate,
                        profile,
                        denoise,
                        declick,
                        dehum,
                        gain_db,
                        region,
                        window_start,
                        frequency_scale,
                    })
                })
                .await;
            let _ = host.update(cx, |this, cx| {
                this.repair.preview_busy = false;
                if this.repair.preview_generation == generation {
                    this.repair.diff = result.diff;
                    this.repair.mask = result.mask;
                    this.repair.clicks = result.clicks;
                    this.repair.selected_click = this
                        .repair
                        .selected_click
                        .min(this.repair.clicks.len().saturating_sub(1));
                    this.repair.stats = Some(result.stats);
                    let _ = (result.start_frame, result.end_frame);
                }
                if this.repair.preview_dirty {
                    this.spawn_repair_preview(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn tick_audition_level(&mut self) {
        let mut left = vec![0.0; AUDITION_WINDOW];
        let mut right = vec![0.0; AUDITION_WINDOW];
        let read = DirectAudio::analysis_tap().copy_recent(&mut left, &mut right);
        if read == 0 {
            return;
        }
        let peak = peak_amplitude(&left[..read]).max(peak_amplitude(&right[..read]));
        let db = lin_to_db(peak);
        // Fast attack, slow release, so a transient is visible without the
        // meter flickering at the refresh rate.
        self.repair.audition_peak_db = if db > self.repair.audition_peak_db {
            db
        } else {
            self.repair.audition_peak_db * 0.82 + db * 0.18
        };
    }

    pub(super) fn repair_audition_enabled(&self) -> bool {
        has_engine_audition(self.repair.module)
    }

    /// Emit an engine preview only for modules the engine can actually apply.
    fn emit_repair_preview(&mut self, cx: &mut App) {
        if self.session.preview_enabled && has_engine_audition(self.repair.module) {
            self.emit_preview(cx);
        }
    }
}

// -------------------------------------------------------------------- worker

struct PreviewRequest {
    module: AudioRepairModule,
    pcm: Arc<[f32]>,
    channels: usize,
    start: usize,
    end: usize,
    sample_rate: u32,
    profile: Option<Vec<f32>>,
    denoise: SpectralDenoiseParams,
    declick: DeclickParams,
    dehum: DehumParams,
    gain_db: f32,
    region: Option<CanvasRegion>,
    window_start: i64,
    frequency_scale: FrequencyScale,
}

/// Run the active processor over the visible range and measure the result.
///
/// Background executor only. This is the same DSP the commit path runs, so the
/// canvas is showing the real outcome rather than a model of it.
fn compute_preview(request: PreviewRequest) -> PreviewResult {
    let PreviewRequest {
        module,
        pcm,
        channels,
        start,
        end,
        sample_rate,
        profile,
        denoise,
        declick,
        dehum,
        gain_db,
        region,
        window_start,
        frequency_scale,
    } = request;

    let slice = &pcm[(start * channels).min(pcm.len())..(end * channels).min(pcm.len())];
    let mut stats = RepairStats::default();
    let mut mask = None;
    let mut clicks = Vec::new();
    let mut processed: Option<Vec<f32>> = None;

    match module {
        AudioRepairModule::Denoise => {
            if let Some(profile) = profile {
                let mono = downmix_interleaved(slice, channels);
                if let Some(gate) = noise_gate_mask(&mono, &profile, denoise) {
                    stats.coverage = Some(gate.coverage());
                    mask = build_mask_image(&gate, sample_rate, frequency_scale).map(|image| {
                        CanvasMask {
                            image,
                            start_frame: start as u64,
                            end_frame: end as u64,
                        }
                    });
                }
                let reduced = reduce_noise_stft(&mono, sample_rate, &profile, denoise);
                processed = Some(spread_mono(&reduced, channels));
            }
        }
        AudioRepairModule::DeClick => {
            clicks = detect_clicks(slice, channels, declick);
            stats.events = clicks.len();
            processed = Some(declick_interleaved(slice, channels, declick).0);
        }
        AudioRepairModule::DeHum => {
            let mut processor = DehumProcessor::new(sample_rate, dehum);
            processor.prepare(sample_rate, channels, slice.len() / channels.max(1));
            let mut out = vec![0.0; slice.len()];
            processor.process(slice, &mut out);
            processed = Some(out);
        }
        AudioRepairModule::SpectralRepair => {
            if let Some(region) = region {
                let mono = downmix_interleaved(slice, channels);
                let params = SpectralGainParams {
                    start_frame: (region.start_frame - window_start - start as i64).max(0),
                    end_frame: (region.end_frame - window_start - start as i64).max(0),
                    min_hz: region.min_hz,
                    max_hz: region.max_hz,
                    gain: db_to_lin(gain_db),
                    fade_bins: 4,
                };
                let out = apply_spectral_gain(&mono, sample_rate, params, StftSettings::default());
                processed = Some(spread_mono(&out, channels));
            }
        }
        _ => {}
    }

    stats.peak_before_db = lin_to_db(peak_amplitude(slice));
    stats.rms_before_db = lin_to_db(rms(slice));
    let diff = processed.as_ref().map(|out| {
        stats.peak_after_db = lin_to_db(peak_amplitude(out));
        stats.rms_after_db = lin_to_db(rms(out));
        CanvasDiff {
            before: viz::hop_levels(&downmix_interleaved(slice, channels), ENVELOPE_HOPS),
            after: viz::hop_levels(&downmix_interleaved(out, channels), ENVELOPE_HOPS),
            start_frame: start as u64,
            end_frame: end as u64,
            stale: false,
        }
    });
    if processed.is_none() {
        stats.peak_after_db = stats.peak_before_db;
        stats.rms_after_db = stats.rms_before_db;
    }

    PreviewResult {
        start_frame: start as u64,
        end_frame: end as u64,
        diff,
        mask,
        clicks,
        stats,
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

fn spread_mono(mono: &[f32], channels: usize) -> Vec<f32> {
    let channels = channels.max(1);
    let mut out = Vec::with_capacity(mono.len() * channels);
    for sample in mono {
        for _ in 0..channels {
            out.push(*sample);
        }
    }
    out
}

// ------------------------------------------------------------------ elements

/// One numbered step of a module's workflow: a disc and a title, with the
/// step's controls indented under the title. A finished step's disc turns
/// into a check, so the panel shows how far along the user is.
fn repair_step(
    number: u8,
    title: &'static str,
    done: bool,
    content: gpui::AnyElement,
) -> impl IntoElement {
    let disc = div()
        .size(px(STEP_DISC))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::PILL));
    let disc = if done {
        disc.bg(Colors::with_alpha(Colors::status_success(), 0.18))
            .child(
                svg()
                    .path(assets::ICON_CHECK_PATH)
                    .size(px(12.0))
                    .text_color(Colors::status_success()),
            )
    } else {
        disc.bg(Colors::surface_raised())
            .text_size(px(typography::DENSE_LABEL))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_secondary())
            .child(number.to_string())
    };
    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(disc)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(title),
                ),
        )
        .child(div().pl(px(STEP_DISC + space::BASE)).child(content))
}

/// A line of guidance under a step.
fn step_note(text: String) -> impl IntoElement {
    div()
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_muted())
        .child(text)
}

/// Label and value on one line, the control on the next. At the inspector's
/// fixed width a side-by-side slider would be too short to aim with.
fn repair_param(label: &'static str, value: String, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(px(size::MICRO))
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(value),
                ),
        )
        .child(control)
}

fn readout_row(label: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(size::MICRO + space::TIGHT))
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(value),
        )
}

fn bind_repair(
    cx: &mut Context<AudioToolWindow>,
    write: impl Fn(&mut AudioToolWindow, f32, &mut Context<AudioToolWindow>) + 'static,
) -> impl Fn(f32, &mut Window, &mut App) + 'static {
    let handle = cx.entity();
    move |value, _window, cx| {
        handle.update(cx, |this, cx| {
            write(this, value, cx);
            cx.notify();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silence(frames: usize, channels: usize) -> Arc<[f32]> {
        (0..frames * channels)
            .map(|i| ((i % 13) as f32 - 6.0) * 1.0e-4)
            .collect::<Vec<_>>()
            .into()
    }

    #[test]
    fn declick_preview_reports_the_events_it_repairs() {
        let mut samples: Vec<f32> = (0..8192)
            .flat_map(|i| {
                let value = (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin() * 0.2;
                [value, value]
            })
            .collect();
        samples[2048] = 0.98;
        samples[2049] = 0.98;
        let result = compute_preview(PreviewRequest {
            module: AudioRepairModule::DeClick,
            pcm: samples.into(),
            channels: 2,
            start: 0,
            end: 8192,
            sample_rate: 48_000,
            profile: None,
            denoise: SpectralDenoiseParams::default(),
            declick: DeclickParams::default(),
            dehum: DehumParams::default(),
            gain_db: 0.0,
            region: None,
            window_start: 0,
            frequency_scale: FrequencyScale::Logarithmic,
        });
        assert!(result.stats.events > 0);
        assert_eq!(result.clicks.len(), result.stats.events);
        assert!(result.stats.peak_after_db < result.stats.peak_before_db);
        assert!(result.diff.is_some());
    }

    #[test]
    fn denoise_preview_without_a_profile_reports_no_change() {
        let result = compute_preview(PreviewRequest {
            module: AudioRepairModule::Denoise,
            pcm: silence(8192, 2),
            channels: 2,
            start: 0,
            end: 8192,
            sample_rate: 48_000,
            profile: None,
            denoise: SpectralDenoiseParams::default(),
            declick: DeclickParams::default(),
            dehum: DehumParams::default(),
            gain_db: 0.0,
            region: None,
            window_start: 0,
            frequency_scale: FrequencyScale::Logarithmic,
        });
        assert!(result.mask.is_none());
        assert!(result.diff.is_none());
        assert_eq!(result.stats.peak_before_db, result.stats.peak_after_db);
    }

    #[test]
    fn only_modules_with_an_engine_path_can_be_auditioned() {
        assert!(has_engine_audition(AudioRepairModule::Denoise));
        assert!(has_engine_audition(AudioRepairModule::DeHum));
        assert!(!has_engine_audition(AudioRepairModule::DeClick));
        assert!(!has_engine_audition(AudioRepairModule::SpectralRepair));
    }
}
