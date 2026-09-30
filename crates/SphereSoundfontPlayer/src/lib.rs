//! RustySynth-backed SoundFont player for Futureboard.
//!
//! Loading a SoundFont is a control/offline operation. Rendering assumes the
//! caller owns the output buffers and keeps filesystem I/O out of the audio path.
//!
//! A parsed [`SoundFont`] is immutable and large (a General MIDI bank is tens of
//! megabytes), while a [`Synthesizer`] is cheap by comparison and must be rebuilt
//! whenever polyphony, reverb/chorus, or the sample rate changes. [`font_cache`]
//! keeps the parse result shareable so those rebuilds — and runtime graph clones —
//! never re-read the file.

pub mod font_cache;
pub mod multitimbral;
pub mod shaping;
#[cfg(any(test, feature = "test-support"))]
pub mod test_font;

pub use shaping::{
    DECIMATOR_LATENCY_SAMPLES, ENVELOPE_MAX_TIME_MS, SoundfontEnvelope, SoundfontRenderQuality,
};
use shaping::{Decimator, GateEnvelope};

pub use multitimbral::{
    CENTER_PAN, DEFAULT_CHANNEL_VOLUME, MIDI_CHANNELS, SoundfontChannel, SoundfontChannels,
    SoundfontPlayerMode, audible_channels, default_channels,
};

pub use rustysynth::SoundFont;
use rustysynth::{Synthesizer, SynthesizerSettings};
use std::ffi::CStr;
use std::fs::File;
use std::io::{Cursor, Read};
use std::os::raw::c_char;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::ptr;
use std::slice;
use std::sync::Arc;

const DEFAULT_SAMPLE_RATE: i32 = 44_100;

/// The MIDI channel rustysynth reserves for percussion (channel 10 in
/// one-based MIDI numbering). Bank select on this channel is offset into the
/// SoundFont's drum banks, so it never carries a melodic preset.
pub const PERCUSSION_CHANNEL: u8 = Synthesizer::PERCUSSION_CHANNEL as u8;

/// SoundFont bank number of the General MIDI drum kits.
pub const DRUM_BANK: i32 = 128;

/// Where a note lands when the player holds a melodic preset but the note
/// arrived on the percussion channel. [`SoundfontPlayer::select_preset_all_channels`]
/// deliberately leaves channel 10 on the font's drum banks, so without this the
/// note would play a drum kit instead of the selected instrument.
const MELODIC_FALLBACK_CHANNEL: u8 = 0;

/// Controller number the engine uses for pitch bend on a controller lane
/// (VST3 `kPitchBend`). Not a real MIDI CC — see [`SoundfontPlayer::controller`].
pub const CONTROLLER_PITCH_BEND: u8 = 129;

/// Controller number the engine uses for channel pressure (VST3 `kAfterTouch`).
pub const CONTROLLER_CHANNEL_PRESSURE: u8 = 128;

/// Controller number the engine uses for a program change (VST3
/// `kCtrlProgramChange`), with the program as the value.
pub const CONTROLLER_PROGRAM_CHANGE: u8 = 130;

#[derive(Debug)]
pub enum SoundfontPlayerError {
    InvalidSampleRate(i32),
    InvalidChannel(u8),
    InvalidNote(u8),
    InvalidVelocity(u8),
    InvalidBank(i32),
    InvalidPatch(i32),
    /// `(bank, patch)` requested but not present in the loaded SoundFont's
    /// preset list — distinct from `InvalidBank`/`InvalidPatch`, which reject
    /// out-of-range values before ever consulting the font.
    PresetNotFound {
        bank: i32,
        patch: i32,
    },
    /// `(bank, patch)` exists in the font but MIDI bank select cannot address
    /// it from `channel` — a drum-bank preset asked for on a melodic channel,
    /// or a melodic preset asked for on the percussion channel.
    PresetUnreachableOnChannel {
        channel: u8,
        bank: i32,
        patch: i32,
    },
    Io(std::io::Error),
    SoundFont(rustysynth::SoundFontError),
    Synthesizer(rustysynth::SynthesizerError),
    BufferLengthMismatch {
        left: usize,
        right: usize,
    },
}

impl std::fmt::Display for SoundfontPlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSampleRate(sample_rate) => {
                write!(f, "invalid sample rate: {sample_rate}")
            }
            Self::InvalidChannel(channel) => write!(f, "invalid MIDI channel: {channel}"),
            Self::InvalidNote(note) => write!(f, "invalid MIDI note: {note}"),
            Self::InvalidVelocity(velocity) => write!(f, "invalid MIDI velocity: {velocity}"),
            Self::InvalidBank(bank) => write!(f, "invalid preset bank: {bank}"),
            Self::InvalidPatch(patch) => write!(f, "invalid preset patch: {patch}"),
            Self::PresetNotFound { bank, patch } => {
                write!(
                    f,
                    "no preset at bank {bank} patch {patch} in this SoundFont"
                )
            }
            Self::PresetUnreachableOnChannel {
                channel,
                bank,
                patch,
            } => {
                write!(
                    f,
                    "bank {bank} patch {patch} cannot be selected on MIDI channel {}",
                    channel + 1
                )
            }
            Self::Io(error) => write!(f, "SoundFont I/O failed: {error}"),
            Self::SoundFont(error) => write!(f, "SoundFont load failed: {error:?}"),
            Self::Synthesizer(error) => write!(f, "synthesizer init failed: {error:?}"),
            Self::BufferLengthMismatch { left, right } => {
                write!(f, "buffer length mismatch: left={left}, right={right}")
            }
        }
    }
}

impl std::error::Error for SoundfontPlayerError {}

impl From<std::io::Error> for SoundfontPlayerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<rustysynth::SoundFontError> for SoundfontPlayerError {
    fn from(value: rustysynth::SoundFontError) -> Self {
        Self::SoundFont(value)
    }
}

impl From<rustysynth::SynthesizerError> for SoundfontPlayerError {
    fn from(value: rustysynth::SynthesizerError) -> Self {
        Self::Synthesizer(value)
    }
}

/// Output frames the render scratch is sized for when a caller does not say.
/// Larger requests are rendered as repeated passes, never by reallocating.
const DEFAULT_MAX_RENDER_FRAMES: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoundfontPlayerSettings {
    /// The rate this player *outputs*. At an oversampled
    /// [`SoundfontPlayerSettings::quality`] the synthesizer inside runs faster
    /// than this; [`SoundfontPlayer::internal_sample_rate`] reports that.
    pub sample_rate: i32,
    pub block_size: usize,
    pub maximum_polyphony: usize,
    pub enable_reverb_and_chorus: bool,
    pub envelope: SoundfontEnvelope,
    pub quality: SoundfontRenderQuality,
    /// Longest render this player is expected to be asked for in one call.
    /// Sizes the oversampling scratch at build time so the audio path never
    /// allocates. `0` uses [`DEFAULT_MAX_RENDER_FRAMES`].
    pub max_render_frames: usize,
    /// One instrument, or sixteen parts (see [`multitimbral`]).
    pub mode: SoundfontPlayerMode,
}

impl Default for SoundfontPlayerSettings {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            block_size: 0,
            maximum_polyphony: 0,
            enable_reverb_and_chorus: true,
            envelope: SoundfontEnvelope::default(),
            quality: SoundfontRenderQuality::default(),
            max_render_frames: 0,
            mode: SoundfontPlayerMode::Single,
        }
    }
}

impl SoundfontPlayerSettings {
    /// The rate the wrapped synthesizer runs at.
    fn internal_sample_rate(self) -> Result<i32, SoundfontPlayerError> {
        if self.sample_rate <= 0 {
            return Err(SoundfontPlayerError::InvalidSampleRate(self.sample_rate));
        }
        self.sample_rate
            .checked_mul(self.quality.oversample() as i32)
            .ok_or(SoundfontPlayerError::InvalidSampleRate(self.sample_rate))
    }

    fn to_rustysynth(self) -> Result<SynthesizerSettings, SoundfontPlayerError> {
        let mut settings = SynthesizerSettings::new(self.internal_sample_rate()?);
        if self.block_size > 0 {
            settings.block_size = self.block_size;
        }
        if self.maximum_polyphony > 0 {
            settings.maximum_polyphony = self.maximum_polyphony;
        }
        settings.enable_reverb_and_chorus = self.enable_reverb_and_chorus;
        Ok(settings)
    }
}

pub struct SoundfontPlayer {
    synthesizer: Synthesizer,
    /// The same parsed font the synthesizer holds. Kept here because rustysynth
    /// only hands back a `&SoundFont`, and rebuilding a synthesizer (or cloning
    /// a runtime graph) must reuse the parse instead of re-reading the file.
    sound_font: Arc<SoundFont>,
    /// The rate this player hands back from [`Self::render`], which is the
    /// synthesizer's own rate divided by the oversampling factor.
    output_sample_rate: i32,
    quality: SoundfontRenderQuality,
    envelope: SoundfontEnvelope,
    gate: GateEnvelope,
    /// `None` at [`SoundfontRenderQuality::Standard`], where the synthesizer
    /// already runs at the output rate and nothing is filtered.
    decimator: Option<Decimator>,
    /// Keys physically down, one bit per MIDI note, per channel.
    held: [u128; 16],
    /// Keys released while the sustain pedal was down. They still sound, so the
    /// amp envelope must count them as active.
    sustained: [u128; 16],
    /// Sustain pedal (CC 64) state, one bit per channel.
    hold_pedal: u16,
    /// The preset last applied by [`Self::select_preset_all_channels`] — the
    /// one preset this player as a whole is playing. Drives [`Self::routed_channel`].
    selected_preset: Option<(i32, i32)>,
    mode: SoundfontPlayerMode,
    /// [`SoundfontPlayerMode::Multi`]: each channel's part as last applied.
    channels: SoundfontChannels,
    /// Whether `channels` has reached the synthesizer since it was built or
    /// reset; until then every channel is applied, not just the changed ones.
    channels_applied: bool,
    /// [`audible_channels`] of `channels`: a note-on for a channel outside it
    /// is dropped.
    audible: u16,
}

