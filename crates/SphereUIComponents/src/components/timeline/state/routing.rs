use super::*;
use crate::audio_connections::{AudioConnectionId, AudioConnectionReference};
use sphere_midi_service::mpe::MpeTrackConfiguration;
use sphere_midi_service::program::MidiProgramSelection;

pub use crate::project::InputMonitorMode;

/// Whether a track is a user-created Bus/Return that may receive normal track
/// outputs and aux sends. VSTi multi-output child strips deliberately use the
/// `Bus` type so the engine can mix them, but they are runtime-derived mixer
/// channels rather than project routing tracks and must never appear as normal
/// Send/Output destinations.
pub fn is_project_routing_track(track: &TrackState) -> bool {
    track.track_type.is_routing() && !is_vsti_output_child_track_id(&track.id)
}

/// A single aux send from this track to a Bus/Return track (Phase 3). The
/// runtime sums `gain_db`-scaled signal into the target's input. UI stores the
/// descriptor; DirectAudio owns the realtime accumulation.
#[derive(Debug, Clone, PartialEq)]
pub struct SendSlotState {
    pub id: String,
    /// Id of the destination Bus/Return track.
    pub target_track_id: String,
    /// Display label for the destination (resolved at edit time; refreshed
    /// from the track list on render).
    pub target_name: String,
    pub enabled: bool,
    /// `true` = tap before the source track fader; `false` = post-fader.
    /// Realtime currently honours post-fader only (pre-fader is a refinement).
    pub pre_fader: bool,
    pub gain_db: f32,
}

