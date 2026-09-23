//! Standard MIDI File writer.
//!
//! The counterpart to [`super::midi_import`]: everything this module emits is
//! parseable by `midi_import::parse_smf_tracks`, and the round-trip is covered
//! by tests at the bottom of this file.
//!
//! Output is always **SMF format 1** with a leading conductor track carrying the
//! tempo map, time signatures, and markers, followed by one `MTrk` per exported
//! track. Beats in `TimelineState` are quarter notes, which is exactly what
//! `division` counts, so no musical rescaling happens here — only beats → ticks.

use super::timeline_state::{MidiControllerKind, MidiSysExKind};

/// Ticks per quarter note. 960 is the common DAW value: divisible by 3, 4, and
/// 5, so triplets and quintuplets land on exact ticks rather than drifting.
pub const TICKS_PER_BEAT: u16 = 960;

/// One note, in beats relative to the start of the exported range.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportNote {
    pub pitch: u8,
    pub start_beats: f64,
    pub duration_beats: f64,
    pub velocity: u8,
    /// Note Off velocity. `None` writes the conventional 0x40.
    pub release_velocity: Option<u8>,
    /// Zero-based MIDI channel (0..=15).
    pub channel: u8,
}

/// One controller point, in beats relative to the start of the exported range.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportControllerPoint {
    pub kind: MidiControllerKind,
    pub beat: f64,
    /// Normalized `0.0..=1.0`, matching the lane model.
    pub value: f64,
    pub channel: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportSysEx {
    pub kind: MidiSysExKind,
    pub beat: f64,
    pub data: Vec<u8>,
}

/// One `MTrk` of musical content.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExportTrack {
    pub name: String,
    pub notes: Vec<ExportNote>,
    pub controller_points: Vec<ExportControllerPoint>,
    pub sysex: Vec<ExportSysEx>,
}

impl ExportTrack {
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty() && self.controller_points.is_empty() && self.sysex.is_empty()
    }
}

/// A tempo change in the exported range. Curved segments are pre-sampled into
/// steps by the caller, because SMF has no ramp representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportTempoPoint {
    pub beat: f64,
    pub bpm: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportTimeSignature {
    pub beat: f64,
    pub numerator: u16,
    pub denominator: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportMarker {
    pub beat: f64,
    pub text: String,
    /// Complete `F0 … F7` messages carried by the marker, written to the
    /// conductor track at the marker's tick.
    pub sysex: Vec<Vec<u8>>,
}

/// Everything one exported `.mid` file contains.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MidiExport {
    /// Written as the conductor track's name (SMF sequence/track name).
    pub sequence_name: String,
    pub tempo_points: Vec<ExportTempoPoint>,
    pub time_signatures: Vec<ExportTimeSignature>,
    pub markers: Vec<ExportMarker>,
    pub tracks: Vec<ExportTrack>,
}

impl MidiExport {
    /// Total musical length in beats, used to place the conductor track's
    /// End of Track so a DAW reading the file sees the full arrangement length
    /// rather than stopping at the last tempo change.
    fn content_end_beats(&self) -> f64 {
        self.tracks
            .iter()
            .flat_map(|track| {
                track
                    .notes
                    .iter()
                    .map(|n| n.start_beats + n.duration_beats)
                    .chain(track.controller_points.iter().map(|p| p.beat))
                    .chain(track.sysex.iter().map(|s| s.beat))
            })
            .chain(self.markers.iter().map(|m| m.beat))
            .chain(self.time_signatures.iter().map(|t| t.beat))
            .chain(self.tempo_points.iter().map(|t| t.beat))
            .fold(0.0_f64, f64::max)
    }

    /// Serialize to Standard MIDI File bytes.
    pub fn to_smf_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // ── MThd ────────────────────────────────────────────────────────────
        out.extend_from_slice(b"MThd");
        out.extend_from_slice(&6u32.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // format 1
        let track_count = (self.tracks.len() as u16).saturating_add(1);
        out.extend_from_slice(&track_count.to_be_bytes());
        out.extend_from_slice(&TICKS_PER_BEAT.to_be_bytes());

        write_chunk(&mut out, &self.conductor_track_bytes());
        for track in &self.tracks {
            write_chunk(&mut out, &track_bytes(track));
        }
        out
    }

    fn conductor_track_bytes(&self) -> Vec<u8> {
        let mut events: Vec<(u64, u8, Vec<u8>)> = Vec::new();

        if !self.sequence_name.trim().is_empty() {
            // Order 0: the name belongs at the very top of the track.
            events.push((0, 0, meta_event(0x03, self.sequence_name.as_bytes())));
        }
        for point in &self.tempo_points {
            let us_per_beat = bpm_to_microseconds_per_beat(point.bpm);
            events.push((
                beats_to_ticks(point.beat),
                1,
                meta_event(
                    0x51,
                    &[
                        ((us_per_beat >> 16) & 0xFF) as u8,
                        ((us_per_beat >> 8) & 0xFF) as u8,
                        (us_per_beat & 0xFF) as u8,
                    ],
                ),
            ));
        }
        for sig in &self.time_signatures {
            // SMF stores the denominator as a power of two, plus MIDI clocks per
            // metronome click and 32nds per quarter note.
            let Some(denominator_pow2) = denominator_to_pow2(sig.denominator) else {
                continue;
            };
            let clocks_per_click = clocks_per_metronome_click(sig.denominator);
            events.push((
                beats_to_ticks(sig.beat),
                1,
                meta_event(
                    0x58,
                    &[
                        sig.numerator.min(255) as u8,
                        denominator_pow2,
                        clocks_per_click,
                        8,
                    ],
                ),
            ));
        }
        for marker in &self.markers {
            let tick = beats_to_ticks(marker.beat);
            let text = marker.text.trim();
            if !text.is_empty() {
                events.push((tick, 2, meta_event(0x06, text.as_bytes())));
            }
            for message in &marker.sysex {
                // SMF stores a normal SysEx as F0 <len> <bytes after F0>.
                let Some(payload) = sphere_midi_service::sysex::to_smf_payload(message) else {
                    continue;
                };
                if payload.is_empty() {
                    continue;
                }
                let mut bytes = vec![0xF0];
                write_vlq(&mut bytes, payload.len() as u32);
                bytes.extend_from_slice(&payload);
                events.push((tick, 3, bytes));
            }
        }

        let end_tick = beats_to_ticks(self.content_end_beats());
        serialize_track(events, end_tick)
    }
}

