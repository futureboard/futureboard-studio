//! The knob description the built-in editors share: a param's range, taper,
//! unit and readout ([`KnobSpec`]), and how values round and print.
//!
//! No GPUI here.

// ── Knobs ───────────────────────────────────────────────────────────────────

/// How a knob's travel maps onto its range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Taper {
    Linear,
    /// Equal ratios per equal turn: frequencies, times, decays.
    Log,
    /// Fine at the bottom: pre-delay, where the first milliseconds matter
    /// most.
    Square,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    Ms,
    Sec,
    Hz,
    Percent,
    Db,
    Times,
    /// A note division, by its index into `echospace::DIVISION_LABELS`.
    Division,
    /// Whole semitones.
    Semitones,
    Cents,
    /// A depth either side, in cents.
    CentsDepth,
    /// Microseconds.
    Us,
    /// A compression ratio, `n:1`.
    Ratio,
    /// A bare number, no unit.
    Plain,
    /// A sidechain filter in Hz, off at or below the threshold given.
    CutHz(f32),
}

/// One knob: which param, its label, range, taper and readout.
#[derive(Clone, Copy, Debug)]
pub struct KnobSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub taper: Taper,
    pub unit: Unit,
    /// Drawn from `centre` outward rather than from the bottom.
    pub bipolar: bool,
    /// The value a bipolar knob's arc starts from.
    pub centre: f32,
}

pub(crate) const fn spec(
    id: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    taper: Taper,
    unit: Unit,
) -> KnobSpec {
    KnobSpec {
        id,
        label,
        min,
        max,
        taper,
        unit,
        bipolar: false,
        centre: 0.0,
    }
}

impl KnobSpec {
    /// The knob's position, 0–1, for `value`.
    pub fn fraction(&self, value: f32) -> f32 {
        let v = value.clamp(self.min, self.max);
        match self.taper {
            Taper::Linear => (v - self.min) / (self.max - self.min),
            Taper::Log => (v / self.min).ln() / (self.max / self.min).ln(),
            Taper::Square => ((v - self.min) / (self.max - self.min)).sqrt(),
        }
    }

    /// The value at knob position `fraction`, rounded to what the readout
    /// shows.
    pub fn value_at(&self, fraction: f32) -> f32 {
        let f = fraction.clamp(0.0, 1.0);
        let raw = match self.taper {
            Taper::Linear => self.min + f * (self.max - self.min),
            Taper::Log => self.min * (self.max / self.min).powf(f),
            Taper::Square => self.min + f * f * (self.max - self.min),
        };
        round_for(self.unit, raw)
    }

    /// The span the knob widget runs over: 0–1 through the taper for a
    /// unipolar knob; the raw range, shifted so `centre` is zero, for a
    /// bipolar one (its arc starts at zero).
    pub fn knob_range(&self) -> (f32, f32) {
        if self.bipolar {
            (self.min - self.centre, self.max - self.centre)
        } else {
            (0.0, 1.0)
        }
    }

    /// `value` in the knob widget's units.
    pub fn to_knob(&self, value: f32) -> f32 {
        if self.bipolar {
            value.clamp(self.min, self.max) - self.centre
        } else {
            self.fraction(value)
        }
    }

    /// The param value at the knob widget's `units`.
    pub fn from_knob(&self, units: f32) -> f32 {
        if self.bipolar {
            round_for(self.unit, (units + self.centre).clamp(self.min, self.max))
        } else {
            self.value_at(units)
        }
    }

    pub fn readout(&self, value: f32) -> String {
        format_value(self.unit, value)
    }
}

