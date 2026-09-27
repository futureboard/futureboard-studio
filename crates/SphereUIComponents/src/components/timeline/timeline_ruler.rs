use crate::assets;
use crate::components::timeline::timeline_state::{
    GridLineLevel, TempoMap, TimeSignatureMap, TimelineGestureContext, TimelineState, HEADER_WIDTH,
    RULER_HEIGHT,
};
use crate::theme::Colors;
use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, svg, AppContext, Bounds, Empty, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, StatefulInteractiveElement, Styled, Window,
};

/// Sink for the measured lane-column origin. Written by the ruler's probe
/// during prepaint, read by `Timeline::render` on the next frame.
pub type LaneOriginProbe = std::rc::Rc<std::cell::Cell<Option<f32>>>;

/// Zero-cost overlay that records its parent's window-space left edge every
/// frame.
///
/// The ruler markings area *is* the lane content column, so its origin is the
/// one number every pointer gesture in the arrangement needs. Measuring it
/// beats deriving it: the shell has a collapsible browser panel and a left
/// rail, and the constants that used to stand in for them were wrong whenever
/// either changed.
fn lane_origin_probe(sink: LaneOriginProbe) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, window, _cx| {
            let measured: f32 = bounds.origin.x.into();
            if sink.get().is_none_or(|prev| (prev - measured).abs() >= 0.5) {
                sink.set(Some(measured));
                // The value is consumed at the top of the *next* render, so ask
                // for one. Without this the first frame after startup or a panel
                // toggle would still hit-test against the stale estimate, and a
                // click landing in that window would miss by the difference.
                window.refresh();
            }
        },
        |_, _: (), _, _| {},
    )
    .absolute()
    .inset_0()
}

#[derive(Clone, Debug)]
struct RulerSeekDrag;

impl Render for RulerSeekDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineRegionDragMode {
    Move,
    Start,
    End,
}

#[derive(Clone, Debug)]
pub struct TimelineRegionDrag {
    pub region_id: String,
    pub mode: TimelineRegionDragMode,
    pub start_beat: f64,
    pub end_beat: f64,
    pub pointer_offset_x: f32,
}

impl Render for TimelineRegionDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone, Debug)]
pub struct TimelineRegionDragUpdate {
    pub region_id: String,
    pub start_beat: f64,
    pub end_beat: f64,
}

/// A loop grab in progress: which edge (or the body) the press took, where the
/// loop was when it started, and how far into it the pointer went down.
#[derive(Clone, Copy, Debug)]
pub struct TimelineLoopDrag {
    pub mode: TimelineRegionDragMode,
    pub start_beat: f32,
    pub end_beat: f32,
    pub pointer_offset_x: f32,
}

/// What a press on the ruler is doing.
///
/// # Why one gesture and not two drag sources
///
/// The ruler used to be two overlapping drag surfaces: the markings area, which
/// scrubs the playhead, and the loop region drawn on top of it, which moved and
/// resized the loop. The deeper element always won, so a press *inside the loop*
/// could never move the playhead — the loop swallowed it, and the one place in
/// the arrangement a user most often wants to drop the playhead was the one
/// place they could not.
///
/// Dragging the playhead is the ruler's primary gesture, so it is now the
/// unmodified one everywhere along the bar *except* within [`LOOP_EDGE_GRAB_PX`]
/// of a loop end: those brace edges stay direct resize handles, the way every
/// DAW does it, so the most common loop edit needs no modifier. Sliding the
/// whole loop, or resizing from its body, is the deliberate, occasional one, so
/// it takes a modifier: **Alt-drag**, on the loop's body to slide it or within
/// [`LOOP_EDGE_GRAB_PX`] of an end to stretch it.
///
/// Deciding this once at mouse-down, rather than letting two `on_drag` sources
/// race, is what makes the rule hold: the loop overlay is now painted only, and
/// a press it does not claim reaches the ruler underneath untouched.
#[derive(Clone, Copy, Debug, Default)]
pub enum RulerGesture {
    #[default]
    Scrub,
    /// Sliding or stretching the loop that is already there.
    Loop(TimelineLoopDrag),
    /// Drawing a new loop from the point the press landed on.
    ///
    /// Without this, Alt-drag was a gesture that did nothing most of the time:
    /// it only worked with looping already on *and* the press inside the loop,
    /// and anywhere else it quietly fell through to scrubbing. A modifier that
    /// works on one strip of the ruler and nowhere else is one the hand never
    /// learns. Now Alt-drag always means the loop — over it, move it; anywhere
    /// else, draw a new one.
    LoopCreate { anchor_beat: f32 },
}

/// How near an end of the loop counts as grabbing that end rather than the body.
const LOOP_EDGE_GRAB_PX: f32 = 8.0;

