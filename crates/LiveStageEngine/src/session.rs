//! What a live setup *is*: the device, the strips, their inserts, the patch
//! and the recording settings. Plain data, saved as JSON, shared by the GUI
//! and the headless server. The realtime side never reads it: the engine
//! compiles it into a [`crate::graph::Graph`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::processing::{EQ_BANDS, MAX_DELAY_MS, Processing};

/// Identifies a strip, a bus or an insert within one session. Stable across
/// save and load, so a patch or a send keeps pointing at the same thing.
pub type Id = u32;

/// DCAs in a session: always exactly this many.
pub const DCA_COUNT: usize = 8;
/// Mute groups in a session: always exactly this many.
pub const MUTE_GROUP_COUNT: usize = 8;
/// Strip and DCA colours are indices into a palette of this many.
pub const PALETTE_COLORS: u8 = 12;
/// What dimming the monitor takes off, dB. Talkback dims it by the same.
pub const MONITOR_DIM_DB: f32 = -20.0;
/// Custom layers (fader banks) a session keeps at most.
pub const MAX_LAYERS: usize = 8;
/// The oscillator's level range, dBFS; the bottom is off.
pub const OSCILLATOR_MIN_DB: f32 = MIN_FADER_DB;
pub const OSCILLATOR_MAX_DB: f32 = 0.0;
/// The oscillator's sine frequency range, Hz.
pub const OSCILLATOR_MIN_HZ: f32 = 20.0;
pub const OSCILLATOR_MAX_HZ: f32 = 20_000.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub name: String,
    pub audio: AudioSettings,
    /// In the order they are shown.
    pub channels: Vec<ChannelStrip>,
    /// In the order they are shown.
    pub buses: Vec<BusStrip>,
    pub master: MasterStrip,
    /// In the order they are shown. Each sums the master and buses after
    /// their faders and feeds only its own output patch.
    pub matrices: Vec<MatrixStrip>,
    /// Where each mix leaves the interface. A source may be patched to any
    /// number of outputs; an output may take any number of sources (summed).
    pub outputs: Vec<OutputPatch>,
    pub recording: RecordSettings,
    /// Always [`DCA_COUNT`] after [`Session::normalize`].
    pub dcas: Vec<Dca>,
    /// Always [`MUTE_GROUP_COUNT`] after [`Session::normalize`].
    pub mute_groups: Vec<MuteGroup>,
    pub monitor: MonitorSettings,
    /// The engineer's talkback mic. Not in scenes, not undoable.
    pub talkback: TalkbackSettings,
    /// The test oscillator. Not in scenes, not undoable.
    pub oscillator: OscillatorSettings,
    /// Custom fader banks, at most [`MAX_LAYERS`]. Undoable, not in scenes.
    pub layers: Vec<Layer>,
    /// The take loaded for playback (virtual soundcheck). Not in scenes, not
    /// undoable.
    pub playback: PlaybackSettings,
    /// The scene list, in the order shown. Each holds a whole [`MixState`].
    pub scenes: Vec<Scene>,
    /// The scene last recalled or stored, if it still exists.
    pub current_scene: Option<Id>,
    /// Next free [`Id`]. Never reused within a session.
    pub next_id: Id,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            name: "Untitled".to_string(),
            audio: AudioSettings::default(),
            channels: Vec::new(),
            buses: Vec::new(),
            master: MasterStrip::default(),
            matrices: Vec::new(),
            outputs: vec![OutputPatch {
                source: PatchSource::Master,
                left: 0,
                right: Some(1),
            }],
            recording: RecordSettings::default(),
            dcas: (0..DCA_COUNT).map(Dca::numbered).collect(),
            mute_groups: (0..MUTE_GROUP_COUNT).map(MuteGroup::numbered).collect(),
            monitor: MonitorSettings::default(),
            talkback: TalkbackSettings::default(),
            oscillator: OscillatorSettings::default(),
            layers: Vec::new(),
            playback: PlaybackSettings::default(),
            scenes: Vec::new(),
            current_scene: None,
            next_id: 1,
        }
    }
}

/// The mix a scene stores and a recall sets: every strip, the output patch,
/// the DCAs and the mute groups. Not the machine (audio device, recording,
/// monitor), not the scene list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixState {
    pub channels: Vec<ChannelStrip>,
    pub buses: Vec<BusStrip>,
    pub master: MasterStrip,
    pub matrices: Vec<MatrixStrip>,
    pub outputs: Vec<OutputPatch>,
    pub dcas: Vec<Dca>,
    pub mute_groups: Vec<MuteGroup>,
}

/// One stored scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub id: Id,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub note: String,
    /// What recalling this scene touches.
    #[serde(default)]
    pub scope: RecallScope,
    #[serde(default)]
    pub mix: MixState,
}

/// What a scene's recall touches. Everything by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallScope {
    /// Trim, polarity, input patch.
    pub input: bool,
    /// The strip's processing section.
    pub processing: bool,
    /// Insert parameters and bypass, for inserts present now and in the
    /// scene.
    pub inserts: bool,
    /// Strip faders and DCA levels.
    pub faders: bool,
    /// Strip mutes, DCA mutes, mute groups on or off.
    pub mutes: bool,
    pub pan: bool,
    /// Send levels and pre/post.
    pub sends: bool,
    /// Strip outputs (master, bus, none) and the output patch.
    pub routing: bool,
    /// DCA and mute-group membership.
    pub assign: bool,
    /// Strip, DCA and mute-group names, and colours.
    pub names: bool,
}

impl Default for RecallScope {
    fn default() -> Self {
        Self {
            input: true,
            processing: true,
            inserts: true,
            faders: true,
            mutes: true,
            pan: true,
            sends: true,
            routing: true,
            assign: true,
            names: true,
        }
    }
}

/// A part of a strip that a paste carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    /// The whole processing section.
    Processing,
    Hpf,
    Gate,
    Eq,
    Comp,
    Delay,
    /// The insert rack, replaced by copies.
    Inserts,
    /// Channels only.
    Sends,
    FaderPan,
    /// Trim and polarity; channels only.
    Input,
    NameColor,
}

/// One insert a paste copies: a new insert of the same plug-in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InsertSettings {
    #[serde(default)]
    pub bypass: bool,
    pub plugin: InsertPlugin,
}

/// What a copied strip carries. Every field is optional: a paste applies the
/// sections it is asked for, from the fields that are here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing: Option<Processing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inserts: Option<Vec<InsertSettings>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sends: Option<Vec<SendSlot>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fader_db: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pan: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trim_db: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_invert: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absent: leave the colour; `null`: the default colour; a palette index.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_option"
    )]
    pub color: Option<Option<u8>>,
}

/// A field that may be absent (`None`), `null` (`Some(None)`) or a value.
fn present_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<u8>>, D::Error> {
    Option::<u8>::deserialize(d).map(Some)
}

/// A DCA: one fader that offsets the level of every strip assigned to it,
/// without moving their faders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dca {
    pub name: String,
    /// Added to each member's fader, dB. [`MIN_FADER_DB`] and below silences
    /// the members.
    pub level_db: f32,
    pub mute: bool,
    /// Palette index; `None` is the default colour.
    pub color: Option<u8>,
}

impl Default for Dca {
    fn default() -> Self {
        Self::numbered(0)
    }
}

impl Dca {
    /// The default for DCA `index` (0-based): "DCA 1" …
    pub fn numbered(index: usize) -> Self {
        Self {
            name: format!("DCA {}", index + 1),
            level_db: 0.0,
            mute: false,
            color: None,
        }
    }
}

/// A mute group: while active, every strip assigned to it is muted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MuteGroup {
    pub name: String,
    pub active: bool,
}

