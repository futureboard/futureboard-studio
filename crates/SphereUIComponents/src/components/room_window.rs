//! 3D Room Preview — the spatial mix in 3D, rendered with wgpu: the room as a
//! box around the listener's head, the layout's speakers on its walls and
//! ceiling lit by their levels, and every placed channel as a sphere where it
//! is heard now, with its height, width, spread and shadow.
//!
//! Dragging empty space orbits the camera, the wheel zooms, and the 3D / Top /
//! Front buttons jump to a view. A channel is dragged to move it across the
//! room at its own height (Shift drags finely, from where it is), as on the
//! Mixer's room panner; the move is one undo step and reaches the engine live.
//! While a channel's spatial automation plays, its sphere follows the lanes at
//! the playhead and a ghost marks its own placement, which is what a drag
//! moves (and what overrides the lanes, as a hand move does).
//!
//! The scene and camera are [`crate::components::room3d`]; its GPU renderer
//! shares the visualizers' device. Where no GPU can draw it, the view falls
//! back to the room from above, and says so.
//!
//! Like the other utility windows it never reads the Studio from `render`:
//! the Studio pushes a [`RoomSnapshot`] in — on every change, and on the meter
//! tick while the window is open — and choices go back out through
//! [`RoomCallbacks`]. Spatial mixing is a Professional feature: without the
//! license the window says so and draws nothing it cannot honour.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled, Window, WindowBounds,
    WindowHandle, WindowKind,
};
use solfege_spatialaudio::{RoomPosition, SourceParams, SpatialFormat};

use crate::components::controls::{
    fb_badge, fb_button, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::room3d::{self, CameraPreset, OrbitCamera};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{self, radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

const WINDOW_WIDTH: f32 = 860.0;
const WINDOW_HEIGHT: f32 = 620.0;
const WINDOW_MIN_WIDTH: f32 = 560.0;
const WINDOW_MIN_HEIGHT: f32 = 420.0;
const LIST_WIDTH: f32 = 236.0;
/// Room left around the walls for the speakers' labels.
const WALL_MARGIN: f32 = 30.0;
/// How close (px) a press has to land to a channel to pick it up.
const HIT_RADIUS: f32 = 14.0;

/// One placed channel as the window draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomSource {
    pub id: String,
    pub name: String,
    pub color: gpui::Rgba,
    /// Where it is heard at the playhead: its placement with any playing
    /// spatial lane applied.
    pub heard: SourceParams,
    /// Its own placement — what a drag moves.
    pub base: SourceParams,
    /// A spatial lane is playing on it.
    pub automated: bool,
    /// The bus it is placed inside, when it is routed into a bus that keeps
    /// its channels' places.
    pub bus: Option<String>,
}

/// What the window draws, pushed in by the Studio.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomSnapshot {
    /// This machine's license grants spatial mixing.
    pub licensed: bool,
    /// The project's saved format.
    pub saved_format: SpatialFormat,
    /// The format the mix plays in here.
    pub format: SpatialFormat,
    pub sources: Vec<RoomSource>,
    /// Each speaker feed's peak (layout order), or the two ears of a binaural
    /// mix, since the last tick. Empty when no engine is metering.
    pub speaker_peaks: Vec<f32>,
    /// The selected track, highlighted.
    pub selected: Option<String>,
}

impl Default for RoomSnapshot {
    fn default() -> Self {
        Self {
            licensed: false,
            saved_format: SpatialFormat::Stereo,
            format: SpatialFormat::Stereo,
            sources: Vec::new(),
            speaker_peaks: Vec::new(),
            selected: None,
        }
    }
}

type IdCb = Arc<dyn Fn(&str, &mut App) + 'static>;
type MoveCb = Arc<dyn Fn(&str, SourceParams, &mut App) + 'static>;

/// What the window asks the Studio to do. Every callback defers its Studio
/// work, so calling it from inside this window's update cannot re-enter it.
#[derive(Clone)]
pub struct RoomCallbacks {
    /// A drag on a channel begins.
    pub on_drag_start: IdCb,
    /// A live value of that drag.
    pub on_drag_preview: MoveCb,
    /// Its release: one undo step.
    pub on_drag_commit: IdCb,
    /// Select the channel (clicked in the room or the list).
    pub on_select: IdCb,
    /// Open Project Settings, where the format is chosen.
    pub on_open_project_settings: Arc<dyn Fn(&mut App) + 'static>,
}

