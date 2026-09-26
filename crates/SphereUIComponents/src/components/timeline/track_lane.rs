use crate::components::edit::{lane_press_intent, LanePressIntent};
use crate::components::timeline::audio_clip::{
    audio_clip, audio_clip_timeline_geometry, AudioClipProcessCommitCb, AudioClipProcessPreviewCb,
    CLIP_RESIZE_HANDLE_MIN_W, CLIP_STRIP_MIN_W,
};
use crate::components::timeline::crossfade_overlay::{
    crossfade_handle, crossfade_overlay, CrossfadeGestureCb, CrossfadeSide,
};
use crate::components::timeline::midi_clip::midi_clip;
use crate::components::timeline::timeline_state::{
    ClipTimeAxis, ClipType, TimelineGestureContext, TimelineState, TimelineTool, TrackState,
    TrackType,
};
use crate::components::timeline::video_clip::video_clip;
use crate::theme::Colors;
use gpui::prelude::FluentBuilder;
use gpui::{div, px, InteractiveElement, IntoElement, ParentElement, Styled};

/// A left press on arrangement lane space that no clip owns, which starts a
/// marquee. Positions are raw window pixels: the timeline anchors the
/// rectangle itself, unsnapped.
#[derive(Debug, Clone, PartialEq)]
pub struct MarqueePress {
    /// The track pressed — or, below the last track, the last one drawn. The
    /// marquee's anchor track.
    pub track_id: String,
    pub window_x: f32,
    pub window_y: f32,
    pub additive: bool,
    /// The press landed on `track_id`'s own lane, so a click without a drag
    /// chooses that track the way a lane click always has. `false` below the
    /// last track, where a click does nothing.
    pub on_lane: bool,
    /// A double-click on an empty MIDI / Instrument lane: create a
    /// default-length clip if it is released without becoming a drag.
    pub create_clip_on_click: bool,
    /// Shift was held: that clip starts off the grid, as with the Pen.
    pub bypass_snap: bool,
}

pub type MarqueePressCb =
    std::sync::Arc<dyn Fn(&MarqueePress, &mut gpui::Window, &mut gpui::App) + 'static>;