impl Default for MuteGroup {
    fn default() -> Self {
        Self::numbered(0)
    }
}

impl MuteGroup {
    /// The default for group `index` (0-based): "Mute 1" …
    pub fn numbered(index: usize) -> Self {
        Self {
            name: format!("Mute {}", index + 1),
            active: false,
        }
    }
}

/// What a solo does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoloMode {
    /// Soloed strips, before their faders, on the monitor bus; the main mix
    /// is untouched.
    #[default]
    Pfl,
    /// Soloed strips, after their faders and pans, on the monitor bus; the
    /// main mix is untouched.
    Afl,
    /// Solo in place: every strip that is not soloed (and not solo safe) is
    /// silenced in the main mix. The monitor bus carries the master.
    Sip,
}

/// The monitor bus: what the engineer listens to (headphones, a wedge),
/// patched to outputs as [`PatchSource::Monitor`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorSettings {
    pub solo_mode: SoloMode,
    /// The monitor bus level, dB.
    pub level_db: f32,
    /// Turns the monitor bus down by [`MONITOR_DIM_DB`].
    pub dim: bool,
    /// What the monitor bus plays while nothing is soloed.
    pub source: MonitorSource,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            solo_mode: SoloMode::Pfl,
            level_db: 0.0,
            dim: false,
            source: MonitorSource::Master,
        }
    }
}

/// What the monitor bus plays while nothing is soloed: the master, or a
/// bus or matrix after its fader (a wedge or IEM mix to listen to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum MonitorSource {
    #[default]
    Master,
    Bus(Id),
    Matrix(Id),
}

/// Where talkback or the oscillator is added: into a mix's sum, before its
/// processing, or into the monitor bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum InjectDest {
    Bus(Id),
    Matrix(Id),
    Master,
    Monitor,
}

/// The engineer's talkback mic: an interface input of its own (it may also
/// feed a channel), high-passed, at a level, into the chosen mixes while
/// talk is pressed. Talking dims the monitor bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TalkbackSettings {
    /// The interface input (0-based); `None` is no talkback mic.
    pub input: Option<u16>,
    pub level_db: f32,
    /// A 100 Hz high-pass (12 dB/oct) against handling noise and pops.
    pub hpf: bool,
    pub to: Vec<InjectDest>,
}

impl Default for TalkbackSettings {
    fn default() -> Self {
        Self {
            input: None,
            level_db: 0.0,
            hpf: true,
            to: Vec::new(),
        }
    }
}

/// What the test oscillator plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OscillatorKind {
    #[default]
    Sine,
    Pink,
    White,
}

/// The test oscillator: a generator at a level into the chosen mixes while
/// it is on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OscillatorSettings {
    pub kind: OscillatorKind,
    /// The sine's frequency.
    pub hz: f32,
    /// Peak level, dBFS ([`OSCILLATOR_MIN_DB`] … 0; the bottom is off).
    pub level_db: f32,
    pub to: Vec<InjectDest>,
}

impl Default for OscillatorSettings {
    fn default() -> Self {
        Self {
            kind: OscillatorKind::Sine,
            hz: 1000.0,
            level_db: -20.0,
            to: Vec::new(),
        }
    }
}

/// A recorded take loaded for playback, and whether its files replace the
/// interface inputs of the channels they are assigned to (virtual
/// soundcheck).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackSettings {
    /// The take folder; `None`: no take loaded.
    pub folder: Option<PathBuf>,
    /// Every file of the take, by name.
    pub tracks: Vec<PlaybackTrack>,
    /// Assigned channels take their input from their file. Always off after
    /// a show is loaded.
    pub virtual_soundcheck: bool,
}

/// One file of the loaded take and the channel it feeds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackTrack {
    /// The file's name in the take folder.
    pub file: String,
    /// The file's channels.
    pub channels: u16,
    /// The channel it feeds in virtual soundcheck; one file per channel.
    pub channel: Option<Id>,
}

/// A custom fader bank: the strips it shows, in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layer {
    pub name: String,
    pub strips: Vec<StripRef>,
}

impl Session {
    /// A session with one mono channel per interface input, named after it,
    /// and the master on the first output pair: what a mixer looks like the
    /// moment it is plugged in.
    pub fn with_inputs(input_names: &[String]) -> Self {
        let mut session = Self::default();
        for (index, name) in input_names.iter().enumerate() {
            let id = session.allocate_id();
            session.channels.push(ChannelStrip::new(
                id,
                name.clone(),
                InputPatch::mono(index as u16),
            ));
        }
        session
    }

