//! Publishes Studio's transport, meters and audio to the native visualizer
//! app (`apps/native/visualizer`) over the shared-memory link
//! ([`visualizer_link`], protocol in its crate docs).
//!
//! Studio creates the region on its first meter tick and keeps it for the
//! session. Nothing else happens until the app stamps its heartbeat: while no
//! visualizer reads, a meter tick costs one atomic load and a clock read, and
//! the audio pump sleeps without listening to any tap, so the engine copies
//! nothing for it.
//!
//! While a visualizer reads:
//! - every meter tick writes the frame (transport, master and Control Room
//!   levels) and the track table (up to [`visualizer_link::MAX_TRACKS`], in
//!   mixer order) under the link's seqlock, on the UI thread;
//! - a pump thread copies the post-fader master tap
//!   (`DirectAudio::visualizer_tap`) and the track tap
//!   (`DirectAudio::track_tap`) into the link's rings, never on the audio
//!   thread;
//! - the track the app asks for is routed to the track tap with
//!   `AudioEngine::set_visualizer_track_tap`, re-applied a few times a second
//!   because a rebuilt engine graph starts with no track tapped.
//!
//! A second Studio cannot create the region while the first holds it; it
//! retries every few seconds and takes over once the first one exits.

use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, Instant};

use visualizer_link::{
    fill_name, flags, kind, track_key, transport, AudioWriter, Frame, Levels, TrackEntry, Writer,
    MAX_TRACKS,
};

use crate::components::timeline::timeline_state::{volume, TimelineState, TrackType};

/// Between attempts to create the region while another process holds it.
const CREATE_RETRY: Duration = Duration::from_secs(3);
/// Writer heartbeat cadence; the app calls Studio gone after a few missed.
const HEARTBEAT_EVERY: Duration = Duration::from_millis(500);
/// How often the requested track tap is re-applied to the engine.
const TAP_REFRESH: Duration = Duration::from_millis(250);
/// Folder depth past which nesting is not shown.
const MAX_DEPTH: u8 = 16;

#[derive(Default)]
pub(crate) struct Publisher {
    writer: Option<Writer>,
    created_attempt_at: Option<Instant>,
    heartbeat_at: Option<Instant>,
    /// Reused every tick, so publishing allocates nothing once warm.
    tracks: Vec<TrackEntry>,
    /// `(key, track id)` currently routed to the engine's track tap.
    tapped: Option<(u64, String)>,
    tap_applied_at: Option<Instant>,
}

impl fmt::Debug for Publisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Publisher")
            .field("published", &self.writer.is_some())
            .field("tapped", &self.tapped.as_ref().map(|(_, id)| id))
            .finish()
    }
}

impl Publisher {
    /// One meter tick. `meters` is this tick's engine snapshot, before the
    /// UI ballistics touch it.
    pub(crate) fn publish(
        &mut self,
        engine: &DirectAudio::AudioEngine,
        meters: &DirectAudio::types::JsMeterSnapshot,
        state: &TimelineState,
        project_name: &str,
    ) {
        let now = Instant::now();
        if self.writer.is_none() && !self.try_create(now) {
            return;
        }
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        if self
            .heartbeat_at
            .is_none_or(|at| now.duration_since(at) >= HEARTBEAT_EVERY)
        {
            writer.stamp_heartbeat();
            self.heartbeat_at = Some(now);
        }
        if !writer.reader_alive() {
            if self.tapped.take().is_some() {
                engine.set_visualizer_track_tap(None);
                writer.set_tapped_track_key(0);
            }
            return;
        }

        fill_tracks(&mut self.tracks, meters, state);
        let frame = build_frame(meters, state, project_name);
        writer.write_frame(&frame, &self.tracks);

        let requested = writer.requested_track_key();
        let current = self.tapped.as_ref().map_or(0, |(key, _)| *key);
        let wanted = (requested != 0)
            .then(|| {
                state
                    .tracks
                    .iter()
                    .take(MAX_TRACKS)
                    .find(|track| track_key(&track.id) == requested)
            })
            .flatten();
        match wanted {
            Some(track) => {
                let due = requested != current
                    || self
                        .tap_applied_at
                        .is_none_or(|at| now.duration_since(at) >= TAP_REFRESH);
                if due {
                    engine.set_visualizer_track_tap(Some(&track.id));
                    if requested != current {
                        self.tapped = Some((requested, track.id.clone()));
                        writer.set_tapped_track_key(requested);
                    }
                    self.tap_applied_at = Some(now);
                }
            }
            None if current != 0 => {
                engine.set_visualizer_track_tap(None);
                self.tapped = None;
                writer.set_tapped_track_key(0);
            }
            None => {}
        }
    }