/// A drag in progress.
#[derive(Debug, Clone)]
struct RoomDrag {
    /// The drag moves the camera, not a channel.
    orbit: bool,
    /// Where the pointer was at the last move (orbiting).
    last: Point<Pixels>,
    id: String,
    /// The placement the drag moves.
    params: SourceParams,
    /// `(pointer, position)` where fine dragging (Shift) began.
    fine_anchor: Option<(Point<Pixels>, RoomPosition)>,
}

/// Where the last paint put the room and each channel, for hit-testing.
#[derive(Default)]
struct PaintedRoom {
    /// The flat view's square (fallback only).
    square: Cell<Option<Bounds<Pixels>>>,
    /// The whole view, which the 3D camera projects into.
    view: Cell<Option<Bounds<Pixels>>>,
    /// The last frame was drawn in 3D.
    three_d: Cell<bool>,
    dots: RefCell<Vec<(String, Point<Pixels>)>>,
}

pub struct RoomWindow {
    focus_handle: FocusHandle,
    snapshot: RoomSnapshot,
    callbacks: RoomCallbacks,
    painted: Rc<PaintedRoom>,
    drag: Option<RoomDrag>,
    camera: OrbitCamera,
    #[cfg(feature = "gpu-renderer")]
    gpu: Rc<RefCell<room3d::gpu::Room3dCanvas>>,
}

impl RoomWindow {
    pub(crate) fn new(
        snapshot: RoomSnapshot,
        callbacks: RoomCallbacks,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            snapshot,
            callbacks,
            painted: Rc::default(),
            drag: None,
            camera: OrbitCamera::default(),
            #[cfg(feature = "gpu-renderer")]
            gpu: Rc::default(),
        }
    }

    /// The Studio's current state. A channel being dragged keeps the place
    /// the drag gave it, so a tick that lands mid-drag cannot pull it back.
    pub fn set_snapshot(&mut self, mut snapshot: RoomSnapshot, cx: &mut Context<Self>) {
        if let Some(drag) = &self.drag {
            if let Some(source) = snapshot.sources.iter_mut().find(|s| s.id == drag.id) {
                source.base = drag.params;
                if !source.automated {
                    source.heard = drag.params;
                }
            }
        }
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            cx.notify();
        }
    }

    /// Room coordinates of a window point at height `z`: through the camera
    /// in 3D, in the last painted square when flat.
    fn room_at(&self, point: Point<Pixels>, z: f32) -> Option<(f32, f32)> {
        if self.painted.three_d.get() {
            let view = self.painted.view.get()?;
            let at = self.camera.room_at(
                f32::from(point.x - view.origin.x),
                f32::from(point.y - view.origin.y),
                f32::from(view.size.width),
                f32::from(view.size.height),
                z,
            )?;
            return Some((at.x, at.y));
        }
        let square = self.painted.square.get()?;
        let side = f32::from(square.size.width).max(1.0);
        let x = (f32::from(point.x) - f32::from(square.origin.x)) / side * 2.0 - 1.0;
        let y = 1.0 - (f32::from(point.y) - f32::from(square.origin.y)) / side * 2.0;
        Some((x, y))
    }

    fn on_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let hit = {
            let dots = self.painted.dots.borrow();
            dots.iter()
                .map(|(id, at)| {
                    let dx = f32::from(at.x - event.position.x);
                    let dy = f32::from(at.y - event.position.y);
                    (id.clone(), dx * dx + dy * dy)
                })
                .filter(|(_, d2)| *d2 <= HIT_RADIUS * HIT_RADIUS)
                // The topmost (last painted) of overlapping channels.
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, _)| id)
        };
        let Some(id) = hit else {
            // Empty space in 3D turns the camera.
            if self.painted.three_d.get() {
                if event.click_count > 1 {
                    self.camera = OrbitCamera::default();
                    cx.notify();
                    return;
                }
                self.drag = Some(RoomDrag {
                    orbit: true,
                    last: event.position,
                    id: String::new(),
                    params: SourceParams::default(),
                    fine_anchor: None,
                });
            }
            return;
        };
        let Some(source) = self.snapshot.sources.iter().find(|s| s.id == id) else {
            return;
        };
        (self.callbacks.on_select)(&id, cx);
        if event.click_count > 1 || event.modifiers.alt {
            // Back front and centre, as the room panner's reset does.
            let params = SourceParams {
                position: RoomPosition::FRONT,
                ..source.base
            };
            (self.callbacks.on_drag_start)(&id, cx);
            (self.callbacks.on_drag_preview)(&id, params, cx);
            (self.callbacks.on_drag_commit)(&id, cx);
            return;
        }
        (self.callbacks.on_drag_start)(&id, cx);
        self.drag = Some(RoomDrag {
            orbit: false,
            last: event.position,
            id,
            params: source.base,
            fine_anchor: event
                .modifiers
                .shift
                .then_some((event.position, source.base.position)),
        });
        cx.notify();
    }

    fn on_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            // Released outside anything that reported it.
            if !drag.orbit {
                (self.callbacks.on_drag_commit)(&drag.id, cx);
            }
            cx.notify();
            return;
        }
        if drag.orbit {
            let dx = f32::from(event.position.x - drag.last.x);
            let dy = f32::from(event.position.y - drag.last.y);
            drag.last = event.position;
            self.camera.orbit(dx, dy);
            self.drag = Some(drag);
            cx.notify();
            return;
        }
        let z = drag.params.position.z;
        let position = if event.modifiers.shift && self.painted.three_d.get() {
            // A tenth as far as the pointer goes, measured on the channel's
            // own plane.
            let (anchor, from) = *drag
                .fine_anchor
                .get_or_insert((event.position, drag.params.position));
            match (self.room_at(anchor, z), self.room_at(event.position, z)) {
                (Some(a), Some(b)) => {
                    RoomPosition::new(from.x + (b.0 - a.0) * 0.1, from.y + (b.1 - a.1) * 0.1, z)
                }
                _ => drag.params.position,
            }
        } else if event.modifiers.shift {
            let (anchor, from) = *drag
                .fine_anchor
                .get_or_insert((event.position, drag.params.position));
            let side = self
                .painted
                .square
                .get()
                .map(|square| f32::from(square.size.width))
                .unwrap_or(1.0)
                .max(1.0);
            // A tenth as far as the pointer goes.
            let dx = f32::from(event.position.x - anchor.x) / side * 2.0 * 0.1;
            let dy = f32::from(event.position.y - anchor.y) / side * 2.0 * 0.1;
            RoomPosition::new(from.x + dx, from.y - dy, z)
        } else {
            drag.fine_anchor = None;
            match self.room_at(event.position, z) {
                Some((x, y)) => RoomPosition::new(x, y, z),
                None => drag.params.position,
            }
        };
        if position != drag.params.position {
            drag.params.position = position;
            (self.callbacks.on_drag_preview)(&drag.id, drag.params, cx);
            if let Some(source) = self.snapshot.sources.iter_mut().find(|s| s.id == drag.id) {
                source.base = drag.params;
                if !source.automated {
                    source.heard = drag.params;
                }
            }
            cx.notify();
        }
        self.drag = Some(drag);
    }

    fn on_up(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            if !drag.orbit {
                (self.callbacks.on_drag_commit)(&drag.id, cx);
            }
            cx.notify();
        }
    }

    fn on_wheel(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut Context<Self>) {
        if !self.painted.three_d.get() {
            return;
        }
        let dy = f32::from(event.delta.pixel_delta(px(20.0)).y);
        self.camera.zoom(dy / 40.0);
        cx.notify();
    }
}

