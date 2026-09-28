//! What a project's mix is rendered as.

use serde::{Deserialize, Serialize};

use crate::layout::SpeakerLayout;

/// The project's output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SpatialFormat {
    /// The ordinary stereo mix: channels pan with their pan control.
    #[default]
    Stereo,
    /// Headphones: every channel placed in the square room around a head.
    Binaural,
    /// A speaker layout: every channel placed in the room and panned over
    /// its speakers.
    Surround(SpeakerLayout),
}

impl SpatialFormat {
    /// The formats a project can choose, in picker order.
    pub const CHOICES: [SpatialFormat; 8] = [
        Self::Stereo,
        Self::Binaural,
        Self::Surround(SpeakerLayout::Quad),
        Self::Surround(SpeakerLayout::Surround51),
        Self::Surround(SpeakerLayout::Surround71),
        Self::Surround(SpeakerLayout::Surround712),
        Self::Surround(SpeakerLayout::Surround714),
        Self::Surround(SpeakerLayout::Lcr),
    ];

    /// Channels the mix bus carries.
    pub fn channel_count(self) -> usize {
        match self {
            Self::Stereo | Self::Binaural => 2,
            Self::Surround(layout) => layout.channel_count(),
        }
    }

    /// Whether channels are placed in the room rather than panned.
    pub fn is_spatial(self) -> bool {
        !matches!(self, Self::Stereo | Self::Surround(SpeakerLayout::Stereo))
    }

    /// Whether a channel's height is heard: headphones place it above the
    /// head, and a layout with a top ring pans up into it. A flat layout
    /// ignores it.
    pub fn has_height(self) -> bool {
        match self {
            Self::Stereo => false,
            Self::Binaural => true,
            Self::Surround(layout) => layout.has_height(),
        }
    }

    /// Whether the mix has an LFE channel for a channel's LFE send.
    pub fn has_lfe(self) -> bool {
        matches!(self, Self::Surround(layout) if layout.lfe_channel().is_some())
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo",
            Self::Binaural => "Binaural",
            Self::Surround(layout) => layout.name(),
        }
    }

    /// A stable token for persistence and menus (`"stereo"`, `"binaural"`,
    /// `"5.1"`).
    pub fn token(self) -> &'static str {
        match self {
            Self::Stereo => "stereo",
            Self::Binaural => "binaural",
            Self::Surround(SpeakerLayout::Stereo) => "stereo",
            Self::Surround(layout) => layout.name(),
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "stereo" => Some(Self::Stereo),
            "binaural" => Some(Self::Binaural),
            other => SpeakerLayout::ALL
                .iter()
                .find(|layout| layout.name().eq_ignore_ascii_case(other))
                .map(|layout| Self::Surround(*layout)),
        }
    }
}

/// How a surround mix reaches a device with too few outputs for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MonitorFold {
    /// Every speaker rendered binaurally where it stands: for headphones.
    #[default]
    Binaural,
    /// The ITU stereo fold-down: for a pair of speakers.
    Stereo,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip() {
        for format in SpatialFormat::CHOICES {
            assert_eq!(SpatialFormat::from_token(format.token()), Some(format));
        }
        assert_eq!(SpatialFormat::from_token("nonsense"), None);
    }

    #[test]
    fn a_format_carries_its_layouts_channels() {
        assert_eq!(SpatialFormat::Binaural.channel_count(), 2);
        assert_eq!(
            SpatialFormat::Surround(SpeakerLayout::Surround71).channel_count(),
            8
        );
        assert!(!SpatialFormat::Stereo.is_spatial());
        assert!(SpatialFormat::Binaural.is_spatial());
    }

    #[test]
    fn height_and_lfe_follow_what_the_format_can_play() {
        let surround = SpatialFormat::Surround;
        assert!(SpatialFormat::Binaural.has_height());
        assert!(!SpatialFormat::Binaural.has_lfe());
        assert!(!surround(SpeakerLayout::Surround51).has_height());
        assert!(surround(SpeakerLayout::Surround51).has_lfe());
        assert!(surround(SpeakerLayout::Surround714).has_height());
        assert!(!surround(SpeakerLayout::Quad).has_lfe());
        assert!(!SpatialFormat::Stereo.has_height());
    }
}
