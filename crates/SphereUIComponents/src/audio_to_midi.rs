//! Hand-off point for an edition-provided audio-to-MIDI converter.
//!
//! An audio clip's menu offers "Convert to MIDI" only while a converter is
//! installed here. The shared crate holds no transcription code: a
//! Professional build's app layer installs the converter when a verified
//! license allows it and removes it when the license lapses, as it does the
//! score view ([`crate::score_view`]). A Community build installs nothing and
//! the menu never shows the command.
//!
//! The converter only listens: it is handed mono samples and answers notes
//! in seconds. Reading the clip's audio, placing the notes on the timeline
//! and the undo step are Studio's own (`layout::audio_to_midi_ops`).

use std::sync::{Arc, OnceLock, RwLock};

/// What kind of audio the clip holds, which decides how it is heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConvertMode {
    /// One note at a time: a voice, a bass line, a lead.
    Melody,
    /// Kick, snare and hi-hat hits, as General MIDI drum notes.
    Drums,
    /// Several notes at once: piano, guitar, pads.
    Chords,
}

impl ConvertMode {
    pub const ALL: [ConvertMode; 3] = [Self::Melody, Self::Drums, Self::Chords];

    /// The menu's words for it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Melody => "Melody",
            Self::Drums => "Drums",
            Self::Chords => "Chords",
        }
    }

    /// The command that runs it on the clip the menu was opened on.
    pub fn command(self) -> &'static str {
        match self {
            Self::Melody => "clip:to-midi-melody",
            Self::Drums => "clip:to-midi-drums",
            Self::Chords => "clip:to-midi-chords",
        }
    }

    pub fn from_command(command: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.command() == command)
    }
}

/// One note the converter heard, in seconds from the start of the audio it
/// was handed.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedNote {
    pub start_seconds: f64,
    pub duration_seconds: f64,
    pub pitch: u8,
    pub velocity: u8,
    /// How the note's pitch moves: `(seconds from the note's start, cents)`.
    /// Empty for a steady note.
    pub bend: Vec<(f64, f32)>,
}

/// Turns mono samples at a sample rate into notes. Called off the UI thread,
/// on a background executor; it may take seconds on a long take.
pub type AudioToMidiConverter =
    Arc<dyn Fn(&[f32], u32, ConvertMode) -> Vec<ConvertedNote> + Send + Sync + 'static>;

fn slot() -> &'static RwLock<Option<AudioToMidiConverter>> {
    static SLOT: OnceLock<RwLock<Option<AudioToMidiConverter>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Install (`Some`) or remove (`None`) the converter.
pub fn set_audio_to_midi_converter(converter: Option<AudioToMidiConverter>) {
    if let Ok(mut guard) = slot().write() {
        *guard = converter;
    }
}

/// The installed converter, if this build and license offer one.
pub fn audio_to_midi_converter() -> Option<AudioToMidiConverter> {
    slot().read().ok().and_then(|guard| guard.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_to_modes() {
        for mode in ConvertMode::ALL {
            assert_eq!(ConvertMode::from_command(mode.command()), Some(mode));
        }
        assert_eq!(ConvertMode::from_command("clip:rename"), None);
    }
}
