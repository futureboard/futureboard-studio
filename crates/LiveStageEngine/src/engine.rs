//! The control side: owns the [`Session`], turns edits into graph swaps or
//! atomic stores, and runs the device, the recorder and the plug-in host.
//!
//! Single-threaded by design — the GUI's main thread or the server's command
//! loop owns it. Only the audio callback runs elsewhere, and it sees nothing
//! but finished [`Graph`]s and the shared cells inside them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};

use crate::builtin_fx::BuiltinFx;
use crate::device::{self, DeviceStatus, DeviceStreams};
use crate::graph::{
    Dest, Graph, InjectTo, InsertCell, InsertDsp, MixFrom, MonitorShared, OscillatorCell,
    PatchFrom, ProcessorCell, ProcessorSender, RecordStream, RtBus, RtChannel, RtMatrix,
    RtMatrixSource, RtMonitor, RtOscillator, RtPatch, RtSend, RtStrip, RtTalkback, SendGains,
    SendTap, StripShared, TalkbackCell,
};
use crate::graph::{PlaybackTap, RtPlayback};
use crate::history::{EditTarget, History, Snapshot, UndoHistory};
use crate::playback::{PlaybackStatus, Player, Transport, assign_tracks, probe_take};
use crate::processing::Processing;
use crate::recorder::{Recording, RecordingSummary};
use crate::scene::{MixParts, RecallReport, recall_into, same_plugin};
use crate::session::{
    AudioSettings, BusRole, BusStrip, ChannelStrip, DCA_COUNT, Id, InjectDest, InputPatch,
    InsertPlugin, InsertSlot, Layer, MAX_FADER_DB, MAX_LAYERS, MIN_FADER_DB, MONITOR_DIM_DB,
    MUTE_GROUP_COUNT, MatrixFeed, MatrixSource, MatrixStrip, MixState, MonitorSettings,
    MonitorSource, OSCILLATOR_MAX_DB, OSCILLATOR_MAX_HZ, OSCILLATOR_MIN_DB, OSCILLATOR_MIN_HZ,
    OscillatorSettings, OutputPatch, PALETTE_COLORS, PatchSource, RecallScope, RecordSettings,
    RecordTap, Scene, Section, SendSlot, Session, SoloMode, StripOutput, StripRef, StripSettings,
    TalkbackSettings, clamp_processing, db_to_gain, processing_is_finite,
};

/// Every edit the engine takes. The GUI calls [`LiveEngine::apply`] with
/// these, and the headless server reads them as JSON lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    SetFader {
        strip: StripRef,
        db: f32,
    },
    SetPan {
        strip: StripRef,
        pan: f32,
    },
    SetMute {
        strip: StripRef,
        mute: bool,
    },
    SetSolo {
        strip: StripRef,
        solo: bool,
    },
    SetTrim {
        channel: Id,
        db: f32,
    },
    SetPhaseInvert {
        channel: Id,
        invert: bool,
    },
    AddChannel {
        name: String,
        input: InputPatch,
    },
    RemoveChannel {
        channel: Id,
    },
    RenameStrip {
        strip: StripRef,
        name: String,
    },
    SetChannelInput {
        channel: Id,
        input: InputPatch,
    },
    SetStripOutput {
        strip: StripRef,
        output: StripOutput,
    },
    /// A bus of `role` (default group), stereo unless `stereo` is false.
    /// An aux bus starts with no output; a group or FX bus feeds the master.
    AddBus {
        name: String,
        #[serde(default)]
        role: Option<BusRole>,
        #[serde(default)]
        stereo: Option<bool>,
    },
    RemoveBus {
        bus: Id,
    },
    SetBusRole {
        bus: Id,
        role: BusRole,
    },
    SetBusStereo {
        bus: Id,
        stereo: bool,
    },
    /// Make or change `channel`'s send to `bus`. Omitted fields stay as
    /// they are; a new send takes its pre/post from the bus's role (aux:
    /// pre-fader, else post) and follows the channel's pan.
    SetSend {
        channel: Id,
        bus: Id,
        level_db: f32,
        #[serde(default)]
        pre_fader: Option<bool>,
        #[serde(default)]
        pan: Option<f32>,
        #[serde(default)]
        pan_follow: Option<bool>,
    },
    RemoveSend {
        channel: Id,
        bus: Id,
    },
    AddInsert {
        strip: StripRef,
        plugin: InsertPlugin,
        index: Option<usize>,
    },
    RemoveInsert {
        strip: StripRef,
        insert: Id,
    },
    MoveInsert {
        strip: StripRef,
        insert: Id,
        to: usize,
    },
    SetInsertBypass {
        insert: Id,
        bypass: bool,
    },
    SetInsertParam {
        insert: Id,
        index: u32,
        value: f32,
    },
    /// Several wire values at once, in order: a preset, an A/B swap.
    SetInsertParams {
        insert: Id,
        values: Vec<(u32, f32)>,
    },
    SetOutputPatch {
        patches: Vec<OutputPatch>,
    },
    SetRecordArm {
        strip: StripRef,
        arm: bool,
    },
    SetRecordSettings {
        settings: RecordSettings,
    },
    SetAudio {
        settings: AudioSettings,
    },
    StartRecording,
    StopRecording,
    /// The strip's whole processing section (HPF, gate, EQ, comp, delay).
    SetProcessing {
        strip: StripRef,
        processing: Processing,
    },
    /// A palette index (`0 .. PALETTE_COLORS`), or `None` for the default.
    SetStripColor {
        strip: StripRef,
        color: Option<u8>,
    },
    SetSoloSafe {
        strip: StripRef,
        safe: bool,
    },
    /// Unsolo every channel and bus.
    ClearSolo,
    AssignDca {
        strip: StripRef,
        dca: usize,
        assigned: bool,
    },
    SetDcaLevel {
        dca: usize,
        db: f32,
    },
    SetDcaMute {
        dca: usize,
        mute: bool,
    },
    RenameDca {
        dca: usize,
        name: String,
    },
    SetDcaColor {
        dca: usize,
        color: Option<u8>,
    },
    AssignMuteGroup {
        strip: StripRef,
        group: usize,
        assigned: bool,
    },
    SetMuteGroup {
        group: usize,
        active: bool,
    },
    RenameMuteGroup {
        group: usize,
        name: String,
    },
    SetMonitor {
        monitor: MonitorSettings,
    },
    /// Show `channel` at `index` (clamped to the end) in the channel order.
    MoveChannel {
        channel: Id,
        index: usize,
    },
    /// Show `bus` at `index` (clamped to the end) in the bus order.
    MoveBus {
        bus: Id,
        index: usize,
    },
    /// Take back the last undoable edit.
    Undo,
    /// Make the last undone edit again.
    Redo,
    /// Store the mix: `id` overwrites that scene (keeping its scope, and its
    /// name unless one is given); no `id` adds a scene after the current
    /// one.
    SceneStore {
        #[serde(default)]
        id: Option<Id>,
        #[serde(default)]
        name: Option<String>,
    },
    SceneRecall {
        id: Id,
    },
    SceneRename {
        id: Id,
        name: String,
    },
    SceneNote {
        id: Id,
        note: String,
    },
    SceneScope {
        id: Id,
        scope: RecallScope,
    },
    SceneDelete {
        id: Id,
    },
    /// Show the scene at `index` (clamped to the end) in the list.
    SceneMove {
        id: Id,
        index: usize,
    },
    SetRecallSafe {
        strip: StripRef,
        safe: bool,
    },
    /// Apply `sections` of `settings` to every target.
    PasteStrip {
        targets: Vec<StripRef>,
        settings: StripSettings,
        sections: Vec<Section>,
    },
    AddMatrix {
        name: String,
        #[serde(default)]
        stereo: Option<bool>,
    },
    RemoveMatrix {
        matrix: Id,
    },
    /// Set (or add) a matrix's contribution from the master or a bus.
    /// `pan` omitted stays as it is (centre for a new source).
    SetMatrixSend {
        matrix: Id,
        source: MatrixFeed,
        level_db: f32,
        #[serde(default)]
        pan: Option<f32>,
    },
    /// Show `matrix` at `index` (clamped to the end) in the matrix order.
    MoveMatrix {
        matrix: Id,
        index: usize,
    },
    SetMatrixStereo {
        matrix: Id,
        stereo: bool,
    },
    /// The talkback setup (input, level, HPF, destinations). Not undoable.
    SetTalkback {
        talkback: TalkbackSettings,
    },
    /// Talk (or stop). Live state, not saved.
    Talk {
        active: bool,
    },
    /// The oscillator setup (kind, frequency, level, destinations). Not
    /// undoable.
    SetOscillator {
        oscillator: OscillatorSettings,
    },
    /// The oscillator on or off (ramped). Live state, not saved.
    OscillatorOn {
        on: bool,
    },
    /// Set custom layer `index` (`0 ..` [`MAX_LAYERS`]); `index` = the
    /// number of layers adds one.
    SetLayer {
        index: usize,
        name: String,
        strips: Vec<StripRef>,
    },
    RemoveLayer {
        index: usize,
    },
    /// Load the take in `folder` for playback: each file's header is read,
    /// a take at another sample rate than the engine's is refused, and each
    /// file goes to the channel whose name gives its file name (as the
    /// recorder names them). Loading the take already loaded keeps its
    /// assignments. Virtual soundcheck turns off. Not undoable.
    PlaybackLoad {
        folder: std::path::PathBuf,
    },
    /// Feed `channel` (or nothing) from `file` of the loaded take; a
    /// channel takes one file at most. Not undoable.
    PlaybackAssign {
        file: String,
        channel: Option<Id>,
    },
    PlaybackUnload,
    /// The transport: play, pause, stop (back to the start). Live state.
    Playback {
        action: Transport,
    },
    /// Go to `seconds` from the start of the take. Live state.
    PlaybackLocate {
        seconds: f64,
    },
    /// Repeat the whole take. Live state.
    PlaybackLoop {
        on: bool,
    },
    /// Every channel with a file assigned takes its input from it, in place
    /// of the interface (crossfaded). Saved, but off after a show loads.
    SetVirtualSoundcheck {
        on: bool,
    },
}

/// Levels since the last [`LiveEngine::meters`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct StripLevels {
    /// Post-fader output.
    pub output: (f32, f32),
    /// Channel input after trim (zero for buses and the master).
    pub input: (f32, f32),
    /// The processing section's gate is open (or off), as last measured.
    pub gate_open: bool,
    /// The most the gate took off since the last read, dB, ≥ 0.
    pub gate_db: f32,
    /// The most the compressor took off since the last read, dB, ≥ 0.
    pub comp_db: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub running: bool,
    pub sample_rate: u32,
    pub in_channels: usize,
    pub out_channels: usize,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub load: f32,
    /// The audio threads run at real-time priority: `None` until the device
    /// has called back (or off Linux, where the system raises its own).
    pub realtime: Option<bool>,
    pub input_underruns: u64,
    pub error: Option<String>,
    pub recording_seconds: Option<f64>,
    pub recording_dropped: u64,
    /// Talk is pressed (or latched).
    pub talkback_active: bool,
    pub oscillator_on: bool,
    /// The loaded take's transport.
    pub playback: PlaybackStatus,
}

/// Where a third-party insert stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum InsertState {
    Ready,
    Loading,
    Failed(String),
}

struct InsertEntry {
    cell: Arc<InsertCell>,
    params: Sender<(u32, f32)>,
}

struct ProcessorEntry {
    cell: Arc<ProcessorCell>,
    settings: ProcessorSender,
}

pub struct LiveEngine {
    session: Session,
    status: Arc<DeviceStatus>,
    streams: Option<DeviceStreams>,
    open_error: Option<String>,
    sample_rate: u32,
    in_channels: usize,
    out_channels: usize,
    graph_tx: Sender<Box<Graph>>,
    garbage_rx: Receiver<Box<Graph>>,
    /// Graph generation the audio thread last adopted.
    adopted: Arc<AtomicU64>,
    generation: u64,
    strips: HashMap<StripRef, Arc<StripShared>>,
    /// `(channel, bus)`.
    sends: HashMap<(Id, Id), Arc<SendGains>>,
    /// `(matrix, source)`.
    matrix_sends: HashMap<(Id, MatrixFeed), Arc<SendGains>>,
    inserts: HashMap<Id, InsertEntry>,
    insert_states: HashMap<Id, InsertState>,
    /// Every strip's processing section, kept across graph rebuilds.
    processors: HashMap<StripRef, ProcessorEntry>,
    monitor: Arc<MonitorShared>,
    /// The monitor source the last graph played (to crossfade from).
    monitor_source_built: Option<MonitorSource>,
    talkback: Arc<TalkbackCell>,
    oscillator: Arc<OscillatorCell>,
    /// The rate the talkback and oscillator cells were built for.
    generators_rate: u32,
    /// Live state, never saved: talk pressed, oscillator on.
    talk_active: bool,
    oscillator_on: bool,
    /// The take [`Session::playback`] names, open, with its reader running.
    player: Option<Player>,
    /// Why the session's take could not be opened (a loaded show).
    playback_error: Option<String>,
    recording: Option<Recording>,
    /// Each recorded strip's ring, while recording.
    record_taps: HashMap<StripRef, Arc<RecordStream>>,
    last_recording: Option<RecordingSummary>,
    history: UndoHistory,
    /// Bumps on every change to the session.
    revision: u64,
    /// Bumps on every change to the scene list (or the current scene).
    scenes_revision: u64,
    #[cfg(feature = "external-plugins")]
    external: Option<crate::external::ExternalHost>,
    #[cfg(feature = "external-plugins")]
    external_events: Vec<crate::external::ExternalEvent>,
}

impl LiveEngine {
    /// Open the device the session names and start mixing. A device that
    /// will not open leaves the engine running without audio and the reason
    /// in [`EngineStatus::error`], so the settings can still be changed.
    pub fn new(session: Session) -> Self {
        let (mut engine, graph_rx, garbage_tx) = Self::unopened(session);
        engine.open_device(graph_rx, garbage_tx);
        engine.reopen_playback();
        engine.refresh_gains();
        engine.rebuild();
        engine
    }

    /// The engine before any device: the graph queue's other ends are
    /// returned for the device (or a test) to take.
    fn unopened(mut session: Session) -> (Self, Receiver<Box<Graph>>, Sender<Box<Graph>>) {
        session.normalize();
        let (graph_tx, graph_rx) = crossbeam_channel::bounded::<Box<Graph>>(8);
        let (garbage_tx, garbage_rx) = crossbeam_channel::bounded::<Box<Graph>>(16);
        let engine = Self {
            session,
            status: Arc::new(DeviceStatus::default()),
            streams: None,
            open_error: None,
            sample_rate: 48_000,
            in_channels: 0,
            out_channels: 2,
            graph_tx,
            garbage_rx,
            adopted: Arc::new(AtomicU64::new(0)),
            generation: 0,
            strips: HashMap::new(),
            sends: HashMap::new(),
            matrix_sends: HashMap::new(),
            inserts: HashMap::new(),
            insert_states: HashMap::new(),
            processors: HashMap::new(),
            monitor: Arc::new(MonitorShared::default()),
            monitor_source_built: None,
            talkback: TalkbackCell::new(48_000),
            oscillator: OscillatorCell::new(48_000),
            generators_rate: 48_000,
            talk_active: false,
            oscillator_on: false,
            player: None,
            playback_error: None,
            recording: None,
            record_taps: HashMap::new(),
            last_recording: None,
            history: UndoHistory::default(),
            revision: 0,
            scenes_revision: 0,
            #[cfg(feature = "external-plugins")]
            external: None,
            #[cfg(feature = "external-plugins")]
            external_events: Vec::new(),
        };
        (engine, graph_rx, garbage_tx)
    }

    /// An engine with no device, for tests: `in_channels` inputs and
    /// `out_channels` outputs at 48 kHz, and the queue its graphs arrive on.
    #[cfg(test)]
    fn offline(
        session: Session,
        in_channels: usize,
        out_channels: usize,
    ) -> (Self, Receiver<Box<Graph>>) {
        let (mut engine, graph_rx, _garbage_tx) = Self::unopened(session);
        engine.in_channels = in_channels;
        engine.out_channels = out_channels;
        engine.reopen_playback();
        engine.refresh_gains();
        engine.rebuild();
        (engine, graph_rx)
    }

    fn open_device(&mut self, graph_rx: Receiver<Box<Graph>>, garbage_tx: Sender<Box<Graph>>) {
        self.streams = None;
        self.status = Arc::new(DeviceStatus::default());
        let adopted = self.adopted.clone();
        let mut graph: Option<Box<Graph>> = None;
        let mut pending_garbage: Option<Box<Graph>> = None;
        let render = move |input: &[f32], output: &mut [f32]| {
            if let Some(old) = pending_garbage.take() {
                if let Err(TrySendError::Full(old)) = garbage_tx.try_send(old) {
                    pending_garbage = Some(old);
                }
            }
            while let Ok(next) = graph_rx.try_recv() {
                adopted.store(next.generation, Ordering::Release);
                if let Some(old) = graph.replace(next) {
                    // Freed on the control thread, never here. Only a burst
                    // of swaps faster than the control thread collects can
                    // overflow both slots; that last one is dropped here.
                    if pending_garbage.is_none() {
                        if let Err(TrySendError::Full(old)) = garbage_tx.try_send(old) {
                            pending_garbage = Some(old);
                        }
                    }
                }
            }
            match graph.as_mut() {
                Some(graph) => graph.process(input, output),
                None => output.fill(0.0),
            }
        };
        match device::open(&self.session.audio, self.status.clone(), render) {
            Ok(streams) => {
                self.sample_rate = streams.sample_rate;
                self.in_channels = streams.in_channels;
                self.out_channels = streams.out_channels;
                self.streams = Some(streams);
                self.open_error = None;
            }
            Err(error) => {
                self.open_error = Some(error);
                self.in_channels = 0;
                self.out_channels = 2;
            }
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn input_channels(&self) -> usize {
        self.in_channels
    }

    pub fn output_channels(&self) -> usize {
        self.out_channels
    }

    pub fn insert_state(&self, insert: Id) -> InsertState {
        self.insert_states
            .get(&insert)
            .cloned()
            .unwrap_or(InsertState::Ready)
    }

    /// Whether a built-in insert measures itself for an editor (see
    /// [`crate::telemetry`]). An insert rebuilt since (a new sample rate)
    /// starts unwatched: callers set it again.
    pub fn watch_insert(&self, insert: Id, watch: bool) {
        if let Some(entry) = self.inserts.get(&insert) {
            entry.cell.telemetry.watched.store(watch, Ordering::Relaxed);
        }
    }

    /// What a watched insert measured since the last call.
    pub fn insert_telemetry(&self, insert: Id) -> Option<crate::telemetry::TelemetryFrame> {
        self.inserts
            .get(&insert)
            .map(|entry| entry.cell.telemetry.take())
    }

    pub fn last_recording(&self) -> Option<&RecordingSummary> {
        self.last_recording.as_ref()
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            running: self.streams.is_some(),
            sample_rate: self.sample_rate,
            in_channels: self.in_channels,
            out_channels: self.out_channels,
            input_device: self.streams.as_ref().and_then(|s| s.input_name.clone()),
            output_device: self.streams.as_ref().map(|s| s.output_name.clone()),
            load: self.status.load.load(),
            realtime: match self.status.realtime.load(Ordering::Relaxed) {
                crate::device::REALTIME_YES => Some(true),
                crate::device::REALTIME_NO => Some(false),
                _ => None,
            },
            input_underruns: self.status.input_underruns.load(Ordering::Relaxed),
            error: self
                .open_error
                .clone()
                .or_else(|| self.status.error.lock().clone()),
            recording_seconds: self.recording.as_ref().map(|r| r.elapsed().as_secs_f64()),
            recording_dropped: self
                .recording
                .as_ref()
                .map(|r| r.dropped_samples())
                .unwrap_or(0),
            talkback_active: self.talk_active,
            oscillator_on: self.oscillator_on,
            playback: self.playback_status(),
        }
    }

    /// The loaded take's transport (stopped at 0 with none), and why it did
    /// not load or stopped reading.
    pub fn playback_status(&self) -> PlaybackStatus {
        let mut status = self.player.as_ref().map(Player::status).unwrap_or_default();
        if status.error.is_none() {
            status.error = self.playback_error.clone();
        }
        status
    }

    /// The talkback mic's linear peak since the last call: after its HPF and
    /// level, whether talking or not.
    pub fn talkback_meter(&self) -> f32 {
        self.talkback.meter.take().0
    }

    /// Peaks and processing reductions since the last call, per strip.
    pub fn meters(&self) -> HashMap<StripRef, StripLevels> {
        self.strips
            .iter()
            .map(|(strip, shared)| {
                let processing = self
                    .processors
                    .get(strip)
                    .map(|entry| entry.cell.take_meters())
                    .unwrap_or(crate::processing::ProcessingMeters {
                        gate_open: true,
                        ..Default::default()
                    });
                (
                    *strip,
                    StripLevels {
                        output: shared.meter.take(),
                        input: shared.input_meter.take(),
                        gate_open: processing.gate_open,
                        gate_db: processing.gate_db,
                        comp_db: processing.comp_db,
                    },
                )
            })
            .collect()
    }

    /// The monitor bus's `(left, right)` linear peaks since the last call.
    pub fn monitor_meter(&self) -> (f32, f32) {
        self.monitor.meter.take()
    }

    /// Housekeeping at UI rate: free the graphs the audio thread is done
    /// with, and take in what the plug-in host reported.
    pub fn poll(&mut self) {
        while self.garbage_rx.try_recv().is_ok() {}
        if let Some(player) = &self.player {
            player.poll();
        }
        #[cfg(feature = "external-plugins")]
        self.poll_external();
    }

    /// Apply one edit. See [`Self::apply_with_note`] for what it says back.
    pub fn apply(&mut self, command: Command) -> Result<(), String> {
        self.apply_with_note(command).map(|_| ())
    }

    /// Apply one edit, recording it for undo when it is undoable (see
    /// [`undo_target`]). `Ok(Some(note))` is something the person should
    /// read: what a recall left as it was, or that there was nothing to undo.
    pub fn apply_with_note(&mut self, command: Command) -> Result<Option<String>, String> {
        self.apply_at(command, Instant::now())
    }

    /// [`Self::apply_with_note`] with the edit's time given: what coalescing
    /// measures. Tests pass their own clock.
    pub(crate) fn apply_at(
        &mut self,
        command: Command,
        now: Instant,
    ) -> Result<Option<String>, String> {
        let changes_session = !matches!(
            command,
            Command::StartRecording
                | Command::StopRecording
                | Command::Undo
                | Command::Redo
                | Command::Talk { .. }
                | Command::OscillatorOn { .. }
                | Command::Playback { .. }
                | Command::PlaybackLocate { .. }
                | Command::PlaybackLoop { .. }
        );
        let note = match &command {
            Command::Undo => return self.undo(),
            Command::Redo => return self.redo(),
            _ if !is_undoable(&command) => self.execute(command)?,
            _ => {
                let target = undo_target(&command, &self.session);
                if self.history.coalesces(target.as_ref(), now) {
                    let note = self.execute(command)?;
                    self.history.touch(now);
                    note
                } else {
                    let label = undo_label(&command, &self.session);
                    #[cfg(feature = "external-plugins")]
                    self.capture_before_removal(&command);
                    let before = self.snapshot();
                    let note = self.execute(command)?;
                    // An edit that changed nothing is not a step.
                    if !(self.session.mix_equals(&before.mix)
                        && self.session.name == before.name
                        && self.session.layers == before.layers)
                    {
                        self.history.push(label, before, target, now);
                    }
                    note
                }
            }
        };
        if changes_session {
            self.revision += 1;
        }
        Ok(note)
    }

    /// Bumps on every change to the session: what autosave and the session
    /// broadcast watch.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Bumps on every change to the scene list or the current scene.
    pub fn scenes_revision(&self) -> u64 {
        self.scenes_revision
    }

    /// What undo and redo would do.
    pub fn history(&self) -> History {
        self.history.summary()
    }

    /// Whether the mix differs from scene `id` within the scene's scope
    /// (recall-safe strips and strips on one side only do not count).
    /// `None` when there is no such scene. Reads only.
    pub fn scene_differs(&self, id: Id) -> Option<bool> {
        let scene = self.session.scene(id)?;
        let session = &self.session;
        // A view of the current mix without copying it.
        let differs = crate::scene::differs_parts(
            MixParts {
                channels: &session.channels,
                buses: &session.buses,
                master: &session.master,
                matrices: &session.matrices,
                outputs: &session.outputs,
                dcas: &session.dcas,
                mute_groups: &session.mute_groups,
            },
            &scene.mix,
            &scene.scope,
        );
        Some(differs)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            mix: self.session.mix_state(),
            name: self.session.name.clone(),
            layers: self.session.layers.clone(),
        }
    }

    fn undo(&mut self) -> Result<Option<String>, String> {
        let current = self.snapshot();
        let Some((_, snapshot)) = self.history.undo(current) else {
            return Ok(Some("Nothing to undo".to_string()));
        };
        self.restore(snapshot);
        self.revision += 1;
        Ok(None)
    }

    fn redo(&mut self) -> Result<Option<String>, String> {
        let current = self.snapshot();
        let Some((_, snapshot)) = self.history.redo(current) else {
            return Ok(Some("Nothing to redo".to_string()));
        };
        self.restore(snapshot);
        self.revision += 1;
        Ok(None)
    }

    /// Go back (or forward) to `snapshot`. What undo does not own stays as
    /// it is now: solo, record arm and recall safe on every strip that
    /// exists on both sides.
    fn restore(&mut self, snapshot: Snapshot) {
        let mut mix = snapshot.mix;
        let session = &self.session;
        let keep = |target: StripRef, core: &mut crate::session::StripCore| {
            if let Some(now) = session.strip(target) {
                core.solo = now.solo;
                core.recall_safe = now.recall_safe;
            }
        };
        for channel in &mut mix.channels {
            keep(StripRef::Channel(channel.id), &mut channel.core);
            if let Some(now) = session.channel(channel.id) {
                channel.record_arm = now.record_arm;
            }
        }
        for bus in &mut mix.buses {
            keep(StripRef::Bus(bus.id), &mut bus.core);
            if let Some(now) = session.bus(bus.id) {
                bus.record_arm = now.record_arm;
            }
        }
        for matrix in &mut mix.matrices {
            keep(StripRef::Matrix(matrix.id), &mut matrix.core);
            if let Some(now) = session.matrix(matrix.id) {
                matrix.record_arm = now.record_arm;
            }
        }
        keep(StripRef::Master, &mut mix.master.core);
        mix.master.record_arm = session.master.record_arm;
        self.session.name = snapshot.name;
        self.session.layers = snapshot.layers;
        self.reconcile(mix);
    }