fn track_bytes(track: &ExportTrack) -> Vec<u8> {
    // `order` breaks ties at the same tick so the stream is deterministic and
    // musically correct: names first, then note-offs before note-ons (so a
    // repeated pitch retriggers instead of being cut by its own predecessor),
    // then controllers, then note-ons.
    let mut events: Vec<(u64, u8, Vec<u8>)> = Vec::new();

    if !track.name.trim().is_empty() {
        events.push((0, 0, meta_event(0x03, track.name.as_bytes())));
    }

    for note in &track.notes {
        let channel = note.channel & 0x0F;
        let pitch = note.pitch.min(127);
        let velocity = note.velocity.clamp(1, 127);
        let start = beats_to_ticks(note.start_beats);
        // A note must occupy at least one tick, or note-on and note-off collapse
        // and the note vanishes on re-import.
        let end = beats_to_ticks(note.start_beats + note.duration_beats).max(start + 1);
        events.push((start, 3, vec![0x90 | channel, pitch, velocity]));
        let release = note
            .release_velocity
            .map(|v| v.clamp(1, 127))
            .unwrap_or(0x40);
        events.push((end, 1, vec![0x80 | channel, pitch, release]));
    }

    for point in &track.controller_points {
        let channel = point.channel & 0x0F;
        let tick = beats_to_ticks(point.beat);
        let value = point.value.clamp(0.0, 1.0);
        match point.kind {
            MidiControllerKind::CC(number) => {
                let data = (value * 127.0).round().clamp(0.0, 127.0) as u8;
                events.push((tick, 2, vec![0xB0 | channel, number.min(127), data]));
            }
            MidiControllerKind::PitchBend => {
                let raw = (value * 16383.0).round().clamp(0.0, 16383.0) as u16;
                events.push((
                    tick,
                    2,
                    vec![
                        0xE0 | channel,
                        (raw & 0x7F) as u8,
                        ((raw >> 7) & 0x7F) as u8,
                    ],
                ));
            }
            MidiControllerKind::ChannelPressure => {
                let data = (value * 127.0).round().clamp(0.0, 127.0) as u8;
                events.push((tick, 2, vec![0xD0 | channel, data]));
            }
            // Poly pressure needs a per-note association the lane model does not
            // carry, so it is not invented here. `midi_import` declines to read
            // it for the same reason.
            MidiControllerKind::PolyPressure => {}
        }
    }

    for sysex in &track.sysex {
        if sysex.data.is_empty() {
            continue;
        }
        let status = match sysex.kind {
            MidiSysExKind::Normal => 0xF0u8,
            MidiSysExKind::Escaped => 0xF7u8,
        };
        let mut bytes = vec![status];
        write_vlq(&mut bytes, sysex.data.len() as u32);
        bytes.extend_from_slice(&sysex.data);
        events.push((beats_to_ticks(sysex.beat), 2, bytes));
    }

    let end_tick = events.iter().map(|(tick, _, _)| *tick).max().unwrap_or(0);
    serialize_track(events, end_tick)
}

/// Sort by (tick, order), then emit with delta times and a closing End of Track.
///
/// Running status is deliberately not used: it saves a few bytes and costs
/// clarity, and every reader has to handle the explicit form anyway.
fn serialize_track(mut events: Vec<(u64, u8, Vec<u8>)>, end_tick: u64) -> Vec<u8> {
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut out = Vec::new();
    let mut last_tick = 0u64;
    for (tick, _, bytes) in &events {
        write_vlq(&mut out, (tick.saturating_sub(last_tick)) as u32);
        out.extend_from_slice(bytes);
        last_tick = *tick;
    }
    write_vlq(&mut out, (end_tick.saturating_sub(last_tick)) as u32);
    out.extend_from_slice(&meta_event(0x2F, &[]));
    out
}

fn write_chunk(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(b"MTrk");
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
}

fn meta_event(meta_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0xFF, meta_type];
    write_vlq(&mut bytes, payload.len() as u32);
    bytes.extend_from_slice(payload);
    bytes
}

fn write_vlq(out: &mut Vec<u8>, mut value: u32) {
    let mut buffer = [0u8; 5];
    let mut index = buffer.len();
    index -= 1;
    buffer[index] = (value & 0x7F) as u8;
    value >>= 7;
    while value > 0 {
        index -= 1;
        buffer[index] = ((value & 0x7F) as u8) | 0x80;
        value >>= 7;
    }
    out.extend_from_slice(&buffer[index..]);
}

pub fn beats_to_ticks(beats: f64) -> u64 {
    if !beats.is_finite() || beats <= 0.0 {
        return 0;
    }
    (beats * TICKS_PER_BEAT as f64).round().max(0.0) as u64
}

fn bpm_to_microseconds_per_beat(bpm: f64) -> u32 {
    // Guard against a degenerate tempo producing a zero or overflowing value;
    // 0x00FFFFFF is the widest a 3-byte SMF tempo can express.
    let bpm = if bpm.is_finite() && bpm > 0.0 {
        bpm
    } else {
        120.0
    };
    (60_000_000.0 / bpm).round().clamp(1.0, 0x00FF_FFFF as f64) as u32
}

/// SMF writes the time-signature denominator as its base-2 exponent, so only
/// powers of two are representable. `None` means "skip this marker" rather than
/// writing a signature the file cannot express.
fn denominator_to_pow2(denominator: u16) -> Option<u8> {
    if denominator == 0 || !denominator.is_power_of_two() {
        return None;
    }
    Some(denominator.trailing_zeros() as u8)
}

/// MIDI clocks per metronome click. 24 clocks is one quarter note, so the click
/// follows the denominator unit — a click per eighth in 6/8, per quarter in 4/4.
fn clocks_per_metronome_click(denominator: u16) -> u8 {
    let clicks = (24.0 * 4.0 / denominator.max(1) as f64).round();
    clicks.clamp(1.0, 255.0) as u8
}

