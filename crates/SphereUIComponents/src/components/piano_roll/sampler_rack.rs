//! The step view's Sampler Rack: one track, one Drum Sampler, a row per pad.
//!
//! Every row is a sampler of its own — a Drum Sampler pad, with its sample,
//! region, tune, filter, envelope, gain, pan, mute, solo and output — and all
//! of them live in the one instrument on the one track. A row's steps are
//! the edited clip's notes at the pad's note, so the pattern is that clip:
//! it plays, saves, undoes and exports like any other.
//!
//! Which pads are rows: every pad with a sample, every pad the clip plays,
//! and any the user added this session (an empty pad, waiting for a sample).
//! Nothing is stored for the rack itself; the pads are the Drum Sampler's
//! own state.

use crate::components::timeline::timeline_state::{MidiNoteState, TimelineState};

/// How many pads a Drum Sampler has (`drumsampler::PADS`).
pub(crate) const PAD_COUNT: usize = 64;

/// The note pad `index` answers to until it is changed: the Drum Sampler's
/// default kit layout, General MIDI's kick upwards.
pub(crate) fn default_note(index: usize) -> u8 {
    (36 + index).min(127) as u8
}

/// What the rack shows of one pad.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PadState {
    pub note: u8,
    /// The sample's file name, if one is loaded.
    pub sample: Option<String>,
    pub muted: bool,
    pub solo: bool,
    /// −60 … +12 dB.
    pub gain_db: f32,
    /// −1 (left) … +1 (right).
    pub pan: f32,
    /// The output it plays through: 0 is Main, 1 … 15 are Out 2 … Out 16.
    pub output: u8,
}

impl PadState {
    pub(crate) fn empty(index: usize) -> Self {
        Self {
            note: default_note(index),
            sample: None,
            muted: false,
            solo: false,
            gain_db: 0.0,
            pan: 0.0,
            output: 0,
        }
    }
}

/// One row of the rack: a pad.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RackRow {
    pub pad: usize,
    pub state: PadState,
}

impl RackRow {
    /// The row's name: its sample's name without the extension, or the pad.
    pub(crate) fn name(&self) -> String {
        match &self.state.sample {
            Some(file) => std::path::Path::new(file)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.clone()),
            None => format!("Pad {}", self.pad + 1),
        }
    }
}

/// The Drum Sampler a rack plays: the edited clip's track and its
/// instrument insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RackTarget {
    pub track_id: String,
    pub insert_id: String,
}

/// Whether `plugin_id` names the built-in Drum Sampler.
pub(crate) fn is_drum_sampler(plugin_id: &str) -> bool {
    plugin_id.rsplit(':').next() == Some("drumsampler")
}

/// The Drum Sampler of the track `clip_id` is on, when that is the track's
/// instrument. `None` for any other instrument: the rack is its pads.
pub(crate) fn rack_target(state: &TimelineState, clip_id: &str) -> Option<RackTarget> {
    let (track, _) = state.find_clip(clip_id)?;
    let slot = track.instrument_insert()?;
    slot.plugin_id
        .as_deref()
        .is_some_and(is_drum_sampler)
        .then(|| RackTarget {
            track_id: track.id.clone(),
            insert_id: slot.id.clone(),
        })
}

/// The rows: pads with a sample, pads `notes` play, and the `added` ones, in
/// pad order. A note several pads answer to shows each of them.
pub(crate) fn rack_rows(
    pads: &[PadState],
    notes: &[MidiNoteState],
    added: &[usize],
) -> Vec<RackRow> {
    pads.iter()
        .enumerate()
        .filter(|(index, pad)| {
            pad.sample.is_some()
                || added.contains(index)
                || notes.iter().any(|note| note.pitch == pad.note)
        })
        .map(|(pad, state)| RackRow {
            pad,
            state: state.clone(),
        })
        .collect()
}

/// How many outputs a Drum Sampler has: Main and Out 2 … Out 16.
pub(crate) const OUTPUTS: u8 = 16;

/// An output's name as the Drum Sampler shows it.
pub(crate) fn output_name(output: u8) -> String {
    if output == 0 {
        "Main".to_string()
    } else {
        format!("Out {}", output + 1)
    }
}