    /// Make `mix` the session's mix and bring every piece of audio state in
    /// line with it, as gently as a single edit would: inserts that still
    /// exist keep their cells (bypass and built-in parameters are sent to
    /// them; a third-party plug-in keeps its live state), processing glides
    /// to its new settings, gains ramp, and the graph is rebuilt only when
    /// its shape changed. Inserts and strips that are new are built the way
    /// a load builds them.
    fn reconcile(&mut self, mut mix: MixState) {
        let old = self.session.mix_state();
        let old_slots: HashMap<Id, InsertSlot> = all_slots(&old)
            .map(|slot| (slot.id, slot.clone()))
            .collect();
        let mut rebuild = !same_shape(&old, &mix);

        // Inserts: keep, update in place, or drop (to be rebuilt).
        let mut kept: Vec<Id> = Vec::new();
        for slot in all_slots_mut(&mut mix) {
            let Some(before) = old_slots.get(&slot.id) else {
                continue;
            };
            if !same_plugin(&before.plugin, &slot.plugin) {
                continue;
            }
            if let (
                InsertPlugin::External { state, .. },
                InsertPlugin::External { state: live, .. },
            ) = (&mut slot.plugin, &before.plugin)
            {
                // The plug-in's own state is what it is; it is not undone.
                state.clone_from(live);
            }
            kept.push(slot.id);
        }
        let new_slots: HashMap<Id, InsertSlot> = all_slots(&mix)
            .map(|slot| (slot.id, slot.clone()))
            .collect();
        for (id, before) in &old_slots {
            let Some(after) = new_slots.get(id).filter(|_| kept.contains(id)) else {
                self.drop_insert(*id);
                // Same id, another plug-in: the graph needs the new cell.
                rebuild |= new_slots.contains_key(id);
                continue;
            };
            if !self.update_insert(before, after) {
                // Not settable in place: rebuilt from the new settings.
                self.drop_insert(*id);
                rebuild = true;
            }
        }

        // Processing: glide where the strip stays, forget where it goes.
        let strips = |mix: &MixState| -> Vec<(StripRef, Processing)> {
            mix.channels
                .iter()
                .map(|c| (StripRef::Channel(c.id), c.core.processing))
                .chain(
                    mix.buses
                        .iter()
                        .map(|b| (StripRef::Bus(b.id), b.core.processing)),
                )
                .chain(std::iter::once((
                    StripRef::Master,
                    mix.master.core.processing,
                )))
                .chain(
                    mix.matrices
                        .iter()
                        .map(|m| (StripRef::Matrix(m.id), m.core.processing)),
                )
                .collect()
        };
        let before: HashMap<StripRef, Processing> = strips(&old).into_iter().collect();
        let after = strips(&mix);
        for (strip, processing) in &after {
            if before.get(strip) != Some(processing) {
                if let Some(entry) = self.processors.get(strip) {
                    entry.settings.send(*processing);
                }
            }
        }
        let remaining: Vec<StripRef> = after.iter().map(|(strip, _)| *strip).collect();
        self.processors.retain(|strip, _| remaining.contains(strip));
        self.strips.retain(|strip, _| remaining.contains(strip));
        let sends: Vec<(Id, Id)> = mix
            .channels
            .iter()
            .flat_map(|c| c.sends.iter().map(move |s| (c.id, s.bus)))
            .collect();
        self.sends.retain(|key, _| sends.contains(key));
        let matrix_sends: Vec<(Id, MatrixFeed)> = mix
            .matrices
            .iter()
            .flat_map(|m| m.sources.iter().map(move |s| (m.id, s.source)))
            .collect();
        self.matrix_sends
            .retain(|key, _| matrix_sends.contains(key));

        // A new graph starts each strip and send at the level its cell holds.
        // Hold the levels playing now through the rebuild, then set the new
        // ones, so the new graph ramps to them as for any single change.
        let held_strips: Vec<(Arc<StripShared>, [f32; 5])> = if rebuild {
            self.strips
                .values()
                .map(|s| {
                    let levels = [
                        s.gain_l.load(),
                        s.gain_r.load(),
                        s.trim.load(),
                        s.cue_pre.load(),
                        s.cue_post.load(),
                    ];
                    (s.clone(), levels)
                })
                .collect()
        } else {
            Vec::new()
        };
        let held_sends: Vec<(Arc<SendGains>, (f32, f32))> = if rebuild {
            self.sends
                .values()
                .chain(self.matrix_sends.values())
                .map(|g| (g.clone(), g.load()))
                .collect()
        } else {
            Vec::new()
        };

        self.session.set_mix(mix);
        // Talkback, oscillator and monitor destinations, layers: only what
        // still exists.
        self.session.prune_references();
        // New strips and sends get their cells here, at their new levels.
        self.refresh_gains();
        if rebuild {
            for (shared, [l, r, trim, pre, post]) in &held_strips {
                shared.gain_l.store(*l);
                shared.gain_r.store(*r);
                shared.trim.store(*trim);
                shared.cue_pre.store(*pre);
                shared.cue_post.store(*post);
            }
            for (gains, (l, r)) in &held_sends {
                gains.store(*l, *r);
            }
            self.rebuild();
            self.refresh_gains();
        }
    }

    /// Send `after`'s bypass and built-in parameters to the live insert that
    /// was `before`. `false` when a parameter cannot be returned to its
    /// default in place (the insert must be rebuilt).
    fn update_insert(&mut self, before: &InsertSlot, after: &InsertSlot) -> bool {
        let Some(entry) = self.inserts.get(&after.id) else {
            // Never built (it failed): the next rebuild tries again.
            return true;
        };
        if before.bypass != after.bypass {
            entry.cell.bypass.store(after.bypass, Ordering::Relaxed);
        }
        let (
            InsertPlugin::Builtin {
                stem,
                params: old_params,
            },
            InsertPlugin::Builtin { params, .. },
        ) = (&before.plugin, &after.plugin)
        else {
            return true;
        };
        if old_params == params {
            return true;
        }
        let mut changes: Vec<(u32, f32)> = params
            .iter()
            .filter(|(index, value)| old_params.get(index) != Some(value))
            .map(|(index, value)| (*index, *value))
            .collect();
        let dropped: Vec<u32> = old_params
            .keys()
            .filter(|index| !params.contains_key(index))
            .copied()
            .collect();
        if !dropped.is_empty() {
            // A parameter the new settings do not name is at its default.
            let Some(defaults) = crate::builtin_fx::builtin_spec(stem).map(|spec| spec.defaults)
            else {
                return false;
            };
            for index in dropped {
                let Some(default) = defaults.get(index as usize) else {
                    return false;
                };
                changes.push((index, *default));
            }
        }
        for change in changes {
            if entry.params.try_send(change).is_err() {
                return false;
            }
        }
        true
    }

    /// The edit itself, with no history.
    fn execute(&mut self, command: Command) -> Result<Option<String>, String> {
        match command {
            Command::SetFader { strip, db } => {
                self.strip_mut(strip)?.fader_db =
                    db.clamp(crate::session::MIN_FADER_DB, crate::session::MAX_FADER_DB);
                self.refresh_gains();
            }
            Command::SetPan { strip, pan } => {
                self.strip_mut(strip)?.pan = pan.clamp(-1.0, 1.0);
                self.refresh_gains();
            }
            Command::SetMute { strip, mute } => {
                self.strip_mut(strip)?.mute = mute;
                self.refresh_gains();
            }
            Command::SetSolo { strip, solo } => {
                if strip == StripRef::Master {
                    return Err("the master cannot be soloed".to_string());
                }
                self.strip_mut(strip)?.solo = solo;
                self.refresh_gains();
            }
            Command::SetTrim { channel, db } => {
                self.channel_mut(channel)?.trim_db = db.clamp(-24.0, 48.0);
                self.refresh_gains();
            }
            Command::SetPhaseInvert { channel, invert } => {
                self.channel_mut(channel)?.phase_invert = invert;
                self.refresh_gains();
            }
            Command::AddChannel { name, input } => {
                let id = self.session.allocate_id();
                self.session
                    .channels
                    .push(ChannelStrip::new(id, name, input));
                self.refresh_gains();
                self.rebuild();
            }
            Command::RemoveChannel { channel } => {
                let Some(index) = self.session.channels.iter().position(|c| c.id == channel) else {
                    return Err(format!("no channel {channel}"));
                };
                let removed = self.session.channels.remove(index);
                self.session
                    .outputs
                    .retain(|p| p.source != PatchSource::Channel(channel));
                for insert in &removed.core.inserts {
                    self.drop_insert(insert.id);
                }
                self.strips.remove(&StripRef::Channel(channel));
                self.processors.remove(&StripRef::Channel(channel));
                self.sends.retain(|(from, _), _| *from != channel);
                self.session.prune_references();
                self.refresh_gains();
                self.rebuild();
            }
            Command::RenameStrip { strip, name } => match strip {
                StripRef::Channel(id) => self.channel_mut(id)?.name = name,
                StripRef::Bus(id) => {
                    self.session
                        .bus_mut(id)
                        .ok_or_else(|| format!("no bus {id}"))?
                        .name = name
                }
                StripRef::Matrix(id) => self.matrix_mut(id)?.name = name,
                StripRef::Master => self.session.name = name,
            },
            Command::SetChannelInput { channel, input } => {
                self.channel_mut(channel)?.input = input;
                self.refresh_gains();
                self.rebuild();
            }
            Command::SetStripOutput { strip, output } => {
                if let StripOutput::Bus(bus) = output {
                    if self.session.bus(bus).is_none() {
                        return Err(format!("no bus {bus}"));
                    }
                }
                match strip {
                    StripRef::Channel(id) => self.channel_mut(id)?.output = output,
                    StripRef::Bus(id) => {
                        if matches!(output, StripOutput::Bus(_)) {
                            return Err("a bus feeds the master or nothing".to_string());
                        }
                        self.session
                            .bus_mut(id)
                            .ok_or_else(|| format!("no bus {id}"))?
                            .output = output;
                    }
                    StripRef::Master => return Err("the master has no output route".to_string()),
                    StripRef::Matrix(_) => {
                        return Err("a matrix feeds only its output patch".to_string());
                    }
                }
                self.refresh_gains();
                self.rebuild();
            }
            Command::AddBus { name, role, stereo } => {
                let id = self.session.allocate_id();
                let role = role.unwrap_or_default();
                self.session.buses.push(BusStrip {
                    id,
                    name,
                    role,
                    stereo: stereo.unwrap_or(true),
                    output: role.default_output(),
                    ..BusStrip::default()
                });
                self.refresh_gains();
                self.rebuild();
            }
            Command::SetBusRole { bus, role } => {
                self.session
                    .bus_mut(bus)
                    .ok_or_else(|| format!("no bus {bus}"))?
                    .role = role;
            }
            Command::SetBusStereo { bus, stereo } => {
                let strip = self
                    .session
                    .bus_mut(bus)
                    .ok_or_else(|| format!("no bus {bus}"))?;
                if strip.stereo != stereo {
                    strip.stereo = stereo;
                    self.refresh_gains();
                    self.rebuild();
                }
            }
            Command::RemoveBus { bus } => {
                let Some(index) = self.session.buses.iter().position(|b| b.id == bus) else {
                    return Err(format!("no bus {bus}"));
                };
                let removed = self.session.buses.remove(index);
                for insert in &removed.core.inserts {
                    self.drop_insert(insert.id);
                }
                for channel in &mut self.session.channels {
                    channel.sends.retain(|s| s.bus != bus);
                    if channel.output == StripOutput::Bus(bus) {
                        channel.output = StripOutput::Master;
                    }
                }
                for matrix in &mut self.session.matrices {
                    matrix.sources.retain(|s| s.source != MatrixFeed::Bus(bus));
                }
                self.session
                    .outputs
                    .retain(|p| p.source != PatchSource::Bus(bus));
                self.session.prune_references();
                self.strips.remove(&StripRef::Bus(bus));
                self.processors.remove(&StripRef::Bus(bus));
                self.sends.retain(|(_, to), _| *to != bus);
                self.matrix_sends
                    .retain(|(_, from), _| *from != MatrixFeed::Bus(bus));
                self.refresh_gains();
                self.rebuild();
            }
            Command::SetSend {
                channel,
                bus,
                level_db,
                pre_fader,
                pan,
                pan_follow,
            } => {
                let Some(role) = self.session.bus(bus).map(|b| b.role) else {
                    return Err(format!("no bus {bus}"));
                };
                for (value, what) in [(Some(level_db), "a send level"), (pan, "a send pan")] {
                    if value.is_some_and(|v| !v.is_finite()) {
                        return Err(format!("{what} must be a number"));
                    }
                }
                let strip = self.channel_mut(channel)?;
                let level_db = level_db.clamp(MIN_FADER_DB, MAX_FADER_DB);
                let pan = pan.map(|p| p.clamp(-1.0, 1.0));
                let mut reshaped = true;
                match strip.sends.iter_mut().find(|s| s.bus == bus) {
                    Some(send) => {
                        let before = (send.pre_fader, send.pan_follow);
                        send.level_db = level_db;
                        send.pre_fader = pre_fader.unwrap_or(send.pre_fader);
                        send.pan = pan.unwrap_or(send.pan);
                        send.pan_follow = pan_follow.unwrap_or(send.pan_follow);
                        // Where the send is taken changes the graph.
                        reshaped = before != (send.pre_fader, send.pan_follow);
                    }
                    None => strip.sends.push(SendSlot {
                        bus,
                        level_db,
                        pre_fader: pre_fader.unwrap_or(role.sends_pre_fader()),
                        pan: pan.unwrap_or(0.0),
                        pan_follow: pan_follow.unwrap_or(true),
                    }),
                }
                self.refresh_gains();
                if reshaped {
                    self.rebuild();
                }
            }
            Command::RemoveSend { channel, bus } => {
                self.channel_mut(channel)?.sends.retain(|s| s.bus != bus);
                self.sends.remove(&(channel, bus));
                self.rebuild();
            }
            Command::AddInsert {
                strip,
                mut plugin,
                index,
            } => {
                if let InsertPlugin::Builtin { stem, params } = &mut plugin {
                    for &(param, value) in crate::builtin_fx::stage_defaults(stem) {
                        params.entry(param).or_insert(value);
                    }
                }
                let id = self.session.allocate_id();
                let slot = InsertSlot {
                    id,
                    bypass: false,
                    plugin,
                };
                self.create_insert(&slot)?;
                let inserts = &mut self.strip_mut(strip)?.inserts;
                let at = index.unwrap_or(inserts.len()).min(inserts.len());
                inserts.insert(at, slot);
                self.rebuild();
            }
            Command::RemoveInsert { strip, insert } => {
                let inserts = &mut self.strip_mut(strip)?.inserts;
                let before = inserts.len();
                inserts.retain(|slot| slot.id != insert);
                if inserts.len() == before {
                    return Err(format!("no insert {insert}"));
                }
                self.drop_insert(insert);
                self.rebuild();
            }
            Command::MoveInsert { strip, insert, to } => {
                let inserts = &mut self.strip_mut(strip)?.inserts;
                let from = inserts
                    .iter()
                    .position(|slot| slot.id == insert)
                    .ok_or_else(|| format!("no insert {insert}"))?;
                let slot = inserts.remove(from);
                inserts.insert(to.min(inserts.len()), slot);
                self.rebuild();
            }
            Command::SetInsertBypass { insert, bypass } => {
                self.insert_slot_mut(insert)?.bypass = bypass;
                if let Some(entry) = self.inserts.get(&insert) {
                    entry.cell.bypass.store(bypass, Ordering::Relaxed);
                }
            }
            Command::SetInsertParam {
                insert,
                index,
                value,
            } => {
                match &mut self.insert_slot_mut(insert)?.plugin {
                    InsertPlugin::Builtin { params, .. } => {
                        params.insert(index, value);
                    }
                    InsertPlugin::External { .. } => {
                        return Err("third-party parameters are set in its editor".to_string());
                    }
                }
                if let Some(entry) = self.inserts.get(&insert) {
                    let _ = entry.params.try_send((index, value));
                }
            }
            Command::SetInsertParams { insert, values } => {
                match &mut self.insert_slot_mut(insert)?.plugin {
                    InsertPlugin::Builtin { params, .. } => {
                        params.extend(values.iter().copied());
                    }
                    InsertPlugin::External { .. } => {
                        return Err("third-party parameters are set in its editor".to_string());
                    }
                }
                if let Some(entry) = self.inserts.get(&insert) {
                    for change in values {
                        let _ = entry.params.try_send(change);
                    }
                }
            }
            Command::SetOutputPatch { patches } => {
                self.session.outputs = patches;
                self.rebuild();
            }
            Command::SetRecordArm { strip, arm } => match strip {
                StripRef::Channel(id) => self.channel_mut(id)?.record_arm = arm,
                StripRef::Bus(id) => {
                    self.session
                        .bus_mut(id)
                        .ok_or_else(|| format!("no bus {id}"))?
                        .record_arm = arm
                }
                StripRef::Matrix(id) => self.matrix_mut(id)?.record_arm = arm,
                StripRef::Master => self.session.master.record_arm = arm,
            },
            Command::SetRecordSettings { settings } => {
                if self.recording.is_some() {
                    return Err("stop recording before changing its settings".to_string());
                }
                self.session.recording = settings;
            }
            Command::SetAudio { settings } => self.set_audio(settings)?,
            Command::StartRecording => self.start_recording()?,
            Command::StopRecording => {
                self.stop_recording();
            }
            Command::SetProcessing {
                strip,
                mut processing,
            } => {
                if !processing_is_finite(&processing) {
                    return Err(
                        "every processing setting must be a number; one was not".to_string()
                    );
                }
                clamp_processing(&mut processing);
                self.strip_mut(strip)?.processing = processing;
                match self.processors.get(&strip) {
                    Some(entry) => entry.settings.send(processing),
                    // Not built yet: the next rebuild builds it from the
                    // session, settled on these settings.
                    None => self.rebuild(),
                }
            }
            Command::SetStripColor { strip, color } => {
                check_color(color)?;
                self.strip_mut(strip)?.color = color;
            }
            Command::SetSoloSafe { strip, safe } => {
                match strip {
                    StripRef::Master => {
                        return Err("the master is never silenced by a solo".to_string());
                    }
                    StripRef::Matrix(_) => {
                        return Err("a matrix is never silenced by a solo".to_string());
                    }
                    _ => {}
                }
                self.strip_mut(strip)?.solo_safe = safe;
                self.refresh_gains();
            }
            Command::ClearSolo => {
                for channel in &mut self.session.channels {
                    channel.core.solo = false;
                }
                for bus in &mut self.session.buses {
                    bus.core.solo = false;
                }
                for matrix in &mut self.session.matrices {
                    matrix.core.solo = false;
                }
                self.refresh_gains();
            }
            Command::AssignDca {
                strip,
                dca,
                assigned,
            } => {
                check_dca(dca)?;
                match strip {
                    StripRef::Master => {
                        return Err("the master cannot follow a DCA".to_string());
                    }
                    StripRef::Matrix(_) => {
                        return Err("a matrix cannot follow a DCA".to_string());
                    }
                    _ => {}
                }
                assign(&mut self.strip_mut(strip)?.dcas, dca, assigned);
                self.refresh_gains();
            }
            Command::SetDcaLevel { dca, db } => {
                check_dca(dca)?;
                if !db.is_finite() {
                    return Err("a DCA level must be a number of dB".to_string());
                }
                self.session.dcas[dca].level_db = db.clamp(MIN_FADER_DB, MAX_FADER_DB);
                self.refresh_gains();
            }
            Command::SetDcaMute { dca, mute } => {
                check_dca(dca)?;
                self.session.dcas[dca].mute = mute;
                self.refresh_gains();
            }
            Command::RenameDca { dca, name } => {
                check_dca(dca)?;
                self.session.dcas[dca].name = name;
            }
            Command::SetDcaColor { dca, color } => {
                check_dca(dca)?;
                check_color(color)?;
                self.session.dcas[dca].color = color;
            }
            Command::AssignMuteGroup {
                strip,
                group,
                assigned,
            } => {
                check_mute_group(group)?;
                if strip == StripRef::Master {
                    return Err("the master cannot be in a mute group".to_string());
                }
                assign(&mut self.strip_mut(strip)?.mute_groups, group, assigned);
                self.refresh_gains();
            }
            Command::SetMuteGroup { group, active } => {
                check_mute_group(group)?;
                self.session.mute_groups[group].active = active;
                self.refresh_gains();
            }
            Command::RenameMuteGroup { group, name } => {
                check_mute_group(group)?;
                self.session.mute_groups[group].name = name;
            }
            Command::SetMonitor { mut monitor } => {
                if !monitor.level_db.is_finite() {
                    return Err("the monitor level must be a number of dB".to_string());
                }
                monitor.level_db = monitor.level_db.clamp(MIN_FADER_DB, MAX_FADER_DB);
                match monitor.source {
                    MonitorSource::Master => {}
                    MonitorSource::Bus(id) => {
                        self.session.bus(id).ok_or_else(|| format!("no bus {id}"))?;
                    }
                    MonitorSource::Matrix(id) => {
                        self.matrix_mut(id)?;
                    }
                }
                let reshaped = monitor.source != self.session.monitor.source;
                self.session.monitor = monitor;
                self.refresh_gains();
                if reshaped {
                    self.rebuild();
                }
            }
            Command::MoveChannel { channel, index } => {
                let channels = &mut self.session.channels;
                let from = channels
                    .iter()
                    .position(|c| c.id == channel)
                    .ok_or_else(|| format!("no channel {channel}"))?;
                let moved = channels.remove(from);
                let to = index.min(channels.len());
                channels.insert(to, moved);
                if from != to {
                    self.rebuild();
                }
            }
            Command::MoveBus { bus, index } => {
                let buses = &mut self.session.buses;
                let from = buses
                    .iter()
                    .position(|b| b.id == bus)
                    .ok_or_else(|| format!("no bus {bus}"))?;
                let moved = buses.remove(from);
                let to = index.min(buses.len());
                buses.insert(to, moved);
                if from != to {
                    self.rebuild();
                }
            }
            // Handled by `apply_at`, never executed.
            Command::Undo | Command::Redo => {}
            Command::SceneStore { id, name } => self.scene_store(id, name)?,
            Command::SceneRecall { id } => return self.scene_recall(id),
            Command::SceneRename { id, name } => self.scene_mut(id)?.name = name,
            Command::SceneNote { id, note } => self.scene_mut(id)?.note = note,
            Command::SceneScope { id, scope } => self.scene_mut(id)?.scope = scope,
            Command::SceneDelete { id } => {
                let index = self.scene_index(id)?;
                self.session.scenes.remove(index);
                if self.session.current_scene == Some(id) {
                    self.session.current_scene = None;
                }
                self.scenes_revision += 1;
            }
            Command::SceneMove { id, index } => {
                let from = self.scene_index(id)?;
                let scene = self.session.scenes.remove(from);
                let to = index.min(self.session.scenes.len());
                self.session.scenes.insert(to, scene);
                self.scenes_revision += 1;
            }
            Command::SetRecallSafe { strip, safe } => self.strip_mut(strip)?.recall_safe = safe,
            Command::PasteStrip {
                targets,
                settings,
                sections,
            } => self.paste_strip(&targets, &settings, &sections)?,
            Command::AddMatrix { name, stereo } => {
                let id = self.session.allocate_id();
                self.session.matrices.push(MatrixStrip {
                    id,
                    name,
                    stereo: stereo.unwrap_or(true),
                    ..MatrixStrip::default()
                });
                self.refresh_gains();
                self.rebuild();
            }
            Command::RemoveMatrix { matrix } => {
                let Some(index) = self.session.matrices.iter().position(|m| m.id == matrix) else {
                    return Err(format!("no matrix {matrix}"));
                };
                let removed = self.session.matrices.remove(index);
                for insert in &removed.core.inserts {
                    self.drop_insert(insert.id);
                }
                self.session
                    .outputs
                    .retain(|p| p.source != PatchSource::Matrix(matrix));
                self.session.prune_references();
                self.strips.remove(&StripRef::Matrix(matrix));
                self.processors.remove(&StripRef::Matrix(matrix));
                self.matrix_sends.retain(|(to, _), _| *to != matrix);
                self.refresh_gains();
                self.rebuild();
            }
            Command::SetMatrixSend {
                matrix,
                source,
                level_db,
                pan,
            } => {
                if let MatrixFeed::Bus(bus) = source {
                    self.session
                        .bus(bus)
                        .ok_or_else(|| format!("no bus {bus}"))?;
                }
                if !level_db.is_finite() || pan.is_some_and(|p| !p.is_finite()) {
                    return Err("a matrix send's level and pan must be numbers".to_string());
                }
                let level_db = level_db.clamp(MIN_FADER_DB, MAX_FADER_DB);
                let pan = pan.map(|p| p.clamp(-1.0, 1.0));
                let strip = self.matrix_mut(matrix)?;
                match strip.sources.iter_mut().find(|s| s.source == source) {
                    Some(existing) => {
                        existing.level_db = level_db;
                        existing.pan = pan.unwrap_or(existing.pan);
                        self.refresh_gains();
                    }
                    None => {
                        strip.sources.push(MatrixSource {
                            source,
                            level_db,
                            pan: pan.unwrap_or(0.0),
                        });
                        self.refresh_gains();
                        self.rebuild();
                    }
                }
            }
            Command::MoveMatrix { matrix, index } => {
                let matrices = &mut self.session.matrices;
                let from = matrices
                    .iter()
                    .position(|m| m.id == matrix)
                    .ok_or_else(|| format!("no matrix {matrix}"))?;
                let moved = matrices.remove(from);
                let to = index.min(matrices.len());
                matrices.insert(to, moved);
                if from != to {
                    self.rebuild();
                }
            }
            Command::SetMatrixStereo { matrix, stereo } => {
                let strip = self.matrix_mut(matrix)?;
                if strip.stereo != stereo {
                    strip.stereo = stereo;
                    self.refresh_gains();
                    self.rebuild();
                }
            }
            Command::SetTalkback { mut talkback } => {
                if !talkback.level_db.is_finite() {
                    return Err("the talkback level must be a number of dB".to_string());
                }
                talkback.level_db = talkback.level_db.clamp(MIN_FADER_DB, MAX_FADER_DB);
                self.check_destinations(&talkback.to)?;
                let before = &self.session.talkback;
                let reshaped = talkback.input != before.input || talkback.to != before.to;
                self.session.talkback = talkback;
                self.session.prune_references();
                self.refresh_gains();
                if reshaped {
                    self.rebuild();
                }
            }
            Command::Talk { active } => {
                self.talk_active = active;
                self.refresh_gains();
            }
            Command::SetOscillator { mut oscillator } => {
                if !oscillator.level_db.is_finite() || !oscillator.hz.is_finite() {
                    return Err("the oscillator's level and frequency must be numbers".to_string());
                }
                oscillator.level_db = oscillator
                    .level_db
                    .clamp(OSCILLATOR_MIN_DB, OSCILLATOR_MAX_DB);
                oscillator.hz = oscillator.hz.clamp(OSCILLATOR_MIN_HZ, OSCILLATOR_MAX_HZ);
                self.check_destinations(&oscillator.to)?;
                let reshaped = oscillator.to != self.session.oscillator.to;
                self.session.oscillator = oscillator;
                self.session.prune_references();
                self.refresh_gains();
                if reshaped {
                    self.rebuild();
                }
            }
            Command::OscillatorOn { on } => {
                self.oscillator_on = on;
                self.refresh_gains();
            }
            Command::SetLayer {
                index,
                name,
                strips,
            } => {
                let layers = self.session.layers.len();
                if index >= MAX_LAYERS || index > layers {
                    return Err(format!(
                        "there is no layer {index}: layers are numbered 0 to {}, and the next new one is {layers}",
                        MAX_LAYERS - 1
                    ));
                }
                if let Some(missing) = strips.iter().find(|s| !self.session.has_strip(**s)) {
                    return Err(format!("no strip {missing:?}"));
                }
                let mut unique: Vec<StripRef> = Vec::new();
                for strip in strips {
                    if !unique.contains(&strip) {
                        unique.push(strip);
                    }
                }
                let layer = Layer {
                    name,
                    strips: unique,
                };
                if index == layers {
                    self.session.layers.push(layer);
                } else {
                    self.session.layers[index] = layer;
                }
            }
            Command::RemoveLayer { index } => {
                if index >= self.session.layers.len() {
                    return Err(format!("there is no layer {index}"));
                }
                self.session.layers.remove(index);
            }
            Command::PlaybackLoad { folder } => {
                // Everything that can fail first: a refused take leaves the
                // loaded one playing.
                let files = probe_take(&folder)?;
                let same = self.session.playback.folder.as_deref() == Some(folder.as_path());
                let saved = if same {
                    self.session.playback.tracks.clone()
                } else {
                    Vec::new()
                };
                let tracks = assign_tracks(&files, &self.session.channels, &saved);
                let player = Player::open(files, self.sample_rate)?;
                self.fade_out_virtual_soundcheck();
                self.player = Some(player);
                self.playback_error = None;
                self.session.playback = crate::session::PlaybackSettings {
                    folder: Some(folder),
                    tracks,
                    virtual_soundcheck: false,
                };
                self.rebuild();
            }
            Command::PlaybackAssign { file, channel } => {
                if let Some(id) = channel {
                    self.session
                        .channel(id)
                        .ok_or_else(|| format!("no channel {id}"))?;
                }
                let tracks = &mut self.session.playback.tracks;
                let index = tracks
                    .iter()
                    .position(|t| t.file == file)
                    .ok_or_else(|| format!("{file} is not in the loaded take"))?;
                if channel.is_some() {
                    for track in tracks.iter_mut().filter(|t| t.channel == channel) {
                        track.channel = None;
                    }
                }
                tracks[index].channel = channel;
                self.rebuild();
            }
            Command::PlaybackUnload => {
                self.fade_out_virtual_soundcheck();
                self.player = None;
                self.playback_error = None;
                self.session.playback = crate::session::PlaybackSettings::default();
                self.rebuild();
            }
            Command::Playback { action } => self.player()?.transport(action),
            Command::PlaybackLocate { seconds } => {
                if !seconds.is_finite() {
                    return Err("the position must be a number of seconds".to_string());
                }
                self.player()?.locate(seconds);
            }
            Command::PlaybackLoop { on } => self.player()?.set_loop(on),
            Command::SetVirtualSoundcheck { on } => {
                if on {
                    self.player()?.set_virtual_soundcheck(true);
                } else if let Some(player) = &self.player {
                    player.set_virtual_soundcheck(false);
                }
                self.session.playback.virtual_soundcheck = on;
            }
        }
        Ok(None)
    }

