use crate::components::timeline::timeline_state::{
    ClipDragItem, ClipEdge, ClipResizeDrag, ClipState, ClipTimeAxis, EffectiveFades, FadeEdge,
    StretchTiming, TimelineState, TimelineTool,
};
use crate::components::timeline::waveform_canvas::waveform_canvas;
use crate::theme::Colors;
use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, point, px, relative, AppContext, DragMoveEvent, Empty, InteractiveElement,
    IntoElement, ParentElement, PathBuilder, Render, StatefulInteractiveElement, Styled, Window,
};

/// A clip-gain or fade gesture on an audio clip, resolved by the timeline.
///
/// The fade variants carry raw window x: the timeline maps it through the one
/// arrangement transform and the grid, the way the Smart Tool's cut does, so a
/// fade lands where it is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioClipProcessUpdate {
    /// The inline gain was pressed: a new gain gesture starts.
    GainPress,
    Gain(f32),
    /// A fade handle was pressed at `window_x`. The press selects the clip as
    /// a press on its body does (`additive`: Cmd/Ctrl toggles it); the
    /// timeline keeps the offset from the fade's end, so the fade does not
    /// jump to the pointer once a drag starts.
    FadePress {
        edge: FadeEdge,
        window_x: f32,
        additive: bool,
    },
    /// The pressed fade handle moved past the drag threshold. Shift bypasses
    /// the grid.
    FadeDrag {
        edge: FadeEdge,
        window_x: f32,
        bypass_snap: bool,
    },
    /// A double-click on a fade handle: that fade back to zero. Its press
    /// selects the clip too.
    FadeReset {
        edge: FadeEdge,
        additive: bool,
    },
    /// The pointer entered (`true`) or left the clip. Its fade handles are
    /// revealed by an overlay, so a hover never rebuilds the lane.
    Hover(bool),
}

/// Why the clip-processing controls on an ARA track are disabled.
pub(crate) const ARA_CLIP_PROCESSING_TOOLTIP: &str =
    "Not applied on ARA tracks: the ARA plug-in renders this track's audio";

pub type AudioClipProcessPreviewCb = std::sync::Arc<
    dyn Fn(&(String, AudioClipProcessUpdate), &mut gpui::Window, &mut gpui::App) + 'static,
>;
pub type AudioClipProcessCommitCb =
    std::sync::Arc<dyn Fn(&(String, ClipState), &mut gpui::Window, &mut gpui::App) + 'static>;

/// A razor gesture on an audio clip, from the Cut tool or the Pointer's
/// Smart Tool cut zone. The timeline resolves every window x to a snapped beat
/// through one transform, so the line it shows is where the split lands.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipCutGesture {
    /// Split `clip_id` at `window_x`. Shift bypasses snap, matching the lane
    /// tools.
    Cut {
        clip_id: String,
        window_x: f32,
        bypass_snap: bool,
    },
    /// The pointer is over a clip's cut zone. `left` / `right` clamp the razor
    /// line to the clip, `top` / `height` span it; all in window coordinates.
    /// The line only shows while `armed` — the cut's modifier is held, by the
    /// same test the click makes ([`smart_cut_modifier_held`]) — and the
    /// timeline keeps the position so pressing or releasing a modifier
    /// without moving shows or hides it.
    Hover {
        window_x: f32,
        bypass_snap: bool,
        armed: bool,
        left: f32,
        right: f32,
        top: f32,
        height: f32,
    },
    /// The pointer left the cut zone, or pressed: hide the line.
    Leave,
}

/// Cut/razor callback. Optional so callers that never cut can pass `None`.
pub type AudioClipCutCb =
    std::sync::Arc<dyn Fn(&ClipCutGesture, &mut gpui::Window, &mut gpui::App) + 'static>;

/// Clips at least this wide get the wider edge handle, so the Smart Tool's
/// duration zone can be found without hunting for a 6 px strip.
const CLIP_WIDE_HANDLE_MIN_W: f32 = 64.0;

/// Width of each edge (duration) handle on a clip `clip_width` wide. Shared by
/// audio and MIDI clips and by the audio cut zone, which starts where the
/// handles stop.
pub(crate) fn clip_resize_handle_w(clip_width: f32) -> f32 {
    if clip_width >= CLIP_WIDE_HANDLE_MIN_W {
        10.0
    } else {
        6.0
    }
}

/// Narrowest clip that still gets a processing strip.
///
/// The strip spends 6 px of padding, a 2 px colour tick, a 5 px gap and a
/// ~38 px level readout before one character of the name can appear. Under
/// this it is a dark bar with nothing legible in it, and it costs two measured
/// text nodes per clip — the single largest item in a dense arrangement frame.
pub(crate) const CLIP_STRIP_MIN_W: f32 = 44.0;

/// Narrowest clip whose strip carries more than its name.
///
/// The level readout and the badges are fixed width, so they do not shrink out
/// of the way — they crowd the name out of a narrow clip while still being too
/// small to read themselves.
const CLIP_STRIP_DETAIL_MIN_W: f32 = 96.0;

/// Narrowest clip that gets edge resize handles.
///
/// Two 6 px handles on a 12 px clip cover the whole of it, leaving no body to
/// grab and no way to move the clip at all. Below this the clip is one target.
pub(crate) const CLIP_RESIZE_HANDLE_MIN_W: f32 = 20.0;

/// Width of one edge handle, exposed for the tier test. The handle itself is a
/// local constant inside the element builder, where it belongs.
#[cfg(test)]
const RESIZE_HANDLE_W_FOR_TESTS: f32 = 6.0;

/// A clip's 1 px border. Its children are laid out inside it, so every handle
/// position below is in that inner box.
pub(crate) const CLIP_BORDER: f32 = 1.0;

/// Hit area of a corner fade handle (and of a crossfade handle), square.
pub(crate) const FADE_HANDLE_HIT: f32 = 12.0;

/// The square a fade handle is drawn as, inside its hit area.
pub(crate) const FADE_HANDLE_SIZE: f32 = 8.0;

/// A rectangle in a clip's inner box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LocalRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl LocalRect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + self.width && y >= self.top && y < self.top + self.height
    }
}

/// One fade handle: where it takes presses, and the square drawn in that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FadeHandleRect {
    pub hit: LocalRect,
    pub mark: LocalRect,
}

/// What a press at a point of an audio clip lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipHitZone {
    FadeIn,
    FadeOut,
    TrimLeft,
    TrimRight,
    /// The Smart Tool's cut zone (Option-click splits).
    CutZone,
    /// Select, move, double-click to open.
    Body,
}

/// Where an audio clip's handles are, in its inner box. The clip element lays
/// its handles out from this and [`Self::hit`] answers what a press there
/// reaches, so the two cannot disagree.
///
/// Priority, top first: the corner fade handles, the full-height trim columns,
/// the cut zone, the body. The fade handles live in the top band the cut zone
/// leaves free ([`SMART_CUT_TOP_CLEARANCE`]), so they never cover it; the trim
/// columns keep the rest of each edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClipHandleLayout {
    pub width: f32,
    pub height: f32,
    pub trim_w: Option<f32>,
    pub fade_in: Option<FadeHandleRect>,
    pub fade_out: Option<FadeHandleRect>,
    pub cut_zone: Option<LocalRect>,
}

impl ClipHandleLayout {
    /// Handles for a clip drawn `width` × `height` (outer). `fade_in_x` /
    /// `fade_out_x` are where the fades end and start in the inner box; `None`
    /// leaves that handle out (another tool, a crossfaded edge).
    pub fn new(
        width: f32,
        height: f32,
        fade_in_x: Option<f32>,
        fade_out_x: Option<f32>,
        cut_zone: bool,
    ) -> Self {
        let inner_w = (width - 2.0 * CLIP_BORDER).max(0.0);
        let inner_h = (height - 2.0 * CLIP_BORDER).max(0.0);
        let edges = width >= CLIP_RESIZE_HANDLE_MIN_W;
        let trim_w = edges.then(|| clip_resize_handle_w(width));
        // Two handles side by side always fit: a zero fade's handle sits in
        // its corner, and the two never cover each other when the fades meet.
        let hit_w = FADE_HANDLE_HIT.min(inner_w * 0.5);
        let hit_h = FADE_HANDLE_HIT.min(SMART_CUT_TOP_CLEARANCE).min(inner_h);
        let mark_size = FADE_HANDLE_SIZE.min(hit_w).min(hit_h);
        let handle = |left: f32, fade_x: f32| {
            let mark_left = (fade_x - mark_size * 0.5)
                .min(left + hit_w - mark_size)
                .max(left);
            FadeHandleRect {
                hit: LocalRect {
                    left,
                    top: 0.0,
                    width: hit_w,
                    height: hit_h,
                },
                mark: LocalRect {
                    left: mark_left,
                    top: ((hit_h - mark_size) * 0.5).max(0.0),
                    width: mark_size,
                    height: mark_size,
                },
            }
        };
        // Room for the fade-out handle to its right, when there is one.
        let fade_in_max = if fade_out_x.is_some() {
            inner_w - 2.0 * hit_w
        } else {
            inner_w - hit_w
        };
        let fade_in = fade_in_x.filter(|_| edges).map(|x| {
            let left = (x - hit_w * 0.5).min(fade_in_max).max(0.0);
            handle(left, x)
        });
        let fade_out = fade_out_x.filter(|_| edges).map(|x| {
            let floor = fade_in.map_or(0.0, |rect| rect.hit.left + hit_w);
            let left = (x - hit_w * 0.5).min(inner_w - hit_w).max(floor);
            handle(left, x)
        });
        let cut_zone = trim_w.filter(|_| cut_zone).map(|trim_w| LocalRect {
            left: trim_w,
            top: SMART_CUT_TOP_CLEARANCE,
            width: (inner_w - 2.0 * trim_w).max(0.0),
            height: (inner_h - SMART_CUT_TOP_CLEARANCE).max(0.0),
        });
        Self {
            width: inner_w,
            height: inner_h,
            trim_w,
            fade_in,
            fade_out,
            cut_zone,
        }
    }