impl Render for RoomWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = &self.snapshot;
        let body = if !snapshot.licensed {
            notice(
                "Spatial mixing is part of Futureboard Professional",
                if snapshot.saved_format.is_spatial() {
                    format!(
                        "This project is mixed in {}. Here it plays as stereo, and every \
                         channel keeps its place for when it is opened with a license.",
                        snapshot.saved_format.name()
                    )
                } else {
                    "Binaural and surround mixing, the room panner and this window come \
                     with a Professional license."
                        .to_string()
                },
                None,
            )
            .into_any_element()
        } else if !snapshot.format.is_spatial() {
            let open = self.callbacks.on_open_project_settings.clone();
            notice(
                "The mix is stereo",
                "Choose Binaural or a speaker layout under Spatial in Project Settings to \
                 place channels in the room."
                    .to_string(),
                Some(
                    fb_button(
                        "room-open-project-settings",
                        "Project Settings",
                        FbButtonKind::Default,
                        true,
                        move |_, _window, cx| open(cx),
                    )
                    .into_any_element(),
                ),
            )
            .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_row()
                .gap(px(space::LOOSE))
                .p(px(space::SECTION))
                .child(self.room_view(cx))
                .child(self.channel_list(cx))
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .text_size(px(typography::UI_SM))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key.as_str() == "escape" {
                    if let Some(drag) = this.drag.take() {
                        if !drag.orbit {
                            (this.callbacks.on_drag_commit)(&drag.id, cx);
                        }
                        cx.notify();
                    } else {
                        window.remove_window();
                    }
                }
            }))
            // The drag follows the pointer anywhere in the window.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                this.on_move(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| this.on_up(event, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| this.on_up(event, cx)),
            )
            .child(external_window_titlebar(
                "3D Room Preview",
                "room-close",
                move |window, _cx| window.remove_window(),
            ))
            .child(body)
    }
}