    /// The loaded take, or why there is none.
    fn player(&self) -> Result<&Player, String> {
        self.player
            .as_ref()
            .ok_or_else(|| match &self.playback_error {
                Some(error) => format!("no take is loaded ({error})"),
                None => "no take is loaded".to_string(),
            })
    }

    /// Turn virtual soundcheck off and let the crossfade back to the live
    /// inputs play out, before the take leaves the graph.
    fn fade_out_virtual_soundcheck(&mut self) {
        let Some(player) = &self.player else {
            return;
        };
        if !self.session.playback.virtual_soundcheck {
            return;
        }
        player.set_virtual_soundcheck(false);
        self.session.playback.virtual_soundcheck = false;
        if self.streams.is_some() {
            // The fade, and a buffer or two around it.
            std::thread::sleep(Duration::from_secs_f32(
                crate::graph::BYPASS_FADE_SECONDS * 3.0,
            ));
        }
    }

    /// Open the take the session names again (a loaded show, a new sample
    /// rate), keeping its assignments. A take that does not open stays in
    /// the session, unloaded, with the reason in the status.
    fn reopen_playback(&mut self) {
        self.player = None;
        self.playback_error = None;
        let Some(folder) = self.session.playback.folder.clone() else {
            return;
        };
        let opened = probe_take(&folder).and_then(|files| {
            let tracks = assign_tracks(
                &files,
                &self.session.channels,
                &self.session.playback.tracks,
            );
            let player = Player::open(files, self.sample_rate)?;
            Ok((player, tracks))
        });
        match opened {
            Ok((player, tracks)) => {
                player.set_virtual_soundcheck(self.session.playback.virtual_soundcheck);
                self.player = Some(player);
                self.session.playback.tracks = tracks;
            }
            Err(error) => {
                self.session.playback.virtual_soundcheck = false;
                self.playback_error = Some(error);
            }
        }
    }

    /// Talkback and oscillator destinations must exist.
    fn check_destinations(&self, to: &[InjectDest]) -> Result<(), String> {
        for dest in to {
            match *dest {
                InjectDest::Bus(id) if self.session.bus(id).is_none() => {
                    return Err(format!("no bus {id}"));
                }
                InjectDest::Matrix(id) if self.session.matrix(id).is_none() => {
                    return Err(format!("no matrix {id}"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn scene_index(&self, id: Id) -> Result<usize, String> {
        self.session
            .scenes
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| format!("no scene {id}"))
    }

    /// A scene to change; the scene list changes with it.
    fn scene_mut(&mut self, id: Id) -> Result<&mut Scene, String> {
        let index = self.scene_index(id)?;
        self.scenes_revision += 1;
        Ok(&mut self.session.scenes[index])
    }

    /// Store the mix into scene `id`, or into a new scene after the current
    /// one. The stored scene becomes the current one.
    fn scene_store(&mut self, id: Option<Id>, name: Option<String>) -> Result<(), String> {
        let mix = self.session.mix_state();
        let id = match id {
            Some(id) => {
                let scene = self.scene_mut(id)?;
                scene.mix = mix;
                if let Some(name) = name {
                    scene.name = name;
                }
                id
            }
            None => {
                let id = self.session.allocate_id();
                let at = self
                    .session
                    .current_scene
                    .and_then(|current| self.session.scenes.iter().position(|s| s.id == current))
                    .map_or(self.session.scenes.len(), |index| index + 1);
                let name = name.unwrap_or_else(|| {
                    // The first "Scene N" not taken.
                    (self.session.scenes.len() + 1..)
                        .map(|n| format!("Scene {n}"))
                        .find(|name| !self.session.scenes.iter().any(|s| &s.name == name))
                        .expect("an unbounded range finds a free name")
                });
                self.session.scenes.insert(
                    at,
                    Scene {
                        id,
                        name,
                        note: String::new(),
                        scope: RecallScope::default(),
                        mix,
                    },
                );
                id
            }
        };
        self.session.current_scene = Some(id);
        self.scenes_revision += 1;
        Ok(())
    }

    /// Recall scene `id` (see [`crate::scene`]): one undo step.
    fn scene_recall(&mut self, id: Id) -> Result<Option<String>, String> {
        let scene = self
            .session
            .scene(id)
            .ok_or_else(|| format!("no scene {id}"))?;
        let mut mix = self.session.mix_state();
        let report: RecallReport = recall_into(&mut mix, &scene.mix, &scene.scope);
        self.reconcile(mix);
        self.session.current_scene = Some(id);
        self.scenes_revision += 1;
        Ok(report.note())
    }

    /// Apply `sections` of `settings` to every target, all or nothing.
    fn paste_strip(
        &mut self,
        targets: &[StripRef],
        settings: &StripSettings,
        sections: &[Section],
    ) -> Result<(), String> {
        for target in targets {
            if self.session.strip(*target).is_none() {
                return Err(format!("no strip {target:?}"));
            }
        }
        let has = |section: Section| sections.contains(&section);
        let mut processing = settings.processing;
        if let Some(processing) = &mut processing {
            if !processing_is_finite(processing) {
                return Err("every processing setting must be a number; one was not".to_string());
            }
            clamp_processing(processing);
        }
        let finite = |value: Option<f32>, what: &str| -> Result<Option<f32>, String> {
            match value {
                Some(v) if !v.is_finite() => Err(format!("{what} must be a number")),
                other => Ok(other),
            }
        };
        let fader_db = finite(settings.fader_db, "a fader level")?
            .map(|db| db.clamp(MIN_FADER_DB, MAX_FADER_DB));
        let pan = finite(settings.pan, "a pan")?.map(|pan| pan.clamp(-1.0, 1.0));
        let trim_db = finite(settings.trim_db, "a trim")?.map(|db| db.clamp(-24.0, 48.0));
        if let Some(color) = settings.color {
            check_color(color)?;
        }
        let inserts = match (&settings.inserts, has(Section::Inserts)) {
            (Some(inserts), true) => {
                for insert in inserts {
                    match &insert.plugin {
                        InsertPlugin::Builtin { stem, .. } => {
                            if crate::builtin_fx::effect_info(stem).is_none() {
                                return Err(format!("{stem} is not a built-in effect"));
                            }
                        }
                        #[cfg(not(feature = "external-plugins"))]
                        InsertPlugin::External { name, .. } => {
                            return Err(format!("{name}: this build runs built-in effects only"));
                        }
                        #[cfg(feature = "external-plugins")]
                        InsertPlugin::External { .. } => {}
                    }
                }
                Some(inserts)
            }
            _ => None,
        };
        let buses: Vec<Id> = self.session.buses.iter().map(|b| b.id).collect();
        let sends: Option<Vec<SendSlot>> = settings.sends.as_ref().map(|sends| {
            let mut kept: Vec<SendSlot> = Vec::new();
            for send in sends.iter().filter(|s| buses.contains(&s.bus)) {
                if kept.iter().all(|s| s.bus != send.bus) {
                    kept.push(SendSlot {
                        bus: send.bus,
                        level_db: if send.level_db.is_finite() {
                            send.level_db.clamp(MIN_FADER_DB, MAX_FADER_DB)
                        } else {
                            MIN_FADER_DB
                        },
                        pre_fader: send.pre_fader,
                        pan: if send.pan.is_finite() {
                            send.pan.clamp(-1.0, 1.0)
                        } else {
                            0.0
                        },
                        pan_follow: send.pan_follow,
                    });
                }
            }
            kept
        });

        let mut mix = self.session.mix_state();
        for target in targets {
            // Fresh ids for every copy, taken before the strip is borrowed.
            let new_inserts: Option<Vec<InsertSlot>> = inserts.map(|inserts| {
                inserts
                    .iter()
                    .map(|insert| InsertSlot {
                        id: self.session.allocate_id(),
                        bypass: insert.bypass,
                        plugin: insert.plugin.clone(),
                    })
                    .collect()
            });
            let (core, name) = match *target {
                StripRef::Channel(id) => {
                    let channel = mix
                        .channels
                        .iter_mut()
                        .find(|c| c.id == id)
                        .expect("checked above");
                    // Channel-only fields.
                    if has(Section::Input) {
                        if let Some(db) = trim_db {
                            channel.trim_db = db;
                        }
                        if let Some(invert) = settings.phase_invert {
                            channel.phase_invert = invert;
                        }
                    }
                    if let (true, Some(sends)) = (has(Section::Sends), &sends) {
                        channel.sends.clone_from(sends);
                    }
                    (&mut channel.core, Some(&mut channel.name))
                }
                StripRef::Bus(id) => {
                    let bus = mix
                        .buses
                        .iter_mut()
                        .find(|b| b.id == id)
                        .expect("checked above");
                    (&mut bus.core, Some(&mut bus.name))
                }
                StripRef::Matrix(id) => {
                    let matrix = mix
                        .matrices
                        .iter_mut()
                        .find(|m| m.id == id)
                        .expect("checked above");
                    (&mut matrix.core, Some(&mut matrix.name))
                }
                // The master's name is the session's: not a strip setting.
                StripRef::Master => (&mut mix.master.core, None),
            };
            if let Some(processing) = processing {
                if has(Section::Processing) {
                    core.processing = processing;
                }
                if has(Section::Hpf) {
                    core.processing.hpf = processing.hpf;
                }
                if has(Section::Gate) {
                    core.processing.gate = processing.gate;
                }
                if has(Section::Eq) {
                    core.processing.eq = processing.eq;
                }
                if has(Section::Comp) {
                    core.processing.comp = processing.comp;
                }
                if has(Section::Delay) {
                    core.processing.delay = processing.delay;
                }
            }
            if let Some(new_inserts) = new_inserts {
                core.inserts = new_inserts;
            }
            if has(Section::FaderPan) {
                if let Some(db) = fader_db {
                    core.fader_db = db;
                }
                if let Some(pan) = pan {
                    core.pan = pan;
                }
            }
            if has(Section::NameColor) {
                if let Some(color) = settings.color {
                    core.color = color;
                }
                if let (Some(name), Some(new)) = (name, &settings.name) {
                    name.clone_from(new);
                }
            }
        }
        self.reconcile(mix);
        Ok(())
    }

    fn strip_mut(&mut self, strip: StripRef) -> Result<&mut crate::session::StripCore, String> {
        self.session
            .strip_mut(strip)
            .ok_or_else(|| format!("no strip {strip:?}"))
    }

    fn channel_mut(&mut self, id: Id) -> Result<&mut ChannelStrip, String> {
        self.session
            .channel_mut(id)
            .ok_or_else(|| format!("no channel {id}"))
    }

    fn matrix_mut(&mut self, id: Id) -> Result<&mut MatrixStrip, String> {
        self.session
            .matrix_mut(id)
            .ok_or_else(|| format!("no matrix {id}"))
    }

    fn insert_slot_mut(&mut self, insert: Id) -> Result<&mut InsertSlot, String> {
        let session = &mut self.session;
        session
            .channels
            .iter_mut()
            .map(|c| &mut c.core)
            .chain(session.buses.iter_mut().map(|b| &mut b.core))
            .chain(std::iter::once(&mut session.master.core))
            .chain(session.matrices.iter_mut().map(|m| &mut m.core))
            .flat_map(|core| core.inserts.iter_mut())
            .find(|slot| slot.id == insert)
            .ok_or_else(|| format!("no insert {insert}"))
    }

    fn all_insert_slots(&self) -> Vec<InsertSlot> {
        self.session
            .strip_cores()
            .flat_map(|(_, core)| core.inserts.iter().cloned())
            .collect()
    }

    /// Fader, DCAs, pan, mute (own, DCA, mute group), solo and trim into
    /// every strip's gain and cue cells, and the monitor's. Cheap enough to
    /// run after any of them changes; the audio thread ramps to the new
    /// values, so nothing clicks.
    fn refresh_gains(&mut self) {
        let session = &self.session;
        let mode = session.monitor.solo_mode;
        // Main-mix gain: the effective level when heard, else silence.
        let heard = |strip: StripRef| -> f32 {
            if !session.audible(strip) {
                return 0.0;
            }
            session.effective_level_db(strip).map_or(0.0, db_to_gain)
        };
        // PFL and AFL cue gains for a strip.
        let cues = |solo: bool| -> (f32, f32) {
            match (solo, mode) {
                (true, SoloMode::Pfl) => (1.0, 0.0),
                (true, SoloMode::Afl) => (0.0, 1.0),
                _ => (0.0, 0.0),
            }
        };
        struct Target {
            strip: StripRef,
            l: f32,
            r: f32,
            trim: f32,
            cue: (f32, f32),
        }
        let mut targets: Vec<Target> = Vec::new();
        for channel in &session.channels {
            let strip = StripRef::Channel(channel.id);
            let gain = heard(strip);
            let (l, r) = if channel.is_stereo() {
                balance(channel.core.pan)
            } else {
                constant_power(channel.core.pan)
            };
            let trim = db_to_gain(channel.trim_db) * if channel.phase_invert { -1.0 } else { 1.0 };
            targets.push(Target {
                strip,
                l: gain * l,
                r: gain * r,
                trim,
                cue: cues(channel.core.solo),
            });
        }
        for bus in &session.buses {
            let strip = StripRef::Bus(bus.id);
            let gain = heard(strip);
            let (l, r) = balance(bus.core.pan);
            targets.push(Target {
                strip,
                l: gain * l,
                r: gain * r,
                trim: 1.0,
                cue: cues(bus.core.solo),
            });
        }
        let master = &session.master.core;
        let gain = if master.mute {
            0.0
        } else {
            db_to_gain(master.fader_db)
        };
        let (l, r) = balance(master.pan);
        targets.push(Target {
            strip: StripRef::Master,
            l: gain * l,
            r: gain * r,
            trim: 1.0,
            cue: (0.0, 0.0),
        });
        for matrix in &session.matrices {
            let strip = StripRef::Matrix(matrix.id);
            let gain = heard(strip);
            let (l, r) = balance(matrix.core.pan);
            // A matrix's solo is always a cue: AFL in solo-in-place mode.
            let cue = match (matrix.core.solo, mode) {
                (true, SoloMode::Pfl) => (1.0, 0.0),
                (true, _) => (0.0, 1.0),
                _ => (0.0, 0.0),
            };
            targets.push(Target {
                strip,
                l: gain * l,
                r: gain * r,
                trim: 1.0,
                cue,
            });
        }

        // The monitor: its source while nothing is soloed, the master in
        // solo in place, else only the cues.
        let monitor = &session.monitor;
        let matrix_solo = session.matrices.iter().any(|m| m.core.solo);
        let (master_feed, source_feed) = if !session.any_solo() {
            match monitor.source {
                MonitorSource::Master => (1.0, 0.0),
                _ => (0.0, 1.0),
            }
        } else if mode == SoloMode::Sip && !matrix_solo {
            (1.0, 0.0)
        } else {
            (0.0, 0.0)
        };
        // Talking dims the monitor as the dim switch does (not twice).
        let dim = if monitor.dim || self.talk_active {
            db_to_gain(MONITOR_DIM_DB)
        } else {
            1.0
        };
        self.monitor.master_feed.store(master_feed);
        self.monitor.source_feed.store(source_feed);
        self.monitor.gain.store(db_to_gain(monitor.level_db) * dim);

        // Sends: see `send_shape`.
        let mut send_targets: Vec<((Id, Id), (f32, f32))> = Vec::new();
        for channel in &session.channels {
            let level = heard(StripRef::Channel(channel.id));
            for send in &channel.sends {
                let bus_stereo = session.bus(send.bus).is_none_or(|b| b.stereo);
                let (_, gains) = send_shape(send, channel.is_stereo(), bus_stereo, level);
                send_targets.push(((channel.id, send.bus), gains));
            }
        }
        let mut matrix_targets: Vec<((Id, MatrixFeed), (f32, f32))> = Vec::new();
        for matrix in &session.matrices {
            for source in &matrix.sources {
                let gain = db_to_gain(source.level_db);
                let (l, r) = if matrix.stereo {
                    balance(source.pan)
                } else {
                    (1.0, 1.0)
                };
                matrix_targets.push(((matrix.id, source.source), (gain * l, gain * r)));
            }
        }

        for target in targets {
            let shared = self.strips.entry(target.strip).or_default();
            shared.gain_l.store(target.l);
            shared.gain_r.store(target.r);
            shared.trim.store(target.trim);
            shared.cue_pre.store(target.cue.0);
            shared.cue_post.store(target.cue.1);
        }
        for (key, (l, r)) in send_targets {
            self.sends
                .entry(key)
                .or_insert_with(|| Arc::new(SendGains::new(l, r)))
                .store(l, r);
        }
        for (key, (l, r)) in matrix_targets {
            self.matrix_sends
                .entry(key)
                .or_insert_with(|| Arc::new(SendGains::new(l, r)))
                .store(l, r);
        }
        self.push_generators();
    }

    /// Talkback and oscillator settings and switches into their cells.
    fn push_generators(&self) {
        let talkback = &self.session.talkback;
        self.talkback.level.store(db_to_gain(talkback.level_db));
        self.talkback.hpf.store(talkback.hpf, Ordering::Relaxed);
        self.talkback
            .active
            .store(self.talk_active, Ordering::Relaxed);
        let oscillator = &self.session.oscillator;
        self.oscillator.level.store(db_to_gain(oscillator.level_db));
        self.oscillator.hz.store(oscillator.hz);
        self.oscillator.set_kind(oscillator.kind);
        self.oscillator
            .on
            .store(self.oscillator_on, Ordering::Relaxed);
    }

    /// Talkback and oscillator cells built for the running sample rate.
    fn ensure_generators(&mut self) {
        if self.generators_rate != self.sample_rate {
            self.talkback = TalkbackCell::new(self.sample_rate);
            self.oscillator = OscillatorCell::new(self.sample_rate);
            self.generators_rate = self.sample_rate;
            self.push_generators();
        }
    }

    fn create_insert(&mut self, slot: &InsertSlot) -> Result<(), String> {
        let dsp = match &slot.plugin {
            InsertPlugin::Builtin { stem, params } => {
                let mut fx = BuiltinFx::new(stem, self.sample_rate)
                    .ok_or_else(|| format!("{stem} is not a built-in effect"))?;
                for (index, value) in params {
                    fx.apply_wire_param(*index, *value);
                }
                InsertDsp::Builtin(fx)
            }
            #[cfg(feature = "external-plugins")]
            InsertPlugin::External {
                format,
                path,
                class_id,
                state,
                ..
            } => {
                if self.external.as_ref().is_none_or(|host| !host.is_alive()) {
                    self.external = Some(crate::external::ExternalHost::spawn()?);
                }
                let host = self.external.as_mut().expect("spawned above");
                let insert = host.load(
                    slot.id,
                    format,
                    path,
                    class_id,
                    self.sample_rate,
                    state.as_ref(),
                )?;
                self.insert_states.insert(slot.id, InsertState::Loading);
                InsertDsp::External(insert)
            }
            #[cfg(not(feature = "external-plugins"))]
            InsertPlugin::External { name, .. } => {
                return Err(format!("{name}: this build runs built-in effects only"));
            }
        };
        let (cell, params) = InsertCell::new(dsp, slot.bypass, self.sample_rate);
        self.inserts.insert(slot.id, InsertEntry { cell, params });
        Ok(())
    }

    fn drop_insert(&mut self, insert: Id) {
        self.inserts.remove(&insert);
        self.insert_states.remove(&insert);
        #[cfg(feature = "external-plugins")]
        if let Some(host) = self.external.as_mut() {
            host.unload(insert);
        }
    }

    /// Every insert in the session has a cell; ones that cannot be built are
    /// left out of the graph and reported as failed.
    fn ensure_inserts(&mut self) {
        for slot in self.all_insert_slots() {
            if self.inserts.contains_key(&slot.id) {
                continue;
            }
            if let Err(error) = self.create_insert(&slot) {
                self.insert_states
                    .insert(slot.id, InsertState::Failed(error));
            }
        }
    }

    /// Every strip has a processing section: a new one (or one rebuilt for
    /// a new sample rate) starts settled on the strip's saved settings.
    fn ensure_processors(&mut self) {
        for (strip, core) in self.session.strip_cores() {
            if self.processors.contains_key(&strip) {
                continue;
            }
            let (cell, settings) = ProcessorCell::new(self.sample_rate, &core.processing);
            self.processors
                .insert(strip, ProcessorEntry { cell, settings });
        }
    }

    /// Compile the session into a graph and hand it to the audio thread.
    fn rebuild(&mut self) {
        self.ensure_inserts();
        self.ensure_processors();
        self.ensure_generators();
        let record_taps = &self.record_taps;
        let session = &self.session;
        let bus_index: HashMap<Id, usize> = session
            .buses
            .iter()
            .enumerate()
            .map(|(i, b)| (b.id, i))
            .collect();
        let dest = |output: StripOutput| match output {
            StripOutput::Master => Dest::Master,
            StripOutput::Bus(id) => bus_index.get(&id).map_or(Dest::None, |i| Dest::Bus(*i)),
            StripOutput::None => Dest::None,
        };
        let cells = |core: &crate::session::StripCore| -> Vec<Arc<InsertCell>> {
            core.inserts
                .iter()
                .filter_map(|slot| self.inserts.get(&slot.id).map(|e| e.cell.clone()))
                .collect()
        };
        let shared = |strip: StripRef| -> Arc<StripShared> {
            self.strips.get(&strip).cloned().unwrap_or_default()
        };
        let processor = |strip: StripRef| -> Arc<ProcessorCell> {
            self.processors
                .get(&strip)
                .expect("ensure_processors built every strip's")
                .cell
                .clone()
        };
        let in_ch = self.in_channels;
        let record_input = session.recording.tap == RecordTap::Input;

        let channels = session
            .channels
            .iter()
            .map(|channel| {
                let strip = StripRef::Channel(channel.id);
                let in_range = |c: Option<u16>| c.map(usize::from).filter(|c| *c < in_ch);
                RtChannel {
                    strip: RtStrip::new(
                        shared(strip),
                        processor(strip),
                        cells(&channel.core),
                        record_taps.get(&strip).cloned(),
                    ),
                    input_left: in_range(channel.input.left),
                    input_right: in_range(channel.input.right),
                    sends: channel
                        .sends
                        .iter()
                        .filter_map(|send| {
                            let bus = *bus_index.get(&send.bus)?;
                            let gains = self.sends.get(&(channel.id, send.bus))?.clone();
                            let bus_stereo = session.buses[bus].stereo;
                            let (tap, _) = send_shape(send, channel.is_stereo(), bus_stereo, 1.0);
                            Some(RtSend::new(bus, gains, tap))
                        })
                        .collect(),
                    dest: dest(channel.output),
                    record_input,
                }
            })
            .collect();
        let buses = session
            .buses
            .iter()
            .map(|bus| {
                let strip = StripRef::Bus(bus.id);
                RtBus {
                    strip: RtStrip::new(
                        shared(strip),
                        processor(strip),
                        cells(&bus.core),
                        record_taps.get(&strip).cloned(),
                    ),
                    dest: match bus.output {
                        StripOutput::Master => Dest::Master,
                        _ => Dest::None,
                    },
                    stereo: bus.stereo,
                }
            })
            .collect();
        let master = RtStrip::new(
            shared(StripRef::Master),
            processor(StripRef::Master),
            cells(&session.master.core),
            record_taps.get(&StripRef::Master).cloned(),
        );
        let matrices: Vec<RtMatrix> = session
            .matrices
            .iter()
            .map(|matrix| {
                let strip = StripRef::Matrix(matrix.id);
                RtMatrix {
                    strip: RtStrip::new(
                        shared(strip),
                        processor(strip),
                        cells(&matrix.core),
                        record_taps.get(&strip).cloned(),
                    ),
                    stereo: matrix.stereo,
                    sources: matrix
                        .sources
                        .iter()
                        .filter_map(|source| {
                            let from = match source.source {
                                MatrixFeed::Master => MixFrom::Master,
                                MatrixFeed::Bus(id) => MixFrom::Bus(*bus_index.get(&id)?),
                            };
                            let gains = self.matrix_sends.get(&(matrix.id, source.source))?;
                            Some(RtMatrixSource::new(from, gains.clone()))
                        })
                        .collect(),
                }
            })
            .collect();
        let matrix_index: HashMap<Id, usize> = session
            .matrices
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id, i))
            .collect();
        let channel_index: HashMap<Id, usize> = session
            .channels
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect();
        let out_ch = self.out_channels;
        let patches = session
            .outputs
            .iter()
            .filter(|p| usize::from(p.left) < out_ch)
            .filter_map(|p| {
                Some(RtPatch {
                    from: match p.source {
                        PatchSource::Master => PatchFrom::Master,
                        PatchSource::Bus(id) => PatchFrom::Bus(*bus_index.get(&id)?),
                        PatchSource::Channel(id) => PatchFrom::Channel(*channel_index.get(&id)?),
                        PatchSource::Monitor => PatchFrom::Monitor,
                        PatchSource::Matrix(id) => PatchFrom::Matrix(*matrix_index.get(&id)?),
                    },
                    left: usize::from(p.left),
                    right: p.right.map(usize::from).filter(|r| *r < out_ch),
                })
            })
            .collect();
        let mix_from = |source: MonitorSource| -> Option<MixFrom> {
            Some(match source {
                MonitorSource::Master => MixFrom::Master,
                MonitorSource::Bus(id) => MixFrom::Bus(*bus_index.get(&id)?),
                MonitorSource::Matrix(id) => MixFrom::Matrix(*matrix_index.get(&id)?),
            })
        };
        let source = session.monitor.source;
        let monitor = RtMonitor::new(self.monitor.clone()).with_source(
            mix_from(source).unwrap_or(MixFrom::Master),
            self.monitor_source_built
                .filter(|before| *before != source)
                .and_then(mix_from),
        );
        let inject_to = |to: &[InjectDest]| -> Vec<InjectTo> {
            to.iter()
                .filter_map(|dest| {
                    Some(match *dest {
                        InjectDest::Master => InjectTo::Master,
                        InjectDest::Monitor => InjectTo::Monitor,
                        InjectDest::Bus(id) => InjectTo::Bus(*bus_index.get(&id)?),
                        InjectDest::Matrix(id) => InjectTo::Matrix(*matrix_index.get(&id)?),
                    })
                })
                .collect()
        };
        let talkback = RtTalkback {
            cell: self.talkback.clone(),
            input: session
                .talkback
                .input
                .map(usize::from)
                .filter(|i| *i < in_ch),
            to: inject_to(&session.talkback.to),
        };
        let oscillator = RtOscillator {
            cell: self.oscillator.clone(),
            to: inject_to(&session.oscillator.to),
        };
        let mut graph = Graph::new(in_ch, out_ch, channels, buses, master, monitor, patches)
            .with_matrices(matrices)
            .with_talkback(talkback)
            .with_oscillator(oscillator);
        if let Some(player) = &self.player {
            // Per channel, the file assigned to it (by name: the take's own
            // order is the player's).
            let taps = session
                .channels
                .iter()
                .map(|channel| {
                    let track = session
                        .playback
                        .tracks
                        .iter()
                        .find(|t| t.channel == Some(channel.id))?;
                    let index = player.files().iter().position(|f| f.name == track.file)?;
                    Some(PlaybackTap {
                        track: index,
                        stereo: channel.is_stereo(),
                    })
                })
                .collect();
            graph = graph.with_playback(RtPlayback {
                cell: player.cell().clone(),
                taps,
            });
        }
        self.monitor_source_built = Some(source);
        self.generation += 1;
        graph.generation = self.generation;
        if self.graph_tx.try_send(Box::new(graph)).is_err() {
            // Eight swaps queued and none taken: the audio thread is not
            // running. Nothing to hand it; the next rebuild tries again.
        }
        while self.garbage_rx.try_recv().is_ok() {}
    }

    /// Wait (briefly) until the audio thread runs the latest graph.
    fn wait_for_adoption(&self) {
        if self.streams.is_none() {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.adopted.load(Ordering::Acquire) < self.generation && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn set_audio(&mut self, settings: AudioSettings) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("stop recording before changing the audio device".to_string());
        }
        let old_rate = self.sample_rate;
        self.session.audio = settings;
        // Drop the old streams first: some APIs hold a device exclusively.
        self.streams = None;
        let (graph_tx, graph_rx) = crossbeam_channel::bounded::<Box<Graph>>(8);
        let (garbage_tx, garbage_rx) = crossbeam_channel::bounded::<Box<Graph>>(16);
        self.graph_tx = graph_tx;
        self.garbage_rx = garbage_rx;
        self.adopted.store(0, Ordering::Release);
        self.open_device(graph_rx, garbage_tx);
        if self.sample_rate != old_rate {
            // Every effect was built for the old rate.
            let ids: Vec<Id> = self.inserts.keys().copied().collect();
            for id in ids {
                self.drop_insert(id);
            }
            self.processors.clear();
            // The take's rings were made for the old rate, and the take
            // may not be at the new one.
            if self.player.is_some() {
                self.reopen_playback();
            }
        }
        self.rebuild();
        self.open_error.clone().map_or(Ok(()), Err)
    }

    fn start_recording(&mut self) -> Result<(), String> {
        if self.recording.is_some() {
            return Ok(());
        }
        if self.streams.is_none() {
            return Err("no audio device is running".to_string());
        }
        let strips = recording_strips(&self.session);
        if strips.is_empty() {
            return Err("arm a channel, bus or the master to record".to_string());
        }
        let named: Vec<(String, usize)> = strips
            .iter()
            .map(|(_, name, channels)| (name.clone(), *channels))
            .collect();
        let mut recording = Recording::prepare(&self.session.recording, self.sample_rate, &named)?;
        recording.start(&self.session.recording, self.sample_rate)?;
        self.record_taps = strips
            .iter()
            .zip(&recording.tracks)
            .map(|((strip, _, _), (_, _, stream))| (*strip, stream.clone()))
            .collect();
        self.recording = Some(recording);
        self.rebuild();
        Ok(())
    }

    /// Stop recording and close the files. `None` when nothing was recording.
    pub fn stop_recording(&mut self) -> Option<RecordingSummary> {
        let recording = self.recording.take()?;
        self.record_taps.clear();
        // The rings leave the graph first, so nothing is still being pushed
        // into them when the writer drains them for the last time.
        self.rebuild();
        self.wait_for_adoption();
        let summary = recording.finish();
        self.last_recording = Some(summary.clone());
        Some(summary)
    }

    /// Write the session to `path`, with every third-party plug-in's current
    /// state captured first (up to `timeout` for the host to answer).
    pub fn save(&mut self, path: &std::path::Path, timeout: Duration) -> Result<(), String> {
        #[cfg(feature = "external-plugins")]
        self.capture_external_states(timeout);
        #[cfg(not(feature = "external-plugins"))]
        let _ = timeout;
        self.session.save(path)
    }

    /// Replace the session with `session` (a loaded file). The device is
    /// reopened only if the file names a different one.
    pub fn load(&mut self, mut session: Session) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("stop recording before opening another session".to_string());
        }
        session.normalize();
        let ids: Vec<Id> = self.inserts.keys().copied().collect();
        for id in ids {
            self.drop_insert(id);
        }
        self.strips.clear();
        self.sends.clear();
        self.matrix_sends.clear();
        // Rebuilt settled on the loaded settings.
        self.processors.clear();
        // Another show starts with talkback and the oscillator off.
        self.talk_active = false;
        self.oscillator_on = false;
        self.monitor_source_built = None;
        // And with its own take (if any), opened below at the running rate.
        self.player = None;
        self.playback_error = None;
        let audio = session.audio.clone();
        let reopen = audio != self.session.audio;
        self.session = session;
        // Another show: nothing to undo into.
        self.history.clear();
        self.revision += 1;
        self.scenes_revision += 1;
        self.refresh_gains();
        let opened = if reopen {
            self.set_audio(audio)
        } else {
            Ok(())
        };
        self.reopen_playback();
        self.rebuild();
        opened
    }
}

/// The strips a recording takes, in file order: `(strip, file name,
/// channels)`.
fn recording_strips(session: &Session) -> Vec<(StripRef, String, usize)> {
    let mut strips = Vec::new();
    for channel in session.channels.iter().filter(|c| c.record_arm) {
        let channels = if channel.is_stereo() || session.recording.tap == RecordTap::PostInserts {
            2
        } else {
            1
        };
        strips.push((
            StripRef::Channel(channel.id),
            channel.name.clone(),
            channels,
        ));
    }
    for bus in session.buses.iter().filter(|b| b.record_arm) {
        strips.push((StripRef::Bus(bus.id), bus.name.clone(), 2));
    }
    if session.master.record_arm {
        strips.push((StripRef::Master, "Master".to_string(), 2));
    }
    // As buses: after the fader, two channels.
    for matrix in session.matrices.iter().filter(|m| m.record_arm) {
        strips.push((StripRef::Matrix(matrix.id), matrix.name.clone(), 2));
    }
    strips
}

fn check_dca(dca: usize) -> Result<(), String> {
    if dca < DCA_COUNT {
        Ok(())
    } else {
        Err(format!(
            "there is no DCA {dca}: DCAs are numbered 0 to {}",
            DCA_COUNT - 1
        ))
    }
}

fn check_mute_group(group: usize) -> Result<(), String> {
    if group < MUTE_GROUP_COUNT {
        Ok(())
    } else {
        Err(format!(
            "there is no mute group {group}: mute groups are numbered 0 to {}",
            MUTE_GROUP_COUNT - 1
        ))
    }
}

fn check_color(color: Option<u8>) -> Result<(), String> {
    match color {
        Some(c) if c >= PALETTE_COLORS => Err(format!(
            "colour {c} is not in the palette: pick 0 to {}, or none",
            PALETTE_COLORS - 1
        )),
        _ => Ok(()),
    }
}

/// Add `index` to (or take it out of) a sorted assignment list.
fn assign(list: &mut Vec<usize>, index: usize, assigned: bool) {
    match (list.binary_search(&index), assigned) {
        (Err(at), true) => list.insert(at, index),
        (Ok(at), false) => {
            list.remove(at);
        }
        _ => {}
    }
}

/// Every insert in a mix, strip by strip.
fn all_slots(mix: &MixState) -> impl Iterator<Item = &InsertSlot> {
    mix.channels
        .iter()
        .map(|c| &c.core)
        .chain(mix.buses.iter().map(|b| &b.core))
        .chain(std::iter::once(&mix.master.core))
        .chain(mix.matrices.iter().map(|m| &m.core))
        .flat_map(|core| core.inserts.iter())
}

fn all_slots_mut(mix: &mut MixState) -> impl Iterator<Item = &mut InsertSlot> {
    mix.channels
        .iter_mut()
        .map(|c| &mut c.core)
        .chain(mix.buses.iter_mut().map(|b| &mut b.core))
        .chain(std::iter::once(&mut mix.master.core))
        .chain(mix.matrices.iter_mut().map(|m| &mut m.core))
        .flat_map(|core| core.inserts.iter_mut())
}

/// Whether two mixes compile to graphs of the same shape: the same strips
/// in the same order, inputs, routes, sends (and their taps), insert racks
/// and output patch. Levels, processing and parameters do not count; they
/// reach the audio thread without a rebuild.
fn same_shape(a: &MixState, b: &MixState) -> bool {
    let racks = |x: &crate::session::StripCore, y: &crate::session::StripCore| {
        x.inserts.len() == y.inserts.len()
            && x.inserts.iter().zip(&y.inserts).all(|(s, t)| s.id == t.id)
    };
    a.outputs == b.outputs
        && a.channels.len() == b.channels.len()
        && a.channels.iter().zip(&b.channels).all(|(x, y)| {
            x.id == y.id
                && x.input == y.input
                && x.output == y.output
                && x.sends.len() == y.sends.len()
                && x.sends.iter().zip(&y.sends).all(|(s, t)| {
                    s.bus == t.bus && s.pre_fader == t.pre_fader && s.pan_follow == t.pan_follow
                })
                && racks(&x.core, &y.core)
        })
        && a.buses.len() == b.buses.len()
        && a.buses.iter().zip(&b.buses).all(|(x, y)| {
            x.id == y.id && x.output == y.output && x.stereo == y.stereo && racks(&x.core, &y.core)
        })
        && racks(&a.master.core, &b.master.core)
        && a.matrices.len() == b.matrices.len()
        && a.matrices.iter().zip(&b.matrices).all(|(x, y)| {
            x.id == y.id
                && x.stereo == y.stereo
                && x.sources.len() == y.sources.len()
                && x.sources
                    .iter()
                    .zip(&y.sources)
                    .all(|(s, t)| s.source == t.source)
                && racks(&x.core, &y.core)
        })
}

/// Whether `command` is an undo step. Everything that changes the mix is;
/// not solo, record arm, recording, the audio device, the monitor, recall
/// safe, or the scene list (store, rename, note, scope, delete, move) —
/// a recall is.
fn is_undoable(command: &Command) -> bool {
    match command {
        Command::SetFader { .. }
        | Command::SetPan { .. }
        | Command::SetMute { .. }
        | Command::SetTrim { .. }
        | Command::SetPhaseInvert { .. }
        | Command::AddChannel { .. }
        | Command::RemoveChannel { .. }
        | Command::RenameStrip { .. }
        | Command::SetChannelInput { .. }
        | Command::SetStripOutput { .. }
        | Command::AddBus { .. }
        | Command::RemoveBus { .. }
        | Command::SetSend { .. }
        | Command::RemoveSend { .. }
        | Command::AddInsert { .. }
        | Command::RemoveInsert { .. }
        | Command::MoveInsert { .. }
        | Command::SetInsertBypass { .. }
        | Command::SetInsertParam { .. }
        | Command::SetInsertParams { .. }
        | Command::SetOutputPatch { .. }
        | Command::SetProcessing { .. }
        | Command::SetStripColor { .. }
        | Command::SetSoloSafe { .. }
        | Command::AssignDca { .. }
        | Command::SetDcaLevel { .. }
        | Command::SetDcaMute { .. }
        | Command::RenameDca { .. }
        | Command::SetDcaColor { .. }
        | Command::AssignMuteGroup { .. }
        | Command::SetMuteGroup { .. }
        | Command::RenameMuteGroup { .. }
        | Command::MoveChannel { .. }
        | Command::MoveBus { .. }
        | Command::SceneRecall { .. }
        | Command::PasteStrip { .. }
        | Command::SetBusRole { .. }
        | Command::SetBusStereo { .. }
        | Command::AddMatrix { .. }
        | Command::RemoveMatrix { .. }
        | Command::SetMatrixSend { .. }
        | Command::MoveMatrix { .. }
        | Command::SetMatrixStereo { .. }
        | Command::SetLayer { .. }
        | Command::RemoveLayer { .. } => true,
        Command::SetTalkback { .. }
        | Command::Talk { .. }
        | Command::SetOscillator { .. }
        | Command::OscillatorOn { .. }
        | Command::SetSolo { .. }
        | Command::ClearSolo
        | Command::SetRecordArm { .. }
        | Command::SetRecordSettings { .. }
        | Command::SetAudio { .. }
        | Command::StartRecording
        | Command::StopRecording
        | Command::SetMonitor { .. }
        | Command::Undo
        | Command::Redo
        | Command::SceneStore { .. }
        | Command::SceneRename { .. }
        | Command::SceneNote { .. }
        | Command::SceneScope { .. }
        | Command::SceneDelete { .. }
        | Command::SceneMove { .. }
        | Command::SetRecallSafe { .. }
        | Command::PlaybackLoad { .. }
        | Command::PlaybackAssign { .. }
        | Command::PlaybackUnload
        | Command::Playback { .. }
        | Command::PlaybackLocate { .. }
        | Command::PlaybackLoop { .. }
        | Command::SetVirtualSoundcheck { .. } => false,
    }
}

/// The processing section a change touches, for its label and coalescing.
fn processing_section(before: &Processing, after: &Processing) -> &'static str {
    let changed = [
        (before.hpf != after.hpf, "HPF"),
        (before.gate != after.gate, "Gate"),
        (before.eq != after.eq, "EQ"),
        (before.comp != after.comp, "Comp"),
        (before.delay != after.delay, "Delay"),
    ];
    let mut touched = changed.iter().filter(|(changed, _)| *changed);
    match (touched.next(), touched.next()) {
        (Some((_, name)), None) => name,
        _ => "Processing",
    }
}

