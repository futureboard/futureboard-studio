//! The SoundFont player a track instrument runs.

use super::*;

pub struct RuntimeSoundfontPlayer {
    pub path: PathBuf,
    pub preset: Option<(i32, i32)>,
    pub volume: f32,
    pub reverb_chorus: bool,
    pub polyphony: usize,
    pub envelope: SoundfontEnvelope,
    pub quality: SoundfontRenderQuality,
    pub mode: SoundfontPlayerMode,
    /// The parts of a [`SoundfontPlayerMode::Multi`] player, reapplied to
    /// every rebuilt synthesizer.
    pub channels: SoundfontChannels,
    pub player: Option<SoundfontPlayer>,
}

impl RuntimeSoundfontPlayer {
    pub(super) fn from_snapshot(track: &EngineTrackSnapshot, sample_rate: u32) -> Option<Self> {
        if !track.builtin_soundfont_player {
            return None;
        }
        let path = track.soundfont_path.as_ref().filter(|p| !p.is_empty())?;
        let preset = track
            .soundfont_preset_bank
            .zip(track.soundfont_preset_patch);
        let mut state = Self {
            path: PathBuf::from(path),
            preset,
            volume: track.soundfont_volume.clamp(0.0, 1.0),
            reverb_chorus: track.soundfont_reverb_chorus,
            polyphony: track.soundfont_polyphony.clamp(1, 256),
            envelope: track.soundfont_envelope.sanitized(),
            quality: track.soundfont_quality,
            mode: track.soundfont_mode,
            channels: track.soundfont_channels,
            player: None,
        };
        state.rebuild(sample_rate, None);
        Some(state)
    }

    /// Installs new parts on a multitimbral player. Allocation-free: this runs
    /// from a control command on the audio thread.
    pub fn set_channels(&mut self, channels: &SoundfontChannels) {
        self.channels = *channels;
        if let Some(player) = self.player.as_mut() {
            player.set_channels(channels);
        }
    }

    /// Output samples of delay this instrument adds — the decimation filter at
    /// an oversampled render quality, zero otherwise. Folded into the track's
    /// plugin latency so delay compensation sees it.
    pub fn latency_samples(&self) -> u32 {
        self.player
            .as_ref()
            .map(SoundfontPlayer::latency_samples)
            .unwrap_or(0)
    }

    /// Rebuilds the synthesizer for the current settings. `sound_font` lets the
    /// caller hand back an already-parsed font (a graph clone, or the player
    /// being replaced), so changing polyphony or reverb never re-reads a bank
    /// that can be tens of megabytes. Control thread only — this can do
    /// filesystem I/O.
    pub(super) fn rebuild(&mut self, sample_rate: u32, sound_font: Option<Arc<SoundFont>>) {
        let settings = SoundfontPlayerSettings {
            sample_rate: sample_rate.max(1) as i32,
            block_size: 0,
            maximum_polyphony: self.polyphony,
            enable_reverb_and_chorus: self.reverb_chorus,
            envelope: self.envelope,
            quality: self.quality,
            // Sized for the callback block so an oversampled render never grows
            // a buffer on the audio thread.
            max_render_frames: DEFAULT_AUDIO_BLOCK_CAPACITY,
            mode: self.mode,
        };
        let built = match sound_font {
            Some(font) => SoundfontPlayer::from_sound_font(font, settings),
            None => SoundfontPlayer::from_path(&self.path, settings),
        };
        match built {
            Ok(mut player) => {
                player.set_master_volume(self.volume);
                if self.mode == SoundfontPlayerMode::Multi {
                    // Each channel is its own part; the player-wide preset
                    // belongs to single mode.
                    player.set_channels(&self.channels);
                } else if let Some((bank, patch)) = self.preset {
                    // Every melodic channel gets the track's preset: a
                    // Futureboard MIDI track can put each note on its own
                    // channel, and only channel 1 answering the selected sound
                    // would leave the rest on the SoundFont's default preset.
                    if let Err(error) = player.select_preset_all_channels(bank, patch) {
                        eprintln!(
                            "[soundfont-player] preset {bank}:{patch} not applied for '{}': {error}",
                            self.path.display()
                        );
                    }
                }
                self.player = Some(player);
            }
            Err(error) => {
                eprintln!(
                    "[soundfont-player] failed to load '{}': {error}",
                    self.path.display()
                );
                self.player = None;
            }
        }
    }
}

impl std::fmt::Debug for RuntimeSoundfontPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSoundfontPlayer")
            .field("path", &self.path)
            .field("preset", &self.preset)
            .field("volume", &self.volume)
            .field("reverb_chorus", &self.reverb_chorus)
            .field("polyphony", &self.polyphony)
            .field("envelope", &self.envelope)
            .field("quality", &self.quality)
            .field("mode", &self.mode)
            .field("loaded", &self.player.is_some())
            .finish()
    }
}

impl Clone for RuntimeSoundfontPlayer {
    fn clone(&self) -> Self {
        let sample_rate = self
            .player
            .as_ref()
            .map(|player| player.sample_rate().max(1) as u32)
            .unwrap_or(48_000);
        // A synthesizer owns per-voice state that must not be shared, but the
        // parsed font is immutable — hand it to the clone instead of reading
        // the file again on every graph swap.
        let sound_font = self.player.as_ref().map(SoundfontPlayer::sound_font);
        let mut cloned = Self {
            path: self.path.clone(),
            preset: self.preset,
            volume: self.volume,
            reverb_chorus: self.reverb_chorus,
            polyphony: self.polyphony,
            envelope: self.envelope,
            quality: self.quality,
            mode: self.mode,
            channels: self.channels,
            player: None,
        };
        cloned.rebuild(sample_rate, sound_font);
        cloned
    }
}
