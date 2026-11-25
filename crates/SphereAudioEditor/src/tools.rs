//! Audio editor tool kinds, targeting, and session state.
//!
//! Windowed tools live in the host (`AudioToolWindowManager`). This module is
//! the UI-agnostic contract: what a tool is, which selection it accepts, and
//! the session a window binds to a clip.

use crate::editing::{AudioRangeSelection, SpectralSelection};

/// Tools that open an independent GPUI window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioToolKind {
    SpectrumAnalyzer,
    Loudness,
    Normalize,
    TransientDetector,
    TimePitch,
    Resample,
    ChannelTools,
    PhaseAnalyzer,
    DcOffset,
    BpmAnalysis,
    KeyAnalysis,
    AudioRepair,
    SpectralProcessor,
    SpectrogramSettings,
}

impl AudioToolKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::SpectrumAnalyzer => "Spectrum Analyzer",
            Self::Loudness => "Loudness Analyzer",
            Self::Normalize => "Normalize",
            Self::TransientDetector => "Transient Detector",
            Self::TimePitch => "Time & Pitch",
            Self::Resample => "Resample",
            Self::ChannelTools => "Channel Tools",
            Self::PhaseAnalyzer => "Phase Analyzer",
            Self::DcOffset => "DC Offset",
            Self::BpmAnalysis => "BPM Analyzer",
            Self::KeyAnalysis => "Key Analyzer",
            Self::AudioRepair => "Audio Repair",
            Self::SpectralProcessor => "Spectral Processing",
            Self::SpectrogramSettings => "Spectrogram Settings",
        }
    }

    pub const fn is_analysis_only(self) -> bool {
        matches!(
            self,
            Self::SpectrumAnalyzer
                | Self::Loudness
                | Self::PhaseAnalyzer
                | Self::BpmAnalysis
                | Self::KeyAnalysis
                | Self::SpectrogramSettings
        )
    }

    pub const fn default_follow_selection(self) -> bool {
        self.is_analysis_only()
    }

    pub const fn supports_clip(self) -> bool {
        !matches!(self, Self::SpectrogramSettings)
    }

    pub const fn supports_time_selection(self) -> bool {
        matches!(
            self,
            Self::SpectrumAnalyzer
                | Self::Loudness
                | Self::Normalize
                | Self::TransientDetector
                | Self::DcOffset
                | Self::BpmAnalysis
                | Self::KeyAnalysis
                | Self::AudioRepair
                | Self::SpectralProcessor
                | Self::PhaseAnalyzer
        )
    }

    pub const fn supports_spectral_selection(self) -> bool {
        matches!(self, Self::AudioRepair | Self::SpectralProcessor)
    }

    pub const fn default_size(self) -> (f32, f32) {
        match self {
            Self::SpectrumAnalyzer => (720.0, 480.0),
            Self::Loudness => (640.0, 420.0),
            Self::Normalize => (640.0, 440.0),
            Self::TransientDetector => (720.0, 440.0),
            Self::TimePitch => (640.0, 420.0),
            Self::Resample => (560.0, 380.0),
            Self::ChannelTools => (560.0, 400.0),
            Self::PhaseAnalyzer => (560.0, 440.0),
            Self::DcOffset => (560.0, 360.0),
            Self::BpmAnalysis => (560.0, 400.0),
            Self::KeyAnalysis => (560.0, 400.0),
            // The repair surface is a canvas workspace, not a dialog: it needs
            // room for the labeled module navigator, the time-frequency canvas,
            // and the contextual inspector side by side.
            Self::AudioRepair => (1232.0, 660.0),
            Self::SpectralProcessor => (720.0, 440.0),
            Self::SpectrogramSettings => (560.0, 360.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioToolTarget {
    pub clip_id: String,
    pub source_id: String,
    pub clip_name: String,
    pub file_label: String,
    pub source_path: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub source_frames: i64,
    pub time_selection: Option<AudioRangeSelection>,
    pub spectral_selection: Option<SpectralSelection>,
}

impl AudioToolTarget {
    pub fn summary_line(&self) -> String {
        let ch = match self.channels {
            1 => "Mono",
            2 => "Stereo",
            n => return format!("{} · {} Hz · {} ch", self.file_label, self.sample_rate, n),
        };
        format!("{} · {} Hz · {}", self.file_label, self.sample_rate, ch)
    }

    pub fn target_label(&self, kind: AudioToolKind) -> &'static str {
        if kind.supports_spectral_selection() && self.spectral_selection.is_some() {
            "Spectral Selection"
        } else if kind.supports_time_selection() && self.time_selection.is_some() {
            "Time Selection"
        } else {
            "Clip"
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioToolSession {
    pub tool_kind: AudioToolKind,
    pub target: AudioToolTarget,
    pub source_revision: u64,
    pub preview_enabled: bool,
    pub preview_bypassed: bool,
    pub follow_selection: bool,
    pub pin_target: bool,
    pub dirty: bool,
    pub analyzing: bool,
    pub analyze_progress: f32,
}

impl AudioToolSession {
    pub fn new(tool_kind: AudioToolKind, target: AudioToolTarget) -> Self {
        Self {
            follow_selection: tool_kind.default_follow_selection(),
            pin_target: !tool_kind.default_follow_selection(),
            tool_kind,
            target,
            source_revision: 0,
            preview_enabled: false,
            preview_bypassed: false,
            dirty: false,
            analyzing: false,
            analyze_progress: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRepairModule {
    Denoise,
    DeClick,
    DeHum,
    DeReverb,
    DeBleed,
    DeFeedback,
    DrumSilencer,
    SpectralRepair,
}

impl AudioRepairModule {
    /// Navigator order. Available modules lead; unimplemented ones stay visible
    /// and disabled so the list does not silently change length between builds.
    pub const ALL: [Self; 8] = [
        Self::Denoise,
        Self::DeClick,
        Self::DeHum,
        Self::SpectralRepair,
        Self::DeReverb,
        Self::DeBleed,
        Self::DeFeedback,
        Self::DrumSilencer,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Denoise => "Noise Reduction",
            Self::DeClick => "De-Click",
            Self::DeHum => "De-Hum",
            Self::DeReverb => "De-Reverb",
            Self::DeBleed => "De-Bleed",
            Self::DeFeedback => "De-Feedback",
            Self::DrumSilencer => "Drum Silencer",
            Self::SpectralRepair => "Spectral Repair",
        }
    }

    /// One-word label for space-constrained chrome.
    ///
    /// Still a word, never an initialism: the navigator must be recognizable at
    /// a glance rather than decoded.
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Denoise => "Noise",
            Self::DeClick => "Click",
            Self::DeHum => "Hum",
            Self::DeReverb => "Reverb",
            Self::DeBleed => "Bleed",
            Self::DeFeedback => "Feedback",
            Self::DrumSilencer => "Drum",
            Self::SpectralRepair => "Spectral",
        }
    }

    /// What the module fixes, in a few plain words. The navigator's second
    /// line, so a row says what it is for and not only what it is called.
    pub const fn purpose(self) -> &'static str {
        match self {
            Self::Denoise => "Hiss and background noise",
            Self::DeClick => "Clicks, pops and crackle",
            Self::DeHum => "Mains hum and buzz",
            Self::SpectralRepair => "Paint out a single sound",
            Self::DeReverb => "Room echo",
            Self::DeBleed => "Spill from other mics",
            Self::DeFeedback => "Ringing feedback",
            Self::DrumSilencer => "Drums inside a mix",
        }
    }

    /// One or two sentences on what the module does and how to use it, for
    /// the top of the inspector.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Denoise => {
                "Removes steady noise such as hiss, fans or air conditioning. \
                 Teach it what the noise sounds like, then choose how much to take away."
            }
            Self::DeClick => {
                "Finds short clicks and pops and smooths them over. \
                 Set how hard it looks, then step through what it found."
            }
            Self::DeHum => {
                "Removes the hum mains power leaves in a recording, \
                 together with its overtones."
            }
            Self::SpectralRepair => {
                "Turns down one sound you draw around on the spectrogram, \
                 like a cough, a phone or a squeak."
            }
            Self::DeReverb | Self::DeBleed | Self::DeFeedback | Self::DrumSilencer => {
                "Not available in this build yet."
            }
        }
    }

    /// What the module draws on the processing canvas. Shown in the inspector
    /// so the overlay is self-describing.
    pub const fn canvas_hint(self) -> &'static str {
        match self {
            Self::Denoise => "Gated time-frequency cells are shaded on the canvas.",
            Self::DeClick => "Detected impulses are marked at their source frame.",
            Self::DeHum => "Notch bands track the fundamental and its harmonics.",
            Self::SpectralRepair => "Draw a time-frequency region on the canvas to target it.",
            Self::DeReverb | Self::DeBleed | Self::DeFeedback | Self::DrumSilencer => {
                "No processor is connected yet."
            }
        }
    }

    pub const fn is_available(self) -> bool {
        matches!(
            self,
            Self::Denoise | Self::DeClick | Self::DeHum | Self::SpectralRepair
        )
    }

    /// Whether the module's result depends on a time-frequency region the user
    /// draws on the canvas.
    pub const fn uses_spectral_selection(self) -> bool {
        matches!(self, Self::SpectralRepair)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzers_follow_selection_by_default() {
        assert!(AudioToolKind::SpectrumAnalyzer.default_follow_selection());
        assert!(!AudioToolKind::Normalize.default_follow_selection());
    }

    #[test]
    fn every_repair_module_explains_itself() {
        for module in AudioRepairModule::ALL {
            assert!(!module.purpose().is_empty());
            assert!(module.summary().ends_with('.'));
        }
    }

    #[test]
    fn unavailable_repair_modules_are_marked() {
        assert!(AudioRepairModule::Denoise.is_available());
        assert!(!AudioRepairModule::DeReverb.is_available());
    }

    #[test]
    fn the_rail_lists_every_module_once_with_available_ones_first() {
        let all = AudioRepairModule::ALL;
        for module in all {
            assert_eq!(all.iter().filter(|other| **other == module).count(), 1);
        }
        let first_unavailable = all
            .iter()
            .position(|module| !module.is_available())
            .unwrap_or(all.len());
        assert!(all[..first_unavailable].iter().all(|m| m.is_available()));
        assert!(all[first_unavailable..].iter().all(|m| !m.is_available()));
    }
}