/// What a continuous edit moves, for coalescing; `None` for one-shot edits.
fn undo_target(command: &Command, session: &Session) -> Option<EditTarget> {
    Some(match command {
        Command::SetFader { strip, .. } => EditTarget::Fader(*strip),
        Command::SetPan { strip, .. } => EditTarget::Pan(*strip),
        Command::SetTrim { channel, .. } => EditTarget::Trim(*channel),
        Command::SetSend { channel, bus, .. } => EditTarget::Send(*channel, *bus),
        Command::SetMatrixSend { matrix, source, .. } => EditTarget::MatrixSend(*matrix, *source),
        Command::SetProcessing { strip, processing } => {
            let before = session.strip(*strip)?.processing;
            EditTarget::Processing(*strip, processing_section(&before, processing))
        }
        Command::SetInsertParam { insert, index, .. } => EditTarget::InsertParam(*insert, *index),
        Command::SetInsertParams { insert, values } => {
            EditTarget::InsertParams(*insert, values.iter().map(|(index, _)| *index).collect())
        }
        Command::SetDcaLevel { dca, .. } => EditTarget::DcaLevel(*dca),
        _ => return None,
    })
}

/// A strip as people name it.
fn strip_name(session: &Session, strip: StripRef) -> String {
    let named = |name: &str, fallback: String| {
        if name.trim().is_empty() {
            fallback
        } else {
            name.to_string()
        }
    };
    match strip {
        StripRef::Channel(id) => session.channel(id).map_or_else(
            || format!("channel {id}"),
            |c| named(&c.name, format!("channel {id}")),
        ),
        StripRef::Bus(id) => session.bus(id).map_or_else(
            || format!("bus {id}"),
            |b| named(&b.name, format!("bus {id}")),
        ),
        StripRef::Matrix(id) => session.matrix(id).map_or_else(
            || format!("matrix {id}"),
            |m| named(&m.name, format!("matrix {id}")),
        ),
        StripRef::Master => "Master".to_string(),
    }
}

/// An insert's plug-in name and the strip it is on.
fn insert_names(session: &Session, insert: Id) -> (String, String) {
    for (strip, core) in session.strip_cores() {
        if let Some(slot) = core.inserts.iter().find(|s| s.id == insert) {
            return (slot.plugin.display_name(), strip_name(session, strip));
        }
    }
    (format!("insert {insert}"), String::new())
}

/// The label of an undo step, read before the edit runs.
fn undo_label(command: &Command, session: &Session) -> String {
    let strip = |s: &StripRef| strip_name(session, *s);
    let channel = |id: &Id| strip_name(session, StripRef::Channel(*id));
    let dca = |i: &usize| {
        session
            .dcas
            .get(*i)
            .map_or_else(|| format!("DCA {}", i + 1), |d| d.name.clone())
    };
    let group = |i: &usize| {
        session
            .mute_groups
            .get(*i)
            .map_or_else(|| format!("Mute {}", i + 1), |g| g.name.clone())
    };
    let on_off = |on: bool| if on { "on" } else { "off" };
    match command {
        Command::SetFader { strip: s, .. } => format!("Fader {}", strip(s)),
        Command::SetPan { strip: s, .. } => format!("Pan {}", strip(s)),
        Command::SetMute { strip: s, mute } => {
            format!("{} {}", if *mute { "Mute" } else { "Unmute" }, strip(s))
        }
        Command::SetTrim { channel: c, .. } => format!("Trim {}", channel(c)),
        Command::SetPhaseInvert { channel: c, .. } => format!("Polarity {}", channel(c)),
        Command::AddChannel { name, .. } => format!("Add channel {name}"),
        Command::RemoveChannel { channel: c } => format!("Remove channel {}", channel(c)),
        Command::RenameStrip { strip: s, .. } => format!("Rename {}", strip(s)),
        Command::SetChannelInput { channel: c, .. } => format!("Input patch {}", channel(c)),
        Command::SetStripOutput { strip: s, .. } => format!("Route {}", strip(s)),
        Command::AddBus { name, .. } => format!("Add bus {name}"),
        Command::RemoveBus { bus } => format!("Remove bus {}", strip(&StripRef::Bus(*bus))),
        Command::SetSend {
            channel: c, bus, ..
        } => format!("Send {} to {}", channel(c), strip(&StripRef::Bus(*bus))),
        Command::RemoveSend { channel: c, bus } => format!(
            "Remove send {} to {}",
            channel(c),
            strip(&StripRef::Bus(*bus))
        ),
        Command::AddInsert {
            strip: s, plugin, ..
        } => {
            format!("Add {} to {}", plugin.display_name(), strip(s))
        }
        Command::RemoveInsert { insert, .. } => {
            let (plugin, on) = insert_names(session, *insert);
            format!("Remove {plugin} from {on}")
        }
        Command::MoveInsert { insert, .. } => {
            let (plugin, on) = insert_names(session, *insert);
            format!("Move {plugin} on {on}")
        }
        Command::SetInsertBypass { insert, bypass } => {
            let (plugin, on) = insert_names(session, *insert);
            format!(
                "{} {plugin} on {on}",
                if *bypass { "Bypass" } else { "Enable" }
            )
        }
        Command::SetInsertParam { insert, .. } | Command::SetInsertParams { insert, .. } => {
            let (plugin, on) = insert_names(session, *insert);
            format!("{plugin} {on}")
        }
        Command::SetOutputPatch { .. } => "Output patch".to_string(),
        Command::SetProcessing {
            strip: s,
            processing,
        } => {
            let section = session.strip(*s).map_or("Processing", |core| {
                processing_section(&core.processing, processing)
            });
            format!("{section} {}", strip(s))
        }
        Command::SetStripColor { strip: s, .. } => format!("Colour {}", strip(s)),
        Command::SetSoloSafe { strip: s, safe } => {
            format!("Solo safe {} {}", strip(s), on_off(*safe))
        }
        Command::AssignDca {
            strip: s,
            dca: d,
            assigned,
        } => format!(
            "{} {} {} {}",
            if *assigned { "Assign" } else { "Unassign" },
            strip(s),
            if *assigned { "to" } else { "from" },
            dca(d)
        ),
        Command::SetDcaLevel { dca: d, .. } => format!("Fader {}", dca(d)),
        Command::SetDcaMute { dca: d, mute } => {
            format!("{} {}", if *mute { "Mute" } else { "Unmute" }, dca(d))
        }
        Command::RenameDca { dca: d, .. } => format!("Rename {}", dca(d)),
        Command::SetDcaColor { dca: d, .. } => format!("Colour {}", dca(d)),
        Command::AssignMuteGroup {
            strip: s,
            group: g,
            assigned,
        } => format!(
            "{} {} {} {}",
            if *assigned { "Assign" } else { "Unassign" },
            strip(s),
            if *assigned { "to" } else { "from" },
            group(g)
        ),
        Command::SetMuteGroup { group: g, active } => format!("{} {}", group(g), on_off(*active)),
        Command::RenameMuteGroup { group: g, .. } => format!("Rename {}", group(g)),
        Command::MoveChannel { channel: c, .. } => format!("Move {}", channel(c)),
        Command::MoveBus { bus, .. } => format!("Move {}", strip(&StripRef::Bus(*bus))),
        Command::SceneRecall { id } => format!(
            "Recall {}",
            session
                .scene(*id)
                .map_or_else(|| format!("scene {id}"), |s| s.name.clone())
        ),
        Command::PasteStrip { targets, .. } => match targets.as_slice() {
            [one] => format!("Paste to {}", strip(one)),
            many => format!("Paste to {} strips", many.len()),
        },
        Command::SetBusRole { bus, role } => format!(
            "{} {}",
            strip(&StripRef::Bus(*bus)),
            match role {
                BusRole::Aux => "to aux",
                BusRole::Group => "to group",
                BusRole::Fx => "to FX",
            }
        ),
        Command::SetBusStereo { bus, stereo } => format!(
            "{} {}",
            strip(&StripRef::Bus(*bus)),
            if *stereo { "stereo" } else { "mono" }
        ),
        Command::AddMatrix { name, .. } => format!("Add matrix {name}"),
        Command::RemoveMatrix { matrix } => {
            format!("Remove matrix {}", strip(&StripRef::Matrix(*matrix)))
        }
        Command::SetMatrixSend { matrix, source, .. } => format!(
            "Send {} to {}",
            match source {
                MatrixFeed::Master => "Master".to_string(),
                MatrixFeed::Bus(bus) => strip(&StripRef::Bus(*bus)),
            },
            strip(&StripRef::Matrix(*matrix))
        ),
        Command::MoveMatrix { matrix, .. } => format!("Move {}", strip(&StripRef::Matrix(*matrix))),
        Command::SetMatrixStereo { matrix, stereo } => format!(
            "{} {}",
            strip(&StripRef::Matrix(*matrix)),
            if *stereo { "stereo" } else { "mono" }
        ),
        Command::SetLayer { index, name, .. } => {
            if *index < session.layers.len() {
                format!("Layer {name}")
            } else {
                format!("Add layer {name}")
            }
        }
        Command::RemoveLayer { index } => format!(
            "Remove layer {}",
            session
                .layers
                .get(*index)
                .map_or_else(|| (index + 1).to_string(), |l| l.name.clone())
        ),
        // Not undo steps; named for completeness.
        _ => "Edit".to_string(),
    }
}