impl RoomWindow {
    /// The room, in 3D through wgpu or flat from above, painted in one canvas
    /// with the camera buttons over it.
    fn room_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot.clone();
        let painted = self.painted.clone();
        let dragging = self
            .drag
            .as_ref()
            .filter(|drag| !drag.orbit)
            .map(|drag| drag.id.clone());
        let camera = self.camera;
        #[cfg(feature = "gpu-renderer")]
        let gpu = self.gpu.clone();
        #[cfg(feature = "gpu-renderer")]
        let gpu_failed = self.gpu.borrow().failed();
        #[cfg(not(feature = "gpu-renderer"))]
        let gpu_failed = true;
        let orbiting = self.drag.as_ref().is_some_and(|drag| drag.orbit);
        div()
            .id("room-view")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .rounded(px(radius::SURFACE))
            .bg(Colors::surface_panel())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .overflow_hidden()
            .cursor(if orbiting {
                gpui::CursorStyle::ClosedHand
            } else {
                gpui::CursorStyle::Crosshair
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| this.on_down(event, cx)),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    this.on_wheel(event, cx)
                }),
            )
            .child(
                canvas(
                    |_bounds, _window, _cx| {},
                    move |bounds, _state, window, cx| {
                        painted.view.set(Some(bounds));
                        #[cfg(feature = "gpu-renderer")]
                        if paint_room_3d(
                            &snapshot,
                            dragging.as_deref(),
                            &camera,
                            &mut gpu.borrow_mut(),
                            &painted,
                            bounds,
                            window,
                            cx,
                        ) {
                            painted.three_d.set(true);
                            return;
                        }
                        let _ = &camera;
                        painted.three_d.set(false);
                        paint_room(&snapshot, dragging.as_deref(), &painted, bounds, window, cx);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(self.view_bar(gpu_failed, cx))
    }

    /// The camera buttons; none when the room is drawn flat (the legend says
    /// why).
    fn view_bar(&self, flat: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let bar = div()
            .absolute()
            .top(px(space::BASE))
            .left(px(space::BASE))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE));
        if flat {
            return bar;
        }
        let current = self.camera.at_preset();
        let mut presets = fb_segmented_track();
        for (index, preset) in CameraPreset::ALL.into_iter().enumerate() {
            let position = match index {
                0 => FbSegment::First,
                i if i + 1 == CameraPreset::ALL.len() => FbSegment::Last,
                _ => FbSegment::Middle,
            };
            presets = presets.child(fb_segment(
                ("room-camera", index),
                preset.label(),
                current == Some(preset),
                position,
                cx.listener(move |this, _event, _window, cx| {
                    this.camera = OrbitCamera::preset(preset);
                    cx.notify();
                }),
            ));
        }
        bar.child(div().w(px(168.0)).child(presets)).child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child("Drag the room to orbit, scroll to zoom"),
        )
    }

    /// Whether the room is drawn flat because no GPU can draw it in 3D.
    fn flat(&self) -> bool {
        #[cfg(feature = "gpu-renderer")]
        {
            self.gpu.borrow().failed()
        }
        #[cfg(not(feature = "gpu-renderer"))]
        {
            true
        }
    }

    /// Every placed channel: where it is, and a click to select it.
    fn channel_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = &self.snapshot;
        let mut rows = div()
            .id("room-channel-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(space::HAIR));
        if snapshot.sources.is_empty() {
            rows = rows.child(
                div()
                    .p(px(space::BASE))
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child("No channel is placed in the room yet."),
            );
        }
        for (index, source) in snapshot.sources.iter().enumerate() {
            let selected = snapshot.selected.as_deref() == Some(source.id.as_str());
            let rest = if selected {
                Colors::composite(Colors::surface_panel(), Colors::state_selected())
            } else {
                Colors::surface_panel()
            };
            let hover = Colors::composite(rest, Colors::state_hover());
            let on_select = self.callbacks.on_select.clone();
            let id = source.id.clone();
            let detail = match &source.bus {
                Some(bus) => format!(
                    "{} · in {bus}",
                    crate::components::mixer_panel::describe_room_position(&source.heard)
                ),
                None => crate::components::mixer_panel::describe_room_position(&source.heard),
            };
            rows = rows.child(
                div()
                    .id(("room-channel", index))
                    .relative()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .px(px(space::BASE))
                    .py(px(space::TIGHT))
                    .rounded(px(radius::CONTROL))
                    .bg(rest)
                    .hover(move |s| s.bg(hover))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_click(cx.listener(move |_this, _event, _window, cx| on_select(&id, cx)))
                    .when(selected, |row| {
                        row.child(
                            div()
                                .absolute()
                                .left_0()
                                .top(px(space::TIGHT))
                                .bottom(px(space::TIGHT))
                                .w(px(2.0))
                                .bg(Colors::accent_primary()),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .size(px(10.0))
                            .rounded(px(5.0))
                            .bg(source.color),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(typography::UI_SM))
                                    .child(source.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(typography::UI_XS))
                                    .text_color(Colors::text_muted())
                                    .child(detail),
                            ),
                    )
                    .when(source.automated, |row| {
                        row.child(fb_badge("Auto", Colors::state_automation()))
                    }),
            );
        }

        div()
            .w(px(LIST_WIDTH))
            .flex_none()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_muted())
                    .child(format!(
                        "CHANNELS · {}",
                        snapshot.format.name().to_uppercase()
                    )),
            )
            .child(rows)
            .child(speaker_legend(snapshot, self.flat()))
    }
}

