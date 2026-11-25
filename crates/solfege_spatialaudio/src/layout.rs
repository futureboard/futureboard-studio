//! Speaker layouts, in the channel order a multichannel WAV and a
//! multichannel device expect (WAVE_FORMAT_EXTENSIBLE / SMPTE:
//! L R C LFE, then back, then side, then tops).
//!
//! Angles follow ITU-R BS.775 for 5.x and the Dolby placements for 7.x and
//! the height layers: azimuth in degrees clockwise from straight ahead (so
//! the right front speaker is at +30°), elevation in degrees above the ear.

use serde::{Deserialize, Serialize};

/// The most channels any layout here has (7.1.4).
pub const MAX_CHANNELS: usize = 12;

/// One speaker: where it is, and what it is called.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speaker {
    pub label: &'static str,
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    /// The low-frequency effects channel: not placed, fed by a send.
    pub lfe: bool,
}

const fn spk(label: &'static str, azimuth_deg: f32, elevation_deg: f32) -> Speaker {
    Speaker {
        label,
        azimuth_deg,
        elevation_deg,
        lfe: false,
    }
}

const LFE: Speaker = Speaker {
    label: "LFE",
    azimuth_deg: 0.0,
    elevation_deg: 0.0,
    lfe: true,
};

const L: Speaker = spk("L", -30.0, 0.0);
const R: Speaker = spk("R", 30.0, 0.0);
const C: Speaker = spk("C", 0.0, 0.0);

static STEREO: [Speaker; 2] = [L, R];
static LCR: [Speaker; 3] = [L, R, C];
static QUAD: [Speaker; 4] = [
    spk("L", -45.0, 0.0),
    spk("R", 45.0, 0.0),
    spk("Ls", -135.0, 0.0),
    spk("Rs", 135.0, 0.0),
];
static S50: [Speaker; 5] = [L, R, C, spk("Ls", -110.0, 0.0), spk("Rs", 110.0, 0.0)];
static S51: [Speaker; 6] = [L, R, C, LFE, spk("Ls", -110.0, 0.0), spk("Rs", 110.0, 0.0)];
static S70: [Speaker; 7] = [
    L,
    R,
    C,
    spk("Lrs", -150.0, 0.0),
    spk("Rrs", 150.0, 0.0),
    spk("Lss", -90.0, 0.0),
    spk("Rss", 90.0, 0.0),
];
static S71: [Speaker; 8] = [
    L,
    R,
    C,
    LFE,
    spk("Lrs", -150.0, 0.0),
    spk("Rrs", 150.0, 0.0),
    spk("Lss", -90.0, 0.0),
    spk("Rss", 90.0, 0.0),
];
static S712: [Speaker; 10] = [
    L,
    R,
    C,
    LFE,
    spk("Lrs", -150.0, 0.0),
    spk("Rrs", 150.0, 0.0),
    spk("Lss", -90.0, 0.0),
    spk("Rss", 90.0, 0.0),
    spk("Ltm", -90.0, 60.0),
    spk("Rtm", 90.0, 60.0),
];
static S714: [Speaker; 12] = [
    L,
    R,
    C,
    LFE,
    spk("Lrs", -150.0, 0.0),
    spk("Rrs", 150.0, 0.0),
    spk("Lss", -90.0, 0.0),
    spk("Rss", 90.0, 0.0),
    spk("Ltf", -45.0, 45.0),
    spk("Rtf", 45.0, 45.0),
    spk("Ltr", -135.0, 45.0),
    spk("Rtr", 135.0, 45.0),
];

/// A speaker arrangement for surround output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SpeakerLayout {
    #[default]
    Stereo,
    Lcr,
    Quad,
    Surround50,
    Surround51,
    Surround70,
    Surround71,
    Surround712,
    Surround714,
}

impl SpeakerLayout {
    /// Every layout, smallest first — for pickers.
    pub const ALL: [SpeakerLayout; 9] = [
        Self::Stereo,
        Self::Lcr,
        Self::Quad,
        Self::Surround50,
        Self::Surround51,
        Self::Surround70,
        Self::Surround71,
        Self::Surround712,
        Self::Surround714,
    ];