/// Where a send is taken and its per-side gains. `level` is the channel's
/// heard gain (fader × DCAs; 0 while muted or silenced by solo in place).
///
/// - Pre-fader: before the fader, at the send level. A pan of its own
///   (into a stereo bus, not following) by the channel's pan law; following,
///   unpanned, as pre-fader sends always were.
/// - Post-fader following the channel into a stereo bus: after the fader
///   and the channel's pan, as post-fader sends always were.
/// - Post-fader with a pan of its own, or into a mono bus: before the fader
///   with the channel's level in the gains, so the channel's pan is not
///   taken.
fn send_shape(
    send: &SendSlot,
    channel_stereo: bool,
    bus_stereo: bool,
    level: f32,
) -> (SendTap, (f32, f32)) {
    let gain = db_to_gain(send.level_db);
    let (l, r) = if bus_stereo && !send.pan_follow {
        if channel_stereo {
            balance(send.pan)
        } else {
            constant_power(send.pan)
        }
    } else {
        (1.0, 1.0)
    };
    if send.pre_fader {
        (SendTap::BeforeFader, (gain * l, gain * r))
    } else if bus_stereo && send.pan_follow {
        (SendTap::AfterFader, (gain, gain))
    } else {
        (SendTap::BeforeFader, (gain * level * l, gain * level * r))
    }
}

/// Stereo balance: the far side is turned down, the near side stays.
fn balance(pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0))
}

