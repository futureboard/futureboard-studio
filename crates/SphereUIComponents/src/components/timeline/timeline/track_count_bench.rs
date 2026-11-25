//! What an arrangement frame costs as the project's track count grows.
//!
//! Rows are virtualized, so a screenful of a 2 000-track project draws the same
//! rows as a screenful of a 20-track one. Anything the frame still does once
//! per *track* rather than once per *visible row* shows up here as a frame cost
//! that climbs with the track count.
//!
//! Run with:
//!
//! ```text
//! cargo test -p sphere_ui_components --features builtin-plugin-editor --release --lib track_count_bench -- --ignored --nocapture
//! ```

use std::time::Instant;

use gpui::{px, TestAppContext};

use super::Timeline;
use crate::components::timeline::timeline_state::{
    AudioClipStretchState, AudioImportState, ClipState, ClipType, MidiNoteState,
};

/// A project of `tracks` MIDI tracks, each with clips across the first
/// minute, so every visible row carries clips at any scroll.
fn timeline_with(tracks: usize) -> Timeline {
    let mut timeline = Timeline::new();
    timeline.state.tracks.clear();
    for _ in 0..tracks {
        let id = timeline.state.create_midi_track();
        let track = timeline
            .state
            .tracks
            .iter_mut()
            .find(|track| track.id == id)
            .expect("track");
        for slot in 0..4 {
            track.clips.push(ClipState {
                id: format!("{id}-clip-{slot}"),
                name: format!("Clip {slot}"),
                start_beat: slot as f32 * 8.0,
                duration_beats: 8.0,
                source_duration_seconds: None,
                offset_beats: 0.0,
                gain: 1.0,
                clip_type: ClipType::Midi {
                    notes: (0..16)
                        .map(|n| MidiNoteState::new(60 + (n % 12) as u8, n as f32 * 0.5, 0.4, 96))
                        .collect(),
                    controller_lanes: Vec::new(),
                    sysex_events: Vec::new(),
                    articulations: Vec::new(),
                },
                muted: false,
                audio_import: AudioImportState::Ready,
                stretch: AudioClipStretchState::default(),
            });
        }
    }
    timeline
}

struct FrameCost {
    wall_ms: f64,
    prepaint_ms: f32,
    paint_ms: f32,
    nodes: u64,
}

/// Frames of a vertical scroll through the middle of the project — the gesture
/// whose cost a big session makes the user feel.
fn measure(cx: &mut TestAppContext, tracks: usize, frames: usize) -> FrameCost {
    let window = cx.open_window(gpui::size(px(1600.0), px(900.0)), move |_, _| {
        timeline_with(tracks)
    });
    cx.run_until_parked();
    // Warm: the first frames create the lane views and fill the caches.
    for _ in 0..3 {
        window
            .update(cx, |timeline, _, cx| {
                timeline.state.viewport.scroll_y += 1.0;
                cx.notify();
            })
            .expect("window is open");
        cx.run_until_parked();
    }

    let (mut prepaint, mut paint, mut nodes) = (0.0_f32, 0.0_f32, 0_u64);
    let started = Instant::now();
    for frame in 0..frames {
        window
            .update(cx, |timeline, _, cx| {
                // ~12 px a frame, a steady wheel scroll.
                timeline.state.viewport.scroll_y = 200.0 + frame as f32 * 12.0;
                cx.notify();
            })
            .expect("window is open");
        cx.run_until_parked();
        let profile = gpui::frame_profile::frame_profile();
        prepaint += profile.prepaint_ms();
        paint += profile.paint_ms();
        nodes = nodes.max(profile.layout_nodes);
    }
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    window
        .update(cx, |_, window, _| window.remove_window())
        .expect("window is open");
    cx.run_until_parked();
    FrameCost {
        wall_ms,
        prepaint_ms: prepaint / frames as f32,
        paint_ms: paint / frames as f32,
        nodes,
    }
}

#[gpui::test]
#[ignore = "measurement, not an assertion"]
fn arrangement_frame_cost_by_track_count(cx: &mut TestAppContext) {
    println!();
    println!(
        "{:>7} {:>9} {:>11} {:>9} {:>7}",
        "tracks", "wall ms", "prepaint ms", "paint ms", "nodes"
    );
    for tracks in [16_usize, 128, 512, 1_024, 2_048] {
        let cost = measure(cx, tracks, 20);
        println!(
            "{tracks:>7} {:>9.3} {:>11.3} {:>9.3} {:>7}",
            cost.wall_ms, cost.prepaint_ms, cost.paint_ms, cost.nodes
        );
    }
    println!();
}
