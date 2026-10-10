//! Remote control by OSC and MIDI: the settings (`remote.json`), the
//! address space both speak, the OSC socket, the MIDI maps with learn, and
//! feedback (OSC subscribers, MIDI outputs for motor faders and LEDs).
//!
//! The OSC socket's thread and midir's callbacks only parse and push into
//! the command loop's queue ([`RemoteSender`], bounded: when full, the
//! newest event is dropped and counted). The command loop turns each event
//! into an ordinary JSON command, checks it with the engineer's permissions
//! and runs it like any other ([`Remote::input`]); feedback compares a map of
//! every address's value after the session moved ([`Remote::tick`]).
//!
//! Addresses (1-based positions, in the order the mixer shows them):
//!
//! ```txt
//! /ch/<n>/fader f 0..1   /ch/<n>/db f   /ch/<n>/mute i   /ch/<n>/pan f -1..1   /ch/<n>/solo i   /ch/<n>/name (read)
//! /bus/<n>/…  /mtx/<n>/…  (the same leaves)   /master/fader|db|mute|pan|name   /dca/<n>/fader|mute|name
//! /ch/<n>/send/<b>/fader|db|pan                (b: the bus's position)
//! /mtx/<n>/src/master/fader|db|pan   /mtx/<n>/src/bus/<b>/fader|db|pan
//! /scene/recall i   /scene/next   /scene/previous   /talk i   /oscillator/on i
//! /playback/play   /playback/stop   /vsc i
//! /livestage/subscribe   (feedback to the sender for 60 s: a full dump, then changes)
//! ```
//!
//! A leaf sent with no argument is a question: its value goes back to the
//! sender.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use livestage_engine::{MAX_FADER_DB, MIN_FADER_DB, MatrixFeed, Session, StripRef, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fader_law::{db_to_position, position_to_db};
use crate::midi::Midi;
use crate::osc::{self, OscArg, OscMessage, OscPacket};
use crate::web::Inbound;

/// The commands [`Remote::command`] answers.
pub const COMMANDS: &[&str] = &[
    "remote_settings",
    "set_remote",
    "midi_ports",
    "midi_learn",
    "midi_learn_cancel",
];
/// Remote events queued for the command loop at most.
pub const QUEUE: usize = 1024;
/// OSC subscribers at once.
pub const MAX_SUBSCRIBERS: usize = 8;
/// A subscription lasts this long unless renewed.
pub const LEASE: Duration = Duration::from_secs(60);
/// Feedback goes out at most this often.
const FEEDBACK_PERIOD: Duration = Duration::from_millis(50);
/// The largest feedback datagram.
const DATAGRAM: usize = 1200;
/// How long the OSC thread waits for a packet before it looks at its stop
/// flag.
const OSC_POLL: Duration = Duration::from_millis(200);

// ----- settings ---------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    pub osc: OscSettings,
    pub midi: MidiSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OscSettings {
    pub enabled: bool,
    pub port: u16,
    pub feedback: bool,
}

impl Default for OscSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8000,
            feedback: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiSettings {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub feedback: bool,
    pub maps: Vec<MidiMap>,
}