/// Mono pan, −3 dB in the centre.
fn constant_power(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

#[cfg(feature = "external-plugins")]
impl LiveEngine {
    fn poll_external(&mut self) {
        use crate::external::ExternalEvent;
        let Some(host) = self.external.as_mut() else {
            return;
        };
        for event in host.poll() {
            match &event {
                ExternalEvent::Loaded { insert, .. } => {
                    self.insert_states.insert(*insert, InsertState::Ready);
                }
                ExternalEvent::LoadFailed { insert, error } => {
                    self.insert_states
                        .insert(*insert, InsertState::Failed(error.clone()));
                }
                ExternalEvent::State {
                    insert,
                    component,
                    controller,
                } => {
                    let state = (component.clone(), controller.clone());
                    if let Ok(slot) = self.insert_slot_mut(*insert) {
                        if let InsertPlugin::External { state: saved, .. } = &mut slot.plugin {
                            *saved = Some(state);
                        }
                    }
                }
                ExternalEvent::HostLost => {
                    for slot in self.all_insert_slots() {
                        if matches!(slot.plugin, InsertPlugin::External { .. }) {
                            self.insert_states.insert(
                                slot.id,
                                InsertState::Failed("the plug-in host stopped".to_string()),
                            );
                        }
                    }
                }
                _ => {}
            }
            self.external_events.push(event);
        }
    }

    /// Editor and load events for the UI, since the last call.
    pub fn take_external_events(&mut self) -> Vec<crate::external::ExternalEvent> {
        std::mem::take(&mut self.external_events)
    }

    /// Reload every third-party insert after the host went away.
    pub fn restart_external_host(&mut self) {
        let external: Vec<Id> = self
            .all_insert_slots()
            .into_iter()
            .filter(|slot| matches!(slot.plugin, InsertPlugin::External { .. }))
            .map(|slot| slot.id)
            .collect();
        for id in &external {
            self.inserts.remove(id);
            self.insert_states.remove(id);
        }
        self.external = None;
        self.rebuild();
    }

    /// Attach `insert`'s editor into native window `parent`.
    pub fn open_external_editor(
        &mut self,
        insert: Id,
        parent: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), String> {
        let slot = self.insert_slot_mut(insert)?.clone();
        let InsertPlugin::External { path, class_id, .. } = &slot.plugin else {
            return Err("not a third-party plug-in".to_string());
        };
        let host = self
            .external
            .as_mut()
            .ok_or_else(|| "the plug-in host is not running".to_string())?;
        host.open_editor(insert, path, class_id, parent, width, height, dpi)
    }

    pub fn resize_external_editor(&mut self, insert: Id, width: u32, height: u32, dpi: u32) {
        if let Some(host) = self.external.as_mut() {
            host.resize_editor(insert, width, height, dpi);
        }
    }

    pub fn close_external_editor(&mut self, insert: Id) {
        if let Some(host) = self.external.as_mut() {
            host.close_editor(insert);
        }
    }

    fn capture_external_states(&mut self, timeout: Duration) {
        let all: Vec<Id> = self.all_insert_slots().iter().map(|s| s.id).collect();
        self.capture_states_of(&all, timeout);
    }

    /// Before an undoable edit removes third-party inserts, ask them for
    /// their state (briefly), so an undo brings them back as they were.
    fn capture_before_removal(&mut self, command: &Command) {
        let rack = |core: &crate::session::StripCore| -> Vec<Id> {
            core.inserts.iter().map(|s| s.id).collect()
        };
        let ids: Vec<Id> = match command {
            Command::RemoveInsert { insert, .. } => vec![*insert],
            Command::RemoveChannel { channel } => self
                .session
                .channel(*channel)
                .map(|c| rack(&c.core))
                .unwrap_or_default(),
            Command::RemoveBus { bus } => self
                .session
                .bus(*bus)
                .map(|b| rack(&b.core))
                .unwrap_or_default(),
            Command::RemoveMatrix { matrix } => self
                .session
                .matrix(*matrix)
                .map(|m| rack(&m.core))
                .unwrap_or_default(),
            Command::PasteStrip {
                targets, sections, ..
            } if sections.contains(&Section::Inserts) => targets
                .iter()
                .filter_map(|t| self.session.strip(*t))
                .flat_map(rack)
                .collect(),
            _ => return,
        };
        if !ids.is_empty() {
            self.capture_states_of(&ids, Duration::from_millis(250));
        }
    }

    fn capture_states_of(&mut self, ids: &[Id], timeout: Duration) {
        use crate::external::ExternalEvent;
        let external: Vec<Id> = self
            .all_insert_slots()
            .into_iter()
            .filter(|slot| ids.contains(&slot.id))
            .filter(|slot| matches!(slot.plugin, InsertPlugin::External { .. }))
            .filter(|slot| self.insert_state(slot.id) == InsertState::Ready)
            .map(|slot| slot.id)
            .collect();
        let Some(host) = self.external.as_mut() else {
            return;
        };
        if external.is_empty() {
            return;
        }
        for id in &external {
            host.request_state(*id);
        }
        let mut waiting: std::collections::HashSet<Id> = external.into_iter().collect();
        let deadline = Instant::now() + timeout;
        while !waiting.is_empty() && Instant::now() < deadline {
            self.poll_external();
            for event in &self.external_events {
                if let ExternalEvent::State { insert, .. } = event {
                    waiting.remove(insert);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pan_laws_meet_at_the_centre_and_reach_the_sides() {
        let (l, r) = constant_power(0.0);
        assert!((l - r).abs() < 1e-6 && (l - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        let (l, r) = constant_power(-1.0);
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6);
        assert_eq!(balance(0.0), (1.0, 1.0));
        assert_eq!(balance(1.0), (0.0, 1.0));
        assert_eq!(balance(-0.5), (1.0, 0.5));
    }

    #[test]
    fn recording_takes_armed_strips_in_order_at_their_width() {
        let mut session = Session::with_inputs(&["Vox".to_string(), "Kick".to_string()]);
        session.channels[0].record_arm = true;
        session.master.record_arm = true;
        let strips = recording_strips(&session);
        assert_eq!(
            strips,
            vec![
                (
                    StripRef::Channel(session.channels[0].id),
                    "Vox".to_string(),
                    1
                ),
                (StripRef::Master, "Master".to_string(), 2),
            ]
        );
        session.recording.tap = RecordTap::PostInserts;
        assert_eq!(recording_strips(&session)[0].2, 2);
    }

    const CENTRE: f32 = std::f32::consts::FRAC_1_SQRT_2;

    /// An engine without a device, two mono inputs ("A", "B") at 0 dB, the
    /// master on outputs 1/2 and the monitor bus on outputs 3/4, playing
    /// constant inputs (A = 0.5, B = 0.25) through the graph it builds.
    struct Rig {
        engine: LiveEngine,
        graphs: Receiver<Box<Graph>>,
        graph: Option<Box<Graph>>,
        /// Time zero of [`Rig::at`]'s clock.
        start: Instant,
    }

    impl Rig {
        fn new() -> Self {
            Self::with_outputs(4)
        }

        /// As [`Rig::new`], with `outputs` interface outputs (at least 4).
        fn with_outputs(outputs: usize) -> Self {
            let mut session = Session::with_inputs(&["A".to_string(), "B".to_string()]);
            for channel in &mut session.channels {
                channel.core.fader_db = 0.0;
                // Every section off: the strip passes its input through.
                channel.core.processing.eq.on = false;
            }
            session.master.core.processing.eq.on = false;
            session.outputs.push(OutputPatch {
                source: PatchSource::Monitor,
                left: 2,
                right: Some(3),
            });
            let (engine, graphs) = LiveEngine::offline(session, 2, outputs);
            let mut rig = Self {
                engine,
                graphs,
                graph: None,
                start: Instant::now(),
            };
            rig.adopt();
            rig
        }

        /// Apply `command` as if at `ms` milliseconds on the rig's clock.
        fn at(&mut self, ms: u64, command: Command) -> Option<String> {
            let now = self.start + Duration::from_millis(ms);
            let note = self.engine.apply_at(command, now).unwrap();
            self.adopt();
            note
        }

        fn undo(&mut self) -> Option<String> {
            let note = self.engine.apply_with_note(Command::Undo).unwrap();
            self.adopt();
            note
        }

        fn redo(&mut self) -> Option<String> {
            let note = self.engine.apply_with_note(Command::Redo).unwrap();
            self.adopt();
            note
        }

        fn id(&self, index: usize) -> Id {
            self.engine.session().channels[index].id
        }

        fn core(&self, strip: StripRef) -> &crate::session::StripCore {
            self.engine.session().strip(strip).unwrap()
        }

        fn adopt(&mut self) {
            while let Ok(graph) = self.graphs.try_recv() {
                self.graph = Some(graph);
            }
        }

        fn apply(&mut self, command: Command) {
            self.engine.apply(command).unwrap();
            self.adopt();
        }

        fn channel(&self, index: usize) -> StripRef {
            StripRef::Channel(self.engine.session().channels[index].id)
        }

        /// The last frame of two blocks (the first ramps to any new gain):
        /// `[master L, master R, monitor L, monitor R]`.
        fn play(&mut self) -> [f32; 4] {
            let out = self.frame(2);
            [out[0], out[1], out[2], out[3]]
        }

        /// The last frame (every output) of `blocks` blocks of 128.
        fn frame(&mut self, blocks: usize) -> Vec<f32> {
            let width = self.graph.as_ref().expect("a graph was built").out_channels;
            let output = self.render(blocks * 128);
            output[output.len() - width..].to_vec()
        }

        /// `frames` frames of the constant inputs, in blocks of 128:
        /// the interleaved output.
        fn render(&mut self, frames: usize) -> Vec<f32> {
            let graph = self.graph.as_mut().expect("a graph was built");
            let width = graph.out_channels;
            let block = 128;
            let input: Vec<f32> = (0..block).flat_map(|_| [0.5, 0.25]).collect();
            let mut all = Vec::with_capacity(frames * width);
            let mut output = vec![0.0; block * width];
            let mut done = 0;
            while done < frames {
                let n = block.min(frames - done);
                graph.process(&input[..n * 2], &mut output[..n * width]);
                all.extend_from_slice(&output[..n * width]);
                done += n;
            }
            all
        }

        /// One output's samples from an interleaved render.
        fn plane(output: &[f32], width: usize, channel: usize) -> Vec<f32> {
            output
                .iter()
                .skip(channel)
                .step_by(width)
                .copied()
                .collect()
        }
    }

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1.0e-4
    }

    #[test]
    fn a_dca_offsets_its_members_and_mutes_them() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let both = (0.5 + 0.25) * CENTRE;
        assert!(close(rig.play()[0], both));

        rig.apply(Command::AssignDca {
            strip: a,
            dca: 2,
            assigned: true,
        });
        // −6.02 dB: half.
        rig.apply(Command::SetDcaLevel {
            dca: 2,
            db: 20.0 * 0.5f32.log10(),
        });
        let out = rig.play();
        assert!(close(out[0], (0.25 + 0.25) * CENTRE), "{out:?}");
        assert_eq!(rig.engine.session().strip(a).unwrap().fader_db, 0.0);

        // Two DCAs add: −6.02 + 6.02 dB = the fader alone.
        rig.apply(Command::AssignDca {
            strip: a,
            dca: 5,
            assigned: true,
        });
        rig.apply(Command::SetDcaLevel {
            dca: 5,
            db: 20.0 * 2f32.log10(),
        });
        assert!(close(rig.play()[0], both));

        rig.apply(Command::SetDcaMute { dca: 5, mute: true });
        assert!(close(rig.play()[0], 0.25 * CENTRE));
        rig.apply(Command::SetDcaMute {
            dca: 5,
            mute: false,
        });
        rig.apply(Command::SetDcaLevel {
            dca: 2,
            db: MIN_FADER_DB,
        });
        assert!(close(rig.play()[0], 0.25 * CENTRE));

        // Unassigned, the DCA no longer touches it.
        rig.apply(Command::AssignDca {
            strip: a,
            dca: 2,
            assigned: false,
        });
        rig.apply(Command::SetDcaLevel { dca: 5, db: 0.0 });
        assert!(close(rig.play()[0], both));
        assert_eq!(rig.engine.session().strip(a).unwrap().dcas, vec![5]);
    }

    #[test]
    fn a_mute_group_mutes_its_members_while_active() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        for strip in [a, b] {
            rig.apply(Command::AssignMuteGroup {
                strip,
                group: 3,
                assigned: true,
            });
        }
        assert!(close(rig.play()[0], 0.75 * CENTRE));
        rig.apply(Command::SetMuteGroup {
            group: 3,
            active: true,
        });
        assert!(close(rig.play()[0], 0.0));
        rig.apply(Command::AssignMuteGroup {
            strip: b,
            group: 3,
            assigned: false,
        });
        assert!(close(rig.play()[0], 0.25 * CENTRE));
        rig.apply(Command::SetMuteGroup {
            group: 3,
            active: false,
        });
        assert!(close(rig.play()[0], 0.75 * CENTRE));
    }

    #[test]
    fn with_nothing_soloed_the_monitor_carries_the_master_at_its_level() {
        let mut rig = Rig::new();
        rig.apply(Command::SetFader {
            strip: StripRef::Master,
            db: 20.0 * 0.5f32.log10(),
        });
        let out = rig.play();
        let master = 0.75 * CENTRE * 0.5;
        assert!(close(out[0], master) && close(out[1], master), "{out:?}");
        assert!(close(out[2], master) && close(out[3], master), "{out:?}");

        rig.apply(Command::SetMonitor {
            monitor: MonitorSettings {
                solo_mode: SoloMode::Pfl,
                level_db: 20.0 * 0.5f32.log10(),
                dim: true,
                ..MonitorSettings::default()
            },
        });
        let out = rig.play();
        assert!(close(out[0], master), "the main mix is untouched");
        assert!(close(out[2], master * 0.5 * 0.1), "{out:?}");
        // The meter holds the peak since it was last read: the ramp down
        // included. Read it once, then once more after a steady stretch.
        let _ = rig.engine.monitor_meter();
        rig.play();
        let (l, r) = rig.engine.monitor_meter();
        assert!(close(l, master * 0.05) && close(r, master * 0.05));
    }

    #[test]
    fn pfl_leaves_the_main_mix_alone_and_fills_the_monitor_before_the_fader() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        rig.apply(Command::SetFader {
            strip: a,
            db: 20.0 * 0.5f32.log10(),
        });
        rig.apply(Command::SetPan { strip: a, pan: 1.0 });
        let main = rig.play();
        rig.apply(Command::SetSolo {
            strip: a,
            solo: true,
        });
        let out = rig.play();
        assert!(close(out[0], main[0]) && close(out[1], main[1]), "{out:?}");
        // A alone, before its fader and pan: a mono strip centred.
        assert!(close(out[2], 0.5) && close(out[3], 0.5), "{out:?}");

        rig.apply(Command::ClearSolo);
        let out = rig.play();
        assert!(close(out[2], main[0]) && close(out[3], main[1]), "{out:?}");
        assert!(!rig.engine.session().any_solo());
    }

    #[test]
    fn afl_takes_the_soloed_strip_after_its_fader_and_pan() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        rig.apply(Command::SetMonitor {
            monitor: MonitorSettings {
                solo_mode: SoloMode::Afl,
                ..MonitorSettings::default()
            },
        });
        rig.apply(Command::SetFader {
            strip: a,
            db: 20.0 * 0.5f32.log10(),
        });
        rig.apply(Command::SetPan {
            strip: a,
            pan: -1.0,
        });
        let main = rig.play();
        rig.apply(Command::SetSolo {
            strip: a,
            solo: true,
        });
        let out = rig.play();
        assert!(close(out[0], main[0]) && close(out[1], main[1]));
        assert!(close(out[2], 0.25) && close(out[3], 0.0), "{out:?}");
        // Two soloed strips sum.
        rig.apply(Command::SetSolo {
            strip: b,
            solo: true,
        });
        let out = rig.play();
        assert!(close(out[2], 0.25 + 0.25 * CENTRE), "{out:?}");
        assert!(close(out[3], 0.25 * CENTRE), "{out:?}");
    }

    #[test]
    fn solo_in_place_silences_the_rest_but_not_a_solo_safe_strip() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        rig.apply(Command::SetMonitor {
            monitor: MonitorSettings {
                solo_mode: SoloMode::Sip,
                ..MonitorSettings::default()
            },
        });
        rig.apply(Command::SetSolo {
            strip: a,
            solo: true,
        });
        let out = rig.play();
        assert!(close(out[0], 0.5 * CENTRE), "{out:?}");
        assert!(close(out[2], out[0]), "the monitor carries the master");
        rig.apply(Command::SetSoloSafe {
            strip: b,
            safe: true,
        });
        let out = rig.play();
        assert!(close(out[0], 0.75 * CENTRE), "{out:?}");
        assert!(close(out[2], out[0]));
        // Switching to PFL with the solo held: the main mix is whole again,
        // the monitor hears A alone.
        rig.apply(Command::SetMonitor {
            monitor: MonitorSettings::default(),
        });
        let out = rig.play();
        assert!(
            close(out[0], 0.75 * CENTRE) && close(out[2], 0.5),
            "{out:?}"
        );
    }

    #[test]
    fn moving_strips_reorders_them_and_keeps_their_processing() {
        let mut rig = Rig::new();
        rig.apply(Command::AddBus {
            name: "X".to_string(),
            role: None,
            stereo: None,
        });
        rig.apply(Command::AddBus {
            name: "Y".to_string(),
            role: None,
            stereo: None,
        });
        let ids: Vec<Id> = rig.engine.session().channels.iter().map(|c| c.id).collect();
        let before = rig.graph.as_ref().unwrap().channels[0]
            .strip
            .processor
            .clone();
        rig.apply(Command::MoveChannel {
            channel: ids[0],
            index: 99,
        });
        let order: Vec<Id> = rig.engine.session().channels.iter().map(|c| c.id).collect();
        assert_eq!(order, vec![ids[1], ids[0]]);
        let graph = rig.graph.as_ref().unwrap();
        assert!(Arc::ptr_eq(&graph.channels[1].strip.processor, &before));
        // Same mix, whatever the order.
        assert!(close(rig.play()[0], 0.75 * CENTRE));

        let buses: Vec<Id> = rig.engine.session().buses.iter().map(|b| b.id).collect();
        rig.apply(Command::MoveBus {
            bus: buses[1],
            index: 0,
        });
        let order: Vec<Id> = rig.engine.session().buses.iter().map(|b| b.id).collect();
        assert_eq!(order, vec![buses[1], buses[0]]);
        assert!(
            rig.engine
                .apply(Command::MoveBus { bus: 999, index: 0 })
                .is_err()
        );
    }

    #[test]
    fn a_processor_survives_a_graph_rebuild_and_takes_new_settings() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let cell = |rig: &Rig| {
            rig.graph.as_ref().unwrap().channels[0]
                .strip
                .processor
                .clone()
        };
        let master = |rig: &Rig| rig.graph.as_ref().unwrap().master.processor.clone();
        let (first, first_master) = (cell(&rig), master(&rig));
        let generation = rig.graph.as_ref().unwrap().generation;
        // Rebuilds: a bus, an insert, a route.
        rig.apply(Command::AddBus {
            name: "Verb".to_string(),
            role: None,
            stereo: None,
        });
        rig.apply(Command::AddInsert {
            strip: a,
            plugin: InsertPlugin::Builtin {
                stem: "equz8".to_string(),
                params: Default::default(),
            },
            index: None,
        });
        assert!(rig.graph.as_ref().unwrap().generation > generation);
        assert!(Arc::ptr_eq(&cell(&rig), &first));
        assert!(Arc::ptr_eq(&master(&rig), &first_master));

        let mut processing = Processing::default();
        processing.comp.on = true;
        processing.comp.ratio = 50.0;
        rig.apply(Command::SetProcessing {
            strip: a,
            processing,
        });
        let sent = first.take_latest().expect("the settings reached the cell");
        assert!(sent.comp.on);
        assert_eq!(sent.comp.ratio, 20.0, "clamped to the documented range");
        assert_eq!(
            rig.engine.session().strip(a).unwrap().processing.comp.ratio,
            20.0
        );

        // A loaded session starts over, settled on what it saved.
        let session = rig.engine.session().clone();
        rig.engine.load(session).unwrap();
        rig.adopt();
        assert!(!Arc::ptr_eq(&cell(&rig), &first));
        assert_eq!(cell(&rig).take_latest(), None);
    }

    #[test]
    fn assignments_and_values_are_checked() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let fails = |rig: &mut Rig, command: Command, says: &str| {
            let error = rig.engine.apply(command).unwrap_err();
            assert!(error.contains(says), "{error}");
        };
        fails(
            &mut rig,
            Command::AssignDca {
                strip: a,
                dca: 8,
                assigned: true,
            },
            "no DCA 8",
        );
        fails(
            &mut rig,
            Command::AssignDca {
                strip: StripRef::Master,
                dca: 0,
                assigned: true,
            },
            "master",
        );
        fails(
            &mut rig,
            Command::AssignMuteGroup {
                strip: StripRef::Master,
                group: 0,
                assigned: true,
            },
            "master",
        );
        fails(
            &mut rig,
            Command::SetMuteGroup {
                group: 8,
                active: true,
            },
            "no mute group 8",
        );
        fails(
            &mut rig,
            Command::SetStripColor {
                strip: a,
                color: Some(12),
            },
            "palette",
        );
        fails(
            &mut rig,
            Command::SetDcaColor {
                dca: 0,
                color: Some(200),
            },
            "palette",
        );
        fails(
            &mut rig,
            Command::SetSoloSafe {
                strip: StripRef::Master,
                safe: true,
            },
            "master",
        );
        let mut nan = Processing::default();
        nan.eq.bands[2].gain_db = f32::NAN;
        fails(
            &mut rig,
            Command::SetProcessing {
                strip: a,
                processing: nan,
            },
            "number",
        );
        fails(
            &mut rig,
            Command::AssignDca {
                strip: StripRef::Channel(999),
                dca: 0,
                assigned: true,
            },
            "no strip",
        );
        // Assigning twice keeps one entry; colours and names land.
        for _ in 0..2 {
            rig.apply(Command::AssignDca {
                strip: a,
                dca: 4,
                assigned: true,
            });
        }
        rig.apply(Command::SetStripColor {
            strip: a,
            color: Some(11),
        });
        rig.apply(Command::RenameDca {
            dca: 4,
            name: "Drums".to_string(),
        });
        rig.apply(Command::RenameMuteGroup {
            group: 0,
            name: "Band".to_string(),
        });
        let session = rig.engine.session();
        assert_eq!(session.strip(a).unwrap().dcas, vec![4]);
        assert_eq!(session.strip(a).unwrap().color, Some(11));
        assert_eq!(session.dcas[4].name, "Drums");
        assert_eq!(session.mute_groups[0].name, "Band");
    }

    /// Every command in the console contract parses as the web UI sends it.
    #[test]
    fn the_console_commands_parse_from_json() {
        let processing = serde_json::to_string(&Processing::default()).unwrap();
        let lines = [
            format!(r#"{{"cmd":"set_processing","strip":{{"kind":"channel","id":3}},"processing":{processing}}}"#),
            r#"{"cmd":"set_strip_color","strip":{"kind":"bus","id":4},"color":null}"#.to_string(),
            r#"{"cmd":"set_strip_color","strip":{"kind":"master"},"color":7}"#.to_string(),
            r#"{"cmd":"set_solo_safe","strip":{"kind":"channel","id":3},"safe":true}"#.to_string(),
            r#"{"cmd":"clear_solo"}"#.to_string(),
            r#"{"cmd":"assign_dca","strip":{"kind":"channel","id":3},"dca":0,"assigned":true}"#.to_string(),
            r#"{"cmd":"set_dca_level","dca":0,"db":-6.0}"#.to_string(),
            r#"{"cmd":"set_dca_mute","dca":0,"mute":true}"#.to_string(),
            r#"{"cmd":"rename_dca","dca":0,"name":"Drums"}"#.to_string(),
            r#"{"cmd":"set_dca_color","dca":0,"color":null}"#.to_string(),
            r#"{"cmd":"assign_mute_group","strip":{"kind":"bus","id":4},"group":1,"assigned":true}"#.to_string(),
            r#"{"cmd":"set_mute_group","group":1,"active":true}"#.to_string(),
            r#"{"cmd":"rename_mute_group","group":1,"name":"Band"}"#.to_string(),
            r#"{"cmd":"set_monitor","monitor":{"solo_mode":"sip","level_db":0,"dim":false}}"#.to_string(),
            r#"{"cmd":"move_channel","channel":3,"index":0}"#.to_string(),
            r#"{"cmd":"move_bus","bus":4,"index":0}"#.to_string(),
        ];
        for line in &lines {
            let command: Command =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&command).unwrap()).unwrap();
            assert_eq!(back, command);
        }
        let levels = serde_json::to_value(StripLevels::default()).unwrap();
        for key in ["output", "input", "gate_open", "gate_db", "comp_db"] {
            assert!(levels.get(key).is_some(), "{key} in {levels}");
        }
    }

    // ── Phase 2: scenes, undo, paste ─────────────────────────────────────

    fn equz8() -> InsertPlugin {
        InsertPlugin::Builtin {
            stem: "equz8".to_string(),
            params: Default::default(),
        }
    }

    fn db(gain: f32) -> f32 {
        20.0 * gain.log10()
    }

    fn insert_cell(rig: &Rig, insert: Id) -> Arc<InsertCell> {
        rig.engine.inserts.get(&insert).unwrap().cell.clone()
    }

    #[test]
    fn a_scene_stores_and_recalls_the_mix_as_one_undo_step() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        rig.at(
            0,
            Command::SetFader {
                strip: a,
                db: db(0.5),
            },
        );
        let scenes = rig.engine.scenes_revision();
        rig.at(
            1000,
            Command::SceneStore {
                id: None,
                name: None,
            },
        );
        assert!(rig.engine.scenes_revision() > scenes);
        let session = rig.engine.session();
        let scene = session.current_scene.expect("the stored scene is current");
        assert_eq!(session.scenes.len(), 1);
        assert_eq!(session.scenes[0].name, "Scene 1");
        assert_eq!(rig.engine.scene_differs(scene), Some(false));
        assert_eq!(rig.engine.scene_differs(999), None);

        rig.at(2000, Command::SetFader { strip: a, db: 0.0 });
        rig.at(3000, Command::SetPan { strip: a, pan: 1.0 });
        assert_eq!(rig.engine.scene_differs(scene), Some(true));
        let undo_depth = rig.engine.history().undo_depth;

        assert_eq!(rig.at(4000, Command::SceneRecall { id: scene }), None);
        assert!(close(rig.core(a).fader_db, db(0.5)));
        assert_eq!(rig.core(a).pan, 0.0);
        assert_eq!(rig.engine.scene_differs(scene), Some(false));
        assert!(close(rig.play()[0], (0.25 + 0.25) * CENTRE));
        let history = rig.engine.history();
        assert_eq!(history.undo.as_deref(), Some("Recall Scene 1"));
        assert_eq!(history.undo_depth, undo_depth + 1);

        // One undo takes the whole recall back.
        rig.undo();
        assert_eq!(rig.core(a).fader_db, 0.0);
        assert_eq!(rig.core(a).pan, 1.0);
        assert_eq!(rig.engine.scene_differs(scene), Some(true));

        // A second store goes after the current one; an overwrite keeps the
        // name unless given one.
        let depth = rig.engine.history().undo_depth;
        rig.at(
            5000,
            Command::SceneStore {
                id: None,
                name: Some("Verse".to_string()),
            },
        );
        let verse = rig.engine.session().current_scene.unwrap();
        rig.at(
            6000,
            Command::SceneStore {
                id: Some(scene),
                name: None,
            },
        );
        let names: Vec<&str> = rig
            .engine
            .session()
            .scenes
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["Scene 1", "Verse"]);
        assert_eq!(rig.engine.session().current_scene, Some(scene));
        rig.at(
            7000,
            Command::SceneMove {
                id: verse,
                index: 0,
            },
        );
        rig.at(
            7100,
            Command::SceneRename {
                id: verse,
                name: "Intro".to_string(),
            },
        );
        rig.at(
            7200,
            Command::SceneNote {
                id: verse,
                note: "Lights down".to_string(),
            },
        );
        let session = rig.engine.session();
        assert_eq!(session.scenes[0].name, "Intro");
        assert_eq!(session.scenes[0].note, "Lights down");
        rig.at(7300, Command::SceneDelete { id: scene });
        assert_eq!(rig.engine.session().current_scene, None);
        assert_eq!(rig.engine.session().scenes.len(), 1);
        assert!(
            rig.engine
                .apply(Command::SceneRecall { id: scene })
                .is_err()
        );
        // None of the scene-list edits were undo steps.
        assert_eq!(rig.engine.history().undo_depth, depth);
    }

    /// The parts of channel `ch` (and the globals) each scope field covers,
    /// in [`RecallScope`] field order.
    fn areas(session: &Session, ch: Id) -> Vec<serde_json::Value> {
        use serde_json::json;
        let c = session.channel(ch).unwrap();
        let (dca, group) = (&session.dcas[0], &session.mute_groups[0]);
        vec![
            json!([c.trim_db, c.phase_invert, c.input]),
            json!(c.core.processing),
            json!(c.core.inserts),
            json!([c.core.fader_db, dca.level_db]),
            json!([c.core.mute, dca.mute, group.active]),
            json!(c.core.pan),
            json!(c.sends),
            json!([c.output, session.outputs]),
            json!([c.core.dcas, c.core.mute_groups]),
            json!([c.name, c.core.color, dca.name, dca.color, group.name]),
        ]
    }

    fn scope_without(field: usize) -> RecallScope {
        let mut scope = RecallScope::default();
        let flag = match field {
            0 => &mut scope.input,
            1 => &mut scope.processing,
            2 => &mut scope.inserts,
            3 => &mut scope.faders,
            4 => &mut scope.mutes,
            5 => &mut scope.pan,
            6 => &mut scope.sends,
            7 => &mut scope.routing,
            8 => &mut scope.assign,
            _ => &mut scope.names,
        };
        *flag = false;
        scope
    }

    /// A rig with a bus, a send and an insert, a scene stored, then every
    /// recallable part of channel A (and DCA 1, mute group 1) changed.
    fn changed_rig() -> (Rig, Id, Vec<serde_json::Value>) {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let ch = rig.id(0);
        rig.apply(Command::AddBus {
            name: "Verb".to_string(),
            role: None,
            stereo: None,
        });
        let bus = rig.engine.session().buses[0].id;
        rig.apply(Command::SetSend {
            channel: ch,
            bus,
            level_db: -12.0,
            pre_fader: Some(false),
            pan: None,
            pan_follow: None,
        });
        rig.apply(Command::AddInsert {
            strip: a,
            plugin: equz8(),
            index: None,
        });
        let insert = rig.core(a).inserts[0].id;
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        let scene = rig.engine.session().current_scene.unwrap();
        let mut processing = rig.core(a).processing;
        processing.comp.on = true;
        let mut outputs = rig.engine.session().outputs.clone();
        outputs.push(OutputPatch {
            source: PatchSource::Bus(bus),
            left: 2,
            right: None,
        });
        for command in [
            Command::SetTrim {
                channel: ch,
                db: 6.0,
            },
            Command::SetPhaseInvert {
                channel: ch,
                invert: true,
            },
            Command::SetChannelInput {
                channel: ch,
                input: InputPatch::mono(1),
            },
            Command::SetProcessing {
                strip: a,
                processing,
            },
            Command::SetInsertParam {
                insert,
                index: 0,
                value: 0.9,
            },
            Command::SetInsertBypass {
                insert,
                bypass: true,
            },
            Command::SetFader {
                strip: a,
                db: -20.0,
            },
            Command::SetDcaLevel { dca: 0, db: -5.0 },
            Command::SetMute {
                strip: a,
                mute: true,
            },
            Command::SetDcaMute { dca: 0, mute: true },
            Command::SetMuteGroup {
                group: 0,
                active: true,
            },
            Command::SetPan { strip: a, pan: 0.5 },
            Command::SetSend {
                channel: ch,
                bus,
                level_db: -3.0,
                pre_fader: Some(true),
                pan: None,
                pan_follow: None,
            },
            Command::SetStripOutput {
                strip: a,
                output: StripOutput::Bus(bus),
            },
            Command::SetOutputPatch { patches: outputs },
            Command::AssignDca {
                strip: a,
                dca: 1,
                assigned: true,
            },
            Command::AssignMuteGroup {
                strip: a,
                group: 1,
                assigned: true,
            },
            Command::RenameStrip {
                strip: a,
                name: "Lead".to_string(),
            },
            Command::SetStripColor {
                strip: a,
                color: Some(3),
            },
            Command::RenameDca {
                dca: 0,
                name: "Drums".to_string(),
            },
            Command::SetDcaColor {
                dca: 0,
                color: Some(4),
            },
            Command::RenameMuteGroup {
                group: 0,
                name: "Band".to_string(),
            },
        ] {
            rig.apply(command);
        }
        let stored = areas(
            &rig.engine.session().scene(scene).unwrap().mix_session(),
            ch,
        );
        (rig, scene, stored)
    }

    impl Scene {
        /// The scene's mix as a session, to read it with the same helpers.
        fn mix_session(&self) -> Session {
            let mut session = Session::default();
            session.set_mix(self.mix.clone());
            session
        }
    }

    #[test]
    fn each_scope_field_leaves_its_part_alone() {
        for field in 0..10 {
            let (mut rig, scene, stored) = changed_rig();
            let ch = rig.id(0);
            let changed = areas(rig.engine.session(), ch);
            for (part, (s, c)) in stored.iter().zip(&changed).enumerate() {
                assert_ne!(s, c, "part {part} was changed after the store");
            }
            let scope = scope_without(field);
            rig.apply(Command::SceneScope { id: scene, scope });
            assert_eq!(rig.engine.session().scene(scene).unwrap().scope, scope);
            rig.apply(Command::SceneRecall { id: scene });
            let got = areas(rig.engine.session(), ch);
            for part in 0..10 {
                let want = if part == field {
                    &changed[part]
                } else {
                    &stored[part]
                };
                assert_eq!(&got[part], want, "scope without field {field}: part {part}");
            }
            assert_eq!(
                rig.engine.scene_differs(scene),
                Some(false),
                "field {field}"
            );
        }
    }

    #[test]
    fn a_full_recall_sets_every_part_and_keeps_live_cells() {
        let (mut rig, scene, stored) = changed_rig();
        let a = rig.channel(0);
        let insert = rig.core(a).inserts[0].id;
        let cell = insert_cell(&rig, insert);
        let processor = rig.graph.as_ref().unwrap().channels[0]
            .strip
            .processor
            .clone();
        let _ = processor.take_latest();
        assert_eq!(rig.engine.scene_differs(scene), Some(true));
        rig.apply(Command::SceneRecall { id: scene });
        assert_eq!(areas(rig.engine.session(), rig.id(0)), stored);
        // The insert and the processing section are the same ones, set in
        // place: the bypass flag landed, the processing glides.
        assert!(Arc::ptr_eq(&insert_cell(&rig, insert), &cell));
        assert!(!cell.bypass.load(Ordering::Relaxed));
        assert!(
            !processor
                .take_latest()
                .expect("new settings were sent")
                .comp
                .on
        );
        let graph = rig.graph.as_ref().unwrap();
        assert!(Arc::ptr_eq(&graph.channels[0].strip.processor, &processor));
        assert!(Arc::ptr_eq(&graph.channels[0].strip.inserts[0], &cell));
    }

    #[test]
    fn a_recall_safe_strip_is_left_alone() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        rig.apply(Command::SetFader {
            strip: a,
            db: -10.0,
        });
        rig.apply(Command::SetFader {
            strip: b,
            db: -10.0,
        });
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        let scene = rig.engine.session().current_scene.unwrap();
        rig.at(0, Command::SetFader { strip: a, db: 0.0 });
        rig.at(1000, Command::SetFader { strip: b, db: 0.0 });
        let depth = rig.engine.history().undo_depth;
        rig.apply(Command::SetRecallSafe {
            strip: a,
            safe: true,
        });
        assert!(rig.core(a).recall_safe);
        assert_eq!(rig.engine.history().undo_depth, depth, "not an undo step");
        rig.apply(Command::SceneRecall { id: scene });
        assert_eq!(rig.core(a).fader_db, 0.0);
        assert_eq!(rig.core(b).fader_db, -10.0);
        // The safe strip does not light "modified" either.
        assert_eq!(rig.engine.scene_differs(scene), Some(false));
    }

    #[test]
    fn strips_added_or_removed_since_the_store_are_left_alone() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        rig.apply(Command::SetFader {
            strip: a,
            db: -10.0,
        });
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        let scene = rig.engine.session().current_scene.unwrap();
        rig.apply(Command::AddChannel {
            name: "C".to_string(),
            input: InputPatch::mono(0),
        });
        let c = rig.channel(2);
        rig.apply(Command::SetFader { strip: c, db: -4.0 });
        let StripRef::Channel(b_id) = b else {
            unreachable!()
        };
        rig.apply(Command::RemoveChannel { channel: b_id });
        // A bus added since, and a send to it: kept.
        rig.apply(Command::AddBus {
            name: "New".to_string(),
            role: None,
            stereo: None,
        });
        let bus = rig.engine.session().buses[0].id;
        rig.apply(Command::SetSend {
            channel: rig.id(0),
            bus,
            level_db: -6.0,
            pre_fader: Some(false),
            pan: None,
            pan_follow: None,
        });
        rig.apply(Command::SetFader { strip: a, db: 0.0 });
        assert_eq!(rig.at(10_000, Command::SceneRecall { id: scene }), None);
        let session = rig.engine.session();
        let names: Vec<&str> = session.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["A", "C"]);
        assert_eq!(rig.core(a).fader_db, -10.0);
        assert_eq!(rig.core(c).fader_db, -4.0);
        assert_eq!(session.channels[0].sends.len(), 1);
        assert_eq!(rig.graph.as_ref().unwrap().channels.len(), 2);
    }

    #[test]
    fn inserts_that_differ_are_left_and_reported() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        rig.apply(Command::AddInsert {
            strip: a,
            plugin: equz8(),
            index: None,
        });
        let first = rig.core(a).inserts[0].id;
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        let scene = rig.engine.session().current_scene.unwrap();
        rig.apply(Command::AddInsert {
            strip: a,
            plugin: InsertPlugin::Builtin {
                stem: "fa76".to_string(),
                params: Default::default(),
            },
            index: None,
        });
        let second = rig.core(a).inserts[1].id;
        rig.apply(Command::RemoveInsert {
            strip: a,
            insert: first,
        });
        let note = rig.at(10_000, Command::SceneRecall { id: scene });
        assert_eq!(
            note.as_deref(),
            Some("2 inserts differ from the scene and were left as they are")
        );
        let ids: Vec<Id> = rig.core(a).inserts.iter().map(|s| s.id).collect();
        assert_eq!(ids, [second]);
        assert!(rig.engine.inserts.contains_key(&second));
        // Through the server's door too.
        let reply = rig
            .engine
            .apply_with_note(Command::SceneRecall { id: scene });
        assert_eq!(reply.unwrap(), note);
    }

    #[test]
    fn solo_is_never_recalled() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        rig.apply(Command::SetSolo {
            strip: a,
            solo: true,
        });
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        let scene = rig.engine.session().current_scene.unwrap();
        rig.apply(Command::ClearSolo);
        rig.apply(Command::SetSolo {
            strip: b,
            solo: true,
        });
        rig.apply(Command::SceneRecall { id: scene });
        assert!(!rig.core(a).solo);
        assert!(rig.core(b).solo);
    }

    #[test]
    fn a_fader_drag_is_one_undo_step() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let revision = rig.engine.revision();
        for i in 0..10u64 {
            rig.at(
                i * 100,
                Command::SetFader {
                    strip: a,
                    db: -(i as f32) - 1.0,
                },
            );
        }
        assert_eq!(rig.engine.revision(), revision + 10);
        let history = rig.engine.history();
        assert_eq!(history.undo.as_deref(), Some("Fader A"));
        assert_eq!(history.undo_depth, 1);
        // A pause longer than the window starts another step.
        rig.at(
            900 + 700,
            Command::SetFader {
                strip: a,
                db: -30.0,
            },
        );
        assert_eq!(rig.engine.history().undo_depth, 2);
        // So does another target, at once.
        rig.at(1700, Command::SetPan { strip: a, pan: 0.5 });
        rig.at(
            1750,
            Command::SetFader {
                strip: a,
                db: -31.0,
            },
        );
        assert_eq!(rig.engine.history().undo_depth, 4);

        rig.undo();
        rig.undo();
        assert_eq!(rig.core(a).fader_db, -30.0);
        assert_eq!(rig.core(a).pan, 0.0);
        rig.undo();
        assert_eq!(rig.core(a).fader_db, -10.0);
        rig.undo();
        assert_eq!(rig.core(a).fader_db, 0.0);
        assert!(close(rig.play()[0], 0.75 * CENTRE));
        assert_eq!(rig.undo().as_deref(), Some("Nothing to undo"));
        let history = rig.engine.history();
        assert_eq!((history.undo_depth, history.redo_depth), (0, 4));
        assert_eq!(history.redo.as_deref(), Some("Fader A"));
        rig.redo();
        assert_eq!(rig.core(a).fader_db, -10.0);
        assert!(close(
            rig.play()[0],
            (0.5 * 10f32.powf(-0.5) + 0.25) * CENTRE
        ));
    }

    #[test]
    fn a_new_edit_clears_redo_and_no_op_edits_are_not_steps() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        rig.at(0, Command::SetFader { strip: a, db: -6.0 });
        rig.undo();
        assert_eq!(rig.engine.history().redo_depth, 1);
        rig.at(
            5000,
            Command::SetMute {
                strip: a,
                mute: true,
            },
        );
        let history = rig.engine.history();
        assert_eq!((history.undo_depth, history.redo_depth), (1, 0));
        assert_eq!(history.undo.as_deref(), Some("Mute A"));
        assert_eq!(rig.redo().as_deref(), Some("Nothing to redo"));
        rig.at(
            10_000,
            Command::SetMute {
                strip: a,
                mute: true,
            },
        );
        assert_eq!(rig.engine.history().undo_depth, 1);
        // Not undoable: solo, the monitor.
        rig.at(
            11_000,
            Command::SetSolo {
                strip: a,
                solo: true,
            },
        );
        rig.at(
            12_000,
            Command::SetMonitor {
                monitor: MonitorSettings::default(),
            },
        );
        assert_eq!(rig.engine.history().undo_depth, 1);
        // Undo leaves the solo where it is.
        rig.undo();
        assert!(!rig.core(a).mute);
        assert!(rig.core(a).solo);
    }

    #[test]
    fn history_keeps_the_last_hundred_steps() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        for i in 0..150u64 {
            rig.at(
                i,
                Command::SetMute {
                    strip: a,
                    mute: i % 2 == 0,
                },
            );
        }
        assert_eq!(rig.engine.history().undo_depth, crate::history::UNDO_DEPTH);
        for _ in 0..100 {
            rig.undo();
        }
        assert_eq!(rig.undo().as_deref(), Some("Nothing to undo"));
        // Step 50 was the oldest kept: the mute as it was before it.
        assert!(!rig.core(a).mute);
    }

    #[test]
    fn processing_undo_glides_the_same_processor() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let processor = rig.graph.as_ref().unwrap().channels[0]
            .strip
            .processor
            .clone();
        let original = rig.core(a).processing;
        let mut eq = original;
        eq.eq.on = true;
        eq.eq.bands[1].gain_db = 6.0;
        rig.at(
            0,
            Command::SetProcessing {
                strip: a,
                processing: eq,
            },
        );
        rig.at(
            100,
            Command::SetProcessing {
                strip: a,
                processing: {
                    let mut p = eq;
                    p.eq.bands[1].gain_db = 7.0;
                    p
                },
            },
        );
        let mut comp = eq;
        comp.eq.bands[1].gain_db = 7.0;
        comp.comp.on = true;
        rig.at(
            200,
            Command::SetProcessing {
                strip: a,
                processing: comp,
            },
        );
        let history = rig.engine.history();
        assert_eq!(history.undo.as_deref(), Some("Comp A"));
        assert_eq!(history.undo_depth, 2, "EQ drag, then the compressor");
        let _ = processor.take_latest();
        rig.undo();
        assert_eq!(processor.take_latest().unwrap().eq.bands[1].gain_db, 7.0);
        assert_eq!(rig.engine.history().undo.as_deref(), Some("EQ A"));
        rig.undo();
        assert_eq!(rig.core(a).processing, original);
        assert_eq!(processor.take_latest(), Some(original));
        assert!(Arc::ptr_eq(
            &rig.graph.as_ref().unwrap().channels[0].strip.processor,
            &processor
        ));
    }

    #[test]
    fn insert_add_remove_and_params_undo() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        rig.at(
            0,
            Command::AddInsert {
                strip: a,
                plugin: equz8(),
                index: None,
            },
        );
        let insert = rig.core(a).inserts[0].id;
        assert_eq!(rig.engine.history().undo.as_deref(), Some("Add EQ-Z8 to A"));
        rig.undo();
        assert!(rig.core(a).inserts.is_empty());
        assert!(!rig.engine.inserts.contains_key(&insert));
        assert!(
            rig.graph.as_ref().unwrap().channels[0]
                .strip
                .inserts
                .is_empty()
        );
        rig.redo();
        assert_eq!(rig.core(a).inserts[0].id, insert, "the same insert, back");
        assert_eq!(
            rig.graph.as_ref().unwrap().channels[0].strip.inserts.len(),
            1
        );

        // A parameter undo sets the live insert in place.
        let cell = insert_cell(&rig, insert);
        rig.at(
            1000,
            Command::SetInsertParam {
                insert,
                index: 0,
                value: 0.7,
            },
        );
        rig.at(
            1100,
            Command::SetInsertParam {
                insert,
                index: 0,
                value: 0.8,
            },
        );
        rig.at(
            2000,
            Command::SetInsertBypass {
                insert,
                bypass: true,
            },
        );
        assert_eq!(rig.engine.history().undo_depth, 3);
        rig.undo();
        assert!(!cell.bypass.load(Ordering::Relaxed));
        rig.undo();
        assert!(Arc::ptr_eq(&insert_cell(&rig, insert), &cell));
        let InsertPlugin::Builtin { params, .. } = &rig.core(a).inserts[0].plugin else {
            unreachable!()
        };
        assert!(params.is_empty());
        rig.redo();

        // Removing it and undoing brings it back with its settings.
        rig.at(3000, Command::RemoveInsert { strip: a, insert });
        assert!(rig.core(a).inserts.is_empty());
        rig.undo();
        let slot = &rig.core(a).inserts[0];
        assert_eq!(slot.id, insert);
        let InsertPlugin::Builtin { params, .. } = &slot.plugin else {
            unreachable!()
        };
        assert_eq!(params.get(&0), Some(&0.8));
        assert!(rig.engine.inserts.contains_key(&insert));
        assert_eq!(
            rig.graph.as_ref().unwrap().channels[0].strip.inserts.len(),
            1
        );
    }

    #[test]
    fn removing_a_bus_undoes_with_its_sends_routes_and_patches() {
        let mut rig = Rig::new();
        let b = rig.channel(1);
        rig.apply(Command::AddBus {
            name: "Drums".to_string(),
            role: None,
            stereo: None,
        });
        let bus = rig.engine.session().buses[0].id;
        rig.apply(Command::SetSend {
            channel: rig.id(0),
            bus,
            level_db: -6.0,
            pre_fader: Some(true),
            pan: None,
            pan_follow: None,
        });
        rig.apply(Command::SetStripOutput {
            strip: b,
            output: StripOutput::Bus(bus),
        });
        let mut outputs = rig.engine.session().outputs.clone();
        outputs.push(OutputPatch {
            source: PatchSource::Bus(bus),
            left: 2,
            right: Some(3),
        });
        rig.apply(Command::SetOutputPatch { patches: outputs });
        let before = rig.engine.session().clone();
        let heard = rig.play();

        rig.at(100_000, Command::RemoveBus { bus });
        assert_eq!(
            rig.engine.history().undo.as_deref(),
            Some("Remove bus Drums")
        );
        assert!(rig.engine.session().buses.is_empty());
        rig.undo();
        assert_eq!(rig.engine.session(), &before);
        let graph = rig.graph.as_ref().unwrap();
        assert_eq!(graph.buses.len(), 1);
        assert_eq!(graph.channels[0].sends.len(), 1);
        let out = rig.play();
        assert!(
            close(out[0], heard[0]) && close(out[1], heard[1]),
            "{out:?} {heard:?}"
        );
    }

    #[test]
    fn paste_applies_the_chosen_sections_as_one_step() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        rig.apply(Command::AddBus {
            name: "Verb".to_string(),
            role: None,
            stereo: None,
        });
        let bus = rig.engine.session().buses[0].id;
        let mut processing = Processing::default();
        processing.eq.bands[2].gain_db = 4.0;
        processing.comp.on = true;
        let settings = StripSettings {
            processing: Some(processing),
            inserts: Some(vec![crate::session::InsertSettings {
                bypass: true,
                plugin: InsertPlugin::Builtin {
                    stem: "equz8".to_string(),
                    params: [(0, 0.3)].into_iter().collect(),
                },
            }]),
            sends: Some(vec![
                SendSlot {
                    bus,
                    level_db: -9.0,
                    pre_fader: true,
                    ..SendSlot::default()
                },
                SendSlot {
                    bus: 999,
                    level_db: 0.0,
                    pre_fader: false,
                    ..SendSlot::default()
                },
            ]),
            fader_db: Some(-5.0),
            pan: Some(-0.5),
            trim_db: Some(3.0),
            phase_invert: Some(true),
            name: Some("Vox".to_string()),
            color: Some(Some(2)),
        };
        let targets = vec![b, StripRef::Bus(bus), StripRef::Master];
        let before = rig.engine.session().clone();
        rig.at(
            0,
            Command::PasteStrip {
                targets: targets.clone(),
                settings: settings.clone(),
                sections: vec![
                    Section::Eq,
                    Section::Inserts,
                    Section::Sends,
                    Section::FaderPan,
                    Section::Input,
                    Section::NameColor,
                ],
            },
        );
        let history = rig.engine.history();
        assert_eq!(history.undo.as_deref(), Some("Paste to 3 strips"));
        assert_eq!(history.undo_depth, 2, "adding the bus, then the paste");
        let session = rig.engine.session();
        let channel = session.channel(rig.id(1)).unwrap();
        assert_eq!(channel.core.processing.eq, processing.eq);
        assert!(!channel.core.processing.comp.on, "only the EQ section");
        assert_eq!(
            channel.sends,
            vec![SendSlot {
                bus,
                level_db: -9.0,
                pre_fader: true,
                ..SendSlot::default()
            }]
        );
        assert_eq!((channel.core.fader_db, channel.core.pan), (-5.0, -0.5));
        assert_eq!((channel.trim_db, channel.phase_invert), (3.0, true));
        assert_eq!(
            (channel.name.as_str(), channel.core.color),
            ("Vox", Some(2))
        );
        let bus_strip = session.bus(bus).unwrap();
        assert_eq!(bus_strip.name, "Vox");
        assert_eq!(bus_strip.core.fader_db, -5.0);
        assert_eq!(session.name, before.name, "the master's name is the show's");
        assert_eq!(session.master.core.color, Some(2));
        // Every target got its own copy of the rack, with fresh ids.
        let racks: Vec<Id> = targets
            .iter()
            .map(|t| {
                let rack = &session.strip(*t).unwrap().inserts;
                assert_eq!(rack.len(), 1);
                assert!(rack[0].bypass);
                rack[0].id
            })
            .collect();
        assert!(racks.iter().all(|id| *id >= before.next_id), "{racks:?}");
        assert_eq!(
            racks.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        for id in &racks {
            assert!(rig.engine.inserts.contains_key(id));
        }
        assert_eq!(
            rig.graph.as_ref().unwrap().channels[1].strip.inserts.len(),
            1
        );
        // A is untouched.
        assert_eq!(rig.core(a), before.strip(a).unwrap());

        rig.undo();
        assert!(rig.engine.session().mix_equals(&before.mix_state()));
        for id in &racks {
            assert!(!rig.engine.inserts.contains_key(id));
        }
        // An unknown target refuses the whole paste.
        assert!(
            rig.engine
                .apply(Command::PasteStrip {
                    targets: vec![b, StripRef::Channel(999)],
                    settings,
                    sections: vec![Section::FaderPan],
                })
                .is_err()
        );
        assert!(rig.engine.session().mix_equals(&before.mix_state()));
    }

    #[test]
    fn the_scene_commands_parse_from_json() {
        let lines = [
            r#"{"cmd":"undo"}"#,
            r#"{"cmd":"redo"}"#,
            r#"{"cmd":"scene_store","id":null}"#,
            r#"{"cmd":"scene_store","id":7,"name":"Verse"}"#,
            r#"{"cmd":"scene_store"}"#,
            r#"{"cmd":"scene_recall","id":7}"#,
            r#"{"cmd":"scene_rename","id":7,"name":"Chorus"}"#,
            r#"{"cmd":"scene_note","id":7,"note":"Lights"}"#,
            r#"{"cmd":"scene_scope","id":7,"scope":{"input":false,"processing":true,"inserts":true,"faders":true,"mutes":true,"pan":true,"sends":true,"routing":true,"assign":true,"names":false}}"#,
            r#"{"cmd":"scene_scope","id":7,"scope":{"faders":false}}"#,
            r#"{"cmd":"scene_delete","id":7}"#,
            r#"{"cmd":"scene_move","id":7,"index":0}"#,
            r#"{"cmd":"set_recall_safe","strip":{"kind":"channel","id":3},"safe":true}"#,
            r#"{"cmd":"paste_strip","targets":[{"kind":"channel","id":3},{"kind":"master"}],"settings":{"fader_db":-3,"color":null,"inserts":[{"bypass":false,"plugin":{"type":"builtin","stem":"fa76","params":[[1,0.5]]}}]},"sections":["fader_pan","name_color","inserts","processing","hpf","gate","eq","comp","delay","sends","input"]}"#,
        ];
        for line in lines {
            let command: Command =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&command).unwrap()).unwrap();
            assert_eq!(back, command, "{line}");
        }
        let Command::PasteStrip { settings, .. } = serde_json::from_str(lines[13]).unwrap() else {
            unreachable!()
        };
        assert_eq!(settings.color, Some(None), "null is the default colour");
        assert_eq!(settings.name, None);
        let Command::SceneScope { scope, .. } = serde_json::from_str(lines[9]).unwrap() else {
            unreachable!()
        };
        assert!(!scope.faders && scope.input && scope.names);
        let history = serde_json::to_value(History::default()).unwrap();
        for key in ["undo", "redo", "undo_depth", "redo_depth"] {
            assert!(history.get(key).is_some(), "{key} in {history}");
        }
    }

    #[test]
    fn revisions_move_with_the_session_and_the_scene_list() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let (revision, scenes) = (rig.engine.revision(), rig.engine.scenes_revision());
        rig.apply(Command::SetFader { strip: a, db: -1.0 });
        assert_eq!(rig.engine.revision(), revision + 1);
        assert_eq!(rig.engine.scenes_revision(), scenes);
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        assert!(rig.engine.scenes_revision() > scenes);
        let revision = rig.engine.revision();
        assert!(
            rig.engine
                .apply(Command::SetFader {
                    strip: StripRef::Channel(999),
                    db: 0.0
                })
                .is_err()
        );
        assert_eq!(
            rig.engine.revision(),
            revision,
            "a refused edit changes nothing"
        );
        let session = rig.engine.session().clone();
        rig.engine.load(session).unwrap();
        assert!(rig.engine.revision() > revision);
        assert_eq!(rig.engine.history(), History::default());
    }

    // ── Phase 3: bus & monitor ───────────────────────────────────────────

    /// Processing with every section off: a strip that passes its sum.
    fn flat() -> Processing {
        let mut processing = Processing::default();
        processing.eq.on = false;
        processing
    }

    fn add_bus(rig: &mut Rig, name: &str, role: BusRole, stereo: bool) -> Id {
        rig.apply(Command::AddBus {
            name: name.to_string(),
            role: Some(role),
            stereo: Some(stereo),
        });
        let id = rig.engine.session().buses.last().unwrap().id;
        rig.apply(Command::SetProcessing {
            strip: StripRef::Bus(id),
            processing: flat(),
        });
        id
    }

    fn add_matrix(rig: &mut Rig, name: &str, stereo: bool) -> Id {
        rig.apply(Command::AddMatrix {
            name: name.to_string(),
            stereo: Some(stereo),
        });
        let id = rig.engine.session().matrices.last().unwrap().id;
        rig.apply(Command::SetProcessing {
            strip: StripRef::Matrix(id),
            processing: flat(),
        });
        id
    }

    /// The master on outputs 1/2 and `source` on 3/4 (in place of the
    /// monitor).
    fn only_patch(rig: &mut Rig, source: PatchSource) {
        rig.apply(Command::SetOutputPatch {
            patches: vec![
                OutputPatch {
                    source: PatchSource::Master,
                    left: 0,
                    right: Some(1),
                },
                OutputPatch {
                    source,
                    left: 2,
                    right: Some(3),
                },
            ],
        });
    }

    fn largest_step(samples: &[f32]) -> f32 {
        samples
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |p, s| p.max(s.abs()))
    }

    #[test]
    fn a_mono_bus_sums_left_and_right_onto_both_sides() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        let bus = add_bus(&mut rig, "Sub", BusRole::Group, false);
        rig.apply(Command::SetStripOutput {
            strip: a,
            output: StripOutput::Bus(bus),
        });
        rig.apply(Command::SetPan {
            strip: a,
            pan: -1.0,
        });
        rig.apply(Command::SetMute {
            strip: b,
            mute: true,
        });
        // A hard left is (0.5, 0); the mono bus makes it 0.25 on both sides.
        let out = rig.play();
        assert!(close(out[0], 0.25) && close(out[1], 0.25), "{out:?}");
        rig.apply(Command::SetBusStereo { bus, stereo: true });
        let out = rig.play();
        assert!(close(out[0], 0.5) && close(out[1], 0.0), "{out:?}");
        assert_eq!(rig.engine.history().undo.as_deref(), Some("Sub stereo"));
        assert!(rig.graph.as_ref().unwrap().buses[0].stereo);
        rig.undo();
        assert!(!rig.engine.session().bus(bus).unwrap().stereo);
        assert!(close(rig.play()[1], 0.25));
    }

    #[test]
    fn a_send_pans_on_its_own_into_a_stereo_bus_and_not_into_a_mono_one() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let ch = rig.id(0);
        let bus = add_bus(&mut rig, "Wedge", BusRole::Aux, true);
        only_patch(&mut rig, PatchSource::Bus(bus));
        rig.apply(Command::SetPan { strip: a, pan: 1.0 });
        rig.apply(Command::SetFader {
            strip: a,
            db: db(0.5),
        });
        let send = |pre: bool, pan: Option<f32>, follow: Option<bool>| Command::SetSend {
            channel: ch,
            bus,
            level_db: 0.0,
            pre_fader: Some(pre),
            pan,
            pan_follow: follow,
        };
        // Following the channel, as sends always were: post-fader after its
        // fader and pan …
        rig.apply(send(false, None, None));
        let out = rig.play();
        assert!(close(out[2], 0.0) && close(out[3], 0.25), "{out:?}");
        // … pre-fader before both.
        rig.apply(send(true, None, None));
        let out = rig.play();
        assert!(close(out[2], 0.5) && close(out[3], 0.5), "{out:?}");
        // A pan of its own: constant power for a mono channel.
        rig.apply(send(true, Some(-1.0), Some(false)));
        let out = rig.play();
        assert!(close(out[2], 0.5) && close(out[3], 0.0), "{out:?}");
        rig.apply(send(true, Some(0.0), None));
        let out = rig.play();
        assert!(
            close(out[2], 0.5 * CENTRE) && close(out[3], 0.5 * CENTRE),
            "{out:?}"
        );
        // Post-fader with a pan of its own: the channel's level, not its pan.
        rig.apply(send(false, Some(-1.0), None));
        let out = rig.play();
        assert!(close(out[2], 0.25) && close(out[3], 0.0), "{out:?}");
        rig.apply(Command::SetMute {
            strip: a,
            mute: true,
        });
        assert!(
            close(rig.play()[2], 0.0),
            "muted, so is its post-fader send"
        );
        rig.apply(Command::SetMute {
            strip: a,
            mute: false,
        });
        // A mono bus ignores the send's pan.
        rig.apply(Command::SetBusStereo { bus, stereo: false });
        let out = rig.play();
        assert!(close(out[2], 0.25) && close(out[3], 0.25), "{out:?}");
        rig.apply(send(true, None, None));
        let out = rig.play();
        assert!(close(out[2], 0.5) && close(out[3], 0.5), "{out:?}");
        let saved = &rig.engine.session().channel(ch).unwrap().sends[0];
        assert_eq!((saved.pan, saved.pan_follow), (-1.0, false));
    }

    #[test]
    fn a_bus_role_sets_its_output_and_new_sends_pre_or_post() {
        let mut rig = Rig::new();
        let ch = rig.id(0);
        let aux = add_bus(&mut rig, "Mon 1", BusRole::Aux, true);
        let fx = add_bus(&mut rig, "Verb", BusRole::Fx, true);
        rig.apply(Command::AddBus {
            name: "Drums".to_string(),
            role: None,
            stereo: None,
        });
        let session = rig.engine.session();
        let group = session.buses[2].clone();
        assert_eq!(session.bus(aux).unwrap().output, StripOutput::None);
        assert_eq!(session.bus(fx).unwrap().output, StripOutput::Master);
        assert_eq!(
            (group.role, group.stereo, group.output),
            (BusRole::Group, true, StripOutput::Master)
        );
        for bus in [aux, fx, group.id] {
            rig.apply(Command::SetSend {
                channel: ch,
                bus,
                level_db: -6.0,
                pre_fader: None,
                pan: None,
                pan_follow: None,
            });
        }
        let pre: Vec<(bool, bool)> = rig.engine.session().channels[0]
            .sends
            .iter()
            .map(|s| (s.pre_fader, s.pan_follow))
            .collect();
        assert_eq!(pre, [(true, true), (false, true), (false, true)]);
        // An existing send keeps its pre/post when it is not given.
        rig.apply(Command::SetBusRole {
            bus: aux,
            role: BusRole::Fx,
        });
        assert_eq!(rig.engine.history().undo.as_deref(), Some("Mon 1 to FX"));
        rig.apply(Command::SetSend {
            channel: ch,
            bus: aux,
            level_db: -3.0,
            pre_fader: None,
            pan: None,
            pan_follow: None,
        });
        assert!(rig.engine.session().channels[0].sends[0].pre_fader);
        assert_eq!(rig.engine.session().bus(aux).unwrap().role, BusRole::Fx);
    }

    #[test]
    fn a_matrix_sums_the_master_and_buses_after_their_faders() {
        let mut rig = Rig::new();
        let ch_b = rig.id(1);
        let bus = add_bus(&mut rig, "Fill", BusRole::Aux, true);
        rig.apply(Command::SetSend {
            channel: ch_b,
            bus,
            level_db: 0.0,
            pre_fader: Some(true),
            pan: None,
            pan_follow: None,
        });
        let matrix = add_matrix(&mut rig, "Delay zone", true);
        let m = StripRef::Matrix(matrix);
        only_patch(&mut rig, PatchSource::Matrix(matrix));
        rig.apply(Command::SetFader {
            strip: StripRef::Master,
            db: db(0.5),
        });
        let master = 0.75 * CENTRE * 0.5;
        assert!(close(rig.play()[2], 0.0), "nothing summed yet");
        rig.apply(Command::SetMatrixSend {
            matrix,
            source: MatrixFeed::Master,
            level_db: db(0.5),
            pan: None,
        });
        let out = rig.play();
        assert!(
            close(out[2], master * 0.5) && close(out[3], master * 0.5),
            "{out:?}"
        );
        rig.apply(Command::SetMatrixSend {
            matrix,
            source: MatrixFeed::Bus(bus),
            level_db: 0.0,
            pan: Some(-1.0),
        });
        let out = rig.play();
        assert!(
            close(out[2], master * 0.5 + 0.25) && close(out[3], master * 0.5),
            "{out:?}"
        );
        // After the bus's fader.
        rig.apply(Command::SetFader {
            strip: StripRef::Bus(bus),
            db: db(0.5),
        });
        let out = rig.play();
        assert!(close(out[2], master * 0.5 + 0.125), "{out:?}");
        // The matrix's own fader, then a mono matrix.
        rig.apply(Command::SetFader {
            strip: m,
            db: db(0.5),
        });
        let out = rig.play();
        assert!(close(out[2], (master * 0.5 + 0.125) * 0.5), "{out:?}");
        assert!(close(out[3], master * 0.25), "{out:?}");
        rig.apply(Command::SetMatrixStereo {
            matrix,
            stereo: false,
        });
        // A mono matrix ignores the bus's pan: it is on both sides, then
        // (L+R)/2.
        let mono = (master * 0.5 + 0.125) * 0.5;
        let out = rig.play();
        assert!(close(out[2], mono) && close(out[3], mono), "{out:?}");
        // The main mix does not hear it.
        assert!(close(out[0], master), "{out:?}");
        assert!(rig.engine.meters().contains_key(&m));
        // Its processing runs: a 600 Hz high-pass takes the (DC) sum out.
        let mut processing = flat();
        processing.hpf.on = true;
        processing.hpf.hz = 600.0;
        processing.hpf.slope_db = 24;
        rig.apply(Command::SetProcessing {
            strip: m,
            processing,
        });
        assert!(rig.frame(40)[2].abs() < 1.0e-3);
        // Not DCA-assignable, routed nowhere but its patch, fed by no
        // missing bus.
        for command in [
            Command::AssignDca {
                strip: m,
                dca: 0,
                assigned: true,
            },
            Command::SetStripOutput {
                strip: m,
                output: StripOutput::Master,
            },
            Command::SetMatrixSend {
                matrix,
                source: MatrixFeed::Bus(999),
                level_db: 0.0,
                pan: None,
            },
            Command::SetSoloSafe {
                strip: m,
                safe: true,
            },
        ] {
            assert!(rig.engine.apply(command).is_err());
        }
        // An insert on it, and a move.
        rig.apply(Command::AddInsert {
            strip: m,
            plugin: equz8(),
            index: None,
        });
        assert_eq!(
            rig.graph.as_ref().unwrap().matrices[0].strip.inserts.len(),
            1
        );
        let second = add_matrix(&mut rig, "Rec", true);
        rig.apply(Command::MoveMatrix {
            matrix: second,
            index: 0,
        });
        let order: Vec<Id> = rig.engine.session().matrices.iter().map(|m| m.id).collect();
        assert_eq!(order, [second, matrix]);
    }

    #[test]
    fn a_matrix_solos_as_a_cue_before_or_after_its_fader() {
        let mut rig = Rig::new();
        let matrix = add_matrix(&mut rig, "Zone", true);
        let m = StripRef::Matrix(matrix);
        rig.apply(Command::SetMatrixSend {
            matrix,
            source: MatrixFeed::Master,
            level_db: 0.0,
            pan: None,
        });
        rig.apply(Command::SetFader {
            strip: m,
            db: db(0.5),
        });
        let master = 0.75 * CENTRE;
        rig.apply(Command::SetSolo {
            strip: m,
            solo: true,
        });
        assert_eq!(rig.engine.session().solo_count(), 1);
        let out = rig.play();
        assert!(close(out[0], master), "{out:?}");
        assert!(
            close(out[2], master),
            "PFL: before the matrix fader {out:?}"
        );
        let mode = |solo_mode| Command::SetMonitor {
            monitor: MonitorSettings {
                solo_mode,
                ..MonitorSettings::default()
            },
        };
        rig.apply(mode(SoloMode::Afl));
        assert!(close(rig.play()[2], master * 0.5));
        // In solo in place a matrix still cues (AFL) and silences nothing.
        rig.apply(mode(SoloMode::Sip));
        let out = rig.play();
        assert!(
            close(out[0], master) && close(out[2], master * 0.5),
            "{out:?}"
        );
        rig.apply(Command::ClearSolo);
        assert!(close(rig.play()[2], master));
        assert!(!rig.engine.session().any_solo());
    }

    #[test]
    fn the_monitor_plays_its_source_while_nothing_is_soloed() {
        let mut rig = Rig::new();
        let a = rig.channel(0);
        let ch_b = rig.id(1);
        let bus = add_bus(&mut rig, "IEM", BusRole::Aux, true);
        rig.apply(Command::SetSend {
            channel: ch_b,
            bus,
            level_db: 0.0,
            pre_fader: Some(true),
            pan: None,
            pan_follow: None,
        });
        rig.apply(Command::SetFader {
            strip: StripRef::Bus(bus),
            db: db(0.5),
        });
        let monitor = |solo_mode, source| Command::SetMonitor {
            monitor: MonitorSettings {
                solo_mode,
                source,
                ..MonitorSettings::default()
            },
        };
        let master = 0.75 * CENTRE;
        rig.apply(monitor(SoloMode::Pfl, MonitorSource::Bus(bus)));
        let out = rig.play();
        assert!(close(out[0], master) && close(out[2], 0.125), "{out:?}");
        // Something soloed: the cue, as ever.
        rig.apply(Command::SetSolo {
            strip: a,
            solo: true,
        });
        assert!(close(rig.play()[2], 0.5));
        // Solo in place: the master.
        rig.apply(monitor(SoloMode::Sip, MonitorSource::Bus(bus)));
        let out = rig.play();
        assert!(close(out[2], out[0]), "{out:?}");
        rig.apply(Command::ClearSolo);
        assert!(close(rig.play()[2], 0.125));
        // Back to the master, crossfaded rather than switched.
        rig.apply(monitor(SoloMode::Pfl, MonitorSource::Master));
        let monitor_plane = Rig::plane(&rig.render(256), 4, 2);
        assert!(close(*monitor_plane.last().unwrap(), master));
        assert!(
            largest_step(&monitor_plane) < 0.01,
            "{}",
            largest_step(&monitor_plane)
        );
        // A matrix as the source.
        let matrix = add_matrix(&mut rig, "Rec", true);
        rig.apply(Command::SetMatrixSend {
            matrix,
            source: MatrixFeed::Bus(bus),
            level_db: 0.0,
            pan: None,
        });
        rig.apply(monitor(SoloMode::Pfl, MonitorSource::Matrix(matrix)));
        assert!(close(rig.play()[2], 0.125));
        // Not monitor settings that do not exist; and a source that goes
        // away puts the master back.
        assert!(
            rig.engine
                .apply(monitor(SoloMode::Pfl, MonitorSource::Bus(999)))
                .is_err()
        );
        rig.apply(Command::RemoveMatrix { matrix });
        assert_eq!(rig.engine.session().monitor.source, MonitorSource::Master);
        assert!(close(rig.play()[2], master));
    }

    #[test]
    fn talkback_goes_only_where_it_is_sent_dims_the_monitor_and_is_high_passed() {
        let mut rig = Rig::with_outputs(6);
        let wedge = add_bus(&mut rig, "Wedge", BusRole::Aux, true);
        let other = add_bus(&mut rig, "IEM", BusRole::Aux, true);
        let mut outputs = rig.engine.session().outputs.clone();
        outputs.push(OutputPatch {
            source: PatchSource::Bus(wedge),
            left: 4,
            right: Some(5),
        });
        rig.apply(Command::SetOutputPatch { patches: outputs });
        let talkback = |hpf: bool| TalkbackSettings {
            input: Some(1),
            level_db: 0.0,
            hpf,
            to: vec![InjectDest::Bus(wedge)],
        };
        rig.apply(Command::SetTalkback {
            talkback: talkback(false),
        });
        let master = 0.75 * CENTRE;
        let quiet = rig.frame(2);
        assert!(close(quiet[4], 0.0) && close(quiet[2], master), "{quiet:?}");
        let revision = rig.engine.revision();
        let undo = rig.engine.history().undo_depth;

        rig.apply(Command::Talk { active: true });
        assert!(rig.engine.status().talkback_active);
        let _ = rig.engine.meters();
        let out = rig.render(128 * 30);
        let heard = Rig::plane(&out, 6, 4);
        // In over 50 ms, a little at a time.
        assert!(largest_step(&heard) < 2.0e-4, "{}", largest_step(&heard));
        assert!(heard[1200] > 0.05 && heard[1200] < 0.2, "{}", heard[1200]);
        assert!(close(*heard.last().unwrap(), 0.25));
        let last = &out[out.len() - 6..];
        assert!(close(last[0], master), "the main mix does not hear it");
        assert!(close(last[2], master * 0.1), "talking dims the monitor");
        let meters = rig.engine.meters();
        assert_eq!(meters[&StripRef::Bus(other)].output, (0.0, 0.0));
        assert!(close(rig.engine.talkback_meter(), 0.25));
        // The talk switch is live state: no session change, no undo step.
        assert_eq!(rig.engine.revision(), revision);
        assert_eq!(rig.engine.history().undo_depth, undo);

        // The high-pass takes the (DC) input out.
        rig.apply(Command::SetTalkback {
            talkback: talkback(true),
        });
        assert!(rig.frame(40)[4].abs() < 0.01);
        rig.apply(Command::SetTalkback {
            talkback: talkback(false),
        });
        assert!(close(rig.frame(10)[4], 0.25));
        assert_eq!(rig.engine.history().undo_depth, undo, "not undoable");

        // Off: out in 50 ms, the monitor back up.
        rig.apply(Command::Talk { active: false });
        let out = rig.render(128 * 30);
        let heard = Rig::plane(&out, 6, 4);
        assert!(largest_step(&heard) < 2.0e-4);
        let last = &out[out.len() - 6..];
        assert!(close(last[4], 0.0) && close(last[2], master), "{last:?}");
        assert!(!rig.engine.status().talkback_active);
        // Only buses and matrices that exist.
        assert!(
            rig.engine
                .apply(Command::SetTalkback {
                    talkback: TalkbackSettings {
                        to: vec![InjectDest::Matrix(999)],
                        ..TalkbackSettings::default()
                    },
                })
                .is_err()
        );
    }

    #[test]
    fn the_oscillator_plays_its_kind_at_its_level_and_ramps_in_and_out() {
        use crate::session::OscillatorKind;
        let mut rig = Rig::new();
        let bus = add_bus(&mut rig, "Test", BusRole::Aux, true);
        only_patch(&mut rig, PatchSource::Bus(bus));
        let oscillator = |kind, level_db| OscillatorSettings {
            kind,
            hz: 1000.0,
            level_db,
            to: vec![InjectDest::Bus(bus)],
        };
        rig.apply(Command::SetOscillator {
            oscillator: oscillator(OscillatorKind::Sine, -20.0),
        });
        assert!(close(rig.frame(2)[2], 0.0), "off until switched on");
        rig.apply(Command::OscillatorOn { on: true });
        assert!(rig.engine.status().oscillator_on);
        let out = rig.render(9600);
        let left = Rig::plane(&out, 4, 2);
        let right = Rig::plane(&out, 4, 3);
        assert_eq!(left, right);
        // −20 dBFS peak once in; ramped in over 50 ms with no step.
        assert!(
            (peak(&left[4800..]) - 0.1).abs() < 1.0e-3,
            "{}",
            peak(&left[4800..])
        );
        assert!(peak(&left[..480]) < 0.025, "{}", peak(&left[..480]));
        let sine_step = std::f32::consts::TAU * 1000.0 / 48_000.0 * 0.1;
        assert!(
            largest_step(&left) < sine_step * 1.01,
            "{}",
            largest_step(&left)
        );
        // Only where it is sent.
        let master = 0.75 * CENTRE;
        assert!(close(out[out.len() - 4], master));

        // White and pink: at most the level, pink the darker.
        let brightness = |samples: &[f32]| {
            let diff: f32 = samples.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
            let level: f32 = samples.iter().map(|s| s.abs()).sum();
            diff / level
        };
        rig.apply(Command::SetOscillator {
            oscillator: oscillator(OscillatorKind::White, -20.0),
        });
        let white = Rig::plane(&rig.render(14_400), 4, 2);
        let white = &white[9600..];
        assert!(
            peak(white) <= 0.1 + 1.0e-6 && peak(white) > 0.09,
            "{}",
            peak(white)
        );
        rig.apply(Command::SetOscillator {
            oscillator: oscillator(OscillatorKind::Pink, -20.0),
        });
        let pink = Rig::plane(&rig.render(14_400), 4, 2);
        let pink = &pink[9600..];
        assert!(
            peak(pink) <= 0.1 + 1.0e-6 && peak(pink) > 0.01,
            "{}",
            peak(pink)
        );
        assert!(
            brightness(pink) < brightness(white) * 0.75,
            "{} {}",
            brightness(pink),
            brightness(white)
        );

        // −90 dBFS is off.
        rig.apply(Command::SetOscillator {
            oscillator: oscillator(OscillatorKind::Sine, OSCILLATOR_MIN_DB),
        });
        assert!(peak(&Rig::plane(&rig.render(1024), 4, 2)[512..]) == 0.0);
        rig.apply(Command::SetOscillator {
            oscillator: oscillator(OscillatorKind::Sine, -20.0),
        });
        let _ = rig.render(4800);
        // Off: out over 50 ms, with no step.
        rig.apply(Command::OscillatorOn { on: false });
        let out = Rig::plane(&rig.render(4800), 4, 2);
        assert!(peak(&out[..240]) > 0.05);
        assert!(peak(&out[2410..]) == 0.0, "{}", peak(&out[2410..]));
        assert!(largest_step(&out) < sine_step * 1.01);
        assert!(!rig.engine.status().oscillator_on);
        // A load switches it off.
        rig.apply(Command::OscillatorOn { on: true });
        let session = rig.engine.session().clone();
        rig.engine.load(session).unwrap();
        assert!(!rig.engine.status().oscillator_on);
    }

    #[test]
    fn removing_a_bus_removes_its_matrix_sources_and_destinations() {
        let mut rig = Rig::new();
        let bus = add_bus(&mut rig, "Verb", BusRole::Fx, true);
        let matrix = add_matrix(&mut rig, "Zone", true);
        for source in [MatrixFeed::Bus(bus), MatrixFeed::Master] {
            rig.apply(Command::SetMatrixSend {
                matrix,
                source,
                level_db: -6.0,
                pan: None,
            });
        }
        rig.apply(Command::SetTalkback {
            talkback: TalkbackSettings {
                to: vec![InjectDest::Bus(bus), InjectDest::Master],
                ..TalkbackSettings::default()
            },
        });
        rig.apply(Command::SetOscillator {
            oscillator: OscillatorSettings {
                to: vec![InjectDest::Bus(bus)],
                ..OscillatorSettings::default()
            },
        });
        rig.apply(Command::SetLayer {
            index: 0,
            name: "Out".to_string(),
            strips: vec![StripRef::Bus(bus), StripRef::Matrix(matrix)],
        });
        let before = rig.engine.session().matrices.clone();
        rig.apply(Command::RemoveBus { bus });
        let session = rig.engine.session();
        let sources: Vec<MatrixFeed> = session.matrices[0]
            .sources
            .iter()
            .map(|s| s.source)
            .collect();
        assert_eq!(sources, [MatrixFeed::Master]);
        assert_eq!(session.talkback.to, [InjectDest::Master]);
        assert!(session.oscillator.to.is_empty());
        assert_eq!(session.layers[0].strips, [StripRef::Matrix(matrix)]);
        assert_eq!(rig.graph.as_ref().unwrap().matrices[0].sources.len(), 1);
        // Undo brings the bus, its matrix source and the layer back; the
        // talkback and oscillator setups are not undone.
        rig.undo();
        let session = rig.engine.session();
        assert_eq!(session.matrices, before);
        assert_eq!(session.layers[0].strips.len(), 2);
        assert_eq!(session.talkback.to, [InjectDest::Master]);
        assert_eq!(rig.graph.as_ref().unwrap().matrices[0].sources.len(), 2);
    }

    #[test]
    fn layers_are_set_replaced_removed_and_undone() {
        let mut rig = Rig::new();
        let (a, b) = (rig.channel(0), rig.channel(1));
        let bus = add_bus(&mut rig, "Drums", BusRole::Group, true);
        rig.apply(Command::SetLayer {
            index: 0,
            name: "Band".to_string(),
            strips: vec![a, b, a],
        });
        assert_eq!(rig.engine.session().layers[0].strips, [a, b]);
        assert_eq!(rig.engine.history().undo.as_deref(), Some("Add layer Band"));
        rig.apply(Command::SetLayer {
            index: 1,
            name: "Out".to_string(),
            strips: vec![StripRef::Bus(bus), StripRef::Master],
        });
        rig.apply(Command::SetLayer {
            index: 0,
            name: "Drums".to_string(),
            strips: vec![b],
        });
        assert_eq!(rig.engine.history().undo.as_deref(), Some("Layer Drums"));
        for (index, strips) in [
            (3, vec![a]),
            (MAX_LAYERS, vec![a]),
            (1, vec![StripRef::Channel(999)]),
        ] {
            assert!(
                rig.engine
                    .apply(Command::SetLayer {
                        index,
                        name: "X".to_string(),
                        strips,
                    })
                    .is_err()
            );
        }
        rig.apply(Command::RemoveLayer { index: 0 });
        assert_eq!(rig.engine.session().layers.len(), 1);
        assert_eq!(rig.engine.session().layers[0].name, "Out");
        assert!(rig.engine.apply(Command::RemoveLayer { index: 5 }).is_err());
        rig.undo();
        assert_eq!(rig.engine.session().layers[0].name, "Drums");
        rig.undo();
        assert_eq!(rig.engine.session().layers[0].name, "Band");
        rig.redo();
        assert_eq!(rig.engine.session().layers[0].name, "Drums");
        // A scene does not carry the layers.
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        let scene = rig.engine.session().current_scene.unwrap();
        rig.apply(Command::SetLayer {
            index: 0,
            name: "Later".to_string(),
            strips: vec![a],
        });
        assert_eq!(rig.engine.scene_differs(scene), Some(false));
        rig.apply(Command::SceneRecall { id: scene });
        assert_eq!(rig.engine.session().layers[0].name, "Later");
        // A strip that goes leaves every layer.
        rig.apply(Command::RemoveChannel { channel: rig.id(0) });
        assert!(rig.engine.session().layers[0].strips.is_empty());
    }

    #[test]
    fn scenes_recall_and_undo_matrices_like_buses() {
        let mut rig = Rig::new();
        let bus = add_bus(&mut rig, "Fill", BusRole::Aux, true);
        let matrix = add_matrix(&mut rig, "Zone", true);
        let m = StripRef::Matrix(matrix);
        rig.apply(Command::SetMatrixSend {
            matrix,
            source: MatrixFeed::Master,
            level_db: -6.0,
            pan: None,
        });
        rig.apply(Command::SetMatrixSend {
            matrix,
            source: MatrixFeed::Bus(bus),
            level_db: 0.0,
            pan: Some(0.5),
        });
        rig.apply(Command::SetFader { strip: m, db: -3.0 });
        only_patch(&mut rig, PatchSource::Matrix(matrix));
        rig.apply(Command::SceneStore {
            id: None,
            name: None,
        });
        let scene = rig.engine.session().current_scene.unwrap();
        let stored = rig.engine.session().matrix(matrix).unwrap().clone();
        let heard = rig.play();

        rig.at(
            10_000,
            Command::SetMatrixSend {
                matrix,
                source: MatrixFeed::Master,
                level_db: -20.0,
                pan: None,
            },
        );
        rig.at(
            11_000,
            Command::SetFader {
                strip: m,
                db: -10.0,
            },
        );
        rig.at(
            12_000,
            Command::RenameStrip {
                strip: m,
                name: "Zone B".to_string(),
            },
        );
        only_patch(&mut rig, PatchSource::Master);
        rig.at(
            13_000,
            Command::SetMatrixStereo {
                matrix,
                stereo: false,
            },
        );
        assert_eq!(rig.engine.scene_differs(scene), Some(true));
        rig.at(14_000, Command::SceneRecall { id: scene });
        let now = rig.engine.session().matrix(matrix).unwrap();
        assert_eq!(now.sources, stored.sources);
        assert_eq!((now.core.fader_db, now.name.as_str()), (-3.0, "Zone"));
        assert!(!now.stereo, "the matrix's width is setup, not recalled");
        assert!(
            rig.engine
                .session()
                .outputs
                .iter()
                .any(|p| p.source == PatchSource::Matrix(matrix)),
            "the output patch is recalled"
        );
        assert_eq!(rig.engine.scene_differs(scene), Some(false));
        rig.at(
            15_000,
            Command::SetMatrixStereo {
                matrix,
                stereo: true,
            },
        );
        let out = rig.play();
        assert!(
            close(out[2], heard[2]) && close(out[3], heard[3]),
            "{out:?} {heard:?}"
        );
        // One undo takes the recall back.
        rig.undo();
        rig.undo();
        assert_eq!(
            rig.engine.session().matrix(matrix).unwrap().core.fader_db,
            -10.0
        );
        rig.redo();
        // Recall safe, and the sends scope.
        rig.apply(Command::SetRecallSafe {
            strip: m,
            safe: true,
        });
        rig.apply(Command::SetFader {
            strip: m,
            db: -40.0,
        });
        rig.apply(Command::SceneRecall { id: scene });
        assert_eq!(
            rig.engine.session().matrix(matrix).unwrap().core.fader_db,
            -40.0
        );
        rig.apply(Command::SetRecallSafe {
            strip: m,
            safe: false,
        });
        rig.apply(Command::SceneScope {
            id: scene,
            scope: RecallScope {
                sends: false,
                ..RecallScope::default()
            },
        });
        rig.apply(Command::SetMatrixSend {
            matrix,
            source: MatrixFeed::Master,
            level_db: -30.0,
            pan: None,
        });
        rig.apply(Command::SceneRecall { id: scene });
        let now = rig.engine.session().matrix(matrix).unwrap();
        assert_eq!(now.core.fader_db, -3.0);
        assert_eq!(now.sources[0].level_db, -30.0);
        // A paste reaches a matrix.
        rig.apply(Command::PasteStrip {
            targets: vec![m],
            settings: StripSettings {
                fader_db: Some(-1.0),
                ..StripSettings::default()
            },
            sections: vec![Section::FaderPan],
        });
        assert_eq!(
            rig.engine.session().matrix(matrix).unwrap().core.fader_db,
            -1.0
        );
    }

    #[test]
    fn matrices_record_like_buses_and_meter() {
        let mut session = Session::with_inputs(&["Vox".to_string()]);
        let id = session.allocate_id();
        session.matrices.push(MatrixStrip {
            id,
            name: "Record feed".to_string(),
            record_arm: true,
            ..MatrixStrip::default()
        });
        assert_eq!(
            recording_strips(&session),
            vec![(StripRef::Matrix(id), "Record feed".to_string(), 2)]
        );
        let (mut engine, _graphs) = LiveEngine::offline(session, 1, 2);
        engine
            .apply(Command::SetRecordArm {
                strip: StripRef::Matrix(id),
                arm: false,
            })
            .unwrap();
        assert!(!engine.session().matrix(id).unwrap().record_arm);
        assert!(engine.meters().contains_key(&StripRef::Matrix(id)));
        let status = serde_json::to_value(engine.status()).unwrap();
        assert_eq!(status["talkback_active"], false);
        assert_eq!(status["oscillator_on"], false);
    }

    /// Every Phase 3 command parses as the web UI sends it.
    #[test]
    fn the_bus_and_monitor_commands_parse_from_json() {
        let lines = [
            r#"{"cmd":"add_bus","name":"Mon 1","role":"aux","stereo":false}"#,
            r#"{"cmd":"add_bus","name":"Grp"}"#,
            r#"{"cmd":"set_bus_role","bus":4,"role":"fx"}"#,
            r#"{"cmd":"set_bus_stereo","bus":4,"stereo":false}"#,
            r#"{"cmd":"set_send","channel":3,"bus":4,"level_db":-6}"#,
            r#"{"cmd":"set_send","channel":3,"bus":4,"level_db":-6,"pre_fader":true,"pan":-0.5,"pan_follow":false}"#,
            r#"{"cmd":"add_matrix","name":"Matrix 1","stereo":true}"#,
            r#"{"cmd":"add_matrix","name":"Matrix 2"}"#,
            r#"{"cmd":"remove_matrix","matrix":9}"#,
            r#"{"cmd":"set_matrix_send","matrix":9,"source":{"kind":"master"},"level_db":-6,"pan":0}"#,
            r#"{"cmd":"set_matrix_send","matrix":9,"source":{"kind":"bus","id":4},"level_db":-6}"#,
            r#"{"cmd":"move_matrix","matrix":9,"index":0}"#,
            r#"{"cmd":"set_matrix_stereo","matrix":9,"stereo":false}"#,
            r#"{"cmd":"set_talkback","talkback":{"input":3,"level_db":0,"hpf":true,"to":[{"kind":"bus","id":4},{"kind":"matrix","id":9},{"kind":"master"},{"kind":"monitor"}]}}"#,
            r#"{"cmd":"set_talkback","talkback":{"input":null}}"#,
            r#"{"cmd":"talk","active":true}"#,
            r#"{"cmd":"set_oscillator","oscillator":{"kind":"pink","hz":1000,"level_db":-20,"to":[{"kind":"master"}]}}"#,
            r#"{"cmd":"oscillator_on","on":true}"#,
            r#"{"cmd":"set_monitor","monitor":{"solo_mode":"pfl","level_db":0,"dim":false,"source":{"kind":"matrix","id":9}}}"#,
            r#"{"cmd":"set_monitor","monitor":{"solo_mode":"afl","level_db":0,"dim":false}}"#,
            r#"{"cmd":"set_layer","index":0,"name":"Band","strips":[{"kind":"channel","id":3},{"kind":"matrix","id":9},{"kind":"master"}]}"#,
            r#"{"cmd":"remove_layer","index":0}"#,
            r#"{"cmd":"set_fader","strip":{"kind":"matrix","id":9},"db":-3}"#,
            r#"{"cmd":"set_output_patch","patches":[{"source":{"kind":"matrix","id":9},"left":4,"right":5}]}"#,
        ];
        for line in lines {
            let command: Command =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&command).unwrap()).unwrap();
            assert_eq!(back, command, "{line}");
        }
        let Command::SetSend {
            pre_fader,
            pan,
            pan_follow,
            ..
        } = serde_json::from_str(lines[4]).unwrap()
        else {
            unreachable!()
        };
        assert_eq!((pre_fader, pan, pan_follow), (None, None, None));
        let Command::SetTalkback { talkback } = serde_json::from_str(lines[14]).unwrap() else {
            unreachable!()
        };
        assert_eq!(talkback, TalkbackSettings::default());
        let Command::SetMonitor { monitor } = serde_json::from_str(lines[19]).unwrap() else {
            unreachable!()
        };
        assert_eq!(monitor.source, MonitorSource::Master);
    }

    /// One file of a test take: name (`.flac`: 24-bit FLAC, else float
    /// WAV), channels, rate, and its sample at `(frame, channel)`.
    type TakeSpec<'a> = (&'a str, u16, u32, &'a dyn Fn(usize, usize) -> f32);

    /// A take folder of files `frames` long, as the recorder writes them.
    fn write_take(name: &str, files: &[TakeSpec], frames: usize) -> std::path::PathBuf {
        use sphere_encoder::{
            AudioEncodeOptions, AudioEncodeSpec, AudioFileFormat, AudioSampleFormat, create_encoder,
        };
        let dir =
            std::env::temp_dir().join(format!("livestage-take-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, channels, rate, sample) in files {
            let flac = file.ends_with(".flac");
            let mut encoder = create_encoder(
                &dir.join(file),
                AudioEncodeSpec {
                    sample_rate: *rate,
                    channels: *channels,
                    sample_format: if flac {
                        AudioSampleFormat::I24
                    } else {
                        AudioSampleFormat::F32
                    },
                },
                AudioEncodeOptions {
                    format: if flac {
                        AudioFileFormat::Flac
                    } else {
                        AudioFileFormat::Wav
                    },
                    ..AudioEncodeOptions::default()
                },
            )
            .unwrap();
            let samples: Vec<f32> = (0..frames)
                .flat_map(|f| (0..usize::from(*channels)).map(move |c| sample(f, c)))
                .collect();
            encoder.write_interleaved_f32(&samples).unwrap();
            encoder.finalize().unwrap();
        }
        dir
    }

    impl Rig {
        fn player(&self) -> &Player {
            self.engine.player.as_ref().expect("a take is loaded")
        }

        /// Play blocks (the audio thread's part of a locate) until the
        /// reader has the rings filled for the latest locate.
        fn until_ready(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.player().is_ready() {
                assert!(
                    Instant::now() < deadline,
                    "the reader never filled the rings"
                );
                self.render(128);
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Channel `index`'s input meter over `frames` more frames.
        fn input_level(&mut self, index: usize, frames: usize) -> (f32, f32) {
            let _ = self.engine.meters();
            self.render(frames);
            self.engine.meters()[&self.channel(index)].input
        }
    }

    /// Channel A (live 0.5) and B (live 0.25), mono; a take of `A.wav` and
    /// `Other.flac`.
    fn playback_rig(name: &str) -> (Rig, std::path::PathBuf) {
        let take = write_take(
            name,
            &[
                ("A.wav", 1, 48_000, &|_, _| 0.3),
                ("Other.flac", 1, 48_000, &|_, _| 0.125),
            ],
            48_000,
        );
        let mut rig = Rig::new();
        rig.apply(Command::PlaybackLoad {
            folder: take.clone(),
        });
        (rig, take)
    }

    #[test]
    fn a_take_loads_each_file_onto_the_channel_of_its_name() {
        let (rig, take) = playback_rig("assign");
        let playback = &rig.engine.session().playback;
        assert_eq!(playback.folder.as_deref(), Some(take.as_path()));
        assert!(!playback.virtual_soundcheck);
        let tracks: Vec<(&str, u16, Option<Id>)> = playback
            .tracks
            .iter()
            .map(|t| (t.file.as_str(), t.channels, t.channel))
            .collect();
        assert_eq!(
            tracks,
            [("A.wav", 1, Some(rig.id(0))), ("Other.flac", 1, None)]
        );
        let status = rig.engine.status().playback;
        assert_eq!(status.state, crate::playback::PlaybackState::Stopped);
        assert!((status.duration - 1.0).abs() < 1e-9);
        assert_eq!(status.error, None);
        let _ = std::fs::remove_dir_all(&take);
    }

    #[test]
    fn virtual_soundcheck_replaces_only_the_assigned_channels() {
        let (mut rig, take) = playback_rig("vsc");
        // Not yet on: the live inputs.
        rig.apply(Command::Playback {
            action: Transport::Play,
        });
        rig.until_ready();
        assert!(close(rig.input_level(0, 1024).0, 0.5));
        rig.apply(Command::SetVirtualSoundcheck { on: true });
        assert!(rig.engine.session().playback.virtual_soundcheck);
        // Past the 10 ms crossfade: A plays its file, B stays live.
        rig.render(1024);
        assert!(close(rig.input_level(0, 1024).0, 0.3));
        assert!(close(rig.input_level(1, 1024).0, 0.25));
        // The FLAC file, onto B, replaces B as well.
        rig.apply(Command::PlaybackAssign {
            file: "Other.flac".into(),
            channel: Some(rig.id(1)),
        });
        rig.render(1024);
        assert!(close(rig.input_level(1, 1024).0, 0.125));
        // Paused: a playback input is silence.
        rig.apply(Command::Playback {
            action: Transport::Pause,
        });
        assert_eq!(rig.input_level(0, 1024).0, 0.0);
        assert_eq!(
            rig.engine.status().playback.state,
            crate::playback::PlaybackState::Paused
        );
        // Off: live again.
        rig.apply(Command::SetVirtualSoundcheck { on: false });
        rig.render(1024);
        assert!(close(rig.input_level(0, 1024).0, 0.5));
        assert!(close(rig.input_level(1, 1024).0, 0.25));
        assert_eq!(rig.engine.status().playback.underruns, 0);
        let _ = std::fs::remove_dir_all(&take);
    }

    #[test]
    fn a_mono_channel_takes_both_sides_and_a_stereo_one_doubles_a_mono_file() {
        let take = write_take(
            "widths",
            &[
                ("A.wav", 2, 48_000, &|_, c| if c == 0 { 0.2 } else { 0.6 }),
                ("B.wav", 1, 48_000, &|_, _| 0.3),
            ],
            48_000,
        );
        let mut rig = Rig::new();
        let b = rig.id(1);
        rig.apply(Command::SetChannelInput {
            channel: b,
            input: InputPatch {
                left: Some(0),
                right: Some(1),
            },
        });
        rig.apply(Command::PlaybackLoad {
            folder: take.clone(),
        });
        rig.apply(Command::SetVirtualSoundcheck { on: true });
        rig.apply(Command::Playback {
            action: Transport::Play,
        });
        rig.until_ready();
        rig.render(1024);
        let (l, r) = rig.input_level(0, 1024);
        assert!(close(l, 0.4) && close(r, 0.4), "(L+R)/2: {l} {r}");
        let (l, r) = rig.input_level(1, 1024);
        assert!(close(l, 0.3) && close(r, 0.3), "both sides: {l} {r}");
        let _ = std::fs::remove_dir_all(&take);
    }

    #[test]
    fn switching_virtual_soundcheck_crossfades() {
        let (mut rig, take) = playback_rig("fade");
        rig.apply(Command::Playback {
            action: Transport::Play,
        });
        rig.until_ready();
        let before = rig.render(1024);
        rig.apply(Command::SetVirtualSoundcheck { on: true });
        let after = rig.render(2048);
        let width = rig.graph.as_ref().unwrap().out_channels;
        let master: Vec<f32> = Rig::plane(&before, width, 0)
            .into_iter()
            .chain(Rig::plane(&after, width, 0))
            .collect();
        let swing = (master[0] - master[master.len() - 1]).abs();
        assert!(swing > 0.05, "the switch changed the mix: {swing}");
        let fade = (crate::graph::BYPASS_FADE_SECONDS * 48_000.0) as usize;
        let step = largest_step(&master);
        assert!(step <= swing / fade as f32 * 1.5, "a step of {step}");
        // And done after the fade.
        let settled = &master[1024 + fade + 2..];
        assert!(largest_step(settled) < 1e-6);
        let _ = std::fs::remove_dir_all(&take);
    }

    #[test]
    fn locate_plays_from_there_after_the_rings_refill() {
        // WAV seeks by byte offset, FLAC by searching its frames.
        locate_in("A.wav");
        locate_in("A.flac");
    }

    fn locate_in(file: &str) {
        // A falling ramp: the level says where it plays.
        let take = write_take(
            &format!("locate-{file}"),
            &[(file, 1, 48_000, &|f, _| 1.0 - f as f32 / 48_000.0)],
            48_000,
        );
        let mut rig = Rig::new();
        rig.apply(Command::PlaybackLoad {
            folder: take.clone(),
        });
        rig.apply(Command::SetVirtualSoundcheck { on: true });
        rig.apply(Command::Playback {
            action: Transport::Play,
        });
        rig.until_ready();
        rig.apply(Command::PlaybackLocate { seconds: 0.5 });
        assert!((rig.engine.status().playback.position - 0.5).abs() < 1e-9);
        rig.until_ready();
        let (level, _) = rig.input_level(0, 128);
        assert!((level - 0.5).abs() < 0.01, "{level}");
        // Stop: back to the start, and silence.
        rig.apply(Command::Playback {
            action: Transport::Stop,
        });
        assert_eq!(rig.engine.status().playback.position, 0.0);
        rig.until_ready();
        assert_eq!(rig.input_level(0, 256).0, 0.0);
        // Played to its end, the take stops and goes back to the start.
        rig.apply(Command::PlaybackLocate { seconds: 0.99 });
        rig.apply(Command::Playback {
            action: Transport::Play,
        });
        rig.until_ready();
        rig.render(4800);
        rig.engine.poll();
        let status = rig.engine.status().playback;
        assert_eq!(status.state, crate::playback::PlaybackState::Stopped);
        assert_eq!(status.position, 0.0);
        assert_eq!(status.underruns, 0);
        let _ = std::fs::remove_dir_all(&take);
    }

    #[test]
    fn a_take_at_another_rate_is_refused_and_the_loaded_one_stays() {
        let (mut rig, take) = playback_rig("rate");
        let other = write_take("rate-44k", &[("A.wav", 1, 44_100, &|_, _| 0.1)], 4410);
        let error = rig
            .engine
            .apply(Command::PlaybackLoad {
                folder: other.clone(),
            })
            .unwrap_err();
        assert!(
            error.contains("44100 Hz") && error.contains("48000 Hz"),
            "{error}"
        );
        assert_eq!(
            rig.engine.session().playback.folder.as_deref(),
            Some(take.as_path())
        );
        assert!(rig.engine.player.is_some());
        // Without a take, the transport says so.
        rig.apply(Command::PlaybackUnload);
        assert_eq!(rig.engine.session().playback, Default::default());
        assert!(
            rig.engine
                .apply(Command::Playback {
                    action: Transport::Play
                })
                .is_err()
        );
        assert!(
            rig.engine
                .apply(Command::SetVirtualSoundcheck { on: true })
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&take);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn a_saved_show_reopens_its_take_with_virtual_soundcheck_off() {
        let (mut rig, take) = playback_rig("reload");
        rig.apply(Command::PlaybackAssign {
            file: "A.wav".into(),
            channel: Some(rig.id(1)),
        });
        rig.apply(Command::SetVirtualSoundcheck { on: true });
        // Not undoable, and not part of a scene.
        assert_eq!(rig.engine.history().undo_depth, 0);
        let file = std::env::temp_dir().join(format!(
            "livestage-playback-show-{}.json",
            std::process::id()
        ));
        rig.engine.session().save(&file).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["playback"]["virtual_soundcheck"], true);
        let loaded = Session::load(&file).unwrap();
        assert!(!loaded.playback.virtual_soundcheck);
        rig.engine.load(loaded).unwrap();
        rig.adopt();
        let playback = &rig.engine.session().playback;
        assert!(!playback.virtual_soundcheck);
        assert_eq!(playback.tracks[0].channel, Some(rig.id(1)));
        assert!(rig.engine.player.is_some());
        assert_eq!(rig.engine.status().playback.error, None);
        // A show whose take is gone loads, with the reason in the status.
        let _ = std::fs::remove_dir_all(&take);
        let mut session = rig.engine.session().clone();
        session.playback.virtual_soundcheck = true;
        rig.engine.load(session).unwrap();
        assert!(rig.engine.player.is_none());
        assert!(!rig.engine.session().playback.virtual_soundcheck);
        assert!(rig.engine.status().playback.error.is_some());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_show_without_playback_loads() {
        let session: Session =
            serde_json::from_str(r#"{"name": "Old show", "channels": []}"#).unwrap();
        assert_eq!(
            session.playback,
            crate::session::PlaybackSettings::default()
        );
        let engine = Rig::new();
        assert_eq!(engine.engine.status().playback.duration, 0.0);
    }

    #[test]
    fn the_playback_commands_parse_from_json() {
        for line in [
            r#"{"cmd":"playback_load","folder":"/data/recordings/Take 1"}"#,
            r#"{"cmd":"playback_assign","file":"Kick.wav","channel":3}"#,
            r#"{"cmd":"playback_assign","file":"Kick.wav","channel":null}"#,
            r#"{"cmd":"playback_unload"}"#,
            r#"{"cmd":"playback","action":"play"}"#,
            r#"{"cmd":"playback","action":"pause"}"#,
            r#"{"cmd":"playback","action":"stop"}"#,
            r#"{"cmd":"playback_locate","seconds":12.5}"#,
            r#"{"cmd":"playback_loop","on":true}"#,
            r#"{"cmd":"set_virtual_soundcheck","on":true}"#,
        ] {
            let command: Command =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(!is_undoable(&command), "{line}");
        }
    }
}
