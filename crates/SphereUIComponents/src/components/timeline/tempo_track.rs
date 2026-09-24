use crate::components::timeline::global_lane_header::{
    global_lane_header, global_lane_resize_handle, GlobalLaneHeaderActions, GlobalLaneResizeArmCb,
    GlobalLaneResizeResetCb,
};
use crate::components::timeline::marker_flag::{marker_flag_layer, MarkerFlag};
use crate::components::timeline::timeline_grid::timeline_grid;
use crate::components::timeline::timeline_state::{
    bpm_to_y, GlobalLaneKind, TempoMap, TimelineState, TEMPO_LANE_PAD,
};
use crate::theme::Colors;
use gpui::{
    canvas, div, fill, point, px, size, Bounds, InteractiveElement, IntoElement, ParentElement,
    PathBuilder, PathStyle, Pixels, StrokeOptions, Styled,
};

/// A left-button press on the Tempo lane, resolved against the lane.
#[derive(Debug, Clone)]
pub struct TempoLaneDown {
    /// Pointer beat, snapped to the grid (where a new marker would go).
    pub beat: f64,
    pub bpm: f64,
    pub window_y: f32,
    /// Marker under the pointer.
    pub point_id: Option<String>,
    /// Left marker of the ramp whose drawn line is under the pointer, when the
    /// press is on the line between two markers rather than on a marker.
    pub segment_left_id: Option<String>,
    pub additive: bool,
    pub alt: bool,
    pub click_count: u32,
}

/// Tempo Track mouse-down.
pub type TempoTrackDownCallback =
    std::sync::Arc<dyn Fn(&TempoLaneDown, &mut gpui::Window, &mut gpui::App) + 'static>;