    pub fn speakers(self) -> &'static [Speaker] {
        match self {
            Self::Stereo => &STEREO,
            Self::Lcr => &LCR,
            Self::Quad => &QUAD,
            Self::Surround50 => &S50,
            Self::Surround51 => &S51,
            Self::Surround70 => &S70,
            Self::Surround71 => &S71,
            Self::Surround712 => &S712,
            Self::Surround714 => &S714,
        }
    }

    pub fn channel_count(self) -> usize {
        self.speakers().len()
    }

    /// Index of the LFE channel, if the layout has one.
    pub fn lfe_channel(self) -> Option<usize> {
        self.speakers().iter().position(|speaker| speaker.lfe)
    }

    /// Whether any speaker sits above the listener.
    pub fn has_height(self) -> bool {
        self.speakers()
            .iter()
            .any(|speaker| speaker.elevation_deg >= 20.0)
    }

    /// Which speaker each channel feeds, as a WAV `dwChannelMask`: the
    /// channels are already in the mask's bit order. 7.1.2's top-middle pair
    /// has no bit of its own and is written as the top-front pair, as is
    /// usual for that layout in a WAV file.
    pub fn wav_channel_mask(self) -> u32 {
        const FL: u32 = 0x1;
        const FR: u32 = 0x2;
        const FC: u32 = 0x4;
        const LFE: u32 = 0x8;
        const BL: u32 = 0x10;
        const BR: u32 = 0x20;
        const SL: u32 = 0x200;
        const SR: u32 = 0x400;
        const TFL: u32 = 0x1000;
        const TFR: u32 = 0x4000;
        const TBL: u32 = 0x8000;
        const TBR: u32 = 0x20000;
        match self {
            Self::Stereo => FL | FR,
            Self::Lcr => FL | FR | FC,
            Self::Quad => FL | FR | BL | BR,
            Self::Surround50 => FL | FR | FC | BL | BR,
            Self::Surround51 => FL | FR | FC | LFE | BL | BR,
            Self::Surround70 => FL | FR | FC | BL | BR | SL | SR,
            Self::Surround71 => FL | FR | FC | LFE | BL | BR | SL | SR,
            Self::Surround712 => FL | FR | FC | LFE | BL | BR | SL | SR | TFL | TFR,
            Self::Surround714 => FL | FR | FC | LFE | BL | BR | SL | SR | TFL | TFR | TBL | TBR,
        }
    }

    /// The conventional name: `5.1`, `7.1.4`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo",
            Self::Lcr => "LCR",
            Self::Quad => "Quad",
            Self::Surround50 => "5.0",
            Self::Surround51 => "5.1",
            Self::Surround70 => "7.0",
            Self::Surround71 => "7.1",
            Self::Surround712 => "7.1.2",
            Self::Surround714 => "7.1.4",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_carry_the_channels_their_names_promise() {
        let counts: Vec<(&str, usize)> = SpeakerLayout::ALL
            .iter()
            .map(|layout| (layout.name(), layout.channel_count()))
            .collect();
        assert_eq!(
            counts,
            vec![
                ("Stereo", 2),
                ("LCR", 3),
                ("Quad", 4),
                ("5.0", 5),
                ("5.1", 6),
                ("7.0", 7),
                ("7.1", 8),
                ("7.1.2", 10),
                ("7.1.4", 12),
            ]
        );
        assert!(
            SpeakerLayout::ALL
                .iter()
                .all(|layout| layout.channel_count() <= MAX_CHANNELS)
        );
    }

    #[test]
    fn each_layout_names_one_speaker_bit_per_channel() {
        for layout in SpeakerLayout::ALL {
            assert_eq!(
                layout.wav_channel_mask().count_ones() as usize,
                layout.channel_count(),
                "{}",
                layout.name()
            );
        }
        assert_eq!(SpeakerLayout::Surround51.wav_channel_mask(), 0x3F);
        assert_eq!(SpeakerLayout::Surround71.wav_channel_mask(), 0x63F);
    }

    #[test]
    fn the_lfe_sits_in_the_fourth_channel_as_wav_expects() {
        assert_eq!(SpeakerLayout::Surround51.lfe_channel(), Some(3));
        assert_eq!(SpeakerLayout::Surround71.lfe_channel(), Some(3));
        assert_eq!(SpeakerLayout::Surround714.lfe_channel(), Some(3));
        assert_eq!(SpeakerLayout::Surround50.lfe_channel(), None);
    }

    #[test]
    fn only_the_height_layouts_have_height() {
        assert!(SpeakerLayout::Surround714.has_height());
        assert!(SpeakerLayout::Surround712.has_height());
        assert!(!SpeakerLayout::Surround71.has_height());
    }
}
