//! # Futureboard MIDI FX
//!
//! MIDI effects and MIDI generators that sit between a track's MIDI (clips,
//! live input) and its instrument, and run on the engine's audio thread.
//!
//! - [`MidiFx`]: one device. [`MidiFx::process`] turns a block of input events
//!   into output events against the block's tempo and position; it never
//!   allocates, locks or blocks. [`MidiFx::release`] ends every note the device
//!   has sounding (bypass, removal, transport reset), so a device can never
//!   leave a stuck note behind.
//! - [`DeviceKind`]: the catalogue — stable ids saved in projects, parameter
//!   lists the UI draws, and [`DeviceKind::create`] (call it off the audio
//!   thread: it allocates the device).
//! - [`Chain`]: devices run in order, with the ping-pong buffers they need.
//!
//! Generators (Pattern, Random Melody) are MIDI FX that make notes from the
//! transport rather than from their input; they play only while the transport
//! runs and pass their input through when *Thru* is on. Their randomness (and
//! Humanize's, Velocity's) is derived from the musical position and a seed,
//! not from a running random state, so a loop or an export plays back the
//! same notes every time.

mod devices;
mod event;
mod params;
mod time;

pub use event::{EventBuffer, Kind, MAX_EVENTS, MidiEvent, NoteTracker, Scheduler, emit};
pub use params::{
    ParamDesc, ParamKind, RATE_LABELS, ROOT_LABELS, SCALE_LABELS, Unit, rate_beats, scale_mask,
};
pub use time::Context;

/// The most parameters any device has.
pub const MAX_PARAMS: usize = 16;

/// One MIDI FX device.
pub trait MidiFx: Send {
    /// Current value of parameter `index` (in [`DeviceKind::params`] order).
    fn param(&self, index: usize) -> f32;

    /// Set parameter `index`, clamped to its range. Out-of-range indices are
    /// ignored. Realtime-safe.
    fn set_param(&mut self, index: usize, value: f32);

    /// Turn `input` into `out` for one block. `out` is cleared first.
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer);

    /// Note-offs at `offset` for everything this device has sounding, and
    /// forget held input and pending events. Used for bypass, removal and
    /// transport resets.
    fn release(&mut self, offset: u32, out: &mut EventBuffer);

    /// The device as its concrete type, for a host that runs devices of its
    /// own in a chain (a hosted plug-in stage) and must reach them between
    /// blocks. Built-in devices have nothing to expose.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
}

/// The built-in MIDI FX and generators.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Arpeggiator,
    Chord,
    Scale,
    Transpose,
    Velocity,
    NoteLength,
    Humanize,
    NoteRepeat,
    PatternGenerator,
    RandomMelody,
}

impl DeviceKind {
    pub const ALL: [DeviceKind; 10] = [
        DeviceKind::Arpeggiator,
        DeviceKind::Chord,
        DeviceKind::Scale,
        DeviceKind::Transpose,
        DeviceKind::Velocity,
        DeviceKind::NoteLength,
        DeviceKind::Humanize,
        DeviceKind::NoteRepeat,
        DeviceKind::PatternGenerator,
        DeviceKind::RandomMelody,
    ];

    /// Stable id saved in projects. Never rename one.
    pub fn id(self) -> &'static str {
        match self {
            DeviceKind::Arpeggiator => "arpeggiator",
            DeviceKind::Chord => "chord",
            DeviceKind::Scale => "scale",
            DeviceKind::Transpose => "transpose",
            DeviceKind::Velocity => "velocity",
            DeviceKind::NoteLength => "note-length",
            DeviceKind::Humanize => "humanize",
            DeviceKind::NoteRepeat => "note-repeat",
            DeviceKind::PatternGenerator => "pattern-generator",
            DeviceKind::RandomMelody => "random-melody",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    /// Localization key of the device name.
    pub fn label(self) -> &'static str {
        match self {
            DeviceKind::Arpeggiator => "midi-fx.device.arpeggiator",
            DeviceKind::Chord => "midi-fx.device.chord",
            DeviceKind::Scale => "midi-fx.device.scale",
            DeviceKind::Transpose => "midi-fx.device.transpose",
            DeviceKind::Velocity => "midi-fx.device.velocity",
            DeviceKind::NoteLength => "midi-fx.device.note-length",
            DeviceKind::Humanize => "midi-fx.device.humanize",
            DeviceKind::NoteRepeat => "midi-fx.device.note-repeat",
            DeviceKind::PatternGenerator => "midi-fx.device.pattern-generator",
            DeviceKind::RandomMelody => "midi-fx.device.random-melody",
        }
    }