// ── Building an export from project state ───────────────────────────────────
//
// These are pure functions over `TimelineState`, so the whole feature is
// testable without a GPUI window.

use super::timeline_state::{
    ClipState, ClipType, MidiOutputChannelMode, TempoCurve, TimelineState, TrackState,
};
use crate::layout::engine_snapshot::{articulated_note_playback, ArticulationLegatoIndex};

/// Beat span to export, in project beats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportRange {
    pub start_beats: f64,
    pub end_beats: f64,
}

impl ExportRange {
    fn contains_start(&self, beat: f64) -> bool {
        beat >= self.start_beats - 1e-6 && beat < self.end_beats - 1e-6
    }
}

/// Curved tempo segments have no SMF representation, so they are sampled into
/// steps. A quarter-beat grid keeps a ritardando smooth without bloating the
/// conductor track.
const TEMPO_CURVE_SAMPLE_BEATS: f64 = 0.25;

/// Which span of the arrangement to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MidiExportSpan {
    /// Everything from bar 1 to the end of the last MIDI clip.
    #[default]
    WholeArrangement,
    /// Just the loop range, shifted so it starts at bar 1.
    LoopRange,
}

/// What the arrangement export includes. Presented by the export dialog; the
/// defaults are "everything", so an untouched dialog writes the same file the
/// direct export used to.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiExportOptions {
    pub span: MidiExportSpan,
    /// Set-tempo events. Off writes a file that follows the receiving DAW's
    /// tempo instead of carrying this project's.
    pub include_tempo_map: bool,
    pub include_time_signatures: bool,
    pub include_markers: bool,
    /// CC, pitch-bend, and channel-pressure lanes.
    pub include_controllers: bool,
    pub include_sysex: bool,
    /// Track ids to write. `None` means every MIDI track — distinct from
    /// `Some(empty)`, which is an explicit "no tracks" the dialog can produce.
    pub track_ids: Option<Vec<String>>,
}

impl Default for MidiExportOptions {
    fn default() -> Self {
        Self {
            span: MidiExportSpan::default(),
            include_tempo_map: true,
            include_time_signatures: true,
            include_markers: true,
            include_controllers: true,
            include_sysex: true,
            track_ids: None,
        }
    }
}

impl MidiExportOptions {
    fn includes_track(&self, track_id: &str) -> bool {
        match &self.track_ids {
            None => true,
            Some(ids) => ids.iter().any(|id| id == track_id),
        }
    }
}

/// Whether this project has a loop span the export dialog can offer.
pub fn loop_export_range(state: &TimelineState) -> Option<ExportRange> {
    let transport = &state.transport;
    (transport.loop_end_beats > transport.loop_start_beats).then(|| ExportRange {
        start_beats: transport.loop_start_beats.max(0.0) as f64,
        end_beats: transport.loop_end_beats as f64,
    })
}

/// The span File ▸ Export MIDI File writes for the chosen [`MidiExportSpan`].
///
/// Asking for the loop range when the project has none falls back to the whole
/// arrangement rather than writing an empty file.
pub fn arrangement_export_range(state: &TimelineState, span: MidiExportSpan) -> ExportRange {
    if span == MidiExportSpan::LoopRange {
        if let Some(range) = loop_export_range(state) {
            return range;
        }
    }
    ExportRange {
        start_beats: 0.0,
        end_beats: arrangement_content_end_beats(state).max(1e-6),
    }
}

/// Default options for this project: the loop span is preselected when looping
/// is actually **enabled**, so a stale loop left over from earlier editing never
/// silently narrows the export — but the dialog still offers it either way.
pub fn default_export_options(state: &TimelineState) -> MidiExportOptions {
    MidiExportOptions {
        span: if state.transport.loop_enabled && loop_export_range(state).is_some() {
            MidiExportSpan::LoopRange
        } else {
            MidiExportSpan::WholeArrangement
        },
        ..MidiExportOptions::default()
    }
}

/// Every track the export dialog can offer, in arrangement order: those holding
/// at least one MIDI clip.
pub fn exportable_tracks(state: &TimelineState) -> Vec<(String, String)> {
    state
        .tracks
        .iter()
        .filter(|track| {
            track
                .clips
                .iter()
                .any(|clip| matches!(clip.clip_type, ClipType::Midi { .. }))
        })
        .map(|track| (track.id.clone(), track.name.clone()))
        .collect()
}

fn arrangement_content_end_beats(state: &TimelineState) -> f64 {
    state
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter())
        .filter(|clip| matches!(clip.clip_type, ClipType::Midi { .. }))
        .map(|clip| (clip.start_beat + clip.duration_beats) as f64)
        .fold(0.0_f64, f64::max)
}

/// Whole arrangement (or the active loop span) as SMF format 1.
///
/// Notes carry the same articulation treatment as playback — the exported file
/// is what you hear, not the raw stored durations, because the receiving DAW
/// has no way to reproduce Futureboard articulations.
pub fn build_arrangement_export(
    state: &TimelineState,
    sequence_name: &str,
    options: &MidiExportOptions,
) -> MidiExport {
    let range = arrangement_export_range(state, options.span);
    let tracks = state
        .tracks
        .iter()
        .filter(|track| options.includes_track(&track.id))
        .filter_map(|track| build_track(track, &range, options))
        .filter(|track| !track.is_empty())
        .collect();

    MidiExport {
        sequence_name: sequence_name.to_string(),
        // A conductor track with no tempo at all is legal — the reader then
        // uses its own — so an unchecked box really does omit the events.
        tempo_points: if options.include_tempo_map {
            collect_tempo_points(state, &range)
        } else {
            Vec::new()
        },
        time_signatures: if options.include_time_signatures {
            collect_time_signatures(state, &range)
        } else {
            Vec::new()
        },
        markers: if options.include_markers {
            collect_markers(state, &range, options.include_sysex)
        } else {
            Vec::new()
        },
        tracks,
    }
}

