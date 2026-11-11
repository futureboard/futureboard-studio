use super::{MpeZone, MpeZoneKind};
use serde::{Deserialize, Serialize};

/// How a track should route note-owned expression to an MPE destination.
///
/// `Auto` keeps the backwards-compatible behaviour: ordinary notes stay on
/// their stored channel until a note contains expression data, at which point
/// the lower zone is used. The explicit zone modes also route unmarked notes
/// through their member channels, which is useful for MPE instruments whose
/// per-note channel assignment is part of the performance contract.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum MpeOutputMode {
    Off,
    #[default]
    Auto,
    Lower,
    Upper,
}

impl MpeOutputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Auto => "Auto Detect",
            Self::Lower => "Lower Zone",
            Self::Upper => "Upper Zone",
        }
    }

    pub const fn to_tag(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Auto => 1,
            Self::Lower => 2,
            Self::Upper => 3,
        }
    }

    pub const fn from_tag(tag: u8) -> Self {
        match tag {
            0 => Self::Off,
            2 => Self::Lower,
            3 => Self::Upper,
            _ => Self::Auto,
        }
    }
}

/// Persisted per-track MPE output settings. This intentionally lives in the
/// MIDI service crate so the UI, realtime engine, and future MIDI 2.0 output
/// adapters share the same zone semantics.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MpeTrackConfiguration {
    pub mode: MpeOutputMode,
    pub member_channels: u8,
    pub member_pitch_range: f32,
    pub manager_pitch_range: f32,
}

impl Default for MpeTrackConfiguration {
    fn default() -> Self {
        Self {
            mode: MpeOutputMode::Auto,
            member_channels: 15,
            member_pitch_range: 2.0,
            manager_pitch_range: 2.0,
        }
    }
}

impl MpeTrackConfiguration {
    pub fn sanitized(self) -> Self {
        let finite_range = |value: f32| {
            if value.is_finite() {
                value.clamp(1.0, 96.0)
            } else {
                2.0
            }
        };
        Self {
            member_channels: self.member_channels.clamp(1, 15),
            member_pitch_range: finite_range(self.member_pitch_range),
            manager_pitch_range: finite_range(self.manager_pitch_range),
            ..self
        }
    }

    /// Resolve the selected output mode into a wire-level zone. `Auto` uses a
    /// lower zone when the caller has established that expression is present.
    pub fn zone(self) -> Option<MpeZone> {
        let config = self.sanitized();
        let mut zone = match config.mode {
            MpeOutputMode::Off => return None,
            MpeOutputMode::Auto | MpeOutputMode::Lower => MpeZone::lower(config.member_channels),
            MpeOutputMode::Upper => MpeZone::upper(config.member_channels),
        };
        zone.member_pitch_range = config.member_pitch_range;
        zone.manager_pitch_range = config.manager_pitch_range;
        Some(zone)
    }

    pub fn should_use_mpe(self, has_expression: bool) -> bool {
        match self.mode {
            MpeOutputMode::Off => false,
            MpeOutputMode::Auto => has_expression,
            MpeOutputMode::Lower | MpeOutputMode::Upper => true,
        }
    }
}

/// A decoded MPE configuration message.  It is protocol metadata, not note
/// data, and can be surfaced by device detection or a track inspector.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MpeConfiguration {
    pub zone: MpeZoneKind,
    pub member_channels: u8,
    pub member_pitch_range: f32,
    pub manager_pitch_range: f32,
}

impl MpeConfiguration {
    pub fn from_zone(zone: MpeZone) -> Self {
        Self {
            zone: zone.kind,
            member_channels: zone.member_channels().len().min(15) as u8,
            member_pitch_range: zone.member_pitch_range,
            manager_pitch_range: zone.manager_pitch_range,
        }
    }
}