    /// Localization key of the one-line summary "Add MIDI FX" shows.
    pub fn summary(self) -> &'static str {
        match self {
            DeviceKind::Arpeggiator => "midi-fx.device.arpeggiator.summary",
            DeviceKind::Chord => "midi-fx.device.chord.summary",
            DeviceKind::Scale => "midi-fx.device.scale.summary",
            DeviceKind::Transpose => "midi-fx.device.transpose.summary",
            DeviceKind::Velocity => "midi-fx.device.velocity.summary",
            DeviceKind::NoteLength => "midi-fx.device.note-length.summary",
            DeviceKind::Humanize => "midi-fx.device.humanize.summary",
            DeviceKind::NoteRepeat => "midi-fx.device.note-repeat.summary",
            DeviceKind::PatternGenerator => "midi-fx.device.pattern-generator.summary",
            DeviceKind::RandomMelody => "midi-fx.device.random-melody.summary",
        }
    }

    /// Makes notes from the transport rather than from its input.
    pub fn is_generator(self) -> bool {
        matches!(
            self,
            DeviceKind::PatternGenerator | DeviceKind::RandomMelody
        )
    }

    pub fn params(self) -> &'static [ParamDesc] {
        devices::params_of(self)
    }

    /// Index of the parameter with stable id `id`.
    pub fn param_index(self, id: &str) -> Option<usize> {
        self.params().iter().position(|p| p.id == id)
    }

    /// Whether parameter `id` has any effect with the device's other settings
    /// (`value` reads one by id). An editor dims the ones that do not: the
    /// chord's intervals outside the Custom shape, the arpeggiator's velocity
    /// while it plays notes as played, the pattern's note while it follows
    /// held notes.
    pub fn param_active(self, id: &str, value: impl Fn(&str) -> f32) -> bool {
        let is = |param: &str, index: f32| value(param).round() == index;
        match (self, id) {
            (DeviceKind::Chord, "interval1" | "interval2" | "interval3") => is("shape", 0.0),
            (DeviceKind::Arpeggiator, "velocity") => is("velocity-mode", 1.0),
            (DeviceKind::PatternGenerator, "note") => is("source", 0.0),
            _ => true,
        }
    }

    /// A new device with default parameters. Allocates: call it off the
    /// audio thread.
    pub fn create(self) -> Box<dyn MidiFx> {
        devices::create(self)
    }
}

/// Devices run in order, each one's output the next one's input.
pub struct Chain {
    devices: Vec<ChainSlot>,
    a: Box<EventBuffer>,
    b: Box<EventBuffer>,
}

struct ChainSlot {
    device: Box<dyn MidiFx>,
    enabled: bool,
}

impl Default for Chain {
    fn default() -> Self {
        Self::new()
    }
}