impl SoundfontPlayer {
    /// Loads `path` through [`font_cache`], so a font already parsed for another
    /// player (or for the previous settings of this one) is reused.
    pub fn from_path(
        path: impl AsRef<Path>,
        settings: SoundfontPlayerSettings,
    ) -> Result<Self, SoundfontPlayerError> {
        let sound_font = font_cache::load(path.as_ref())?;
        Self::from_sound_font(sound_font, settings)
    }

    /// Reads and parses `path` unconditionally, bypassing [`font_cache`].
    pub fn from_path_uncached(
        path: impl AsRef<Path>,
        settings: SoundfontPlayerSettings,
    ) -> Result<Self, SoundfontPlayerError> {
        let mut file = File::open(path)?;
        Self::from_reader(&mut file, settings)
    }

    pub fn from_bytes(
        bytes: &[u8],
        settings: SoundfontPlayerSettings,
    ) -> Result<Self, SoundfontPlayerError> {
        let mut cursor = Cursor::new(bytes);
        Self::from_reader(&mut cursor, settings)
    }

    pub fn from_reader<R: Read>(
        reader: &mut R,
        settings: SoundfontPlayerSettings,
    ) -> Result<Self, SoundfontPlayerError> {
        Self::from_sound_font(Arc::new(SoundFont::new(reader)?), settings)
    }

    /// Builds a player over an already-parsed font. Rebuilding for new settings
    /// costs a voice pool and preset table, not another multi-megabyte parse.
    pub fn from_sound_font(
        sound_font: Arc<SoundFont>,
        settings: SoundfontPlayerSettings,
    ) -> Result<Self, SoundfontPlayerError> {
        let synth_settings = settings.to_rustysynth()?;
        let synthesizer = Synthesizer::new(&sound_font, &synth_settings)?;
        let envelope = settings.envelope.sanitized();
        let factor = settings.quality.oversample();
        let max_render_frames = if settings.max_render_frames == 0 {
            DEFAULT_MAX_RENDER_FRAMES
        } else {
            settings.max_render_frames
        };
        Ok(Self {
            synthesizer,
            sound_font,
            output_sample_rate: settings.sample_rate,
            quality: settings.quality,
            envelope,
            gate: GateEnvelope::new(envelope, settings.sample_rate),
            decimator: (factor > 1).then(|| Decimator::new(factor, max_render_frames)),
            held: [0; 16],
            sustained: [0; 16],
            hold_pedal: 0,
            selected_preset: None,
            mode: settings.mode,
            channels: default_channels(),
            channels_applied: false,
            audible: u16::MAX,
        })
    }

    /// The MIDI channel a note has to be sent on to actually reach this player's
    /// selected preset.
    ///
    /// Bank select on the percussion channel is offset into the font's drum
    /// banks, so a preset can only be addressed from one side of that line:
    ///
    /// - a **drum-bank** preset exists *only* on channel 10, so every note is
    ///   routed there. Without this a track whose notes carry channel 1 — which
    ///   is what the piano roll writes — would play whatever melodic preset
    ///   channel 1 happens to hold, and the chosen kit would never sound.
    /// - a **melodic** preset was applied to every channel *except* 10, so a
    ///   note that arrives on channel 10 is moved to
    ///   [`MELODIC_FALLBACK_CHANNEL`]. Other channels are left alone, which
    ///   keeps per-channel pitch bend and CC working for tracks that put each
    ///   note on its own channel.
    ///
    /// With no preset selected the channel passes through untouched. In
    /// [`SoundfontPlayerMode::Multi`] every channel is its own part and keeps
    /// its notes.
    pub fn routed_channel(&self, channel: u8) -> u8 {
        let channel = channel.min(15);
        if self.mode == SoundfontPlayerMode::Multi {
            return channel;
        }
        match self.selected_preset {
            Some((bank, _)) if bank >= DRUM_BANK => PERCUSSION_CHANNEL,
            Some(_) if channel == PERCUSSION_CHANNEL => MELODIC_FALLBACK_CHANNEL,
            _ => channel,
        }
    }

    /// The preset this player as a whole is set to, if one was applied through
    /// [`Self::select_preset_all_channels`].
    pub fn selected_preset(&self) -> Option<(i32, i32)> {
        self.selected_preset
    }

    /// Whether any note is sounding by the amp envelope's reckoning: a key down,
    /// or a key released under the sustain pedal.
    #[inline]
    fn any_note_active(&self) -> bool {
        self.held
            .iter()
            .zip(self.sustained.iter())
            .any(|(held, sustained)| (held | sustained) != 0)
    }

    /// Opens or closes the amp envelope if `before` no longer matches the note
    /// bookkeeping. Every method that changes `held`/`sustained`/`hold_pedal`
    /// must sample [`Self::any_note_active`] first and end here.
    #[inline]
    fn refresh_gate(&mut self, before: bool) {
        let after = self.any_note_active();
        if after && !before {
            self.gate.open();
        } else if !after && before {
            self.gate.close();
        }
    }

    #[inline]
    fn track_note_on(&mut self, channel: u8, note: u8) {
        let before = self.any_note_active();
        let bit = 1u128 << note;
        self.held[channel as usize] |= bit;
        self.sustained[channel as usize] &= !bit;
        self.refresh_gate(before);
    }

    #[inline]
    fn track_note_off(&mut self, channel: u8, note: u8) {
        let before = self.any_note_active();
        let bit = 1u128 << note;
        self.held[channel as usize] &= !bit;
        if self.hold_pedal & (1 << channel) != 0 {
            self.sustained[channel as usize] |= bit;
        }
        self.refresh_gate(before);
    }

    #[inline]
    fn track_hold_pedal(&mut self, channel: u8, down: bool) {
        let before = self.any_note_active();
        if down {
            self.hold_pedal |= 1 << channel;
        } else {
            self.hold_pedal &= !(1 << channel);
            self.sustained[channel as usize] = 0;
        }
        self.refresh_gate(before);
    }

    #[inline]
    fn track_all_notes_off(&mut self, channel: Option<u8>) {
        let before = self.any_note_active();
        match channel {
            Some(channel) => {
                self.held[channel as usize] = 0;
                self.sustained[channel as usize] = 0;
            }
            None => {
                self.held = [0; 16];
                self.sustained = [0; 16];
            }
        }
        self.refresh_gate(before);
    }

    /// The amp envelope currently shaping this player's output.
    pub fn envelope(&self) -> SoundfontEnvelope {
        self.envelope
    }

    /// Installs a new amp envelope. Unlike polyphony and reverb this needs no
    /// rebuild — the envelope is ours, not one of rustysynth's fixed settings —
    /// so it is safe from a control command drained on the audio thread.
    pub fn set_envelope(&mut self, envelope: SoundfontEnvelope) {
        let envelope = envelope.sanitized();
        self.envelope = envelope;
        self.gate.configure(envelope, self.output_sample_rate);
        if self.any_note_active() {
            self.gate.open();
        }
    }

    pub fn quality(&self) -> SoundfontRenderQuality {
        self.quality
    }

    /// Rate the wrapped synthesizer runs at — the output rate times the
    /// oversampling factor.
    pub fn internal_sample_rate(&self) -> i32 {
        self.synthesizer.get_sample_rate()
    }

    /// Output samples of delay this player adds, from the decimation filter at
    /// an oversampled quality. Zero at [`SoundfontRenderQuality::Standard`].
    /// The engine folds this into the track's delay compensation.
    pub fn latency_samples(&self) -> u32 {
        if self.decimator.is_some() {
            DECIMATOR_LATENCY_SAMPLES
        } else {
            0
        }
    }

    /// The parsed font backing this player, for reuse by a rebuilt player.
    pub fn sound_font(&self) -> Arc<SoundFont> {
        Arc::clone(&self.sound_font)
    }

    pub fn note_on(
        &mut self,
        channel: u8,
        note: u8,
        velocity: u8,
    ) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        validate_note(note)?;
        if velocity > 127 {
            return Err(SoundfontPlayerError::InvalidVelocity(velocity));
        }

