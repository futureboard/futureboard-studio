//! Per-track program and bank selection, and the patch names of the GM, GS
//! and XG sound sets.
//!
//! A track's selection is sent as Bank Select MSB (CC 0), Bank Select LSB
//! (CC 32) and Program Change, in that order: a device applies the bank it
//! was last given when the program change arrives. General MIDI (level 1)
//! has no banks, so a GM selection sends the program change alone.
//!
//! The names cover the GM level 1 sound set, which is also the capital
//! (bank 0) set of GS and the normal-voice bank 0 of XG, plus the drum kits of
//! each format. A variation bank has no name table here: its patches are shown
//! as the capital tone they vary, with the bank number.

use serde::{Deserialize, Serialize};

/// Which sound set's bank layout and patch names a track follows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MidiPatchFormat {
    /// General MIDI (level 1): 128 programs, no banks.
    #[default]
    Gm,
    /// Roland GS: bank MSB selects the variation, LSB the map.
    Gs,
    /// Yamaha XG: bank MSB selects the voice type (normal, SFX, drums), LSB
    /// the bank within it.
    Xg,
}

impl MidiPatchFormat {
    pub const ALL: [Self; 3] = [Self::Gm, Self::Gs, Self::Xg];

    pub fn label(self) -> &'static str {
        match self {
            Self::Gm => "GM",
            Self::Gs => "GS",
            Self::Xg => "XG",
        }
    }

    pub const fn to_tag(self) -> u8 {
        match self {
            Self::Gm => 0,
            Self::Gs => 1,
            Self::Xg => 2,
        }
    }

    pub const fn from_tag(tag: u8) -> Self {
        match tag {
            1 => Self::Gs,
            2 => Self::Xg,
            _ => Self::Gm,
        }
    }

    /// Whether this format selects banks at all.
    pub const fn has_banks(self) -> bool {
        !matches!(self, Self::Gm)
    }
}

/// XG bank MSB of the drum kits.
pub const XG_DRUM_BANK_MSB: u8 = 127;
/// XG bank MSB of the SFX drum kits.
pub const XG_SFX_KIT_BANK_MSB: u8 = 126;
/// XG bank MSB of the SFX voices.
pub const XG_SFX_BANK_MSB: u8 = 64;

/// A track's program and bank. `program: None` sends nothing: the instrument
/// keeps whatever it is set to, which is what a track that never chose a patch
/// has always done.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MidiProgramSelection {
    pub format: MidiPatchFormat,
    /// `0..=127`.
    pub program: Option<u8>,
    /// Bank Select MSB, `0..=127`. Not sent for GM.
    pub bank_msb: u8,
    /// Bank Select LSB, `0..=127`. Not sent for GM.
    pub bank_lsb: u8,
}

/// One channel message of a program selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramMessage {
    /// Controller number and 7-bit value (CC 0 or CC 32).
    BankSelect {
        controller: u8,
        value: u8,
    },
    ProgramChange {
        program: u8,
    },
}

impl ProgramMessage {
    /// The raw MIDI bytes on `channel` (`0..=15`) and how many are used.
    pub fn bytes(self, channel: u8) -> ([u8; 3], usize) {
        let channel = channel & 0x0F;
        match self {
            Self::BankSelect { controller, value } => {
                ([0xB0 | channel, controller & 0x7F, value & 0x7F], 3)
            }
            Self::ProgramChange { program } => ([0xC0 | channel, program & 0x7F, 0], 2),
        }
    }
}

impl MidiProgramSelection {
    pub fn sanitized(self) -> Self {
        Self {
            program: self.program.map(|program| program.min(127)),
            bank_msb: self.bank_msb.min(127),
            bank_lsb: self.bank_lsb.min(127),
            ..self
        }
    }