/// `value` rounded to its readout's resolution, so a knob never leaves a
/// value behind that the readout cannot show.
pub fn round_for(unit: Unit, value: f32) -> f32 {
    let step = match unit {
        Unit::Ms if value < 1.0 => 0.01,
        Unit::Ms if value < 10.0 => 0.1,
        Unit::Ms => 1.0,
        Unit::Sec if value < 10.0 => 0.01,
        Unit::Sec => 0.1,
        Unit::Hz if value < 1.0 => 0.01,
        // The readout shows tenths below 10 Hz; a whole-hertz step would
        // snap a 5.5 Hz rate to 6.
        Unit::Hz if value < 10.0 => 0.1,
        Unit::Hz if value < 1_000.0 => 1.0,
        Unit::Hz => 10.0,
        Unit::Percent => 1.0,
        Unit::Db => 0.1,
        Unit::Times => 0.01,
        Unit::Division | Unit::Semitones | Unit::Cents | Unit::CentsDepth => 1.0,
        Unit::Us | Unit::Plain | Unit::CutHz(_) => 1.0,
        Unit::Ratio if value < 10.0 => 0.1,
        Unit::Ratio => 0.5,
    };
    (value / step).round() * step
}

pub fn format_value(unit: Unit, value: f32) -> String {
    match unit {
        Unit::Ms if value >= 1_000.0 => format!("{:.2} s", value / 1_000.0),
        Unit::Ms if value < 1.0 => format!("{value:.2} ms"),
        Unit::Ms if value < 10.0 => format!("{value:.1} ms"),
        Unit::Ms => format!("{value:.0} ms"),
        Unit::Sec if value.is_infinite() => "∞".to_string(),
        Unit::Sec if value < 10.0 => format!("{value:.2} s"),
        Unit::Sec => format!("{value:.1} s"),
        Unit::Hz if value < 1.0 => format!("{value:.2} Hz"),
        Unit::Hz if value < 10.0 => format!("{value:.1} Hz"),
        Unit::Hz if value < 1_000.0 => format!("{value:.0} Hz"),
        Unit::Hz => format!("{:.1} kHz", value / 1_000.0),
        Unit::Percent => format!("{value:.0} %"),
        Unit::Db if value.abs() < 0.05 => "0.0 dB".to_string(),
        Unit::Db => format!("{value:+.1} dB"),
        Unit::Times => format!("{value:.2}×"),
        Unit::Semitones if value.abs() < 0.5 => "0 st".to_string(),
        Unit::Semitones => format!("{value:+.0} st"),
        Unit::Cents if value.abs() < 0.5 => "0 ¢".to_string(),
        Unit::Cents => format!("{value:+.0} ¢"),
        Unit::CentsDepth => format!("±{value:.0} ¢"),
        Unit::Us => format!("{value:.0} µs"),
        Unit::Ratio if value < 10.0 => format!("{value:.1}:1"),
        Unit::Ratio => format!("{value:.0}:1"),
        Unit::Plain => format!("{value:.0}"),
        Unit::CutHz(off) if value <= off => "Off".to_string(),
        Unit::CutHz(_) => format_value(Unit::Hz, value),
        Unit::Division => echospace::DIVISION_LABELS
            .get(value.round().clamp(0.0, echospace::MAX_DIVISION_WIRE) as usize)
            .copied()
            .unwrap_or("—")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readouts_read_like_a_studio() {
        assert_eq!(format_value(Unit::Ms, 375.0), "375 ms");
        assert_eq!(format_value(Unit::Ms, 1_250.0), "1.25 s");
        assert_eq!(format_value(Unit::Hz, 9_500.0), "9.5 kHz");
        assert_eq!(format_value(Unit::Db, -1.5), "-1.5 dB");
        assert_eq!(format_value(Unit::Db, 0.01), "0.0 dB");
        assert_eq!(format_value(Unit::Division, 9.0), "1/8.");
        assert_eq!(format_value(Unit::Sec, f32::INFINITY), "∞");
        assert_eq!(format_value(Unit::Ms, 0.25), "0.25 ms");
        assert_eq!(round_for(Unit::Ms, 0.013), 0.01);
        assert_eq!(format_value(Unit::Ratio, 4.0), "4.0:1");
        assert_eq!(format_value(Unit::Ratio, 20.0), "20:1");
        assert_eq!(format_value(Unit::CutHz(20.0), 0.0), "Off");
        assert_eq!(format_value(Unit::CutHz(20.0), 90.0), "90 Hz");
        assert_eq!(format_value(Unit::Us, 400.0), "400 µs");
    }
}
