//! One automatic crossfade on an audio lane: the overlap of two clips, drawn
//! as the pair of equal-power curves the engine plays across it, with a handle
//! at its top centre.
//!
//! Dragging the handle lengthens (right) or shortens (left) the crossfade
//! about its centre by trimming both clips; the timeline plans it in seconds
//! (`TimelineState::plan_crossfade`), previews it live and records the whole
//! gesture as one undo step when the button comes up. A double-click resets it
//! to the default length.
//!
//! The overlay is painted after the lane's clips, so the handle is above both
//! of them wherever they overlap. Only the handle takes presses: the rest of
//! the overlap still selects, moves and trims the clips under it.
//!
//! The curves are each clip's *effective* fade — what the engine plays — so
//! when a clip's two crossfades together are longer than the clip, the one
//! the engine shortens is drawn shortened too. The handle is live only where
//! the planner can resize the crossfade; elsewhere it is drawn disabled and
//! says why ([`crossfade_handle`]).

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, AppContext, DragMoveEvent, Empty, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window,
};

use crate::components::timeline::audio_clip::{
    fade_edge_curve, fade_handle_mark, paint_fade_curves, FadeCurvePoints,
    ARA_CLIP_PROCESSING_TOOLTIP, CLIP_BORDER, FADE_HANDLE_HIT, FADE_HANDLE_SIZE,
};
use crate::components::timeline::timeline_state::{
    AudioCrossfade, ClipTimeAxis, CrossfadeBlock, EffectiveFades, FadeEdge, TimelineState,
    CLIP_LANE_PAD,
};
use crate::theme::Colors;

/// One clip of a crossfade: its time axis and the fades it plays.
pub struct CrossfadeSide<'a> {
    pub time: &'a ClipTimeAxis<'a>,
    pub fades: EffectiveFades,
}

/// What a crossfade's handle does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossfadeHandle {
    /// No handle: another tool, or clips too narrow for handles.
    Hidden,
    /// Drag to resize, double-click for the default length.
    Live,
    /// Drawn muted and inert; the tooltip says why.
    Disabled(&'static str),
}

/// The handle for a crossfade whose clips are wide enough for one under the
/// Pointer (`shown`). On an ARA track nothing it did would play. On a pair
/// [`TimelineState::plan_crossfade`] cannot resize, a drag would do nothing.
/// `block` is [`TimelineState::crossfade_adjust_block`]: Warp, reversed,
/// undecoded, a legacy offset trim, or no room for the shortest crossfade.
/// Both cases are drawn disabled with the reason, so no handle is live that
/// the planner would refuse.
pub fn crossfade_handle(shown: bool, ara: bool, block: Option<CrossfadeBlock>) -> CrossfadeHandle {
    if !shown {
        CrossfadeHandle::Hidden
    } else if ara {
        CrossfadeHandle::Disabled(ARA_CLIP_PROCESSING_TOOLTIP)
    } else if let Some(block) = block {
        CrossfadeHandle::Disabled(block.handle_tooltip())
    } else {
        CrossfadeHandle::Live
    }
}

