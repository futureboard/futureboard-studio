//! WrapSynth's modulation: what can move what, and by how much.
//!
//! A fixed matrix of [`MOD_SLOTS`] slots, each one source, one destination
//! and a signed amount. Every destination is moved in its own knob's space —
//! an amount of 1 sweeps the whole knob, as in the editor — except the
//! pitches, which move in semitones ([`PITCH_MOD_SEMITONES`] at full amount).
//! Everything here is plain data: the voices read it per sample with no
//! allocation or lookup.

use serde::{Deserialize, Serialize};

use crate::Params;

pub const MOD_SLOTS: usize = 8;
/// A pitch destination at full amount moves this far, up or down.
pub const PITCH_MOD_SEMITONES: f32 = 24.0;
/// Where a drag-and-drop assignment starts.
pub const DEFAULT_AMOUNT: f32 = 0.5;

/// What modulates. Envelopes, velocity and the mod wheel are unipolar
/// (0..1); the LFOs and the played note are bipolar (−1..1, the note
/// centred on middle C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ModSource {
    #[default]
    None,
    Env1,
    Env2,
    Env3,
    Lfo1,
    Lfo2,
    Velocity,
    ModWheel,
    Note,
}

impl ModSource {
    pub const ALL: [ModSource; 9] = [
        Self::None,
        Self::Env1,
        Self::Env2,
        Self::Env3,
        Self::Lfo1,
        Self::Lfo2,
        Self::Velocity,
        Self::ModWheel,
        Self::Note,
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
            Self::Env1 => "Env 1",
            Self::Env2 => "Env 2",
            Self::Env3 => "Env 3",
            Self::Lfo1 => "LFO 1",
            Self::Lfo2 => "LFO 2",
            Self::Velocity => "Velocity",
            Self::ModWheel => "Mod Wheel",
            Self::Note => "Note",
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
    OscAPan,
    OscBPitch,
    OscBPosition,
    OscBLevel,
    OscBPan,
    SubLevel,
    NoiseLevel,
    FilterCutoff,
    FilterResonance,
    FilterDrive,
    FilterMix,
    UnisonDetune,
    Amp,
    // Added with the third oscillator, the warps and the second filter;
    // appended, so saved matrices keep their meaning.
    OscCPitch,
    OscCPosition,
    OscCLevel,
    OscCPan,
    OscAWarp,
    OscBWarp,
    OscCWarp,
    Filter2Cutoff,
    Filter2Resonance,
    Filter2Drive,
    Filter2Mix,
}

impl ModDest {
    pub const ALL: [ModDest; 28] = [
        Self::None,
        Self::OscAPitch,
        Self::OscAPosition,
        Self::OscALevel,
        Self::OscAPan,
        Self::OscBPitch,
        Self::OscBPosition,
        Self::OscBLevel,
        Self::OscBPan,
        Self::SubLevel,
        Self::NoiseLevel,
        Self::FilterCutoff,
        Self::FilterResonance,
        Self::FilterDrive,
        Self::FilterMix,
        Self::UnisonDetune,
        Self::Amp,
        Self::OscCPitch,
        Self::OscCPosition,
        Self::OscCLevel,
        Self::OscCPan,
        Self::OscAWarp,
        Self::OscBWarp,
        Self::OscCWarp,
        Self::Filter2Cutoff,
        Self::Filter2Resonance,
        Self::Filter2Drive,
        Self::Filter2Mix,
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
            Self::OscAPan => "Osc A Pan",
            Self::OscBPitch => "Osc B Pitch",
            Self::OscBPosition => "Osc B WT Pos",
            Self::OscBLevel => "Osc B Level",
            Self::OscBPan => "Osc B Pan",
            Self::SubLevel => "Sub Level",
            Self::NoiseLevel => "Noise Level",
            Self::FilterCutoff => "Filter 1 Cutoff",
            Self::FilterResonance => "Filter 1 Res",
            Self::FilterDrive => "Filter 1 Drive",
            Self::FilterMix => "Filter 1 Mix",
            Self::UnisonDetune => "Unison Detune",
            Self::Amp => "Amp",
            Self::OscCPitch => "Osc C Pitch",
            Self::OscCPosition => "Osc C WT Pos",
            Self::OscCLevel => "Osc C Level",
            Self::OscCPan => "Osc C Pan",
            Self::OscAWarp => "Osc A Warp",
            Self::OscBWarp => "Osc B Warp",
            Self::OscCWarp => "Osc C Warp",
            Self::Filter2Cutoff => "Filter 2 Cutoff",
            Self::Filter2Resonance => "Filter 2 Res",
            Self::Filter2Drive => "Filter 2 Drive",
            Self::Filter2Mix => "Filter 2 Mix",
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
    pub fn is_active(&self) -> bool {
        self.source != ModSource::None && self.dest != ModDest::None && self.amount != 0.0
    }
}

/// Routes `source` to `dest`: the slot already joining them, or the first
/// empty one, set to [`DEFAULT_AMOUNT`]. `None` when every slot is in use.
pub fn assign(params: &mut Params, source: ModSource, dest: ModDest) -> Option<usize> {
    if source == ModSource::None || dest == ModDest::None {
        return None;
    }
    let slots = &mut params.mod_slots;
    if let Some(index) = slots
        .iter()
        .position(|slot| slot.source == source && slot.dest == dest)
    {
        return Some(index);
    }
    let index = slots
        .iter()
        .position(|slot| slot.source == ModSource::None || slot.dest == ModDest::None)?;
    slots[index] = ModSlot {
        source,
        dest,
        amount: DEFAULT_AMOUNT,
    };
    Some(index)
}

/// The slots that move `dest`, as `(slot, row)`.
pub fn routes_to(params: &Params, dest: ModDest) -> Vec<(usize, ModSlot)> {
    params
        .mod_slots
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, slot)| slot.dest == dest && slot.source != ModSource::None)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_values_round_trip() {
        for source in ModSource::ALL {
            assert_eq!(ModSource::from_wire(source.to_wire()), source);
        }
        for dest in ModDest::ALL {
            assert_eq!(ModDest::from_wire(dest.to_wire()), dest);
        }
        assert_eq!(ModDest::from_wire(99.0), ModDest::Filter2Mix);
        assert_eq!(
            ModDest::from_wire(16.0),
            ModDest::Amp,
            "old indices keep their meaning"
        );
        assert_eq!(ModSource::from_wire(-3.0), ModSource::None);
    }

    #[test]
    fn assigning_reuses_a_route_then_fills_empty_slots() {
        let mut params = crate::default_params();
        let first = assign(&mut params, ModSource::Lfo1, ModDest::FilterCutoff);
        assert_eq!(first, Some(0));
        assert_eq!(params.mod_slots[0].amount, DEFAULT_AMOUNT);
        params.mod_slots[0].amount = -0.2;
        // The same route again: the slot it has, amount untouched.
        assert_eq!(
            assign(&mut params, ModSource::Lfo1, ModDest::FilterCutoff),
            Some(0)
        );
        assert_eq!(params.mod_slots[0].amount, -0.2);
        assert_eq!(
            assign(&mut params, ModSource::Env2, ModDest::FilterCutoff),
            Some(1)
        );
        assert_eq!(routes_to(&params, ModDest::FilterCutoff).len(), 2);
        for index in 2..MOD_SLOTS {
            params.mod_slots[index] = ModSlot {
                source: ModSource::Velocity,
                dest: ModDest::Amp,
                amount: 0.1,
            };
        }
        assert_eq!(
            assign(&mut params, ModSource::Note, ModDest::OscAPitch),
            None
        );
        assert_eq!(assign(&mut params, ModSource::None, ModDest::Amp), None);
    }
}