/// One MIDI clip as a standalone file, its notes starting at bar 1.
///
/// The tempo and time signature in force at the clip's arrangement position are
/// written at beat 0 so the phrase reads back at the speed it was written at.
/// Returns `None` when the id is not a MIDI clip.
pub fn build_clip_export(
    state: &TimelineState,
    clip_id: &str,
    sequence_name: &str,
) -> Option<MidiExport> {
    let (track, clip) = state.tracks.iter().find_map(|track| {
        track
            .clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .map(|clip| (track, clip))
    })?;
    if !matches!(clip.clip_type, ClipType::Midi { .. }) {
        return None;
    }

    let clip_start = clip.start_beat.max(0.0) as f64;
    let range = ExportRange {
        start_beats: clip_start,
        end_beats: clip_start + clip.duration_beats.max(0.0) as f64,
    };

    // Only this clip: build the track from a one-clip view so sibling clips on
    // the same track are excluded.
    let mut export_track = ExportTrack {
        name: if track.name.trim().is_empty() {
            clip.name.clone()
        } else {
            track.name.clone()
        },
        ..Default::default()
    };
    // A single clip carries everything it has: the include toggles belong to
    // the arrangement dialog, which this path does not go through.
    append_clip(
        &mut export_track,
        track,
        clip,
        &range,
        &MidiExportOptions::default(),
    );

    let signature = state.time_signature_map.time_signature_at_beat(clip_start);
    Some(MidiExport {
        sequence_name: sequence_name.to_string(),
        tempo_points: vec![ExportTempoPoint {
            beat: 0.0,
            bpm: state
                .tempo_map
                .bpm_at_beat(clip_start, state.bpm as f64)
                .max(1.0),
        }],
        time_signatures: vec![ExportTimeSignature {
            beat: 0.0,
            numerator: signature.numerator,
            denominator: signature.denominator,
        }],
        markers: collect_markers(state, &range, true),
        tracks: if export_track.is_empty() {
            Vec::new()
        } else {
            vec![export_track]
        },
    })
}

fn build_track(
    track: &TrackState,
    range: &ExportRange,
    options: &MidiExportOptions,
) -> Option<ExportTrack> {
    let mut export_track = ExportTrack {
        name: track.name.clone(),
        ..Default::default()
    };
    for clip in &track.clips {
        append_clip(&mut export_track, track, clip, range, options);
    }
    Some(export_track)
}

/// Mirrors the engine snapshot's MIDI conversion so an exported file matches
/// playback: muted clips and muted notes are dropped, notes keep their
/// articulated length/velocity, and the track's channel mode decides whether a
/// note plays on its own channel or the track's fixed one.
fn append_clip(
    export_track: &mut ExportTrack,
    track: &TrackState,
    clip: &ClipState,
    range: &ExportRange,
    options: &MidiExportOptions,
) {
    if clip.muted {
        return;
    }
    let ClipType::Midi {
        notes,
        controller_lanes,
        sysex_events,
        articulations,
    } = &clip.clip_type
    else {
        return;
    };

    let clip_start = clip.start_beat.max(0.0) as f64;
    let output_mode: MidiOutputChannelMode = track.routing.output_channel_mode();
    let lane_channel = output_mode
        .resolve(track.routing.default_note_channel())
        .raw();
    let legato_index = ArticulationLegatoIndex::build(notes, output_mode);

    for note in notes.iter().filter(|note| !note.muted) {
        let channel = output_mode.resolve(note.channel).raw();
        let (length_beats, velocity) =
            articulated_note_playback(note, articulations, channel, &legato_index);
        if length_beats <= 0.0 {
            continue; // matches the engine: zero-length notes never sound
        }
        let absolute_start = clip_start + note.start.max(0.0) as f64;
        if !range.contains_start(absolute_start) {
            continue;
        }
        // Truncate at the range end so a partial export cannot run past the
        // span the user asked for.
        let absolute_end = (absolute_start + length_beats as f64).min(range.end_beats);
        export_track.notes.push(ExportNote {
            pitch: note.pitch.min(127),
            start_beats: absolute_start - range.start_beats,
            duration_beats: (absolute_end - absolute_start).max(0.0),
            velocity,
            release_velocity: note.release_velocity,
            channel,
        });
    }

    for lane in controller_lanes
        .iter()
        .filter(|_| options.include_controllers)
        .filter(|lane| !lane.points.is_empty())
    {
        for point in &lane.points {
            let absolute = clip_start + point.beat.max(0.0) as f64;
            if !range.contains_start(absolute) {
                continue;
            }
            export_track.controller_points.push(ExportControllerPoint {
                kind: lane.kind,
                beat: absolute - range.start_beats,
                value: point.value.clamp(0.0, 1.0) as f64,
                channel: lane_channel,
            });
        }
    }

    for sysex in sysex_events
        .iter()
        .filter(|_| options.include_sysex)
        .filter(|s| !s.data.is_empty())
    {
        let absolute = clip_start + sysex.beat.max(0.0) as f64;
        if !range.contains_start(absolute) {
            continue;
        }
        export_track.sysex.push(ExportSysEx {
            kind: sysex.kind.clone(),
            beat: absolute - range.start_beats,
            data: sysex.data.clone(),
        });
    }
}

fn collect_tempo_points(state: &TimelineState, range: &ExportRange) -> Vec<ExportTempoPoint> {
    let base = state.bpm as f64;
    let mut out = vec![ExportTempoPoint {
        beat: 0.0,
        bpm: state
            .tempo_map
            .bpm_at_beat(range.start_beats, base)
            .max(1.0),
    }];

    let points = &state.tempo_map.points;
    for (index, point) in points.iter().enumerate() {
        if point.beat <= range.start_beats || point.beat >= range.end_beats {
            continue;
        }
        out.push(ExportTempoPoint {
            beat: point.beat - range.start_beats,
            bpm: point.bpm.max(1.0),
        });
        // A curved segment has to be sampled: SMF only understands steps.
        if point.curve == TempoCurve::Hold {
            continue;
        }
        let segment_end = points
            .get(index + 1)
            .map(|next| next.beat)
            .unwrap_or(range.end_beats)
            .min(range.end_beats);
        let mut beat = point.beat + TEMPO_CURVE_SAMPLE_BEATS;
        while beat < segment_end - 1e-6 {
            out.push(ExportTempoPoint {
                beat: beat - range.start_beats,
                bpm: state.tempo_map.bpm_at_beat(beat, base).max(1.0),
            });
            beat += TEMPO_CURVE_SAMPLE_BEATS;
        }
    }

    out.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    out.dedup_by(|a, b| (a.beat - b.beat).abs() < 1e-9 && (a.bpm - b.bpm).abs() < 1e-9);
    out
}

