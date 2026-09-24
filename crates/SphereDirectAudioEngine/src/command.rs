use crate::runtime::RuntimeProject;

/// Commands sent from the control thread to the audio callback via a
/// lock-free bounded channel.  The audio callback drains these with
/// `try_recv()` at the top of each block — no blocking, no allocation.
#[derive(Debug)]
pub enum EngineCommand {
    /// Replace the callback's render graph with a fully prepared project.
    LoadProject(Box<RuntimeProject>),
    /// Enable or disable the sine test tone.
    SetTestTone { enabled: bool, frequency: f32 },
    /// Set master output gain (linear, 0..2).
    SetMasterVolume { value: f32 },
    /// Replace the Control Room's monitor source. The control thread resolves
    /// nothing here — `RuntimeProject::resolve_indices` maps the id to an
    /// index when the command is applied, so the callback never hashes a
    /// string per block.
    SetMonitorSource {
        source: crate::monitor::MonitorSource,
    },
    /// Set the Control Room's level/shape controls (gain, mute, dim, mono).
    /// Playback-only: never reaches export or recording.
    SetMonitorControl {
        control: crate::monitor::MonitorControl,
    },
    /// Select the hardware output pair the Control Room feeds.
    SetMonitorOutput {
        target: crate::monitor::MonitorOutputTarget,
    },
    /// Publish the compiled hardware-output ownership: which stage writes the
    /// device, Master's own destination pair, and the effective monitoring pair.
    /// Fixed size and allocation-free, because ownership and port resolution
    /// were both decided on the control thread.
    SetHardwareOutputOwnership {
        owner: crate::monitor::HardwareOutputOwner,
        /// Master's own `(left, right)` device channels, or `None` when Master
        /// has no output destination.
        master: Option<(u16, u16)>,
        /// The effective monitoring `(left, right)` device channels.
        monitor: Option<(u16, u16)>,
    },
    /// Set one channel's Pre/After-Fader Listen state. `track_index` is
    /// resolved by the control thread so the payload owns no allocation.
    SetTrackListen {
        track_index: usize,
        listen: crate::monitor::ListenMode,
    },
    /// Clear Listen on every channel, returning the Control Room to its
    /// selected source (normally the master bus).
    ClearAllListen,
    /// Set a track's gain (linear, 0..2).
    SetTrackVolume { track_id: String, value: f32 },
    /// Set a track's pan (-1..1).
    SetTrackPan { track_id: String, value: f32 },
    /// Mute or unmute a track.
    SetTrackMute { track_id: String, muted: bool },
    /// Solo or unsolo a track.
    SetTrackSolo { track_id: String, solo: bool },
    /// Update record-arm, monitoring, and input-channel routing without
    /// replacing the project graph. The control thread resolves the stable
    /// track id to an index before enqueueing so the callback payload is fixed
    /// size and owns no heap allocation.
    SetTrackInputState {
        track_index: usize,
        record_armed: bool,
        monitor_enabled: bool,
        input_source: crate::runtime::RuntimeTrackInputSource,
    },
    /// Route a track or bus into an Audio Jam publish slot, or stop.
    ///
    /// A publish is a session-scoped decision, not project state, so it arrives
    /// as its own command rather than through a graph rebuild — a performer
    /// starting to share a bus must not cost the room a reload. The control
    /// thread resolves both the track id and the slot before enqueueing, so the
    /// callback only ever writes to an index.
    /// Whether this track keeps a copy of its post-fader block for a track
    /// that reads it as input.
    ///
    /// Separate from the input-route command because it lands on a *different*
    /// track: routing B's input to A changes what B reads and what A has to
    /// keep, and the callback's graph has to be told both.
    SetTrackLoopbackPublish { track_index: usize, publish: bool },
    SetTrackJamPublish {
        track_index: usize,
        slot: Option<u32>,
    },
    /// Assign the channel pairs of the Audio Jam multitrack stream.
    ///
    /// `pairs[k]` is the track index filling pair `k`, and [`NO_JAM_PAIR`]
    /// marks a pair nobody fills. A fixed array rather than a `Vec` because the
    /// callback applies this command and must not free a heap allocation; the
    /// whole assignment is replaced at once because a stream's channel layout
    /// is announced to receivers once and cannot be edited underneath them.
    SetJamMultitrackPairs {
        pairs: [u32; crate::jam_bus::MAX_MULTITRACK_PAIRS],
    },
    /// Set non-destructive stereo/mono/mid/side monitoring preview.
    SetTrackPreviewMode { track_id: String, value: f32 },
    /// Set a plugin/insert parameter.
    SetInsertParam {
        track_id: String,
        insert_id: String,
        param_id: String,
        value: f32,
    },
    /// Immediate MIDI preview note-on from the UI piano roll. Bypasses timeline
    /// scheduling and can render while transport is stopped.
    MidiPreviewNoteOn {
        track_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    /// Immediate MIDI preview note-off from the UI piano roll.
    MidiPreviewNoteOff {
        track_id: String,
        channel: u8,
        pitch: u8,
    },
    /// Immediate MIDI CC preview from the UI/control MIDI input router.
    MidiPreviewControlChange {
        track_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    },
    /// Panic/cleanup for preview notes on one track.
    MidiPreviewAllNotesOff { track_id: String },
    /// Sample-synchronous bridged-plugin preview note-on (audio callback writes
    /// into the shared MIDI ring before the next process block).
    PluginPreviewNoteOn {
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    PluginPreviewNoteOff {
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
    },
    PluginPreviewControlChange {
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    },
    PluginPreviewAllNotesOff {
        track_id: String,
        plugin_instance_id: String,
    },
    /// Immediate 14-bit pitch bend (`8192` = centre). Carried apart from the
    /// CC commands so the bend keeps its full resolution; an empty
    /// `plugin_instance_id` targets the track's in-process instrument.
    PluginPreviewPitchBend {
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        value: u16,
    },
    /// Start transport (playback) from current position.
    StartTransport,
    /// Stop transport (but keep position).
    StopTransport,
    /// Seek transport to an absolute position in seconds.
    Seek { position_seconds: f64 },
    /// Suspend metronome click scheduling during playhead scrubbing.
    SetMetronomeSuspended(bool),
    /// Enable or disable generated metronome clicks.
    SetMetronomeEnabled(bool),
    /// Click level and timbre from Settings → Recording → Metronome. `volume`
    /// is a linear multiplier on the click's own gain; `sound` is a
    /// [`crate::backend::render::MetronomeSound`] code (the same
    /// code-in-a-command shape `SetTrackPreviewMode` uses).
    SetMetronomeVoice { volume: f32, sound: u8 },
    /// Start an audible record count-in: `beats` clicks, `samples_per_beat`
    /// apart, accented every `beats_per_bar`. Runs with the transport parked —
    /// see [`crate::backend::render::LocalAudioState::begin_count_in`].
    StartCountIn {
        beats: u32,
        beats_per_bar: u32,
        samples_per_beat: u64,
    },
    /// Abandon a count-in in progress.
    CancelCountIn,
    /// Set project tempo for metronome scheduling (static tempo shortcut).
    SetBpm(f64),
    /// Replace the authoritative tempo map used for beat/time/sample conversion.
    SetTempoMap(crate::tempo_map::RuntimeTempoMapSnapshot),
    /// Set project time signature for metronome accent scheduling.
    SetTimeSignature(u32, u32),
    /// Replace the authoritative time-signature map for metronome accents.
    SetTimeSignatureMap(crate::time_signature_map::RuntimeTimeSignatureMapSnapshot),
    /// Enable/disable loop region and set its bounds in seconds.
    SetLoop {
        enabled: bool,
        start_seconds: f64,
        end_seconds: f64,
    },
    /// Stage 3b: install (or clear, with `sink = None`) the realtime
    /// plugin-bridge sink for `insert_id` (one shared-memory region + handshake
    /// per insert so serial FX chains do not share `request_seq`/`done_seq`).
    SetPluginBridgeSink {
        insert_id: String,
        sink: Option<std::sync::Arc<dyn crate::plugin_bridge::PluginBridgeSink>>,
    },
    /// Install the ARA playback renderers for `track_id`, replacing whatever was
    /// there. The list is built and ARA-bound on the control thread — the
    /// callback only swaps the vector in and retires the old one off-thread,
    /// because dropping the last handle to a bound instance destroys a C++ VST3
    /// processor.
    SetAraRenderers {
        track_id: String,
        renderers: Vec<crate::runtime::RuntimeAraRenderer>,
    },
    /// Play a pre-decoded standalone browser sample through the master output.
    /// `token` is the `SharedState::audition_request_token` value this decode
    /// was started for; the callback drops the source when a newer selection
    /// has since been made, so a slow decode can never resurrect a sample the
    /// user already moved past.
    StartAudition {
        token: u64,
        source: Box<crate::audio_file::AudioFileBuffer>,
    },
    /// Stop the current standalone browser sample audition.
    StopAudition,
    /// Keep rendering a bridged track while its plugin editor is open (VSTi
    /// internal keyboard / groove preview needs a live DSP loop).
    SetBridgeEditorActive { track_id: String, active: bool },
    /// Control-thread synchronization point: the callback sets `ack` when it
    /// drains this command, proving every command sent *before* it has been
    /// applied. Used by offline export to hand bridge-sink ownership across
    /// deterministically instead of sleeping a guessed interval. The handler is
    /// one atomic store — wait-free; only the control side ever waits.
    CommandBarrier {
        ack: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
}