/// Which part of the loop an Alt-press at `local_x` took, if any.
///
/// The edges win over the body, and a loop narrower than two grab zones still
/// resolves: the half the pointer is on decides, so a two-beat loop zoomed out
/// to twelve pixels can still be stretched from either end.
fn loop_grab_mode(local_x: f32, left_x: f32, right_x: f32) -> Option<TimelineRegionDragMode> {
    if local_x < left_x - LOOP_EDGE_GRAB_PX || local_x > right_x + LOOP_EDGE_GRAB_PX {
        return None;
    }
    let near_start = (local_x - left_x).abs() <= LOOP_EDGE_GRAB_PX;
    let near_end = (local_x - right_x).abs() <= LOOP_EDGE_GRAB_PX;
    Some(match (near_start, near_end) {
        (true, true) => {
            if local_x - left_x <= right_x - local_x {
                TimelineRegionDragMode::Start
            } else {
                TimelineRegionDragMode::End
            }
        }
        (true, false) => TimelineRegionDragMode::Start,
        (false, true) => TimelineRegionDragMode::End,
        (false, false) => TimelineRegionDragMode::Move,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct TimelineLoopDragUpdate {
    pub start_beat: f32,
    pub end_beat: f32,
}

/// The ruler's tick marks as a single painted layer.
fn ruler_ticks(
    lines: &[crate::components::timeline::timeline_state::GridLine],
) -> impl IntoElement {
    // Resolved here, off the paint closure: the colour lookup and the level
    // match are per line, and the closure runs again on every repaint.
    let ticks: Vec<(f32, f32, gpui::Rgba)> = lines
        .iter()
        .map(|line| {
            let (height, alpha) = match line.level {
                GridLineLevel::Bar => (RULER_HEIGHT - 2.0, 0.28),
                GridLineLevel::Beat => (RULER_HEIGHT * 0.46, 0.18),
                GridLineLevel::Sub => (RULER_HEIGHT * 0.18, 0.10),
            };
            (
                line.x,
                height,
                Colors::with_alpha(Colors::timeline_ruler_tick(), alpha),
            )
        })
        .collect();
    canvas(
        |_bounds, _window, _cx| {},
        move |bounds: gpui::Bounds<gpui::Pixels>, (), window, _cx| {
            let bottom: f32 = bounds.size.height.into();
            for (x, height, color) in &ticks {
                // Anchored to the bottom edge, which is what `.bottom_0()` did.
                let top = (bottom - height).max(0.0);
                window.paint_quad(gpui::fill(
                    gpui::Bounds::new(
                        bounds.origin + gpui::point(px(*x), px(top)),
                        gpui::size(px(1.0), px(*height)),
                    ),
                    *color,
                ));
            }
        },
    )
    .absolute()
    .inset_0()
}

/// One global M / S latch in the Arrangement header.
///
/// Deliberately an *indicator that can be cleared*, not a "mute everything"
/// button. Muting every track would have to overwrite the per-track mutes the
/// user set by hand, and there is no state left to restore them from; clearing
/// only ever removes latches the user can see are on, so the gesture is
/// reversible by hand and never invents mixer state.
///
/// Dark when nothing is latched, and inert then — pressing it would have
/// nothing to clear, so it does not pretend to be a control.
fn global_latch(
    id: &'static str,
    label: &'static str,
    active: bool,
    tone: gpui::Rgba,
    tooltip: &'static str,
    on_click: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(crate::theme::size::MICRO))
        .w(px(crate::theme::size::MICRO))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .bg(if active {
            Colors::with_alpha(tone, 0.18)
        } else {
            Colors::surface_raised()
        })
        .border(px(1.0))
        .border_color(if active {
            Colors::with_alpha(tone, 0.55)
        } else {
            Colors::border_subtle()
        })
        .text_size(px(crate::theme::typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(if active { tone } else { Colors::text_faint() })
        .tooltip(crate::components::controls::fb_tooltip(tooltip))
        .when(active, |latch| {
            latch
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(Colors::with_alpha(tone, 0.28)))
                .on_click(move |_, window, cx| {
                    on_click(&(), window, cx);
                })
        })
        .child(label)
}

pub fn timeline_ruler(
    state: &TimelineState,
    on_add_track: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>,
    on_toggle_snap: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>,
    on_grid_menu: std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    on_clear_all_mutes: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>,
    on_clear_all_solos: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>,
    on_seek: std::sync::Arc<
        dyn Fn(&f32, crate::layout::SeekReason, &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_region_drag: std::sync::Arc<
        dyn Fn(&TimelineRegionDragUpdate, &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_loop_drag: std::sync::Arc<
        dyn Fn(&TimelineLoopDragUpdate, &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_ruler_context: std::sync::Arc<
        dyn Fn(&(f32, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_playhead_scrub_begin: Option<
        std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + Send + Sync + 'static>,
    >,
    on_playhead_scrub_end: Option<
        std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + Send + Sync + 'static>,
    >,
    gesture_kind: std::rc::Rc<std::cell::Cell<RulerGesture>>,
    origin_probe: LaneOriginProbe,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("TimelineRuler");
    let on_toggle_snap_clone = on_toggle_snap.clone();
    let on_grid_menu_clone = on_grid_menu.clone();
    let on_add_track_clone = on_add_track.clone();
    let any_muted = state.any_track_muted();
    let any_soloed = state.any_track_soloed();

    let ruler_grid_width = state.viewport.viewport_width.max(1.0);
    // The ruler's ticks follow the project timebase; the grid behind the clips
    // stays musical. For Bars+Beats these are the same lines.
    let lines = state.ruler_grid_lines(ruler_grid_width);

    let on_seek_clone = on_seek.clone();
    let on_seek_drag = on_seek.clone();
    let scrub_begin = on_playhead_scrub_begin.clone();
    let scrub_end = on_playhead_scrub_end.clone();
    let scrub_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let scrub_active_drag = scrub_active.clone();
    let scrub_active_up = scrub_active.clone();
    // A ruler drag routinely ends with the pointer off the ruler, and GPUI's
    // `on_mouse_up` only fires while the hitbox is hovered — so the release that
    // resumes the metronome was simply lost, leaving the click suspended until
    // the next Play. `on_mouse_up_out` is the other half of that pair; the
    // shared `swap(false)` makes exactly one of the two run the end callback.
    let scrub_active_up_out = scrub_active.clone();
    let scrub_end_out = on_playhead_scrub_end.clone();
    // Resolved once per frame from last frame's measurement, so every handler
    // built below shares one origin.
    let lane_origin = state.lane_origin_x();
    // The loop's span in the same lane-local pixels the press reports, resolved
    // once so the mouse-down handler is a comparison and not a projection.
    let loop_span = state.transport.loop_enabled.then(|| {
        let start = state
            .transport
            .loop_start_beats
            .min(state.transport.loop_end_beats);
        let end = state
            .transport
            .loop_start_beats
            .max(state.transport.loop_end_beats);
        (start, end, state.beats_to_x(start), state.beats_to_x(end))
    });
    // What the press in flight is doing. Set once at mouse-down and read by the
    // one drag handler; see `RulerGesture`. Owned by the `Timeline` component so
    // it survives the re-render that GPUI's drag-arming triggers between the
    // mouse-down that classifies the press and the first drag-move that acts on
    // it — a per-frame `Cell` would be reset to `Scrub` in that gap, which is
    // why loop edits silently fell through to scrubbing.
    let gesture_down = gesture_kind.clone();
    let gesture_move = gesture_kind.clone();
    let gesture_up = gesture_kind.clone();
    let gesture_up_out = gesture_kind.clone();
    let on_region_drag_move = on_region_drag.clone();
    // Both drag closures only map pointer x -> snapped beats, so they capture
    // this frame's geometry instead of a deep clone of the whole project.
    let gesture = std::rc::Rc::new(TimelineGestureContext::from_state(state));
    let state_for_region_drag = std::rc::Rc::clone(&gesture);
    let on_loop_drag_move = on_loop_drag.clone();
    let state_for_press = std::rc::Rc::clone(&gesture);
    let state_for_loop_drag = gesture;

    div()
        .flex()
        .flex_row()
        .h(px(RULER_HEIGHT))
        .w_full()
        .bg(Colors::timeline_ruler_background())
        // A decisive edge, not a hairline. The ruler is the boundary between
        // chrome and musical content, so it has to close firmly against the
        // arrangement canvas or the two planes bleed together.
        .border_b(px(1.0))
        .border_color(Colors::border_normal())
        .child(
            // Left Ruler Header Area — uses the same deeper background
            // and strong right border as the TrackHeader rows so the
            // entire left column reads as a single frontmost pane.
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w(px(HEADER_WIDTH))
                .h_full()
                .px(px(8.0))
                .bg(Colors::surface_panel())
                .border_r(px(1.0))
                .border_color(Colors::border_strong())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(crate::theme::space::SNUG))
                        .min_w(px(0.0))
                        .child(
                            div()
                                .flex_none()
                                .text_color(Colors::timeline_ruler_text())
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Arrangement"),
                        )
                        // The two global latches sit at the head of the column
                        // whose rows carry the per-track M and S, which is what
                        // makes them read as "M and S for all of this".
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(2.0))
                                .flex_none()
                                .child(global_latch(
                                    "ruler-clear-all-mutes",
                                    "M",
                                    any_muted,
                                    Colors::status_warning(),
                                    if any_muted {
                                        "Tracks are muted — click to unmute all"
                                    } else {
                                        "No track is muted"
                                    },
                                    on_clear_all_mutes,
                                ))
                                .child(global_latch(
                                    "ruler-clear-all-solos",
                                    "S",
                                    any_soloed,
                                    Colors::accent_primary(),
                                    if any_soloed {
                                        "Tracks are soloed — click to clear all solo"
                                    } else {
                                        "No track is soloed"
                                    },
                                    on_clear_all_solos,
                                )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        // Add Track Button
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .h(px(20.0))
                                .px(px(5.0))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .bg(Colors::surface_raised())
                                .border(px(1.0))
                                .border_color(Colors::border_subtle())
                                .cursor(gpui::CursorStyle::PointingHand)
                                .text_color(Colors::text_secondary())
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .id("ruler-add-track-btn")
                                .hover(|style| style.bg(Colors::surface_hover()))
                                .on_click(move |_, window, cx| {
                                    on_add_track_clone(&(), window, cx);
                                })
                                .child("+ Add"),
                        )
                        // Snap Toggle Button
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .h(px(20.0))
                                .w(px(20.0))
                                .rounded(px(crate::theme::radius::CONTROL))
                                // Active = accent stroke + accent icon, not a
                                // filled accent background (matches transport
                                // toolbar styling).
                                .bg(Colors::surface_raised())
                                .border(px(1.0))
                                .border_color(if state.snap_to_grid {
                                    Colors::with_alpha(Colors::accent_primary(), 0.55)
                                } else {
                                    Colors::border_subtle()
                                })
                                .cursor(gpui::CursorStyle::PointingHand)
                                .id("ruler-snap-toggle-btn")
                                .tooltip(|window, cx| {
                                    let text = match crate::keymap::shortcut_for_command(
                                        "timeline:toggle-snap",
                                    ) {
                                        Some(shortcut) => format!("Snap to Grid ({shortcut})"),
                                        None => "Snap to Grid".to_string(),
                                    };
                                    crate::components::controls::fb_tooltip(text)(window, cx)
                                })
                                .on_click(move |_, window, cx| {
                                    on_toggle_snap_clone(&(), window, cx);
                                })
                                .child(
                                    svg()
                                        .path(assets::ICON_MAGNET_PATH)
                                        .w(px(12.0))
                                        .h(px(12.0))
                                        .text_color(if state.snap_to_grid {
                                            Colors::accent_primary()
                                        } else {
                                            Colors::text_secondary()
                                        }),
                                ),
                        )
                        // Grid resolution dropdown. Opens on press, like the
                        // transport's Count-In menu, at the pointer.
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap(px(2.0))
                                .h(px(20.0))
                                .pl(px(4.0))
                                .pr(px(3.0))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .bg(Colors::surface_raised())
                                .border(px(1.0))
                                .border_color(Colors::border_subtle())
                                .cursor(gpui::CursorStyle::PointingHand)
                                .text_color(Colors::text_muted())
                                .text_size(px(9.0))
                                .id("ruler-grid-res-btn")
                                .tooltip(crate::components::controls::fb_tooltip("Grid"))
                                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                                    cx.stop_propagation();
                                    on_grid_menu_clone(
                                        &(event.position.x.into(), event.position.y.into()),
                                        window,
                                        cx,
                                    );
                                })
                                .child(state.grid_step_label())
                                .child(
                                    svg()
                                        .path(assets::ICON_CHEVRON_DOWN_PATH)
                                        .w(px(8.0))
                                        .h(px(8.0))
                                        .flex_none()
                                        .text_color(Colors::text_faint()),
                                ),
                        ),
                ),
        )
        .child(
            // Right Ruler Markings Area
            div()
                .flex_1()
                .h_full()
                .relative()
                // Clip all ruler ticks / bar-beat labels / tempo + time-signature
                // marker pills to this content rect. Without this, a marker whose
                // x is at or left of the content edge (during horizontal scroll)
                // draws with a negative `left` straight over the left "Arrangement"
                // ruler header. This is the ruler's `ruler_content_rect`.
                .overflow_hidden()
                .cursor(gpui::CursorStyle::Crosshair)
                .id("ruler-markings-area")
                .child(lane_origin_probe(origin_probe))
                // Debug: outline the ruler content clip rect (FUTUREBOARD_UI_DEBUG_CLIPS=1).
                .children(crate::perf::debug_clip_outline())
                // The press decides what this drag is: Alt on the loop takes the
                // loop, anything else takes the playhead. See `RulerGesture`.
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |event: &gpui::MouseDownEvent, window, cx| {
                        let x: f32 = event.position.x.into();
                        let click_x = x - lane_origin;
                        if event.modifiers.alt {
                            let grab = loop_span.and_then(|(start, end, left_x, right_x)| {
                                loop_grab_mode(click_x, left_x, right_x).map(|mode| {
                                    RulerGesture::Loop(TimelineLoopDrag {
                                        mode,
                                        start_beat: start,
                                        end_beat: end,
                                        pointer_offset_x: click_x - left_x,
                                    })
                                })
                            });
                            gesture_down.set(grab.unwrap_or_else(|| {
                                // Not on the loop: this press is the start of a
                                // new one, anchored where it landed.
                                RulerGesture::LoopCreate {
                                    anchor_beat: state_for_press
                                        .snap_beats(state_for_press.x_to_beats(click_x))
                                        .max(0.0),
                                }
                            }));
                            // Alt-pressing the ruler must not also move the
                            // playhead: the press belongs to the loop now.
                            window.prevent_default();
                            cx.stop_propagation();
                            return;
                        }
                        // Unmodified, the loop's END HANDLES are still direct
                        // grab targets: dragging a brace edge resizes the loop
                        // the way every DAW does, no modifier to learn. A press
                        // on the loop *body* (or empty ruler) still scrubs, so
                        // dropping the playhead anywhere — including inside the
                        // loop — keeps working. Only the edges are claimed here;
                        // the body falls through to the scrub below.
                        let edge_grab = loop_span.and_then(|(start, end, left_x, right_x)| {
                            loop_grab_mode(click_x, left_x, right_x).and_then(|mode| {
                                matches!(
                                    mode,
                                    TimelineRegionDragMode::Start | TimelineRegionDragMode::End
                                )
                                .then_some(RulerGesture::Loop(
                                    TimelineLoopDrag {
                                        mode,
                                        start_beat: start,
                                        end_beat: end,
                                        pointer_offset_x: click_x - left_x,
                                    },
                                ))
                            })
                        });
                        if let Some(grab) = edge_grab {
                            gesture_down.set(grab);
                            window.prevent_default();
                            cx.stop_propagation();
                            return;
                        }
                        gesture_down.set(RulerGesture::Scrub);
                        on_seek_clone(
                            &click_x,
                            crate::layout::SeekReason::TimelineClick,
                            window,
                            cx,
                        );
                    },
                )
                // Right-click → position-aware tempo menu.
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    move |event: &gpui::MouseDownEvent, window, cx| {
                        let x: f32 = event.position.x.into();
                        let y: f32 = event.position.y.into();
                        let click_x = x - lane_origin;
                        on_ruler_context(&(click_x, x, y), window, cx);
                    },
                )
                .on_drag(RulerSeekDrag, {
                    let scrub_active = scrub_active.clone();
                    move |_, _offset, _window, cx| {
                        scrub_active.store(false, std::sync::atomic::Ordering::Relaxed);
                        cx.new(|_| RulerSeekDrag)
                    }
                })
                .on_drag_move::<RulerSeekDrag>(
                    move |event: &gpui::DragMoveEvent<RulerSeekDrag>, window, cx| {
                        let x: f32 = event.event.position.x.into();
                        let ox: f32 = event.bounds.origin.x.into();
                        let local_x = (x - ox).max(0.0);
                        let beat_at_x = |x: f32| {
                            state_for_loop_drag
                                .snap_beats(state_for_loop_drag.x_to_beats(x))
                                .max(0.0)
                        };
                        if let RulerGesture::LoopCreate { anchor_beat } = gesture_move.get() {
                            let here = beat_at_x(local_x);
                            // Either direction draws the same loop: the anchor
                            // is one end, the pointer is the other, and which is
                            // "start" is arithmetic, not a rule the user has to
                            // know. A zero-width drag is left alone rather than
                            // published as an empty loop.
                            let (start_beat, end_beat) =
                                (anchor_beat.min(here), anchor_beat.max(here));
                            if (end_beat - start_beat) > f32::EPSILON {
                                on_loop_drag_move(
                                    &TimelineLoopDragUpdate {
                                        start_beat,
                                        end_beat,
                                    },
                                    window,
                                    cx,
                                );
                            }
                            window.prevent_default();
                            cx.stop_propagation();
                            return;
                        }
                        if let RulerGesture::Loop(drag) = gesture_move.get() {
                            let (start_beat, end_beat) = match drag.mode {
                                TimelineRegionDragMode::Move => {
                                    let length = (drag.end_beat - drag.start_beat).max(1.0e-3);
                                    let start = beat_at_x(local_x - drag.pointer_offset_x);
                                    (start, start + length)
                                }
                                TimelineRegionDragMode::Start => {
                                    (beat_at_x(local_x), drag.end_beat)
                                }
                                TimelineRegionDragMode::End => {
                                    (drag.start_beat, beat_at_x(local_x))
                                }
                            };
                            on_loop_drag_move(
                                &TimelineLoopDragUpdate {
                                    start_beat,
                                    end_beat,
                                },
                                window,
                                cx,
                            );
                            window.prevent_default();
                            cx.stop_propagation();
                            return;
                        }
                        if !scrub_active_drag.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            if let Some(cb) = scrub_begin.as_ref() {
                                cb(window, cx);
                            }
                        }
                        on_seek_drag(
                            &local_x,
                            crate::layout::SeekReason::UserDragging,
                            window,
                            cx,
                        );
                        window.prevent_default();
                        cx.stop_propagation();
                    },
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    move |_: &gpui::MouseUpEvent, window, cx| {
                        gesture_up.set(RulerGesture::Scrub);
                        if scrub_active_up.swap(false, std::sync::atomic::Ordering::Relaxed) {
                            if let Some(cb) = scrub_end.as_ref() {
                                cb(window, cx);
                            }
                        }
                    },
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    move |_: &gpui::MouseUpEvent, window, cx| {
                        gesture_up_out.set(RulerGesture::Scrub);
                        if scrub_active_up_out.swap(false, std::sync::atomic::Ordering::Relaxed) {
                            if let Some(cb) = scrub_end_out.as_ref() {
                                cb(window, cx);
                            }
                        }
                    },
                )
                .on_drag_move::<TimelineRegionDrag>(
                    move |event: &gpui::DragMoveEvent<TimelineRegionDrag>, window, cx| {
                        let drag = event.drag(cx);
                        let x: f32 = event.event.position.x.into();
                        let ox: f32 = event.bounds.origin.x.into();
                        let local_x = (x - ox).max(0.0);
                        let beat_at_x = |x: f32| {
                            state_for_region_drag
                                .snap_beats(state_for_region_drag.x_to_beats(x))
                                .max(0.0) as f64
                        };
                        let (start_beat, end_beat) = match drag.mode {
                            TimelineRegionDragMode::Move => {
                                let length = (drag.end_beat - drag.start_beat).max(1.0e-3);
                                let start = beat_at_x(local_x - drag.pointer_offset_x);
                                (start, start + length)
                            }
                            TimelineRegionDragMode::Start => (beat_at_x(local_x), drag.end_beat),
                            TimelineRegionDragMode::End => (drag.start_beat, beat_at_x(local_x)),
                        };
                        on_region_drag_move(
                            &TimelineRegionDragUpdate {
                                region_id: drag.region_id.clone(),
                                start_beat,
                                end_beat,
                            },
                            window,
                            cx,
                        );
                        window.prevent_default();
                        cx.stop_propagation();
                    },
                )
                .children(if state.transport.loop_enabled {
                    let lx = state.beats_to_x(state.transport.loop_start_beats);
                    let rx = state.beats_to_x(state.transport.loop_end_beats);
                    let width = (rx - lx).max(0.0);
                    Some(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(lx))
                            .w(px(width))
                            // Loop range highlight: keep extremely subtle so it never reads
                            // as a foreground "region strip" over the ruler/viewport.
                            .bg(Colors::with_alpha(Colors::timeline_selection(), 0.28))
                            .border_l(px(2.0))
                            .border_r(px(2.0))
                            .border_color(Colors::with_alpha(Colors::timeline_selection(), 0.70))
                            .child(
                                div()
                                    .absolute()
                                    .left(px(-1.0))
                                    .top(px(4.0))
                                    .w(px(3.0))
                                    .h(px(12.0))
                                    .rounded(px(crate::theme::radius::MICRO))
                                    .bg(Colors::with_alpha(Colors::timeline_selection(), 0.95)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .right(px(-1.0))
                                    .top(px(4.0))
                                    .w(px(3.0))
                                    .h(px(12.0))
                                    .rounded(px(crate::theme::radius::MICRO))
                                    .bg(Colors::with_alpha(Colors::timeline_selection(), 0.95)),
                            ),
                    )
                } else {
                    None
                })
                .children(state.regions.iter().filter_map(|region| {
                    // The Region lane owns regions outright when it is visible:
                    // their blocks, their trim handles, their menu. A second
                    // copy here would put two grab targets on one object, and
                    // the ruler's sits on top of the playhead scrub. The ruler
                    // keeps these compact bars only while that lane is hidden,
                    // so nothing disappears when the lane is turned off.
                    if state.show_region_track {
                        return None;
                    }
                    let (start, end) = region.normalized_range();
                    let x = state.beats_to_x(start as f32);
                    let rx = state.beats_to_x(end as f32);
                    let width = (rx - x).max(1.0);
                    if x > ruler_grid_width + 24.0 || x + width < -24.0 {
                        return None;
                    }
                    let color = crate::color::parse_hex_color(&region.color_hex)
                        .unwrap_or_else(|_| Colors::accent_success());
                    let id_num = {
                        use std::hash::{Hash, Hasher};
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        region.id.hash(&mut hasher);
                        hasher.finish() as usize
                    };
                    let body_drag = TimelineRegionDrag {
                        region_id: region.id.clone(),
                        mode: TimelineRegionDragMode::Move,
                        start_beat: start,
                        end_beat: end,
                        pointer_offset_x: 0.0,
                    };
                    let start_drag = TimelineRegionDrag {
                        region_id: region.id.clone(),
                        mode: TimelineRegionDragMode::Start,
                        start_beat: start,
                        end_beat: end,
                        pointer_offset_x: 0.0,
                    };
                    let end_drag = TimelineRegionDrag {
                        region_id: region.id.clone(),
                        mode: TimelineRegionDragMode::End,
                        start_beat: start,
                        end_beat: end,
                        pointer_offset_x: 0.0,
                    };
                    Some(
                        div()
                            .absolute()
                            .left(px(x))
                            .top(px(1.0))
                            .h(px(13.0))
                            .w(px(width))
                            .rounded(px(crate::theme::radius::MICRO))
                            .bg(Colors::with_alpha(color, 0.20))
                            .border(px(1.0))
                            .border_color(Colors::with_alpha(color, 0.55))
                            .overflow_hidden()
                            .cursor(gpui::CursorStyle::PointingHand)
                            .id(("ruler-region", id_num))
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            })
                            .on_drag(body_drag, |drag, offset, _window, cx| {
                                cx.new(|_| TimelineRegionDrag {
                                    pointer_offset_x: offset.x.into(),
                                    ..drag.clone()
                                })
                            })
                            .child(
                                div()
                                    .px(px(4.0))
                                    .text_size(px(8.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(color)
                                    .truncate()
                                    .child(region.name.clone()),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(px(6.0))
                                    .cursor(gpui::CursorStyle::ResizeLeft)
                                    .id(("ruler-region-start", id_num))
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_drag(start_drag, |drag, _offset, _window, cx| {
                                        cx.new(|_| drag.clone())
                                    }),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .right_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(px(6.0))
                                    .cursor(gpui::CursorStyle::ResizeRight)
                                    .id(("ruler-region-end", id_num))
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_drag(end_drag, |drag, _offset, _window, cx| {
                                        cx.new(|_| drag.clone())
                                    }),
                            ),
                    )
                }))
                .children(state.markers.iter().filter_map(|marker| {
                    // Same rule as regions above: the Marker lane owns markers
                    // while it is showing.
                    if state.show_marker_track {
                        return None;
                    }
                    let x = state.beats_to_x(marker.beat as f32);
                    if x < -24.0 || x > ruler_grid_width + 24.0 {
                        return None;
                    }
                    let color = crate::color::parse_hex_color(&marker.color_hex)
                        .unwrap_or_else(|_| Colors::accent_primary());
                    Some(
                        div()
                            .absolute()
                            .left(px(x))
                            .top(px(0.0))
                            .bottom_0()
                            .w(px(1.0))
                            .bg(Colors::with_alpha(color, 0.70))
                            .child(
                                div()
                                    .absolute()
                                    .left(px(-4.0))
                                    .top(px(2.0))
                                    .w(px(9.0))
                                    .h(px(9.0))
                                    .rounded(px(crate::theme::radius::MICRO))
                                    .bg(color),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left(px(5.0))
                                    .top(px(1.0))
                                    .min_w(px(38.0))
                                    .max_w(px(110.0))
                                    .text_size(px(8.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(color)
                                    .truncate()
                                    .child(marker.name.clone()),
                            ),
                    )
                }))
                // Ticks: every visible grid line, drawn as a 1 px vertical mark
                // anchored to the bottom of the ruler. Bar lines reach the top;
                // beat and sub lines are shorter.
                //
                // One canvas, not a div per line — the same move the grid layer
                // made. At a typical zoom this is 70-150 lines and the budget
                // allows 1200, and each one was a laid-out node whose only job
                // was to be a coloured rectangle.
                .child(ruler_ticks(&lines))
                // Labels: emitted as siblings of the ticks (not children of a
                // 1 px-wide tick div, which previously made labels wrap one
                // character per line and look like random digits). Each label
                // gets its own min-width so the text lays out on a single row.
                .children(lines.iter().filter(|l| l.show_label).map(|line| {
                    let label = state.format_grid_line_label(line);
                    let (font_weight, text_color) = match line.level {
                        GridLineLevel::Bar => {
                            (gpui::FontWeight::BOLD, Colors::timeline_ruler_text())
                        }
                        _ => (gpui::FontWeight::NORMAL, Colors::text_muted()),
                    };
                    div()
                        .absolute()
                        .left(px(line.x + 3.0))
                        .top(px(4.0))
                        .min_w(px(40.0))
                        .text_size(px(10.0))
                        .font_weight(font_weight)
                        .text_color(text_color)
                        .child(label)
                }))
                // Tempo markers — lightweight BPM labels anchored to the bottom
                // of the ruler so they never collide with the bar/beat labels at
                // the top. Visible whenever the project has tempo automation,
                // even when the Tempo Track lane is hidden. Only markers inside
                // the visible viewport are emitted.
                // Fallback chips only. When the lane is open it already draws
                // the marker as an anchored flag, and duplicating it here put a
                // second chip in a 30px band that also holds the bar numbers —
                // which is what made the label at bar 1 unreadable.
                .children(state.time_signature_map.points.iter().filter_map(|point| {
                    if state.show_time_signature_track {
                        return None;
                    }
                    let x = state.beats_to_x(point.beat as f32);
                    if x < -24.0 || x > ruler_grid_width + 24.0 {
                        return None;
                    }
                    let label =
                        TimeSignatureMap::format_marker_label(point.numerator, point.denominator);
                    Some(
                        div()
                            .absolute()
                            // Clamp the label to the left content edge so a marker
                            // at/left of the viewport stays readable inside the ruler
                            // content instead of being pushed under the header clip.
                            .left(px((x + 1.0).max(0.0)))
                            .top(px(14.0))
                            .flex()
                            .items_center()
                            .h(px(12.0))
                            .px(px(3.0))
                            .rounded(px(crate::theme::radius::MICRO))
                            .bg(Colors::with_alpha(Colors::text_muted(), 0.12))
                            .border_l(px(1.0))
                            .border_color(Colors::with_alpha(Colors::text_muted(), 0.35))
                            .text_size(px(9.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_secondary())
                            .child(label),
                    )
                }))
                .children(state.tempo_map.points.iter().filter_map(|point| {
                    if state.show_tempo_track {
                        return None;
                    }
                    let x = state.beats_to_x(point.beat as f32);
                    if x < -24.0 || x > ruler_grid_width + 24.0 {
                        return None;
                    }
                    let label = TempoMap::format_marker_label(point.bpm);
                    Some(
                        div()
                            .absolute()
                            // Clamp the label to the left content edge so a marker
                            // at/left of the viewport stays readable inside the ruler
                            // content instead of being pushed under the header clip.
                            .left(px((x + 1.0).max(0.0)))
                            .bottom(px(1.0))
                            .flex()
                            .items_center()
                            .h(px(12.0))
                            .px(px(3.0))
                            .rounded(px(crate::theme::radius::MICRO))
                            .bg(Colors::with_alpha(Colors::accent_primary(), 0.18))
                            .border_l(px(1.0))
                            .border_color(Colors::with_alpha(Colors::accent_primary(), 0.6))
                            .text_size(px(9.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::accent_primary())
                            .child(label),
                    )
                })),
        )
}

#[cfg(test)]
mod loop_grab_tests {
    use super::*;

    /// The whole point of the Alt gesture is that it is *precise*: the press has
    /// to land on the loop to take it, so a press anywhere else on the ruler
    /// still moves the playhead even with Alt down.
    #[test]
    fn a_press_clear_of_the_loop_takes_nothing() {
        assert!(loop_grab_mode(10.0, 100.0, 200.0).is_none());
        assert!(loop_grab_mode(300.0, 100.0, 200.0).is_none());
    }

    #[test]
    fn the_body_slides_and_the_ends_stretch() {
        assert_eq!(
            loop_grab_mode(150.0, 100.0, 200.0),
            Some(TimelineRegionDragMode::Move)
        );
        assert_eq!(
            loop_grab_mode(102.0, 100.0, 200.0),
            Some(TimelineRegionDragMode::Start)
        );
        assert_eq!(
            loop_grab_mode(198.0, 100.0, 200.0),
            Some(TimelineRegionDragMode::End)
        );
    }

    /// Just outside an end still grabs that end: the loop's edge is a 2 px line,
    /// and asking for pixel-exact aim on it would make stretching a chore.
    #[test]
    fn the_grab_zone_reaches_outside_the_loop() {
        assert_eq!(
            loop_grab_mode(94.0, 100.0, 200.0),
            Some(TimelineRegionDragMode::Start)
        );
        assert_eq!(
            loop_grab_mode(206.0, 100.0, 200.0),
            Some(TimelineRegionDragMode::End)
        );
    }

    /// A loop zoomed down to a few pixels has two overlapping grab zones. It
    /// still has to be stretchable from either end, so the half the pointer is
    /// on decides — rather than one end always winning and the other becoming
    /// unreachable.
    #[test]
    fn a_tiny_loop_still_resolves_to_the_nearer_end() {
        assert_eq!(
            loop_grab_mode(101.0, 100.0, 106.0),
            Some(TimelineRegionDragMode::Start)
        );
        assert_eq!(
            loop_grab_mode(105.0, 100.0, 106.0),
            Some(TimelineRegionDragMode::End)
        );
    }
}
