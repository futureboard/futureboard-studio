use crate::components::timeline::timeline_state::{
    ClipDragItem, ClipEdge, ClipResizeDrag, ClipState, StretchTiming, TimelineState, TimelineTool,
};
use crate::components::timeline::waveform_canvas::waveform_canvas;
use crate::theme::Colors;
use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, relative, AppContext, DragMoveEvent, Empty, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioClipProcessUpdate {
    Gain(f32),
    FadeInMs(f32),
    FadeOutMs(f32),
}

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
    /// The line only shows while `alt` is held — the modifier the cut needs —
    /// and the timeline keeps the position so pressing or releasing Option
    /// without moving shows or hides it.
    Hover {
        window_x: f32,
        bypass_snap: bool,
        alt: bool,
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
const CLIP_STRIP_MIN_W: f32 = 44.0;

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
const CLIP_RESIZE_HANDLE_MIN_W: f32 = 20.0;

/// Width of one edge handle, exposed for the tier test. The handle itself is a
/// local constant inside the element builder, where it belongs.
#[cfg(test)]
const RESIZE_HANDLE_W_FOR_TESTS: f32 = 6.0;

/// Wall-clock length the clip plays for, in seconds.
///
/// Fades are authored in milliseconds, so they are resolved against this rather
/// than against the beat span the clip happens to cover.
fn audio_clip_timeline_duration_seconds(clip: &ClipState, state: &TimelineState) -> f32 {
    if let Some(seconds) = clip
        .stretch
        .played_seconds_for_project_bpm(state.bpm.max(1.0) as f64)
    {
        return (seconds as f32).max(0.001);
    }
    // Pending and legacy clips may not have decoded source bounds yet.
    (clip.duration_beats * state.seconds_per_beat()).max(0.001)
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

fn compact_gain_control(
    clip: &ClipState,
    on_preview: AudioClipProcessPreviewCb,
    on_commit: AudioClipProcessCommitCb,
) -> impl IntoElement {
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

    div()
        .id(gpui::ElementId::Name(id.into()))
        .w(px(62.0))
        .h(px(16.0))
        .flex_none()
        .relative()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
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
        )
        .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
            cx.stop_propagation();
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
    let option_only = |m: &gpui::Modifiers| m.alt && !m.platform && !m.control && !m.function;
    option_only(down)
        && option_only(up)
        && click_count == 1
        && travel_px <= SMART_CUT_MAX_TRAVEL_PX
        && !drag_active
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
                    alt: event.modifiers.alt,
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

#[derive(Clone, Copy)]
enum FadeEdge {
    In,
    Out,
}

#[allow(clippy::too_many_arguments)]
fn fade_drag_zone(
    clip: &ClipState,
    edge: FadeEdge,
    clip_duration_seconds: f32,
    fade_width: f32,
    body_height: f32,
    on_preview: AudioClipProcessPreviewCb,
    on_commit: AudioClipProcessCommitCb,
) -> impl IntoElement {
    let edge_name = match edge {
        FadeEdge::In => "in",
        FadeEdge::Out => "out",
    };
    let id = format!("audio-clip-fade-{edge_name}-{}", clip.id);
    let move_id = id.clone();
    let preview_id = clip.id.clone();
    let commit_id = clip.id.clone();
    let commit_out_id = clip.id.clone();
    let original = clip.clone();
    let original_out = clip.clone();
    let on_commit_out = on_commit.clone();

    div()
        .id(gpui::ElementId::Name(id.into()))
        .absolute()
        .top_0()
        .when(matches!(edge, FadeEdge::In), |this| this.left_0())
        .when(matches!(edge, FadeEdge::Out), |this| this.right_0())
        .w(relative(0.5))
        .h(px(10.0))
        .cursor(match edge {
            FadeEdge::In => gpui::CursorStyle::ResizeLeft,
            FadeEdge::Out => gpui::CursorStyle::ResizeRight,
        })
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                let ratio = match edge {
                    FadeEdge::In => ((x - ox) / width).clamp(0.0, 1.0),
                    FadeEdge::Out => (1.0 - (x - ox) / width).clamp(0.0, 1.0),
                };
                let ms = ratio * clip_duration_seconds.max(0.001) * 500.0;
                let update = match edge {
                    FadeEdge::In => AudioClipProcessUpdate::FadeInMs(ms),
                    FadeEdge::Out => AudioClipProcessUpdate::FadeOutMs(ms),
                };
                on_preview(&(preview_id.clone(), update), window, cx);
            },
        )
        .on_mouse_up(gpui::MouseButton::Left, move |_, window, cx| {
            on_commit(&(commit_id.clone(), original.clone()), window, cx);
        })
        .on_mouse_up_out(gpui::MouseButton::Left, move |_, window, cx| {
            on_commit_out(&(commit_out_id.clone(), original_out.clone()), window, cx);
        })
        .child(
            div()
                .absolute()
                .top_0()
                .when(matches!(edge, FadeEdge::In), |this| {
                    this.left(px(fade_width.max(0.0) - 3.0))
                })
                .when(matches!(edge, FadeEdge::Out), |this| {
                    this.right(px(fade_width.max(0.0) - 3.0))
                })
                .w(px(6.0))
                .h(px(6.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::surface_input())
                .border(px(1.0))
                .border_color(Colors::accent_primary()),
        )
        .child(
            div()
                .absolute()
                .top(px(5.0))
                .when(matches!(edge, FadeEdge::In), |this| this.left_0())
                .when(matches!(edge, FadeEdge::Out), |this| this.right_0())
                .w(px(fade_width.max(0.0)))
                .h(px((body_height - 5.0).max(1.0)))
                .border_color(Colors::with_alpha(Colors::accent_primary(), 0.55))
                .when(matches!(edge, FadeEdge::In), |this| this.border_r(px(1.0)))
                .when(matches!(edge, FadeEdge::Out), |this| this.border_l(px(1.0))),
        )
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
    auto_crossfade_in_beats: f32,
    auto_crossfade_out_beats: f32,
    on_process_preview: AudioClipProcessPreviewCb,
    on_process_commit: AudioClipProcessCommitCb,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("AudioClip");
    let clip_id = clip.id.clone();
    let drag_clip_id = clip.id.clone();
    let drag_track_id = track_id.to_string();
    let drag_name = clip.name.clone();
    let drag_start_beat = clip.start_beat;
    let selected = state.selection.selected_clip_ids.contains(&clip.id);
    let _pixels_per_second = state.viewport.pixels_per_second;
    let seconds_per_beat = state.seconds_per_beat();
    let stretch_badge = stretch_badge(clip, state);
    let (left, width) = audio_clip_timeline_geometry(clip, state);
    let clip_duration_seconds = audio_clip_timeline_duration_seconds(clip, state);
    let fade_in_seconds = (clip.stretch.fade_in_ms.max(0.0) / 1000.0)
        .max(auto_crossfade_in_beats.max(0.0) * seconds_per_beat)
        .min(clip_duration_seconds);
    let fade_out_seconds = (clip.stretch.fade_out_ms.max(0.0) / 1000.0)
        .max(auto_crossfade_out_beats.max(0.0) * seconds_per_beat)
        .min((clip_duration_seconds - fade_in_seconds).max(0.0));
    // Pixels per second *of this clip*, not of the timeline. Under a tempo map
    // the clip's own seconds-to-pixels scale is its drawn width over the length
    // it plays for; using the global figure detaches the fade from the audio it
    // is fading exactly where the tempo moves.
    let clip_pixels_per_second = width / clip_duration_seconds.max(0.001);
    let fade_in_w = (fade_in_seconds * clip_pixels_per_second).min(width);
    let fade_out_w = (fade_out_seconds * clip_pixels_per_second).min((width - fade_in_w).max(0.0));
    let manual_fade_in_w =
        ((clip.stretch.fade_in_ms.max(0.0) / 1000.0) * clip_pixels_per_second).min(width * 0.5);
    let manual_fade_out_w =
        ((clip.stretch.fade_out_ms.max(0.0) / 1000.0) * clip_pixels_per_second).min(width * 0.5);
    let has_auto_crossfade = auto_crossfade_in_beats > 0.0 || auto_crossfade_out_beats > 0.0;
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
            >= if has_auto_crossfade || stretch_badge.is_some() {
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
    // With no strip the waveform owns the whole clip, and the fade overlays
    // reach the bottom edge instead of stopping above a bar that is not there.
    let strip_h = if show_strip { HEADER_H } else { 0.0 };
    let body_h = (clip_h - strip_h).max(1.0);
    let gain_preview = on_process_preview.clone();
    let gain_commit = on_process_commit.clone();
    let fade_in_preview = on_process_preview.clone();
    let fade_in_commit = on_process_commit.clone();
    let fade_out_preview = on_process_preview;
    let fade_out_commit = on_process_commit;

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
        // Smart Tool: the Pointer's lower half cuts. Before the strip, so the
        // strip's own controls (inline gain) stay on top of it.
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
                    show_inline_gain.then(|| compact_gain_control(clip, gain_preview, gain_commit)),
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
                .children((show_strip_detail && has_auto_crossfade).then(|| {
                    div()
                        .flex_none()
                        .px(px(4.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .bg(Colors::accent_soft())
                        .text_size(px(7.5))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::accent_primary())
                        .child("XFADE")
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
        .children((fade_in_w > 1.0).then(|| {
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom(px(strip_h))
                .w(px(fade_in_w))
                .bg(Colors::with_alpha(Colors::surface_panel_alt(), 0.34))
                .border_r(px(1.0))
                .border_color(if auto_crossfade_in_beats > 0.0 {
                    Colors::accent_primary()
                } else {
                    Colors::with_alpha(track_color, 0.55)
                })
        }))
        .children((fade_out_w > 1.0).then(|| {
            div()
                .absolute()
                .right_0()
                .top_0()
                .bottom(px(strip_h))
                .w(px(fade_out_w))
                .bg(Colors::with_alpha(Colors::surface_panel_alt(), 0.34))
                .border_l(px(1.0))
                .border_color(if auto_crossfade_out_beats > 0.0 {
                    Colors::accent_primary()
                } else {
                    Colors::with_alpha(track_color, 0.55)
                })
        }))
        // The fade handles stay on a crossfaded edge.
        //
        // They used to be hidden the moment an overlap produced an automatic
        // crossfade — which is exactly the edge somebody wants to reach for. The
        // length was then whatever the overlap happened to be and there was no
        // way to touch it at all, so the only way to change a crossfade was to
        // drag the clip itself and change the overlap. The manual fade already
        // wins when it is longer than the automatic one (see `fade_in_seconds`
        // above), so keeping the handle is what makes that reachable.
        .children(selected.then(|| {
            fade_drag_zone(
                clip,
                FadeEdge::In,
                clip_duration_seconds,
                manual_fade_in_w,
                body_h,
                fade_in_preview,
                fade_in_commit,
            )
        }))
        .children(selected.then(|| {
            fade_drag_zone(
                clip,
                FadeEdge::Out,
                clip_duration_seconds,
                manual_fade_out_w,
                body_h,
                fade_out_preview,
                fade_out_commit,
            )
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

        assert!(!splits(&plain, &plain, 1, 0.0, false), "a plain click selects");
        assert!(!splits(&cmd, &cmd, 1, 0.0, false), "Cmd-click is additive select");
        assert!(!splits(&shift, &shift, 1, 0.0, false));
        assert!(!splits(&cmd_alt, &cmd_alt, 1, 0.0, false));

        assert!(splits(&alt, &alt, 1, 0.0, false));
        assert!(splits(&alt, &alt, 1, 2.0, false), "within the drag threshold");
        assert!(splits(&alt_shift, &alt_shift, 1, 1.0, false), "Shift bypasses snap");

        assert!(!splits(&alt, &alt, 1, 2.5, false), "past the drag threshold");
        assert!(!splits(&alt, &alt, 2, 0.0, false), "part of a double-click");
        assert!(!splits(&alt, &alt, 1, 0.0, true), "a drag is in flight");
        assert!(!splits(&alt, &plain, 1, 0.0, false), "Option let go first");
        assert!(!splits(&plain, &alt, 1, 0.0, false), "Option pressed late");
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
