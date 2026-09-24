//! What one arrangement frame costs, at a scale a real session reaches.
//!
//! The arrangement rebuilds its lanes whenever `Timeline` notifies — which is
//! every scroll, every zoom, every selection and every edit. Vertical rows are
//! virtualized and off-screen clips are culled, so the question that decides
//! whether a big session is usable is not "how many tracks" but **what a single
//! visible clip costs**, and whether that cost is bounded by the pixels it
//! occupies or by the data behind it.
//!
//! These measure the data work of one frame — the geometry every lane derives
//! before a single quad is painted. They are deliberately not GPUI benchmarks:
//! element construction and paint are the other half, but they cannot be the
//! half that grows with note count, and this is where that growth would be.
//!
//! Run with:
//!
//! ```text
//! cargo test -p sphere_ui_components --lib arrangement_bench -- --nocapture --ignored
//! ```
//!
//! Most are `#[ignore]`d because they are measurements, not assertions. The
//! exceptions assert a *shape* — that a cost does not grow with something it
//! must not grow with — which holds on any machine.

use std::time::Instant;

use crate::components::timeline::audio_clip::audio_clip_timeline_geometry;
use crate::components::timeline::render::clip_geometry::{
    controller_preview_cached, note_preview_cached, note_previews_built, visible_clip_px_range,
    ClipAxis,
};
use crate::components::timeline::timeline_state::{
    tempo_maps_built, AudioClipStretchState, AudioImportState, ClipState, ClipType,
    CreateTrackOptions, InputMonitorMode, MidiControllerKind, MidiControllerLane,
    MidiControllerPoint, MidiNoteState, TempoCurve, TimelineState, TrackRowLayout, TrackType,
};

/// Viewport the numbers are quoted against: a normal editing window.
const VIEWPORT_W: f32 = 1600.0;
const VIEWPORT_H: f32 = 900.0;

fn dense_notes(count: usize, clip_len: f32) -> Vec<MidiNoteState> {
    let scale = [0_i32, 2, 3, 5, 7, 8, 10];
    (0..count)
        .map(|index| {
            let beat = index as f32 / count as f32 * clip_len;
            MidiNoteState::new(
                (36 + scale[index % scale.len()] + 12 * ((index / 37) as i32 % 5)) as u8,
                beat,
                clip_len / count as f32 * 1.5,
                80 + (index % 40) as u8,
            )
        })
        .collect()
}

fn controller_lane(points: usize, clip_len: f32) -> MidiControllerLane {
    MidiControllerLane {
        kind: MidiControllerKind::CC(11),
        points: (0..points)
            .map(|index| {
                let t = index as f32 / points.max(1) as f32;
                MidiControllerPoint {
                    id: index as u64 + 1,
                    beat: t * clip_len,
                    value: 0.5 + 0.5 * (t * std::f32::consts::TAU * 3.0).sin(),
                }
            })
            .collect(),
        visible: true,
        height: 48.0,
        collapsed: false,
    }
}

fn midi_clip_state(id: usize, start: f32, len: f32, notes: usize, cc_points: usize) -> ClipState {
    ClipState {
        id: format!("clip-midi-{id}"),
        name: format!("MIDI {id}"),
        start_beat: start,
        duration_beats: len,
        source_duration_seconds: None,
        offset_beats: 0.0,
        gain: 1.0,
        clip_type: ClipType::Midi {
            notes: dense_notes(notes, len),
            controller_lanes: if cc_points > 0 {
                vec![controller_lane(cc_points, len)]
            } else {
                Vec::new()
            },
            sysex_events: Vec::new(),
            articulations: Vec::new(),
        },
        muted: false,
        audio_import: AudioImportState::Ready,
        stretch: AudioClipStretchState::default(),
    }
}

fn audio_clip_state(id: usize, start: f32, len_seconds: f64) -> ClipState {
    let mut clip = ClipState {
        id: format!("clip-audio-{id}"),
        name: format!("Audio {id}"),
        start_beat: start,
        duration_beats: (len_seconds * 2.0) as f32,
        source_duration_seconds: Some(len_seconds),
        offset_beats: 0.0,
        gain: 1.0,
        clip_type: ClipType::Audio {
            file_id: format!("asset-{id}"),
            source_path: Some(format!("take-{id}.wav")),
        },
        muted: false,
        audio_import: AudioImportState::Ready,
        stretch: AudioClipStretchState::default(),
    };
    let frames = (len_seconds * 48_000.0) as u64;
    clip.stretch.original_sample_rate = 48_000;
    clip.stretch.project_sample_rate = 48_000;
    clip.stretch.original_duration_samples = frames;
    clip.stretch.source_start_samples = 0;
    clip.stretch.source_end_samples = frames;
    clip
}