/// The note `from` moved `notches` semitones (up is positive), stepping over
/// the notes in `taken` (other pads' notes, so two rows never merge), held
/// inside 0 … 127. Stays at `from` when every note that way is taken.
pub(crate) fn next_free_note(from: u8, notches: i32, taken: &[u8]) -> u8 {
    let step = notches.signum();
    let mut note = i32::from(from);
    let mut left = notches.abs();
    while left > 0 {
        let mut next = note + step;
        while (0..=127).contains(&next) && taken.contains(&(next as u8)) {
            next += step;
        }
        if !(0..=127).contains(&next) {
            break;
        }
        note = next;
        left -= 1;
    }
    note as u8
}

/// The first pad not already a row: where "+ Add Sampler" puts the next one.
pub(crate) fn next_free_pad(rows: &[RackRow], pads: usize) -> Option<usize> {
    (0..pads).find(|pad| !rows.iter().any(|row| row.pad == *pad))
}

/// The Drum Sampler's pads on `insert_id`, as its state mirror holds them;
/// the default kit when the mirror has nothing yet (a fresh instrument).
#[cfg(feature = "builtin-plugin-editor")]
pub(crate) fn pads_of(insert_id: &str) -> Vec<PadState> {
    let params = crate::components::builtin_plugin_editor::builtin_drum_sampler_params(insert_id)
        .unwrap_or_else(drumsampler::default_params);
    params
        .pads
        .iter()
        .map(|pad| PadState {
            note: pad.note,
            sample: pad.sample_name.clone(),
            muted: pad.muted,
            solo: pad.solo,
            gain_db: pad.gain_db,
            pan: pad.pan,
            output: pad.output,
        })
        .collect()
}

/// Without the built-in editors there is no pad state to read: the default
/// kit layout.
#[cfg(not(feature = "builtin-plugin-editor"))]
pub(crate) fn pads_of(_insert_id: &str) -> Vec<PadState> {
    (0..PAD_COUNT).map(PadState::empty).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(pitch: u8, start: f32) -> MidiNoteState {
        MidiNoteState::new(pitch, start, 0.25, 100)
    }

    fn kit() -> Vec<PadState> {
        (0..PAD_COUNT).map(PadState::empty).collect()
    }

    /// Rows are the pads that hold something: a sample, a note the clip
    /// plays, or an added one; in pad order.
    #[test]
    fn the_rows_are_the_pads_in_use() {
        let mut pads = kit();
        pads[5].sample = Some("clap_09.wav".into());
        let notes = [note(38, 0.0), note(36, 1.0), note(36, 2.0)];
        let rows = rack_rows(&pads, &notes, &[9]);
        let indices: Vec<usize> = rows.iter().map(|row| row.pad).collect();
        assert_eq!(indices, vec![0, 2, 5, 9]);
        assert_eq!(rows[2].name(), "clap_09");
        assert_eq!(rows[0].name(), "Pad 1");
        // A pad's note follows the pad, not its position.
        pads[3].note = 38;
        let rows = rack_rows(&pads, &notes, &[]);
        assert!(rows.iter().any(|row| row.pad == 3));
    }

    #[test]
    fn add_sampler_takes_the_first_pad_not_shown() {
        let pads = kit();
        let rows = rack_rows(&pads, &[note(36, 0.0), note(37, 0.0)], &[]);
        assert_eq!(next_free_pad(&rows, PAD_COUNT), Some(2));
        let full: Vec<RackRow> = (0..PAD_COUNT)
            .map(|pad| RackRow {
                pad,
                state: PadState::empty(pad),
            })
            .collect();
        assert_eq!(next_free_pad(&full, PAD_COUNT), None);
    }

    #[test]
    fn a_pads_note_moves_past_the_notes_other_pads_use() {
        assert_eq!(next_free_note(38, 1, &[]), 39);
        assert_eq!(next_free_note(38, 2, &[39]), 41, "D#2 is another pad's");
        assert_eq!(next_free_note(38, -3, &[36, 37]), 33);
        assert_eq!(next_free_note(126, 5, &[]), 127, "held at the top");
        assert_eq!(next_free_note(1, -1, &[0]), 1, "nothing free below");
        assert_eq!(output_name(0), "Main");
        assert_eq!(output_name(3), "Out 4");
    }

    #[test]
    fn only_the_drum_sampler_makes_a_rack() {
        assert!(is_drum_sampler("builtin:drumsampler"));
        assert!(is_drum_sampler("drumsampler"));
        assert!(!is_drum_sampler("builtin:quicksampler"));
        assert!(!is_drum_sampler("com.vendor.drumsamplerpro"));
    }
}