impl SendSlotState {
    /// Linear send gain from `gain_db` (clamped to a sane range).
    pub fn gain_linear(&self) -> f32 {
        10f32.powf(self.gain_db.clamp(-60.0, 6.0) / 20.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackOutputRouting {
    Main,
    Bus {
        bus_id: String,
    },
    /// A **MIDI** hardware output port, set from the MIDI Out selector on a
    /// MIDI track. Despite the name this is never an audio destination: audio
    /// leaves through the Master / Monitor Output Audio Connections
    /// ([`crate::output_routing`]), and no audio selector can produce this
    /// variant. MIDI device routing is deliberately independent of the
    /// Audio Connections registry.
    HardwareOutput {
        device_id: String,
        channel: u32,
    },
    /// A MIDI track's notes/controllers are redirected to the named
    /// Instrument track's own plugin instead of that instrument's own clips.
    /// Only meaningful on `TrackType::Midi` tracks; see
    /// `TimelineState::effective_instrument_track_id`.
    Instrument {
        track_id: String,
    },
    None,
}

impl TrackOutputRouting {
    pub fn label(&self) -> String {
        match self {
            Self::Main => "Main".to_string(),
            Self::Bus { bus_id } => bus_id.clone(),
            Self::HardwareOutput { device_id, channel } => {
                format!("{device_id} ch {}", channel + 1)
            }
            // Callers that know the live track list should prefer
            // `panel::midi_output_combo_label`, which resolves the target's
            // display name; this is the id-only fallback.
            Self::Instrument { track_id } => format!("Instrument - {track_id}"),
            Self::None => "None".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackAudioFormat {
    Mono,
    Stereo,
}

impl TrackAudioFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mono => "Mono",
            Self::Stereo => "Stereo",
        }
    }

    /// Channel count this format needs from an Input Audio Connection.
    pub fn channel_count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackMidiInputRouting {
    None,
    AllInputs,
    MidiDevice { device_id: String },
}

impl TrackMidiInputRouting {
    pub fn label(&self) -> String {
        match self {
            Self::None => "None".to_string(),
            Self::AllInputs => "All MIDI Inputs".to_string(),
            Self::MidiDevice { device_id } => device_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRoutingState {
    /// Logical audio input bus from the project Audio Connections registry.
    /// `None` is No Input. Never a raw device id or channel index — the
    /// registry is the only layer mapping a bus to physical ports.
    ///
    /// Independent from [`Self::midi_input`]: a track may legitimately hold
    /// both an audio input connection and a MIDI input assignment.
    pub audio_input_connection_id: Option<AudioConnectionId>,
    pub output: TrackOutputRouting,
    pub audio_format: TrackAudioFormat,
    pub midi_input: TrackMidiInputRouting,
    /// `None` means All channels. `Some` is clamped to 1..=16 by mutation
    /// helpers and project-load conversion.
    pub midi_channel: Option<u8>,
    /// Which incoming MIDI channels this track listens to. Model only in this
    /// pass — not yet enforced on the recording input path.
    pub midi_input_filter: MidiInputChannelFilter,
    /// `true` plays each note back on its own channel ([`MidiOutputChannelMode::PerNote`]);
    /// `false` (default) forces every note onto `midi_channel` (or channel 1),
    /// matching the pre-existing single-channel-per-track behavior.
    pub midi_output_per_note: bool,
    /// Per-track MPE output policy. Community builds retain and play this
    /// state, while the Professional UI exposes its editing controls.
    pub mpe: MpeTrackConfiguration,
    /// The bank and program this track sets its instrument (or MIDI output
    /// device) to, on its MIDI channel, in the GM/GS/XG layout it names. No
    /// program sends nothing.
    pub program: MidiProgramSelection,
}

impl TrackRoutingState {
    /// The effective output channel policy, derived from `midi_channel` /
    /// `midi_output_per_note` so there is exactly one field driving the
    /// existing channel selector UI and no duplicated state to fall out of
    /// sync.
    pub fn output_channel_mode(&self) -> MidiOutputChannelMode {
        if self.midi_output_per_note {
            MidiOutputChannelMode::PerNote
        } else {
            MidiOutputChannelMode::Fixed(MidiChannel::from_ui(self.midi_channel.unwrap_or(1)))
        }
    }

    /// Channel newly drawn notes on this track should default to.
    pub fn default_note_channel(&self) -> MidiChannel {
        MidiChannel::from_ui(self.midi_channel.unwrap_or(1))
    }
}

impl TrackRoutingState {
    pub fn for_track_type(track_type: TrackType) -> Self {
        match track_type {
            TrackType::Audio => Self {
                audio_input_connection_id: None,
                output: TrackOutputRouting::Main,
                audio_format: TrackAudioFormat::Stereo,
                midi_input: TrackMidiInputRouting::None,
                midi_channel: None,
                midi_input_filter: MidiInputChannelFilter::All,
                midi_output_per_note: false,
                mpe: MpeTrackConfiguration::default(),
                program: MidiProgramSelection::default(),
            },
            TrackType::Instrument => Self {
                audio_input_connection_id: None,
                output: TrackOutputRouting::Main,
                audio_format: TrackAudioFormat::Stereo,
                midi_input: TrackMidiInputRouting::AllInputs,
                midi_channel: None,
                midi_input_filter: MidiInputChannelFilter::All,
                midi_output_per_note: false,
                mpe: MpeTrackConfiguration::default(),
                program: MidiProgramSelection::default(),
            },
            TrackType::Midi => Self {
                audio_input_connection_id: None,
                output: TrackOutputRouting::None,
                audio_format: TrackAudioFormat::Stereo,
                midi_input: TrackMidiInputRouting::AllInputs,
                midi_channel: None,
                midi_input_filter: MidiInputChannelFilter::All,
                midi_output_per_note: false,
                mpe: MpeTrackConfiguration::default(),
                program: MidiProgramSelection::default(),
            },
            TrackType::Bus | TrackType::Return | TrackType::Group => Self {
                audio_input_connection_id: None,
                output: TrackOutputRouting::Main,
                audio_format: TrackAudioFormat::Stereo,
                midi_input: TrackMidiInputRouting::None,
                midi_channel: None,
                midi_input_filter: MidiInputChannelFilter::All,
                midi_output_per_note: false,
                mpe: MpeTrackConfiguration::default(),
                program: MidiProgramSelection::default(),
            },
            TrackType::Master => Self {
                audio_input_connection_id: None,
                output: TrackOutputRouting::Main,
                audio_format: TrackAudioFormat::Stereo,
                midi_input: TrackMidiInputRouting::None,
                midi_channel: None,
                midi_input_filter: MidiInputChannelFilter::All,
                midi_output_per_note: false,
                mpe: MpeTrackConfiguration::default(),
                program: MidiProgramSelection::default(),
            },
            // A Video track is picture-only: no input, no output, no MIDI.
            TrackType::Video => Self {
                audio_input_connection_id: None,
                output: TrackOutputRouting::None,
                audio_format: TrackAudioFormat::Stereo,
                midi_input: TrackMidiInputRouting::None,
                midi_channel: None,
                midi_input_filter: MidiInputChannelFilter::All,
                midi_output_per_note: false,
                mpe: MpeTrackConfiguration::default(),
                program: MidiProgramSelection::default(),
            },
        }
    }
}

impl TimelineState {
    /// Assign a track's audio input to a logical Audio Connection.
    ///
    /// Stores only the stable id, so a later rename or device change on that
    /// connection flows through without touching the track.
    pub fn set_track_audio_input_connection(
        &mut self,
        track_id: &str,
        connection_id: Option<AudioConnectionId>,
    ) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.audio_input_connection_id != connection_id {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] audio input track={} old={:?} new={:?}",
                        track_id, t.routing.audio_input_connection_id, connection_id
                    );
                }
                t.routing.audio_input_connection_id = connection_id;
                return true;
            }
        }
        false
    }

    /// Clear every track assignment pointing at `connection_id`.
    ///
    /// Affected tracks become No Input rather than being silently pointed at
    /// another bus. Returns the changed track ids.
    pub fn unassign_audio_connection(&mut self, connection_id: &AudioConnectionId) -> Vec<String> {
        let mut affected = Vec::new();
        for track in &mut self.tracks {
            if track.routing.audio_input_connection_id.as_ref() == Some(connection_id) {
                track.routing.audio_input_connection_id = None;
                affected.push(track.id.clone());
            }
        }
        affected
    }

    /// Tracks whose audio input references `connection_id`.
    pub fn tracks_using_audio_connection(&self, connection_id: &AudioConnectionId) -> Vec<&str> {
        self.tracks
            .iter()
            .filter(|track| track.routing.audio_input_connection_id.as_ref() == Some(connection_id))
            .map(|track| track.id.as_str())
            .collect()
    }

    // ── Master / Monitor output routing ──────────────────────────────────────

    /// Assign the project's Master output. `None` is No Output — the project
    /// then has no hardware destination at all, which is a valid state and not
    /// a reason to substitute one.
    pub fn set_master_output_connection(
        &mut self,
        connection_id: Option<AudioConnectionId>,
    ) -> bool {
        if self.master_output_connection_id == connection_id {
            return false;
        }
        if routing_debug_enabled() {
            eprintln!(
                "[routing] master output old={:?} new={:?}",
                self.master_output_connection_id, connection_id
            );
        }
        self.master_output_connection_id = connection_id;
        true
    }

    /// Assign the Monitor / Control Room output override. `None` is **Follow
    /// Master Output**.
    ///
    /// Deliberately never touches the Master assignment: Source and Output are
    /// independent concepts, and so are the two buses' destinations.
    pub fn set_monitor_output_connection(
        &mut self,
        connection_id: Option<AudioConnectionId>,
    ) -> bool {
        if self.monitor_output_connection_id == connection_id {
            return false;
        }
        if routing_debug_enabled() {
            eprintln!(
                "[routing] monitor output old={:?} new={:?}",
                self.monitor_output_connection_id, connection_id
            );
        }
        self.monitor_output_connection_id = connection_id;
        true
    }

    /// The Output Audio Connection the Control Room actually feeds: the
    /// override, else Master. `None` means silence.
    pub fn effective_monitor_output_connection(&self) -> Option<AudioConnectionId> {
        crate::output_routing::effective_monitor_output(
            self.master_output_connection_id.as_ref(),
            self.monitor_output_connection_id.as_ref(),
        )
    }

    /// Everything in the project that points at `connection_id`, so a removal
    /// can name all of its consequences — including a bus used only by Master.
    pub fn audio_connection_references(
        &self,
        connection_id: &AudioConnectionId,
    ) -> Vec<AudioConnectionReference> {
        let mut references: Vec<AudioConnectionReference> = self
            .tracks
            .iter()
            .filter(|track| track.routing.audio_input_connection_id.as_ref() == Some(connection_id))
            .map(|track| AudioConnectionReference::TrackInput(track.id.clone()))
            .collect();
        if self.master_output_connection_id.as_ref() == Some(connection_id) {
            references.push(AudioConnectionReference::MasterOutput);
        }
        if self.monitor_output_connection_id.as_ref() == Some(connection_id) {
            references.push(AudioConnectionReference::MonitorOutput);
        }
        references
    }

    /// Clear the Master and Monitor references to `connection_id`.
    ///
    /// Clearing Monitor returns it to Follow Master Output; clearing Master
    /// leaves the project with no output. Neither is re-pointed at some other
    /// bus — choosing a replacement is the user's call.
    pub fn unassign_output_connection(
        &mut self,
        connection_id: &AudioConnectionId,
    ) -> Vec<AudioConnectionReference> {
        let mut cleared = Vec::new();
        if self.master_output_connection_id.as_ref() == Some(connection_id) {
            self.master_output_connection_id = None;
            cleared.push(AudioConnectionReference::MasterOutput);
        }
        if self.monitor_output_connection_id.as_ref() == Some(connection_id) {
            self.monitor_output_connection_id = None;
            cleared.push(AudioConnectionReference::MonitorOutput);
        }
        cleared
    }

    /// Resolve which track's plugin instance should actually receive a
    /// track's MIDI events during playback/preview: an Instrument track
    /// plays its own clips; a MIDI track routed via
    /// `TrackOutputRouting::Instrument` plays through that target instead
    /// (only while the target still exists and is still an Instrument
    /// track — a stale/retyped target yields `None`, i.e. silence, rather
    /// than guessing a different destination).
    pub fn effective_instrument_track_id(&self, track_id: &str) -> Option<String> {
        let track = self.tracks.iter().find(|t| t.id == track_id)?;
        match track.track_type {
            TrackType::Instrument => Some(track.id.clone()),
            TrackType::Midi => match &track.routing.output {
                TrackOutputRouting::Instrument {
                    track_id: target_id,
                } => self
                    .tracks
                    .iter()
                    .find(|t| t.id == *target_id && t.track_type == TrackType::Instrument)
                    .map(|t| t.id.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn set_track_output_routing(&mut self, track_id: &str, output: TrackOutputRouting) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.output != output {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] output track={} old={:?} new={:?}",
                        track_id, t.routing.output, output
                    );
                }
                t.routing.output = output;
                return true;
            }
        }
        false
    }

    pub fn set_track_audio_format(
        &mut self,
        track_id: &str,
        audio_format: TrackAudioFormat,
    ) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.audio_format != audio_format {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] audio_format track={} old={:?} new={:?}",
                        track_id, t.routing.audio_format, audio_format
                    );
                }
                t.routing.audio_format = audio_format;
                return true;
            }
        }
        false
    }

    pub fn set_track_midi_input(
        &mut self,
        track_id: &str,
        midi_input: TrackMidiInputRouting,
    ) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.midi_input != midi_input {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] midi_input track={} old={:?} new={:?}",
                        track_id, t.routing.midi_input, midi_input
                    );
                }
                t.routing.midi_input = midi_input;
                return true;
            }
        }
        false
    }

    pub fn set_track_midi_channel(&mut self, track_id: &str, channel: Option<u8>) -> bool {
        let channel = channel.map(|ch| ch.clamp(1, 16));
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.midi_channel != channel {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] midi_channel track={} old={:?} new={:?}",
                        track_id, t.routing.midi_channel, channel
                    );
                }
                t.routing.midi_channel = channel;
                return true;
            }
        }
        false
    }

    /// Set the track's output channel policy (see [`TrackRoutingState::output_channel_mode`]).
    /// Returns `true` if it changed — callers should panic/all-notes-off the
    /// track afterwards so notes already sounding on the old channel don't stick.
    pub fn set_track_midi_output_per_note(&mut self, track_id: &str, per_note: bool) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.midi_output_per_note != per_note {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] midi_output_per_note track={} old={} new={}",
                        track_id, t.routing.midi_output_per_note, per_note
                    );
                }
                t.routing.midi_output_per_note = per_note;
                return true;
            }
        }
        false
    }

    /// Set the complete MPE output policy as one state mutation. Keeping the
    /// settings together makes changing a zone/range atomic for undo, project
    /// serialization, and engine snapshot rebuilds.
    pub fn set_track_mpe_configuration(
        &mut self,
        track_id: &str,
        configuration: MpeTrackConfiguration,
    ) -> bool {
        let configuration = configuration.sanitized();
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.mpe != configuration {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] mpe track={} old={:?} new={:?}",
                        track_id, t.routing.mpe, configuration
                    );
                }
                t.routing.mpe = configuration;
                return true;
            }
        }
        false
    }

    /// Set the track's bank/program selection as one mutation, so format,
    /// bank and program change together for undo, save and the engine.
    pub fn set_track_program_selection(
        &mut self,
        track_id: &str,
        selection: MidiProgramSelection,
    ) -> bool {
        let selection = selection.sanitized();
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.routing.program != selection {
                if routing_debug_enabled() {
                    eprintln!(
                        "[routing] program track={} old={:?} new={:?}",
                        track_id, t.routing.program, selection
                    );
                }
                t.routing.program = selection;
                return true;
            }
        }
        false
    }

    /// Add an aux send from `track_id` to the first Bus/Return track that
    /// isn't already a target (Phase 3 — a richer target picker is a follow-up,
    /// mirroring how inserts auto-seeded before the picker overlay). Returns
    /// the new send id, or `None` if there is no eligible routing track or the
    /// track already sends to every routing track.
    pub fn add_send(&mut self, track_id: &str) -> Option<String> {
        let existing: Vec<String> = self
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.sends.iter().map(|s| s.target_track_id.clone()).collect())
            .unwrap_or_default();
        let target = self.tracks.iter().find(|t| {
            t.id != track_id && is_project_routing_track(t) && !existing.contains(&t.id)
        })?;
        let target_id = target.id.clone();
        self.add_send_to_target(track_id, &target_id)
    }

    pub fn add_send_to_target(&mut self, track_id: &str, target_track_id: &str) -> Option<String> {
        if track_id == target_track_id {
            return None;
        }
        let (target_id, target_name) = self
            .tracks
            .iter()
            .find(|t| t.id == target_track_id && is_project_routing_track(t))
            .map(|target| (target.id.clone(), target.name.clone()))?;

        let track = self.tracks.iter_mut().find(|t| t.id == track_id)?;
        // Allow bus/return → bus/return chains; engine rejects cycles at plan time.
        // VSTi multi-out children remain send sources as before.
        if track.sends.iter().any(|s| s.target_track_id == target_id) {
            return None;
        }
        let send_id = format!("send-{}-{}", track.id, track.sends.len() + 1);
        track.sends.push(SendSlotState {
            id: send_id.clone(),
            target_track_id: target_id.clone(),
            target_name,
            enabled: true,
            pre_fader: false,
            gain_db: 0.0,
        });
        if routing_debug_enabled() {
            eprintln!(
                "[routing] add_send track={} send={} -> {}",
                track_id, send_id, target_id
            );
        }
        Some(send_id)
    }

    pub fn create_return_and_send(&mut self, track_id: &str) -> Option<(String, String)> {
        if !self.tracks.iter().any(|track| track.id == track_id) {
            return None;
        }
        let next_return = self
            .tracks
            .iter()
            .filter(|track| track.track_type == TrackType::Return)
            .count()
            + 1;
        let return_id = self.create_track(CreateTrackOptions {
            track_type: TrackType::Return,
            name: format!("Return {next_return}"),
            color: self.track_color_for_index(self.tracks.len()),
            volume: volume::db_to_norm(0.0),
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let send_id = self.add_send_to_target(track_id, &return_id)?;
        Some((return_id, send_id))
    }

    /// Create a mixer-only Bus immediately (no Add Track dialog). Optionally
    /// routes the given tracks' main outs into the new bus.
    pub fn create_bus_track(&mut self, route_from_track_ids: &[String]) -> String {
        let next_bus = self
            .tracks
            .iter()
            .filter(|track| track.track_type == TrackType::Bus)
            .count()
            + 1;
        let bus_id = self.create_track(CreateTrackOptions {
            track_type: TrackType::Bus,
            name: format!("Bus {next_bus}"),
            color: self.track_color_for_index(self.tracks.len()),
            volume: volume::db_to_norm(0.0),
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        for track_id in route_from_track_ids {
            if track_id == &bus_id {
                continue;
            }
            let Some(track) = self.find_track(track_id) else {
                continue;
            };
            if track.track_type.is_routing() || track.track_type == TrackType::Master {
                continue;
            }
            self.set_track_output_routing(
                track_id,
                TrackOutputRouting::Bus {
                    bus_id: bus_id.clone(),
                },
            );
        }
        self.select_track(&bus_id);
        bus_id
    }

    pub fn remove_send(&mut self, track_id: &str, send_id: &str) {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.sends.retain(|s| s.id != send_id);
            if routing_debug_enabled() {
                eprintln!("[routing] remove_send track={} send={}", track_id, send_id);
            }
        }
    }

    pub fn set_send_gain_db(&mut self, track_id: &str, send_id: &str, gain_db: f32) -> bool {
        let Some(send) = self
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .and_then(|track| track.sends.iter_mut().find(|send| send.id == send_id))
        else {
            return false;
        };
        let next = gain_db.clamp(-60.0, 6.0);
        if (send.gain_db - next).abs() <= 1.0e-4 {
            return false;
        }
        send.gain_db = next;
        true
    }

    pub fn send_order(&self, track_id: &str) -> Vec<String> {
        self.tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.sends.iter().map(|send| send.id.clone()).collect())
            .unwrap_or_default()
    }

    pub fn set_send_order(&mut self, track_id: &str, ordered_ids: &[String]) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return false;
        };
        let before: Vec<String> = track.sends.iter().map(|send| send.id.clone()).collect();
        let mut remaining = std::mem::take(&mut track.sends);
        let mut reordered = Vec::with_capacity(remaining.len());
        for wanted in ordered_ids {
            if let Some(pos) = remaining.iter().position(|send| send.id == *wanted) {
                reordered.push(remaining.remove(pos));
            }
        }
        reordered.append(&mut remaining);
        let after: Vec<String> = reordered.iter().map(|send| send.id.clone()).collect();
        track.sends = reordered;
        before != after
    }

    pub fn reordered_send_ids(
        ids: &[String],
        dragged_send_id: &str,
        insertion_index: usize,
    ) -> Vec<String> {
        let Some(origin) = ids.iter().position(|id| id == dragged_send_id) else {
            return ids.to_vec();
        };
        let dragged = ids[origin].clone();
        let mut remaining = ids.to_vec();
        remaining.remove(origin);
        let target = if insertion_index > origin {
            insertion_index.saturating_sub(1)
        } else {
            insertion_index
        }
        .min(remaining.len());
        remaining.insert(target, dragged);
        remaining
    }

    pub fn toggle_send_enabled(&mut self, track_id: &str, send_id: &str) -> Option<bool> {
        let track = self.tracks.iter_mut().find(|t| t.id == track_id)?;
        let send = track.sends.iter_mut().find(|s| s.id == send_id)?;
        send.enabled = !send.enabled;
        Some(send.enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_track(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: volume::db_to_norm(0.0),
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    #[test]
    fn audio_input_assignment_stores_only_the_connection_id() {
        use crate::audio_connections::AudioConnectionId;

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = create_track(&mut state, TrackType::Audio, "Audio");
        let connection = AudioConnectionId::from_stored("ac-test-mic");

        assert!(state.set_track_audio_input_connection(&track_id, Some(connection.clone())));
        assert_eq!(
            state
                .find_track(&track_id)
                .unwrap()
                .routing
                .audio_input_connection_id
                .as_ref(),
            Some(&connection)
        );
        // Re-assigning the same connection is not a change.
        assert!(!state.set_track_audio_input_connection(&track_id, Some(connection.clone())));

        // Changing the track format no longer clears the assignment: channel
        // compatibility is the registry's concern, not the track's. (Audio
        // tracks already default to Stereo, so move to Mono to force a real
        // change.)
        assert!(state.set_track_audio_format(&track_id, TrackAudioFormat::Mono));
        assert_eq!(
            state
                .find_track(&track_id)
                .unwrap()
                .routing
                .audio_input_connection_id
                .as_ref(),
            Some(&connection)
        );

        assert!(state.set_track_audio_input_connection(&track_id, None));
        assert!(state
            .find_track(&track_id)
            .unwrap()
            .routing
            .audio_input_connection_id
            .is_none());
    }

    #[test]
    fn removing_a_connection_unassigns_exactly_the_tracks_that_used_it() {
        use crate::audio_connections::AudioConnectionId;

        let mut state = TimelineState::default();
        state.tracks.clear();
        let a = create_track(&mut state, TrackType::Audio, "A");
        let b = create_track(&mut state, TrackType::Audio, "B");
        let c = create_track(&mut state, TrackType::Audio, "C");
        let mic = AudioConnectionId::from_stored("ac-mic");
        let guitar = AudioConnectionId::from_stored("ac-guitar");

        state.set_track_audio_input_connection(&a, Some(mic.clone()));
        state.set_track_audio_input_connection(&b, Some(mic.clone()));
        state.set_track_audio_input_connection(&c, Some(guitar.clone()));

        let mut users = state.tracks_using_audio_connection(&mic);
        users.sort();
        assert_eq!(users, vec![a.as_str(), b.as_str()]);

        let mut affected = state.unassign_audio_connection(&mic);
        affected.sort();
        assert_eq!(affected, vec![a.clone(), b.clone()]);
        assert!(state
            .find_track(&a)
            .unwrap()
            .routing
            .audio_input_connection_id
            .is_none());
        assert_eq!(
            state
                .find_track(&c)
                .unwrap()
                .routing
                .audio_input_connection_id
                .as_ref(),
            Some(&guitar),
            "an unrelated track keeps its assignment"
        );
    }

    // ── Master / Monitor output routing ──────────────────────────────────────

    #[test]
    fn master_and_monitor_outputs_store_only_connection_ids_and_stay_independent() {
        let mut state = TimelineState::default();
        let main = AudioConnectionId::from_stored("ac-main");
        let headphones = AudioConnectionId::from_stored("ac-headphones");

        assert!(state.set_master_output_connection(Some(main.clone())));
        assert_eq!(state.master_output_connection_id.as_ref(), Some(&main));
        assert!(!state.set_master_output_connection(Some(main.clone())));

        // Monitor's default is Follow Master Output, which resolves to Master.
        assert!(state.monitor_output_connection_id.is_none());
        assert_eq!(
            state.effective_monitor_output_connection(),
            Some(main.clone())
        );

        // Overriding Monitor must not disturb Master.
        assert!(state.set_monitor_output_connection(Some(headphones.clone())));
        assert_eq!(
            state.effective_monitor_output_connection(),
            Some(headphones.clone())
        );
        assert_eq!(
            state.master_output_connection_id.as_ref(),
            Some(&main),
            "changing the Monitor output must never change Master"
        );

        // Back to Follow Master Output.
        assert!(state.set_monitor_output_connection(None));
        assert_eq!(state.effective_monitor_output_connection(), Some(main));
    }

    /// With no Master output and no override there is no destination at all —
    /// the correct answer is silence, not a substituted one.
    #[test]
    fn no_master_output_and_no_override_resolves_to_nothing() {
        let state = TimelineState::default();
        assert!(state.master_output_connection_id.is_none());
        assert!(state.monitor_output_connection_id.is_none());
        assert!(state.effective_monitor_output_connection().is_none());
    }

    #[test]
    fn references_name_track_master_and_monitor_users_of_a_connection() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = create_track(&mut state, TrackType::Audio, "Audio");
        let main = AudioConnectionId::from_stored("ac-main");
        let headphones = AudioConnectionId::from_stored("ac-headphones");

        state.set_track_audio_input_connection(&track_id, Some(main.clone()));
        state.set_master_output_connection(Some(main.clone()));
        state.set_monitor_output_connection(Some(headphones.clone()));

        assert_eq!(
            state.audio_connection_references(&main),
            vec![
                AudioConnectionReference::TrackInput(track_id),
                AudioConnectionReference::MasterOutput,
            ]
        );
        assert_eq!(
            state.audio_connection_references(&headphones),
            vec![AudioConnectionReference::MonitorOutput]
        );
        assert!(state
            .audio_connection_references(&AudioConnectionId::from_stored("ac-unused"))
            .is_empty());
    }

    #[test]
    fn removing_the_master_output_clears_master_and_leaves_monitor_alone() {
        let mut state = TimelineState::default();
        let main = AudioConnectionId::from_stored("ac-main");
        let headphones = AudioConnectionId::from_stored("ac-headphones");
        state.set_master_output_connection(Some(main.clone()));
        state.set_monitor_output_connection(Some(headphones.clone()));

        assert_eq!(
            state.unassign_output_connection(&main),
            vec![AudioConnectionReference::MasterOutput]
        );
        assert!(state.master_output_connection_id.is_none());
        assert_eq!(
            state.monitor_output_connection_id.as_ref(),
            Some(&headphones),
            "the Monitor override is untouched"
        );
    }

    /// Removing the bus a Monitor override points at returns Monitor to Follow
    /// Master Output — it never silently picks another output.
    #[test]
    fn removing_the_monitor_override_returns_monitor_to_follow_master() {
        let mut state = TimelineState::default();
        let main = AudioConnectionId::from_stored("ac-main");
        let headphones = AudioConnectionId::from_stored("ac-headphones");
        state.set_master_output_connection(Some(main.clone()));
        state.set_monitor_output_connection(Some(headphones.clone()));

        assert_eq!(
            state.unassign_output_connection(&headphones),
            vec![AudioConnectionReference::MonitorOutput]
        );
        assert!(state.monitor_output_connection_id.is_none());
        assert_eq!(
            state.effective_monitor_output_connection(),
            Some(main),
            "Follow Master Output, not a replacement bus"
        );
    }

    #[test]
    fn sends_ignore_vsti_multiout_child_tracks() {
        let mut state = TimelineState::default();
        state.tracks.clear();

        let source_id = create_track(&mut state, TrackType::Audio, "Audio");
        let child_id = create_track(&mut state, TrackType::Bus, "VSTi Out 1");
        state
            .tracks
            .iter_mut()
            .find(|track| track.id == child_id)
            .unwrap()
            .id = vsti_output_child_track_id("insert-track-1-1", 0);
        let child_id = vsti_output_child_track_id("insert-track-1-1", 0);
        let return_id = create_track(&mut state, TrackType::Return, "Return 1");

        assert!(!is_project_routing_track(
            state.find_track(&child_id).unwrap()
        ));
        assert!(is_project_routing_track(
            state.find_track(&return_id).unwrap()
        ));
        assert!(state.add_send_to_target(&source_id, &child_id).is_none());

        let send_id = state
            .add_send(&source_id)
            .expect("real return should be selected");
        let source = state.find_track(&source_id).unwrap();
        let send = source.sends.iter().find(|send| send.id == send_id).unwrap();
        assert_eq!(send.target_track_id, return_id);

        let child_send_id = state
            .add_send_to_target(&child_id, &return_id)
            .expect("VSTi output child may send to a project routing track");
        let child = state.find_track(&child_id).unwrap();
        assert!(child.sends.iter().any(|send| send.id == child_send_id));

        let return_count_before = state
            .tracks
            .iter()
            .filter(|track| track.track_type == TrackType::Return)
            .count();
        let (created_return_id, created_send_id) = state
            .create_return_and_send(&child_id)
            .expect("VSTi output child may create a return and send to it");
        assert_eq!(
            state
                .tracks
                .iter()
                .filter(|track| track.track_type == TrackType::Return)
                .count(),
            return_count_before + 1
        );
        let child = state.find_track(&child_id).unwrap();
        assert!(child.sends.iter().any(|send| {
            send.id == created_send_id && send.target_track_id == created_return_id
        }));
    }

    #[test]
    fn send_gain_updates_and_clamps_in_db() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let source_id = create_track(&mut state, TrackType::Audio, "Audio");
        let return_id = create_track(&mut state, TrackType::Return, "Return");
        let send_id = state
            .add_send_to_target(&source_id, &return_id)
            .expect("send");

        assert!(state.set_send_gain_db(&source_id, &send_id, -12.5));
        assert_eq!(
            state.find_track(&source_id).unwrap().sends[0].gain_db,
            -12.5
        );
        assert!(state.set_send_gain_db(&source_id, &send_id, 30.0));
        assert_eq!(state.find_track(&source_id).unwrap().sends[0].gain_db, 6.0);
        assert!(!state.set_send_gain_db(&source_id, &send_id, 6.0));
    }

    #[test]
    fn create_bus_routes_sources_and_hides_from_arrangement() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let audio_id = create_track(&mut state, TrackType::Audio, "Drums");
        let bus_id = state.create_bus_track(std::slice::from_ref(&audio_id));
        let bus = state.find_track(&bus_id).unwrap();
        assert_eq!(bus.track_type, TrackType::Bus);
        assert!(is_arrangement_hidden_track(bus));
        assert_eq!(
            state.find_track(&audio_id).unwrap().routing.output,
            TrackOutputRouting::Bus {
                bus_id: bus_id.clone()
            }
        );
        let layout = state.track_row_layout();
        assert_eq!(layout.row_for_track(&bus_id).unwrap().height, 0.0);
        assert!(layout.row_for_track(&audio_id).unwrap().height > 0.0);
    }
}