/// A session shaped like real work: MIDI tracks carrying dense imported parts,
/// audio tracks carrying comped takes, all of it inside the visible window so
/// nothing is culled and the numbers describe the worst honest case.
struct Session {
    state: TimelineState,
}

impl Session {
    fn build(
        midi_tracks: usize,
        clips_per_track: usize,
        notes_per_clip: usize,
        cc_points: usize,
        audio_tracks: usize,
        audio_clips_per_track: usize,
    ) -> Self {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.viewport.viewport_width = VIEWPORT_W;
        state.viewport.viewport_height = VIEWPORT_H;
        state.viewport.pixels_per_second = 40.0;
        state.sync_pixels_per_beat();

        let clip_len = 8.0_f32;
        let mut clip_id = 0usize;
        for track_index in 0..midi_tracks {
            let track_id = state.create_track(CreateTrackOptions {
                track_type: TrackType::Midi,
                name: format!("MIDI {track_index}"),
                color: gpui::Rgba {
                    r: 0.3,
                    g: 0.6,
                    b: 0.9,
                    a: 1.0,
                },
                volume: 0.8,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            let track = state
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
                .expect("track");
            for slot in 0..clips_per_track {
                track.clips.push(midi_clip_state(
                    clip_id,
                    slot as f32 * clip_len,
                    clip_len,
                    notes_per_clip,
                    cc_points,
                ));
                clip_id += 1;
            }
        }
        for track_index in 0..audio_tracks {
            let track_id = state.create_track(CreateTrackOptions {
                track_type: TrackType::Audio,
                name: format!("Audio {track_index}"),
                color: gpui::Rgba {
                    r: 0.9,
                    g: 0.5,
                    b: 0.3,
                    a: 1.0,
                },
                volume: 0.8,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            let track = state
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
                .expect("track");
            for slot in 0..audio_clips_per_track {
                track
                    .clips
                    .push(audio_clip_state(clip_id, slot as f32 * 8.0, 4.0));
                clip_id += 1;
            }
        }
        Self { state }
    }

    fn total_notes(&self) -> usize {
        self.state
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .map(|clip| match &clip.clip_type {
                ClipType::Midi { notes, .. } => notes.len(),
                _ => 0,
            })
            .sum()
    }

    /// The data work one arrangement repaint does for every visible lane.
    ///
    /// Mirrors `track_lane`: horizontal cull, then per surviving clip the same
    /// geometry the element build resolves. Vertical virtualization is applied
    /// exactly as `track_list` applies it.
    fn frame_geometry(&self) -> FrameWork {
        let state = &self.state;
        let layout = TrackRowLayout::build(state);
        let (visible_start, visible_end, _, _) =
            crate::components::timeline::track_resize::visible_track_row_range(
                &layout,
                state.viewport.scroll_y,
                state.viewport.viewport_height,
                2,
            );
        let mut work = FrameWork::default();
        let viewport_w = state.viewport.viewport_width.max(1.0);
        let ppb = state.viewport.pixels_per_beat;

        for track in &state.tracks[visible_start..visible_end] {
            work.lanes += 1;
            for clip in &track.clips {
                let (left, width) = match clip.clip_type {
                    ClipType::Audio { .. } => audio_clip_timeline_geometry(clip, state),
                    _ => (
                        state.beats_to_x(clip.start_beat),
                        (clip.duration_beats * ppb).max(10.0),
                    ),
                };
                if left + width < 0.0 || left > viewport_w {
                    continue;
                }
                work.clips += 1;
                if let ClipType::Midi {
                    notes,
                    controller_lanes,
                    ..
                } = &clip.clip_type
                {
                    if let Some((px_start, px_end)) = visible_clip_px_range(left, width, viewport_w)
                    {
                        if let Some(preview) = note_preview_cached(
                            &clip.id,
                            notes,
                            clip.duration_beats,
                            &ClipAxis::linear(ppb),
                            px_start,
                            px_end,
                        ) {
                            work.painted_note_quads += preview.columns.len() + preview.quads.len();
                        }
                    }
                    if let Some(preview) = controller_preview_cached(
                        &clip.id,
                        controller_lanes,
                        clip.duration_beats,
                        &ClipAxis::linear(ppb),
                        width,
                    ) {
                        work.painted_controller_columns +=
                            preview.columns * preview.lane_kinds.len();
                    }
                }
            }
        }
        work
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct FrameWork {
    lanes: usize,
    clips: usize,
    painted_note_quads: usize,
    painted_controller_columns: usize,
}

/// Frames of a *scroll*, which is the gesture whose cost the user feels and the
/// one a naive geometry cache would miss on every frame.
///
/// Measuring a still arrangement would flatter any cache: nothing changes, so
/// everything hits. The arrangement is scrolled by a realistic amount per frame
/// instead, so the numbers describe the case that actually has to hold up.
fn time_scroll_frames(session: &mut Session, frames: usize) -> (FrameWork, f64) {
    // One warm pass so first-touch page faults are not in the number.
    let work = session.frame_geometry();
    let origin = session.state.viewport.scroll_x;
    let started = Instant::now();
    for frame in 0..frames {
        // ~8 px a frame: an unhurried drag at 60 Hz.
        session.state.viewport.scroll_x = origin + frame as f32 * 8.0;
        std::hint::black_box(session.frame_geometry());
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    session.state.viewport.scroll_x = origin;
    (work, ms)
}

#[test]
#[ignore = "measurement, not an assertion"]
fn arrangement_frame_cost_by_session_size() {
    let cases = [
        ("small: 8 MIDI × 4 clips × 200 notes", 8, 4, 200, 64, 4, 4),
        (
            "medium: 32 MIDI × 8 clips × 1k notes",
            32,
            8,
            1_000,
            256,
            16,
            8,
        ),
        (
            "large: 64 MIDI × 16 clips × 4k notes",
            64,
            16,
            4_000,
            512,
            32,
            16,
        ),
    ];
    println!();
    println!(
        "{:<40} {:>10} {:>7} {:>7} {:>9} {:>10}",
        "session", "notes", "lanes", "clips", "quads", "ms/frame"
    );
    for (label, midi_tracks, clips, notes, cc, audio_tracks, audio_clips) in cases {
        let mut session = Session::build(midi_tracks, clips, notes, cc, audio_tracks, audio_clips);
        let total_notes = session.total_notes();
        let (work, ms) = time_scroll_frames(&mut session, 20);
        println!(
            "{label:<40} {total_notes:>10} {:>7} {:>7} {:>9} {ms:>9.3}",
            work.lanes, work.clips, work.painted_note_quads
        );
    }
    println!();
}

/// The property the arrangement has to have: a clip's cost is what it *shows*,
/// not what it *holds*. Ten times the notes behind the same pixels must not be
/// ten times the work, or a dense imported part makes scrolling unusable.
///
/// Asserted on *work done*, not on a stopwatch. A preview build is the whole
/// per-note cost, so counting builds states the invariant exactly — and unlike
/// a wall-clock comparison it says the same thing on a loaded machine as on an
/// idle one. [`arrangement_frame_cost_by_session_size`] has the durations.
#[test]
fn a_clips_frame_cost_does_not_scale_with_the_notes_behind_it() {
    let mut sparse = Session::build(8, 4, 500, 0, 0, 0);
    let mut dense = Session::build(8, 4, 50_000, 0, 0, 0);
    assert_eq!(dense.total_notes(), sparse.total_notes() * 100);

    let before = note_previews_built();
    let (sparse_work, _) = time_scroll_frames(&mut sparse, 30);
    let sparse_builds = note_previews_built() - before;

    let before = note_previews_built();
    let (dense_work, _) = time_scroll_frames(&mut dense, 30);
    let dense_builds = note_previews_built() - before;

    assert_eq!(
        sparse_work.clips, dense_work.clips,
        "the two sessions must show the same clips"
    );
    // Not exact equality: the preview cache is process-wide and bounded, so
    // anything else drawing (another window, another test beside this one) can
    // evict an entry and cost a rebuild. The bound is still far below what a
    // note-count-sensitive key would produce — that would be one build per clip
    // per frame, roughly thirty times this.
    assert!(
        dense_builds <= sparse_builds * 4,
        "100x the notes caused {dense_builds} preview builds against {sparse_builds} — \
         the cache is keyed on something that scales with the note count"
    );
    // The real invariant: builds are bounded by the scroll, not by the frames.
    // 30 frames of 8 px is 240 px, so each clip crosses at most a tile or two.
    assert!(
        dense_builds <= dense_work.clips as u64 * 3,
        "{dense_builds} builds for {} visible clips over 240 px of scroll — the window \
         key is invalidating every frame",
        dense_work.clips
    );
}

/// Scrolling past a clip must stop costing anything for it. Culling that only
/// skips the *paint* still pays for the geometry, which is most of the cost.
#[test]
fn scrolled_out_clips_cost_nothing() {
    let session = Session::build(8, 8, 4_000, 128, 0, 0);
    let onscreen = session.frame_geometry();
    assert!(onscreen.clips > 0);

    let mut scrolled = Session::build(8, 8, 4_000, 128, 0, 0);
    // Far past the end of every clip.
    scrolled.state.viewport.scroll_x = 500_000.0;
    let offscreen = scrolled.frame_geometry();
    assert_eq!(
        offscreen.clips, 0,
        "clips scrolled far off the right edge are still being built"
    );
    assert_eq!(offscreen.painted_note_quads, 0);
}

/// A tempo map must not make every audio clip pay to rebuild it. The geometry
/// of an audio clip resolves through the tempo map, and doing that per clip per
/// frame turns a project with tempo automation into a slideshow.
#[test]
#[ignore = "measurement, not an assertion"]
fn audio_clip_geometry_cost_with_and_without_tempo_automation() {
    let mut flat = Session::build(0, 0, 0, 0, 40, 16);
    let mut curved = Session::build(0, 0, 0, 0, 40, 16);
    for marker in 0..64 {
        curved.state.tempo_map.add_or_update_point(
            marker as f64 * 8.0,
            90.0 + (marker % 7) as f64 * 6.0,
            if marker % 3 == 0 {
                TempoCurve::Linear
            } else {
                TempoCurve::Hold
            },
        );
    }

    let (flat_work, flat_ms) = time_scroll_frames(&mut flat, 20);
    let (curved_work, curved_ms) = time_scroll_frames(&mut curved, 20);
    println!();
    println!(
        "flat tempo   : {:>4} clips  {flat_ms:>8.3} ms/frame",
        flat_work.clips
    );
    println!(
        "64 tempo mks : {:>4} clips  {curved_ms:>8.3} ms/frame",
        curved_work.clips
    );
    println!();
}

/// The same property for the tempo map: an audio clip's geometry must not cost
/// a rebuild of the project's tempo map, or every marker the user adds slows
/// every frame down again.
#[test]
fn audio_clip_geometry_does_not_scale_with_tempo_marker_count() {
    let mut flat = Session::build(0, 0, 0, 0, 40, 16);
    let mut curved = Session::build(0, 0, 0, 0, 40, 16);
    for marker in 0..128 {
        curved
            .state
            .tempo_map
            .add_or_update_point(marker as f64 * 4.0, 100.0, TempoCurve::Hold);
    }

    let (flat_work, _) = time_scroll_frames(&mut flat, 30);
    let before = tempo_maps_built();
    let (curved_work, _) = time_scroll_frames(&mut curved, 30);
    let builds = tempo_maps_built() - before;

    assert_eq!(flat_work.clips, curved_work.clips);
    // The tempo did not move during the scroll, so one build covers all 30
    // frames and all 165 clips. Anything proportional to either means the map
    // is being rebuilt inside the per-clip geometry again.
    assert!(
        builds <= 2,
        "{builds} tempo maps built across 30 frames of {} clips — the map is being rebuilt \
         per clip",
        curved_work.clips
    );
}

/// What one `TimelineState` clone costs. Global lanes used to clone the whole
/// state into their pointer handlers on every render.
#[test]
#[ignore = "measurement, not an assertion"]
fn timeline_state_clone_cost_by_session_size() {
    let cases = [
        ("small: 8 MIDI × 4 clips × 200 notes", 8, 4, 200, 64, 4, 4),
        (
            "medium: 32 MIDI × 8 clips × 1k notes",
            32,
            8,
            1_000,
            256,
            16,
            8,
        ),
        (
            "large: 64 MIDI × 16 clips × 4k notes",
            64,
            16,
            4_000,
            512,
            32,
            16,
        ),
    ];
    println!();
    for (label, midi_tracks, clips, notes, cc, audio_tracks, audio_clips) in cases {
        let session = Session::build(midi_tracks, clips, notes, cc, audio_tracks, audio_clips);
        let runs = 10;
        let start = Instant::now();
        for _ in 0..runs {
            std::hint::black_box(session.state.clone());
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        println!("{label:<40} clone {ms:>8.3} ms");
    }
    println!();
}
