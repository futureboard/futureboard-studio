//! The Drum Sampler editor's right-click menus: on a pad (in the grid or the
//! mixer), on a file in the Samples folder, and on the waveform. Drawn with
//! the app's own [`context_menu_overlay`](crate::components::context_menu),
//! the same menu on every platform.
//!
//! This module builds the entries and holds the pure edits behind them; the
//! window runs the commands.

use drumsampler::{OUTPUTS, PADS, Pad, Params};

use crate::components::context_menu::ContextMenuEntry;
use crate::components::drum_sampler_panel::{
    DrumSamplerPanelState, DrumView, output_label, pad_number,
};
use crate::components::soundfont_player_mdi::note_label;

/// What was right-clicked.
#[derive(Clone, Debug, PartialEq)]
pub enum DrumMenuTarget {
    Pad(usize),
    /// A file in the Samples folder, by name.
    File(String),
    /// The selected pad's waveform.
    Waveform,
}

pub mod command {
    pub const PLAY: &str = "play";
    pub const LOAD: &str = "load";
    pub const COPY: &str = "copy";
    pub const PASTE: &str = "paste";
    pub const RESET: &str = "reset";
    pub const OWN_OUTPUT: &str = "own-output";
    pub const FIRST_OUTPUT: &str = "first-output";
    pub const MUTE: &str = "mute";
    pub const SOLO: &str = "solo";
    pub const SHOW_PADS: &str = "show-pads";
    pub const SHOW_MIXER: &str = "show-mixer";
    pub const LOAD_FILE: &str = "load-file";
    pub const LOAD_FILE_EMPTY: &str = "load-file-empty";
    pub const REVEAL: &str = "reveal";
    pub const RESET_START: &str = "reset-start";
    pub const RESET_END: &str = "reset-end";
    pub const RESET_REGION: &str = "reset-region";
    pub const REVERSE: &str = "reverse";
}

/// The lowest output past Out 1 that no other pad uses, for giving `pad` a
/// channel of its own. `None` when every output is taken.
pub fn free_output(params: &Params, pad: usize) -> Option<u8> {
    (1..OUTPUTS as u8).find(|output| {
        params
            .pads
            .iter()
            .enumerate()
            .all(|(index, other)| index == pad || other.output != *output)
    })
}

/// `target` shaped like `source`: its sound — tune, level, pan, envelope,
/// filter, region, direction, velocity and choke group — but still on its
/// own note, sample, output, mute and solo.
pub fn paste_settings(target: &Pad, source: &Pad) -> Pad {
    Pad {
        note: target.note,
        sample_name: target.sample_name.clone(),
        output: target.output,
        muted: target.muted,
        solo: target.solo,
        ..source.clone()
    }
}

/// Pad `index` back at its defaults, keeping its note, sample and output.
pub fn reset_pad(index: usize, pad: &Pad) -> Pad {
    Pad {
        note: pad.note,
        sample_name: pad.sample_name.clone(),
        output: pad.output,
        ..drumsampler::default_pad(index)
    }
}

/// The first pad from `from` on (wrapping) with no sample.
pub fn next_empty_pad(params: &Params, from: usize) -> Option<usize> {
    (0..PADS)
        .map(|offset| (from + offset) % PADS)
        .find(|index| params.pads[*index].sample_name.is_none())
}

