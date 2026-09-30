//! Multitimbral mode: one SoundFont, sixteen MIDI channels, each its own part.
//!
//! In [`SoundfontPlayerMode::Single`] the player is one instrument — one preset
//! on every channel, drum kits routed to channel 10 (see
//! [`crate::SoundfontPlayer::routed_channel`]). In
//! [`SoundfontPlayerMode::Multi`] the channels are independent, the way a
//! General MIDI module is played: every channel keeps the notes that arrive on
//! it and has its own preset, level, pan, mute and solo. Several MIDI tracks
//! routed to one instrument track, each on its own channel, then play the
//! parts of one arrangement from one font.
//!
//! Level and pan are the channel's own MIDI controllers (CC 7 and CC 10), so a
//! controller lane on a part moves the same value the channel strip set.

use serde::{Deserialize, Serialize};

/// MIDI channels a player has.
pub const MIDI_CHANNELS: usize = 16;

/// The default channel level, the value General MIDI resets CC 7 to.
pub const DEFAULT_CHANNEL_VOLUME: u8 = 100;
/// Centre pan (CC 10).
pub const CENTER_PAN: u8 = 64;

/// How a player treats the MIDI channels it receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SoundfontPlayerMode {
    /// One preset for the whole player.
    #[default]
    Single,
    /// Sixteen parts, one per MIDI channel.
    Multi,
}

impl SoundfontPlayerMode {
    pub const ALL: [SoundfontPlayerMode; 2] =
        [SoundfontPlayerMode::Single, SoundfontPlayerMode::Multi];

    /// Stable key for persistence: never renumbered.
    pub fn key(self) -> &'static str {
        match self {
            SoundfontPlayerMode::Single => "single",
            SoundfontPlayerMode::Multi => "multi",
        }
    }

    /// The mode a persisted key names; an unknown key is [`Self::Single`], the
    /// mode every file played in before the key existed.
    pub fn from_key(key: &str) -> Self {
        match key {
            "multi" => SoundfontPlayerMode::Multi,
            _ => SoundfontPlayerMode::Single,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SoundfontPlayerMode::Single => "Single",
            SoundfontPlayerMode::Multi => "16 Channels",
        }
    }
}

/// One channel of a multitimbral player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundfontChannel {
    /// `(bank, patch)`; a bank from [`crate::DRUM_BANK`] up is a drum kit.
    /// `None` leaves the channel at the font's default for its side.
    pub preset: Option<(i32, i32)>,
    /// CC 7, `0..=127`.
    pub volume: u8,
    /// CC 10, `0..=127`, [`CENTER_PAN`] centred.
    pub pan: u8,
    pub mute: bool,
    pub solo: bool,
}

impl Default for SoundfontChannel {
    fn default() -> Self {
        Self {
            preset: None,
            volume: DEFAULT_CHANNEL_VOLUME,
            pan: CENTER_PAN,
            mute: false,
            solo: false,
        }
    }
}

impl SoundfontChannel {
    pub fn sanitized(self) -> Self {
        Self {
            preset: self
                .preset
                .filter(|(bank, patch)| (0..=16_383).contains(bank) && (0..=127).contains(patch)),
            volume: self.volume.min(127),
            pan: self.pan.min(127),
            ..self
        }
    }

    pub fn is_drum_kit(&self) -> bool {
        matches!(self.preset, Some((bank, _)) if bank >= crate::DRUM_BANK)
    }
}

/// All sixteen channels, index 0 = MIDI channel 1.
pub type SoundfontChannels = [SoundfontChannel; MIDI_CHANNELS];

/// Sixteen channels at their General MIDI rest state: level 100, centred,
/// nothing muted or soloed, no preset chosen.
pub fn default_channels() -> SoundfontChannels {
    [SoundfontChannel::default(); MIDI_CHANNELS]
}

/// The channels that sound, one bit per channel: not muted, and soloed when
/// any channel is.
pub fn audible_channels(channels: &SoundfontChannels) -> u16 {
    let soloed = channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel.solo)
        .fold(0u16, |mask, (index, _)| mask | 1 << index);
    channels
        .iter()
        .enumerate()
        .filter(|(index, channel)| !channel.mute && (soloed == 0 || soloed & (1 << index) != 0))
        .fold(0u16, |mask, (index, _)| mask | 1 << index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_round_trips_through_its_key() {
        for mode in SoundfontPlayerMode::ALL {
            assert_eq!(SoundfontPlayerMode::from_key(mode.key()), mode);
        }
        assert_eq!(
            SoundfontPlayerMode::from_key("?"),
            SoundfontPlayerMode::Single
        );
    }

    #[test]
    fn solo_silences_every_channel_it_is_not_on_and_mute_wins_over_it() {
        let mut channels = default_channels();
        assert_eq!(audible_channels(&channels), u16::MAX);
        channels[2].mute = true;
        assert_eq!(audible_channels(&channels), u16::MAX & !(1 << 2));
        channels[5].solo = true;
        channels[9].solo = true;
        assert_eq!(audible_channels(&channels), 1 << 5 | 1 << 9);
        channels[9].mute = true;
        assert_eq!(audible_channels(&channels), 1 << 5);
    }

    #[test]
    fn a_channel_is_sanitized_into_the_midi_ranges() {
        let channel = SoundfontChannel {
            preset: Some((0, 200)),
            volume: 200,
            pan: 255,
            ..SoundfontChannel::default()
        }
        .sanitized();
        assert_eq!(channel.preset, None);
        assert_eq!((channel.volume, channel.pan), (127, 127));
    }
}