    pub fn allocate_id(&mut self) -> Id {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    pub fn channel(&self, id: Id) -> Option<&ChannelStrip> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn channel_mut(&mut self, id: Id) -> Option<&mut ChannelStrip> {
        self.channels.iter_mut().find(|c| c.id == id)
    }

    pub fn bus(&self, id: Id) -> Option<&BusStrip> {
        self.buses.iter().find(|b| b.id == id)
    }

    pub fn bus_mut(&mut self, id: Id) -> Option<&mut BusStrip> {
        self.buses.iter_mut().find(|b| b.id == id)
    }

    pub fn matrix(&self, id: Id) -> Option<&MatrixStrip> {
        self.matrices.iter().find(|m| m.id == id)
    }

    pub fn matrix_mut(&mut self, id: Id) -> Option<&mut MatrixStrip> {
        self.matrices.iter_mut().find(|m| m.id == id)
    }

    /// The strip settings shared by channels, buses, matrices and the master.
    pub fn strip(&self, target: StripRef) -> Option<&StripCore> {
        match target {
            StripRef::Channel(id) => self.channel(id).map(|c| &c.core),
            StripRef::Bus(id) => self.bus(id).map(|b| &b.core),
            StripRef::Matrix(id) => self.matrix(id).map(|m| &m.core),
            StripRef::Master => Some(&self.master.core),
        }
    }

    pub fn strip_mut(&mut self, target: StripRef) -> Option<&mut StripCore> {
        match target {
            StripRef::Channel(id) => self.channel_mut(id).map(|c| &mut c.core),
            StripRef::Bus(id) => self.bus_mut(id).map(|b| &mut b.core),
            StripRef::Matrix(id) => self.matrix_mut(id).map(|m| &mut m.core),
            StripRef::Master => Some(&mut self.master.core),
        }
    }

    /// Every strip with its settings: channels, buses, the master, then the
    /// matrices, each in the order shown.
    pub fn strip_cores(&self) -> impl Iterator<Item = (StripRef, &StripCore)> {
        self.channels
            .iter()
            .map(|c| (StripRef::Channel(c.id), &c.core))
            .chain(self.buses.iter().map(|b| (StripRef::Bus(b.id), &b.core)))
            .chain(std::iter::once((StripRef::Master, &self.master.core)))
            .chain(
                self.matrices
                    .iter()
                    .map(|m| (StripRef::Matrix(m.id), &m.core)),
            )
    }

    /// Whether any channel, bus or matrix is soloed.
    pub fn any_solo(&self) -> bool {
        self.input_solo() || self.matrices.iter().any(|m| m.core.solo)
    }

    /// Whether any channel or bus is soloed: what solo in place acts on. A
    /// matrix's solo is always a cue (AFL in solo-in-place mode).
    pub fn input_solo(&self) -> bool {
        self.channels.iter().any(|c| c.core.solo) || self.buses.iter().any(|b| b.core.solo)
    }

    /// How many channels, buses and matrices are soloed.
    pub fn solo_count(&self) -> usize {
        self.channels.iter().filter(|c| c.core.solo).count()
            + self.buses.iter().filter(|b| b.core.solo).count()
            + self.matrices.iter().filter(|m| m.core.solo).count()
    }

    /// Muted by its own mute, an assigned DCA's mute or an active mute group
    /// it belongs to.
    pub fn muted(&self, target: StripRef) -> bool {
        let Some(core) = self.strip(target) else {
            return true;
        };
        core.mute
            || core
                .dcas
                .iter()
                .any(|&i| self.dcas.get(i).is_some_and(|d| d.mute))
            || core
                .mute_groups
                .iter()
                .any(|&i| self.mute_groups.get(i).is_some_and(|g| g.active))
    }

    /// Silenced in the main mix by someone else's solo: only in
    /// [`SoloMode::Sip`]. A solo-safe strip never is, and a bus fed by a
    /// soloed (or solo-safe) channel stays open, or the solo would mute what
    /// it was meant to isolate.
    pub fn solo_silenced(&self, target: StripRef) -> bool {
        if self.monitor.solo_mode != SoloMode::Sip || !self.input_solo() {
            return false;
        }
        let Some(core) = self.strip(target) else {
            return true;
        };
        if core.solo || core.solo_safe {
            return false;
        }
        match target {
            // Fed by the master and buses after solo has acted on them.
            StripRef::Master | StripRef::Matrix(_) => false,
            StripRef::Channel(_) => true,
            StripRef::Bus(id) => !self.channels.iter().any(|c| {
                (c.core.solo || c.core.solo_safe)
                    && (c.output == StripOutput::Bus(id) || c.sends.iter().any(|s| s.bus == id))
            }),
        }
    }

    /// Whether `target` is heard in the main mix: not muted, and not
    /// silenced by someone else's solo.
    pub fn audible(&self, target: StripRef) -> bool {
        !self.muted(target) && !self.solo_silenced(target)
    }

    /// `target`'s effective level, dB: its fader plus every assigned DCA.
    /// `None` when the fader or any of those DCAs is at the bottom (silence).
    pub fn effective_level_db(&self, target: StripRef) -> Option<f32> {
        let core = self.strip(target)?;
        if core.fader_db <= MIN_FADER_DB {
            return None;
        }
        let mut level = core.fader_db;
        for &index in &core.dcas {
            let Some(dca) = self.dcas.get(index) else {
                continue;
            };
            if dca.level_db <= MIN_FADER_DB {
                return None;
            }
            level += dca.level_db;
        }
        Some(level)
    }

    /// Bring a session read from a file (or built by hand) into shape:
    /// exactly [`DCA_COUNT`] DCAs and [`MUTE_GROUP_COUNT`] mute groups,
    /// assignments that point at them (sorted, once each, none on the
    /// master), colours in the palette and processing values in range.
    pub fn normalize(&mut self) {
        self.dcas.truncate(DCA_COUNT);
        while self.dcas.len() < DCA_COUNT {
            self.dcas.push(Dca::numbered(self.dcas.len()));
        }
        self.mute_groups.truncate(MUTE_GROUP_COUNT);
        while self.mute_groups.len() < MUTE_GROUP_COUNT {
            self.mute_groups
                .push(MuteGroup::numbered(self.mute_groups.len()));
        }
        for dca in &mut self.dcas {
            dca.color = dca.color.filter(|c| *c < PALETTE_COLORS);
            if !dca.level_db.is_finite() {
                dca.level_db = 0.0;
            }
            dca.level_db = dca.level_db.clamp(MIN_FADER_DB, MAX_FADER_DB);
        }
        let tidy = |indices: &mut Vec<usize>, count: usize| {
            indices.retain(|i| *i < count);
            indices.sort_unstable();
            indices.dedup();
        };
        let cores = self
            .channels
            .iter_mut()
            .map(|c| &mut c.core)
            .chain(self.buses.iter_mut().map(|b| &mut b.core))
            .chain(self.matrices.iter_mut().map(|m| &mut m.core));
        for core in cores {
            tidy(&mut core.dcas, DCA_COUNT);
            tidy(&mut core.mute_groups, MUTE_GROUP_COUNT);
            core.color = core.color.filter(|c| *c < PALETTE_COLORS);
            clamp_processing(&mut core.processing);
        }
        for channel in &mut self.channels {
            for send in &mut channel.sends {
                send.level_db =
                    finite_or(send.level_db, MIN_FADER_DB).clamp(MIN_FADER_DB, MAX_FADER_DB);
                send.pan = finite_or(send.pan, 0.0).clamp(-1.0, 1.0);
            }
        }
        for matrix in &mut self.matrices {
            // Not DCA-assignable.
            matrix.core.dcas.clear();
            for source in &mut matrix.sources {
                source.level_db =
                    finite_or(source.level_db, MIN_FADER_DB).clamp(MIN_FADER_DB, MAX_FADER_DB);
                source.pan = finite_or(source.pan, 0.0).clamp(-1.0, 1.0);
            }
        }
        let master = &mut self.master.core;
        master.dcas.clear();
        master.mute_groups.clear();
        master.solo = false;
        master.color = master.color.filter(|c| *c < PALETTE_COLORS);
        clamp_processing(&mut master.processing);
        self.monitor.level_db =
            finite_or(self.monitor.level_db, 0.0).clamp(MIN_FADER_DB, MAX_FADER_DB);
        let talkback = &mut self.talkback;
        talkback.level_db = finite_or(talkback.level_db, 0.0).clamp(MIN_FADER_DB, MAX_FADER_DB);
        let oscillator = &mut self.oscillator;
        oscillator.hz =
            finite_or(oscillator.hz, 1000.0).clamp(OSCILLATOR_MIN_HZ, OSCILLATOR_MAX_HZ);
        oscillator.level_db =
            finite_or(oscillator.level_db, -20.0).clamp(OSCILLATOR_MIN_DB, OSCILLATOR_MAX_DB);
        self.layers.truncate(MAX_LAYERS);
        // A show opens on the live inputs, whatever it was saved with.
        self.playback.virtual_soundcheck = false;
        if let Some(current) = self.current_scene {
            if self.scene(current).is_none() {
                self.current_scene = None;
            }
        }
        self.prune_references();
    }

    /// Whether `target` names a strip of this session.
    pub fn has_strip(&self, target: StripRef) -> bool {
        self.strip(target).is_some()
    }

    /// Drop whatever points at a strip that is not there (any more): matrix
    /// sources from a bus that is gone (and doubles), talkback and
    /// oscillator destinations, layer members; a monitor source that is
    /// gone goes back to the master. Cheap; run after strips are removed.
    pub fn prune_references(&mut self) {
        let buses: Vec<Id> = self.buses.iter().map(|b| b.id).collect();
        let matrices: Vec<Id> = self.matrices.iter().map(|m| m.id).collect();
        for matrix in &mut self.matrices {
            let mut seen: Vec<MatrixFeed> = Vec::new();
            matrix.sources.retain(|s| {
                let keep = match s.source {
                    MatrixFeed::Master => true,
                    MatrixFeed::Bus(id) => buses.contains(&id),
                } && !seen.contains(&s.source);
                seen.push(s.source);
                keep
            });
        }
        let dest_exists = |dest: &InjectDest| match dest {
            InjectDest::Bus(id) => buses.contains(id),
            InjectDest::Matrix(id) => matrices.contains(id),
            InjectDest::Master | InjectDest::Monitor => true,
        };
        for to in [&mut self.talkback.to, &mut self.oscillator.to] {
            let mut seen: Vec<InjectDest> = Vec::new();
            to.retain(|dest| {
                let keep = dest_exists(dest) && !seen.contains(dest);
                seen.push(*dest);
                keep
            });
        }
        let source_exists = match self.monitor.source {
            MonitorSource::Master => true,
            MonitorSource::Bus(id) => buses.contains(&id),
            MonitorSource::Matrix(id) => matrices.contains(&id),
        };
        if !source_exists {
            self.monitor.source = MonitorSource::Master;
        }
        let mut layers = std::mem::take(&mut self.layers);
        for layer in &mut layers {
            let mut seen: Vec<StripRef> = Vec::new();
            layer.strips.retain(|strip| {
                let keep = self.has_strip(*strip) && !seen.contains(strip);
                seen.push(*strip);
                keep
            });
        }
        self.layers = layers;
        // Playback files: to channels that exist, one file per channel.
        let channels: Vec<Id> = self.channels.iter().map(|c| c.id).collect();
        let mut fed: Vec<Id> = Vec::new();
        for track in &mut self.playback.tracks {
            if let Some(id) = track.channel {
                if !channels.contains(&id) || fed.contains(&id) {
                    track.channel = None;
                } else {
                    fed.push(id);
                }
            }
        }
    }

    /// A copy of the mix: what a scene stores.
    pub fn mix_state(&self) -> MixState {
        MixState {
            channels: self.channels.clone(),
            buses: self.buses.clone(),
            master: self.master.clone(),
            matrices: self.matrices.clone(),
            outputs: self.outputs.clone(),
            dcas: self.dcas.clone(),
            mute_groups: self.mute_groups.clone(),
        }
    }

    /// Replace the mix wholesale (plain data: the engine keeps its own state
    /// consistent around this).
    pub fn set_mix(&mut self, mix: MixState) {
        self.channels = mix.channels;
        self.buses = mix.buses;
        self.master = mix.master;
        self.matrices = mix.matrices;
        self.outputs = mix.outputs;
        self.dcas = mix.dcas;
        self.mute_groups = mix.mute_groups;
    }

    /// Whether the mix equals `mix`, without copying it.
    pub fn mix_equals(&self, mix: &MixState) -> bool {
        self.channels == mix.channels
            && self.buses == mix.buses
            && self.master == mix.master
            && self.matrices == mix.matrices
            && self.outputs == mix.outputs
            && self.dcas == mix.dcas
            && self.mute_groups == mix.mute_groups
    }

    pub fn scene(&self, id: Id) -> Option<&Scene> {
        self.scenes.iter().find(|s| s.id == id)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut session: Self =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        session.normalize();
        Ok(session)
    }

    /// Write the session to `path` atomically: see [`write_atomic`].
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        write_atomic(path, &text)
    }
}

