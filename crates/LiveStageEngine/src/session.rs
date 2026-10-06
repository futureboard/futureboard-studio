//! What a live setup *is*: the device, the strips, their inserts, the patch
//! and the recording settings. Plain data, saved as JSON, shared by the GUI
//! and the headless server. The realtime side never reads it: the engine
//! compiles it into a [`crate::graph::Graph`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Identifies a strip, a bus or an insert within one session. Stable across
/// save and load, so a patch or a send keeps pointing at the same thing.
pub type Id = u32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub name: String,
    pub audio: AudioSettings,
    pub channels: Vec<ChannelStrip>,
    pub buses: Vec<BusStrip>,
    pub master: MasterStrip,
    /// Where each mix leaves the interface. A source may be patched to any
    /// number of outputs; an output may take any number of sources (summed).
    pub outputs: Vec<OutputPatch>,
    pub recording: RecordSettings,
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
            outputs: vec![OutputPatch {
                source: PatchSource::Master,
                left: 0,
                right: Some(1),
            }],
            recording: RecordSettings::default(),
            next_id: 1,
        }
    }
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

    /// The strip settings shared by channels, buses and the master.
    pub fn strip(&self, target: StripRef) -> Option<&StripCore> {
        match target {
            StripRef::Channel(id) => self.channel(id).map(|c| &c.core),
            StripRef::Bus(id) => self.bus(id).map(|b| &b.core),
            StripRef::Master => Some(&self.master.core),
        }
    }

    pub fn strip_mut(&mut self, target: StripRef) -> Option<&mut StripCore> {
        match target {
            StripRef::Channel(id) => self.channel_mut(id).map(|c| &mut c.core),
            StripRef::Bus(id) => self.bus_mut(id).map(|b| &mut b.core),
            StripRef::Master => Some(&mut self.master.core),
        }
    }

    /// Whether any channel or bus is soloed. Solo-in-place: when one is,
    /// every strip that is not soloed (and not solo-safe) is silent.
    pub fn any_solo(&self) -> bool {
        self.channels.iter().any(|c| c.core.solo) || self.buses.iter().any(|b| b.core.solo)
    }

    /// Whether `target` is heard: not muted, and not silenced by someone
    /// else's solo. A bus fed by a soloed channel stays open, or the solo
    /// would mute what it was meant to isolate.
    pub fn audible(&self, target: StripRef) -> bool {
        let Some(core) = self.strip(target) else {
            return false;
        };
        if core.mute {
            return false;
        }
        if !self.any_solo() {
            return true;
        }
        match target {
            StripRef::Master => true,
            StripRef::Channel(_) => core.solo,
            StripRef::Bus(id) => {
                core.solo
                    || self.channels.iter().any(|c| {
                        c.core.solo
                            && (c.output == StripOutput::Bus(id)
                                || c.sends.iter().any(|s| s.bus == id))
                    })
            }
        }
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// Which strip a command addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum StripRef {
    Channel(Id),
    Bus(Id),
    Master,
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

/// Fader, pan, mute, solo and inserts: what every strip has.
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
    pub inserts: Vec<InsertSlot>,
}

impl Default for StripCore {
    fn default() -> Self {
        Self {
            fader_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            inserts: Vec::new(),
        }
    }
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
}

impl Default for SendSlot {
    fn default() -> Self {
        Self {
            bus: 0,
            level_db: MIN_FADER_DB,
            pre_fader: false,
        }
    }
}

/// An aux or group bus. Feeds the master, or only its own output patch (a
/// monitor mix).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusStrip {
    pub id: Id,
    pub name: String,
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
            core: StripCore::default(),
            output: StripOutput::Master,
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
    fn solo_isolates_the_channel_and_keeps_the_bus_it_feeds() {
        let mut session = Session::with_inputs(&["A".to_string(), "B".to_string()]);
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
    }
}
