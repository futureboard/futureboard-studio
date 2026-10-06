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
    AtomicF32, Dest, Graph, InsertCell, InsertDsp, PatchFrom, RecordStream, RtBus, RtChannel,
    RtPatch, RtSend, RtStrip, StripShared,
};
use crate::recorder::{Recording, RecordingSummary};
use crate::session::{
    AudioSettings, BusStrip, ChannelStrip, Id, InputPatch, InsertPlugin, InsertSlot, OutputPatch,
    PatchSource, RecordSettings, RecordTap, SendSlot, Session, StripOutput, StripRef, db_to_gain,
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
    AddBus {
        name: String,
    },
    RemoveBus {
        bus: Id,
    },
    SetSend {
        channel: Id,
        bus: Id,
        level_db: f32,
        pre_fader: bool,
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
}

/// Peak levels since the last [`LiveEngine::meters`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct StripLevels {
    /// Post-fader output.
    pub output: (f32, f32),
    /// Channel input after trim (zero for buses and the master).
    pub input: (f32, f32),
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
    pub input_underruns: u64,
    pub error: Option<String>,
    pub recording_seconds: Option<f64>,
    pub recording_dropped: u64,
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
    sends: HashMap<(Id, Id), Arc<AtomicF32>>,
    inserts: HashMap<Id, InsertEntry>,
    insert_states: HashMap<Id, InsertState>,
    recording: Option<Recording>,
    /// Each recorded strip's ring, while recording.
    record_taps: HashMap<StripRef, Arc<RecordStream>>,
    last_recording: Option<RecordingSummary>,
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
        let (graph_tx, graph_rx) = crossbeam_channel::bounded::<Box<Graph>>(8);
        let (garbage_tx, garbage_rx) = crossbeam_channel::bounded::<Box<Graph>>(16);
        let mut engine = Self {
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
            inserts: HashMap::new(),
            insert_states: HashMap::new(),
            recording: None,
            record_taps: HashMap::new(),
            last_recording: None,
            #[cfg(feature = "external-plugins")]
            external: None,
            #[cfg(feature = "external-plugins")]
            external_events: Vec::new(),
        };
        engine.open_device(graph_rx, garbage_tx);
        engine.refresh_gains();
        engine.rebuild();
        engine
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
        }
    }

    /// Peaks since the last call, per strip.
    pub fn meters(&self) -> HashMap<StripRef, StripLevels> {
        self.strips
            .iter()
            .map(|(strip, shared)| {
                (
                    *strip,
                    StripLevels {
                        output: shared.meter.take(),
                        input: shared.input_meter.take(),
                    },
                )
            })
            .collect()
    }

    /// Housekeeping at UI rate: free the graphs the audio thread is done
    /// with, and take in what the plug-in host reported.
    pub fn poll(&mut self) {
        while self.garbage_rx.try_recv().is_ok() {}
        #[cfg(feature = "external-plugins")]
        self.poll_external();
    }

    pub fn apply(&mut self, command: Command) -> Result<(), String> {
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
                self.sends.retain(|(from, _), _| *from != channel);
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
                }
                self.refresh_gains();
                self.rebuild();
            }
            Command::AddBus { name } => {
                let id = self.session.allocate_id();
                self.session.buses.push(BusStrip {
                    id,
                    name,
                    ..BusStrip::default()
                });
                self.refresh_gains();
                self.rebuild();
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
                self.session
                    .outputs
                    .retain(|p| p.source != PatchSource::Bus(bus));
                self.strips.remove(&StripRef::Bus(bus));
                self.sends.retain(|(_, to), _| *to != bus);
                self.refresh_gains();
                self.rebuild();
            }
            Command::SetSend {
                channel,
                bus,
                level_db,
                pre_fader,
            } => {
                if self.session.bus(bus).is_none() {
                    return Err(format!("no bus {bus}"));
                }
                let strip = self.channel_mut(channel)?;
                let level_db =
                    level_db.clamp(crate::session::MIN_FADER_DB, crate::session::MAX_FADER_DB);
                let mut reshaped = true;
                match strip.sends.iter_mut().find(|s| s.bus == bus) {
                    Some(send) => {
                        reshaped = send.pre_fader != pre_fader;
                        send.level_db = level_db;
                        send.pre_fader = pre_fader;
                    }
                    None => strip.sends.push(SendSlot {
                        bus,
                        level_db,
                        pre_fader,
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
                plugin,
                index,
            } => {
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
        }
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

    fn insert_slot_mut(&mut self, insert: Id) -> Result<&mut InsertSlot, String> {
        let session = &mut self.session;
        session
            .channels
            .iter_mut()
            .map(|c| &mut c.core)
            .chain(session.buses.iter_mut().map(|b| &mut b.core))
            .chain(std::iter::once(&mut session.master.core))
            .flat_map(|core| core.inserts.iter_mut())
            .find(|slot| slot.id == insert)
            .ok_or_else(|| format!("no insert {insert}"))
    }

    fn all_insert_slots(&self) -> Vec<InsertSlot> {
        self.session
            .channels
            .iter()
            .map(|c| &c.core)
            .chain(self.session.buses.iter().map(|b| &b.core))
            .chain(std::iter::once(&self.session.master.core))
            .flat_map(|core| core.inserts.iter().cloned())
            .collect()
    }

    /// Fader, pan, solo/mute and trim into every strip's gain cells. Cheap
    /// enough to run after any of them changes.
    fn refresh_gains(&mut self) {
        let session = &self.session;
        let mut targets: Vec<(StripRef, f32, f32, f32)> = Vec::new();
        for channel in &session.channels {
            let strip = StripRef::Channel(channel.id);
            let gain = if session.audible(strip) {
                db_to_gain(channel.core.fader_db)
            } else {
                0.0
            };
            let (l, r) = if channel.is_stereo() {
                balance(channel.core.pan)
            } else {
                constant_power(channel.core.pan)
            };
            let trim = db_to_gain(channel.trim_db) * if channel.phase_invert { -1.0 } else { 1.0 };
            targets.push((strip, gain * l, gain * r, trim));
        }
        for bus in &session.buses {
            let strip = StripRef::Bus(bus.id);
            let gain = if session.audible(strip) {
                db_to_gain(bus.core.fader_db)
            } else {
                0.0
            };
            let (l, r) = balance(bus.core.pan);
            targets.push((strip, gain * l, gain * r, 1.0));
        }
        let master = &session.master.core;
        let gain = if master.mute {
            0.0
        } else {
            db_to_gain(master.fader_db)
        };
        let (l, r) = balance(master.pan);
        targets.push((StripRef::Master, gain * l, gain * r, 1.0));

        for (strip, l, r, trim) in targets {
            let shared = self.strips.entry(strip).or_default();
            shared.gain_l.store(l);
            shared.gain_r.store(r);
            shared.trim.store(trim);
        }
        for channel in &self.session.channels {
            for send in &channel.sends {
                let gain = db_to_gain(send.level_db);
                self.sends
                    .entry((channel.id, send.bus))
                    .or_insert_with(|| Arc::new(AtomicF32::new(gain)))
                    .store(gain);
            }
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
        let (cell, params) = InsertCell::new(dsp, slot.bypass);
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

    /// Compile the session into a graph and hand it to the audio thread.
    fn rebuild(&mut self) {
        self.ensure_inserts();
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
                            let gain = self.sends.get(&(channel.id, send.bus))?.clone();
                            Some(RtSend::new(bus, gain, send.pre_fader))
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
                        cells(&bus.core),
                        record_taps.get(&strip).cloned(),
                    ),
                    dest: match bus.output {
                        StripOutput::Master => Dest::Master,
                        _ => Dest::None,
                    },
                }
            })
            .collect();
        let master = RtStrip::new(
            shared(StripRef::Master),
            cells(&session.master.core),
            record_taps.get(&StripRef::Master).cloned(),
        );
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
                    },
                    left: usize::from(p.left),
                    right: p.right.map(usize::from).filter(|r| *r < out_ch),
                })
            })
            .collect();
        let mut graph = Graph::new(in_ch, out_ch, channels, buses, master, patches);
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
    pub fn load(&mut self, session: Session) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("stop recording before opening another session".to_string());
        }
        let ids: Vec<Id> = self.inserts.keys().copied().collect();
        for id in ids {
            self.drop_insert(id);
        }
        self.strips.clear();
        self.sends.clear();
        let audio = session.audio.clone();
        let reopen = audio != self.session.audio;
        self.session = session;
        self.refresh_gains();
        if reopen {
            self.set_audio(audio)
        } else {
            self.rebuild();
            Ok(())
        }
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
    strips
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
        use crate::external::ExternalEvent;
        let external: Vec<Id> = self
            .all_insert_slots()
            .into_iter()
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
}