    /// What the selection sends, in order: bank MSB, bank LSB, program.
    /// Empty when no program is chosen.
    pub fn messages(self) -> impl Iterator<Item = ProgramMessage> {
        let selection = self.sanitized();
        let banks = selection.program.is_some() && selection.format.has_banks();
        let bank = banks.then_some([
            ProgramMessage::BankSelect {
                controller: 0,
                value: selection.bank_msb,
            },
            ProgramMessage::BankSelect {
                controller: 32,
                value: selection.bank_lsb,
            },
        ]);
        bank.into_iter().flatten().chain(
            selection
                .program
                .map(|program| ProgramMessage::ProgramChange { program }),
        )
    }

    /// The bank as `(msb, lsb)` when this format sends one.
    pub fn bank(self) -> Option<(u8, u8)> {
        self.format
            .has_banks()
            .then_some((self.bank_msb.min(127), self.bank_lsb.min(127)))
    }

    /// Whether `program` on this bank picks a drum kit rather than a
    /// melodic patch. XG says so by bank; GM and GS by the part, which is
    /// channel 10 (`channel` is 1-based here, as the track shows it).
    pub fn is_drum_bank(self, channel: u8) -> bool {
        match self.format {
            MidiPatchFormat::Xg => {
                matches!(self.bank_msb, XG_DRUM_BANK_MSB | XG_SFX_KIT_BANK_MSB)
            }
            MidiPatchFormat::Gm | MidiPatchFormat::Gs => channel == 10,
        }
    }

    /// The name of `program` in this selection's format and bank, on a track
    /// playing on `channel` (1-based).
    pub fn patch_name(self, program: u8, channel: u8) -> String {
        patch_name(self.format, self.bank_msb, self.bank_lsb, program, channel)
    }
}

/// The name of `program` (`0..=127`) in `format`, on `bank_msb`/`bank_lsb`,
/// played on `channel` (1-based). Numbers are 1-based in the label, the way
/// every patch list prints them.
pub fn patch_name(
    format: MidiPatchFormat,
    bank_msb: u8,
    bank_lsb: u8,
    program: u8,
    channel: u8,
) -> String {
    let program = program.min(127);
    let number = u16::from(program) + 1;
    let selection = MidiProgramSelection {
        format,
        program: Some(program),
        bank_msb,
        bank_lsb,
    };
    if selection.is_drum_bank(channel) {
        let kits = match (format, bank_msb) {
            (MidiPatchFormat::Xg, XG_SFX_KIT_BANK_MSB) => XG_SFX_KITS,
            (MidiPatchFormat::Xg, _) => XG_DRUM_KITS,
            _ => GS_DRUM_KITS,
        };
        return match kits.iter().find(|(kit, _)| *kit == program) {
            Some((_, name)) => format!("{number} {name}"),
            None => format!("{number} Drum Kit"),
        };
    }
    let base = GM_PROGRAM_NAMES[program as usize];
    match format {
        MidiPatchFormat::Gm => format!("{number} {base}"),
        MidiPatchFormat::Gs if bank_msb == 0 => format!("{number} {base}"),
        MidiPatchFormat::Gs => format!("{number} {base} (Var {bank_msb})"),
        MidiPatchFormat::Xg if bank_msb == XG_SFX_BANK_MSB => format!("{number} SFX Voice"),
        MidiPatchFormat::Xg if bank_msb == 0 && bank_lsb == 0 => format!("{number} {base}"),
        MidiPatchFormat::Xg => format!("{number} {base} (Bank {bank_msb}/{bank_lsb})"),
    }
}