    /// What a press at inner-box point `(x, y)` reaches.
    pub fn hit(&self, x: f32, y: f32) -> ClipHitZone {
        // Painted last, so tested first; the two never overlap anyway.
        if self.fade_out.is_some_and(|rect| rect.hit.contains(x, y)) {
            return ClipHitZone::FadeOut;
        }
        if self.fade_in.is_some_and(|rect| rect.hit.contains(x, y)) {
            return ClipHitZone::FadeIn;
        }
        if let Some(trim_w) = self.trim_w {
            if x < trim_w {
                return ClipHitZone::TrimLeft;
            }
            if x >= self.width - trim_w {
                return ClipHitZone::TrimRight;
            }
        }
        if self.cut_zone.is_some_and(|rect| rect.contains(x, y)) {
            return ClipHitZone::CutZone;
        }
        ClipHitZone::Body
    }
}

/// Where a clip's fade handles go, from the fades it plays: at the end of the
/// fade-in and the start of the fade-out, through the clip's time map
/// ([`TimelineState::clip_time_axis`]) the curves are drawn with and a drag is
/// resolved through. A crossfaded edge gets no handle: the crossfade's own
/// handle replaces it. `clip_left` / `width` / `height` are the clip's drawn
/// rectangle.
pub(crate) fn clip_fade_handle_layout(
    time: &ClipTimeAxis<'_>,
    fades: &EffectiveFades,
    clip_left: f32,
    width: f32,
    height: f32,
    cut_zone: bool,
) -> ClipHandleLayout {
    let inner_left = clip_left + CLIP_BORDER;
    let inner_w = (width - 2.0 * CLIP_BORDER).max(0.0);
    let local_x =
        |seconds: f64| (time.lane_x_at_local_seconds(seconds) - inner_left).clamp(0.0, inner_w);
    let fade_in_x = (!fades.in_crossfade).then(|| local_x(fades.in_seconds));
    let fade_out_x = (!fades.out_crossfade)
        .then(|| local_x((fades.played_seconds - fades.out_seconds).max(0.0)));
    ClipHandleLayout::new(width, height, fade_in_x, fade_out_x, cut_zone)
}

/// The square a fade or crossfade handle is drawn as. Muted on an ARA track,
/// whose engine path ignores it.
pub(crate) fn fade_handle_mark(size: f32, active: bool, disabled: bool) -> gpui::Div {
    let border = if disabled {
        Colors::with_alpha(
            Colors::text_disabled(),
            crate::theme::state::DISABLED_CONTENT,
        )
    } else if active {
        Colors::text_primary()
    } else {
        Colors::accent_primary()
    };
    div()
        .w(px(size))
        .h(px(size))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(if active {
            Colors::accent_primary()
        } else {
            Colors::surface_input()
        })
        .border(px(1.0))
        .border_color(border)
}

/// Equal-power gain `t` of the way through a rising fade: the law the engine
/// plays (`FadeCurve::EqualPower`), so the drawn curve is the heard one.
pub(crate) fn equal_power_rising(t: f32) -> f32 {
    (t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2).sin()
}

/// One fade to draw: points `(x, gain)` in the clip's inner box.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FadeCurvePoints {
    pub points: Vec<(f32, f32)>,
}

/// How many segments a fade `width_px` wide is drawn with. Narrow fades get a
/// few (a zoomed-out arrangement draws many clips); nothing under 2 px.
pub(crate) fn fade_curve_segments(width_px: f32) -> usize {
    if width_px < 2.0 {
        0
    } else {
        ((width_px / 4.0) as usize).clamp(2, 32)
    }
}

/// Sample a clip's manual fade on `edge` for drawing; `None` on a crossfaded
/// edge, which the track's crossfade overlay draws. See [`fade_edge_curve`].
pub(crate) fn fade_curve_points(
    time: &ClipTimeAxis<'_>,
    fades: &EffectiveFades,
    edge: FadeEdge,
    inner_left: f32,
) -> Option<FadeCurvePoints> {
    if fades.crossfaded(edge) {
        return None;
    }
    fade_edge_curve(time, fades, edge, inner_left, None)
}

/// Sample the fade a clip plays on `edge` for drawing, stepping through its
/// own playing time and placing each step through its time map: under a
/// tempo ramp the curve bends exactly where the audio it fades does. The map
/// is resolved once per clip, so a sample is arithmetic, not a tempo-map
/// lookup.
///
/// `within` keeps only the part inside that span of the clip's time (a
/// crossfade overlay draws the part of each fade over its own overlap).
/// Points are `(x - origin_x, gain)`.
pub(crate) fn fade_edge_curve(
    time: &ClipTimeAxis<'_>,
    fades: &EffectiveFades,
    edge: FadeEdge,
    origin_x: f32,
    within: Option<(f64, f64)>,
) -> Option<FadeCurvePoints> {
    let seconds = fades.seconds(edge);
    if !(seconds > 0.0) {
        return None;
    }
    let (from, rising) = match edge {
        FadeEdge::In => (0.0, true),
        FadeEdge::Out => ((fades.played_seconds - seconds).max(0.0), false),
    };
    let (mut first, mut last) = (from, from + seconds);
    if let Some((low, high)) = within {
        first = first.max(low);
        last = last.min(high);
    }
    if !(last > first) {
        return None;
    }
    let x0 = time.lane_x_at_local_seconds(first);
    let x1 = time.lane_x_at_local_seconds(last);
    let segments = fade_curve_segments(x1 - x0);
    if segments == 0 {
        return None;
    }
    let linear = time.is_linear();
    let points = (0..=segments)
        .map(|i| {
            let u = i as f64 / segments as f64;
            let at = if i == segments {
                last
            } else {
                first + (last - first) * u
            };
            let x = if linear {
                x0 + (x1 - x0) * u as f32
            } else {
                time.lane_x_at_local_seconds(at)
            };
            let t = ((at - from) / seconds) as f32;
            let gain = if rising {
                equal_power_rising(t)
            } else {
                equal_power_rising(1.0 - t)
            };
            (x - origin_x, gain)
        })
        .collect();
    Some(FadeCurvePoints { points })
}

/// Paint fades as the engine plays them: the part each one removes shaded (when
/// `shade` is given), the gain drawn as a line. `top` / `bottom` bound the
/// curve in window y.
pub(crate) fn paint_fade_curves(
    curves: &[FadeCurvePoints],
    origin_x: f32,
    top: f32,
    bottom: f32,
    shade: Option<gpui::Rgba>,
    line_color: gpui::Rgba,
    window: &mut Window,
) {
    for curve in curves {
        let (Some(first), Some(last)) = (curve.points.first(), curve.points.last()) else {
            continue;
        };
        let y_at = |gain: f32| bottom - (bottom - top) * gain;
        let mut fill = PathBuilder::fill();
        let mut line = PathBuilder::stroke(px(1.0));
        fill.move_to(point(px(origin_x + first.0), px(top)));
        for (i, &(x, gain)) in curve.points.iter().enumerate() {
            let p = point(px(origin_x + x), px(y_at(gain)));
            fill.line_to(p);
            if i == 0 {
                line.move_to(p);
            } else {
                line.line_to(p);
            }
        }
        fill.line_to(point(px(origin_x + last.0), px(top)));
        fill.close();
        if let Some(shade) = shade {
            if let Ok(path) = fill.build() {
                window.paint_path(path, shade);
            }
        }
        if let Ok(path) = line.build() {
            window.paint_path(path, line_color);
        }
    }
}