impl Default for MidiSettings {
    fn default() -> Self {
        Self {
            inputs: Vec::new(),
            outputs: Vec::new(),
            feedback: true,
            maps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MidiMap {
    pub midi: MidiKey,
    pub target: String,
    #[serde(default)]
    pub mode: MapMode,
}

/// A MIDI message as a map matches it: channel 1–16, CC/note/program
/// number 0–127.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MidiKey {
    pub channel: u8,
    pub kind: MidiKind,
    pub number: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MidiKind {
    Cc,
    Note,
    Pc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MapMode {
    #[default]
    Absolute,
    Toggle,
    Momentary,
}

/// Check settings from a client and put targets in their plain spelling.
/// One map per MIDI message: the last one given wins.
pub fn validate(mut settings: RemoteSettings) -> Result<RemoteSettings, String> {
    if settings.osc.port == 0 {
        return Err("the OSC port is 1 to 65535".to_string());
    }
    let mut maps: Vec<MidiMap> = Vec::with_capacity(settings.midi.maps.len());
    for mut map in settings.midi.maps {
        check_key(&map.midi)?;
        let target = parse_target(&map.target)
            .filter(Target::settable)
            .ok_or_else(|| format!("{} is not an address a MIDI control can set", map.target))?;
        map.target = target.address();
        maps.retain(|m| m.midi != map.midi);
        maps.push(map);
    }
    settings.midi.maps = maps;
    for list in [&mut settings.midi.inputs, &mut settings.midi.outputs] {
        let mut seen = Vec::new();
        list.retain(|name| {
            !name.is_empty() && !seen.contains(name) && {
                seen.push(name.clone());
                true
            }
        });
    }
    Ok(settings)
}

fn check_key(key: &MidiKey) -> Result<(), String> {
    if !(1..=16).contains(&key.channel) {
        return Err("a MIDI channel is 1 to 16".to_string());
    }
    if key.number > 127 {
        return Err("a MIDI number is 0 to 127".to_string());
    }
    Ok(())
}

// ----- the address space --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripPos {
    Channel(usize),
    Bus(usize),
    Matrix(usize),
    Master,
    Dca(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leaf {
    Fader,
    Db,
    Mute,
    Pan,
    Solo,
    Name,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendLeaf {
    Fader,
    Db,
    Pan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePos {
    Master,
    Bus(usize),
}

/// One address, positions 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Strip(StripPos, Leaf),
    Send(usize, usize, SendLeaf),
    MatrixSource(usize, SourcePos, SendLeaf),
    SceneRecall,
    SceneNext,
    ScenePrevious,
    Talk,
    OscillatorOn,
    PlaybackPlay,
    PlaybackStop,
    VirtualSoundcheck,
    Subscribe,
}

/// What kind of value an address carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 0..1, the fader law's travel.
    Position,
    Db,
    Pan,
    Switch,
    /// Acts on a press; no value.
    Trigger,
    /// A scene position.
    Recall,
    Text,
    Subscribe,
}

pub fn parse_target(address: &str) -> Option<Target> {
    let parts: Vec<&str> = address.strip_prefix('/')?.split('/').collect();
    let pos = |text: &str| -> Option<usize> {
        let n: usize = text.parse().ok()?;
        (1..=4096).contains(&n).then(|| n - 1)
    };
    let leaf = |text: &str| -> Option<Leaf> {
        Some(match text {
            "fader" => Leaf::Fader,
            "db" => Leaf::Db,
            "mute" => Leaf::Mute,
            "pan" => Leaf::Pan,
            "solo" => Leaf::Solo,
            "name" => Leaf::Name,
            _ => return None,
        })
    };
    let send_leaf = |text: &str| -> Option<SendLeaf> {
        Some(match text {
            "fader" => SendLeaf::Fader,
            "db" => SendLeaf::Db,
            "pan" => SendLeaf::Pan,
            _ => return None,
        })
    };
    Some(match parts.as_slice() {
        ["ch", n, l] => Target::Strip(StripPos::Channel(pos(n)?), leaf(l)?),
        ["bus", n, l] => Target::Strip(StripPos::Bus(pos(n)?), leaf(l)?),
        ["mtx", n, l] => Target::Strip(StripPos::Matrix(pos(n)?), leaf(l)?),
        ["master", l] => match leaf(l)? {
            Leaf::Solo => return None,
            l => Target::Strip(StripPos::Master, l),
        },
        ["dca", n, l] => {
            let n = pos(n)?;
            if n >= livestage_engine::DCA_COUNT {
                return None;
            }
            match leaf(l)? {
                l @ (Leaf::Fader | Leaf::Mute | Leaf::Name) => Target::Strip(StripPos::Dca(n), l),
                _ => return None,
            }
        }
        ["ch", n, "send", b, l] => Target::Send(pos(n)?, pos(b)?, send_leaf(l)?),
        ["mtx", n, "src", "master", l] => {
            Target::MatrixSource(pos(n)?, SourcePos::Master, send_leaf(l)?)
        }
        ["mtx", n, "src", "bus", b, l] => {
            Target::MatrixSource(pos(n)?, SourcePos::Bus(pos(b)?), send_leaf(l)?)
        }
        ["scene", "recall"] => Target::SceneRecall,
        ["scene", "next"] => Target::SceneNext,
        ["scene", "previous"] => Target::ScenePrevious,
        ["talk"] => Target::Talk,
        ["oscillator", "on"] => Target::OscillatorOn,
        ["playback", "play"] => Target::PlaybackPlay,
        ["playback", "stop"] => Target::PlaybackStop,
        ["vsc"] => Target::VirtualSoundcheck,
        ["livestage", "subscribe"] => Target::Subscribe,
        _ => return None,
    })
}

impl Target {
    /// The address in its plain spelling.
    pub fn address(&self) -> String {
        let strip = |pos: &StripPos| match pos {
            StripPos::Channel(n) => format!("/ch/{}", n + 1),
            StripPos::Bus(n) => format!("/bus/{}", n + 1),
            StripPos::Matrix(n) => format!("/mtx/{}", n + 1),
            StripPos::Master => "/master".to_string(),
            StripPos::Dca(n) => format!("/dca/{}", n + 1),
        };
        let leaf = |l: &Leaf| match l {
            Leaf::Fader => "fader",
            Leaf::Db => "db",
            Leaf::Mute => "mute",
            Leaf::Pan => "pan",
            Leaf::Solo => "solo",
            Leaf::Name => "name",
        };
        let send_leaf = |l: &SendLeaf| match l {
            SendLeaf::Fader => "fader",
            SendLeaf::Db => "db",
            SendLeaf::Pan => "pan",
        };
        match self {
            Target::Strip(pos, l) => format!("{}/{}", strip(pos), leaf(l)),
            Target::Send(c, b, l) => format!("/ch/{}/send/{}/{}", c + 1, b + 1, send_leaf(l)),
            Target::MatrixSource(m, SourcePos::Master, l) => {
                format!("/mtx/{}/src/master/{}", m + 1, send_leaf(l))
            }
            Target::MatrixSource(m, SourcePos::Bus(b), l) => {
                format!("/mtx/{}/src/bus/{}/{}", m + 1, b + 1, send_leaf(l))
            }
            Target::SceneRecall => "/scene/recall".to_string(),
            Target::SceneNext => "/scene/next".to_string(),
            Target::ScenePrevious => "/scene/previous".to_string(),
            Target::Talk => "/talk".to_string(),
            Target::OscillatorOn => "/oscillator/on".to_string(),
            Target::PlaybackPlay => "/playback/play".to_string(),
            Target::PlaybackStop => "/playback/stop".to_string(),
            Target::VirtualSoundcheck => "/vsc".to_string(),
            Target::Subscribe => "/livestage/subscribe".to_string(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Target::Strip(_, Leaf::Fader)
            | Target::Send(_, _, SendLeaf::Fader)
            | Target::MatrixSource(_, _, SendLeaf::Fader) => Kind::Position,
            Target::Strip(_, Leaf::Db)
            | Target::Send(_, _, SendLeaf::Db)
            | Target::MatrixSource(_, _, SendLeaf::Db) => Kind::Db,
            Target::Strip(_, Leaf::Pan)
            | Target::Send(_, _, SendLeaf::Pan)
            | Target::MatrixSource(_, _, SendLeaf::Pan) => Kind::Pan,
            Target::Strip(_, Leaf::Mute | Leaf::Solo)
            | Target::Talk
            | Target::OscillatorOn
            | Target::VirtualSoundcheck => Kind::Switch,
            Target::Strip(_, Leaf::Name) => Kind::Text,
            Target::SceneNext
            | Target::ScenePrevious
            | Target::PlaybackPlay
            | Target::PlaybackStop => Kind::Trigger,
            Target::SceneRecall => Kind::Recall,
            Target::Subscribe => Kind::Subscribe,
        }
    }

    /// Whether a control can set it (a name is feedback only).
    pub fn settable(&self) -> bool {
        !matches!(self.kind(), Kind::Text | Kind::Subscribe)
    }
}

/// Live state feedback carries besides the session: talk and the
/// oscillator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Live {
    pub talk: bool,
    pub oscillator: bool,
}

impl Live {
    pub fn of(status: &livestage_engine::EngineStatus) -> Self {
        Self {
            talk: status.talkback_active,
            oscillator: status.oscillator_on,
        }
    }
}

fn strip_ref(session: &Session, pos: StripPos) -> Option<StripRef> {
    Some(match pos {
        StripPos::Channel(n) => StripRef::Channel(session.channels.get(n)?.id),
        StripPos::Bus(n) => StripRef::Bus(session.buses.get(n)?.id),
        StripPos::Matrix(n) => StripRef::Matrix(session.matrices.get(n)?.id),
        StripPos::Master => StripRef::Master,
        StripPos::Dca(_) => return None,
    })
}

fn feed(session: &Session, source: SourcePos) -> Option<MatrixFeed> {
    Some(match source {
        SourcePos::Master => MatrixFeed::Master,
        SourcePos::Bus(b) => MatrixFeed::Bus(session.buses.get(b)?.id),
    })
}

/// A send's (or matrix source's) level and pan; one that does not exist
/// reads as off and centred.
fn send_values(session: &Session, target: &Target) -> Option<(f32, f32)> {
    match *target {
        Target::Send(c, b, _) => {
            let channel = session.channels.get(c)?;
            let bus = session.buses.get(b)?.id;
            let slot = channel.sends.iter().find(|s| s.bus == bus);
            Some(slot.map_or((MIN_FADER_DB, 0.0), |s| (s.level_db, s.pan)))
        }
        Target::MatrixSource(m, source, _) => {
            let matrix = session.matrices.get(m)?;
            let feed = feed(session, source)?;
            let slot = matrix.sources.iter().find(|s| s.source == feed);
            Some(slot.map_or((MIN_FADER_DB, 0.0), |s| (s.level_db, s.pan)))
        }
        _ => None,
    }
}

fn switch(on: bool) -> OscArg {
    OscArg::Int(i32::from(on))
}

/// An address's current value; `None` for one with no value (a trigger) or
/// whose strip does not exist.
pub fn read(session: &Session, live: Live, target: &Target) -> Option<OscArg> {
    match *target {
        Target::Strip(StripPos::Dca(n), leaf) => {
            let dca = session.dcas.get(n)?;
            match leaf {
                Leaf::Fader => Some(OscArg::Float(db_to_position(dca.level_db))),
                Leaf::Mute => Some(switch(dca.mute)),
                Leaf::Name => Some(OscArg::Str(dca.name.clone())),
                _ => None,
            }
        }
        Target::Strip(pos, leaf) => {
            let strip = strip_ref(session, pos)?;
            let core = session.strip(strip)?;
            Some(match leaf {
                Leaf::Fader => OscArg::Float(db_to_position(core.fader_db)),
                Leaf::Db => OscArg::Float(core.fader_db),
                Leaf::Mute => switch(core.mute),
                Leaf::Pan => OscArg::Float(core.pan),
                Leaf::Solo => switch(core.solo),
                Leaf::Name => OscArg::Str(match strip {
                    StripRef::Channel(id) => session.channel(id)?.name.clone(),
                    StripRef::Bus(id) => session.bus(id)?.name.clone(),
                    StripRef::Matrix(id) => session.matrix(id)?.name.clone(),
                    StripRef::Master => "Master".to_string(),
                }),
            })
        }
        Target::Send(_, _, leaf) | Target::MatrixSource(_, _, leaf) => {
            let (level, pan) = send_values(session, target)?;
            Some(match leaf {
                SendLeaf::Fader => OscArg::Float(db_to_position(level)),
                SendLeaf::Db => OscArg::Float(level),
                SendLeaf::Pan => OscArg::Float(pan),
            })
        }
        Target::Talk => Some(switch(live.talk)),
        Target::OscillatorOn => Some(switch(live.oscillator)),
        Target::VirtualSoundcheck => Some(switch(session.playback.virtual_soundcheck)),
        _ => None,
    }
}

/// Every address with a value, in mixer order, with its value now.
pub fn values(session: &Session, live: Live) -> Vec<(String, OscArg)> {
    let mut targets = Vec::new();
    let leaves = [
        Leaf::Fader,
        Leaf::Db,
        Leaf::Mute,
        Leaf::Pan,
        Leaf::Solo,
        Leaf::Name,
    ];
    let send_leaves = [SendLeaf::Fader, SendLeaf::Db, SendLeaf::Pan];
    for c in 0..session.channels.len() {
        targets.extend(leaves.map(|l| Target::Strip(StripPos::Channel(c), l)));
        for b in 0..session.buses.len() {
            targets.extend(send_leaves.map(|l| Target::Send(c, b, l)));
        }
    }
    for b in 0..session.buses.len() {
        targets.extend(leaves.map(|l| Target::Strip(StripPos::Bus(b), l)));
    }
    for m in 0..session.matrices.len() {
        targets.extend(leaves.map(|l| Target::Strip(StripPos::Matrix(m), l)));
        targets.extend(send_leaves.map(|l| Target::MatrixSource(m, SourcePos::Master, l)));
        for b in 0..session.buses.len() {
            targets.extend(send_leaves.map(|l| Target::MatrixSource(m, SourcePos::Bus(b), l)));
        }
    }
    targets.extend(
        [Leaf::Fader, Leaf::Db, Leaf::Mute, Leaf::Pan, Leaf::Name]
            .map(|l| Target::Strip(StripPos::Master, l)),
    );
    for d in 0..session.dcas.len().min(livestage_engine::DCA_COUNT) {
        targets.extend(
            [Leaf::Fader, Leaf::Mute, Leaf::Name].map(|l| Target::Strip(StripPos::Dca(d), l)),
        );
    }
    targets.extend([
        Target::Talk,
        Target::OscillatorOn,
        Target::VirtualSoundcheck,
    ]);
    targets
        .iter()
        .filter_map(|t| Some((t.address(), read(session, live, t)?)))
        .collect()
}

/// What an incoming value asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// No argument: a question (or a bare trigger).
    Nothing,
    Number(f32),
}

/// The JSON command that sets `target` from `input`: `Ok(None)` when there
/// is nothing to do (a button released), `Err` when it cannot be done (no
/// such strip or scene, a name, a bad value).
pub fn command(session: &Session, target: &Target, input: Input) -> Result<Option<Value>, ()> {
    let number = match input {
        Input::Number(v) if v.is_finite() => Some(v),
        Input::Number(_) => return Err(()),
        Input::Nothing => None,
    };
    let kind = target.kind();
    let value = match kind {
        Kind::Trigger | Kind::Subscribe => number.unwrap_or(1.0),
        _ => number.ok_or(())?,
    };
    if kind == Kind::Trigger && value == 0.0 {
        // A button let go.
        return Ok(None);
    }
    let db = match kind {
        Kind::Position => position_to_db(value),
        _ => value.clamp(MIN_FADER_DB, MAX_FADER_DB),
    };
    let on = value >= 0.5;
    let pan = value.clamp(-1.0, 1.0);
    Ok(Some(match *target {
        Target::Strip(StripPos::Dca(n), leaf) => match leaf {
            Leaf::Fader => json!({"cmd": "set_dca_level", "dca": n, "db": db}),
            Leaf::Mute => json!({"cmd": "set_dca_mute", "dca": n, "mute": on}),
            _ => return Err(()),
        },
        Target::Strip(pos, leaf) => {
            let strip = strip_ref(session, pos).ok_or(())?;
            match leaf {
                Leaf::Fader | Leaf::Db => json!({"cmd": "set_fader", "strip": strip, "db": db}),
                Leaf::Mute => json!({"cmd": "set_mute", "strip": strip, "mute": on}),
                Leaf::Pan => json!({"cmd": "set_pan", "strip": strip, "pan": pan}),
                Leaf::Solo => json!({"cmd": "set_solo", "strip": strip, "solo": on}),
                Leaf::Name => return Err(()),
            }
        }
        Target::Send(c, b, leaf) => {
            let channel = session.channels.get(c).ok_or(())?.id;
            let bus = session.buses.get(b).ok_or(())?.id;
            match leaf {
                SendLeaf::Fader | SendLeaf::Db => json!({
                    "cmd": "set_send", "channel": channel, "bus": bus, "level_db": db,
                }),
                // A pan of its own: the send stops following the channel's.
                SendLeaf::Pan => {
                    let (level, _) = send_values(session, target).ok_or(())?;
                    json!({
                        "cmd": "set_send", "channel": channel, "bus": bus, "level_db": level,
                        "pan": pan, "pan_follow": false,
                    })
                }
            }
        }
        Target::MatrixSource(m, source, leaf) => {
            let matrix = session.matrices.get(m).ok_or(())?.id;
            let source = feed(session, source).ok_or(())?;
            match leaf {
                SendLeaf::Fader | SendLeaf::Db => json!({
                    "cmd": "set_matrix_send", "matrix": matrix, "source": source, "level_db": db,
                }),
                SendLeaf::Pan => {
                    let (level, _) = send_values(session, target).ok_or(())?;
                    json!({
                        "cmd": "set_matrix_send", "matrix": matrix, "source": source,
                        "level_db": level, "pan": pan,
                    })
                }
            }
        }
        Target::SceneRecall => {
            let position = value.round();
            if position < 1.0 {
                return Err(());
            }
            let scene = session.scenes.get(position as usize - 1).ok_or(())?;
            json!({"cmd": "scene_recall", "id": scene.id})
        }
        Target::SceneNext | Target::ScenePrevious => {
            let current = session
                .current_scene
                .and_then(|id| session.scenes.iter().position(|s| s.id == id));
            let index = match (*target, current) {
                (Target::SceneNext, None) => 0,
                (Target::SceneNext, Some(i)) => i + 1,
                (_, Some(i)) if i > 0 => i - 1,
                _ => return Ok(None),
            };
            match session.scenes.get(index) {
                Some(scene) => json!({"cmd": "scene_recall", "id": scene.id}),
                None => return Ok(None),
            }
        }
        Target::Talk => json!({"cmd": "talk", "active": on}),
        Target::OscillatorOn => json!({"cmd": "oscillator_on", "on": on}),
        Target::PlaybackPlay => json!({"cmd": "playback", "action": "play"}),
        Target::PlaybackStop => json!({"cmd": "playback", "action": "stop"}),
        Target::VirtualSoundcheck => json!({"cmd": "set_virtual_soundcheck", "on": on}),
        Target::Subscribe => return Err(()),
    }))
}

// ----- MIDI -----------------------------------------------------------------------

/// A channel message as the maps see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    pub key: MidiKey,
    /// CC value, note velocity (0 for a note off), program number.
    pub value: u8,
    /// A note on, a CC at 64 or more, a program change.
    pub press: bool,
}

pub fn decode_midi(bytes: &[u8]) -> Option<MidiEvent> {
    let status = *bytes.first()?;
    let channel = (status & 0x0F) + 1;
    let data = |i: usize| bytes.get(i).copied().filter(|b| *b < 0x80);
    let (kind, number, value) = match status & 0xF0 {
        0x80 => (MidiKind::Note, data(1)?, 0),
        0x90 => (MidiKind::Note, data(1)?, data(2)?),
        0xB0 => (MidiKind::Cc, data(1)?, data(2)?),
        0xC0 => (MidiKind::Pc, data(1)?, data(1)?),
        _ => return None,
    };
    let press = match kind {
        MidiKind::Note => value > 0,
        MidiKind::Cc => value >= 64,
        MidiKind::Pc => true,
    };
    Some(MidiEvent {
        key: MidiKey {
            channel,
            kind,
            number,
        },
        value,
        press,
    })
}

/// Whether `map` answers `event`. A program-change map onto
/// `/scene/recall` takes any program on its channel.
fn map_matches(map: &MidiMap, event: &MidiEvent) -> bool {
    if map.midi.kind == MidiKind::Pc && event.key.kind == MidiKind::Pc {
        return map.midi.channel == event.key.channel
            && (map.target == "/scene/recall" || map.midi.number == event.key.number);
    }
    map.midi == event.key
}

/// The value `event` gives `map`'s target: `None` when it does nothing
/// (a release on a toggle).
fn midi_input(
    map: &MidiMap,
    target: &Target,
    event: &MidiEvent,
    session: &Session,
    live: Live,
) -> Option<Input> {
    let v = f32::from(event.value) / 127.0;
    match target.kind() {
        Kind::Position => Some(Input::Number(v)),
        Kind::Db => Some(Input::Number(position_to_db(v))),
        Kind::Pan => Some(Input::Number(v * 2.0 - 1.0)),
        Kind::Trigger => event.press.then_some(Input::Number(1.0)),
        Kind::Recall => match event.key.kind {
            MidiKind::Pc | MidiKind::Cc => Some(Input::Number(f32::from(event.value) + 1.0)),
            MidiKind::Note => event
                .press
                .then(|| Input::Number(f32::from(event.key.number) + 1.0)),
        },
        Kind::Switch => match map.mode {
            MapMode::Toggle => {
                if !event.press {
                    return None;
                }
                let now = read(session, live, target).is_some_and(|v| v == OscArg::Int(1));
                Some(Input::Number(if now { 0.0 } else { 1.0 }))
            }
            MapMode::Momentary | MapMode::Absolute => {
                if event.key.kind == MidiKind::Pc {
                    return None;
                }
                Some(Input::Number(if event.press { 1.0 } else { 0.0 }))
            }
        },
        Kind::Text | Kind::Subscribe => None,
    }
}

/// The byte a value goes out as (CC value or note velocity).
fn feedback_byte(target: &Target, value: &OscArg) -> Option<u8> {
    let v = match value {
        OscArg::Float(v) => *v,
        OscArg::Int(i) => return Some(if *i != 0 { 127 } else { 0 }),
        _ => return None,
    };
    let unit = match target.kind() {
        Kind::Position => v,
        Kind::Db => db_to_position(v),
        Kind::Pan => (v + 1.0) / 2.0,
        _ => return None,
    };
    Some((unit.clamp(0.0, 1.0) * 127.0).round() as u8)
}

fn midi_bytes(key: &MidiKey, data: u8) -> Option<[u8; 3]> {
    let channel = key.channel.saturating_sub(1) & 0x0F;
    match key.kind {
        MidiKind::Cc => Some([0xB0 | channel, key.number, data]),
        MidiKind::Note => Some([0x90 | channel, key.number, data]),
        MidiKind::Pc => None,
    }
}

// ----- the queue into the command loop ------------------------------------------------

/// What the OSC thread and the MIDI callbacks hand the command loop.
pub enum RemoteIn {
    Osc {
        from: SocketAddr,
        packet: OscPacket,
    },
    Midi {
        port: u64,
        message: [u8; 3],
        len: u8,
    },
}

#[derive(Default)]
struct Counters {
    /// Events in the queue now.
    pending: AtomicUsize,
    /// Events dropped because the queue was full.
    dropped: AtomicU64,
    /// OSC datagrams received.
    received: AtomicU64,
    /// OSC datagrams that did not parse.
    unreadable: AtomicU64,
}

/// The inbound queue as remote producers see it: bounded at [`QUEUE`]
/// events; a push never blocks — when full, the event is dropped and
/// counted.
#[derive(Clone)]
pub struct RemoteSender {
    inbound: Sender<Inbound>,
    counters: Arc<Counters>,
}

impl RemoteSender {
    pub fn new(inbound: Sender<Inbound>) -> Self {
        Self {
            inbound,
            counters: Arc::default(),
        }
    }

    pub fn push(&self, event: RemoteIn) -> bool {
        let counters = &self.counters;
        if counters.pending.fetch_add(1, Ordering::AcqRel) >= QUEUE {
            counters.pending.fetch_sub(1, Ordering::AcqRel);
            counters.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if self.inbound.send(Inbound::Remote(event)).is_err() {
            counters.pending.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// The command loop took one event off the queue.
    fn taken(&self) {
        self.counters.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

struct OscSocket {
    socket: UdpSocket,
    local: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for OscSocket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_osc(ip: IpAddr, port: u16, sender: RemoteSender) -> Result<OscSocket, String> {
    let socket = UdpSocket::bind((ip, port)).map_err(|e| format!("OSC on {ip}:{port}: {e}"))?;
    let local = socket.local_addr().map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(OSC_POLL))
        .map_err(|e| e.to_string())?;
    let reader = socket.try_clone().map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread = std::thread::Builder::new()
        .name("livestage-osc".to_string())
        .spawn({
            let stop = stop.clone();
            move || {
                let mut buffer = vec![0u8; 65_536];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((len, from)) = reader.recv_from(&mut buffer) else {
                        continue; // the poll timeout, or a reset from a gone peer
                    };
                    sender.counters.received.fetch_add(1, Ordering::Relaxed);
                    match osc::decode(&buffer[..len]) {
                        Ok(packet) => {
                            sender.push(RemoteIn::Osc { from, packet });
                        }
                        Err(_) => {
                            sender.counters.unreadable.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(OscSocket {
        socket,
        local,
        stop,
        thread: Some(thread),
    })
}

// ----- the remote as the command loop holds it ------------------------------------------

/// What the command loop does after an event: the commands to run (as the
/// engineer), and messages for the admins' pages.
#[derive(Default)]
pub struct Actions {
    pub commands: Vec<Value>,
    pub messages: Vec<Value>,
}

pub struct Remote {
    path: Option<PathBuf>,
    settings: RemoteSettings,
    /// `None` in tests: nothing listens.
    sender: Option<RemoteSender>,
    /// Where OSC listens: the web UI's address.
    ip: IpAddr,
    osc: Option<OscSocket>,
    osc_error: Option<String>,
    subscribers: Vec<(SocketAddr, Instant)>,
    midi: Midi,
    /// `midi_learn`: the target and mode the next MIDI message is mapped to.
    learning: Option<(String, MapMode)>,
    /// Feedback: the value of every address as last sent, and what it was
    /// worked out from.
    sent: HashMap<String, OscArg>,
    sent_from: Option<(u64, Live, usize)>,
    next_feedback: Instant,
    /// MIDI messages whose own value is not to be echoed back to them.
    echo: HashMap<MidiKey, u8>,
    /// OSC messages with an unknown address or a bad value; MIDI maps that
    /// came to nothing; commands refused or failed.
    ignored: u64,
    refused: u64,
}

impl Remote {
    /// The settings at `path` (none there, or unreadable: the defaults, OSC
    /// off), started.
    pub fn open(
        path: Option<PathBuf>,
        ip: IpAddr,
        sender: Option<RemoteSender>,
    ) -> (Self, Vec<String>) {
        let mut log = Vec::new();
        let settings = match &path {
            Some(path) if path.exists() => match std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|text| {
                    serde_json::from_str::<RemoteSettings>(&text).map_err(|e| e.to_string())
                })
                .and_then(validate)
            {
                Ok(settings) => settings,
                Err(error) => {
                    log.push(format!(
                        "{}: {error}; remote control is off until it is set again",
                        path.display()
                    ));
                    RemoteSettings::default()
                }
            },
            _ => RemoteSettings::default(),
        };
        let mut remote = Self {
            path,
            settings: RemoteSettings::default(),
            sender,
            ip,
            osc: None,
            osc_error: None,
            subscribers: Vec::new(),
            midi: Midi::default(),
            learning: None,
            sent: HashMap::new(),
            sent_from: None,
            next_feedback: Instant::now(),
            echo: HashMap::new(),
            ignored: 0,
            refused: 0,
        };
        remote.apply(settings);
        if let Some(osc) = &remote.osc {
            log.push(format!("OSC remote on udp://{}", osc.local));
            if !osc.local.ip().is_loopback() {
                log.push(
                    "OSC is open to the network: anyone who can reach it controls this mixer"
                        .to_string(),
                );
            }
        }
        if let Some(error) = &remote.osc_error {
            log.push(error.clone());
        }
        (remote, log)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[cfg(test)]
    pub fn settings(&self) -> &RemoteSettings {
        &self.settings
    }

    /// Whether anything is listening or wants feedback.
    pub fn active(&self) -> bool {
        self.osc.is_some()
            || !self.settings.midi.inputs.is_empty()
            || !self.settings.midi.outputs.is_empty()
    }

    /// Start, stop or move OSC and open the MIDI ports for `settings`.
    fn apply(&mut self, settings: RemoteSettings) {
        let osc_moved = settings.osc.enabled != self.settings.osc.enabled
            || settings.osc.port != self.settings.osc.port
            || (settings.osc.enabled && self.osc.is_none());
        self.settings = settings;
        if osc_moved {
            // Closed (its thread joined) before the port is bound again.
            self.osc = None;
            self.osc_error = None;
            self.subscribers.clear();
            if self.settings.osc.enabled {
                match &self.sender {
                    Some(sender) => {
                        match start_osc(self.ip, self.settings.osc.port, sender.clone()) {
                            Ok(osc) => self.osc = Some(osc),
                            Err(error) => self.osc_error = Some(error),
                        }
                    }
                    None => self.osc_error = Some("OSC is not started in tests".to_string()),
                }
            }
        }
        self.midi.set_ports(
            &self.settings.midi.inputs,
            &self.settings.midi.outputs,
            self.sender.as_ref(),
        );
        // Everything is sent again to whoever listens now.
        self.sent.clear();
        self.sent_from = None;
    }

    /// `{"osc":{…},"midi":{…},"dropped","ignored","refused","learning"}`.
    pub fn status(&self) -> Value {
        let counters = self.sender.as_ref().map(|s| s.counters.clone());
        let count = |f: fn(&Counters) -> u64| counters.as_ref().map_or(0, |c| f(c));
        json!({
            "osc": {
                "listening": self.osc.as_ref().map(|o| o.local.to_string()),
                "error": self.osc_error,
                "subscribers": self.subscribers.len(),
                "received": count(|c| c.received.load(Ordering::Relaxed)),
                "unreadable": count(|c| c.unreadable.load(Ordering::Relaxed)),
            },
            "midi": self.midi.status(),
            "dropped": count(|c| c.dropped.load(Ordering::Relaxed)),
            "ignored": self.ignored,
            "refused": self.refused,
            "learning": self.learning.as_ref().map(|(target, mode)| json!({"target": target, "mode": mode})),
        })
    }

    /// `{"type":"remote","remote":settings,"status":…}`.
    pub fn message(&self) -> Value {
        json!({"type": "remote", "remote": self.settings, "status": self.status()})
    }

    /// A command run for the remote was refused or failed.
    pub fn refused(&mut self) {
        self.refused += 1;
    }

    /// `remote_settings`, `set_remote`, `midi_ports`, `midi_learn`,
    /// `midi_learn_cancel` (admins only — checked before). `None` for any
    /// other command. The second value: tell the admins' pages.
    pub fn command(&mut self, cmd: &str, value: &Value) -> Option<(Value, bool)> {
        Some(match cmd {
            "remote_settings" => (
                json!({"ok": true, "remote": self.settings, "status": self.status()}),
                false,
            ),
            "set_remote" => {
                let asked = value
                    .get("remote")
                    .cloned()
                    .ok_or_else(|| "set_remote needs \"remote\"".to_string())
                    .and_then(|v| {
                        serde_json::from_value::<RemoteSettings>(v).map_err(|e| e.to_string())
                    })
                    .and_then(validate);
                match asked {
                    Ok(settings) => match self.save(&settings) {
                        Ok(()) => {
                            self.apply(settings);
                            (
                                json!({"ok": true, "remote": self.settings, "status": self.status()}),
                                true,
                            )
                        }
                        Err(error) => (json!({"ok": false, "error": error}), false),
                    },
                    Err(error) => (json!({"ok": false, "error": error}), false),
                }
            }
            "midi_ports" => match crate::midi::list() {
                Ok((inputs, outputs)) => (
                    json!({"ok": true, "inputs": inputs, "outputs": outputs}),
                    false,
                ),
                Err(error) => (
                    json!({"ok": false, "error": error, "inputs": [], "outputs": []}),
                    false,
                ),
            },
            "midi_learn" => {
                let target = value
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mode = value
                    .get("mode")
                    .filter(|m| !m.is_null())
                    .map(|m| serde_json::from_value::<MapMode>(m.clone()))
                    .transpose();
                match (parse_target(target).filter(Target::settable), mode) {
                    (Some(target), Ok(mode)) => {
                        self.learning = Some((target.address(), mode.unwrap_or_default()));
                        (json!({"ok": true}), true)
                    }
                    (None, _) => (
                        json!({"ok": false, "error": format!("{target} is not an address a MIDI control can set")}),
                        false,
                    ),
                    (_, Err(error)) => (json!({"ok": false, "error": error.to_string()}), false),
                }
            }
            "midi_learn_cancel" => {
                let was = self.learning.take().is_some();
                (json!({"ok": true}), was)
            }
            _ => return None,
        })
    }

    fn save(&self, settings: &RemoteSettings) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
        write_atomic(path, &text)
    }

    /// One event off the queue.
    pub fn input(
        &mut self,
        event: RemoteIn,
        session: &Session,
        live: Live,
        now: Instant,
    ) -> Actions {
        if let Some(sender) = &self.sender {
            sender.taken();
        }
        let mut actions = Actions::default();
        match event {
            RemoteIn::Osc { from, packet } => {
                for message in packet.into_messages() {
                    self.osc_message(from, message, session, live, now, &mut actions);
                }
            }
            RemoteIn::Midi { port, message, len } => {
                if self.midi.input_name(port).is_none() {
                    return actions; // a port closed since
                }
                if let Some(event) = decode_midi(&message[..usize::from(len)]) {
                    self.midi_event(event, session, live, &mut actions);
                }
            }
        }
        actions
    }

    fn osc_message(
        &mut self,
        from: SocketAddr,
        message: OscMessage,
        session: &Session,
        live: Live,
        now: Instant,
        actions: &mut Actions,
    ) {
        let Some(target) = parse_target(&message.address) else {
            self.ignored += 1;
            return;
        };
        if target == Target::Subscribe {
            self.subscribe(from, session, live, now);
            return;
        }
        let input = match message.args.first() {
            None | Some(OscArg::Nil) => Input::Nothing,
            Some(arg) => match arg.as_f32() {
                Some(v) => Input::Number(v),
                None => {
                    self.ignored += 1;
                    return;
                }
            },
        };
        // A leaf asked with no value: answer with it.
        if input == Input::Nothing && !matches!(target.kind(), Kind::Trigger) {
            match read(session, live, &target) {
                Some(value) => self.send_osc(
                    from,
                    &[OscMessage {
                        address: target.address(),
                        args: vec![value],
                    }],
                ),
                None => self.ignored += 1,
            }
            return;
        }
        match command(session, &target, input) {
            Ok(Some(command)) => actions.commands.push(command),
            Ok(None) => {}
            Err(()) => self.ignored += 1,
        }
    }

    fn subscribe(&mut self, from: SocketAddr, session: &Session, live: Live, now: Instant) {
        if let Some(entry) = self.subscribers.iter_mut().find(|(addr, _)| *addr == from) {
            entry.1 = now + LEASE;
        } else {
            self.subscribers.retain(|(_, until)| *until > now);
            if self.subscribers.len() >= MAX_SUBSCRIBERS {
                self.ignored += 1;
                return;
            }
            self.subscribers.push((from, now + LEASE));
        }
        // A full dump at once, on every (re)subscription.
        let dump: Vec<OscMessage> = values(session, live)
            .into_iter()
            .map(|(address, value)| OscMessage {
                address,
                args: vec![value],
            })
            .collect();
        self.send_osc(from, &dump);
    }

    fn send_osc(&self, to: SocketAddr, messages: &[OscMessage]) {
        let Some(osc) = &self.osc else {
            return;
        };
        if messages.len() == 1 {
            let _ = osc.socket.send_to(&osc::message_bytes(&messages[0]), to);
            return;
        }
        for datagram in osc::bundles(messages, DATAGRAM) {
            let _ = osc.socket.send_to(&datagram, to);
        }
    }

    fn midi_event(
        &mut self,
        event: MidiEvent,
        session: &Session,
        live: Live,
        actions: &mut Actions,
    ) {
        if let Some((target, mode)) = self.learning.clone() {
            // A press: not the release that follows the move that was meant.
            if event.press || event.key.kind == MidiKind::Cc {
                self.learning = None;
                let map = MidiMap {
                    midi: event.key,
                    target,
                    mode,
                };
                let mut settings = self.settings.clone();
                settings.midi.maps.retain(|m| m.midi != map.midi);
                settings.midi.maps.push(map.clone());
                match self.save(&settings) {
                    Ok(()) => {
                        self.apply(settings);
                        actions
                            .messages
                            .push(json!({"type": "remote", "learned": map}));
                    }
                    Err(error) => actions.messages.push(json!({
                        "type": "remote", "learned": null, "error": error,
                    })),
                }
                actions.messages.push(self.message());
            }
            return;
        }
        let maps: Vec<MidiMap> = self
            .settings
            .midi
            .maps
            .iter()
            .filter(|m| map_matches(m, &event))
            .cloned()
            .collect();
        if maps.is_empty() {
            self.ignored += 1;
        }
        for map in maps {
            let Some(target) = parse_target(&map.target) else {
                continue;
            };
            let Some(input) = midi_input(&map, &target, &event, session, live) else {
                continue;
            };
            match command(session, &target, input) {
                Ok(Some(command)) => {
                    // The control already shows what it just sent.
                    if map.mode != MapMode::Toggle {
                        let shown = match target.kind() {
                            Kind::Switch => {
                                if event.press {
                                    127
                                } else {
                                    0
                                }
                            }
                            _ => event.value,
                        };
                        if shown == event.value {
                            self.echo.insert(map.midi, shown);
                        }
                    }
                    actions.commands.push(command);
                }
                Ok(None) => {}
                Err(()) => self.ignored += 1,
            }
        }
    }

    /// Lapsed subscriptions, MIDI ports to look for again, and feedback when
    /// the session (or talk, or the oscillator) moved. Returns messages for
    /// the admins' pages.
    pub fn tick(
        &mut self,
        session: &Session,
        revision: u64,
        live: Live,
        now: Instant,
    ) -> Vec<Value> {
        let mut messages = Vec::new();
        let before = self.subscribers.len();
        self.subscribers.retain(|(_, until)| *until > now);
        let mut status_moved = before != self.subscribers.len();
        let sender = self.sender.clone();
        if self.midi.tick(now, sender.as_ref()) {
            status_moved = true;
            self.sent_from = None; // a port back: send it everything
            self.sent.clear();
        }
        if status_moved {
            messages.push(self.message());
        }
        if now < self.next_feedback {
            return messages;
        }
        self.next_feedback = now + FEEDBACK_PERIOD;
        let osc_listeners =
            self.settings.osc.feedback && self.osc.is_some() && !self.subscribers.is_empty();
        let midi_listeners = self.settings.midi.feedback
            && self.midi.has_outputs()
            && !self.settings.midi.maps.is_empty();
        if !osc_listeners && !midi_listeners {
            self.sent_from = None;
            self.sent.clear();
            self.echo.clear();
            return messages;
        }
        let from = (revision, live, self.subscribers.len());
        if self.sent_from == Some(from) {
            // A move that changed nothing: nothing to hold back either.
            self.echo.clear();
            return messages;
        }
        let first = self.sent_from.is_none();
        self.sent_from = Some(from);
        let mut changed = Vec::new();
        for (address, value) in values(session, live) {
            if self.sent.get(&address) != Some(&value) {
                self.sent.insert(address.clone(), value.clone());
                changed.push(OscMessage {
                    address,
                    args: vec![value],
                });
            }
        }
        if osc_listeners && !first && !changed.is_empty() {
            let subscribers: Vec<SocketAddr> = self.subscribers.iter().map(|(a, _)| *a).collect();
            for to in subscribers {
                self.send_osc(to, &changed);
            }
        }
        if midi_listeners {
            for message in &changed {
                for map in &self.settings.midi.maps {
                    if map.target != message.address {
                        continue;
                    }
                    let Some(target) = parse_target(&map.target) else {
                        continue;
                    };
                    let Some(data) = feedback_byte(&target, &message.args[0]) else {
                        continue;
                    };
                    if self.echo.get(&map.midi) == Some(&data) {
                        continue;
                    }
                    if let Some(bytes) = midi_bytes(&map.midi, data) {
                        self.midi.send(&bytes);
                    }
                }
            }
        }
        self.echo.clear();
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use livestage_engine::{BusStrip, ChannelStrip, MatrixSource, MatrixStrip, SendSlot};

    fn session() -> Session {
        let mut s = Session::default();
        for (id, name) in [(10, "Kick"), (11, "Snare")] {
            s.channels
                .push(ChannelStrip::new(id, name.into(), Default::default()));
        }
        s.buses.push(BusStrip {
            id: 20,
            name: "Mon 1".into(),
            ..BusStrip::default()
        });
        s.buses.push(BusStrip {
            id: 21,
            name: "Mon 2".into(),
            ..BusStrip::default()
        });
        s.channels[1].sends.push(SendSlot {
            bus: 21,
            level_db: -6.0,
            pan: 0.5,
            ..SendSlot::default()
        });
        s.matrices.push(MatrixStrip {
            id: 30,
            name: "Fill".into(),
            sources: vec![MatrixSource {
                source: MatrixFeed::Bus(21),
                level_db: -3.0,
                pan: -0.25,
            }],
            ..MatrixStrip::default()
        });
        s.channels[0].core.fader_db = 0.0;
        s.channels[0].core.mute = true;
        for (i, id) in [(0, 40), (1, 41), (2, 42)] {
            let scene: livestage_engine::Scene =
                serde_json::from_value(json!({"id": id, "name": format!("S{i}"), "mix": {}}))
                    .unwrap();
            s.scenes.push(scene);
        }
        s.current_scene = Some(41);
        s
    }

    fn cmd(s: &Session, address: &str, input: Input) -> Value {
        command(s, &parse_target(address).unwrap(), input)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn addresses_parse_and_print_back() {
        for address in [
            "/ch/1/fader",
            "/ch/32/db",
            "/bus/2/mute",
            "/mtx/1/pan",
            "/master/fader",
            "/dca/8/mute",
            "/ch/2/send/1/pan",
            "/mtx/1/src/master/db",
            "/mtx/1/src/bus/2/fader",
            "/scene/recall",
            "/scene/next",
            "/scene/previous",
            "/talk",
            "/oscillator/on",
            "/playback/play",
            "/playback/stop",
            "/vsc",
            "/livestage/subscribe",
        ] {
            assert_eq!(parse_target(address).unwrap().address(), address);
        }
        assert_eq!(
            parse_target("/ch/01/fader").unwrap().address(),
            "/ch/1/fader"
        );
        for bad in [
            "/ch/0/fader",
            "/ch/1/gain",
            "/dca/9/fader",
            "/dca/1/pan",
            "/master/solo",
            "ch/1/fader",
            "/x",
        ] {
            assert!(parse_target(bad).is_none(), "{bad}");
        }
        assert!(!parse_target("/ch/1/name").unwrap().settable());
    }

    #[test]
    fn values_map_to_engine_commands_through_the_fader_law() {
        let s = session();
        assert_eq!(
            cmd(&s, "/ch/2/fader", Input::Number(0.75)),
            json!({"cmd": "set_fader", "strip": {"kind": "channel", "id": 11}, "db": 0.0})
        );
        assert_eq!(cmd(&s, "/ch/2/fader", Input::Number(0.5))["db"], -20.0);
        assert_eq!(cmd(&s, "/bus/1/db", Input::Number(-12.5))["db"], -12.5);
        assert_eq!(
            cmd(&s, "/bus/1/db", Input::Number(40.0))["db"],
            MAX_FADER_DB
        );
        assert_eq!(
            cmd(&s, "/mtx/1/mute", Input::Number(1.0)),
            json!({"cmd": "set_mute", "strip": {"kind": "matrix", "id": 30}, "mute": true})
        );
        assert_eq!(cmd(&s, "/master/pan", Input::Number(-3.0))["pan"], -1.0);
        assert_eq!(
            cmd(&s, "/ch/1/solo", Input::Number(1.0)),
            json!({"cmd": "set_solo", "strip": {"kind": "channel", "id": 10}, "solo": true})
        );
        assert_eq!(
            cmd(&s, "/dca/3/fader", Input::Number(0.75)),
            json!({"cmd": "set_dca_level", "dca": 2, "db": 0.0})
        );
        assert_eq!(
            cmd(&s, "/ch/1/send/2/db", Input::Number(-10.0)),
            json!({"cmd": "set_send", "channel": 10, "bus": 21, "level_db": -10.0})
        );
        assert_eq!(
            cmd(&s, "/ch/2/send/2/pan", Input::Number(0.25)),
            json!({"cmd": "set_send", "channel": 11, "bus": 21, "level_db": -6.0,
                   "pan": 0.25, "pan_follow": false})
        );
        assert_eq!(
            cmd(&s, "/mtx/1/src/bus/2/pan", Input::Number(0.5)),
            json!({"cmd": "set_matrix_send", "matrix": 30, "source": {"kind": "bus", "id": 21},
                   "level_db": -3.0, "pan": 0.5})
        );
        assert_eq!(
            cmd(&s, "/mtx/1/src/master/fader", Input::Number(0.75)),
            json!({"cmd": "set_matrix_send", "matrix": 30, "source": {"kind": "master"}, "level_db": 0.0})
        );
        assert_eq!(
            cmd(&s, "/scene/recall", Input::Number(3.0)),
            json!({"cmd": "scene_recall", "id": 42})
        );
        assert_eq!(
            cmd(&s, "/scene/next", Input::Nothing),
            json!({"cmd": "scene_recall", "id": 42})
        );
        assert_eq!(
            cmd(&s, "/scene/previous", Input::Number(1.0)),
            json!({"cmd": "scene_recall", "id": 40})
        );
        assert_eq!(
            cmd(&s, "/talk", Input::Number(1.0)),
            json!({"cmd": "talk", "active": true})
        );
        assert_eq!(
            cmd(&s, "/oscillator/on", Input::Number(0.0)),
            json!({"cmd": "oscillator_on", "on": false})
        );
        assert_eq!(
            cmd(&s, "/playback/play", Input::Nothing),
            json!({"cmd": "playback", "action": "play"})
        );
        assert_eq!(
            cmd(&s, "/playback/stop", Input::Number(1.0)),
            json!({"cmd": "playback", "action": "stop"})
        );
        assert_eq!(
            cmd(&s, "/vsc", Input::Number(1.0)),
            json!({"cmd": "set_virtual_soundcheck", "on": true})
        );

        // A button let go does nothing; what does not exist is refused.
        let next = parse_target("/scene/next").unwrap();
        assert_eq!(command(&s, &next, Input::Number(0.0)), Ok(None));
        for (address, input) in [
            ("/ch/3/fader", Input::Number(0.5)),
            ("/bus/1/send/1/fader", Input::Number(0.5)),
            ("/scene/recall", Input::Number(4.0)),
            ("/scene/recall", Input::Number(0.0)),
            ("/ch/1/name", Input::Number(1.0)),
            ("/ch/1/fader", Input::Number(f32::NAN)),
            ("/ch/1/fader", Input::Nothing),
        ] {
            if let Some(target) = parse_target(address) {
                assert_eq!(command(&s, &target, input), Err(()), "{address}");
            }
        }
        // At the last scene, next stays.
        let mut last = session();
        last.current_scene = Some(42);
        assert_eq!(command(&last, &next, Input::Nothing), Ok(None));
    }

    #[test]
    fn feedback_values_cover_every_strip_and_send() {
        let s = session();
        let live = Live {
            talk: true,
            oscillator: false,
        };
        let map: HashMap<String, OscArg> = values(&s, live).into_iter().collect();
        assert_eq!(map["/ch/1/fader"], OscArg::Float(0.75));
        assert_eq!(map["/ch/1/mute"], OscArg::Int(1));
        assert_eq!(map["/ch/1/name"], OscArg::Str("Kick".into()));
        assert_eq!(map["/ch/2/send/2/db"], OscArg::Float(-6.0));
        assert_eq!(map["/ch/2/send/2/pan"], OscArg::Float(0.5));
        assert_eq!(
            map["/ch/1/send/1/db"],
            OscArg::Float(MIN_FADER_DB),
            "no send: off"
        );
        assert_eq!(map["/mtx/1/src/bus/2/db"], OscArg::Float(-3.0));
        assert_eq!(map["/mtx/1/src/master/db"], OscArg::Float(MIN_FADER_DB));
        assert_eq!(map["/master/name"], OscArg::Str("Master".into()));
        assert_eq!(map["/dca/8/mute"], OscArg::Int(0));
        assert_eq!(map["/talk"], OscArg::Int(1));
        assert_eq!(map["/oscillator/on"], OscArg::Int(0));
        assert_eq!(map["/vsc"], OscArg::Int(0));
        assert!(!map.contains_key("/master/solo"));
        assert!(!map.contains_key("/scene/next"));
    }

    #[test]
    fn midi_messages_drive_their_maps() {
        let s = session();
        let live = Live::default();
        let cc = |n: u8, v: u8| decode_midi(&[0xB0, n, v]).unwrap();
        let map = |kind, number, target: &str, mode| MidiMap {
            midi: MidiKey {
                channel: 1,
                kind,
                number,
            },
            target: target.into(),
            mode,
        };
        let run = |m: &MidiMap, e: &MidiEvent| {
            let target = parse_target(&m.target).unwrap();
            midi_input(m, &target, e, &s, live)
                .and_then(|input| command(&s, &target, input).unwrap())
        };

        let fader = map(MidiKind::Cc, 7, "/ch/2/fader", MapMode::Absolute);
        assert_eq!(run(&fader, &cc(7, 127)).unwrap()["db"], MAX_FADER_DB);
        assert_eq!(run(&fader, &cc(7, 0)).unwrap()["db"], MIN_FADER_DB);
        let pan = map(MidiKind::Cc, 10, "/ch/2/pan", MapMode::Absolute);
        assert_eq!(run(&pan, &cc(10, 0)).unwrap()["pan"], -1.0);
        assert_eq!(run(&pan, &cc(10, 127)).unwrap()["pan"], 1.0);

        // Toggle: a press flips the mute; the release does nothing.
        let mute = map(MidiKind::Note, 36, "/ch/1/mute", MapMode::Toggle);
        let on = decode_midi(&[0x90, 36, 100]).unwrap();
        let off = decode_midi(&[0x80, 36, 0]).unwrap();
        assert_eq!(run(&mute, &on).unwrap()["mute"], false, "Kick was muted");
        assert_eq!(run(&mute, &off), None);
        let cc_mute = map(MidiKind::Cc, 20, "/ch/2/mute", MapMode::Toggle);
        assert_eq!(run(&cc_mute, &cc(20, 64)).unwrap()["mute"], true);
        assert_eq!(run(&cc_mute, &cc(20, 63)), None);

        // Momentary talk: on while held.
        let talk = map(MidiKind::Note, 40, "/talk", MapMode::Momentary);
        assert_eq!(
            run(&talk, &decode_midi(&[0x90, 40, 1]).unwrap()).unwrap()["active"],
            true
        );
        assert_eq!(
            run(&talk, &decode_midi(&[0x90, 40, 0]).unwrap()).unwrap()["active"],
            false
        );

        // A program change recalls scene program + 1, whatever the number.
        let pc = map(MidiKind::Pc, 0, "/scene/recall", MapMode::Absolute);
        let program = decode_midi(&[0xC0, 2]).unwrap();
        assert!(map_matches(&pc, &program));
        assert_eq!(
            run(&pc, &program).unwrap(),
            json!({"cmd": "scene_recall", "id": 42})
        );
        assert!(
            !map_matches(&pc, &decode_midi(&[0xC1, 2]).unwrap()),
            "another channel"
        );

        assert!(decode_midi(&[0xF8]).is_none());
        assert!(decode_midi(&[0xB0, 7]).is_none());
        assert_eq!(decode_midi(&[0x93, 60, 0]).unwrap().key.channel, 4);
    }

    #[test]
    fn feedback_bytes_follow_the_value_kind() {
        let fader = parse_target("/ch/1/fader").unwrap();
        let db = parse_target("/ch/1/db").unwrap();
        let pan = parse_target("/ch/1/pan").unwrap();
        let mute = parse_target("/ch/1/mute").unwrap();
        assert_eq!(feedback_byte(&fader, &OscArg::Float(1.0)), Some(127));
        assert_eq!(feedback_byte(&db, &OscArg::Float(0.0)), Some(95));
        assert_eq!(feedback_byte(&pan, &OscArg::Float(0.0)), Some(64));
        assert_eq!(feedback_byte(&mute, &OscArg::Int(1)), Some(127));
        assert_eq!(
            midi_bytes(
                &MidiKey {
                    channel: 2,
                    kind: MidiKind::Cc,
                    number: 7
                },
                100
            ),
            Some([0xB1, 7, 100])
        );
        // A CC value goes through the fader law and back to itself.
        for value in 0..=127u8 {
            let position = f32::from(value) / 127.0;
            let back = feedback_byte(&db, &OscArg::Float(position_to_db(position)));
            assert_eq!(back, Some(value));
        }
    }

    #[test]
    fn settings_are_checked_and_one_map_per_message_kept() {
        let settings: RemoteSettings = serde_json::from_value(json!({
            "osc": {"enabled": true, "port": 9000},
            "midi": {"inputs": ["A", "A", ""], "maps": [
                {"midi": {"channel": 1, "kind": "cc", "number": 7}, "target": "/ch/01/fader"},
                {"midi": {"channel": 1, "kind": "cc", "number": 7}, "target": "/ch/2/fader", "mode": "absolute"},
                {"midi": {"channel": 1, "kind": "note", "number": 36}, "target": "/ch/1/mute", "mode": "toggle"}
            ]}
        }))
        .unwrap();
        let settings = validate(settings).unwrap();
        assert!(settings.osc.feedback, "defaults fill in");
        assert_eq!(settings.midi.inputs, ["A"]);
        assert_eq!(settings.midi.maps.len(), 2);
        assert_eq!(settings.midi.maps[0].target, "/ch/2/fader");
        assert_eq!(settings.midi.maps[1].target, "/ch/1/mute");
        assert!(settings.midi.feedback);

        let defaults = RemoteSettings::default();
        assert_eq!(
            serde_json::to_value(&defaults).unwrap(),
            json!({"osc": {"enabled": false, "port": 8000, "feedback": true},
                   "midi": {"inputs": [], "outputs": [], "feedback": true, "maps": []}})
        );
        for bad in [
            json!({"midi": {"maps": [{"midi": {"channel": 0, "kind": "cc", "number": 7}, "target": "/talk"}]}}),
            json!({"midi": {"maps": [{"midi": {"channel": 1, "kind": "cc", "number": 128}, "target": "/talk"}]}}),
            json!({"midi": {"maps": [{"midi": {"channel": 1, "kind": "cc", "number": 1}, "target": "/ch/1/name"}]}}),
            json!({"osc": {"port": 0}}),
        ] {
            let settings: RemoteSettings = serde_json::from_value(bad.clone()).unwrap();
            assert!(validate(settings).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_queue_is_bounded_and_counts_what_it_drops() {
        let (tx, rx) = std::sync::mpsc::channel();
        let sender = RemoteSender::new(tx);
        let event = || RemoteIn::Midi {
            port: 0,
            message: [0xB0, 7, 1],
            len: 3,
        };
        for _ in 0..QUEUE {
            assert!(sender.push(event()));
        }
        assert!(!sender.push(event()));
        assert_eq!(sender.counters.dropped.load(Ordering::Relaxed), 1);
        // The loop takes one: room for one more.
        let _ = rx.recv().unwrap();
        sender.taken();
        assert!(sender.push(event()));
    }

    #[test]
    fn learn_maps_the_next_message_and_replaces_one_on_it() {
        let s = session();
        let (mut remote, _) = Remote::open(None, "127.0.0.1".parse().unwrap(), None);
        let (reply, _) = remote
            .command("midi_learn", &json!({"target": "/ch/9/name"}))
            .unwrap();
        assert_eq!(reply["ok"], false);
        let (reply, tell) = remote
            .command(
                "midi_learn",
                &json!({"target": "/ch/2/fader", "mode": "absolute"}),
            )
            .unwrap();
        assert!(reply["ok"] == true && tell);
        assert_eq!(remote.status()["learning"]["target"], "/ch/2/fader");

        let mut actions = Actions::default();
        remote.midi_event(
            decode_midi(&[0xB0, 7, 99]).unwrap(),
            &s,
            Live::default(),
            &mut actions,
        );
        assert!(actions.commands.is_empty(), "learning moves nothing");
        assert_eq!(
            actions.messages[0],
            json!({"type": "remote", "learned": {
                "midi": {"channel": 1, "kind": "cc", "number": 7},
                "target": "/ch/2/fader", "mode": "absolute"}})
        );
        assert_eq!(remote.settings().midi.maps.len(), 1);

        // Learn the same control for another target: it is replaced.
        remote.command("midi_learn", &json!({"target": "/ch/1/fader"}));
        remote.midi_event(
            decode_midi(&[0xB0, 7, 5]).unwrap(),
            &s,
            Live::default(),
            &mut actions,
        );
        assert_eq!(remote.settings().midi.maps.len(), 1);
        assert_eq!(remote.settings().midi.maps[0].target, "/ch/1/fader");

        // Now the control moves the fader.
        let mut actions = Actions::default();
        remote.midi_event(
            decode_midi(&[0xB0, 7, 127]).unwrap(),
            &s,
            Live::default(),
            &mut actions,
        );
        assert_eq!(actions.commands[0]["db"], MAX_FADER_DB);
        assert_eq!(
            remote.echo.get(&remote.settings().midi.maps[0].midi),
            Some(&127)
        );

        remote.command(
            "midi_learn",
            &json!({"target": "/talk", "mode": "momentary"}),
        );
        let (_, tell) = remote.command("midi_learn_cancel", &json!({})).unwrap();
        assert!(tell);
        assert_eq!(remote.status()["learning"], Value::Null);
    }
}