/// The LFE's level (it has no place in the room) and how to read the room.
fn speaker_legend(snapshot: &RoomSnapshot, flat: bool) -> impl IntoElement {
    let lfe = match snapshot.format {
        SpatialFormat::Surround(layout) => layout
            .lfe_channel()
            .map(|channel| snapshot.speaker_peaks.get(channel).copied().unwrap_or(0.0)),
        _ => None,
    };
    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .when(flat, |card| {
            card.child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::status_warning())
                    .child("3D view unavailable on this machine: the room is drawn from above."),
            )
        })
        .when_some(lfe, |card, peak| {
            card.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .w(px(28.0))
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child("LFE"),
                    )
                    .child(level_bar(peak)),
            )
        })
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(
                    "Drag a channel to move it at its height (set height in the Inspector); \
                     Shift moves finely, a double-click puts it back front and centre. Speaker \
                     levels are after the master fader.",
                ),
        )
}

/// A horizontal level bar, `-60..0 dB` across.
fn level_bar(peak: f32) -> impl IntoElement {
    let fraction = level_fraction(peak);
    div().flex_1().h(px(6.0)).bg(Colors::meter_rail()).child(
        div()
            .h_full()
            .w(gpui::relative(fraction))
            .bg(level_color(peak)),
    )
}

/// `-60..0 dBFS` as `0..1`.
fn level_fraction(peak: f32) -> f32 {
    if peak <= 0.0 || !peak.is_finite() {
        return 0.0;
    }
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn level_color(peak: f32) -> gpui::Rgba {
    if peak >= 1.0 {
        Colors::meter_clip()
    } else if peak >= 0.5 {
        Colors::meter_high()
    } else if peak >= 0.125 {
        Colors::meter_mid()
    } else {
        Colors::meter_low()
    }
}

/// A notice in place of the room, with an optional action.
fn notice(title: &'static str, text: String, action: Option<gpui::AnyElement>) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p(px(space::PAGE))
        .child(
            div()
                .w(px(420.0))
                .max_w_full()
                .flex()
                .flex_col()
                .gap(px(space::BASE))
                .p(px(space::SECTION))
                .rounded(px(radius::SURFACE))
                .bg(Colors::surface_panel())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .text_size(px(typography::UI_TITLE))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_SM))
                        .text_color(Colors::text_secondary())
                        .child(text),
                )
                .children(action),
        )
}

/// `color` as the renderer's `0..1` RGBA.
fn rgba(color: gpui::Rgba) -> [f32; 4] {
    [color.r, color.g, color.b, color.a]
}