/// Authoritative horizontal geometry for an audio clip: `(left_x, width_px)` in
/// lane space.
///
/// Both edges are resolved on the *beat* axis, which is the axis the ruler, the
/// grid, the playhead and every other lane are drawn on. The clip's end beat
/// comes from [`TimelineState::audio_clip_end_beat`] — the same derivation
/// `reconcile_audio_clip_lengths` writes into the model — so a clip is never
/// culled at one width and painted at another, and a tempo ramp bends the drawn
/// clip exactly as much as it bends the beat grid underneath it.
///
/// Resolving the width in seconds against `pixels_per_second` (which it used to
/// do) is only equivalent while the tempo is constant: under a tempo map the
/// seconds axis and the beat axis are different axes, and the clip drifted off
/// its own grid position by the difference.
pub(crate) fn audio_clip_timeline_geometry(clip: &ClipState, state: &TimelineState) -> (f32, f32) {
    // The shared clip geometry; see [`TimelineState::clip_lane_rect`].
    state.clip_lane_x_span(clip)
}

#[derive(Clone, Debug)]
struct AudioClipProcessDrag {
    id: String,
}

impl Render for AudioClipProcessDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.000_001 {
        -60.0
    } else {
        (20.0 * gain.log10()).clamp(-60.0, 12.0)
    }
}

fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db.clamp(-60.0, 12.0) / 20.0)
}

fn gain_to_norm(gain: f32) -> f32 {
    let db = gain_to_db(gain);
    if db <= 0.0 {
        ((db + 60.0) / 60.0) * 0.5
    } else {
        0.5 + (db / 12.0) * 0.5
    }
}

fn norm_to_gain(norm: f32) -> f32 {
    let norm = norm.clamp(0.0, 1.0);
    let db = if norm <= 0.5 {
        norm * 120.0 - 60.0
    } else {
        (norm - 0.5) * 24.0
    };
    db_to_gain(db)
}

/// The strip's inline clip gain. `disabled` on an ARA track, where the engine
/// does not apply clip gain: drawn muted, explained, and inert.
fn compact_gain_control(
    clip: &ClipState,
    disabled: bool,
    on_preview: AudioClipProcessPreviewCb,
    on_commit: AudioClipProcessCommitCb,
) -> gpui::AnyElement {
    let value = gain_to_norm(clip.gain).clamp(0.0, 1.0);
    let id = format!("audio-clip-gain-{}", clip.id);
    let move_id = id.clone();
    let preview_id = clip.id.clone();
    let commit_id = clip.id.clone();
    let commit_out_id = clip.id.clone();
    let original = clip.clone();
    let original_out = clip.clone();
    let on_commit_out = on_commit.clone();
    let reset_id = clip.id.clone();
    let reset_preview = on_preview.clone();

    let control = div()
        .id(gpui::ElementId::Name(id.into()))
        .w(px(62.0))
        .h(px(16.0))
        .flex_none()
        .relative()
        .when(disabled, |this| {
            this.opacity(crate::theme::state::DISABLED_CONTENT)
        })
        .child(
            div()
                .absolute()
                .left(px(4.0))
                .right(px(4.0))
                .top(px(7.0))
                .h(px(2.0))
                .rounded(px(crate::theme::radius::PILL))
                .bg(Colors::fader_rail())
                .border(px(1.0))
                .border_color(Colors::fader_groove()),
        )
        .child(
            div()
                .absolute()
                .left(relative(value))
                .ml(-px(3.0))
                .top(px(2.0))
                .w(px(6.0))
                .h(px(12.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::surface_input())
                .border(px(1.0))
                .border_color(Colors::fader_thumb_border())
                .child(
                    div()
                        .absolute()
                        .top(px(2.0))
                        .bottom(px(2.0))
                        .left(px(2.0))
                        .w(px(1.0))
                        .bg(Colors::accent_primary()),
                ),
        );
    if disabled {
        return control
            .tooltip(crate::components::fb_tooltip(ARA_CLIP_PROCESSING_TOOLTIP))
            .into_any_element();
    }
    control
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
            cx.stop_propagation();
            reset_preview(
                &(reset_id.clone(), AudioClipProcessUpdate::GainPress),
                window,
                cx,
            );
            if event.click_count >= 2 {
                // Clip gain is a bipolar dB control, independent of the track
                // volume fader. Its neutral/reset value is always 0 dB.
                reset_preview(
                    &(
                        reset_id.clone(),
                        AudioClipProcessUpdate::Gain(db_to_gain(0.0)),
                    ),
                    window,
                    cx,
                );
            }
        })
        .on_drag(
            AudioClipProcessDrag {
                id: move_id.clone(),
            },
            |drag, _offset, _window, cx| cx.new(|_| drag.clone()),
        )
        .on_drag_move::<AudioClipProcessDrag>(
            move |event: &DragMoveEvent<AudioClipProcessDrag>, window, cx| {
                if event.drag(cx).id != move_id {
                    return;
                }
                let x: f32 = event.event.position.x.into();
                let ox: f32 = event.bounds.origin.x.into();
                let width = f32::from(event.bounds.size.width).max(1.0);
                let gain = norm_to_gain((x - ox) / width);
                on_preview(
                    &(preview_id.clone(), AudioClipProcessUpdate::Gain(gain)),
                    window,
                    cx,
                );
            },
        )
        .on_mouse_up(gpui::MouseButton::Left, move |_, window, cx| {
            on_commit(&(commit_id.clone(), original.clone()), window, cx);
        })
        .on_mouse_up_out(gpui::MouseButton::Left, move |_, window, cx| {
            on_commit_out(&(commit_out_id.clone(), original_out.clone()), window, cx);
        })
        .into_any_element()
}

/// Height of the band along a clip's top edge the Smart Tool's cut zone leaves
/// free, so the corner fade handles have the corners to themselves.
const SMART_CUT_TOP_CLEARANCE: f32 = 12.0;

/// Farthest the pointer may travel between press and release for a click in
/// the cut zone to split: GPUI's own drag threshold, past which it is a drag.
const SMART_CUT_MAX_TRAVEL_PX: f32 = 2.0;

/// Whether a click in the Smart Tool's cut zone splits the clip.
///
/// Only an Option/Alt single click does: Option held at the press *and* the
/// release (Shift may join it to bypass snap; Cmd or Ctrl may not), the first
/// click of a sequence, no travel past the drag threshold and no drag in
/// flight. Every other click is the ordinary select click the clip body has
/// already handled — including the first click of a double-click meant to open
/// the editor — and an Option-drag stays a clone.
pub(crate) fn smart_cut_click_splits(
    down: &gpui::Modifiers,
    up: &gpui::Modifiers,
    click_count: usize,
    travel_px: f32,
    drag_active: bool,
) -> bool {
    smart_cut_modifier_held(down)
        && smart_cut_modifier_held(up)
        && click_count == 1
        && travel_px <= SMART_CUT_MAX_TRAVEL_PX
        && !drag_active
}

/// The Smart Tool cut's modifier: Option/Alt, with Shift allowed (it only
/// bypasses snap) and Cmd, Ctrl and Fn not. The razor line shows exactly
/// while this holds, so it never promises a split the click will not make.
pub(crate) fn smart_cut_modifier_held(modifiers: &gpui::Modifiers) -> bool {
    modifiers.alt && !modifiers.platform && !modifiers.control && !modifiers.function
}