/// The two curves of `crossfade`, `(x - origin_x, gain)`: the left clip's
/// fade-out and the right clip's fade-in as each plays them, within the
/// overlap. Each is stepped through its own clip's time axis, so a tempo ramp
/// bends it where it bends the audio, and it is the effective fade — a fade
/// the engine clamps short starts late here too.
pub(crate) fn crossfade_curves(
    crossfade: &AudioCrossfade,
    left: &CrossfadeSide<'_>,
    right: &CrossfadeSide<'_>,
    origin_x: f32,
) -> Vec<FadeCurvePoints> {
    let overlap = |time: &ClipTimeAxis<'_>| {
        (
            time.local_seconds_at_beat(crossfade.start_beat),
            time.local_seconds_at_beat(crossfade.end_beat),
        )
    };
    [
        fade_edge_curve(
            left.time,
            &left.fades,
            FadeEdge::Out,
            origin_x,
            Some(overlap(left.time)),
        ),
        fade_edge_curve(
            right.time,
            &right.fades,
            FadeEdge::In,
            origin_x,
            Some(overlap(right.time)),
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// A gesture on a crossfade handle, resolved by the timeline.
#[derive(Debug, Clone, PartialEq)]
pub enum CrossfadeGesture {
    /// The handle was pressed at `window_x`.
    Press {
        left_id: String,
        right_id: String,
        window_x: f32,
    },
    /// The pressed handle moved to `window_x`.
    Drag {
        left_id: String,
        right_id: String,
        window_x: f32,
    },
    /// A double-click: back to the default length.
    Reset { left_id: String, right_id: String },
}

pub type CrossfadeGestureCb =
    std::sync::Arc<dyn Fn(&CrossfadeGesture, &mut gpui::Window, &mut gpui::App) + 'static>;

/// The drag payload: which crossfade, so every other handle ignores it.
#[derive(Clone, Debug)]
struct CrossfadeHandleDrag {
    key: String,
}

impl Render for CrossfadeHandleDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Height of an audio clip's name strip, which the curves stop above.
const CLIP_STRIP_H: f32 = 20.0;

/// The overlay for `crossfade` on a lane `row_height` tall, or `None` when it
/// is out of view. `strip` says whether the clips draw their name strip (the
/// curves stop above it); `handle` what its handle does. On an ARA track the
/// curves are drawn muted too, since the engine plays none of it.
#[allow(clippy::too_many_arguments)]
pub fn crossfade_overlay(
    crossfade: &AudioCrossfade,
    state: &TimelineState,
    left: &CrossfadeSide<'_>,
    right: &CrossfadeSide<'_>,
    row_height: f32,
    strip: bool,
    handle: CrossfadeHandle,
    ara: bool,
    on_gesture: Option<CrossfadeGestureCb>,
) -> Option<gpui::AnyElement> {
    let x0 = state.beats_to_x(crossfade.start_beat as f32);
    let x1 = state.beats_to_x(crossfade.end_beat as f32);
    let viewport_w = state.viewport.viewport_width.max(1.0);
    if x1 < -FADE_HANDLE_HIT || x0 > viewport_w + FADE_HANDLE_HIT {
        return None;
    }
    let width = (x1 - x0).max(0.0);
    let top = CLIP_LANE_PAD + CLIP_BORDER;
    let height = (row_height - 2.0 * (CLIP_LANE_PAD + CLIP_BORDER)).max(0.0);
    let curve_height = if strip && height > CLIP_STRIP_H * 2.0 {
        height - CLIP_STRIP_H
    } else {
        height
    };

    let curves = crossfade_curves(crossfade, left, right, x0);
    let line = if ara {
        Colors::with_alpha(
            Colors::text_disabled(),
            crate::theme::state::DISABLED_CONTENT,
        )
    } else {
        Colors::accent_primary()
    };

    let key = format!("{}>{}", crossfade.left_id, crossfade.right_id);
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish() as usize
    };
    let handle_element = (handle != CrossfadeHandle::Hidden).then(|| {
        let disabled = match handle {
            CrossfadeHandle::Disabled(reason) => Some(reason),
            _ => None,
        };
        let element = div()
            .id(("audio-crossfade-handle", id_num))
            .absolute()
            .left(px(width * 0.5 - FADE_HANDLE_HIT * 0.5))
            .top_0()
            .w(px(FADE_HANDLE_HIT))
            .h(px(FADE_HANDLE_HIT.min(height)))
            .child(
                fade_handle_mark(FADE_HANDLE_SIZE, false, disabled.is_some())
                    .absolute()
                    .left(px((FADE_HANDLE_HIT - FADE_HANDLE_SIZE) * 0.5))
                    .top(px((FADE_HANDLE_HIT - FADE_HANDLE_SIZE) * 0.5)),
            );
        match (disabled, on_gesture) {
            (Some(reason), _) => element
                .tooltip(crate::components::fb_tooltip(reason))
                .into_any_element(),
            (None, None) => element.into_any_element(),
            (None, Some(on_gesture)) => {
                let (left_id, right_id) = (crossfade.left_id.clone(), crossfade.right_id.clone());
                let (drag_left, drag_right) = (left_id.clone(), right_id.clone());
                let move_key = key.clone();
                let on_drag = on_gesture.clone();
                element
                    .cursor(gpui::CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |event: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let gesture = if event.click_count >= 2 {
                                CrossfadeGesture::Reset {
                                    left_id: left_id.clone(),
                                    right_id: right_id.clone(),
                                }
                            } else {
                                CrossfadeGesture::Press {
                                    left_id: left_id.clone(),
                                    right_id: right_id.clone(),
                                    window_x: event.position.x.into(),
                                }
                            };
                            on_gesture(&gesture, window, cx);
                        },
                    )
                    .on_drag(CrossfadeHandleDrag { key }, |drag, _offset, _window, cx| {
                        cx.new(|_| drag.clone())
                    })
                    .on_drag_move::<CrossfadeHandleDrag>(
                        move |event: &DragMoveEvent<CrossfadeHandleDrag>, window, cx| {
                            if event.drag(cx).key != move_key {
                                return;
                            }
                            on_drag(
                                &CrossfadeGesture::Drag {
                                    left_id: drag_left.clone(),
                                    right_id: drag_right.clone(),
                                    window_x: event.event.position.x.into(),
                                },
                                window,
                                cx,
                            );
                        },
                    )
                    .into_any_element()
            }
        }
    });

    Some(
        div()
            .absolute()
            .left(px(x0))
            .top(px(top))
            .w(px(width))
            .h(px(height))
            .when(!curves.is_empty(), move |this| {
                this.child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            let top: f32 = bounds.origin.y.into();
                            paint_fade_curves(
                                &curves,
                                bounds.origin.x.into(),
                                top,
                                top + f32::from(bounds.size.height),
                                // The curves cross, so only their lines are
                                // drawn: a shade would cover both clips.
                                None,
                                line,
                                window,
                            );
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(width))
                    .h(px(curve_height)),
                )
            })
            .children(handle_element)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::{crossfade_curves, crossfade_handle, CrossfadeHandle, CrossfadeSide};
    use crate::components::timeline::audio_clip::ARA_CLIP_PROCESSING_TOOLTIP;
    use crate::components::timeline::timeline_state::{
        crossfade_pair_block, AudioClipStretchState, AudioImportState, ClipState, ClipType,
        CrossfadeBlock, TimelineState, DEFAULT_CROSSFADE_SECONDS, MIN_CROSSFADE_SECONDS,
    };

    /// A decoded audio clip `seconds` long at `start_beat`, its whole file.
    fn audio_clip(id: &str, start_beat: f32, seconds: f64) -> ClipState {
        let mut clip = ClipState {
            id: id.to_string(),
            name: id.to_string(),
            start_beat,
            duration_beats: (seconds * 2.0) as f32,
            source_duration_seconds: Some(seconds),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: id.to_string(),
                source_path: Some(format!("{id}.wav")),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        let frames = (seconds * 48_000.0).round() as u64;
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = frames;
        clip.stretch.source_end_samples = frames;
        clip
    }

    /// 120 BPM, two beats a second, 100 px a second.
    fn state_with(clips: Vec<ClipState>) -> TimelineState {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.tracks.clear();
        let track_id = state.create_audio_track();
        let track = state.tracks.iter_mut().find(|t| t.id == track_id).unwrap();
        track.clips = clips;
        state.reconcile_audio_clip_lengths();
        state.viewport.pixels_per_second = 100.0;
        state.sync_pixels_per_beat();
        state
    }

    /// The handle is live only where the planner can re-trim both clips; an
    /// ARA track or a pair it refuses gets a disabled handle that says why,
    /// and a pair it refuses is exactly a pair with a block.
    #[test]
    fn a_crossfade_handle_is_live_only_where_the_planner_can_act() {
        assert_eq!(
            crossfade_handle(false, false, None),
            CrossfadeHandle::Hidden
        );
        assert_eq!(
            crossfade_handle(false, true, Some(CrossfadeBlock::Warp)),
            CrossfadeHandle::Hidden
        );
        assert_eq!(crossfade_handle(true, false, None), CrossfadeHandle::Live);
        assert_eq!(
            crossfade_handle(true, true, None),
            CrossfadeHandle::Disabled(ARA_CLIP_PROCESSING_TOOLTIP)
        );
        for block in [
            CrossfadeBlock::NotAudio,
            CrossfadeBlock::Warp,
            CrossfadeBlock::Reversed,
            CrossfadeBlock::Undecoded,
            CrossfadeBlock::LegacyOffset,
            CrossfadeBlock::NoRoom,
        ] {
            assert_eq!(
                crossfade_handle(true, false, Some(block)),
                CrossfadeHandle::Disabled(block.handle_tooltip())
            );
        }

        // Two overlapping takes: live, and the planner resizes them.
        let state = state_with(vec![audio_clip("a", 0.0, 3.0), audio_clip("b", 4.0, 3.0)]);
        let (a, b) = (&state.tracks[0].clips[0], &state.tracks[0].clips[1]);
        assert_eq!(state.audio_crossfades(&state.tracks[0]).crossfades.len(), 1);
        let center = state.crossfade_center_seconds(a, b);
        assert!(state.plan_crossfade(a, b, center, 0.2).is_some());
        let block = crossfade_pair_block(a, b);
        assert_eq!(crossfade_handle(true, false, block), CrossfadeHandle::Live);

        // Reversed: still crossfades, but the handle is disabled, because
        // every plan is refused.
        let mut reversed = b.clone();
        reversed.stretch.reverse = true;
        assert!(state.plan_crossfade(a, &reversed, center, 0.2).is_none());
        let block = crossfade_pair_block(a, &reversed);
        assert_eq!(
            crossfade_handle(true, false, block),
            CrossfadeHandle::Disabled(CrossfadeBlock::Reversed.handle_tooltip())
        );
    }

    /// A pair nothing blocks from being trimmed can still have no room for
    /// the shortest crossfade. Then the planner refuses every length, and
    /// the handle is disabled and says so. `audio_clip` is a whole file, so
    /// no edge reaches past where it is.
    #[test]
    fn a_crossfade_with_no_room_to_resize_has_a_disabled_handle() {
        let lengths = [MIN_CROSSFADE_SECONDS, DEFAULT_CROSSFADE_SECONDS, 0.2];
        // `b` starts 0.7 ms (0.0014 beats) before `a` ends: a crossfade
        // plays, and at most 0.7 ms of one fits. `c` is 0.1 s long and
        // starts 50 ms before `a` ends: too short to keep its minimum body
        // outside any crossfade.
        for (start, seconds) in [(6.0 - 0.0014, 3.0), (5.9, 0.1)] {
            let state = state_with(vec![
                audio_clip("a", 0.0, 3.0),
                audio_clip("b", start, seconds),
            ]);
            let track = &state.tracks[0];
            assert_eq!(state.audio_crossfades(track).crossfades.len(), 1);
            let (a, b) = (&track.clips[0], &track.clips[1]);
            assert_eq!(crossfade_pair_block(a, b), None);
            let center = state.crossfade_center_seconds(a, b);
            for length in lengths {
                assert!(state.plan_crossfade(a, b, center, length).is_none());
            }
            assert!(!state.can_adjust_crossfade(a, b));
            let block = state.crossfade_adjust_block(a, b);
            assert_eq!(block, Some(CrossfadeBlock::NoRoom));
            assert_eq!(
                crossfade_handle(true, false, block),
                CrossfadeHandle::Disabled(CrossfadeBlock::NoRoom.handle_tooltip())
            );
        }

        // Across the limit the handle is live exactly where every length
        // plans. With no audio beyond the edges the room is the overlap,
        // swept here from 0.6 ms to 1.4 ms.
        let mut live = 0;
        for tenths in 6..=14 {
            let overlap_beats = tenths as f32 * 0.0002;
            let state = state_with(vec![
                audio_clip("a", 0.0, 3.0),
                audio_clip("b", 6.0 - overlap_beats, 3.0),
            ]);
            let (a, b) = (&state.tracks[0].clips[0], &state.tracks[0].clips[1]);
            let center = state.crossfade_center_seconds(a, b);
            let adjustable = state.can_adjust_crossfade(a, b);
            for length in lengths {
                assert_eq!(
                    state.plan_crossfade(a, b, center, length).is_some(),
                    adjustable,
                    "{tenths} tenths of a ms, {length} s"
                );
            }
            let handle = crossfade_handle(true, false, state.crossfade_adjust_block(a, b));
            assert_eq!(handle == CrossfadeHandle::Live, adjustable);
            live += usize::from(adjustable);
        }
        assert!(live > 0 && live < 9, "the sweep crosses the limit: {live}");
    }

    /// The review's case at 120 BPM: A[0,6) B[2,8) C[4,10). B plays 3 s with
    /// 2 s of crossfade on each edge, so the engine shortens its fade-out to
    /// the 1 s left, beats 6..8. The B→C overlay spans the whole overlap
    /// (4..8) but draws B's curve only where B fades, while C fades in over
    /// all of it. The A→C overlay draws the tail of A's 2 s fade-out.
    #[test]
    fn the_crossfade_overlay_draws_the_fades_the_engine_plays() {
        let state = state_with(vec![
            audio_clip("a", 0.0, 3.0),
            audio_clip("b", 2.0, 3.0),
            audio_clip("c", 4.0, 3.0),
        ]);
        let track = &state.tracks[0];
        let crossfades = state.audio_crossfades(track);
        let side = |index: usize| {
            let clip = &track.clips[index];
            (
                state.clip_time_axis(clip),
                state.effective_clip_fades(clip, &crossfades),
            )
        };
        let (a_time, a_fades) = side(0);
        let (b_time, b_fades) = side(1);
        let (c_time, c_fades) = side(2);
        assert!((b_fades.out_seconds - 1.0).abs() < 1.0e-6, "clamped");
        let x = |beat: f64| state.beats_to_x(beat as f32);
        let close = |a: f32, b: f32| (a - b).abs() < 0.01;

        let b_to_c = crossfades
            .crossfades
            .iter()
            .find(|xf| xf.left_id == "b" && xf.right_id == "c")
            .unwrap();
        let origin = x(b_to_c.start_beat);
        let curves = crossfade_curves(
            b_to_c,
            &CrossfadeSide {
                time: &b_time,
                fades: b_fades,
            },
            &CrossfadeSide {
                time: &c_time,
                fades: c_fades,
            },
            origin,
        );
        assert_eq!(curves.len(), 2);
        let (b_first, b_last) = (curves[0].points[0], *curves[0].points.last().unwrap());
        assert!(close(b_first.0, x(6.0) - origin), "B fades from beat 6");
        assert!(close(b_last.0, x(8.0) - origin));
        assert!((b_first.1 - 1.0).abs() < 1.0e-6 && b_last.1.abs() < 1.0e-6);
        let (c_first, c_last) = (curves[1].points[0], *curves[1].points.last().unwrap());
        assert!(close(c_first.0, 0.0) && close(c_last.0, x(8.0) - origin));
        assert!(c_first.1.abs() < 1.0e-6 && (c_last.1 - 1.0).abs() < 1.0e-6);

        // A→C covers beats 4..6, the second half of A's fade-out (2..6).
        let a_to_c = crossfades
            .crossfades
            .iter()
            .find(|xf| xf.left_id == "a" && xf.right_id == "c")
            .unwrap();
        let origin = x(a_to_c.start_beat);
        let curves = crossfade_curves(
            a_to_c,
            &CrossfadeSide {
                time: &a_time,
                fades: a_fades,
            },
            &CrossfadeSide {
                time: &c_time,
                fades: c_fades,
            },
            origin,
        );
        let a_curve = &curves[0].points;
        assert!(close(a_curve[0].0, 0.0));
        assert!(close(a_curve.last().unwrap().0, x(6.0) - origin));
        let half = std::f32::consts::FRAC_1_SQRT_2;
        assert!((a_curve[0].1 - half).abs() < 1.0e-3, "halfway down already");
        // C's fade-in is drawn only over this overlap, 4..6.
        assert!(close(curves[1].points.last().unwrap().0, x(6.0) - origin));
    }
}