/// Tempo Track context menu: `(beat, bpm, point_id, screen_x, screen_y)`.
pub type TempoTrackContextCallback = std::sync::Arc<
    dyn Fn(&(f64, f64, Option<String>, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

pub type GlobalLaneVoidCallback =
    std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>;
pub type GlobalLaneMenuCallback =
    std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>;

/// Pointer slop around a tempo marker, in lane pixels, on both axes.
///
/// The dot is 7-9 px across; without a pad this size it is a smaller target
/// than the pointer that has to hit it.
const TEMPO_POINT_HIT_PX: f32 = 12.0;

/// Pointer slop around the drawn tempo line, in lane pixels, for grabbing a
/// ramp to bend it.
const TEMPO_CURVE_HIT_PX: f32 = 6.0;

/// What the lane's pointer handlers need, owned. Never a clone of the whole
/// `TimelineState`: that deep-copied every track, clip and note in the
/// project twice per render (7 ms per copy at 32 tracks, ~90 ms at 64), which
/// is what made pressing and dragging on the lane lag.
struct TempoLaneHit {
    gesture: crate::components::timeline::timeline_state::TimelineGestureContext,
    tempo: TempoMap,
    base_bpm: f64,
    origin_y: f32,
    lane_h: f32,
    min_bpm: f64,
    max_bpm: f64,
}

impl TempoLaneHit {
    fn new(state: &TimelineState, min_bpm: f64, max_bpm: f64) -> Self {
        Self {
            gesture: state.gesture_context(),
            tempo: state.tempo_map.clone(),
            base_bpm: state.bpm as f64,
            origin_y: state.tempo_lane_origin_y(),
            lane_h: state.tempo_track_height(),
            min_bpm,
            max_bpm,
        }
    }

    /// Same mapping as [`TimelineState::tempo_bpm_at_window_y`].
    fn bpm_at_window_y(&self, window_y: f32) -> f64 {
        crate::components::timeline::timeline_state::y_to_bpm(
            window_y - self.origin_y,
            self.lane_h,
            self.min_bpm,
            self.max_bpm,
        )
    }

    /// [`TEMPO_POINT_HIT_PX`] in beats and BPM at the current zoom and range.
    fn tolerances(&self) -> (f64, f64) {
        let ppb = self.gesture.viewport.pixels_per_beat.max(1.0) as f64;
        let usable = (self.lane_h - 2.0 * TEMPO_LANE_PAD).max(1.0) as f64;
        (
            TEMPO_POINT_HIT_PX as f64 / ppb,
            (self.max_bpm - self.min_bpm) * TEMPO_POINT_HIT_PX as f64 / usable,
        )
    }

    fn point_at(&self, beat: f64, bpm: f64) -> Option<String> {
        let (beat_tol, bpm_tol) = self.tolerances();
        self.tempo
            .points
            .iter()
            .find(|p| (p.beat - beat).abs() <= beat_tol && (p.bpm - bpm).abs() <= bpm_tol)
            .map(|p| p.id.clone())
    }

    /// Left marker of the ramp whose drawn line passes within
    /// [`TEMPO_CURVE_HIT_PX`] of the pointer. Only ramps between two real
    /// markers qualify: the stretch before the first marker is the base
    /// tempo, which has no marker to carry a bend.
    fn segment_at(&self, beat: f64, window_y: f32) -> Option<String> {
        let points = &self.tempo.points;
        let index = points.iter().rposition(|p| p.beat <= beat)?;
        points.get(index + 1)?;
        let line_y = self.origin_y
            + bpm_to_y(
                self.tempo.bpm_at_beat(beat, self.base_bpm),
                self.lane_h,
                self.min_bpm,
                self.max_bpm,
            );
        ((window_y - line_y).abs() <= TEMPO_CURVE_HIT_PX).then(|| points[index].id.clone())
    }
}

/// Global Tempo Track lane — header + automation curve over the project TempoMap.
pub fn tempo_track_lane(
    state: &TimelineState,
    lane_height: f32,
    on_down: Option<TempoTrackDownCallback>,
    on_context: Option<TempoTrackContextCallback>,
    on_add: Option<GlobalLaneVoidCallback>,
    on_header_menu: Option<GlobalLaneMenuCallback>,
    on_hide: Option<GlobalLaneVoidCallback>,
    on_toggle_collapsed: Option<GlobalLaneVoidCallback>,
    on_resize_arm: Option<GlobalLaneResizeArmCb>,
    on_resize_reset: Option<GlobalLaneResizeResetCb>,
) -> impl IntoElement {
    let (min_bpm, max_bpm) = state.tempo_lane_bpm_range();
    let lane_w = state.viewport.viewport_width.max(1.0);
    let num_cols = lane_w.ceil().max(1.0) as usize;

    let mut samples: Vec<f32> = Vec::with_capacity(num_cols + 1);
    for col in 0..=num_cols {
        let beat = state.x_to_beat(col as f32);
        let bpm = state.effective_bpm_at_beat(beat);
        samples.push(bpm_to_y(bpm, lane_height, min_bpm, max_bpm));
    }

    let line_color = Colors::accent_primary();
    let fill_under = Colors::with_alpha(Colors::accent_primary(), 0.08);
    let baseline_bpm = if state.tempo_map.points.is_empty() {
        state.bpm as f64
    } else {
        state.tempo_map.points[0].bpm
    };
    let baseline_y = bpm_to_y(baseline_bpm, lane_height, min_bpm, max_bpm);
    let baseline_color = Colors::with_alpha(Colors::text_primary(), 0.12);

    let curve = canvas(
        |_b, _w, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let bl = Bounds::new(
                bounds.origin + point(px(0.0), px(baseline_y)),
                size(px(lane_w), px(1.0)),
            );
            window.paint_quad(fill(bl, baseline_color));

            // A single stroked path lets GPUI's tessellator generate coverage
            // for fractional y positions. Per-column quads produced stair-step
            // edges and shimmered when the lane was zoomed or displayed at 125%
            // / 150% Windows scaling.
            if samples.len() >= 2 {
                let options = StrokeOptions::default()
                    .with_line_width(1.6)
                    .with_miter_limit(2.0);
                let mut path = PathBuilder::stroke(px(1.6)).with_style(PathStyle::Stroke(options));
                path.move_to(bounds.origin + point(px(0.0), px(samples[0])));
                for (col, y) in samples.iter().enumerate().skip(1) {
                    path.line_to(bounds.origin + point(px(col as f32), px(*y)));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, line_color);
                }
            }

            // One 1px column per pixel: a continuous wash under the curve.
            // This used to paint a 2px bar every 3rd column, which is a 2-on
            // 1-off stripe — at any zoom that beats against the pixel grid and
            // reads as moiré rather than as a filled region.
            for col in 0..num_cols {
                let top = samples[col].min(samples[col + 1]);
                let fill_h = (lane_height - top).max(0.0);
                if fill_h > 0.5 {
                    let fr = Bounds::new(
                        bounds.origin + point(px(col as f32), px(top)),
                        size(px(1.0), px(fill_h)),
                    );
                    window.paint_quad(fill(fr, fill_under));
                }
            }
        },
    )
    .absolute()
    .inset_0();

    let points = state.tempo_map.points.clone();
    let selected_id = state.selected_tempo_point_id.clone();
    let show_all_labels = points.len() <= 1;
    // The dot stays on the curve at the marker's actual BPM height — that is
    // both the value readout and the drag target. The label moved off it into a
    // flag anchored on the beat, so the text no longer floats above the curve
    // needing to be clamped away from the lane edges.
    let mut markers: Vec<gpui::Div> = Vec::new();
    let mut flags: Vec<MarkerFlag> = Vec::new();
    for p in &points {
        let x = state.beats_to_x(p.beat as f32);
        if x < -64.0 || x > lane_w + 64.0 {
            continue;
        }
        let y = bpm_to_y(p.bpm, lane_height, min_bpm, max_bpm);
        let selected = selected_id.as_deref() == Some(p.id.as_str());
        let size_px = if selected { 9.0 } else { 7.0 };
        let (fill_color, ring) = if selected {
            (Colors::text_primary(), Colors::accent_primary())
        } else {
            (
                Colors::accent_primary(),
                Colors::with_alpha(Colors::text_primary(), 0.55),
            )
        };

        // A 7 px dot is smaller than the pointer that has to land on it, so the
        // dot sits inside a transparent pad the size of the lane's hit slop.
        // The pad carries the cursor only — the lane's single hit layer still
        // resolves every press, so the two can never disagree about which
        // marker was meant.
        markers.push(
            div()
                .absolute()
                .left(px(x - TEMPO_POINT_HIT_PX))
                .top(px(y - TEMPO_POINT_HIT_PX))
                .w(px(TEMPO_POINT_HIT_PX * 2.0))
                .h(px(TEMPO_POINT_HIT_PX * 2.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor(gpui::CursorStyle::PointingHand)
                .child(
                    div()
                        .w(px(size_px))
                        .h(px(size_px))
                        .rounded(px(crate::theme::radius::PILL))
                        .bg(fill_color)
                        .border(px(1.5))
                        .border_color(ring),
                ),
        );

        if selected || show_all_labels {
            flags.push(MarkerFlag {
                x,
                label: TempoMap::format_marker_label(p.bpm),
                selected,
            });
        }
    }
    // A project with a constant tempo has no points at all, so without this the
    // lane would sit empty and give no reading. Show the *effective* value as an
    // implicit flag instead of writing an anchor point into the map — a real
    // point would make `tempo_has_automation()` true and light the AUTO badge on
    // a project that has no automation.
    if points.is_empty() {
        flags.push(MarkerFlag {
            x: state.beats_to_x(0.0),
            label: TempoMap::format_marker_label(state.bpm as f64),
            selected: false,
        });
    }
    let (flag_layer, flag_labels) = marker_flag_layer(flags, lane_w, lane_height);

    let subtitle = state.tempo_lane_header_subtitle();

    let hit = std::rc::Rc::new(TempoLaneHit::new(state, min_bpm, max_bpm));
    let interaction = on_down.map(|cb| {
        let hit_left = hit.clone();
        let mut layer = div()
            .absolute()
            .inset_0()
            .id("tempo-track-hit")
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let hit = &hit_left;
                    let wx: f32 = event.position.x.into();
                    let wy: f32 = event.position.y.into();
                    let lane_x = hit.gesture.lane_x_from_window_x(wx);
                    let beat = hit.gesture.x_to_beat(lane_x).max(0.0);
                    let snapped = hit.gesture.snap_beats(beat as f32) as f64;
                    let bpm = hit.bpm_at_window_y(wy);
                    // The *raw* beat, not the snapped one: at high zoom the snap
                    // step is wider than the tolerance, so snapping first made a
                    // marker sitting off the grid impossible to grab.
                    let point_id = hit.point_at(beat, bpm);
                    let segment_left_id = if point_id.is_none() {
                        hit.segment_at(beat, wy)
                    } else {
                        None
                    };
                    cb(
                        &TempoLaneDown {
                            beat: snapped,
                            bpm,
                            window_y: wy,
                            point_id,
                            segment_left_id,
                            additive: event.modifiers.shift || event.modifiers.control,
                            alt: event.modifiers.alt,
                            click_count: event.click_count as u32,
                        },
                        window,
                        cx,
                    );
                },
            );
        if let Some(ctx_cb) = on_context {
            let hit_right = hit.clone();
            layer = layer.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let hit = &hit_right;
                    let wx: f32 = event.position.x.into();
                    let wy: f32 = event.position.y.into();
                    let lane_x = hit.gesture.lane_x_from_window_x(wx);
                    let beat = hit.gesture.x_to_beat(lane_x).max(0.0);
                    let bpm = hit.bpm_at_window_y(wy);
                    // A right-click on a ramp's line targets the marker that
                    // starts it, so its curve menu is one click away.
                    let point_id = hit.point_at(beat, bpm).or_else(|| hit.segment_at(beat, wy));
                    ctx_cb(&(beat, bpm, point_id, wx, wy), window, cx);
                },
            );
        }
        layer
    });

    let header = global_lane_header(
        "tempo",
        "Tempo",
        subtitle,
        state.tempo_track_collapsed,
        "Hide Tempo Track",
        GlobalLaneHeaderActions {
            on_add,
            on_menu: on_header_menu,
            on_hide,
            on_toggle_collapsed,
        },
    );

    let resize_handle = on_resize_arm
        .zip(on_resize_reset)
        .map(|(arm, reset)| global_lane_resize_handle(GlobalLaneKind::Tempo, arm, reset));

    div()
        .flex()
        .flex_row()
        .relative()
        .h(px(lane_height))
        .w_full()
        .bg(Colors::surface_panel_alt())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(header)
        .child(
            div()
                .flex_1()
                .h_full()
                .relative()
                .overflow_hidden()
                // The lane's own surface stops at the header. The content area
                // takes the arrangement's background and its grid, so musical
                // time runs unbroken from the ruler down through the tracks.
                .bg(Colors::timeline_content_background())
                .child(timeline_grid(state, lane_w, lane_height))
                .child(curve)
                .child(flag_layer)
                .children(flag_labels)
                .children(interaction)
                .children(markers)
                // Debug: outline tempo_lane_content_rect (FUTUREBOARD_UI_DEBUG_CLIPS=1).
                .children(crate::perf::debug_clip_outline()),
        )
        .children(resize_handle)
}