/// The Pointer's cut zone: the clip body between its edge handles, below the
/// top band the fade handles use. With Option/Alt held, hovering shows the
/// razor line and a single click splits there (see
/// [`smart_cut_click_splits`]). Without it the zone is inert: a press selects
/// the clip and a drag moves it — the clip body owns both — and the strip's
/// inline gain, drawn above the zone, still takes its own presses.
fn smart_cut_zone(
    clip_id: &str,
    id_num: usize,
    handle_w: f32,
    on_cut: AudioClipCutCb,
) -> impl IntoElement {
    let bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>> =
        Default::default();
    let measured = bounds.clone();
    let on_move = on_cut.clone();
    let on_hover = on_cut.clone();
    let on_down = on_cut.clone();
    let clip_id = clip_id.to_string();

    div()
        .id(("audio-clip-cut-zone", id_num))
        .absolute()
        .left(px(handle_w))
        .right(px(handle_w))
        .top(px(SMART_CUT_TOP_CLEARANCE))
        .bottom_0()
        .child(
            canvas(
                move |zone, _window, _cx| measured.set(Some(zone)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_move(move |event: &gpui::MouseMoveEvent, window, cx| {
            if event.pressed_button.is_some() {
                on_move(&ClipCutGesture::Leave, window, cx);
                return;
            }
            let Some(zone) = bounds.get() else {
                return;
            };
            let top: f32 = zone.origin.y.into();
            let height: f32 = zone.size.height.into();
            on_move(
                &ClipCutGesture::Hover {
                    window_x: event.position.x.into(),
                    bypass_snap: event.modifiers.shift,
                    armed: smart_cut_modifier_held(&event.modifiers),
                    left: zone.origin.x.into(),
                    right: (zone.origin.x + zone.size.width).into(),
                    // The line spans the whole clip, not only the zone.
                    top: top - SMART_CUT_TOP_CLEARANCE,
                    height: height + SMART_CUT_TOP_CLEARANCE,
                },
                window,
                cx,
            );
        })
        .on_hover(move |hovered, window, cx| {
            if !*hovered {
                on_hover(&ClipCutGesture::Leave, window, cx);
            }
        })
        // No stop_propagation: the clip body still selects on this press.
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            on_down(&ClipCutGesture::Leave, window, cx);
        })
        .on_click(move |event, window, cx| {
            let gpui::ClickEvent::Mouse(click) = event else {
                return;
            };
            // A drag of the clip that started here also ends here; that was a
            // move, not a cut. So is every click without Option.
            let travel = (click.up.position - click.down.position).magnitude() as f32;
            if !smart_cut_click_splits(
                &click.down.modifiers,
                &click.up.modifiers,
                click.down.click_count,
                travel,
                cx.has_active_drag(),
            ) {
                return;
            }
            on_cut(
                &ClipCutGesture::Cut {
                    clip_id: clip_id.clone(),
                    window_x: click.up.position.x.into(),
                    bypass_snap: click.up.modifiers.shift,
                },
                window,
                cx,
            );
        })
}

/// A corner fade handle: a small hit area at the top edge where the fade ends
/// (fade-in) or starts (fade-out), with a diagonal resize cursor. It stops the
/// press, so the clip body never moves or cuts from it, but the press still
/// selects the clip by the body's rule (`Timeline::press_clip_selection`): the
/// hit areas are there on every clip, shown or not, and a plain click on one
/// must not be lost. The timeline resolves the drag and commits it when the
/// button comes up (see `Timeline::finish_clip_handle_gestures`).
///
/// The square is drawn here only when `show_mark` (a selected clip); a hovered
/// clip's squares are painted by the timeline's overlay, so hovering never
/// rebuilds the lane. On an ARA track the handle only explains why it does
/// nothing, and its press reaches the body, which selects.
#[allow(clippy::too_many_arguments)]
fn fade_handle(
    clip_id: &str,
    id_num: usize,
    edge: FadeEdge,
    rect: FadeHandleRect,
    show_mark: bool,
    disabled: bool,
    on_preview: AudioClipProcessPreviewCb,
) -> gpui::AnyElement {
    let (name, cursor) = match edge {
        FadeEdge::In => (
            "audio-clip-fade-in",
            gpui::CursorStyle::ResizeUpLeftDownRight,
        ),
        FadeEdge::Out => (
            "audio-clip-fade-out",
            gpui::CursorStyle::ResizeUpRightDownLeft,
        ),
    };
    let handle = div()
        .id((name, id_num))
        .absolute()
        .left(px(rect.hit.left))
        .top(px(rect.hit.top))
        .w(px(rect.hit.width))
        .h(px(rect.hit.height))
        .children(show_mark.then(|| {
            fade_handle_mark(rect.mark.width, false, disabled)
                .absolute()
                .left(px(rect.mark.left - rect.hit.left))
                .top(px(rect.mark.top - rect.hit.top))
        }));
    if disabled {
        return handle
            .tooltip(crate::components::fb_tooltip(ARA_CLIP_PROCESSING_TOOLTIP))
            .into_any_element();
    }
    let drag_id = format!("{name}-{clip_id}");
    let move_id = drag_id.clone();
    let press_id = clip_id.to_string();
    let drag_clip_id = clip_id.to_string();
    let on_drag_preview = on_preview.clone();
    handle
        .cursor(cursor)
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                // The press selects the clip as a press on its body does.
                let additive = event.modifiers.control || event.modifiers.platform;
                let update = if event.click_count >= 2 {
                    AudioClipProcessUpdate::FadeReset { edge, additive }
                } else {
                    AudioClipProcessUpdate::FadePress {
                        edge,
                        window_x: event.position.x.into(),
                        additive,
                    }
                };
                on_preview(&(press_id.clone(), update), window, cx);
            },
        )
        .on_drag(
            AudioClipProcessDrag { id: drag_id },
            |drag, _offset, _window, cx| cx.new(|_| drag.clone()),
        )
        .on_drag_move::<AudioClipProcessDrag>(
            move |event: &DragMoveEvent<AudioClipProcessDrag>, window, cx| {
                if event.drag(cx).id != move_id {
                    return;
                }
                on_drag_preview(
                    &(
                        drag_clip_id.clone(),
                        AudioClipProcessUpdate::FadeDrag {
                            edge,
                            window_x: event.event.position.x.into(),
                            bypass_snap: event.event.modifiers.shift,
                        },
                    ),
                    window,
                    cx,
                );
            },
        )
        .into_any_element()
}

pub struct ClipDragPreview {
    pub name: String,
    pub color: gpui::Rgba,
}

