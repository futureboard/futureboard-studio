//! WrapSynth's modulation: what can move what, and by how much.
//!
//! A fixed matrix of [`MOD_SLOTS`] slots, each one source, one destination
//! and a signed amount. Every destination moves in its own knob's space — an
//! amount of 1 sweeps the whole knob, as in the editor — except the pitches,
//! which move in semitones ([`PITCH_MOD_SEMITONES`] at full amount), and the
//! pan, which moves across the whole field.
//!
//! Everything here is plain data: the voices read the active slots per sample
//! with no allocation or lookup.

use serde::{Deserialize, Serialize};

pub const MOD_SLOTS: usize = 8;
/// A pitch destination at full amount moves this far, up or down.
pub const PITCH_MOD_SEMITONES: f32 = 24.0;

/// What modulates. Envelopes, velocity, the mod wheel and aftertouch are
/// unipolar (0..1); the LFOs, the played note (centred on middle C) and the
/// per-note random value are bipolar (−1..1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ModSource {
    #[default]
    None,
    /// The amp envelope.
    Env1,
    /// The filter envelope.
    Env2,
    /// The modulation envelope.
    Env3,
    Lfo1,
    Lfo2,
    Velocity,
    ModWheel,
    Aftertouch,
    Note,
    /// A value drawn once per note.
    Random,
}

impl ModSource {
    pub const ALL: [ModSource; 11] = [
        Self::None,
        Self::Env1,
        Self::Env2,
        Self::Env3,
        Self::Lfo1,
        Self::Lfo2,
        Self::Velocity,
        Self::ModWheel,
        Self::Aftertouch,
        Self::Note,
        Self::Random,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn to_wire(self) -> f32 {
        self.index() as f32
    }

    pub fn from_wire(value: f32) -> Self {
        let index = value.round().clamp(0.0, (Self::COUNT - 1) as f32) as usize;
        Self::ALL[index]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Env1 => "Amp Env",
            Self::Env2 => "Filter Env",
            Self::Env3 => "Mod Env",
            Self::Lfo1 => "LFO 1",
            Self::Lfo2 => "LFO 2",
            Self::Velocity => "Velocity",
            Self::ModWheel => "Mod Wheel",
            Self::Aftertouch => "Aftertouch",
            Self::Note => "Note",
            Self::Random => "Random",
        }
    }
}

/// What is modulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ModDest {
    #[default]
    None,
    OscAPitch,
    OscAPosition,
    OscALevel,
    OscBPitch,
    OscBPosition,
    OscBLevel,
    SubLevel,
    NoiseLevel,
    FilterCutoff,
    FilterResonance,
    FilterDrive,
    UnisonDetune,
    Amp,
    Pan,
}

impl ModDest {
    pub const ALL: [ModDest; 15] = [
        Self::None,
        Self::OscAPitch,
        Self::OscAPosition,
        Self::OscALevel,
        Self::OscBPitch,
        Self::OscBPosition,
        Self::OscBLevel,
        Self::SubLevel,
        Self::NoiseLevel,
        Self::FilterCutoff,
        Self::FilterResonance,
        Self::FilterDrive,
        Self::UnisonDetune,
        Self::Amp,
        Self::Pan,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn to_wire(self) -> f32 {
        self.index() as f32
    }

    pub fn from_wire(value: f32) -> Self {
        let index = value.round().clamp(0.0, (Self::COUNT - 1) as f32) as usize;
        Self::ALL[index]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::OscAPitch => "Osc A Pitch",
            Self::OscAPosition => "Osc A WT Pos",
            Self::OscALevel => "Osc A Level",
            Self::OscBPitch => "Osc B Pitch",
            Self::OscBPosition => "Osc B WT Pos",
            Self::OscBLevel => "Osc B Level",
            Self::SubLevel => "Sub Level",
            Self::NoiseLevel => "Noise Level",
            Self::FilterCutoff => "Cutoff",
            Self::FilterResonance => "Resonance",
            Self::FilterDrive => "Drive",
            Self::UnisonDetune => "Unison Detune",
            Self::Amp => "Amp",
            Self::Pan => "Pan",
        }
    }
}

/// One row of the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModSlot {
    pub source: ModSource,
    pub dest: ModDest,
    /// −1..1.
    pub amount: f32,
}

impl ModSlot {
    pub const EMPTY: Self = Self {
        source: ModSource::None,
        dest: ModDest::None,
        amount: 0.0,
    };

    pub fn is_active(&self) -> bool {
        self.source != ModSource::None && self.dest != ModDest::None && self.amount != 0.0
    }
}

/// The active rows of a matrix, packed to the front so the voices loop over
/// exactly the routes that do something.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveRoutes {
    pub slots: [ModSlot; MOD_SLOTS],
    pub count: usize,
}

impl ActiveRoutes {
    pub fn new(slots: &[ModSlot; MOD_SLOTS]) -> Self {
        let mut active = Self {
            slots: [ModSlot::EMPTY; MOD_SLOTS],
            count: 0,
        };
        for slot in slots.iter().filter(|slot| slot.is_active()) {
            active.slots[active.count] = *slot;
            active.count += 1;
        }
        active
    }

    pub fn routes(&self) -> &[ModSlot] {
        &self.slots[..self.count]
    }
}

/// One voice's modulation sources at one sample, indexed by
/// [`ModSource::index`].
pub type SourceValues = [f32; ModSource::COUNT];
/// What the matrix adds to every destination, indexed by [`ModDest::index`].
pub type DestOffsets = [f32; ModDest::COUNT];

/// Sum every active route into per-destination offsets. Allocation-free.
#[inline]
pub fn apply_routes(routes: &ActiveRoutes, sources: &SourceValues) -> DestOffsets {
    let mut offsets = [0.0; ModDest::COUNT];
    for slot in routes.routes() {
        offsets[slot.dest.index()] += sources[slot.source.index()] * slot.amount;
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_values_round_trip_and_clamp() {
        for source in ModSource::ALL {
            assert_eq!(ModSource::from_wire(source.to_wire()), source);
        }
        for dest in ModDest::ALL {
            assert_eq!(ModDest::from_wire(dest.to_wire()), dest);
        }
        assert_eq!(ModDest::from_wire(99.0), ModDest::Pan);
        assert_eq!(ModSource::from_wire(-3.0), ModSource::None);
    }

    #[test]
    fn only_complete_routes_are_active_and_they_sum() {
        let mut slots = [ModSlot::EMPTY; MOD_SLOTS];
        slots[0] = ModSlot {
            source: ModSource::Lfo1,
            dest: ModDest::FilterCutoff,
            amount: 0.5,
        };
        slots[3] = ModSlot {
            source: ModSource::Velocity,
            dest: ModDest::FilterCutoff,
            amount: -0.25,
        };
        // Incomplete: no destination, or no amount.
        slots[5] = ModSlot {
            source: ModSource::Env3,
            dest: ModDest::None,
            amount: 1.0,
        };
        slots[6] = ModSlot {
            source: ModSource::Env3,
            dest: ModDest::Amp,
            amount: 0.0,
        };
        let routes = ActiveRoutes::new(&slots);
        assert_eq!(routes.count, 2);
        let mut sources = [0.0; ModSource::COUNT];
        sources[ModSource::Lfo1.index()] = -1.0;
        sources[ModSource::Velocity.index()] = 1.0;
        let offsets = apply_routes(&routes, &sources);
        assert!((offsets[ModDest::FilterCutoff.index()] + 0.75).abs() < 1.0e-6);
        assert_eq!(offsets[ModDest::Amp.index()], 0.0);
    }
}