/// The menu for `target`. `can_paste`: a pad's settings have been copied.
pub fn menu_entries(
    panel: &DrumSamplerPanelState,
    target: &DrumMenuTarget,
    can_paste: bool,
) -> Vec<ContextMenuEntry> {
    match target {
        DrumMenuTarget::Pad(index) => pad_entries(panel, *index, can_paste),
        DrumMenuTarget::File(name) => {
            let selected = panel.selected;
            let empty = next_empty_pad(&panel.params, selected);
            vec![
                ContextMenuEntry::Header(name.clone()),
                ContextMenuEntry::item(
                    format!("Load onto Pad {}", pad_number(selected)),
                    command::LOAD_FILE,
                ),
                match empty {
                    Some(pad) => ContextMenuEntry::item(
                        format!("Load onto Next Empty Pad ({})", pad_number(pad)),
                        command::LOAD_FILE_EMPTY,
                    ),
                    None => ContextMenuEntry::disabled_item(
                        "Load onto Next Empty Pad",
                        command::LOAD_FILE_EMPTY,
                    ),
                },
                ContextMenuEntry::Separator,
                ContextMenuEntry::item("Show in Folder", command::REVEAL),
            ]
        }
        DrumMenuTarget::Waveform => {
            let pad = panel.pad();
            let full = pad.start <= 0.0 && pad.end >= 1.0;
            vec![
                ContextMenuEntry::Header(format!("Pad {} region", pad_number(panel.selected))),
                ContextMenuEntry::item("Play", command::PLAY),
                ContextMenuEntry::Separator,
                if pad.start > 0.0 {
                    ContextMenuEntry::item("Reset Start", command::RESET_START)
                } else {
                    ContextMenuEntry::disabled_item("Reset Start", command::RESET_START)
                },
                if pad.end < 1.0 {
                    ContextMenuEntry::item("Reset End", command::RESET_END)
                } else {
                    ContextMenuEntry::disabled_item("Reset End", command::RESET_END)
                },
                if full {
                    ContextMenuEntry::disabled_item("Play Whole Sample", command::RESET_REGION)
                } else {
                    ContextMenuEntry::item("Play Whole Sample", command::RESET_REGION)
                },
                ContextMenuEntry::Separator,
                ContextMenuEntry::checked_item("Reverse", command::REVERSE, pad.reverse),
            ]
        }
    }
}