/// `value`, or `default` when it is not a number.
fn finite_or(value: f32, default: f32) -> f32 {
    if value.is_finite() { value } else { default }
}

/// `path` with `suffix` appended to its file name: `show.json` → `show.json.bak`.
fn with_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Write `bytes` to `path` so that `path` is always either the old file or
/// the new one, never half of one: the bytes go to `path.tmp`, are flushed
/// to the disk, and the finished file is renamed over `path`. The previous
/// file is kept as `path.bak`.
///
/// The previous file is *copied* to `.bak` (not renamed), so `path` exists
/// at every moment. Renaming over an existing file replaces it on Windows
/// (`MoveFileEx` with replace) and on POSIX; a file system that refuses
/// (some network shares) gets the old file removed first, with the complete
/// new one already on disk beside it.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let at = |p: &std::path::Path, error: std::io::Error| format!("{}: {error}", p.display());
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = with_suffix(path, ".tmp");
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(at(&tmp, error));
    }
    if path.is_file() {
        let bak = with_suffix(path, ".bak");
        let copied = std::fs::copy(path, &bak)
            .and_then(|_| std::fs::OpenOptions::new().write(true).open(&bak))
            .and_then(|file| file.sync_all());
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&tmp);
            return Err(at(&bak, error));
        }
    }
    if let Err(first) = std::fs::rename(&tmp, path) {
        let retried = if path.exists() {
            std::fs::remove_file(path).and_then(|()| std::fs::rename(&tmp, path))
        } else {
            Err(first)
        };
        if let Err(error) = retried {
            let _ = std::fs::remove_file(&tmp);
            return Err(at(path, error));
        }
    }
    // The rename itself reaches the disk with the directory.
    #[cfg(unix)]
    if let Some(dir) = parent
        .map_or_else(|| std::fs::File::open("."), std::fs::File::open)
        .ok()
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Which strip a command addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum StripRef {
    Channel(Id),
    Bus(Id),
    Master,
    Matrix(Id),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// cpal host name ("WASAPI", "ALSA", …); `None` is the platform default.
    pub host: Option<String>,
    /// Device names; `None` is the host's default device.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// `0` is the output device's own rate.
    pub sample_rate: u32,
    /// Frames per callback; `0` lets the device choose.
    pub buffer_frames: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            host: None,
            input_device: None,
            output_device: None,
            sample_rate: 0,
            buffer_frames: 256,
        }
    }
}

/// Fader, pan, mute, solo, processing and inserts: what every strip has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripCore {
    /// Fader level, dB. [`MIN_FADER_DB`] and below is silence.
    pub fader_db: f32,
    /// −1 (left) … +1 (right). A balance on a stereo source, a constant-power
    /// pan on a mono one.
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// The strip's own HPF, gate, EQ, compressor and delay, before the
    /// inserts.
    pub processing: Processing,
    pub inserts: Vec<InsertSlot>,
    /// Palette index (`0 .. PALETTE_COLORS`); `None` is the default colour.
    pub color: Option<u8>,
    /// The DCAs this strip follows (indices into [`Session::dcas`]).
    /// Channels and buses only.
    pub dcas: Vec<usize>,
    /// The mute groups this strip belongs to (indices into
    /// [`Session::mute_groups`]). Channels and buses only.
    pub mute_groups: Vec<usize>,
    /// Never silenced by someone else's solo in place.
    pub solo_safe: bool,
    /// A scene recall leaves this strip entirely alone.
    pub recall_safe: bool,
}

impl Default for StripCore {
    fn default() -> Self {
        Self {
            fader_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            processing: Processing::default(),
            inserts: Vec::new(),
            color: None,
            dcas: Vec::new(),
            mute_groups: Vec::new(),
            solo_safe: false,
            recall_safe: false,
        }
    }
}

/// `processing` with every value inside the range its field documents
/// (non-finite values return to the default). The DSP takes any value; this
/// keeps what is saved and shown honest.
pub fn clamp_processing(processing: &mut Processing) {
    let defaults = Processing::default();
    let fit = |value: &mut f32, default: f32, min: f32, max: f32| {
        if !value.is_finite() {
            *value = default;
        }
        *value = value.clamp(min, max);
    };
    let p = processing;
    fit(&mut p.hpf.hz, defaults.hpf.hz, 20.0, 600.0);
    p.hpf.slope_db = match p.hpf.slope_db {
        0..=14 => 12,
        15..=20 => 18,
        _ => 24,
    };
    let g = &defaults.gate;
    fit(&mut p.gate.threshold_db, g.threshold_db, -80.0, 0.0);
    fit(&mut p.gate.range_db, g.range_db, -80.0, 0.0);
    fit(&mut p.gate.attack_ms, g.attack_ms, 0.05, 100.0);
    fit(&mut p.gate.hold_ms, g.hold_ms, 0.0, 2000.0);
    fit(&mut p.gate.release_ms, g.release_ms, 5.0, 4000.0);
    for i in 0..EQ_BANDS {
        let d = defaults.eq.bands[i];
        let band = &mut p.eq.bands[i];
        fit(&mut band.hz, d.hz, 20.0, 20_000.0);
        fit(&mut band.gain_db, d.gain_db, -18.0, 18.0);
        fit(&mut band.q, d.q, 0.1, 10.0);
    }
    let c = &defaults.comp;
    fit(&mut p.comp.threshold_db, c.threshold_db, -60.0, 0.0);
    fit(&mut p.comp.ratio, c.ratio, 1.0, 20.0);
    fit(&mut p.comp.attack_ms, c.attack_ms, 0.1, 200.0);
    fit(&mut p.comp.release_ms, c.release_ms, 10.0, 2000.0);
    fit(&mut p.comp.knee_db, c.knee_db, 0.0, 24.0);
    fit(&mut p.comp.makeup_db, c.makeup_db, 0.0, 24.0);
    fit(&mut p.delay.ms, defaults.delay.ms, 0.0, MAX_DELAY_MS);
}