pub fn track_lane(
    track: &TrackState,
    track_index: usize,
    state: &TimelineState,
    gesture: &std::rc::Rc<TimelineGestureContext>,
    row_height: f32,
    on_select_track: std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>,
    on_select_clip: std::sync::Arc<
        dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_add_clip: std::sync::Arc<
        dyn Fn(&(String, f32, u32, bool), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_track_context_menu: Option<
        std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_clip_context_menu: Option<
        std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_open_editor: Option<std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>>,
    on_range_start: Option<MarqueePressCb>,
    _on_erase_start: Option<
        std::sync::Arc<dyn Fn(&f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_erase_clip: Option<
        std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_cut_clip: Option<crate::components::timeline::audio_clip::AudioClipCutCb>,
    erase_preview_ids: Option<&std::collections::HashSet<String>>,
    on_audio_clip_process_preview: AudioClipProcessPreviewCb,
    on_audio_clip_process_commit: AudioClipProcessCommitCb,
    on_crossfade: Option<CrossfadeGestureCb>,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("TrackLane");
    let track_id = track.id.clone();
    let is_track_selected = state.display_selection().selected_track_id.as_ref() == Some(&track.id);
    let even = track_index % 2 == 0;

    let bg = if is_track_selected {
        Colors::timeline_selected_lane_background()
    } else if even {
        Colors::timeline_lane_background()
    } else {
        Colors::timeline_lane_alt_background()
    };

    let on_select = on_select_track.clone();
    let track_id_select = track_id.clone();

    let on_add = on_add_clip.clone();
    let track_id_add = track_id.clone();

    // Where this track's clips are drawn (lane x, this frame's scroll), for the
    // create gestures' empty-lane test in the press handler. Captured here
    // because the handler cannot borrow the track.
    let clips_ref: std::rc::Rc<Vec<(f32, f32)>> = std::rc::Rc::new(
        track
            .clips
            .iter()
            .map(|clip| {
                let (left, width) = state.clip_lane_x_span(clip);
                (left, left + width)
            })
            .collect(),
    );

    let viewport_w = state.viewport.viewport_width.max(1.0);

    // This lane's crossfades, resolved once per render by the same code the
    // engine snapshot uses, so every clip's drawn fades are the ones it plays.
    let crossfades = state.audio_crossfades(track);
    // The ARA plug-in renders this track: clip fades, crossfades and clip gain
    // are drawn but disabled, because the engine does not apply them.
    let ara = track.ara.is_some();
    // Each clip's playing time on the beat axis, built once per render from
    // one resolved tempo map: its fade curves, fade handles and crossfade
    // curves are all placed through it, as the drags are resolved.
    let tempo = state.tempo_lookup();
    let time_axes: Vec<ClipTimeAxis<'_>> = track
        .clips
        .iter()
        .map(|clip| state.clip_time_axis_in(tempo.clone(), clip))
        .collect();

    // Map clips — skip lanes outside the horizontal viewport.
    let clip_elements: Vec<_> = track
        .clips
        .iter()
        .zip(&time_axes)
        .filter_map(|(clip, time)| {
            let (clip_left, clip_width) = if matches!(clip.clip_type, ClipType::Audio { .. }) {
                audio_clip_timeline_geometry(clip, state)
            } else {
                (
                    state.beats_to_x(clip.start_beat),
                    state
                        .beat_span_px(clip.start_beat, clip.duration_beats)
                        .max(10.0),
                )
            };
            if clip_left + clip_width < 0.0 || clip_left > viewport_w {
                return None;
            }

            let track_color = track.color;
            let on_sel_clip = on_select_clip.clone();
            let on_clip_context = on_clip_context_menu.clone();
            let on_open = on_open_editor.clone();
            let on_del = on_erase_clip.clone();
            let on_cut = on_cut_clip.clone();
            let erase_target = erase_preview_ids
                .map(|s| s.contains(&clip.id))
                .unwrap_or(false);
            let on_process_preview = on_audio_clip_process_preview.clone();
            let on_process_commit = on_audio_clip_process_commit.clone();
            Some(match clip.clip_type {
                ClipType::Audio { .. } => audio_clip(
                    clip,
                    &track.id,
                    track_color,
                    state,
                    row_height,
                    on_sel_clip,
                    on_open,
                    on_clip_context,
                    on_del,
                    on_cut,
                    erase_target,
                    state.effective_clip_fades(clip, &crossfades),
                    time,
                    ara,
                    on_process_preview,
                    on_process_commit,
                )
                .into_any_element(),
                ClipType::Midi { .. } => midi_clip(
                    clip,
                    &track.id,
                    track_color,
                    state,
                    row_height,
                    on_sel_clip,
                    on_clip_context,
                    on_open,
                    on_del,
                    erase_target,
                )
                .into_any_element(),
                ClipType::Video { .. } => video_clip(
                    clip,
                    &track.id,
                    track_color,
                    state,
                    row_height,
                    on_sel_clip,
                    on_clip_context,
                    on_open,
                    on_del,
                    erase_target,
                )
                .into_any_element(),
            })
        })
        .collect();

    if crate::perf::enabled() {
        crate::perf::count("rendered_clips", clip_elements.len() as u64);
        crate::perf::count("total_clips", track.clips.len() as u64);
    }

    // One overlay per crossfade, above both clips. It has a handle under the
    // Pointer, on clips wide enough for their own edge handles. The handle is
    // live only where the planner can resize the crossfade: both clips can
    // be re-trimmed, and there is room for one of the minimum length.
    let pointer = state.active_tool == TimelineTool::Pointer;
    let crossfade_elements: Vec<_> = crossfades
        .crossfades
        .iter()
        .filter_map(|crossfade| {
            let index_of = |id: &str| track.clips.iter().position(|clip| clip.id == id);
            let (left_index, right_index) = (
                index_of(&crossfade.left_id)?,
                index_of(&crossfade.right_id)?,
            );
            let (left, right) = (&track.clips[left_index], &track.clips[right_index]);
            let narrowest = state
                .clip_lane_x_span(left)
                .1
                .min(state.clip_lane_x_span(right).1);
            let shown = pointer && narrowest >= CLIP_RESIZE_HANDLE_MIN_W;
            // The planner's own feasibility test, asked only for a handle that
            // is drawn and not already disabled by ARA.
            let block = (shown && !ara)
                .then(|| state.crossfade_adjust_block(left, right))
                .flatten();
            let handle = crossfade_handle(shown, ara, block);
            crossfade_overlay(
                crossfade,
                state,
                &CrossfadeSide {
                    time: &time_axes[left_index],
                    fades: state.effective_clip_fades(left, &crossfades),
                },
                &CrossfadeSide {
                    time: &time_axes[right_index],
                    fades: state.effective_clip_fades(right, &crossfades),
                },
                row_height,
                narrowest >= CLIP_STRIP_MIN_W,
                handle,
                ara,
                on_crossfade.clone(),
            )
        })
        .collect();

    let active_tool = state.active_tool;
    let track_type = track.track_type;
    let midi_lane = matches!(track_type, TrackType::Midi | TrackType::Instrument);
    // Pen and Cut change what a press does, so they say so.
    let lane_cursor = if matches!(active_tool, TimelineTool::Pen | TimelineTool::Cut) {
        gpui::CursorStyle::Crosshair
    } else {
        gpui::CursorStyle::Arrow
    };
    // Gesture closures must own their coordinate inputs. Cloning the whole
    // `TimelineState` here deep-copied every clip and MIDI note in the project
    // once per visible row per frame; the shared per-frame context carries only
    // the viewport transform and snap grid.
    let state_ref = std::rc::Rc::clone(gesture);
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        track.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    div()
        .flex_1()
        .h(px(row_height))
        .bg(bg)
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .relative()
        .overflow_hidden()
        .cursor(lane_cursor)
        .id(("track-lane", id_num))
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                let x: f32 = event.position.x.into();
                let lane_x = state_ref.lane_x_from_window_x(x);
                let click_count = event.click_count as u32;

                // The create gestures describe themselves as empty-lane
                // gestures, so the lane checks for itself rather than relying
                // on a clip element above it stopping the press — pressing a
                // clip that was already selected once created a new clip beside
                // it. Tested against where the clips are drawn, so the minimum
                // drawn width counts and the pad bands above and below a clip
                // do not make a clip there.
                let over_existing_clip = clips_ref
                    .iter()
                    .any(|(left, right)| lane_x >= *left && lane_x <= *right);

                match lane_press_intent(
                    active_tool,
                    Some(track_type),
                    event.click_count,
                    &event.modifiers,
                ) {
                    LanePressIntent::Marquee {
                        additive,
                        create_clip_on_click,
                    } => match on_range_start.as_ref() {
                        Some(start_marquee) => start_marquee(
                            &MarqueePress {
                                track_id: track_id_select.clone(),
                                window_x: x,
                                window_y: event.position.y.into(),
                                additive,
                                on_lane: true,
                                create_clip_on_click: create_clip_on_click
                                    && midi_lane
                                    && !over_existing_clip,
                                bypass_snap: event.modifiers.shift,
                            },
                            window,
                            cx,
                        ),
                        None if !additive => on_select(&track_id_select, window, cx),
                        None => {}
                    },
                    LanePressIntent::Pen => {
                        if over_existing_clip {
                            // Still a press on this track: pressing a lane is
                            // how a track is chosen.
                            on_select(&track_id_select, window, cx);
                            return;
                        }
                        let bypass_snap = event.modifiers.shift;
                        let snapped_beat = state_ref
                            .snap_beats_with_bypass(state_ref.x_to_beats(lane_x), bypass_snap);
                        on_add(
                            &(track_id_add.clone(), snapped_beat, click_count, bypass_snap),
                            window,
                            cx,
                        );
                    }
                    LanePressIntent::SelectTrack | LanePressIntent::Ignore => {
                        on_select(&track_id_select, window, cx);
                    }
                }
            },
        )
        .when_some(on_track_context_menu, |this, open_menu| {
            let context_track_id = track_id.clone();
            this.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    open_menu(&(context_track_id.clone(), x, y), window, cx);
                },
            )
        })
        // Clips always render at full strength — automation now lives in its
        // own sub-lanes below the track, so the clip area stays clean.
        .child(div().absolute().inset_0().children(clip_elements))
        .children(
            (!crossfade_elements.is_empty())
                .then(|| div().absolute().inset_0().children(crossfade_elements)),
        )
}