fn pad_entries(
    panel: &DrumSamplerPanelState,
    index: usize,
    can_paste: bool,
) -> Vec<ContextMenuEntry> {
    let pad = &panel.params.pads[index];
    let has_sample = pad.sample_name.is_some();
    let title = format!(
        "Pad {} · {} — {}",
        pad_number(index),
        note_label(pad.note),
        pad.sample_name.as_deref().unwrap_or("empty")
    );
    let own = match (pad.output, free_output(&panel.params, index)) {
        (0, Some(free)) => ContextMenuEntry::item(
            format!("Own Output ({})", output_label(free)),
            command::OWN_OUTPUT,
        ),
        (0, None) => {
            ContextMenuEntry::disabled_item("Own Output (all in use)", command::OWN_OUTPUT)
        }
        (current, _) => ContextMenuEntry::checked_item(
            format!("Own Output ({})", output_label(current)),
            command::OWN_OUTPUT,
            true,
        ),
    };
    let mut entries = vec![
        ContextMenuEntry::Header(title),
        if has_sample {
            ContextMenuEntry::item("Play", command::PLAY)
        } else {
            ContextMenuEntry::disabled_item("Play", command::PLAY)
        },
        ContextMenuEntry::item(
            if has_sample {
                "Replace Sample…"
            } else {
                "Load Sample…"
            },
            command::LOAD,
        ),
        ContextMenuEntry::Separator,
        ContextMenuEntry::item("Copy Settings", command::COPY),
        if can_paste {
            ContextMenuEntry::item("Paste Settings", command::PASTE)
        } else {
            ContextMenuEntry::disabled_item("Paste Settings", command::PASTE)
        },
        ContextMenuEntry::item("Reset Settings", command::RESET),
        ContextMenuEntry::Separator,
        own,
        if pad.output == 0 {
            ContextMenuEntry::checked_item("Out 1 (this track)", command::FIRST_OUTPUT, true)
        } else {
            ContextMenuEntry::item("Back to Out 1 (this track)", command::FIRST_OUTPUT)
        },
        ContextMenuEntry::Separator,
        ContextMenuEntry::checked_item("Mute", command::MUTE, pad.muted),
        ContextMenuEntry::checked_item("Solo", command::SOLO, pad.solo),
        ContextMenuEntry::Separator,
    ];
    entries.push(match panel.view {
        DrumView::Pads => ContextMenuEntry::item("Show in Mixer", command::SHOW_MIXER),
        DrumView::Mixer => ContextMenuEntry::item("Edit Pad", command::SHOW_PADS),
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_of(entries: &[ContextMenuEntry], wanted: &str) -> Option<(String, bool, bool)> {
        entries.iter().find_map(|entry| match entry {
            ContextMenuEntry::Item {
                label,
                command,
                disabled,
                checked,
                ..
            } if command == wanted => Some((label.clone(), *disabled, *checked)),
            _ => None,
        })
    }

    #[test]
    fn a_pad_gets_the_lowest_output_no_other_pad_uses() {
        let mut params = drumsampler::default_params();
        assert_eq!(free_output(&params, 0), Some(1));
        params.pads[3].output = 1;
        params.pads[7].output = 2;
        assert_eq!(free_output(&params, 0), Some(3));
        // A pad's own output does not count against it.
        assert_eq!(free_output(&params, 3), Some(1));
        for (index, pad) in params.pads.iter_mut().enumerate().take(OUTPUTS) {
            pad.output = index as u8;
        }
        assert_eq!(free_output(&params, 40), None);
    }

    #[test]
    fn pasting_takes_the_sound_and_keeps_the_pad() {
        let mut source = drumsampler::default_pad(0);
        source.gain_db = -6.0;
        source.decay_ms = 300.0;
        source.output = 4;
        source.sample_name = Some("kick.wav".into());
        let mut target = drumsampler::default_pad(9);
        target.sample_name = Some("snare.wav".into());
        let pasted = paste_settings(&target, &source);
        assert_eq!((pasted.gain_db, pasted.decay_ms), (-6.0, 300.0));
        assert_eq!(pasted.note, target.note);
        assert_eq!(pasted.sample_name.as_deref(), Some("snare.wav"));
        assert_eq!(pasted.output, 0);
    }

    #[test]
    fn a_reset_keeps_the_note_sample_and_output() {
        let mut pad = drumsampler::default_pad(2);
        pad.note = 50;
        pad.gain_db = -12.0;
        pad.output = 3;
        pad.sample_name = Some("hat.wav".into());
        let reset = reset_pad(2, &pad);
        assert_eq!(reset.gain_db, 0.0);
        assert_eq!((reset.note, reset.output), (50, 3));
        assert_eq!(reset.sample_name.as_deref(), Some("hat.wav"));
    }

    #[test]
    fn the_next_empty_pad_wraps_round_the_kit() {
        let mut params = drumsampler::default_params();
        params.pads[5].sample_name = Some("a.wav".into());
        assert_eq!(next_empty_pad(&params, 5), Some(6));
        for pad in params.pads.iter_mut().skip(6) {
            pad.sample_name = Some("x.wav".into());
        }
        assert_eq!(next_empty_pad(&params, 6), Some(0));
    }

    #[test]
    fn an_empty_pad_cannot_be_played_and_paste_waits_for_a_copy() {
        let panel = DrumSamplerPanelState::default();
        let entries = menu_entries(&panel, &DrumMenuTarget::Pad(0), false);
        assert_eq!(command_of(&entries, command::PLAY).map(|c| c.1), Some(true));
        assert_eq!(
            command_of(&entries, command::PASTE).map(|c| c.1),
            Some(true)
        );
        assert_eq!(
            command_of(&entries, command::LOAD).map(|c| c.0),
            Some("Load Sample…".to_string())
        );
        assert_eq!(
            command_of(&entries, command::OWN_OUTPUT).map(|c| c.0),
            Some("Own Output (Out 2)".to_string())
        );
        let pasteable = menu_entries(&panel, &DrumMenuTarget::Pad(0), true);
        assert_eq!(
            command_of(&pasteable, command::PASTE).map(|c| c.1),
            Some(false)
        );
    }
}