/// Draw the room in 3D through wgpu, then the names over it, and record
/// where each channel landed for picking. False: the GPU cannot, and the
/// caller draws the room flat.
#[cfg(feature = "gpu-renderer")]
#[allow(clippy::too_many_arguments)]
fn paint_room_3d(
    snapshot: &RoomSnapshot,
    dragging: Option<&str>,
    camera: &OrbitCamera,
    gpu: &mut room3d::gpu::Room3dCanvas,
    painted: &PaintedRoom,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let palette = room3d::RoomPalette {
        background: rgba(Colors::surface_canvas()),
        floor: rgba(Colors::composite(
            Colors::surface_canvas(),
            Colors::with_alpha(Colors::surface_panel_raised(), 0.55),
        )),
        grid: rgba(Colors::composite(
            Colors::surface_canvas(),
            Colors::border_subtle(),
        )),
        edge: rgba(Colors::composite(
            Colors::surface_canvas(),
            Colors::border_strong(),
        )),
        ear_plane: rgba(Colors::with_alpha(Colors::text_primary(), 0.035)),
        head: rgba(Colors::composite(
            Colors::surface_panel_raised(),
            Colors::with_alpha(Colors::text_muted(), 0.5),
        )),
        speaker: rgba(Colors::surface_panel_raised()),
        accent: rgba(Colors::accent_primary()),
        shadow: [0.0, 0.0, 0.0, 0.35],
    };
    let speakers = room3d::scene_speakers(
        snapshot.format,
        &snapshot.speaker_peaks,
        |peak| {
            let lit = level_fraction(peak);
            (lit > 0.0).then(|| {
                rgba(Colors::composite(
                    Colors::surface_panel_raised(),
                    Colors::with_alpha(level_color(peak), 0.3 + 0.7 * lit),
                ))
            })
        },
        palette.speaker,
    );
    let sources: Vec<room3d::SceneSource> = snapshot
        .sources
        .iter()
        .map(|source| room3d::SceneSource {
            heard: source.heard,
            base: source.base,
            color: rgba(source.color),
            automated: source.automated,
            selected: snapshot.selected.as_deref() == Some(source.id.as_str())
                || dragging == Some(source.id.as_str()),
        })
        .collect();
    let mesh = room3d::build_mesh(
        camera,
        &palette,
        &speakers,
        &sources,
        snapshot.format == SpatialFormat::Binaural,
    );
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    if !gpu.paint(&mesh, &camera.view_proj(w / h.max(1.0)), bounds, window, cx) {
        return false;
    }

    // Names over the frame: speakers above their boxes, channels above their
    // spheres. Picking uses the same projection.
    let screen = |point: room3d::Vec3| {
        camera
            .project(point, w, h)
            .map(|(x, y, depth)| (bounds.origin.x + px(x), bounds.origin.y + px(y), depth))
    };
    if let SpatialFormat::Surround(layout) = snapshot.format {
        let mut index = 0;
        for speaker in layout.speakers().iter().filter(|speaker| !speaker.lfe) {
            let Some(placed) = speakers.get(index) else {
                break;
            };
            index += 1;
            let at = room3d::speaker_world(placed);
            if let Some((x, y, _)) = screen([at[0], at[1] + 0.1, at[2]]) {
                label(
                    window,
                    cx,
                    speaker.label,
                    x,
                    y - px(12.0),
                    Colors::text_muted(),
                    typography::DENSE_CAPTION,
                );
            }
        }
    }
    let mut dots = Vec::with_capacity(snapshot.sources.len());
    for source in &snapshot.sources {
        let at = room3d::world(source.heard.position);
        let Some((x, y, _)) = screen(at) else {
            continue;
        };
        dots.push((source.id.clone(), gpui::point(x, y)));
        if let Some((lx, ly, _)) = screen([at[0], at[1] + room3d::SOURCE_RADIUS * 1.6, at[2]]) {
            let selected = snapshot.selected.as_deref() == Some(source.id.as_str());
            label(
                window,
                cx,
                &source.name,
                lx,
                ly - px(14.0),
                if selected {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                },
                typography::DENSE_LABEL,
            );
        }
    }
    *painted.dots.borrow_mut() = dots;
    true
}

/// Where a room point lands in `square`.
fn at(square: Bounds<Pixels>, position: RoomPosition) -> Point<Pixels> {
    let side = square.size.width;
    gpui::point(
        square.origin.x + side * ((position.x + 1.0) / 2.0),
        square.origin.y + side * ((1.0 - position.y) / 2.0),
    )
}

fn disc(window: &mut Window, centre: Point<Pixels>, radius: f32, color: gpui::Rgba) {
    let r = px(radius);
    window.paint_quad(
        gpui::fill(
            Bounds {
                origin: gpui::point(centre.x - r, centre.y - r),
                size: size(r * 2.0, r * 2.0),
            },
            color,
        )
        .corner_radii(r),
    );
}

fn ring(window: &mut Window, centre: Point<Pixels>, radius: f32, color: gpui::Rgba) {
    let r = px(radius);
    window.paint_quad(
        gpui::outline(
            Bounds {
                origin: gpui::point(centre.x - r, centre.y - r),
                size: size(r * 2.0, r * 2.0),
            },
            color,
            gpui::BorderStyle::default(),
        )
        .corner_radii(r),
    );
}

