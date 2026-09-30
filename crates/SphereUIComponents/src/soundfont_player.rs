//! Native SoundFont player bridge for Futureboard Studio.
//!
//! This module keeps the GPUI app wired to the RustySynth-backed
//! `SphereSoundfontPlayer` crate without inventing a sampler UI in this slice.
//! SoundFont loading remains a control/offline operation; rendering is delegated
//! to the player crate's preloaded handle.

pub use sphere_soundfont_player::{
    audible_channels, SoundfontChannel, SoundfontChannels, SoundfontEnvelope, SoundfontPlayer,
    SoundfontPlayerError, SoundfontPlayerMode, SoundfontPlayerSettings, SoundfontPresetInfo,
    SoundfontRenderQuality, DRUM_BANK, ENVELOPE_MAX_TIME_MS, PERCUSSION_CHANNEL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoundfontPlayerBackendStatus {
    pub available: bool,
    pub backend: &'static str,
}

pub fn soundfont_player_backend_status() -> SoundfontPlayerBackendStatus {
    SoundfontPlayerBackendStatus {
        available: true,
        backend: "rustysynth",
    }
}

pub fn default_soundfont_player_settings(sample_rate: i32) -> SoundfontPlayerSettings {
    SoundfontPlayerSettings {
        sample_rate,
        ..SoundfontPlayerSettings::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_status_is_available() {
        let status = soundfont_player_backend_status();
        assert!(status.available);
        assert_eq!(status.backend, "rustysynth");
    }

    #[test]
    fn default_settings_use_requested_sample_rate() {
        let settings = default_soundfont_player_settings(48_000);
        assert_eq!(settings.sample_rate, 48_000);
    }
}