        // A velocity-0 note-on is a note-off in MIDI; letting it through as an
        // "on" would leave the amp envelope gated open on a note that stopped.
        if velocity == 0 {
            return self.note_off(channel, note);
        }
        let channel = self.routed_channel(channel);
        if !self.channel_audible(channel) {
            return Ok(());
        }
        self.synthesizer
            .note_on(channel.into(), note.into(), velocity.into());
        self.track_note_on(channel, note);
        Ok(())
    }

    pub fn note_off(&mut self, channel: u8, note: u8) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        validate_note(note)?;
        // Routed the same way the note-on was, so the release always finds the
        // voice it started.
        let channel = self.routed_channel(channel);
        self.synthesizer.note_off(channel.into(), note.into());
        self.track_note_off(channel, note);
        Ok(())
    }

    pub fn all_notes_off(&mut self, immediate: bool) {
        self.synthesizer.note_off_all(immediate);
        self.track_all_notes_off(None);
    }

    pub fn process_midi_message(
        &mut self,
        channel: u8,
        command: u8,
        data1: u8,
        data2: u8,
    ) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        let channel = self.routed_channel(channel);
        if command & 0xF0 == 0x90 && data2 > 0 && !self.channel_audible(channel) {
            return Ok(());
        }
        self.synthesizer.process_midi_message(
            channel.into(),
            command.into(),
            data1.into(),
            data2.into(),
        );
        // Raw MIDI reaches the same voices the typed methods do, so the amp
        // envelope's note bookkeeping has to follow it too.
        self.track_raw_midi(channel, command, data1, data2);
        Ok(())
    }

    /// Mirrors a raw MIDI message into the amp envelope's note bookkeeping.
    /// Only the messages that start or stop voices matter here; the synthesizer
    /// has already been told about all of them.
    fn track_raw_midi(&mut self, channel: u8, command: u8, data1: u8, data2: u8) {
        match command & 0xF0 {
            0x90 if data2 > 0 && data1 <= 127 => self.track_note_on(channel, data1),
            0x80 | 0x90 if data1 <= 127 => self.track_note_off(channel, data1),
            0xB0 => match data1 {
                64 => self.track_hold_pedal(channel, data2 >= 64),
                // All Sound Off / All Notes Off.
                120 | 123 => self.track_all_notes_off(Some(channel)),
                _ => {}
            },
            _ => {}
        }
    }

    /// Resets the synthesizer. rustysynth's own `reset` returns every channel to
    /// its default bank and patch, so the player-wide preset selection is
    /// dropped with it and the caller must re-apply
    /// [`Self::select_preset_all_channels`] before the next note.
    ///
    /// A [`SoundfontPlayerMode::Multi`] player's parts are the player's own
    /// state, not the synthesizer's, so they are applied again at once.
    pub fn reset(&mut self) {
        self.synthesizer.reset();
        self.selected_preset = None;
        self.held = [0; 16];
        self.sustained = [0; 16];
        self.hold_pedal = 0;
        self.gate.reset();
        if let Some(decimator) = self.decimator.as_mut() {
            decimator.reset();
        }
        if self.mode == SoundfontPlayerMode::Multi && self.channels_applied {
            self.channels_applied = false;
            let channels = self.channels;
            self.set_channels(&channels);
        }
    }

    pub fn mode(&self) -> SoundfontPlayerMode {
        self.mode
    }

    /// Each channel's part as last applied by [`Self::set_channels`].
    pub fn channels(&self) -> &SoundfontChannels {
        &self.channels
    }

    #[inline]
    fn channel_audible(&self, channel: u8) -> bool {
        self.audible & (1 << channel.min(15)) != 0
    }

    /// Installs the sixteen parts of a [`SoundfontPlayerMode::Multi`] player,
    /// sending only what changed: a preset as bank select + program change, a
    /// level as CC 7, a pan as CC 10. A channel that stops sounding (muted, or
    /// another channel soloed) releases its notes; one that starts sounding
    /// again plays from its next note.
    ///
    /// Allocation-free, so it is applied from a control command drained on the
    /// audio thread, and a part edit never rebuilds the synthesizer or cuts the
    /// notes of the other channels. A preset the font does not have leaves the
    /// channel on the one it had. Ignored by a single-instrument player.
    pub fn set_channels(&mut self, channels: &SoundfontChannels) {
        if self.mode != SoundfontPlayerMode::Multi {
            return;
        }
        let force = !self.channels_applied;
        let audible = audible_channels(channels);
        for (index, next) in channels.iter().enumerate() {
            let next = next.sanitized();
            let previous = self.channels[index];
            let channel = index as u8;
            let mut applied = next;
            if force || previous.preset != next.preset {
                applied.preset = match next.preset {
                    Some((bank, patch)) => match self.select_channel_preset(channel, bank, patch) {
                        Ok(()) => next.preset,
                        Err(_) if force => None,
                        Err(_) => previous.preset,
                    },
                    None => None,
                };
            }
            if force || previous.volume != next.volume {
                self.synthesizer.process_midi_message(
                    channel.into(),
                    0xB0,
                    0x07,
                    next.volume.into(),
                );
            }
            if force || previous.pan != next.pan {
                self.synthesizer
                    .process_midi_message(channel.into(), 0xB0, 0x0A, next.pan.into());
            }
            let bit = 1u16 << index;
            if self.audible & bit != 0 && audible & bit == 0 {
                self.synthesizer.note_off_all_channel(channel.into(), false);
                self.track_all_notes_off(Some(channel));
            }
            self.channels[index] = applied;
        }
        self.audible = audible;
        self.channels_applied = true;
    }

    /// Whether `channel` currently addresses the font's drum banks.
    pub fn is_drum_channel(&self, channel: u8) -> bool {
        self.synthesizer
            .is_percussion_channel(channel.min(15).into())
    }

    /// Puts `(bank, patch)` on one channel of a [`SoundfontPlayerMode::Multi`]
    /// player. Unlike [`Self::select_preset`] any channel can hold a drum kit:
    /// the channel is moved to the drum banks first.
    fn select_channel_preset(
        &mut self,
        channel: u8,
        bank: i32,
        patch: i32,
    ) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        if !self.has_preset(bank, patch) {
            return Err(SoundfontPlayerError::PresetNotFound { bank, patch });
        }
        let drums = bank >= DRUM_BANK;
        self.synthesizer
            .set_percussion_channel(channel.into(), drums);
        let selected = if drums { bank - DRUM_BANK } else { bank };
        self.synthesizer
            .process_midi_message(channel.into(), 0xB0, 0x00, (selected >> 7) & 0x7F);
        self.synthesizer
            .process_midi_message(channel.into(), 0xB0, 0x20, selected & 0x7F);
        self.synthesizer
            .process_midi_message(channel.into(), 0xC0, patch, 0);
        Ok(())
    }

    pub fn set_master_volume(&mut self, value: f32) {
        self.synthesizer.set_master_volume(value.max(0.0));
    }

    pub fn master_volume(&self) -> f32 {
        self.synthesizer.get_master_volume()
    }

    pub fn enable_reverb_and_chorus(&self) -> bool {
        self.synthesizer.get_enable_reverb_and_chorus()
    }

    pub fn render(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), SoundfontPlayerError> {
        if left.len() != right.len() {
            return Err(SoundfontPlayerError::BufferLengthMismatch {
                left: left.len(),
                right: right.len(),
            });
        }
        match self.decimator.as_mut() {
            None => self.synthesizer.render(left, right),
            Some(decimator) => {
                let synthesizer = &mut self.synthesizer;
                decimator.process(left, right, |os_left, os_right| {
                    synthesizer.render(os_left, os_right);
                });
            }
        }
        self.gate.apply(left, right);
        Ok(())
    }

    /// The rate this player outputs at. Not the synthesizer's rate when an
    /// oversampled quality is selected — see [`Self::internal_sample_rate`].
    pub fn sample_rate(&self) -> i32 {
        self.output_sample_rate
    }

    pub fn block_size(&self) -> usize {
        self.synthesizer.get_block_size()
    }

    pub fn maximum_polyphony(&self) -> usize {
        self.synthesizer.get_maximum_polyphony()
    }

    /// Voices currently sounding, out of [`Self::maximum_polyphony`].
    ///
    /// Read on the audio thread right after a render block so the transport's
    /// load meter reports the count the block actually produced. It is a plain
    /// field read — no allocation, no lock — which is what makes it legal
    /// there.
    pub fn active_voice_count(&self) -> usize {
        self.synthesizer.get_active_voice_count()
    }

    /// The loaded SoundFont's own bank name (from its INFO chunk), e.g.
    /// "General MIDI" — control-side metadata for a UI title, not audio state.
    pub fn bank_name(&self) -> &str {
        self.sound_font.get_info().get_bank_name()
    }

    /// How many presets the loaded SoundFont exposes.
    pub fn preset_count(&self) -> usize {
        self.sound_font.get_presets().len()
    }

    /// Every preset (MIDI bank + patch + display name) in the loaded
    /// SoundFont, sorted by `(bank, patch)`. Control/offline operation —
    /// walks the font's preset table, never touches the render path.
    pub fn list_presets(&self) -> Vec<SoundfontPresetInfo> {
        let mut presets: Vec<SoundfontPresetInfo> = self
            .sound_font
            .get_presets()
            .iter()
            .map(|preset| SoundfontPresetInfo {
                bank: preset.get_bank_number(),
                patch: preset.get_patch_number(),
                name: preset.get_name().to_string(),
            })
            .collect();
        presets.sort_by_key(|preset| (preset.bank, preset.patch));
        presets
    }

    /// Whether `(bank, patch)` exists in the loaded font. Allocation-free, so
    /// preset selection stays usable from a control command drained on the
    /// audio thread — unlike [`Self::list_presets`], which builds owned names.
    pub fn has_preset(&self, bank: i32, patch: i32) -> bool {
        self.sound_font
            .get_presets()
            .iter()
            .any(|preset| preset.get_bank_number() == bank && preset.get_patch_number() == patch)
    }

    /// Name of `(bank, patch)` in the loaded font, borrowed from the font's own
    /// preset table.
    pub fn preset_name(&self, bank: i32, patch: i32) -> Option<&str> {
        self.sound_font
            .get_presets()
            .iter()
            .find(|preset| preset.get_bank_number() == bank && preset.get_patch_number() == patch)
            .map(|preset| preset.get_name())
    }

    /// Selects a preset on `channel` via MIDI Bank Select (CC0 MSB / CC32
    /// LSB) followed by Program Change — the standard way to switch patches
    /// on a General MIDI-style synth, so this also works against any other
    /// host that only understands MIDI. Rejects `(bank, patch)` pairs that are
    /// not in the font instead of silently falling back to whatever the
    /// program change lands on.
    ///
    /// The percussion channel offsets bank select into the drum banks, so a
    /// drum-bank preset is requested there with the bank the drum kits occupy
    /// and a melodic preset cannot be selected on it at all.
    pub fn select_preset(
        &mut self,
        channel: u8,
        bank: i32,
        patch: i32,
    ) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        if !(0..=16_383).contains(&bank) {
            return Err(SoundfontPlayerError::InvalidBank(bank));
        }
        if !(0..=127).contains(&patch) {
            return Err(SoundfontPlayerError::InvalidPatch(patch));
        }
        if !self.has_preset(bank, patch) {
            return Err(SoundfontPlayerError::PresetNotFound { bank, patch });
        }
        let Some(selected) = bank_select_value(channel, bank) else {
            return Err(SoundfontPlayerError::PresetUnreachableOnChannel {
                channel,
                bank,
                patch,
            });
        };

        let bank_msb = (selected >> 7) & 0x7F;
        let bank_lsb = selected & 0x7F;
        self.synthesizer
            .process_midi_message(channel.into(), 0xB0, 0x00, bank_msb);
        self.synthesizer
            .process_midi_message(channel.into(), 0xB0, 0x20, bank_lsb);
        self.synthesizer
            .process_midi_message(channel.into(), 0xC0, patch, 0);
        Ok(())
    }

    /// Selects one preset on every melodic channel so a track plays the chosen
    /// sound no matter which MIDI channel its notes carry (Futureboard tracks
    /// can put each note on its own channel). The percussion channel keeps the
    /// font's drum banks.
    ///
    /// A drum-bank preset is instead selected on the percussion channel alone,
    /// which is the only channel that can address it. Incoming notes are then
    /// routed to that channel by [`Self::routed_channel`] — selecting the preset
    /// on channel 10 is not enough on its own, because the track's notes carry
    /// whatever channel the piano roll wrote.
    pub fn select_preset_all_channels(
        &mut self,
        bank: i32,
        patch: i32,
    ) -> Result<(), SoundfontPlayerError> {
        if bank >= DRUM_BANK {
            self.select_preset(PERCUSSION_CHANNEL, bank, patch)?;
        } else {
            for channel in 0..Synthesizer::CHANNEL_COUNT as u8 {
                if channel == PERCUSSION_CHANNEL {
                    continue;
                }
                self.select_preset(channel, bank, patch)?;
            }
        }
        // Recorded only after every channel took the preset, so a partial
        // failure cannot leave notes routed at a preset that was never applied.
        self.selected_preset = Some((bank, patch));
        Ok(())
    }

    /// Applies one controller-lane value. Plain CC numbers go through as MIDI
    /// control change; the engine's out-of-band controller numbers for pitch
    /// bend and channel pressure are translated to their own MIDI status bytes
    /// instead of being clamped into the CC range.
    pub fn controller(
        &mut self,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        match controller {
            CONTROLLER_PITCH_BEND => {
                // Lane values are 7-bit; expand to the 14-bit bend range so
                // centre (64) lands on the unbent 8192 rather than slightly flat.
                let bend = u16::from(value.min(127)) << 7;
                self.pitch_bend(channel, bend)
            }
            // rustysynth has no channel-pressure handling; dropping it is
            // honest, and clamping it into CC 127 would be a wrong sound.
            CONTROLLER_CHANNEL_PRESSURE => Ok(()),
            // A track's program selection: the patch of the bank last
            // selected on this channel (CC 0 / CC 32 arrive just before it).
            CONTROLLER_PROGRAM_CHANGE => {
                let channel = self.routed_channel(channel);
                self.synthesizer.process_midi_message(
                    channel.into(),
                    0xC0,
                    value.min(127).into(),
                    0,
                );
                Ok(())
            }
            _ => {
                let controller = controller.min(127);
                let value = value.min(127);
                let channel = self.routed_channel(channel);
                self.synthesizer.process_midi_message(
                    channel.into(),
                    0xB0,
                    controller.into(),
                    value.into(),
                );
                self.track_raw_midi(channel, 0xB0, controller, value);
                Ok(())
            }
        }
    }

    /// Sets the 14-bit pitch bend for `channel` (`0x2000` is centre).
    pub fn pitch_bend(&mut self, channel: u8, value: u16) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        let value = value.min(0x3FFF);
        let channel = self.routed_channel(channel);
        self.synthesizer.process_midi_message(
            channel.into(),
            0xE0,
            i32::from(value & 0x7F),
            i32::from(value >> 7),
        );
        Ok(())
    }

    /// Releases every held note on `channel` without touching other channels.
    pub fn all_notes_off_channel(
        &mut self,
        channel: u8,
        immediate: bool,
    ) -> Result<(), SoundfontPlayerError> {
        validate_channel(channel)?;
        let channel = self.routed_channel(channel);
        self.synthesizer
            .note_off_all_channel(channel.into(), immediate);
        self.track_all_notes_off(Some(channel));
        Ok(())
    }
}