/// The General MIDI level 1 sound set, by program number (0-based).
pub const GM_PROGRAM_NAMES: [&str; 128] = [
    // Piano
    "Acoustic Grand Piano",
    "Bright Acoustic Piano",
    "Electric Grand Piano",
    "Honky-tonk Piano",
    "Electric Piano 1",
    "Electric Piano 2",
    "Harpsichord",
    "Clavinet",
    // Chromatic Percussion
    "Celesta",
    "Glockenspiel",
    "Music Box",
    "Vibraphone",
    "Marimba",
    "Xylophone",
    "Tubular Bells",
    "Dulcimer",
    // Organ
    "Drawbar Organ",
    "Percussive Organ",
    "Rock Organ",
    "Church Organ",
    "Reed Organ",
    "Accordion",
    "Harmonica",
    "Tango Accordion",
    // Guitar
    "Acoustic Guitar (nylon)",
    "Acoustic Guitar (steel)",
    "Electric Guitar (jazz)",
    "Electric Guitar (clean)",
    "Electric Guitar (muted)",
    "Overdriven Guitar",
    "Distortion Guitar",
    "Guitar Harmonics",
    // Bass
    "Acoustic Bass",
    "Electric Bass (finger)",
    "Electric Bass (pick)",
    "Fretless Bass",
    "Slap Bass 1",
    "Slap Bass 2",
    "Synth Bass 1",
    "Synth Bass 2",
    // Strings
    "Violin",
    "Viola",
    "Cello",
    "Contrabass",
    "Tremolo Strings",
    "Pizzicato Strings",
    "Orchestral Harp",
    "Timpani",
    // Ensemble
    "String Ensemble 1",
    "String Ensemble 2",
    "Synth Strings 1",
    "Synth Strings 2",
    "Choir Aahs",
    "Voice Oohs",
    "Synth Voice",
    "Orchestra Hit",
    // Brass
    "Trumpet",
    "Trombone",
    "Tuba",
    "Muted Trumpet",
    "French Horn",
    "Brass Section",
    "Synth Brass 1",
    "Synth Brass 2",
    // Reed
    "Soprano Sax",
    "Alto Sax",
    "Tenor Sax",
    "Baritone Sax",
    "Oboe",
    "English Horn",
    "Bassoon",
    "Clarinet",
    // Pipe
    "Piccolo",
    "Flute",
    "Recorder",
    "Pan Flute",
    "Blown Bottle",
    "Shakuhachi",
    "Whistle",
    "Ocarina",
    // Synth Lead
    "Lead 1 (square)",
    "Lead 2 (sawtooth)",
    "Lead 3 (calliope)",
    "Lead 4 (chiff)",
    "Lead 5 (charang)",
    "Lead 6 (voice)",
    "Lead 7 (fifths)",
    "Lead 8 (bass + lead)",
    // Synth Pad
    "Pad 1 (new age)",
    "Pad 2 (warm)",
    "Pad 3 (polysynth)",
    "Pad 4 (choir)",
    "Pad 5 (bowed)",
    "Pad 6 (metallic)",
    "Pad 7 (halo)",
    "Pad 8 (sweep)",
    // Synth Effects
    "FX 1 (rain)",
    "FX 2 (soundtrack)",
    "FX 3 (crystal)",
    "FX 4 (atmosphere)",
    "FX 5 (brightness)",
    "FX 6 (goblins)",
    "FX 7 (echoes)",
    "FX 8 (sci-fi)",
    // Ethnic
    "Sitar",
    "Banjo",
    "Shamisen",
    "Koto",
    "Kalimba",
    "Bagpipe",
    "Fiddle",
    "Shanai",
    // Percussive
    "Tinkle Bell",
    "Agogo",
    "Steel Drums",
    "Woodblock",
    "Taiko Drum",
    "Melodic Tom",
    "Synth Drum",
    "Reverse Cymbal",
    // Sound Effects
    "Guitar Fret Noise",
    "Breath Noise",
    "Seashore",
    "Bird Tweet",
    "Telephone Ring",
    "Helicopter",
    "Applause",
    "Gunshot",
];

