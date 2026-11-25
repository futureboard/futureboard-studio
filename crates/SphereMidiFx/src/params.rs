//! Parameter descriptions: what the UI draws, what a project saves, what
//! automation maps onto. Everything here is `'static` so the audio thread
//! never builds or frees a description.

/// How a parameter's value is shown and stepped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    /// A continuous value in `min..=max`.
    Continuous,
    /// A whole number in `min..=max`.
    Integer,
    /// One of `labels` (localization keys); the value is the index.
    Choice(&'static [&'static str]),
    /// Off (`0.0`) or on (`1.0`).
    Toggle,
}

/// The unit a value is shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    None,
    Percent,
    Semitones,
    Milliseconds,
    /// A MIDI note number (shown as a name, e.g. C3).
    Note,
    /// A MIDI value `0..=127`.
    Midi,
}

/// One parameter of a device.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamDesc {
    /// Stable identifier, saved in projects. Never rename one.
    pub id: &'static str,
    /// Localization key of the label (`midi-fx.param.<id>`).
    pub label: &'static str,
    pub kind: ParamKind,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: Unit,
}

impl ParamDesc {
    pub const fn continuous(
        id: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: Unit,
    ) -> Self {
        Self {
            id,
            label,
            kind: ParamKind::Continuous,
            min,
            max,
            default,
            unit,
        }
    }

    pub const fn integer(
        id: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: Unit,
    ) -> Self {
        Self {
            id,
            label,
            kind: ParamKind::Integer,
            min,
            max,
            default,
            unit,
        }
    }