/// Whether every number in `processing` is finite.
pub fn processing_is_finite(processing: &Processing) -> bool {
    let p = processing;
    let mut values = vec![
        p.hpf.hz,
        p.gate.threshold_db,
        p.gate.range_db,
        p.gate.attack_ms,
        p.gate.hold_ms,
        p.gate.release_ms,
        p.comp.threshold_db,
        p.comp.ratio,
        p.comp.attack_ms,
        p.comp.release_ms,
        p.comp.knee_db,
        p.comp.makeup_db,
        p.delay.ms,
    ];
    for band in &p.eq.bands {
        values.extend([band.hz, band.gain_db, band.q]);
    }
    values.iter().all(|v| v.is_finite())
}

/// The bottom of the fader: at or below it a strip is silent.
pub const MIN_FADER_DB: f32 = -90.0;
/// The top of the fader.
pub const MAX_FADER_DB: f32 = 10.0;

pub fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_FADER_DB {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelStrip {
    pub id: Id,
    pub name: String,
    pub input: InputPatch,
    /// Input gain before anything else, dB.
    pub trim_db: f32,
    pub phase_invert: bool,
    #[serde(flatten)]
    pub core: StripCore,
    pub sends: Vec<SendSlot>,
    pub output: StripOutput,
    /// Recorded when recording runs.
    pub record_arm: bool,
}

impl Default for ChannelStrip {
    fn default() -> Self {
        Self::new(0, String::new(), InputPatch::default())
    }
}

impl ChannelStrip {
    /// A new channel opens with its fader down: an input that reaches the
    /// speakers the moment it is patched can feed back before anyone has
    /// touched the mixer.
    pub fn new(id: Id, name: String, input: InputPatch) -> Self {
        Self {
            id,
            name,
            input,
            trim_db: 0.0,
            phase_invert: false,
            core: StripCore {
                fader_db: MIN_FADER_DB,
                ..StripCore::default()
            },
            sends: Vec::new(),
            output: StripOutput::Master,
            record_arm: false,
        }
    }

    pub fn is_stereo(&self) -> bool {
        self.input.right.is_some()
    }
}

/// Which interface inputs feed a channel (0-based). No left input is an
/// unpatched channel: silent until patched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InputPatch {
    pub left: Option<u16>,
    pub right: Option<u16>,
}

impl InputPatch {
    pub fn mono(channel: u16) -> Self {
        Self {
            left: Some(channel),
            right: None,
        }
    }

    pub fn stereo(left: u16, right: u16) -> Self {
        Self {
            left: Some(left),
            right: Some(right),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum StripOutput {
    #[default]
    Master,
    Bus(Id),
    /// Feeds nothing: reaches the interface only through the output patch
    /// (a direct out) or not at all.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendSlot {
    pub bus: Id,
    pub level_db: f32,
    /// Taken before the fader (a monitor mix) rather than after it (an effect
    /// send).
    pub pre_fader: bool,
    /// −1 … +1 into a stereo bus, when not [`Self::pan_follow`]: constant
    /// power for a mono channel, balance for a stereo one. A mono bus
    /// ignores it.
    pub pan: f32,
    /// Follow the channel instead: a post-fader send is taken after the
    /// channel's pan, a pre-fader send before it (unpanned) — as sends
    /// always were.
    pub pan_follow: bool,
}

impl Default for SendSlot {
    fn default() -> Self {
        Self {
            bus: 0,
            level_db: MIN_FADER_DB,
            pre_fader: false,
            pan: 0.0,
            pan_follow: true,
        }
    }
}

/// What a bus is for. It sets the defaults of new sends and the bus's
/// output, and how the UI groups it; the signal flow is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusRole {
    /// A monitor mix: new sends pre-fader; added with no output (patched to
    /// a wedge or IEM).
    Aux,
    /// A subgroup: channels routed to it (or sent); feeds the master.
    #[default]
    Group,
    /// An effect send and its return in one strip: new sends post-fader;
    /// feeds the master.
    Fx,
}

impl BusRole {
    /// Whether a new send to a bus of this role is pre-fader.
    pub fn sends_pre_fader(self) -> bool {
        self == Self::Aux
    }

    /// The output a new bus of this role gets.
    pub fn default_output(self) -> StripOutput {
        match self {
            Self::Aux => StripOutput::None,
            Self::Group | Self::Fx => StripOutput::Master,
        }
    }
}

/// An aux, group or FX bus. Feeds the master, or only its own output patch
/// (a monitor mix).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "BusStripFile")]
pub struct BusStrip {
    pub id: Id,
    pub name: String,
    /// A show from before roles reads `group` for a bus feeding the master,
    /// else `aux`.
    pub role: BusRole,
    /// `false`: a mono bus — its sum is (L+R)/2 on both sides, and sends to
    /// it have no pan.
    pub stereo: bool,
    #[serde(flatten)]
    pub core: StripCore,
    pub output: StripOutput,
    pub record_arm: bool,
}

impl Default for BusStrip {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            role: BusRole::Group,
            stereo: true,
            core: StripCore::default(),
            output: StripOutput::Master,
            record_arm: false,
        }
    }
}

/// A [`BusStrip`] as a file has it: the role may be missing (a show from
/// before roles), and is then read from the bus's output.
#[derive(Deserialize)]
#[serde(default)]
struct BusStripFile {
    id: Id,
    name: String,
    role: Option<BusRole>,
    stereo: bool,
    #[serde(flatten)]
    core: StripCore,
    output: StripOutput,
    record_arm: bool,
}

impl Default for BusStripFile {
    fn default() -> Self {
        let bus = BusStrip::default();
        Self {
            id: bus.id,
            name: bus.name,
            role: None,
            stereo: bus.stereo,
            core: bus.core,
            output: bus.output,
            record_arm: bus.record_arm,
        }
    }
}

impl From<BusStripFile> for BusStrip {
    fn from(file: BusStripFile) -> Self {
        let role = file.role.unwrap_or(if file.output == StripOutput::Master {
            BusRole::Group
        } else {
            BusRole::Aux
        });
        Self {
            id: file.id,
            name: file.name,
            role,
            stereo: file.stereo,
            core: file.core,
            output: file.output,
            record_arm: file.record_arm,
        }
    }
}

/// What a matrix sums: the master or a bus, after its fader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum MatrixFeed {
    Master,
    Bus(Id),
}

/// One contribution to a matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MatrixSource {
    pub source: MatrixFeed,
    pub level_db: f32,
    /// Balance into a stereo matrix, −1 … +1; a mono matrix ignores it.
    pub pan: f32,
}

impl Default for MatrixSource {
    fn default() -> Self {
        Self {
            source: MatrixFeed::Master,
            level_db: MIN_FADER_DB,
            pan: 0.0,
        }
    }
}

/// A matrix: the master and buses after their faders, each at its own
/// level, → processing → inserts → fader → its output patch. Feeds nothing
/// else. A delay or fill zone, a record feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MatrixStrip {
    pub id: Id,
    pub name: String,
    /// `false`: a mono matrix — its sum is (L+R)/2 on both sides.
    pub stereo: bool,
    #[serde(flatten)]
    pub core: StripCore,
    pub sources: Vec<MatrixSource>,
    pub record_arm: bool,
}