/// GS drum kits by program (also the GM level 2 kit numbering).
pub const GS_DRUM_KITS: &[(u8, &str)] = &[
    (0, "Standard Kit"),
    (8, "Room Kit"),
    (16, "Power Kit"),
    (24, "Electronic Kit"),
    (25, "TR-808 Kit"),
    (32, "Jazz Kit"),
    (40, "Brush Kit"),
    (48, "Orchestra Kit"),
    (56, "SFX Kit"),
    (127, "CM-64/32L Kit"),
];

/// XG drum kits (bank MSB 127) by program.
pub const XG_DRUM_KITS: &[(u8, &str)] = &[
    (0, "Standard Kit"),
    (1, "Standard Kit 2"),
    (8, "Room Kit"),
    (16, "Rock Kit"),
    (24, "Electro Kit"),
    (25, "Analog Kit"),
    (32, "Jazz Kit"),
    (40, "Brush Kit"),
    (48, "Classic Kit"),
];

/// XG SFX kits (bank MSB 126) by program.
pub const XG_SFX_KITS: &[(u8, &str)] = &[(0, "SFX Kit 1"), (1, "SFX Kit 2")];

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(
        format: MidiPatchFormat,
        msb: u8,
        lsb: u8,
        program: Option<u8>,
    ) -> MidiProgramSelection {
        MidiProgramSelection {
            format,
            program,
            bank_msb: msb,
            bank_lsb: lsb,
        }
    }

    #[test]
    fn gm_sends_the_program_alone_and_banked_formats_send_the_bank_first() {
        let gm: Vec<_> = selection(MidiPatchFormat::Gm, 8, 3, Some(24))
            .messages()
            .collect();
        assert_eq!(gm, [ProgramMessage::ProgramChange { program: 24 }]);

        let xg: Vec<_> = selection(MidiPatchFormat::Xg, 127, 0, Some(0))
            .messages()
            .collect();
        assert_eq!(
            xg,
            [
                ProgramMessage::BankSelect {
                    controller: 0,
                    value: 127
                },
                ProgramMessage::BankSelect {
                    controller: 32,
                    value: 0
                },
                ProgramMessage::ProgramChange { program: 0 },
            ]
        );
    }

    #[test]
    fn no_program_sends_nothing() {
        assert_eq!(
            selection(MidiPatchFormat::Gs, 8, 0, None)
                .messages()
                .count(),
            0
        );
    }

    #[test]
    fn messages_are_channel_messages() {
        let (bytes, len) = ProgramMessage::ProgramChange { program: 5 }.bytes(9);
        assert_eq!(&bytes[..len], &[0xC9, 5]);
        let (bytes, len) = ProgramMessage::BankSelect {
            controller: 32,
            value: 2,
        }
        .bytes(0);
        assert_eq!(&bytes[..len], &[0xB0, 32, 2]);
    }

    #[test]
    fn names_follow_the_format_bank_and_part() {
        assert_eq!(
            patch_name(MidiPatchFormat::Gm, 0, 0, 0, 1),
            "1 Acoustic Grand Piano"
        );
        assert_eq!(patch_name(MidiPatchFormat::Gm, 0, 0, 127, 1), "128 Gunshot");
        assert_eq!(
            patch_name(MidiPatchFormat::Gs, 0, 0, 25, 10),
            "26 TR-808 Kit"
        );
        assert_eq!(
            patch_name(MidiPatchFormat::Gs, 8, 0, 0, 1),
            "1 Acoustic Grand Piano (Var 8)"
        );
        assert_eq!(
            patch_name(MidiPatchFormat::Xg, 127, 0, 24, 1),
            "25 Electro Kit"
        );
        // XG decides drums by bank, not by channel.
        assert_eq!(
            patch_name(MidiPatchFormat::Xg, 0, 0, 0, 10),
            "1 Acoustic Grand Piano"
        );
    }

    #[test]
    fn tags_round_trip() {
        for format in MidiPatchFormat::ALL {
            assert_eq!(MidiPatchFormat::from_tag(format.to_tag()), format);
        }
    }
}