impl Chain {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            a: Box::default(),
            b: Box::default(),
        }
    }

    pub fn push(&mut self, device: Box<dyn MidiFx>, enabled: bool) {
        self.devices.push(ChainSlot { device, enabled });
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Take every device out (for re-ordering or reuse), leaving the chain
    /// empty. Allocates: control thread only.
    pub fn take_devices(&mut self) -> Vec<(Box<dyn MidiFx>, bool)> {
        self.devices
            .drain(..)
            .map(|slot| (slot.device, slot.enabled))
            .collect()
    }

    pub fn device_mut(&mut self, index: usize) -> Option<&mut dyn MidiFx> {
        Some(self.devices.get_mut(index)?.device.as_mut())
    }

    /// Bypass or re-enable device `index`. A change re-routes every note in
    /// flight, so the chain releases what it has sounding into `out` at
    /// `offset` and starts again from the next note.
    pub fn set_enabled(&mut self, index: usize, enabled: bool, offset: u32, out: &mut EventBuffer) {
        let changed = self
            .devices
            .get(index)
            .is_some_and(|slot| slot.enabled != enabled);
        if changed {
            self.release(offset, out);
            self.devices[index].enabled = enabled;
        }
    }

    pub fn is_enabled(&self, index: usize) -> bool {
        self.devices.get(index).is_some_and(|s| s.enabled)
    }

    /// Run the chain on `io` in place.
    pub fn process(&mut self, ctx: &Context, io: &mut EventBuffer) {
        if self.devices.iter().all(|s| !s.enabled) {
            return;
        }
        self.a.copy_from(io);
        for slot in self.devices.iter_mut().filter(|s| s.enabled) {
            slot.device.process(ctx, &self.a, &mut self.b);
            std::mem::swap(&mut self.a, &mut self.b);
        }
        io.copy_from(&self.a);
    }

    /// End every note the chain has sounding (into `out` at `offset`) and
    /// reset every device. Only the last enabled device's note-offs reach
    /// `out`: what the others sounded was input to a later device, never
    /// heard by the instrument, and that device tracks what it sent on.
    pub fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        let last = self.devices.iter().rposition(|s| s.enabled);
        for (i, slot) in self.devices.iter_mut().enumerate() {
            if Some(i) == last {
                slot.device.release(offset, out);
            } else {
                self.b.clear();
                slot.device.release(offset, &mut self.b);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_its_id_and_has_sane_params() {
        for kind in DeviceKind::ALL {
            assert_eq!(DeviceKind::from_id(kind.id()), Some(kind));
            let params = kind.params();
            assert!(params.len() <= MAX_PARAMS, "{kind:?}");
            let device = kind.create();
            for (i, p) in params.iter().enumerate() {
                assert!(
                    p.min <= p.default && p.default <= p.max,
                    "{kind:?}.{}",
                    p.id
                );
                assert_eq!(device.param(i), p.default, "{kind:?}.{}", p.id);
                assert!(p.label.starts_with("midi-fx.param."), "{}", p.label);
                assert_eq!(kind.param_index(p.id), Some(i));
            }
            let mut ids: Vec<_> = params.iter().map(|p| p.id).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), params.len(), "{kind:?} has duplicate param ids");
        }
    }

    #[test]
    fn dependent_params_follow_the_setting_they_depend_on() {
        // Reads only ids the device has, so a rename cannot silently leave a
        // knob dimmed for good.
        let reader = |kind: DeviceKind, set: &'static [(&'static str, f32)]| {
            move |id: &str| {
                let index = kind
                    .param_index(id)
                    .unwrap_or_else(|| panic!("{kind:?}.{id}"));
                set.iter()
                    .find(|(k, _)| *k == id)
                    .map_or(kind.params()[index].default, |&(_, v)| v)
            }
        };
        let chord = DeviceKind::Chord;
        assert!(!chord.param_active("interval1", reader(chord, &[])));
        assert!(chord.param_active("interval3", reader(chord, &[("shape", 0.0)])));
        let arp = DeviceKind::Arpeggiator;
        assert!(!arp.param_active("velocity", reader(arp, &[])));
        assert!(arp.param_active("velocity", reader(arp, &[("velocity-mode", 1.0)])));
        let pattern = DeviceKind::PatternGenerator;
        assert!(pattern.param_active("note", reader(pattern, &[])));
        assert!(!pattern.param_active("note", reader(pattern, &[("source", 1.0)])));
        assert!(pattern.param_active("steps", reader(pattern, &[("source", 1.0)])));
    }

    #[test]
    fn a_chain_feeds_each_device_the_previous_output() {
        let mut chain = Chain::new();
        let mut transpose = DeviceKind::Transpose.create();
        transpose.set_param(0, 12.0);
        chain.push(transpose, true);
        let mut chord = DeviceKind::Chord.create();
        let shape = DeviceKind::Chord.param_index("shape").unwrap();
        chord.set_param(shape, 1.0); // major
        chain.push(chord, true);
        let ctx = Context {
            sample_rate: 48_000.0,
            frames: 256,
            tempo_bpm: 120.0,
            ppq: 0.0,
            playing: true,
        };
        let mut io = EventBuffer::new();
        io.push(MidiEvent::note_on(0, 0, 48, 0.8));
        chain.process(&ctx, &mut io);
        let keys: Vec<_> = io.as_slice().iter().map(|e| e.data).collect();
        assert_eq!(keys, vec![60, 64, 67]);
        let mut out = EventBuffer::new();
        chain.release(5, &mut out);
        assert_eq!(out.len(), 3);
    }
}