impl Default for MatrixStrip {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            stereo: true,
            core: StripCore::default(),
            sources: Vec::new(),
            record_arm: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MasterStrip {
    #[serde(flatten)]
    pub core: StripCore,
    pub record_arm: bool,
}

/// One effect in a strip's rack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InsertSlot {
    pub id: Id,
    #[serde(default)]
    pub bypass: bool,
    pub plugin: InsertPlugin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InsertPlugin {
    /// A Futureboard built-in, run inside the engine. `params` are its wire
    /// values by index: what was changed from the defaults, replayed on load.
    Builtin {
        stem: String,
        #[serde(default, with = "param_pairs")]
        params: BTreeMap<u32, f32>,
    },
    /// A third-party plug-in, run in the plug-in host process.
    External {
        /// "VST3", "VST2", "CLAP" or "AU".
        format: String,
        path: String,
        class_id: String,
        name: String,
        /// Opaque saved state, base64 (component, controller).
        #[serde(default)]
        state: Option<(String, String)>,
    },
}

impl InsertPlugin {
    pub fn display_name(&self) -> String {
        match self {
            Self::Builtin { stem, .. } => crate::builtin_fx::display_name(stem)
                .unwrap_or(stem.as_str())
                .to_string(),
            Self::External { name, .. } => name.clone(),
        }
    }
}

/// Wire parameters as `[[index, value], …]`. A map with integer keys does
/// not survive the flattened strip structs (serde turns its keys into
/// strings there), and a list reads as plainly in the file.
mod param_pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(map: &BTreeMap<u32, f32>, s: S) -> Result<S::Ok, S::Error> {
        map.iter().collect::<Vec<_>>().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<u32, f32>, D::Error> {
        Ok(Vec::<(u32, f32)>::deserialize(d)?.into_iter().collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputPatch {
    pub source: PatchSource,
    /// Interface output channels, 0-based. No right output sums the source
    /// to mono on the left one.
    pub left: u16,
    pub right: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum PatchSource {
    Master,
    Bus(Id),
    /// A channel's post-fader signal: a direct out.
    Channel(Id),
    /// The monitor bus: solo (PFL/AFL), or the monitor source when nothing
    /// is soloed, at the monitor level.
    Monitor,
    /// A matrix after its fader.
    Matrix(Id),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordFormat {
    #[default]
    Wav,
    Flac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordTap {
    /// The channel as it arrives, after trim: a clean multitrack to mix later.
    #[default]
    Input,
    /// After the inserts, before the fader: what the rack made of it.
    PostInserts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordSettings {
    /// Each take goes into its own dated folder in here. `None` is
    /// `<Music>/LiveStage`.
    pub folder: Option<PathBuf>,
    pub format: RecordFormat,
    /// 16, 24 or 32 (float).
    pub bit_depth: u16,
    pub tap: RecordTap,
}

impl Default for RecordSettings {
    fn default() -> Self {
        Self {
            folder: None,
            format: RecordFormat::Wav,
            bit_depth: 24,
            tap: RecordTap::Input,
        }
    }
}

impl RecordSettings {
    pub fn resolved_folder(&self) -> PathBuf {
        self.folder.clone().unwrap_or_else(default_record_folder)
    }
}

fn default_record_folder() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let music = base.join("Music");
    if music.is_dir() {
        music.join("LiveStage")
    } else {
        base.join("LiveStage")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_round_trips_through_json() {
        let mut session = Session::with_inputs(&["Kick".to_string(), "Snare".to_string()]);
        let bus = session.allocate_id();
        session.buses.push(BusStrip {
            id: bus,
            name: "Reverb".to_string(),
            ..BusStrip::default()
        });
        let insert = session.allocate_id();
        let kick = session.channels[0].id;
        let channel = session.channel_mut(kick).unwrap();
        channel.sends.push(SendSlot {
            bus,
            level_db: -6.0,
            pre_fader: false,
            ..SendSlot::default()
        });
        channel.core.inserts.push(InsertSlot {
            id: insert,
            bypass: false,
            plugin: InsertPlugin::Builtin {
                stem: "fa76".to_string(),
                params: BTreeMap::from([(2, 0.5)]),
            },
        });
        session.outputs.push(OutputPatch {
            source: PatchSource::Bus(bus),
            left: 2,
            right: Some(3),
        });

        let text = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(back, session);
    }

    #[test]
    fn solo_in_place_isolates_the_channel_and_keeps_the_bus_it_feeds() {
        let mut session = Session::with_inputs(&["A".to_string(), "B".to_string()]);
        session.monitor.solo_mode = SoloMode::Sip;
        let bus = session.allocate_id();
        session.buses.push(BusStrip {
            id: bus,
            ..BusStrip::default()
        });
        let (a, b) = (session.channels[0].id, session.channels[1].id);
        session.channel_mut(a).unwrap().output = StripOutput::Bus(bus);
        assert!(session.audible(StripRef::Channel(b)));

        session.channel_mut(a).unwrap().core.solo = true;
        assert!(session.audible(StripRef::Channel(a)));
        assert!(!session.audible(StripRef::Channel(b)));
        assert!(session.audible(StripRef::Bus(bus)));
        assert!(session.audible(StripRef::Master));

        // Solo safe: never silenced.
        session.channel_mut(b).unwrap().core.solo_safe = true;
        assert!(session.audible(StripRef::Channel(b)));

        // Pre-fader listen leaves the main mix alone.
        session.monitor.solo_mode = SoloMode::Pfl;
        session.channel_mut(b).unwrap().core.solo_safe = false;
        assert!(session.audible(StripRef::Channel(b)));
    }

    /// A show file from before the console core (no processing, colours,
    /// DCAs, mute groups or monitor) loads with their defaults.
    #[test]
    fn an_old_show_file_loads_with_the_new_defaults() {
        let old = r#"{
            "name": "Old show",
            "channels": [{
                "id": 1, "name": "Kick", "input": {"left": 0, "right": null},
                "trim_db": 3.0, "phase_invert": false,
                "fader_db": -6.0, "pan": 0.0, "mute": false, "solo": true,
                "inserts": [], "sends": [], "output": {"kind": "master"},
                "record_arm": false
            }],
            "buses": [{"id": 2, "name": "Verb", "fader_db": 0.0, "pan": 0.0,
                       "mute": false, "solo": false, "inserts": [],
                       "output": {"kind": "master"}, "record_arm": false}],
            "master": {"fader_db": 0.0, "pan": 0.0, "mute": false, "solo": false,
                       "inserts": [], "record_arm": false},
            "outputs": [{"source": {"kind": "master"}, "left": 0, "right": 1}],
            "next_id": 3
        }"#;
        let path = std::env::temp_dir().join(format!("livestage-old-{}.json", std::process::id()));
        std::fs::write(&path, old).unwrap();
        let session = Session::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let kick = &session.channels[0];
        assert_eq!(kick.trim_db, 3.0);
        assert_eq!(kick.core.fader_db, -6.0);
        assert!(kick.core.solo);
        assert_eq!(kick.core.processing, Processing::default());
        assert_eq!(kick.core.color, None);
        assert!(kick.core.dcas.is_empty() && kick.core.mute_groups.is_empty());
        assert!(!kick.core.solo_safe);
        assert_eq!(session.dcas.len(), DCA_COUNT);
        assert_eq!(session.dcas[7].name, "DCA 8");
        assert_eq!(session.mute_groups.len(), MUTE_GROUP_COUNT);
        assert_eq!(session.mute_groups[0].name, "Mute 1");
        assert_eq!(session.monitor, MonitorSettings::default());
        assert_eq!(session.monitor.solo_mode, SoloMode::Pfl);
    }

    /// The new fields survive a save and load, and `monitor` is a patch
    /// source spelled `{"kind":"monitor"}`.
    #[test]
    fn console_fields_round_trip() {
        let mut session = Session::with_inputs(&["Vox".to_string()]);
        let core = &mut session.channels[0].core;
        core.processing.comp.on = true;
        core.processing.comp.ratio = 4.0;
        core.processing.eq.bands[1].kind = crate::processing::EqKind::HighShelf;
        core.color = Some(5);
        core.dcas = vec![0, 3];
        core.mute_groups = vec![1];
        core.solo_safe = true;
        session.dcas[3].level_db = -6.0;
        session.dcas[3].color = Some(11);
        session.mute_groups[1].active = true;
        session.monitor = MonitorSettings {
            solo_mode: SoloMode::Afl,
            level_db: -3.0,
            dim: true,
            source: MonitorSource::Master,
        };
        session.outputs.push(OutputPatch {
            source: PatchSource::Monitor,
            left: 2,
            right: Some(3),
        });
        let text = serde_json::to_string(&session).unwrap();
        assert!(text.contains(r#"{"kind":"monitor"}"#), "{text}");
        assert!(text.contains(r#""solo_mode":"afl""#), "{text}");
        let back: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(back, session);
    }

    /// A show from before scenes loads with none, recall safe off and every
    /// recall scope on.
    #[test]
    fn a_show_without_scenes_loads() {
        let old = r#"{
            "name": "Phase 1 show",
            "channels": [{"id": 1, "name": "Kick", "input": {"left": 0, "right": null},
                          "fader_db": -6.0, "output": {"kind": "master"}}],
            "next_id": 2
        }"#;
        let session: Session = serde_json::from_str(old).unwrap();
        assert!(session.scenes.is_empty());
        assert_eq!(session.current_scene, None);
        assert!(!session.channels[0].core.recall_safe);
        let scene: Scene = serde_json::from_str(r#"{"id": 3}"#).unwrap();
        assert_eq!(scene.scope, RecallScope::default());
        assert!(scene.scope.names && scene.scope.input);
        assert_eq!(scene.mix, MixState::default());
    }

    #[test]
    fn scenes_round_trip_and_a_stale_current_scene_is_dropped() {
        let mut session = Session::with_inputs(&["Vox".to_string()]);
        session.channels[0].core.recall_safe = true;
        let id = session.allocate_id();
        session.scenes.push(Scene {
            id,
            name: "Verse".to_string(),
            note: "Capo 2".to_string(),
            scope: RecallScope {
                faders: false,
                ..RecallScope::default()
            },
            mix: session.mix_state(),
        });
        session.current_scene = Some(id);
        let text = serde_json::to_string(&session).unwrap();
        let mut back: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(back, session);
        back.normalize();
        assert_eq!(back.current_scene, Some(id));
        back.scenes.clear();
        back.normalize();
        assert_eq!(back.current_scene, None);
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("livestage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saving_is_atomic_and_keeps_the_previous_file() {
        let dir = scratch_dir("atomic");
        let path = dir.join("show.json");
        let mut session = Session::with_inputs(&["A".to_string()]);
        session.save(&path).unwrap();
        assert!(
            !dir.join("show.json.bak").exists(),
            "nothing before the first save"
        );
        assert!(!dir.join("show.json.tmp").exists());
        let first = std::fs::read(&path).unwrap();

        session.name = "Second".to_string();
        session.save(&path).unwrap();
        assert_eq!(std::fs::read(dir.join("show.json.bak")).unwrap(), first);
        assert_eq!(Session::load(&path).unwrap().name, "Second");
        assert!(!dir.join("show.json.tmp").exists());

        // A save that cannot be written leaves the file whole: here the
        // temporary file's place is taken by a folder.
        std::fs::create_dir(dir.join("show.json.tmp")).unwrap();
        session.name = "Third".to_string();
        assert!(session.save(&path).is_err());
        assert_eq!(Session::load(&path).unwrap().name, "Second");
        assert_eq!(std::fs::read(dir.join("show.json.bak")).unwrap(), first);
        std::fs::remove_dir(dir.join("show.json.tmp")).unwrap();

        // A save into a folder that does not exist yet makes it.
        let nested = dir.join("a").join("b.json");
        session.save(&nested).unwrap();
        assert_eq!(Session::load(&nested).unwrap().name, "Third");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_settings_tell_absent_from_null() {
        let absent: StripSettings = serde_json::from_str(r#"{"fader_db": -3}"#).unwrap();
        assert_eq!(absent.color, None);
        assert_eq!(absent.fader_db, Some(-3.0));
        let null: StripSettings = serde_json::from_str(r#"{"color": null}"#).unwrap();
        assert_eq!(null.color, Some(None));
        let set: StripSettings = serde_json::from_str(r#"{"color": 4}"#).unwrap();
        assert_eq!(set.color, Some(Some(4)));
        for settings in [absent, null, set] {
            let text = serde_json::to_string(&settings).unwrap();
            let back: StripSettings = serde_json::from_str(&text).unwrap();
            assert_eq!(back, settings, "{text}");
        }
    }

    #[test]
    fn normalize_pads_truncates_and_tidies_assignments() {
        let mut session = Session::with_inputs(&["A".to_string()]);
        session.dcas.truncate(3);
        session.mute_groups.extend((0..4).map(MuteGroup::numbered));
        session.channels[0].core.dcas = vec![9, 2, 2, 0];
        session.channels[0].core.mute_groups = vec![8, 7];
        session.channels[0].core.color = Some(12);
        session.channels[0].core.processing.hpf.hz = 5.0;
        session.channels[0].core.processing.hpf.slope_db = 17;
        session.master.core.dcas = vec![1];
        session.normalize();
        assert_eq!(session.dcas.len(), DCA_COUNT);
        assert_eq!(session.dcas[5].name, "DCA 6");
        assert_eq!(session.mute_groups.len(), MUTE_GROUP_COUNT);
        let core = &session.channels[0].core;
        assert_eq!(core.dcas, vec![0, 2]);
        assert_eq!(core.mute_groups, vec![7]);
        assert_eq!(core.color, None);
        assert_eq!(core.processing.hpf.hz, 20.0);
        assert_eq!(core.processing.hpf.slope_db, 18);
        assert!(session.master.core.dcas.is_empty());
    }

    #[test]
    fn dcas_add_to_the_fader_and_mute_their_members() {
        let mut session = Session::with_inputs(&["A".to_string()]);
        let a = StripRef::Channel(session.channels[0].id);
        session.strip_mut(a).unwrap().fader_db = -6.0;
        session.strip_mut(a).unwrap().dcas = vec![1, 4];
        session.dcas[1].level_db = -3.0;
        session.dcas[4].level_db = 2.0;
        assert_eq!(session.effective_level_db(a), Some(-7.0));
        session.dcas[4].level_db = MIN_FADER_DB;
        assert_eq!(session.effective_level_db(a), None);
        session.dcas[4].level_db = 0.0;
        // An unassigned DCA does nothing.
        session.dcas[0].mute = true;
        assert!(!session.muted(a));
        session.dcas[1].mute = true;
        assert!(session.muted(a));
        session.dcas[1].mute = false;
        session.strip_mut(a).unwrap().mute_groups = vec![2];
        session.mute_groups[3].active = true;
        assert!(!session.muted(a));
        session.mute_groups[2].active = true;
        assert!(session.muted(a));
    }

    /// A show from before Phase 3: buses read their role from their output
    /// (group to the master, else aux), every bus is stereo, every send
    /// follows the channel's pan; no matrices, talkback, oscillator setup or
    /// layers; the monitor plays the master.
    #[test]
    fn a_show_without_bus_roles_or_matrices_loads_as_it_sounded() {
        let old = r#"{
            "name": "Phase 2 show",
            "channels": [{"id": 1, "name": "Kick", "input": {"left": 0, "right": null},
                          "fader_db": -6.0, "output": {"kind": "master"},
                          "sends": [{"bus": 2, "level_db": -10.0, "pre_fader": true},
                                    {"bus": 3, "level_db": -3.0, "pre_fader": false}]}],
            "buses": [{"id": 2, "name": "Wedge", "output": {"kind": "none"}},
                      {"id": 3, "name": "Verb", "output": {"kind": "master"}}],
            "monitor": {"solo_mode": "afl", "level_db": -3.0, "dim": false},
            "scenes": [{"id": 4, "name": "Verse", "mix": {
                "buses": [{"id": 2, "name": "Wedge", "output": {"kind": "none"}}]}}],
            "next_id": 5
        }"#;
        let path = std::env::temp_dir().join(format!("livestage-p2-{}.json", std::process::id()));
        std::fs::write(&path, old).unwrap();
        let session = Session::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let roles: Vec<(BusRole, bool)> =
            session.buses.iter().map(|b| (b.role, b.stereo)).collect();
        assert_eq!(roles, [(BusRole::Aux, true), (BusRole::Group, true)]);
        assert_eq!(session.scenes[0].mix.buses[0].role, BusRole::Aux);
        for send in &session.channels[0].sends {
            assert!(send.pan_follow && send.pan == 0.0);
        }
        assert_eq!(session.channels[0].sends[0].level_db, -10.0);
        assert!(session.matrices.is_empty() && session.layers.is_empty());
        assert_eq!(session.talkback, TalkbackSettings::default());
        assert!(session.talkback.hpf && session.talkback.input.is_none());
        assert_eq!(session.oscillator, OscillatorSettings::default());
        assert_eq!(session.monitor.source, MonitorSource::Master);
        assert_eq!(session.monitor.solo_mode, SoloMode::Afl);
        // A role given is kept, whatever the output.
        let fx: BusStrip =
            serde_json::from_str(r#"{"id": 9, "role": "fx", "output": {"kind": "none"}}"#).unwrap();
        assert_eq!(fx.role, BusRole::Fx);
        assert_eq!(BusStrip::default().role, BusRole::Group);
    }

    #[test]
    fn bus_and_monitor_fields_round_trip() {
        let mut session = Session::with_inputs(&["Vox".to_string()]);
        let bus = session.allocate_id();
        session.buses.push(BusStrip {
            id: bus,
            name: "Mon".to_string(),
            role: BusRole::Aux,
            stereo: false,
            output: StripOutput::None,
            ..BusStrip::default()
        });
        session.channels[0].sends.push(SendSlot {
            bus,
            level_db: -4.0,
            pre_fader: true,
            pan: -0.25,
            pan_follow: false,
        });
        let matrix = session.allocate_id();
        session.matrices.push(MatrixStrip {
            id: matrix,
            name: "Fill".to_string(),
            stereo: false,
            sources: vec![
                MatrixSource {
                    source: MatrixFeed::Master,
                    level_db: -6.0,
                    pan: 0.5,
                },
                MatrixSource {
                    source: MatrixFeed::Bus(bus),
                    level_db: -12.0,
                    pan: 0.0,
                },
            ],
            record_arm: true,
            ..MatrixStrip::default()
        });
        session.matrices[0].core.processing.delay.ms = 40.0;
        session.outputs.push(OutputPatch {
            source: PatchSource::Matrix(matrix),
            left: 4,
            right: None,
        });
        session.monitor.source = MonitorSource::Matrix(matrix);
        session.talkback = TalkbackSettings {
            input: Some(7),
            level_db: -3.0,
            hpf: false,
            to: vec![InjectDest::Bus(bus), InjectDest::Monitor],
        };
        session.oscillator = OscillatorSettings {
            kind: OscillatorKind::Pink,
            hz: 440.0,
            level_db: -30.0,
            to: vec![InjectDest::Matrix(matrix), InjectDest::Master],
        };
        session.layers.push(Layer {
            name: "Outs".to_string(),
            strips: vec![
                StripRef::Matrix(matrix),
                StripRef::Bus(bus),
                StripRef::Master,
            ],
        });
        let text = serde_json::to_string(&session).unwrap();
        for piece in [
            r#""role":"aux""#,
            r#""stereo":false"#,
            r#""pan_follow":false"#,
            &format!(r#"{{"kind":"matrix","id":{matrix}}}"#),
            r#""source":{"kind":"master"}"#,
            r#""kind":"pink""#,
            r#"{"kind":"monitor"}"#,
        ] {
            assert!(text.contains(piece), "{piece} in {text}");
        }
        let back: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(back, session);
        let mut normalized = back.clone();
        normalized.normalize();
        assert_eq!(normalized, session, "nothing to tidy");
    }

    #[test]
    fn normalize_drops_what_points_at_strips_that_are_gone() {
        let mut session = Session::with_inputs(&["A".to_string()]);
        let a = StripRef::Channel(session.channels[0].id);
        let matrix = session.allocate_id();
        session.matrices.push(MatrixStrip {
            id: matrix,
            sources: vec![
                MatrixSource {
                    source: MatrixFeed::Bus(77),
                    ..MatrixSource::default()
                },
                MatrixSource::default(),
                MatrixSource {
                    level_db: f32::NAN,
                    ..MatrixSource::default()
                },
            ],
            ..MatrixStrip::default()
        });
        session.matrices[0].core.dcas = vec![2];
        session.talkback.to = vec![
            InjectDest::Bus(77),
            InjectDest::Matrix(matrix),
            InjectDest::Matrix(matrix),
        ];
        session.oscillator.to = vec![InjectDest::Matrix(78), InjectDest::Monitor];
        session.oscillator.level_db = 6.0;
        session.oscillator.hz = 5.0;
        session.monitor.source = MonitorSource::Bus(77);
        session.layers = (0..10)
            .map(|i| Layer {
                name: format!("L{i}"),
                strips: vec![a, StripRef::Bus(77), a],
            })
            .collect();
        session.normalize();
        let sources = &session.matrices[0].sources;
        assert_eq!(sources.len(), 1, "the missing bus and the double go");
        assert_eq!(sources[0].source, MatrixFeed::Master);
        assert!(session.matrices[0].core.dcas.is_empty());
        assert_eq!(session.talkback.to, [InjectDest::Matrix(matrix)]);
        assert_eq!(session.oscillator.to, [InjectDest::Monitor]);
        assert_eq!(
            (session.oscillator.level_db, session.oscillator.hz),
            (OSCILLATOR_MAX_DB, OSCILLATOR_MIN_HZ)
        );
        assert_eq!(session.monitor.source, MonitorSource::Master);
        assert_eq!(session.layers.len(), MAX_LAYERS);
        assert_eq!(session.layers[0].strips, [a]);
    }

    #[test]
    fn a_matrix_is_a_strip_and_its_solo_is_a_cue() {
        let mut session = Session::with_inputs(&["A".to_string()]);
        session.monitor.solo_mode = SoloMode::Sip;
        let id = session.allocate_id();
        session.matrices.push(MatrixStrip {
            id,
            ..MatrixStrip::default()
        });
        let m = StripRef::Matrix(id);
        session.strip_mut(m).unwrap().solo = true;
        assert!(session.any_solo() && !session.input_solo());
        assert_eq!(session.solo_count(), 1);
        // A matrix solo silences nothing, even in solo in place.
        assert!(session.audible(StripRef::Channel(session.channels[0].id)));
        assert!(session.audible(m));
        let refs: Vec<StripRef> = session.strip_cores().map(|(s, _)| s).collect();
        assert_eq!(refs.last(), Some(&m));
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            format!(r#"{{"kind":"matrix","id":{id}}}"#)
        );
    }
}