fn collect_time_signatures(state: &TimelineState, range: &ExportRange) -> Vec<ExportTimeSignature> {
    let at_start = state
        .time_signature_map
        .time_signature_at_beat(range.start_beats);
    let mut out = vec![ExportTimeSignature {
        beat: 0.0,
        numerator: at_start.numerator,
        denominator: at_start.denominator,
    }];
    for point in &state.time_signature_map.points {
        if point.beat <= range.start_beats || point.beat >= range.end_beats {
            continue;
        }
        out.push(ExportTimeSignature {
            beat: point.beat - range.start_beats,
            numerator: point.numerator,
            denominator: point.denominator,
        });
    }
    out.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    out
}

fn collect_markers(
    state: &TimelineState,
    range: &ExportRange,
    include_sysex: bool,
) -> Vec<ExportMarker> {
    let mut out: Vec<ExportMarker> = state
        .markers
        .iter()
        .filter(|marker| range.contains_start(marker.beat))
        .map(|marker| ExportMarker {
            beat: marker.beat - range.start_beats,
            text: marker.name.clone(),
            sysex: if include_sysex {
                marker.sysex.clone()
            } else {
                Vec::new()
            },
        })
        .collect();
    out.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::midi_import::{parse_smf_tracks, ImportedSysExKind};

    fn note(pitch: u8, start: f64, duration: f64, velocity: u8) -> ExportNote {
        ExportNote {
            pitch,
            start_beats: start,
            duration_beats: duration,
            velocity,
            release_velocity: None,
            channel: 0,
        }
    }

    fn export_with(tracks: Vec<ExportTrack>) -> MidiExport {
        MidiExport {
            sequence_name: "Song".to_string(),
            tempo_points: vec![ExportTempoPoint {
                beat: 0.0,
                bpm: 120.0,
            }],
            time_signatures: vec![ExportTimeSignature {
                beat: 0.0,
                numerator: 4,
                denominator: 4,
            }],
            markers: Vec::new(),
            tracks,
        }
    }

    #[test]
    fn notes_round_trip_through_the_importer() {
        let export = export_with(vec![ExportTrack {
            name: "Keys".to_string(),
            notes: vec![
                note(60, 0.0, 1.0, 100),
                note(64, 1.0, 0.5, 80),
                note(67, 2.5, 1.5, 127),
            ],
            ..Default::default()
        }]);

        let bytes = export.to_smf_bytes();
        let tracks = parse_smf_tracks(&bytes).expect("parse");
        // Conductor track carries no notes, so it produces one empty track.
        let keys = tracks
            .iter()
            .find(|t| t.name.as_deref() == Some("Keys"))
            .expect("named track survived");
        assert_eq!(keys.clip.notes.len(), 3);
        for (actual, expected) in keys.clip.notes.iter().zip([
            (60u8, 0.0f32, 1.0f32, 100u8),
            (64, 1.0, 0.5, 80),
            (67, 2.5, 1.5, 127),
        ]) {
            assert_eq!(actual.pitch, expected.0);
            assert!((actual.start - expected.1).abs() < 1e-4, "start {actual:?}");
            assert!(
                (actual.duration - expected.2).abs() < 1e-4,
                "duration {actual:?}"
            );
            assert_eq!(actual.velocity, expected.3);
        }
    }

    #[test]
    fn repeated_pitch_at_the_same_tick_stays_two_notes() {
        // Note-off must be ordered before note-on at a shared tick, or the new
        // note's on is immediately cancelled by the previous note's off and the
        // pair collapses into one.
        let export = export_with(vec![ExportTrack {
            name: "Repeat".to_string(),
            notes: vec![note(60, 0.0, 1.0, 100), note(60, 1.0, 1.0, 100)],
            ..Default::default()
        }]);

        let tracks = parse_smf_tracks(&export.to_smf_bytes()).expect("parse");
        let repeat = tracks
            .iter()
            .find(|t| t.name.as_deref() == Some("Repeat"))
            .expect("track");
        assert_eq!(repeat.clip.notes.len(), 2);
        assert!(repeat
            .clip
            .notes
            .iter()
            .all(|n| (n.duration - 1.0).abs() < 1e-4));
    }

    #[test]
    fn a_note_shorter_than_one_tick_still_survives() {
        let export = export_with(vec![ExportTrack {
            name: "Tiny".to_string(),
            notes: vec![note(60, 0.0, 0.0, 100)],
            ..Default::default()
        }]);
        let tracks = parse_smf_tracks(&export.to_smf_bytes()).expect("parse");
        let tiny = tracks
            .iter()
            .find(|t| t.name.as_deref() == Some("Tiny"))
            .expect("track");
        assert_eq!(tiny.clip.notes.len(), 1, "zero-length note must not vanish");
    }

    #[test]
    fn controller_lanes_round_trip() {
        let export = export_with(vec![ExportTrack {
            name: "CC".to_string(),
            controller_points: vec![
                ExportControllerPoint {
                    kind: MidiControllerKind::CC(1),
                    beat: 0.0,
                    value: 0.0,
                    channel: 0,
                },
                ExportControllerPoint {
                    kind: MidiControllerKind::CC(1),
                    beat: 2.0,
                    value: 1.0,
                    channel: 0,
                },
                ExportControllerPoint {
                    kind: MidiControllerKind::PitchBend,
                    beat: 1.0,
                    value: 0.5,
                    channel: 0,
                },
            ],
            ..Default::default()
        }]);

        let tracks = parse_smf_tracks(&export.to_smf_bytes()).expect("parse");
        let cc_track = tracks
            .iter()
            .find(|t| t.name.as_deref() == Some("CC"))
            .expect("track");
        let cc = cc_track
            .clip
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::CC(1))
            .expect("cc lane");
        assert_eq!(cc.points.len(), 2);
        assert!((cc.points[0].value - 0.0).abs() < 1e-3);
        assert!((cc.points[1].value - 1.0).abs() < 1e-3);

        let bend = cc_track
            .clip
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::PitchBend)
            .expect("bend lane");
        assert!((bend.points[0].value - 0.5).abs() < 1e-3);
    }

    #[test]
    fn markers_and_sysex_round_trip() {
        let mut export = export_with(vec![ExportTrack {
            name: "Sys".to_string(),
            sysex: vec![ExportSysEx {
                kind: MidiSysExKind::Normal,
                beat: 1.0,
                data: vec![0x41, 0x10, 0xF7],
            }],
            ..Default::default()
        }]);
        let gs_reset = vec![
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7,
        ];
        export.markers = vec![ExportMarker {
            beat: 4.0,
            text: "Chorus".to_string(),
            sysex: vec![gs_reset.clone()],
        }];

        let tracks = parse_smf_tracks(&export.to_smf_bytes()).expect("parse");
        let conductor = &tracks[0];
        assert_eq!(conductor.clip.markers.len(), 1);
        assert_eq!(conductor.clip.markers[0].text, "Chorus");
        assert!((conductor.clip.markers[0].beat - 4.0).abs() < 1e-4);
        // The marker's SysEx lands on the conductor track at the marker tick.
        assert_eq!(conductor.clip.sysex_events.len(), 1);
        assert!((conductor.clip.sysex_events[0].beat - 4.0).abs() < 1e-4);
        assert_eq!(
            sphere_midi_service::sysex::from_smf_payload(&conductor.clip.sysex_events[0].data),
            gs_reset
        );

        let sys = tracks
            .iter()
            .find(|t| t.name.as_deref() == Some("Sys"))
            .expect("track");
        assert_eq!(sys.clip.sysex_events.len(), 1);
        assert_eq!(sys.clip.sysex_events[0].kind, ImportedSysExKind::Normal);
        assert_eq!(sys.clip.sysex_events[0].data, vec![0x41, 0x10, 0xF7]);
    }

    #[test]
    fn per_note_channels_split_into_separate_imported_tracks() {
        let mut high = note(72, 0.0, 1.0, 100);
        high.channel = 3;
        let export = export_with(vec![ExportTrack {
            name: "Multi".to_string(),
            notes: vec![note(60, 0.0, 1.0, 100), high],
            ..Default::default()
        }]);

        let tracks = parse_smf_tracks(&export.to_smf_bytes()).expect("parse");
        // The importer splits a multi-channel MTrk per channel, so the channel
        // assignment survives even though the lane model is clip-global.
        let multi: Vec<_> = tracks
            .iter()
            .filter(|t| {
                t.name
                    .as_deref()
                    .is_some_and(|name| name.starts_with("Multi"))
            })
            .collect();
        assert_eq!(multi.len(), 2);
        let channels: Vec<u8> = multi
            .iter()
            .filter_map(|t| t.channel_hint.map(|c| c.raw()))
            .collect();
        assert!(channels.contains(&0) && channels.contains(&3));
    }

    #[test]
    fn tempo_and_time_signature_are_encoded_in_the_conductor_track() {
        let mut export = export_with(Vec::new());
        export.tempo_points = vec![ExportTempoPoint {
            beat: 0.0,
            bpm: 140.0,
        }];
        export.time_signatures = vec![ExportTimeSignature {
            beat: 0.0,
            numerator: 6,
            denominator: 8,
        }];
        let bytes = export.to_smf_bytes();

        // 60_000_000 / 140 = 428571 us per quarter note.
        let expected = 428_571u32;
        let tempo = [
            0xFF,
            0x51,
            0x03,
            ((expected >> 16) & 0xFF) as u8,
            ((expected >> 8) & 0xFF) as u8,
            (expected & 0xFF) as u8,
        ];
        assert!(
            bytes.windows(tempo.len()).any(|w| w == tempo),
            "set-tempo meta event missing"
        );
        // 6/8 => numerator 6, denominator exponent 3, 12 clocks per click.
        let sig = [0xFFu8, 0x58, 0x04, 6, 3, 12, 8];
        assert!(
            bytes.windows(sig.len()).any(|w| w == sig),
            "time-signature meta event missing"
        );
        // Still parseable with no musical tracks at all.
        assert!(parse_smf_tracks(&bytes).is_ok());
    }

    #[test]
    fn non_power_of_two_denominators_are_skipped_not_corrupted() {
        // SMF cannot express 5/6; writing a bogus exponent would produce a file
        // that reads back at the wrong meter.
        assert_eq!(denominator_to_pow2(6), None);
        assert_eq!(denominator_to_pow2(4), Some(2));
        assert_eq!(denominator_to_pow2(8), Some(3));
    }

    // ── Building from project state ─────────────────────────────────────────

    mod from_state {
        use super::super::*;
        use crate::components::edit::EditCommand;
        use crate::components::timeline::timeline_state::{
            CreateTrackOptions, InputMonitorMode, TempoPoint, TimeSignaturePoint,
            TimelineMarkerState, TimelineState, TrackType,
        };

        /// One instrument track with a 4-beat MIDI clip at `clip_start`.
        fn state_with_clip(clip_start: f32) -> (TimelineState, String) {
            let mut state = TimelineState::default();
            state.tracks.clear();
            let track_id = state.create_track(CreateTrackOptions {
                track_type: TrackType::Instrument,
                name: "Inst".to_string(),
                color: gpui::Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            let clip = state
                .build_midi_clip(&track_id, clip_start, 4.0)
                .expect("clip");
            let clip_id = clip.id.clone();
            EditCommand::CreateClip { track_id, clip }.execute(&mut state);
            (state, clip_id)
        }

        #[test]
        fn arrangement_places_notes_at_absolute_beats() {
            let (mut state, clip_id) = state_with_clip(8.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.add_midi_note(&clip_id, 64, 2.0, 1.0, 100).unwrap();

            let export = build_arrangement_export(&state, "Song", &MidiExportOptions::default());
            let track = export.tracks.first().expect("one track");
            let starts: Vec<f64> = track.notes.iter().map(|n| n.start_beats).collect();
            assert_eq!(starts, vec![8.0, 10.0]);
        }

        #[test]
        fn muted_clips_and_notes_are_excluded_like_playback() {
            let (mut state, clip_id) = state_with_clip(0.0);
            let muted = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.add_midi_note(&clip_id, 64, 1.0, 1.0, 100).unwrap();
            state.set_midi_notes_muted(&clip_id, &[muted], true);

            let export = build_arrangement_export(&state, "Song", &MidiExportOptions::default());
            let track = export.tracks.first().expect("track");
            assert_eq!(track.notes.len(), 1);
            assert_eq!(track.notes[0].pitch, 64);
        }

        #[test]
        fn an_enabled_loop_narrows_the_export_and_shifts_it_to_bar_one() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.add_midi_note(&clip_id, 64, 2.0, 1.0, 100).unwrap();
            state.transport.loop_enabled = true;
            state.transport.loop_start_beats = 2.0;
            state.transport.loop_end_beats = 4.0;

            let export = build_arrangement_export(&state, "Song", &default_export_options(&state));
            let track = export.tracks.first().expect("track");
            assert_eq!(track.notes.len(), 1, "only the in-range note");
            assert_eq!(track.notes[0].pitch, 64);
            assert!(
                track.notes[0].start_beats.abs() < 1e-9,
                "the range start becomes bar 1"
            );
        }

        #[test]
        fn a_disabled_loop_does_not_silently_narrow_the_export() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.add_midi_note(&clip_id, 64, 2.0, 1.0, 100).unwrap();
            // A stale loop range left over from editing, with looping off.
            state.transport.loop_enabled = false;
            state.transport.loop_start_beats = 2.0;
            state.transport.loop_end_beats = 4.0;

            let export = build_arrangement_export(&state, "Song", &default_export_options(&state));
            assert_eq!(export.tracks[0].notes.len(), 2);
        }

        #[test]
        fn a_note_running_past_the_range_is_truncated_at_the_range_end() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 8.0, 100).unwrap();
            state.transport.loop_enabled = true;
            state.transport.loop_start_beats = 0.0;
            state.transport.loop_end_beats = 2.0;

            let export = build_arrangement_export(&state, "Song", &default_export_options(&state));
            let note = &export.tracks[0].notes[0];
            assert!(
                (note.duration_beats - 2.0).abs() < 1e-9,
                "expected truncation to the range, got {note:?}"
            );
        }

        #[test]
        fn clip_export_is_relative_and_carries_the_tempo_in_force() {
            let (mut state, clip_id) = state_with_clip(16.0);
            state.add_midi_note(&clip_id, 60, 1.0, 1.0, 100).unwrap();
            state.bpm = 100.0;
            state.tempo_map.points = vec![TempoPoint::new(16.0, 90.0, TempoCurve::Hold)];

            let export = build_clip_export(&state, &clip_id, "Phrase").expect("midi clip");
            let track = export.tracks.first().expect("track");
            assert_eq!(track.notes.len(), 1);
            assert!(
                (track.notes[0].start_beats - 1.0).abs() < 1e-9,
                "clip-relative start"
            );
            assert_eq!(export.tempo_points.len(), 1);
            assert!((export.tempo_points[0].bpm - 90.0).abs() < 1e-9);
        }

        #[test]
        fn clip_export_declines_a_non_midi_clip() {
            let (state, _clip_id) = state_with_clip(0.0);
            assert!(build_clip_export(&state, "no-such-clip", "X").is_none());
        }

        #[test]
        fn tempo_and_time_signature_changes_inside_the_range_are_carried() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.bpm = 120.0;
            state.tempo_map.points = vec![TempoPoint::new(2.0, 90.0, TempoCurve::Hold)];
            state.time_signature_map.points = vec![
                TimeSignaturePoint::new(0.0, 4, 4),
                TimeSignaturePoint::new(2.0, 3, 4),
            ];

            let export = build_arrangement_export(&state, "Song", &MidiExportOptions::default());
            assert_eq!(export.tempo_points.len(), 2);
            assert!((export.tempo_points[0].bpm - 120.0).abs() < 1e-9);
            assert!((export.tempo_points[1].beat - 2.0).abs() < 1e-9);
            assert_eq!(export.time_signatures.len(), 2);
            assert_eq!(export.time_signatures[1].numerator, 3);
        }

        #[test]
        fn a_curved_tempo_segment_is_sampled_into_steps() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 4.0, 100).unwrap();
            state.bpm = 120.0;
            state.tempo_map.points = vec![
                TempoPoint::new(0.5, 120.0, TempoCurve::Linear),
                TempoPoint::new(3.5, 60.0, TempoCurve::Hold),
            ];

            let export = build_arrangement_export(&state, "Song", &MidiExportOptions::default());
            // SMF has no ramp, so the segment must appear as many small steps
            // rather than one jump at the end.
            assert!(
                export.tempo_points.len() > 4,
                "expected a sampled ramp, got {:?}",
                export.tempo_points
            );
            assert!(export
                .tempo_points
                .windows(2)
                .all(|w| w[0].beat <= w[1].beat));
        }

        #[test]
        fn markers_in_range_are_shifted_with_the_export() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.markers = vec![
                TimelineMarkerState {
                    id: "m1".to_string(),
                    beat: 1.0,
                    name: "Intro".to_string(),
                    color_hex: "#fff".to_string(),
                    sysex: Vec::new(),
                },
                TimelineMarkerState {
                    id: "m2".to_string(),
                    beat: 99.0,
                    name: "Way out".to_string(),
                    color_hex: "#fff".to_string(),
                    sysex: Vec::new(),
                },
            ];
            state.transport.loop_enabled = true;
            state.transport.loop_start_beats = 0.5;
            state.transport.loop_end_beats = 4.0;

            let export = build_arrangement_export(&state, "Song", &default_export_options(&state));
            assert_eq!(export.markers.len(), 1);
            assert_eq!(export.markers[0].text, "Intro");
            assert!((export.markers[0].beat - 0.5).abs() < 1e-9);
        }

        #[test]
        fn unchecking_an_include_omits_only_that_part() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.markers = vec![TimelineMarkerState {
                id: "m1".to_string(),
                beat: 1.0,
                name: "Intro".to_string(),
                color_hex: "#fff".to_string(),
                sysex: Vec::new(),
            }];

            let all = build_arrangement_export(&state, "Song", &MidiExportOptions::default());
            assert!(!all.markers.is_empty());
            assert!(!all.tempo_points.is_empty());
            assert!(!all.time_signatures.is_empty());

            let no_markers = build_arrangement_export(
                &state,
                "Song",
                &MidiExportOptions {
                    include_markers: false,
                    ..MidiExportOptions::default()
                },
            );
            assert!(no_markers.markers.is_empty());
            // Unchecking one box must not disturb the others.
            assert_eq!(no_markers.tempo_points, all.tempo_points);
            assert_eq!(no_markers.time_signatures, all.time_signatures);
            assert_eq!(no_markers.tracks, all.tracks);

            let no_tempo = build_arrangement_export(
                &state,
                "Song",
                &MidiExportOptions {
                    include_tempo_map: false,
                    include_time_signatures: false,
                    ..MidiExportOptions::default()
                },
            );
            assert!(no_tempo.tempo_points.is_empty());
            assert!(no_tempo.time_signatures.is_empty());
            // Still a readable file with no conductor content at all.
            assert!(crate::components::timeline::midi_import::parse_smf_tracks(
                &no_tempo.to_smf_bytes()
            )
            .is_ok());
        }

        #[test]
        fn unchecking_controllers_and_sysex_drops_them_but_keeps_notes() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.put_controller_point(&clip_id, MidiControllerKind::CC(1), 0.5, 0.75);

            let with_cc = build_arrangement_export(&state, "Song", &MidiExportOptions::default());
            assert!(!with_cc.tracks[0].controller_points.is_empty());

            let without = build_arrangement_export(
                &state,
                "Song",
                &MidiExportOptions {
                    include_controllers: false,
                    include_sysex: false,
                    ..MidiExportOptions::default()
                },
            );
            assert!(without.tracks[0].controller_points.is_empty());
            assert!(without.tracks[0].sysex.is_empty());
            assert_eq!(without.tracks[0].notes.len(), 1, "notes are unaffected");
        }

        #[test]
        fn a_track_selection_writes_only_the_named_tracks() {
            let (mut state, clip_a) = state_with_clip(0.0);
            state.add_midi_note(&clip_a, 60, 0.0, 1.0, 100).unwrap();
            let track_a = state.tracks[0].id.clone();
            // A second instrument track with its own clip.
            let track_b = state.create_track(CreateTrackOptions {
                track_type: TrackType::Instrument,
                name: "Second".to_string(),
                color: gpui::Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            let clip_b = state.build_midi_clip(&track_b, 0.0, 4.0).expect("clip");
            let clip_b_id = clip_b.id.clone();
            EditCommand::CreateClip {
                track_id: track_b.clone(),
                clip: clip_b,
            }
            .execute(&mut state);
            state.add_midi_note(&clip_b_id, 72, 0.0, 1.0, 100).unwrap();

            assert_eq!(exportable_tracks(&state).len(), 2);

            let only_b = build_arrangement_export(
                &state,
                "Song",
                &MidiExportOptions {
                    track_ids: Some(vec![track_b.clone()]),
                    ..MidiExportOptions::default()
                },
            );
            assert_eq!(only_b.tracks.len(), 1);
            assert_eq!(only_b.tracks[0].name, "Second");

            // An empty selection is an explicit "nothing", not "everything".
            let none = build_arrangement_export(
                &state,
                "Song",
                &MidiExportOptions {
                    track_ids: Some(Vec::new()),
                    ..MidiExportOptions::default()
                },
            );
            assert!(none.tracks.is_empty());
            let _ = track_a;
        }

        #[test]
        fn the_dialog_preselects_the_loop_span_only_when_looping_is_on() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.transport.loop_start_beats = 1.0;
            state.transport.loop_end_beats = 3.0;

            state.transport.loop_enabled = false;
            assert_eq!(
                default_export_options(&state).span,
                MidiExportSpan::WholeArrangement
            );
            state.transport.loop_enabled = true;
            assert_eq!(
                default_export_options(&state).span,
                MidiExportSpan::LoopRange
            );
            // The dialog can still offer the span either way.
            assert!(loop_export_range(&state).is_some());
        }

        #[test]
        fn asking_for_a_loop_span_that_does_not_exist_falls_back_to_the_whole_arrangement() {
            let (mut state, clip_id) = state_with_clip(0.0);
            state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
            state.add_midi_note(&clip_id, 64, 2.0, 1.0, 100).unwrap();
            // A new project ships with a default loop range, which is exactly
            // why the span is an explicit choice rather than inferred; clear it
            // so there is genuinely nothing to fall back to.
            state.transport.loop_start_beats = 0.0;
            state.transport.loop_end_beats = 0.0;
            assert!(loop_export_range(&state).is_none());

            let export = build_arrangement_export(
                &state,
                "Song",
                &MidiExportOptions {
                    span: MidiExportSpan::LoopRange,
                    ..MidiExportOptions::default()
                },
            );
            assert_eq!(
                export.tracks[0].notes.len(),
                2,
                "must not write an empty file"
            );
        }

        #[test]
        fn a_project_with_no_midi_still_writes_a_readable_file() {
            let mut state = TimelineState::default();
            state.tracks.clear();
            let export = build_arrangement_export(&state, "Empty", &MidiExportOptions::default());
            let bytes = export.to_smf_bytes();
            let tracks =
                crate::components::timeline::midi_import::parse_smf_tracks(&bytes).expect("parse");
            assert_eq!(tracks.len(), 1, "conductor track only");
        }
    }

    #[test]
    fn vlq_matches_the_spec_boundaries() {
        let cases: [(u32, &[u8]); 5] = [
            (0, &[0x00]),
            (0x7F, &[0x7F]),
            (0x80, &[0x81, 0x00]),
            (0x3FFF, &[0xFF, 0x7F]),
            (0x0FFFFFFF, &[0xFF, 0xFF, 0xFF, 0x7F]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            write_vlq(&mut out, value);
            assert_eq!(out, expected, "vlq({value})");
        }
    }
}