impl Render for ClipDragPreview {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .h(px(24.0))
            .min_w(px(96.0))
            .max_w(px(220.0))
            .px(px(8.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color({
                let mut c = self.color;
                c.a = 0.7;
                c
            })
            .bg(Colors::surface_raised())
            .shadow_lg()
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .text_size(px(10.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.name.clone()),
            )
    }
}

/// What the clip's processing strip says about its stretch state.
///
/// `locked` is the tempo-follow flag, not "has a ratio": a Tempo Sync clip's
/// bar count belongs to the project tempo, so its badge has to read differently
/// from a Manual clip that merely happens to sit at some ratio right now. The
/// caller paints the two on separate channels — colour *and* glyph — because
/// "is this clip pinned to the grid?" is the question a tempo change makes you
/// ask about every clip at once.
struct StretchBadge {
    label: String,
    locked: bool,
}

fn stretch_badge(clip: &ClipState, state: &TimelineState) -> Option<StretchBadge> {
    let stretch = &clip.stretch;
    let (semi, cents) = stretch.pitch_semi_and_cents();
    let transpose = (stretch.transpose_available() && stretch.pitch_shift_semitones.abs() > 1.0e-4)
        .then(|| {
            if cents.abs() >= 0.5 {
                format!("{:+.0}st {:+.0}c", semi, cents)
            } else {
                format!("{:+.0}st", semi)
            }
        });
    let locked = stretch.follows_project_tempo();
    let timing = match stretch.timing() {
        StretchTiming::Off => None,
        StretchTiming::Tempo => Some(match stretch.bpm_source {
            Some(source_bpm) => format!("{source_bpm:.0}→{:.0}", state.bpm),
            None => "Tempo ?".to_string(),
        }),
        StretchTiming::Speed => {
            let ratio = stretch.effective_time_ratio(state.bpm as f64);
            Some(format!("{:.0}%", ratio * 100.0))
        }
        StretchTiming::Warp => Some(format!("Warp {}", stretch.warp_markers.len())),
    };
    let label = match (timing, transpose) {
        (None, None) => return None,
        (Some(timing), None) => timing,
        (None, Some(transpose)) => transpose,
        (Some(timing), Some(transpose)) => format!("{timing} {transpose}"),
    };
    Some(StretchBadge { label, locked })
}

pub fn audio_clip(
    clip: &ClipState,
    track_id: &str,
    track_color: gpui::Rgba,
    state: &TimelineState,
    row_height: f32,
    on_select_clip: std::sync::Arc<
        dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_open_editor: Option<std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>>,
    _on_context_menu: Option<
        std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_erase_clip: Option<
        std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_cut_clip: Option<AudioClipCutCb>,
    erase_target: bool,
    fades: EffectiveFades,
    time: &ClipTimeAxis<'_>,
    ara: bool,
    on_process_preview: AudioClipProcessPreviewCb,
    on_process_commit: AudioClipProcessCommitCb,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("AudioClip");
    let clip_id = clip.id.clone();
    let drag_clip_id = clip.id.clone();
    let drag_track_id = track_id.to_string();
    let drag_name = clip.name.clone();
    let drag_start_beat = clip.start_beat;
    let selected = state
        .display_selection()
        .selected_clip_ids
        .contains(&clip.id);
    let stretch_badge = stretch_badge(clip, state);
    let (left, width) = audio_clip_timeline_geometry(clip, state);
    let has_crossfade = fades.any();
    // Detail by width. Below each threshold the thing being dropped is already
    // illegible, so this is what the clip *should* look like — and it is also
    // where the arrangement's frame goes.
    //
    // The strip's two text runs are measured layout nodes: a zoomed-out
    // arrangement is exactly the case with the most clips and the least room on
    // each, and a screenful of clips too narrow to read was measured at 59% of
    // the whole frame drawing labels nobody can read. See
    // `gpui::frame_bench::arrangement_frame_phase_cost`.
    let show_strip = width >= CLIP_STRIP_MIN_W;
    let show_strip_detail = width >= CLIP_STRIP_DETAIL_MIN_W;
    let show_resize_handles = width >= CLIP_RESIZE_HANDLE_MIN_W;
    let show_inline_gain = selected
        && width
            >= if has_crossfade || stretch_badge.is_some() {
                220.0
            } else {
                150.0
            };
    let gain_db = gain_to_db(clip.gain);

    // Vertical geometry from the shared clip rectangle, the one the marquee
    // hit-tests against.
    let lane_rect = state.clip_lane_rect(clip, row_height);
    let pad = lane_rect.top;
    let clip_h = lane_rect.height;

    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        clip.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    let on_select = on_select_clip.clone();
    let open_editor = on_open_editor.clone();
    let clip_for_erase = clip.id.clone();
    let erase_cb = on_erase_clip.clone();
    let active_tool = state.active_tool;
    let cut_cb = on_cut_clip.clone();
    let clip_for_cut = clip.id.clone();
    let resize_left = ClipResizeDrag {
        clip_id: clip.id.clone(),
        edge: ClipEdge::Left,
        start_beat: clip.start_beat,
        duration_beats: clip.duration_beats,
    };
    let resize_right = ClipResizeDrag {
        clip_id: clip.id.clone(),
        edge: ClipEdge::Right,
        start_beat: clip.start_beat,
        duration_beats: clip.duration_beats,
    };
    let resize_handle_w = clip_resize_handle_w(width);
    const HEADER_H: f32 = 20.0;
    // With no strip the waveform owns the whole clip, and the fade curves
    // reach the bottom edge instead of stopping above a bar that is not there.
    let strip_h = if show_strip { HEADER_H } else { 0.0 };
    let pointer = active_tool == TimelineTool::Pointer;
    // Corner fade handles: Pointer only. Their hit areas are always there —
    // small and cheap — and a hovered clip's squares are painted by the
    // timeline's overlay; a selected clip draws its own.
    let handles =
        pointer.then(|| clip_fade_handle_layout(time, &fades, left, width, clip_h, false));
    // The fades as the engine plays them, over the waveform. A crossfaded
    // edge is drawn by the track's crossfade overlay instead.
    let curves: Vec<FadeCurvePoints> = [FadeEdge::In, FadeEdge::Out]
        .into_iter()
        .filter_map(|edge| fade_curve_points(time, &fades, edge, left + CLIP_BORDER))
        .collect();
    let (curve_shade, curve_line) = if ara {
        (
            Colors::with_alpha(Colors::surface_canvas(), 0.25),
            Colors::with_alpha(
                Colors::text_disabled(),
                crate::theme::state::DISABLED_CONTENT,
            ),
        )
    } else {
        (
            Colors::with_alpha(Colors::surface_canvas(), 0.55),
            Colors::text_secondary(),
        )
    };
    let gain_preview = on_process_preview.clone();
    let gain_commit = on_process_commit;
    let hover_preview = on_process_preview.clone();
    let hover_clip_id = clip.id.clone();
    let fade_preview = on_process_preview;

    div()
        .absolute()
        .left(px(left))
        .top(px(pad))
        .w(px(width))
        .h(px(clip_h))
        .rounded(px(crate::theme::radius::CONTROL))
        .overflow_hidden()
        .bg(Colors::timeline_audio_clip_fill(track_color, selected))
        .border(px(1.0))
        .border_color(if erase_target {
            Colors::status_error()
        } else {
            Colors::timeline_audio_clip_border(track_color, selected)
        })
        .cursor(if active_tool == TimelineTool::Cut {
            // Cut tool: a press splits (never drags), so the cursor says razor,
            // not move — a stray `C` must not leave a silent split armed.
            gpui::CursorStyle::Crosshair
        } else {
            gpui::CursorStyle::OpenHand
        })
        .id(("audio-clip", id_num))
        // Entering or leaving the clip reveals or hides its fade handles
        // through the timeline's overlay, without rebuilding this lane.
        .when(
            handles.is_some_and(|layout| layout.fade_in.is_some() || layout.fade_out.is_some()),
            move |this| {
                this.on_hover(move |hovered, window, cx| {
                    hover_preview(
                        &(
                            hover_clip_id.clone(),
                            AudioClipProcessUpdate::Hover(*hovered),
                        ),
                        window,
                        cx,
                    );
                })
            },
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                // Cut/razor tool: a click splits the clip at the cursor instead
                // of selecting/opening it. The timeline resolves the window x to
                // a snapped beat (Shift bypasses snap, matching the lane tools).
                if active_tool == TimelineTool::Cut {
                    if let Some(cut) = cut_cb.as_ref() {
                        cut(
                            &ClipCutGesture::Cut {
                                clip_id: clip_for_cut.clone(),
                                window_x: event.position.x.into(),
                                bypass_snap: event.modifiers.shift,
                            },
                            window,
                            cx,
                        );
                    }
                    return;
                }
                let additive = event.modifiers.control || event.modifiers.platform;
                on_select(
                    &(clip_id.clone(), additive, event.modifiers.alt),
                    window,
                    cx,
                );
                if event.click_count >= 2 {
                    if let Some(open) = open_editor.as_ref() {
                        open(window, cx);
                    }
                }
            },
        )
        .on_mouse_down(
            gpui::MouseButton::Right,
            move |_event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if let Some(erase) = erase_cb.as_ref() {
                    erase(&clip_for_erase, window, cx);
                }
            },
        )
        .on_drag(
            ClipDragItem {
                clip_id: drag_clip_id,
                source_track_id: drag_track_id,
                start_beat: drag_start_beat,
            },
            move |_drag, _offset, _window, cx| {
                cx.new(|_| ClipDragPreview {
                    name: drag_name.clone(),
                    color: track_color,
                })
            },
        )
        .flex()
        .flex_col()
        .justify_between()
        // Waveform preview area
        .child(div().flex_1().min_h_0().child(waveform_canvas(
            clip,
            track_color,
            state,
            left,
            width,
        )))
        // Smart Tool: an Option-click on the Pointer's clip body cuts (see
        // `smart_cut_zone`). Before the strip, so the strip's own controls
        // (inline gain) stay on top of it.
        .children(
            (active_tool == TimelineTool::Pointer && show_resize_handles)
                .then(|| on_cut_clip.clone())
                .flatten()
                .map(|cut| smart_cut_zone(&clip.id, id_num, resize_handle_w, cut)),
        )
        // Processing strip: clip identity, inline gain, crossfade, and stretch.
        // Absent on a clip too narrow to show any of it — see
        // [`CLIP_STRIP_MIN_W`].
        .children(show_strip.then(|| {
            div()
                .h(px(HEADER_H))
                .flex_none()
                .bg(if selected {
                    Colors::surface_selected_soft()
                } else {
                    Colors::surface_panel_alt()
                })
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .pl(px(6.0))
                .pr(px(4.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .child(
                    div()
                        .w(px(2.0))
                        .h(px(10.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .bg(track_color),
                )
                .child(
                    div()
                        .text_size(px(9.0))
                        .min_w(px(0.0))
                        .flex_1()
                        .truncate()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if selected {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(clip.name.clone()),
                )
                .children(
                    show_inline_gain
                        .then(|| compact_gain_control(clip, ara, gain_preview, gain_commit)),
                )
                // The level readout is fixed width, so on a narrow clip it does
                // not shrink — it takes the name's room and is still too small
                // to read. See [`CLIP_STRIP_DETAIL_MIN_W`].
                .children((show_strip_detail).then(|| {
                    div()
                        .flex_none()
                        .text_size(px(8.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if gain_db.abs() > 0.05 {
                            Colors::accent_primary()
                        } else {
                            Colors::text_muted()
                        })
                        .child(format!("{gain_db:+.1} dB"))
                }))
                .children((show_strip_detail && has_crossfade).then(|| {
                    let badge = div()
                        .id(("audio-clip-xfade", id_num))
                        .flex_none()
                        .px(px(4.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .text_size(px(7.5))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("XFADE");
                    if ara {
                        badge
                            .bg(Colors::with_alpha(Colors::text_disabled(), 0.14))
                            .text_color(Colors::text_disabled())
                            .tooltip(crate::components::fb_tooltip(ARA_CLIP_PROCESSING_TOOLTIP))
                            .into_any_element()
                    } else {
                        badge
                            .bg(Colors::accent_soft())
                            .text_color(Colors::accent_primary())
                            .into_any_element()
                    }
                }))
                .children(stretch_badge.filter(|_| show_strip_detail).map(|badge| {
                    // A tempo-locked clip is marked on two channels: the
                    // automation hue (the same one the tempo lane uses, because
                    // that is what owns the clip's length) plus a leading glyph.
                    // A merely-stretched clip keeps the accent and no glyph.
                    let hue = if badge.locked {
                        Colors::state_automation()
                    } else {
                        Colors::accent_primary()
                    };
                    let mut chip = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .flex_none()
                        .px(px(4.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .bg(Colors::with_alpha(hue, 0.14))
                        .text_size(px(8.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(hue);
                    if badge.locked {
                        chip = chip.child(
                            gpui::svg()
                                .path(crate::assets::ICON_MAGNET_PATH)
                                .w(px(8.0))
                                .h(px(8.0))
                                .text_color(hue),
                        );
                    }
                    chip.child(badge.label)
                }))
            // Clip length text intentionally not rendered on the clip body — the
            // name (flex_1) fills the bar, so no gap remains. Duration stays in the
            // model and the inspector; resize/trim handles are unaffected.
        }))
        // The fades as they play: the shaded part is what each one removes.
        .children((!curves.is_empty()).then(move || {
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let top: f32 = bounds.origin.y.into();
                    let bottom = top + f32::from(bounds.size.height);
                    paint_fade_curves(
                        &curves,
                        bounds.origin.x.into(),
                        top,
                        bottom,
                        Some(curve_shade),
                        curve_line,
                        window,
                    );
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom(px(strip_h))
        }))
        // Edge handles, on a clip wide enough to have edges *and* a body — see
        // [`CLIP_RESIZE_HANDLE_MIN_W`].
        .children(show_resize_handles.then(|| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(px(resize_handle_w))
                .cursor(gpui::CursorStyle::ResizeLeft)
                .id(("audio-clip-resize-l", id_num))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(resize_left, |drag, _offset, _window, cx| {
                    cx.new(|_| drag.clone())
                })
        }))
        .children(show_resize_handles.then(|| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(resize_handle_w))
                .cursor(gpui::CursorStyle::ResizeRight)
                .id(("audio-clip-resize-r", id_num))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(resize_right, |drag, _offset, _window, cx| {
                    cx.new(|_| drag.clone())
                })
        }))
        // Corner fade handles, after the edge handles so they win the top of
        // each edge; the edge keeps the rest of its column. See
        // [`ClipHandleLayout`] for the whole priority order.
        .children(handles.and_then(|layout| layout.fade_in).map(|rect| {
            fade_handle(
                &clip.id,
                id_num,
                FadeEdge::In,
                rect,
                selected,
                ara,
                fade_preview.clone(),
            )
        }))
        .children(handles.and_then(|layout| layout.fade_out).map(|rect| {
            fade_handle(
                &clip.id,
                id_num,
                FadeEdge::Out,
                rect,
                selected,
                ara,
                fade_preview.clone(),
            )
        }))
}

#[cfg(test)]
mod tests {
    use super::{
        audio_clip_timeline_geometry, db_to_gain, gain_to_db, gain_to_norm, norm_to_gain,
        CLIP_RESIZE_HANDLE_MIN_W, CLIP_STRIP_DETAIL_MIN_W, CLIP_STRIP_MIN_W,
    };
    use crate::components::timeline::timeline_state::{
        AudioClipStretchState, AudioImportState, ClipState, ClipType, TimelineState,
    };

    /// The tiers have to stay in the order they describe, and the narrowest one
    /// has to stay narrow enough that a clip anybody would read a name on still
    /// has one.
    ///
    /// They are not cosmetic: the strip's two text runs are measured layout
    /// nodes, and on a zoomed-out arrangement — the case with the most clips and
    /// the least room on each — drawing them was measured at 59% of the frame.
    /// Raising `CLIP_STRIP_MIN_W` back toward zero puts that cost back.
    #[test]
    fn clip_detail_tiers_stay_ordered_and_useful() {
        assert!(
            CLIP_RESIZE_HANDLE_MIN_W < CLIP_STRIP_MIN_W,
            "a clip should get its edges back before it gets a label bar"
        );
        assert!(
            CLIP_STRIP_MIN_W < CLIP_STRIP_DETAIL_MIN_W,
            "the strip has to exist before it can carry a readout"
        );
        // Two 6 px handles need a body between them to leave anything to grab.
        assert!(CLIP_RESIZE_HANDLE_MIN_W >= 2.0 * super::RESIZE_HANDLE_W_FOR_TESTS);
        assert_eq!(
            super::clip_resize_handle_w(CLIP_RESIZE_HANDLE_MIN_W),
            super::RESIZE_HANDLE_W_FOR_TESTS
        );
        // The wide handles still leave at least as much body as the narrow
        // tier does, so the Smart Tool's drag and cut zones never vanish.
        let wide = super::CLIP_WIDE_HANDLE_MIN_W;
        assert!(
            wide - 2.0 * super::clip_resize_handle_w(wide)
                >= CLIP_RESIZE_HANDLE_MIN_W - 2.0 * super::RESIZE_HANDLE_W_FOR_TESTS
        );
        // A clip wide enough for a few characters of name keeps its strip.
        assert!(
            CLIP_STRIP_MIN_W <= 64.0,
            "a readable clip lost its name bar"
        );
    }

    /// The Smart Tool cut zone used to split on any plain click in the lower
    /// half of a clip, which is where people click to select it.
    #[test]
    fn only_an_option_single_click_splits_in_the_cut_zone() {
        use super::smart_cut_click_splits as splits;
        let plain = gpui::Modifiers::default();
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        let alt_shift = gpui::Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        let cmd = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };
        let cmd_alt = gpui::Modifiers {
            platform: true,
            alt: true,
            ..Default::default()
        };
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };

        assert!(
            !splits(&plain, &plain, 1, 0.0, false),
            "a plain click selects"
        );
        assert!(
            !splits(&cmd, &cmd, 1, 0.0, false),
            "Cmd-click is additive select"
        );
        assert!(!splits(&shift, &shift, 1, 0.0, false));
        assert!(!splits(&cmd_alt, &cmd_alt, 1, 0.0, false));

        assert!(splits(&alt, &alt, 1, 0.0, false));
        assert!(
            splits(&alt, &alt, 1, 2.0, false),
            "within the drag threshold"
        );
        assert!(
            splits(&alt_shift, &alt_shift, 1, 1.0, false),
            "Shift bypasses snap"
        );

        assert!(
            !splits(&alt, &alt, 1, 2.5, false),
            "past the drag threshold"
        );
        assert!(!splits(&alt, &alt, 2, 0.0, false), "part of a double-click");
        assert!(!splits(&alt, &alt, 1, 0.0, true), "a drag is in flight");
        assert!(!splits(&alt, &plain, 1, 0.0, false), "Option let go first");
        assert!(!splits(&plain, &alt, 1, 0.0, false), "Option pressed late");
    }

    /// The razor line shows under exactly the modifiers a click would split
    /// with. It used to show whenever Option was held, so Cmd+Option drew a
    /// line where the click only selected.
    #[test]
    fn the_razor_line_shows_exactly_when_a_click_would_split() {
        use super::{smart_cut_click_splits, smart_cut_modifier_held};
        for bits in 0..32_u8 {
            let modifiers = gpui::Modifiers {
                alt: bits & 1 != 0,
                shift: bits & 2 != 0,
                platform: bits & 4 != 0,
                control: bits & 8 != 0,
                function: bits & 16 != 0,
            };
            assert_eq!(
                smart_cut_modifier_held(&modifiers),
                smart_cut_click_splits(&modifiers, &modifiers, 1, 0.0, false),
                "{modifiers:?}"
            );
        }
        let cmd_alt = gpui::Modifiers {
            platform: true,
            alt: true,
            ..Default::default()
        };
        assert!(!smart_cut_modifier_held(&cmd_alt));
    }

    /// Every width tier that has edge handles, from the narrowest up.
    const HANDLE_TIERS: [f32; 7] = [20.0, 30.0, 44.0, 63.0, 64.0, 96.0, 400.0];
    const CLIP_H: f32 = 58.0;

    fn center(rect: super::LocalRect) -> (f32, f32) {
        (rect.left + rect.width * 0.5, rect.top + rect.height * 0.5)
    }

    fn overlaps(a: super::LocalRect, b: super::LocalRect) -> bool {
        a.left < b.left + b.width
            && b.left < a.left + a.width
            && a.top < b.top + b.height
            && b.top < a.top + a.height
    }

    /// Priority: a corner fade handle beats the trim column under it, the trim
    /// beats the body, and the fade handles never reach into the Smart Tool's
    /// cut zone.
    #[test]
    fn corner_fade_handles_beat_trim_and_trim_beats_the_body() {
        use super::{ClipHandleLayout, ClipHitZone};
        for width in HANDLE_TIERS {
            let inner_w = width - 2.0;
            let layout = ClipHandleLayout::new(width, CLIP_H, Some(0.0), Some(inner_w), true);
            let fade_in = layout.fade_in.expect("fade-in handle");
            let fade_out = layout.fade_out.expect("fade-out handle");
            let trim_w = layout.trim_w.expect("edge handles");
            let (x, y) = center(fade_in.hit);
            assert_eq!(layout.hit(x, y), ClipHitZone::FadeIn, "width {width}");
            let (x, y) = center(fade_out.hit);
            assert_eq!(layout.hit(x, y), ClipHitZone::FadeOut, "width {width}");
            // The trim keeps the rest of its column.
            assert_eq!(layout.hit(0.5, 20.0), ClipHitZone::TrimLeft);
            assert_eq!(layout.hit(inner_w - 0.5, 20.0), ClipHitZone::TrimRight);
            // Under the top band, between the edges, is the cut zone.
            let cut = layout.cut_zone.expect("cut zone");
            assert_eq!(layout.hit(trim_w + 0.5, 30.0), ClipHitZone::CutZone);
            for handle in [fade_in, fade_out] {
                assert!(
                    !overlaps(handle.hit, cut),
                    "width {width}: handle over the cut zone"
                );
                assert!(handle.hit.top + handle.hit.height <= super::SMART_CUT_TOP_CLEARANCE);
                assert!(handle.hit.left >= 0.0 && handle.hit.left + handle.hit.width <= inner_w);
                // The square drawn is inside the area that takes the press.
                assert!(handle.mark.left >= handle.hit.left);
                assert!(handle.mark.left + handle.mark.width <= handle.hit.left + handle.hit.width);
            }
            // The top band between the corners still moves the clip.
            if inner_w > 2.0 * super::FADE_HANDLE_HIT {
                assert_eq!(layout.hit(inner_w * 0.5, 4.0), ClipHitZone::Body);
            }
            // Without the cut zone (another tool) the middle is body.
            let plain = ClipHandleLayout::new(width, CLIP_H, Some(0.0), Some(inner_w), false);
            assert_eq!(plain.hit(inner_w * 0.5, 30.0), ClipHitZone::Body);
        }
    }

    /// A zero-length fade can always be pulled out of its corner: at every
    /// width tier both corner handles exist, sit in their corners, and neither
    /// covers the other.
    #[test]
    fn a_zero_fade_handle_is_reachable_at_every_width_tier() {
        use super::{ClipHandleLayout, ClipHitZone};
        for width in HANDLE_TIERS {
            let inner_w = width - 2.0;
            let layout = ClipHandleLayout::new(width, CLIP_H, Some(0.0), Some(inner_w), true);
            let (fade_in, fade_out) = (layout.fade_in.unwrap(), layout.fade_out.unwrap());
            assert!(!overlaps(fade_in.hit, fade_out.hit), "width {width}");
            assert_eq!(layout.hit(0.5, 0.5), ClipHitZone::FadeIn, "width {width}");
            assert_eq!(
                layout.hit(inner_w - 0.5, 0.5),
                ClipHitZone::FadeOut,
                "width {width}"
            );
            assert_eq!(fade_in.mark.left, 0.0, "the square sits in the corner");
            assert_eq!(fade_out.mark.left + fade_out.mark.width, inner_w);
        }
    }

    /// Fades that meet leave both handles reachable; a crossfaded edge has no
    /// handle; a clip too narrow for edge handles has none at all.
    #[test]
    fn fade_handles_stay_apart_and_only_where_they_belong() {
        use super::{ClipHandleLayout, ClipHitZone};
        let layout = ClipHandleLayout::new(200.0, CLIP_H, Some(99.0), Some(99.0), true);
        let (fade_in, fade_out) = (layout.fade_in.unwrap(), layout.fade_out.unwrap());
        assert!(!overlaps(fade_in.hit, fade_out.hit));
        assert_eq!(layout.hit(center(fade_in.hit).0, 4.0), ClipHitZone::FadeIn);
        assert_eq!(
            layout.hit(center(fade_out.hit).0, 4.0),
            ClipHitZone::FadeOut
        );

        let crossfaded = ClipHandleLayout::new(200.0, CLIP_H, None, Some(198.0), true);
        assert!(crossfaded.fade_in.is_none());
        assert_eq!(crossfaded.hit(0.5, 0.5), ClipHitZone::TrimLeft);

        let narrow_w = CLIP_RESIZE_HANDLE_MIN_W - 1.0;
        let narrow = ClipHandleLayout::new(narrow_w, CLIP_H, Some(0.0), Some(17.0), true);
        assert!(narrow.fade_in.is_none() && narrow.fade_out.is_none());
        assert!(narrow.trim_w.is_none() && narrow.cut_zone.is_none());
        assert_eq!(narrow.hit(1.0, 1.0), ClipHitZone::Body);
    }

    /// The handle is where the fade is drawn, and dragging it resolves back to
    /// the same fade: one transform for drawing and hit-testing.
    #[test]
    fn a_fade_handle_sits_where_its_curve_ends() {
        use super::{clip_fade_handle_layout, fade_curve_points, CLIP_BORDER};
        use crate::components::timeline::timeline_state::FadeEdge;

        let mut clip = two_second_clip("clip-fade");
        clip.start_beat = 1.0;
        clip.stretch.fade_in_ms = 500.0;
        clip.stretch.fade_out_ms = 250.0;
        let mut state = state_with_clip(clip, 120.0);
        state.viewport.pixels_per_second = 400.0;
        state.sync_pixels_per_beat();
        let clip = state.tracks[0].clips[0].clone();
        let crossfades = state.audio_crossfades(&state.tracks[0]);
        let fades = state.effective_clip_fades(&clip, &crossfades);
        let (left, width) = audio_clip_timeline_geometry(&clip, &state);
        let inner_left = left + CLIP_BORDER;
        let time = state.clip_time_axis(&clip);
        let layout = clip_fade_handle_layout(&time, &fades, left, width, CLIP_H, true);

        let curve_in = fade_curve_points(&time, &fades, FadeEdge::In, inner_left).unwrap();
        let fade_in_end = curve_in.points.last().unwrap().0;
        let mark = layout.fade_in.unwrap().mark;
        assert!((mark.left + mark.width * 0.5 - fade_in_end).abs() < 0.01);
        let back = state.clip_local_seconds_at_lane_x(&clip, inner_left + fade_in_end);
        assert!(
            (back - 0.5).abs() < 0.002,
            "grabbing the handle reads {back} s"
        );

        let curve_out = fade_curve_points(&time, &fades, FadeEdge::Out, inner_left).unwrap();
        let fade_out_start = curve_out.points.first().unwrap().0;
        let mark = layout.fade_out.unwrap().mark;
        assert!((mark.left + mark.width * 0.5 - fade_out_start).abs() < 0.01);
        // The curve is the engine's equal-power law, not a straight ramp.
        let n = curve_in.points.len() - 1;
        for (i, &(_, gain)) in curve_in.points.iter().enumerate() {
            let t = i as f32 / n as f32;
            assert!((gain - (t * std::f32::consts::FRAC_PI_2).sin()).abs() < 1.0e-6);
        }
        let (_, mid) = curve_in.points[n / 2];
        assert!(
            mid > (n / 2) as f32 / n as f32 + 0.1,
            "bowed above a linear ramp"
        );
    }

    /// A fade curve drawn through the clip's axis, built once, lands every
    /// point exactly where the per-call transform (a tempo-map lookup per
    /// sample, as the curves used to be drawn and as a drag is still
    /// resolved) puts it: flat and under a tempo ramp, where each point is
    /// placed through the map.
    #[test]
    fn a_fade_curve_through_a_cached_axis_is_the_direct_transform() {
        use super::{fade_curve_points, fade_curve_segments, CLIP_BORDER};
        use crate::components::timeline::timeline_state::{FadeEdge, TempoCurve};

        for ramped in [false, true] {
            let mut clip = two_second_clip("clip-ramp");
            clip.start_beat = 1.0;
            clip.stretch.fade_in_ms = 700.0;
            clip.stretch.fade_out_ms = 900.0;
            let mut state = state_with_clip(clip, 120.0);
            state.viewport.pixels_per_second = 400.0;
            state.sync_pixels_per_beat();
            if ramped {
                state
                    .tempo_map
                    .add_or_update_point(0.0, 60.0, TempoCurve::Linear);
                state
                    .tempo_map
                    .add_or_update_point(8.0, 180.0, TempoCurve::Hold);
                state.reconcile_audio_clip_lengths();
            }
            state.sync_time_warp();
            let clip = state.tracks[0].clips[0].clone();
            let crossfades = state.audio_crossfades(&state.tracks[0]);
            let fades = state.effective_clip_fades(&clip, &crossfades);
            let (left, _) = audio_clip_timeline_geometry(&clip, &state);
            let inner_left = left + CLIP_BORDER;
            let time = state.clip_time_axis(&clip);
            for edge in [FadeEdge::In, FadeEdge::Out] {
                let curve = fade_curve_points(&time, &fades, edge, inner_left).unwrap();
                let seconds = fades.seconds(edge);
                let from = match edge {
                    FadeEdge::In => 0.0,
                    FadeEdge::Out => fades.played_seconds - seconds,
                };
                let direct_x = |at: f64| state.clip_lane_x_at_local_seconds(&clip, at) - inner_left;
                let segments = fade_curve_segments(direct_x(from + seconds) - direct_x(from));
                assert_eq!(curve.points.len(), segments + 1);
                for (i, &(x, _)) in curve.points.iter().enumerate() {
                    let at = if i == segments {
                        from + seconds
                    } else {
                        from + seconds * (i as f64 / segments as f64)
                    };
                    if ramped || i == 0 || i == segments {
                        // The ends (where the handle sits) always, and every
                        // point under a ramp: exactly the direct transform.
                        assert_eq!(x, direct_x(at), "ramped={ramped} {edge:?} point {i}");
                    } else {
                        // Flat tempo draws a straight line between the two
                        // ends; the direct transform rounds each point to a
                        // whole pixel.
                        assert!((x - direct_x(at)).abs() <= 0.5, "{edge:?} point {i}");
                    }
                }
            }
        }
    }

    #[test]
    fn fade_curves_are_drawn_only_when_they_are_visible() {
        assert_eq!(super::fade_curve_segments(1.5), 0);
        assert_eq!(super::fade_curve_segments(2.0), 2);
        assert_eq!(super::fade_curve_segments(40.0), 10);
        assert_eq!(super::fade_curve_segments(4_000.0), 32);
    }

    #[test]
    fn clip_gain_mapping_roundtrips_unity_and_limits() {
        assert!((gain_to_db(1.0) - 0.0).abs() < 1.0e-6);
        assert!((gain_to_norm(1.0) - 0.5).abs() < 1.0e-6);
        assert!((norm_to_gain(gain_to_norm(1.0)) - 1.0).abs() < 1.0e-5);
        assert!((db_to_gain(12.0) - 10.0_f32.powf(12.0 / 20.0)).abs() < 1.0e-5);
        assert_eq!(gain_to_db(0.0), -60.0);
    }

    #[test]
    fn timeline_geometry_uses_trimmed_source_window_not_full_asset_duration() {
        let state = TimelineState::default();
        let mut clip = ClipState {
            id: "clip-trimmed".to_string(),
            name: "Trimmed".to_string(),
            start_beat: 2.0,
            duration_beats: 4.0,
            source_duration_seconds: Some(30.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "asset".to_string(),
                source_path: Some("take.wav".to_string()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.source_start_samples = 48_000;
        clip.stretch.source_end_samples = 144_000;

        let (left, width) = audio_clip_timeline_geometry(&clip, &state);
        assert!((left - 150.0).abs() < 0.001);
        assert!((width - 300.0).abs() < 0.001);

        clip.stretch.source_end_samples = 96_000;
        let (_, trimmed_width) = audio_clip_timeline_geometry(&clip, &state);
        assert!((trimmed_width - 150.0).abs() < 0.001);
    }

    /// Build a two-second audio clip at 48 kHz, un-stretched.
    fn two_second_clip(id: &str) -> ClipState {
        let mut clip = ClipState {
            id: id.to_string(),
            name: "Take".to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            source_duration_seconds: Some(2.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "asset".to_string(),
                source_path: Some("take.wav".to_string()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = 96_000;
        clip.stretch.source_start_samples = 0;
        clip.stretch.source_end_samples = 96_000;
        clip
    }

    fn state_with_clip(clip: ClipState, bpm: f32) -> TimelineState {
        let mut state = TimelineState::default();
        state.bpm = bpm;
        let track_id = state.create_audio_track();
        let track = state
            .tracks
            .iter_mut()
            .find(|t| t.id == track_id)
            .expect("track");
        track.clips.push(clip);
        state.reconcile_audio_clip_lengths();
        state
    }

    /// An un-stretched clip is two seconds of audio at any tempo. Doubling the
    /// project tempo must double its bar count, not squeeze the audio.
    #[test]
    fn an_unstretched_clip_keeps_its_seconds_when_the_tempo_changes() {
        let mut state = state_with_clip(two_second_clip("clip-off"), 120.0);
        let before = state.tracks[0].clips[0].duration_beats;
        assert!((before - 4.0).abs() < 0.01, "2 s at 120 BPM is 4 beats");

        state.bpm = 240.0;
        assert!(state.reconcile_audio_clip_lengths());
        let after = state.tracks[0].clips[0].duration_beats;
        assert!(
            (after - 8.0).abs() < 0.01,
            "2 s at 240 BPM is 8 beats, got {after}"
        );
    }

    /// A tempo-synced clip is *defined* in bars. Doubling the tempo must leave
    /// its bar count alone — that is what "locked to the timeline" means.
    #[test]
    fn a_tempo_synced_clip_keeps_its_bars_when_the_tempo_changes() {
        let mut clip = two_second_clip("clip-sync");
        clip.stretch.mode = crate::components::timeline::timeline_state::StretchMode::TempoSync;
        clip.stretch.bpm_source = Some(120.0);
        clip.stretch.apply_tempo_sync(120.0);
        let mut state = state_with_clip(clip, 120.0);
        let before = state.tracks[0].clips[0].duration_beats;

        state.bpm = 240.0;
        state
            .tracks
            .iter_mut()
            .flat_map(|t| t.clips.iter_mut())
            .for_each(|c| c.stretch.apply_tempo_sync(240.0));
        state.reconcile_audio_clip_lengths();
        let after = state.tracks[0].clips[0].duration_beats;
        assert!(
            (after - before).abs() < 0.05,
            "a tempo-synced clip keeps {before} beats, got {after}"
        );
    }

    /// The drawn width and the model's bar count describe one object: after a
    /// tempo change the clip must be grabbable exactly where it is painted.
    #[test]
    fn the_drawn_width_and_the_model_agree_after_a_tempo_change() {
        let mut state = state_with_clip(two_second_clip("clip-agree"), 120.0);
        state.bpm = 172.0;
        state.sync_pixels_per_beat();
        state.reconcile_audio_clip_lengths();

        let clip = &state.tracks[0].clips[0];
        let (_, drawn_w) = audio_clip_timeline_geometry(clip, &state);
        let model_w = clip.duration_beats * state.viewport.pixels_per_beat;
        assert!(
            (drawn_w - model_w).abs() < 1.5,
            "drawn {drawn_w} px vs model {model_w} px"
        );
    }

    /// A tempo marker inside an un-stretched clip must not move a sample of it.
    ///
    /// The clip plays for a fixed wall-clock length, so a section that speeds up
    /// under it covers *more* beats in the same time. The bar count is what
    /// moves; the audio does not.
    #[test]
    fn a_tempo_change_under_an_unstretched_clip_keeps_its_wall_clock_length() {
        use crate::components::timeline::timeline_state::TempoCurve;

        let mut state = state_with_clip(two_second_clip("clip-drift"), 120.0);
        let before_beats = state.tracks[0].clips[0].duration_beats;
        assert!((before_beats - 4.0).abs() < 0.01);

        // Double the tempo one beat into the clip: the first beat runs at 120,
        // the rest at 240, so two seconds now covers 1 + 3 = ... beats.
        state
            .tempo_map
            .add_or_update_point(0.0, 120.0, TempoCurve::Hold);
        state
            .tempo_map
            .add_or_update_point(1.0, 240.0, TempoCurve::Hold);
        assert!(state.reconcile_audio_clip_lengths());

        let clip = &state.tracks[0].clips[0];
        // 1 beat at 120 BPM = 0.5 s, leaving 1.5 s at 240 BPM = 6 beats.
        assert!(
            (clip.duration_beats - 7.0).abs() < 0.01,
            "expected 7 beats, got {}",
            clip.duration_beats
        );
        // And the length it actually plays is untouched.
        let seconds = state.seconds_at_beat((clip.start_beat + clip.duration_beats) as f64)
            - state.seconds_at_beat(clip.start_beat as f64);
        assert!((seconds - 2.0).abs() < 1.0e-6, "played {seconds} s");
    }

    /// A tempo ramp under an un-stretched clip is the same promise: the clip
    /// still plays for exactly as long, whatever the beat grid does over it.
    #[test]
    fn a_tempo_ramp_under_an_unstretched_clip_keeps_its_wall_clock_length() {
        use crate::components::timeline::timeline_state::TempoCurve;

        let mut state = state_with_clip(two_second_clip("clip-ramp"), 120.0);
        state
            .tempo_map
            .add_or_update_point(0.0, 60.0, TempoCurve::Linear);
        state
            .tempo_map
            .add_or_update_point(8.0, 180.0, TempoCurve::Smooth);
        state
            .tempo_map
            .add_or_update_point(16.0, 90.0, TempoCurve::Hold);
        state.reconcile_audio_clip_lengths();

        let clip = &state.tracks[0].clips[0];
        let seconds = state.seconds_at_beat((clip.start_beat + clip.duration_beats) as f64)
            - state.seconds_at_beat(clip.start_beat as f64);
        assert!(
            (seconds - 2.0).abs() < 1.0e-5,
            "a ramp moved the clip's length to {seconds} s"
        );
    }

    /// A clip whose source has not been decoded yet has nothing authoritative
    /// to derive from and must be left where it is.
    #[test]
    fn a_pending_clip_is_left_alone() {
        let mut clip = two_second_clip("clip-pending");
        clip.stretch.source_end_samples = 0;
        clip.stretch.original_duration_samples = 0;
        clip.audio_import = AudioImportState::Pending;
        let mut state = state_with_clip(clip, 120.0);
        state.bpm = 240.0;
        assert!(!state.reconcile_audio_clip_lengths());
        assert!((state.tracks[0].clips[0].duration_beats - 4.0).abs() < 1.0e-6);
    }
}