fn label(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    centre_x: Pixels,
    top: Pixels,
    color: gpui::Rgba,
    size_px: f32,
) {
    if text.is_empty() {
        return;
    }
    let run = gpui::TextRun {
        len: text.len(),
        font: theme::ui_font(),
        color: color.into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(text.to_string().into(), px(size_px), &[run], None);
    let origin = gpui::point(centre_x - line.width / 2.0, top);
    let _ = line.paint(
        origin,
        px(size_px * typography::LINE_HEIGHT),
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// The whole room: floor, the listener, the speakers with their levels, and
/// every channel. Records where it put the square and the channels.
fn paint_room(
    snapshot: &RoomSnapshot,
    dragging: Option<&str>,
    painted: &PaintedRoom,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let side = (f32::from(bounds.size.width).min(f32::from(bounds.size.height))
        - 2.0 * WALL_MARGIN)
        .max(40.0);
    let square = Bounds {
        origin: gpui::point(
            bounds.origin.x + (bounds.size.width - px(side)) / 2.0,
            bounds.origin.y + (bounds.size.height - px(side)) / 2.0,
        ),
        size: size(px(side), px(side)),
    };
    painted.square.set(Some(square));

    // The floor, its walls, and a cross through the listener. Square: it is
    // a coordinate space, like the timeline grid.
    window.paint_quad(gpui::fill(square, Colors::surface_canvas()));
    window.paint_quad(gpui::outline(
        square,
        Colors::border_normal(),
        gpui::BorderStyle::default(),
    ));
    let centre = at(square, RoomPosition::CENTRE);
    for step in [-0.5f32, 0.0, 0.5] {
        let rule = if step == 0.0 {
            Colors::border_normal()
        } else {
            Colors::border_subtle()
        };
        let x = at(square, RoomPosition::new(step, 0.0, 0.0)).x;
        let y = at(square, RoomPosition::new(0.0, step, 0.0)).y;
        window.paint_quad(gpui::fill(
            Bounds {
                origin: gpui::point(x, square.origin.y),
                size: size(px(1.0), square.size.height),
            },
            rule,
        ));
        window.paint_quad(gpui::fill(
            Bounds {
                origin: gpui::point(square.origin.x, y),
                size: size(square.size.width, px(1.0)),
            },
            rule,
        ));
    }
    // Which wall is the front, where no speaker labels it.
    if !matches!(snapshot.format, SpatialFormat::Surround(_)) {
        label(
            window,
            cx,
            "FRONT",
            centre.x,
            square.origin.y - px(WALL_MARGIN * 0.5 + 6.0),
            Colors::text_faint(),
            typography::DENSE_CAPTION,
        );
    }

    // The listener, facing the front wall.
    disc(window, centre, 9.0, Colors::surface_panel_raised());
    ring(window, centre, 9.0, Colors::border_strong());
    window.paint_quad(gpui::fill(
        Bounds {
            origin: gpui::point(centre.x - px(1.0), centre.y - px(15.0)),
            size: size(px(2.0), px(6.0)),
        },
        Colors::text_muted(),
    ));

    // The speakers on the walls, lit by their level; the top ring hollow.
    match snapshot.format {
        SpatialFormat::Surround(layout) => {
            for (channel, speaker) in layout.speakers().iter().enumerate() {
                if speaker.lfe {
                    continue;
                }
                let position = solfege_spatialaudio::speaker_position(
                    speaker.azimuth_deg,
                    speaker.elevation_deg,
                );
                let point = at(square, position);
                let peak = snapshot.speaker_peaks.get(channel).copied().unwrap_or(0.0);
                let top = speaker.elevation_deg >= 20.0;
                // Pull the top ring a little inward so it does not hide the
                // floor speaker on the same wall.
                let point = if top {
                    gpui::point(
                        point.x + (centre.x - point.x) * 0.16,
                        point.y + (centre.y - point.y) * 0.16,
                    )
                } else {
                    point
                };
                let lit = level_fraction(peak);
                let fill = if lit > 0.0 {
                    Colors::composite(
                        Colors::surface_panel_raised(),
                        Colors::with_alpha(level_color(peak), 0.25 + 0.75 * lit),
                    )
                } else {
                    Colors::surface_panel_raised()
                };
                let half = px(7.0);
                let speaker_box = Bounds {
                    origin: gpui::point(point.x - half, point.y - half),
                    size: size(half * 2.0, half * 2.0),
                };
                if top {
                    window.paint_quad(
                        gpui::outline(speaker_box, fill, gpui::BorderStyle::default())
                            .corner_radii(px(radius::MICRO)),
                    );
                } else {
                    window
                        .paint_quad(gpui::fill(speaker_box, fill).corner_radii(px(radius::MICRO)));
                    window.paint_quad(
                        gpui::outline(
                            speaker_box,
                            Colors::border_strong(),
                            gpui::BorderStyle::default(),
                        )
                        .corner_radii(px(radius::MICRO)),
                    );
                }
                // The label outside the wall (inside for the top ring).
                let outward = if top { -1.0 } else { 1.0 };
                let dy = if position.y >= 0.99 {
                    -14.0 * outward - 4.0
                } else if position.y <= -0.99 {
                    9.0 * outward
                } else {
                    -4.0
                };
                let dx = if position.x <= -0.99 {
                    -18.0 * outward
                } else if position.x >= 0.99 {
                    18.0 * outward
                } else {
                    0.0
                };
                label(
                    window,
                    cx,
                    speaker.label,
                    point.x + px(dx),
                    point.y + px(dy),
                    Colors::text_muted(),
                    typography::DENSE_CAPTION,
                );
            }
        }
        SpatialFormat::Binaural => {
            // The two ears, lit by their level.
            for (index, side_x) in [-1.0f32, 1.0].into_iter().enumerate() {
                let peak = snapshot.speaker_peaks.get(index).copied().unwrap_or(0.0);
                let ear = gpui::point(centre.x + px(side_x * 12.0), centre.y);
                let lit = level_fraction(peak);
                disc(
                    window,
                    ear,
                    3.5,
                    if lit > 0.0 {
                        Colors::with_alpha(level_color(peak), 0.35 + 0.65 * lit)
                    } else {
                        Colors::text_faint()
                    },
                );
            }
        }
        SpatialFormat::Stereo => {}
    }

    // Every channel: its two sides for a stereo pair, its own placement as a
    // ring while automation moves it, the dot (bigger the higher it sits),
    // and its name.
    let mut dots = Vec::with_capacity(snapshot.sources.len());
    let mut order: Vec<&RoomSource> = snapshot.sources.iter().collect();
    // The selected and the dragged channel paint last, on top.
    order.sort_by_key(|source| {
        (dragging == Some(source.id.as_str())) as u8 * 2
            + (snapshot.selected.as_deref() == Some(source.id.as_str())) as u8
    });
    for source in order {
        let heard = source.heard;
        let point = at(square, heard.position);
        let half_width = heard.half_width_radians();
        if half_width > 1.0e-3 {
            let side_color = Colors::with_alpha(source.color, 0.6);
            for angle in [-half_width, half_width] {
                let side_point = at(square, heard.position.rotated(angle));
                disc(window, side_point, 2.5, side_color);
            }
        }
        if source.automated {
            let base_point = at(square, source.base.position);
            ring(
                window,
                base_point,
                6.0,
                Colors::with_alpha(source.color, 0.55),
            );
        }
        let selected = snapshot.selected.as_deref() == Some(source.id.as_str());
        let radius = 6.0 + 4.0 * heard.position.z.clamp(0.0, 1.0);
        // Spread as a soft halo.
        if heard.spread > 0.01 {
            disc(
                window,
                point,
                radius + 18.0 * heard.spread,
                Colors::with_alpha(source.color, 0.12),
            );
        }
        disc(window, point, radius + 1.5, Colors::surface_canvas());
        disc(window, point, radius, source.color);
        if selected || dragging == Some(source.id.as_str()) {
            ring(window, point, radius + 3.0, Colors::accent_primary());
        }
        label(
            window,
            cx,
            &source.name,
            point.x,
            point.y + px(radius + 3.0),
            if selected {
                Colors::text_primary()
            } else {
                Colors::text_secondary()
            },
            typography::DENSE_LABEL,
        );
        dots.push((source.id.clone(), point));
    }
    *painted.dots.borrow_mut() = dots;
}

/// Open the 3D Room Preview, centred on the Studio window.
pub fn open_room_window(
    owner_bounds: Option<Bounds<Pixels>>,
    snapshot: RoomSnapshot,
    callbacks: RoomCallbacks,
    cx: &mut App,
) -> Result<WindowHandle<RoomWindow>, String> {
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        owner_bounds,
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        cx,
    )));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_min_size = Some(size(px(WINDOW_MIN_WIDTH), px(WINDOW_MIN_HEIGHT)));
    apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|cx| RoomWindow::new(snapshot, callbacks, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_minus_sixty_to_zero_db_across_the_bar() {
        assert_eq!(level_fraction(0.0), 0.0);
        assert_eq!(level_fraction(f32::NAN), 0.0);
        assert!((level_fraction(1.0) - 1.0).abs() < 1.0e-6);
        assert!((level_fraction(0.001) - 0.0).abs() < 1.0e-3);
        assert!((level_fraction(10f32.powf(-30.0 / 20.0)) - 0.5).abs() < 1.0e-4);
    }
}