    pub const fn choice(
        id: &'static str,
        label: &'static str,
        labels: &'static [&'static str],
        default: usize,
    ) -> Self {
        Self {
            id,
            label,
            kind: ParamKind::Choice(labels),
            min: 0.0,
            max: (labels.len() - 1) as f32,
            default: default as f32,
            unit: Unit::None,
        }
    }

    pub const fn toggle(id: &'static str, label: &'static str, default: bool) -> Self {
        Self {
            id,
            label,
            kind: ParamKind::Toggle,
            min: 0.0,
            max: 1.0,
            default: if default { 1.0 } else { 0.0 },
            unit: Unit::None,
        }
    }

    /// `value` brought into range and onto this kind's steps.
    pub fn clamp(&self, value: f32) -> f32 {
        let v = if value.is_finite() {
            value
        } else {
            self.default
        };
        let v = v.clamp(self.min, self.max);
        match self.kind {
            ParamKind::Continuous => v,
            ParamKind::Integer | ParamKind::Choice(_) => v.round(),
            ParamKind::Toggle => {
                if v >= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// `value` as `0.0..=1.0` (for automation lanes).
    pub fn normalize(&self, value: f32) -> f32 {
        if self.max <= self.min {
            0.0
        } else {
            ((self.clamp(value) - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
        }
    }

    /// A `0.0..=1.0` value back in this parameter's range.
    pub fn denormalize(&self, normalized: f32) -> f32 {
        self.clamp(self.min + normalized.clamp(0.0, 1.0) * (self.max - self.min))
    }
}

/// Note values shared by every tempo-synced device, in quarter notes.
pub const RATE_LABELS: &[&str] = &[
    "midi-fx.rate.1-1",
    "midi-fx.rate.1-2",
    "midi-fx.rate.1-2d",
    "midi-fx.rate.1-2t",
    "midi-fx.rate.1-4",
    "midi-fx.rate.1-4d",
    "midi-fx.rate.1-4t",
    "midi-fx.rate.1-8",
    "midi-fx.rate.1-8d",
    "midi-fx.rate.1-8t",
    "midi-fx.rate.1-16",
    "midi-fx.rate.1-16d",
    "midi-fx.rate.1-16t",
    "midi-fx.rate.1-32",
    "midi-fx.rate.1-32t",
];

/// Length in quarter notes of [`RATE_LABELS`]`[index]`.
pub fn rate_beats(index: f32) -> f64 {
    const BEATS: [f64; 15] = [
        4.0,
        2.0,
        3.0,
        4.0 / 3.0,
        1.0,
        1.5,
        2.0 / 3.0,
        0.5,
        0.75,
        1.0 / 3.0,
        0.25,
        0.375,
        1.0 / 6.0,
        0.125,
        1.0 / 12.0,
    ];
    let i = (index.max(0.0).round() as usize).min(BEATS.len() - 1);
    BEATS[i]
}

/// Index of 1/16 in [`RATE_LABELS`], the usual default.
pub const RATE_SIXTEENTH: usize = 10;

/// Scales shared by Scale and the Random Melody generator.
pub const SCALE_LABELS: &[&str] = &[
    "midi-fx.scale.major",
    "midi-fx.scale.natural-minor",
    "midi-fx.scale.harmonic-minor",
    "midi-fx.scale.melodic-minor",
    "midi-fx.scale.dorian",
    "midi-fx.scale.phrygian",
    "midi-fx.scale.lydian",
    "midi-fx.scale.mixolydian",
    "midi-fx.scale.locrian",
    "midi-fx.scale.major-pentatonic",
    "midi-fx.scale.minor-pentatonic",
    "midi-fx.scale.blues",
    "midi-fx.scale.chromatic",
];

/// Pitch-class membership of [`SCALE_LABELS`]`[index]`, bit `n` = semitone
/// `n` above the root.
pub fn scale_mask(index: f32) -> u16 {
    const fn mask(steps: &[u8]) -> u16 {
        let mut m = 0u16;
        let mut i = 0;
        while i < steps.len() {
            m |= 1 << steps[i];
            i += 1;
        }
        m
    }
    const MASKS: [u16; 13] = [
        mask(&[0, 2, 4, 5, 7, 9, 11]),
        mask(&[0, 2, 3, 5, 7, 8, 10]),
        mask(&[0, 2, 3, 5, 7, 8, 11]),
        mask(&[0, 2, 3, 5, 7, 9, 11]),
        mask(&[0, 2, 3, 5, 7, 9, 10]),
        mask(&[0, 1, 3, 5, 7, 8, 10]),
        mask(&[0, 2, 4, 6, 7, 9, 11]),
        mask(&[0, 2, 4, 5, 7, 9, 10]),
        mask(&[0, 1, 3, 5, 6, 8, 10]),
        mask(&[0, 2, 4, 7, 9]),
        mask(&[0, 3, 5, 7, 10]),
        mask(&[0, 3, 5, 6, 7, 10]),
        mask(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
    ];
    let i = (index.max(0.0).round() as usize).min(MASKS.len() - 1);
    MASKS[i]
}

/// Note names for a root choice.
pub const ROOT_LABELS: &[&str] = &[
    "midi-fx.note.c",
    "midi-fx.note.c-sharp",
    "midi-fx.note.d",
    "midi-fx.note.d-sharp",
    "midi-fx.note.e",
    "midi-fx.note.f",
    "midi-fx.note.f-sharp",
    "midi-fx.note.g",
    "midi-fx.note.g-sharp",
    "midi-fx.note.a",
    "midi-fx.note.a-sharp",
    "midi-fx.note.b",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_steps_by_kind() {
        let i = ParamDesc::integer("x", "x", -12.0, 12.0, 0.0, Unit::Semitones);
        assert_eq!(i.clamp(3.6), 4.0);
        assert_eq!(i.clamp(99.0), 12.0);
        assert_eq!(i.clamp(f32::NAN), 0.0);
        let t = ParamDesc::toggle("t", "t", false);
        assert_eq!(t.clamp(0.7), 1.0);
        let c = ParamDesc::choice("c", "c", RATE_LABELS, RATE_SIXTEENTH);
        assert_eq!(c.max, 14.0);
        assert_eq!(c.denormalize(c.normalize(5.0)), 5.0);
    }

    #[test]
    fn rates_and_scales() {
        assert_eq!(rate_beats(RATE_SIXTEENTH as f32), 0.25);
        assert_eq!(rate_beats(7.0), 0.5);
        assert_eq!(RATE_LABELS.len(), 15);
        assert_eq!(scale_mask(0.0).count_ones(), 7);
        assert_eq!(scale_mask(12.0), 0x0FFF);
        assert_eq!(SCALE_LABELS.len(), 13);
    }
}