    fn try_create(&mut self, now: Instant) -> bool {
        if self
            .created_attempt_at
            .is_some_and(|at| now.duration_since(at) < CREATE_RETRY)
        {
            return false;
        }
        self.created_attempt_at = Some(now);
        match Writer::create() {
            Ok(writer) => {
                spawn_audio_pump(writer.audio());
                self.writer = Some(writer);
                true
            }
            Err(_) => false,
        }
    }
}

fn levels(peak_l: f64, peak_r: f64, rms_l: f64, rms_r: f64) -> Levels {
    Levels {
        peak_l: peak_l as f32,
        peak_r: peak_r as f32,
        rms_l: rms_l as f32,
        rms_r: rms_r as f32,
    }
}

fn rgb(color: gpui::Rgba) -> u32 {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

fn track_kind(id: &str, track_type: TrackType) -> u8 {
    if id.starts_with("vsti-out:") {
        return kind::PLUGIN_OUTPUT;
    }
    match track_type {
        TrackType::Audio | TrackType::Master => kind::AUDIO,
        TrackType::Midi => kind::MIDI,
        TrackType::Instrument => kind::INSTRUMENT,
        TrackType::Bus => kind::BUS,
        TrackType::Return => kind::RETURN,
        TrackType::Group => kind::GROUP,
        TrackType::Video => kind::VIDEO,
    }
}

fn fill_tracks(
    out: &mut Vec<TrackEntry>,
    meters: &DirectAudio::types::JsMeterSnapshot,
    state: &TimelineState,
) {
    out.clear();
    let index_by_id = state.track_index_by_id();
    let selected: HashSet<&str> = state
        .selection
        .selected_track_ids
        .iter()
        .map(String::as_str)
        .chain(state.selection.selected_track_id.as_deref())
        .collect();
    for track in state.tracks.iter().take(MAX_TRACKS) {
        let mut depth = 0u8;
        let mut parent = track.parent_group_id.as_deref();
        while let Some(id) = parent {
            if depth >= MAX_DEPTH {
                break;
            }
            depth += 1;
            parent = index_by_id
                .get(id)
                .and_then(|index| state.tracks[*index].parent_group_id.as_deref());
        }
        let mut track_flags = 0;
        for (on, bit) in [
            (track.muted, flags::MUTED),
            (track.solo, flags::SOLO),
            (track.armed, flags::ARMED),
            (selected.contains(track.id.as_str()), flags::SELECTED),
            (track.meter_clip, flags::CLIPPED),
        ] {
            if on {
                track_flags |= bit;
            }
        }
        let mut entry = TrackEntry {
            key: track_key(&track.id),
            kind: track_kind(&track.id, track.track_type),
            flags: track_flags,
            depth,
            color: rgb(track.color),
            volume_db: volume::norm_to_db(track.volume_effective),
            pan: track.pan,
            ..TrackEntry::default()
        };
        fill_name(&mut entry.name, &track.name);
        out.push(entry);
    }
    for meter in &meters.tracks {
        if let Some(entry) = index_by_id
            .get(meter.track_id.as_str())
            .and_then(|index| out.get_mut(*index))
        {
            entry.levels = levels(meter.peak_l, meter.peak_r, meter.rms_l, meter.rms_r);
        }
    }
}

fn build_frame(
    meters: &DirectAudio::types::JsMeterSnapshot,
    state: &TimelineState,
    project_name: &str,
) -> Frame {
    let beats = f64::from(state.transport.playhead_beats);
    let base_bpm = f64::from(state.bpm.max(1.0));
    let signature = state.time_signature_at_playhead();
    let mut transport_bits = 0;
    for (on, bit) in [
        (state.transport.playing, transport::PLAYING),
        (state.transport.recording, transport::RECORDING),
        (state.transport.loop_enabled, transport::LOOPING),
    ] {
        if on {
            transport_bits |= bit;
        }
    }
    let mut frame = Frame {
        position_beats: beats,
        position_seconds: state.tempo_map.seconds_at_beat(beats.max(0.0), base_bpm),
        bpm: state.tempo_map.bpm_at_beat(beats, base_bpm) as f32,
        time_sig_num: u32::from(signature.numerator),
        time_sig_den: u32::from(signature.denominator),
        transport: transport_bits,
        master: levels(
            meters.master_peak_l,
            meters.master_peak_r,
            meters.master_rms_l,
            meters.master_rms_r,
        ),
        monitor: levels(meters.monitor_peak_l, meters.monitor_peak_r, 0.0, 0.0),
        master_volume_db: volume::norm_to_db(state.display_master_volume()),
        selected_track_key: state
            .selection
            .selected_track_id
            .as_deref()
            .map_or(0, track_key),
        ..Frame::default()
    };
    fill_name(&mut frame.project_name, project_name);
    frame
}

/// Copies the master and track taps into the link's rings while a
/// visualizer reads. Runs for the life of the process; listens to the taps
/// only while a reader is alive.
fn spawn_audio_pump(audio: AudioWriter) {
    let _ = std::thread::Builder::new()
        .name("visualizer-link".into())
        .spawn(move || pump_audio(&audio));
}

fn pump_audio(audio: &AudioWriter) {
    const BLOCK: usize = 8_192;
    let master = DirectAudio::visualizer_tap();
    let track = DirectAudio::track_tap();
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut cursors: Option<(u64, u64)> = None;
    while !crate::shutdown::ShutdownState::global().is_shutting_down() {
        if !audio.reader_alive() {
            if cursors.take().is_some() {
                master.unlisten();
                track.unlisten();
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        let (master_cursor, track_cursor) =
            cursors.get_or_insert_with(|| (master.listen(), track.listen()));
        loop {
            let read = master.read(*master_cursor, &mut left, &mut right);
            *master_cursor = read.next;
            if read.frames == 0 {
                break;
            }
            audio.write_master_audio(&left[..read.frames], &right[..read.frames]);
        }
        loop {
            let read = track.read(*track_cursor, &mut left, &mut right);
            *track_cursor = read.next;
            if read.frames == 0 {
                break;
            }
            audio.write_track_audio(&left[..read.frames], &right[..read.frames]);
        }
        let rate = master.sample_rate().max(track.sample_rate());
        if rate != 0 {
            audio.set_sample_rate(rate);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if cursors.is_some() {
        master.unlisten();
        track.unlisten();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_pack_as_rrggbb() {
        let color = gpui::Rgba {
            r: 1.0,
            g: 0.5,
            b: 0.0,
            a: 1.0,
        };
        assert_eq!(rgb(color), 0xff_80_00);
    }

    #[test]
    fn plugin_output_children_are_their_own_kind() {
        assert_eq!(
            track_kind("vsti-out:insert-1:bus:1", TrackType::Audio),
            kind::PLUGIN_OUTPUT
        );
        assert_eq!(track_kind("track-1", TrackType::Bus), kind::BUS);
    }
}