/// The bank-select value that makes rustysynth resolve `bank` on `channel`, or
/// `None` when the channel cannot reach that bank at all. The percussion
/// channel adds [`DRUM_BANK`] to whatever bank select it receives, so it only
/// addresses drum banks and every other channel only addresses melodic ones.
fn bank_select_value(channel: u8, bank: i32) -> Option<i32> {
    if channel == PERCUSSION_CHANNEL {
        (bank >= DRUM_BANK).then_some(bank - DRUM_BANK)
    } else {
        (bank < DRUM_BANK).then_some(bank)
    }
}

/// One preset (MIDI bank + patch + display name) from a loaded SoundFont.
/// Plain data — no gpui / rendering dependency, so any UI layer (native GPUI,
/// web, or a future host) can build a preset browser from this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundfontPresetInfo {
    pub bank: i32,
    pub patch: i32,
    pub name: String,
}

fn validate_channel(channel: u8) -> Result<(), SoundfontPlayerError> {
    if channel >= Synthesizer::CHANNEL_COUNT as u8 {
        Err(SoundfontPlayerError::InvalidChannel(channel))
    } else {
        Ok(())
    }
}

fn validate_note(note: u8) -> Result<(), SoundfontPlayerError> {
    if note > 127 {
        Err(SoundfontPlayerError::InvalidNote(note))
    } else {
        Ok(())
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SphereSoundfontPlayerConfig {
    pub sample_rate: i32,
    pub block_size: usize,
    pub maximum_polyphony: usize,
    pub enable_reverb_and_chorus: u8,
    /// Amp envelope times in milliseconds and sustain in `0.0..=1.0`; all zero
    /// with `sustain = 1.0` (the `Default`) leaves the envelope bypassed.
    pub envelope_attack_ms: f32,
    pub envelope_decay_ms: f32,
    pub envelope_sustain: f32,
    pub envelope_release_ms: f32,
    /// Oversampling factor: 1, 2 or 4. Anything else falls back to 1.
    pub render_oversample: u8,
    pub max_render_frames: usize,
}

impl Default for SphereSoundfontPlayerConfig {
    fn default() -> Self {
        let settings = SoundfontPlayerSettings::default();
        Self {
            sample_rate: settings.sample_rate,
            block_size: settings.block_size,
            maximum_polyphony: settings.maximum_polyphony,
            enable_reverb_and_chorus: u8::from(settings.enable_reverb_and_chorus),
            envelope_attack_ms: settings.envelope.attack_ms,
            envelope_decay_ms: settings.envelope.decay_ms,
            envelope_sustain: settings.envelope.sustain,
            envelope_release_ms: settings.envelope.release_ms,
            render_oversample: settings.quality.oversample() as u8,
            max_render_frames: settings.max_render_frames,
        }
    }
}

impl SphereSoundfontPlayerConfig {
    fn into_settings(self) -> SoundfontPlayerSettings {
        SoundfontPlayerSettings {
            sample_rate: if self.sample_rate == 0 {
                DEFAULT_SAMPLE_RATE
            } else {
                self.sample_rate
            },
            block_size: self.block_size,
            maximum_polyphony: self.maximum_polyphony,
            enable_reverb_and_chorus: self.enable_reverb_and_chorus != 0,
            envelope: SoundfontEnvelope {
                attack_ms: self.envelope_attack_ms,
                decay_ms: self.envelope_decay_ms,
                sustain: self.envelope_sustain,
                release_ms: self.envelope_release_ms,
            }
            .sanitized(),
            quality: match self.render_oversample {
                2 => SoundfontRenderQuality::High,
                4 => SoundfontRenderQuality::Ultra,
                _ => SoundfontRenderQuality::Standard,
            },
            max_render_frames: self.max_render_frames,
            mode: SoundfontPlayerMode::Single,
        }
    }
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SphereSoundfontPlayerStatus {
    Ok = 0,
    NullPointer = -1,
    InvalidArgument = -2,
    LoadFailed = -3,
    RenderFailed = -4,
    Panic = -5,
}

fn status_from_error(error: &SoundfontPlayerError) -> SphereSoundfontPlayerStatus {
    match error {
        SoundfontPlayerError::Io(_)
        | SoundfontPlayerError::SoundFont(_)
        | SoundfontPlayerError::Synthesizer(_) => SphereSoundfontPlayerStatus::LoadFailed,
        SoundfontPlayerError::BufferLengthMismatch { .. } => {
            SphereSoundfontPlayerStatus::RenderFailed
        }
        SoundfontPlayerError::InvalidSampleRate(_)
        | SoundfontPlayerError::InvalidChannel(_)
        | SoundfontPlayerError::InvalidNote(_)
        | SoundfontPlayerError::InvalidVelocity(_)
        | SoundfontPlayerError::InvalidBank(_)
        | SoundfontPlayerError::InvalidPatch(_)
        | SoundfontPlayerError::PresetNotFound { .. }
        | SoundfontPlayerError::PresetUnreachableOnChannel { .. } => {
            SphereSoundfontPlayerStatus::InvalidArgument
        }
    }
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `path` must point to a valid NUL-terminated string, and `out_player` must be
/// a valid writable pointer. The returned handle must be released with
/// [`sphere_soundfont_player_destroy`].
pub unsafe extern "C" fn sphere_soundfont_player_create_from_path(
    path: *const c_char,
    config: SphereSoundfontPlayerConfig,
    out_player: *mut *mut SoundfontPlayer,
) -> i32 {
    if path.is_null() || out_player.is_null() {
        return SphereSoundfontPlayerStatus::NullPointer as i32;
    }
    unsafe {
        *out_player = ptr::null_mut();
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let path = unsafe { CStr::from_ptr(path) }
            .to_string_lossy()
            .into_owned();
        SoundfontPlayer::from_path(path, config.into_settings())
    }));

    match result {
        Ok(Ok(player)) => {
            unsafe {
                *out_player = Box::into_raw(Box::new(player));
            }
            SphereSoundfontPlayerStatus::Ok as i32
        }
        Ok(Err(error)) => status_from_error(&error) as i32,
        Err(_) => SphereSoundfontPlayerStatus::Panic as i32,
    }
}

#[unsafe(no_mangle)]
/// # Safety
///
/// When `len` is non-zero, `data` must point to `len` readable bytes.
/// `out_player` must be a valid writable pointer. The returned handle must be
/// released with [`sphere_soundfont_player_destroy`].
pub unsafe extern "C" fn sphere_soundfont_player_create_from_memory(
    data: *const u8,
    len: usize,
    config: SphereSoundfontPlayerConfig,
    out_player: *mut *mut SoundfontPlayer,
) -> i32 {
    if out_player.is_null() || (data.is_null() && len > 0) {
        return SphereSoundfontPlayerStatus::NullPointer as i32;
    }
    unsafe {
        *out_player = ptr::null_mut();
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let bytes = if len == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(data, len) }
        };
        SoundfontPlayer::from_bytes(bytes, config.into_settings())
    }));

    match result {
        Ok(Ok(player)) => {
            unsafe {
                *out_player = Box::into_raw(Box::new(player));
            }
            SphereSoundfontPlayerStatus::Ok as i32
        }
        Ok(Err(error)) => status_from_error(&error) as i32,
        Err(_) => SphereSoundfontPlayerStatus::Panic as i32,
    }
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be null or a handle returned by this crate that has not already
/// been destroyed.
pub unsafe extern "C" fn sphere_soundfont_player_destroy(player: *mut SoundfontPlayer) {
    if !player.is_null() {
        unsafe {
            drop(Box::from_raw(player));
        }
    }
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be a valid, uniquely owned handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_note_on(
    player: *mut SoundfontPlayer,
    channel: u8,
    note: u8,
    velocity: u8,
) -> i32 {
    with_player(player, |player| player.note_on(channel, note, velocity))
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be a valid, uniquely owned handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_note_off(
    player: *mut SoundfontPlayer,
    channel: u8,
    note: u8,
) -> i32 {
    with_player(player, |player| player.note_off(channel, note))
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be a valid, uniquely owned handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_all_notes_off(
    player: *mut SoundfontPlayer,
    immediate: u8,
) -> i32 {
    with_player(player, |player| {
        player.all_notes_off(immediate != 0);
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be a valid, uniquely owned handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_process_midi_message(
    player: *mut SoundfontPlayer,
    channel: u8,
    command: u8,
    data1: u8,
    data2: u8,
) -> i32 {
    with_player(player, |player| {
        player.process_midi_message(channel, command, data1, data2)
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be a valid, uniquely owned handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_reset(player: *mut SoundfontPlayer) -> i32 {
    with_player(player, |player| {
        player.reset();
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be a valid, uniquely owned handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_set_master_volume(
    player: *mut SoundfontPlayer,
    value: f32,
) -> i32 {
    with_player(player, |player| {
        player.set_master_volume(value);
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be a valid, uniquely owned handle returned by this crate. When
/// `frames` is non-zero, `left` and `right` must each point to `frames`
/// writable `f32` samples and must not alias each other.
pub unsafe extern "C" fn sphere_soundfont_player_render(
    player: *mut SoundfontPlayer,
    left: *mut f32,
    right: *mut f32,
    frames: usize,
) -> i32 {
    if player.is_null() {
        return SphereSoundfontPlayerStatus::NullPointer as i32;
    }
    if frames == 0 {
        return SphereSoundfontPlayerStatus::Ok as i32;
    }
    if left.is_null() || right.is_null() {
        return SphereSoundfontPlayerStatus::NullPointer as i32;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let player = unsafe { player.as_mut() }.ok_or(SphereSoundfontPlayerStatus::NullPointer)?;
        let left = unsafe { slice::from_raw_parts_mut(left, frames) };
        let right = unsafe { slice::from_raw_parts_mut(right, frames) };
        player
            .render(left, right)
            .map_err(|error| status_from_error(&error))
    }));

    match result {
        Ok(Ok(())) => SphereSoundfontPlayerStatus::Ok as i32,
        Ok(Err(status)) => status as i32,
        Err(_) => SphereSoundfontPlayerStatus::Panic as i32,
    }
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be null or a valid handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_sample_rate(
    player: *const SoundfontPlayer,
) -> i32 {
    if player.is_null() {
        return 0;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        unsafe { player.as_ref() }.map_or(0, SoundfontPlayer::sample_rate)
    }));
    result.unwrap_or(0)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be null or a valid handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_block_size(
    player: *const SoundfontPlayer,
) -> usize {
    if player.is_null() {
        return 0;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        unsafe { player.as_ref() }.map_or(0, SoundfontPlayer::block_size)
    }));
    result.unwrap_or(0)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `player` must be null or a valid handle returned by this crate.
pub unsafe extern "C" fn sphere_soundfont_player_maximum_polyphony(
    player: *const SoundfontPlayer,
) -> usize {
    if player.is_null() {
        return 0;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        unsafe { player.as_ref() }.map_or(0, SoundfontPlayer::maximum_polyphony)
    }));
    result.unwrap_or(0)
}

fn with_player(
    player: *mut SoundfontPlayer,
    f: impl FnOnce(&mut SoundfontPlayer) -> Result<(), SoundfontPlayerError>,
) -> i32 {
    if player.is_null() {
        return SphereSoundfontPlayerStatus::NullPointer as i32;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let player = unsafe { player.as_mut() }.ok_or(SphereSoundfontPlayerStatus::NullPointer)?;
        f(player).map_err(|error| status_from_error(&error))
    }));

    match result {
        Ok(Ok(())) => SphereSoundfontPlayerStatus::Ok as i32,
        Ok(Err(status)) => status as i32,
        Err(_) => SphereSoundfontPlayerStatus::Panic as i32,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn sphere_soundfont_player_null() -> *mut SoundfontPlayer {
    ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player() -> SoundfontPlayer {
        SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings::default(),
        )
        .expect("synthetic font loads")
    }

    fn peak(player: &mut SoundfontPlayer, frames: usize) -> f32 {
        let mut left = vec![0.0; frames];
        let mut right = vec![0.0; frames];
        player.render(&mut left, &mut right).expect("render");
        left.iter()
            .chain(right.iter())
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    fn multi_player() -> SoundfontPlayer {
        SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings {
                mode: SoundfontPlayerMode::Multi,
                enable_reverb_and_chorus: false,
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("synthetic font loads")
    }

    fn stereo_peaks(player: &mut SoundfontPlayer, frames: usize) -> (f32, f32) {
        let mut left = vec![0.0; frames];
        let mut right = vec![0.0; frames];
        player.render(&mut left, &mut right).expect("render");
        let peak = |buffer: &[f32]| buffer.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        (peak(&left), peak(&right))
    }

    fn melodic_parts() -> SoundfontChannels {
        let mut channels = default_channels();
        for channel in channels.iter_mut() {
            channel.preset = Some(test_font::MELODIC_PRESET);
        }
        channels
    }

    /// A kit on channel 10, reverb off so the layers stay where they are
    /// panned.
    fn kit_player(mode: SoundfontPlayerMode) -> SoundfontPlayer {
        let mut player = SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings {
                mode,
                enable_reverb_and_chorus: false,
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("synthetic font loads");
        match mode {
            SoundfontPlayerMode::Single => player
                .select_preset_all_channels(test_font::DRUM_PRESET.0, test_font::DRUM_PRESET.1)
                .expect("kit selects"),
            SoundfontPlayerMode::Multi => {
                let mut channels = default_channels();
                channels[3].preset = Some(test_font::DRUM_PRESET);
                channels[9].preset = Some(test_font::DRUM_PRESET);
                player.set_channels(&channels);
            }
        }
        player
    }

    fn rendered(player: &mut SoundfontPlayer, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let mut left = vec![0.0; frames];
        let mut right = vec![0.0; frames];
        player.render(&mut left, &mut right).expect("render");
        (left, right)
    }

    fn buffer_peak(buffer: &[f32]) -> f32 {
        buffer.iter().fold(0.0f32, |p, s| p.max(s.abs()))
    }

    /// A kit's stereo pair shares one choke group. Both halves of one hit must
    /// sound: the group is about cutting earlier hits, not the hit's own layers.
    #[test]
    fn a_kit_hit_plays_every_layer_of_its_choke_group() {
        let mut player = kit_player(SoundfontPlayerMode::Single);
        player
            .note_on(PERCUSSION_CHANNEL, 42, 127)
            .expect("note on");
        let (l, r) = stereo_peaks(&mut player, 4_096);
        assert!(l > 0.01 && r > 0.01, "both halves of the pair: l={l} r={r}");
    }

    /// A closed hi-hat cuts the open one: every layer of the earlier hit fades
    /// out, and the new hit plays whole.
    #[test]
    fn a_choke_group_cuts_every_layer_of_the_earlier_hit() {
        let mut player = kit_player(SoundfontPlayerMode::Single);
        player
            .note_on(PERCUSSION_CHANNEL, 46, 127)
            .expect("open hat");
        let (open_l, open_r) = stereo_peaks(&mut player, 4_096);
        player
            .note_on(PERCUSSION_CHANNEL, 42, 20)
            .expect("closed hat");
        // 50 ms: well past the choke's fade.
        let _ = stereo_peaks(&mut player, 2_205);
        let (l, r) = stereo_peaks(&mut player, 4_096);
        assert!(
            l < open_l * 0.2 && r < open_r * 0.2,
            "the open hat is cut on both sides: open=({open_l}, {open_r}) now=({l}, {r})"
        );
        assert!(l > 1.0e-4 && r > 1.0e-4, "the closed hat plays both halves");
    }

    /// The cut is a fade, not a step: no sample jumps the way a voice stopped
    /// mid-cycle does.
    #[test]
    fn a_choked_hit_fades_out_without_a_click() {
        let mut player = kit_player(SoundfontPlayerMode::Single);
        player
            .note_on(PERCUSSION_CHANNEL, 46, 127)
            .expect("open hat");
        let (before_l, _) = rendered(&mut player, 4_096);
        player
            .note_on(PERCUSSION_CHANNEL, 42, 1)
            .expect("closed hat");
        let (after_l, _) = rendered(&mut player, 2_048);
        let mut previous = *before_l.last().expect("rendered");
        assert!(
            previous.abs() > 0.05,
            "the open hat was sounding: {previous}"
        );
        let mut largest = 0.0f32;
        for sample in after_l {
            largest = largest.max((sample - previous).abs());
            previous = sample;
        }
        assert!(largest < 0.01, "largest sample-to-sample step {largest}");
    }

    /// A choke group belongs to its channel: a kit on another channel does not
    /// cut this one.
    #[test]
    fn a_choke_group_does_not_reach_another_channel() {
        let mut player = kit_player(SoundfontPlayerMode::Multi);
        player
            .note_on(PERCUSSION_CHANNEL, 46, 127)
            .expect("channel 10 hat");
        let (open_l, _) = stereo_peaks(&mut player, 4_096);
        player.note_on(3, 42, 20).expect("channel 4 hat");
        let _ = stereo_peaks(&mut player, 2_205);
        let (left, _) = rendered(&mut player, 8_192);
        assert!(
            buffer_peak(&left) > open_l * 0.5,
            "channel 10's hat still rings: {} vs {open_l}",
            buffer_peak(&left)
        );
    }

    #[test]
    fn a_multi_player_keeps_every_note_on_the_channel_it_arrived_on() {
        let mut player = multi_player();
        player.set_channels(&melodic_parts());
        // Channel 10 plays its own part, not a reroute to channel 1.
        player
            .note_on(PERCUSSION_CHANNEL, 60, 100)
            .expect("note on");
        assert!(peak(&mut player, 2_048) > 0.01);
        assert_eq!(
            player.routed_channel(PERCUSSION_CHANNEL),
            PERCUSSION_CHANNEL
        );
        assert!(
            !player.is_drum_channel(PERCUSSION_CHANNEL),
            "a melodic part on 10"
        );
    }

    #[test]
    fn any_channel_can_hold_a_drum_kit() {
        let mut player = multi_player();
        let mut channels = melodic_parts();
        channels[3].preset = Some(test_font::DRUM_PRESET);
        player.set_channels(&channels);
        assert!(player.is_drum_channel(3));
        assert_eq!(player.channels()[3].preset, Some(test_font::DRUM_PRESET));
        player.note_on(3, 36, 100).expect("note on");
        assert!(
            peak(&mut player, 2_048) > 0.01,
            "the kit on channel 4 sounds"
        );
        // Back to a melodic preset.
        channels[3].preset = Some(test_font::MELODIC_PRESET);
        player.set_channels(&channels);
        assert!(!player.is_drum_channel(3));
    }

    #[test]
    fn a_preset_the_font_lacks_leaves_the_channel_on_its_last_one() {
        let mut player = multi_player();
        let mut channels = melodic_parts();
        player.set_channels(&channels);
        channels[0].preset = Some((0, 99));
        player.set_channels(&channels);
        assert_eq!(player.channels()[0].preset, Some(test_font::MELODIC_PRESET));
    }

    #[test]
    fn a_muted_part_drops_its_notes_and_releases_what_it_held() {
        let mut player = multi_player();
        let mut channels = melodic_parts();
        player.set_channels(&channels);
        player.note_on(1, 60, 100).expect("note on");
        let held = peak(&mut player, 2_048);
        assert!(held > 0.01);

        channels[1].mute = true;
        player.set_channels(&channels);
        let _ = peak(&mut player, 44_100);
        assert!(
            peak(&mut player, 2_048) < held * 0.05,
            "the held note released"
        );
        player.note_on(1, 64, 100).expect("note on");
        assert!(
            peak(&mut player, 2_048) < 1.0e-4,
            "a new note on a muted part is dropped"
        );
        // Other parts play on.
        player.note_on(2, 64, 100).expect("note on");
        assert!(peak(&mut player, 2_048) > 0.01);
    }

    #[test]
    fn solo_leaves_only_the_soloed_parts_sounding() {
        let mut player = multi_player();
        let mut channels = melodic_parts();
        channels[4].solo = true;
        player.set_channels(&channels);
        player.note_on(0, 60, 100).expect("note on");
        assert!(peak(&mut player, 2_048) < 1.0e-4);
        player.note_on(4, 60, 100).expect("note on");
        assert!(peak(&mut player, 2_048) > 0.01);
    }

    #[test]
    fn level_and_pan_are_the_channels_own_controllers() {
        let mut loud = multi_player();
        let mut quiet = multi_player();
        let mut channels = melodic_parts();
        loud.set_channels(&channels);
        channels[0].volume = 40;
        quiet.set_channels(&channels);
        loud.note_on(0, 60, 100).expect("note on");
        quiet.note_on(0, 60, 100).expect("note on");
        let (loud_l, _) = stereo_peaks(&mut loud, 4_096);
        let (quiet_l, _) = stereo_peaks(&mut quiet, 4_096);
        assert!(quiet_l < loud_l * 0.5, "loud={loud_l} quiet={quiet_l}");

        let mut left = multi_player();
        channels[0].volume = DEFAULT_CHANNEL_VOLUME;
        channels[0].pan = 0;
        left.set_channels(&channels);
        left.note_on(0, 60, 100).expect("note on");
        let (l, r) = stereo_peaks(&mut left, 4_096);
        assert!(r < l * 0.2, "hard left: l={l} r={r}");
    }

    #[test]
    fn a_reset_keeps_the_parts() {
        let mut player = multi_player();
        let mut channels = melodic_parts();
        channels[6].preset = Some(test_font::DRUM_PRESET);
        channels[2].mute = true;
        player.set_channels(&channels);
        player.reset();
        assert!(player.is_drum_channel(6));
        player.note_on(2, 60, 100).expect("note on");
        assert!(
            peak(&mut player, 2_048) < 1.0e-4,
            "still muted after a reset"
        );
    }

    #[test]
    fn a_single_player_ignores_parts() {
        let mut player = player();
        let mut channels = melodic_parts();
        channels[0].mute = true;
        player.set_channels(&channels);
        player.note_on(0, 60, 100).expect("note on");
        assert!(peak(&mut player, 2_048) > 0.01);
    }

    #[test]
    fn loads_synthetic_font_metadata() {
        let player = player();
        assert_eq!(player.bank_name(), test_font::BANK_NAME);
        assert_eq!(player.preset_count(), 2);
        assert!(player.has_preset(test_font::MELODIC_PRESET.0, test_font::MELODIC_PRESET.1));
        assert!(player.has_preset(test_font::DRUM_PRESET.0, test_font::DRUM_PRESET.1));
        assert!(!player.has_preset(0, 42));
        assert_eq!(
            player.preset_name(test_font::MELODIC_PRESET.0, test_font::MELODIC_PRESET.1),
            Some("Test Tone")
        );
    }

    #[test]
    fn note_on_renders_audio_and_note_off_releases_it() {
        let mut player = player();
        assert_eq!(peak(&mut player, 512), 0.0, "silent before any note");

        player.note_on(0, 60, 100).expect("note on");
        let held = peak(&mut player, 4_096);
        assert!(held > 0.01, "held note should render audio");

        // Reverb and chorus are on by default, so the voices stop but their
        // tail keeps decaying — assert the drop, not instant digital silence.
        player.all_notes_off(true);
        let released = peak(&mut player, 4_096);
        assert!(
            released < held * 0.5,
            "immediate all-notes-off should drop the level: held={held} released={released}"
        );
    }

    #[test]
    fn select_preset_all_channels_makes_every_melodic_channel_play() {
        let mut player = player();
        player
            .select_preset_all_channels(test_font::MELODIC_PRESET.0, test_font::MELODIC_PRESET.1)
            .expect("melodic preset selects");

        for channel in [0u8, 3, 15] {
            player.note_on(channel, 60, 100).expect("note on");
            assert!(
                peak(&mut player, 2_048) > 0.01,
                "channel {channel} should play the selected preset"
            );
            player.all_notes_off(true);
        }
    }

    #[test]
    fn drum_bank_preset_only_selects_on_the_percussion_channel() {
        let mut player = player();
        let (bank, patch) = test_font::DRUM_PRESET;

        player
            .select_preset(PERCUSSION_CHANNEL, bank, patch)
            .expect("drum preset selects on the percussion channel");

        let error = player.select_preset(0, bank, patch).unwrap_err();
        assert!(matches!(
            error,
            SoundfontPlayerError::PresetUnreachableOnChannel { channel: 0, .. }
        ));

        // A melodic preset is equally unreachable from the percussion channel.
        let (bank, patch) = test_font::MELODIC_PRESET;
        let error = player
            .select_preset(PERCUSSION_CHANNEL, bank, patch)
            .unwrap_err();
        assert!(matches!(
            error,
            SoundfontPlayerError::PresetUnreachableOnChannel { .. }
        ));
    }

    #[test]
    fn select_preset_all_channels_routes_a_drum_bank_to_percussion() {
        let mut player = player();
        player
            .select_preset_all_channels(test_font::DRUM_PRESET.0, test_font::DRUM_PRESET.1)
            .expect("drum preset routes to the percussion channel");

        player
            .note_on(PERCUSSION_CHANNEL, 60, 100)
            .expect("note on");
        assert!(peak(&mut player, 2_048) > 0.01);
    }

    #[test]
    fn missing_preset_is_reported_before_any_program_change() {
        let mut player = player();
        let error = player.select_preset(0, 0, 42).unwrap_err();
        assert!(matches!(
            error,
            SoundfontPlayerError::PresetNotFound { bank: 0, patch: 42 }
        ));
    }

    #[test]
    fn pitch_bend_controller_uses_the_bend_status_not_a_cc() {
        let mut player = player();
        player.note_on(0, 60, 100).expect("note on");
        let mut unbent = vec![0.0; 4_096];
        let mut scratch = vec![0.0; 4_096];
        player.render(&mut unbent, &mut scratch).expect("render");

        // CC 7 (channel volume) at 0 must silence the note; the pitch-bend
        // controller number must not be clamped into the CC range and do that.
        player
            .controller(0, CONTROLLER_PITCH_BEND, 127)
            .expect("pitch bend");
        let bent_peak = peak(&mut player, 4_096);
        assert!(bent_peak > 0.01, "pitch bend must not silence the note");

        player.controller(0, 7, 0).expect("channel volume");
        assert!(
            peak(&mut player, 8_192) < bent_peak,
            "channel volume 0 should reduce the rendered level"
        );
    }

    #[test]
    fn master_volume_scales_the_rendered_level() {
        let mut player = player();
        player.set_master_volume(1.0);
        player.note_on(0, 60, 100).expect("note on");
        let loud = peak(&mut player, 4_096);

        player.all_notes_off(true);
        player.set_master_volume(0.1);
        player.note_on(0, 60, 100).expect("note on");
        let quiet = peak(&mut player, 4_096);

        assert!(loud > quiet * 2.0, "loud={loud} quiet={quiet}");
    }

    #[test]
    fn mismatched_render_buffers_are_rejected_before_rustysynth_panics() {
        let mut player = player();
        let mut left = vec![0.0; 64];
        let mut right = vec![0.0; 32];
        let error = player.render(&mut left, &mut right).unwrap_err();
        assert!(matches!(
            error,
            SoundfontPlayerError::BufferLengthMismatch {
                left: 64,
                right: 32
            }
        ));
    }

    #[test]
    fn font_cache_reuses_one_parse_across_players() {
        let dir = std::env::temp_dir().join(format!(
            "futureboard-sf2-cache-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("test.sf2");
        test_font::write_sf2(&path).expect("write font");

        let (_, _, misses_before) = font_cache::stats();
        let first = SoundfontPlayer::from_path(&path, SoundfontPlayerSettings::default())
            .expect("first load");
        let (_, hits_before, misses_after_first) = font_cache::stats();
        assert_eq!(
            misses_after_first,
            misses_before + 1,
            "first load parses the file"
        );

        let second = SoundfontPlayer::from_path(&path, SoundfontPlayerSettings::default())
            .expect("second load");
        let (_, hits_after, misses_after_second) = font_cache::stats();
        assert_eq!(
            misses_after_second, misses_after_first,
            "second load must not re-parse"
        );
        assert_eq!(hits_after, hits_before + 1);
        assert!(Arc::ptr_eq(&first.sound_font(), &second.sound_font()));

        drop((first, second));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rebuilding_for_new_settings_reuses_the_parsed_font() {
        let font = test_font::sound_font();
        let player =
            SoundfontPlayer::from_sound_font(Arc::clone(&font), SoundfontPlayerSettings::default())
                .expect("build");
        let rebuilt = SoundfontPlayer::from_sound_font(
            player.sound_font(),
            SoundfontPlayerSettings {
                maximum_polyphony: 32,
                enable_reverb_and_chorus: false,
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("rebuild");

        assert!(Arc::ptr_eq(&font, &rebuilt.sound_font()));
        assert_eq!(rebuilt.maximum_polyphony(), 32);
        assert!(!rebuilt.enable_reverb_and_chorus());
    }

    #[test]
    fn default_ffi_config_uses_default_settings() {
        let config = SphereSoundfontPlayerConfig::default();
        let settings = config.into_settings();
        assert_eq!(settings.sample_rate, 44_100);
        assert!(settings.enable_reverb_and_chorus);
    }

    #[test]
    fn zero_sample_rate_ffi_config_uses_default() {
        let config = SphereSoundfontPlayerConfig {
            sample_rate: 0,
            ..SphereSoundfontPlayerConfig::default()
        };
        assert_eq!(config.into_settings().sample_rate, 44_100);
    }

    #[test]
    fn invalid_sample_rate_is_rejected() {
        let err = SoundfontPlayerSettings {
            sample_rate: -1,
            ..SoundfontPlayerSettings::default()
        }
        .to_rustysynth()
        .unwrap_err();
        assert!(matches!(err, SoundfontPlayerError::InvalidSampleRate(-1)));
    }

    #[test]
    fn a_drum_bank_preset_plays_from_the_channel_the_piano_roll_writes() {
        // The bug this covers: selecting a bank-128 kit only program-changed
        // channel 10, but a track's notes arrive on channel 1, so the kit never
        // sounded — channel 1 played whatever melodic preset it still held.
        let mut player = player();
        let (bank, patch) = test_font::DRUM_PRESET;
        player
            .select_preset_all_channels(bank, patch)
            .expect("drum preset selects");
        assert_eq!(player.selected_preset(), Some((bank, patch)));

        for channel in [0u8, 1, 7, 15] {
            assert_eq!(
                player.routed_channel(channel),
                PERCUSSION_CHANNEL,
                "channel {channel} must be routed to percussion"
            );
            player.note_on(channel, 60, 100).expect("note on");
            assert!(
                peak(&mut player, 2_048) > 0.01,
                "a drum kit must sound for a note written on channel {}",
                channel + 1
            );
            player.all_notes_off(true);
            peak(&mut player, 4_096); // let the tail settle before the next one
        }
    }

    #[test]
    fn a_drum_note_off_releases_the_voice_its_note_on_started() {
        let mut player = player();
        let (bank, patch) = test_font::DRUM_PRESET;
        player
            .select_preset_all_channels(bank, patch)
            .expect("drum preset selects");

        player.note_on(3, 60, 100).expect("note on channel 4");
        let held = peak(&mut player, 2_048);
        assert!(held > 0.01);
        // Routed to percussion on the way in, so it has to be routed the same
        // way on the way out or the note would hang.
        player.note_off(3, 60).expect("note off channel 4");
        let released = peak(&mut player, 8_192);
        assert!(
            released < held * 0.5,
            "note off must reach the routed voice: held={held} released={released}"
        );
    }

    #[test]
    fn a_melodic_preset_still_plays_for_a_note_written_on_channel_ten() {
        // The mirror of the drum case: `select_preset_all_channels` leaves
        // channel 10 on the font's drum banks, so a melodic track that happens
        // to use channel 10 would play a kit instead of its instrument.
        let mut player = player();
        let (bank, patch) = test_font::MELODIC_PRESET;
        player
            .select_preset_all_channels(bank, patch)
            .expect("melodic preset selects");

        assert_eq!(player.routed_channel(PERCUSSION_CHANNEL), 0);
        player
            .note_on(PERCUSSION_CHANNEL, 60, 100)
            .expect("note on channel 10");
        assert!(peak(&mut player, 2_048) > 0.01);
    }

    #[test]
    fn melodic_presets_leave_every_other_channel_alone() {
        // Per-channel routing has to survive, or per-note pitch bend and CC
        // would all collapse onto one channel.
        let mut player = player();
        player
            .select_preset_all_channels(test_font::MELODIC_PRESET.0, test_font::MELODIC_PRESET.1)
            .expect("melodic preset selects");
        for channel in [0u8, 1, 8, 10, 15] {
            assert_eq!(player.routed_channel(channel), channel);
        }
    }

    #[test]
    fn an_unselected_player_routes_nothing() {
        let player = player();
        assert_eq!(player.selected_preset(), None);
        for channel in [0u8, 9, 15] {
            assert_eq!(player.routed_channel(channel), channel);
        }
    }

    #[test]
    fn a_failed_preset_selection_does_not_start_routing() {
        let mut player = player();
        player
            .select_preset_all_channels(0, 42)
            .expect_err("preset is not in the font");
        assert_eq!(
            player.selected_preset(),
            None,
            "a rejected preset must not redirect notes"
        );
    }

    #[test]
    fn reset_drops_the_routing_with_the_preset_it_described() {
        let mut player = player();
        player
            .select_preset_all_channels(test_font::DRUM_PRESET.0, test_font::DRUM_PRESET.1)
            .expect("drum preset selects");
        player.reset();
        assert_eq!(player.selected_preset(), None);
        assert_eq!(player.routed_channel(0), 0);
    }

    #[test]
    fn a_default_player_reports_no_shaping() {
        let player = player();
        assert!(player.envelope().is_bypassed());
        assert_eq!(player.quality(), SoundfontRenderQuality::Standard);
        assert_eq!(player.latency_samples(), 0);
        assert_eq!(player.internal_sample_rate(), player.sample_rate());
    }

    #[test]
    fn an_attack_fades_the_instrument_in() {
        let mut shaped = SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings {
                sample_rate: 44_100,
                envelope: SoundfontEnvelope {
                    attack_ms: 250.0,
                    ..SoundfontEnvelope::default()
                },
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("shaped player");
        let mut plain = player();

        shaped.note_on(0, 60, 100).expect("note on");
        plain.note_on(0, 60, 100).expect("note on");
        // 512 frames is ~12 ms — well inside a 250 ms attack, so the shaped
        // player must still be far quieter than the unshaped one.
        let shaped_peak = peak(&mut shaped, 512);
        let plain_peak = peak(&mut plain, 512);
        assert!(
            shaped_peak < plain_peak * 0.25,
            "attack should fade in: shaped={shaped_peak} plain={plain_peak}"
        );

        // ...and catch up once the attack has run its course.
        let shaped_later = peak(&mut shaped, 44_100);
        assert!(
            shaped_later > plain_peak * 0.5,
            "attack should complete: {shaped_later}"
        );
    }

    #[test]
    fn a_release_fades_the_instrument_out_after_the_last_note() {
        let mut player = SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings {
                sample_rate: 44_100,
                envelope: SoundfontEnvelope {
                    release_ms: 20.0,
                    ..SoundfontEnvelope::default()
                },
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("shaped player");

        player.note_on(0, 60, 100).expect("note on");
        let held = peak(&mut player, 4_096);
        assert!(held > 0.01, "held note renders: {held}");

        player.note_off(0, 60).expect("note off");
        // 20 ms at 44.1 kHz is 882 samples, so this block contains the whole
        // ramp; the block after it is what must be exactly silent. Nothing
        // shorter would prove the point — the reverb tail alone keeps ringing.
        let ramp = peak(&mut player, 4_096);
        assert!(ramp > 0.0, "the release ramp itself still renders");
        let released = peak(&mut player, 4_096);
        assert_eq!(
            released, 0.0,
            "the envelope release must reach true silence, got {released}"
        );
    }

    #[test]
    fn the_sustain_pedal_keeps_the_envelope_open_after_the_key_lifts() {
        let mut player = SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings {
                sample_rate: 44_100,
                envelope: SoundfontEnvelope {
                    release_ms: 5.0,
                    ..SoundfontEnvelope::default()
                },
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("shaped player");

        player.controller(0, 64, 127).expect("pedal down");
        player.note_on(0, 60, 100).expect("note on");
        peak(&mut player, 2_048);
        player.note_off(0, 60).expect("key up under the pedal");
        let pedalled = peak(&mut player, 4_096);
        assert!(
            pedalled > 0.01,
            "the pedal must hold the envelope open: {pedalled}"
        );

        player.controller(0, 64, 0).expect("pedal up");
        peak(&mut player, 4_096); // consumes the 5 ms release ramp
        let released = peak(&mut player, 4_096);
        assert_eq!(released, 0.0, "pedal up releases the envelope");
    }

    #[test]
    fn an_oversampled_player_still_outputs_at_the_requested_rate() {
        let player = SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings {
                sample_rate: 48_000,
                quality: SoundfontRenderQuality::Ultra,
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("oversampled player");
        assert_eq!(player.sample_rate(), 48_000, "callers see the output rate");
        assert_eq!(player.internal_sample_rate(), 192_000);
        assert_eq!(player.latency_samples(), DECIMATOR_LATENCY_SAMPLES);
    }

    #[test]
    fn oversampled_rendering_produces_the_same_note_at_a_comparable_level() {
        // Not a bit-for-bit match — the whole point is that the signal path
        // differs — but a quality change must not alter the instrument's level
        // or silence it.
        let mut levels = Vec::new();
        for quality in SoundfontRenderQuality::ALL {
            let mut player = SoundfontPlayer::from_sound_font(
                test_font::sound_font(),
                SoundfontPlayerSettings {
                    sample_rate: 44_100,
                    quality,
                    max_render_frames: 512,
                    ..SoundfontPlayerSettings::default()
                },
            )
            .expect("player");
            player.note_on(0, 60, 100).expect("note on");
            levels.push((quality, peak(&mut player, 8_192)));
        }
        let (_, reference) = levels[0];
        assert!(reference > 0.01, "reference level {reference}");
        for (quality, level) in &levels[1..] {
            assert!(
                (*level - reference).abs() < reference * 0.35,
                "{quality:?} level {level} should track standard {reference}"
            );
        }
    }

    #[test]
    fn oversampled_rendering_handles_a_block_longer_than_its_preallocation() {
        let mut player = SoundfontPlayer::from_sound_font(
            test_font::sound_font(),
            SoundfontPlayerSettings {
                sample_rate: 44_100,
                quality: SoundfontRenderQuality::High,
                max_render_frames: 64,
                ..SoundfontPlayerSettings::default()
            },
        )
        .expect("player");
        player.note_on(0, 60, 100).expect("note on");
        assert!(peak(&mut player, 4_096) > 0.01, "long block still renders");
    }

    #[test]
    fn setting_an_envelope_needs_no_rebuild_and_takes_effect() {
        let mut player = player();
        assert!(player.envelope().is_bypassed());
        player.set_envelope(SoundfontEnvelope {
            attack_ms: 500.0,
            ..SoundfontEnvelope::default()
        });
        assert!(!player.envelope().is_bypassed());

        player.note_on(0, 60, 100).expect("note on");
        let early = peak(&mut player, 256);
        let late = peak(&mut player, 44_100);
        assert!(
            late > early * 2.0,
            "the new attack should still be ramping: early={early} late={late}"
        );
    }

    #[test]
    fn null_render_handle_is_rejected() {
        let status = unsafe {
            sphere_soundfont_player_render(ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), 16)
        };
        assert_eq!(status, SphereSoundfontPlayerStatus::NullPointer as i32);
    }
}
