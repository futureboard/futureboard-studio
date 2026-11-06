//! The drum editor's rows: which note each lane plays and what it is called.
//!
//! In drum mode the pitch axis is a list of lanes, not 128 keys. A lane is a
//! MIDI note, top to bottom in ascending order — kick first, as a drum map
//! lists its kit — named by the General MIDI percussion map unless the track's
//! own instrument says otherwise (the built-in Drum Sampler's pads, by their
//! samples). Every row mapping the editor does in drum mode goes through here,
//! so the lane list, the grid rows, the hits and hit-testing agree.

use std::collections::BTreeSet;

/// General MIDI percussion, notes 35 to 81 (channel 10). The standard's names,
/// shortened where a lane label has to fit.
const GM_DRUMS: [&str; 47] = [
    "Acoustic Kick",  // 35
    "Kick",           // 36
    "Side Stick",     // 37
    "Snare",          // 38
    "Hand Clap",      // 39
    "Electric Snare", // 40
    "Low Floor Tom",  // 41
    "Closed Hi-Hat",  // 42
    "High Floor Tom", // 43
    "Pedal Hi-Hat",   // 44
    "Low Tom",        // 45
    "Open Hi-Hat",    // 46
    "Low-Mid Tom",    // 47
    "Hi-Mid Tom",     // 48
    "Crash 1",        // 49
    "High Tom",       // 50
    "Ride 1",         // 51
    "China",          // 52
    "Ride Bell",      // 53
    "Tambourine",     // 54
    "Splash",         // 55
    "Cowbell",        // 56
    "Crash 2",        // 57
    "Vibraslap",      // 58
    "Ride 2",         // 59
    "Hi Bongo",       // 60
    "Low Bongo",      // 61
    "Mute Hi Conga",  // 62
    "Open Hi Conga",  // 63
    "Low Conga",      // 64
    "High Timbale",   // 65
    "Low Timbale",    // 66
    "High Agogo",     // 67
    "Low Agogo",      // 68
    "Cabasa",         // 69
    "Maracas",        // 70
    "Short Whistle",  // 71
    "Long Whistle",   // 72
    "Short Guiro",    // 73
    "Long Guiro",     // 74
    "Claves",         // 75
    "Hi Wood Block",  // 76
    "Low Wood Block", // 77
    "Mute Cuica",     // 78
    "Open Cuica",     // 79
    "Mute Triangle",  // 80
    "Open Triangle",  // 81
];

const GM_FIRST: u8 = 35;
const GM_LAST: u8 = 81;

/// What an empty clip shows in Used mode: a kit to start a beat on.
const CORE_KIT: [u8; 6] = [36, 38, 42, 46, 49, 51];

/// The General MIDI name of a percussion note, if it has one.
pub(super) fn gm_drum_name(pitch: u8) -> Option<&'static str> {
    (GM_FIRST..=GM_LAST)
        .contains(&pitch)
        .then(|| GM_DRUMS[(pitch - GM_FIRST) as usize])
}

/// Which lanes the drum editor lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrumLanes {
    /// The whole General MIDI kit, plus any other note the clip plays.
    Kit,
    /// Only the notes the clip plays (a starter kit when it plays none).
    Used,
}

/// The lanes, top to bottom: ascending notes. `used` is every note the clip
/// plays; none of them is ever left without a lane, so no hit is hidden.
pub(super) fn drum_rows(lanes: DrumLanes, used: &[u8]) -> Vec<u8> {
    let mut rows: BTreeSet<u8> = used.iter().copied().collect();
    match lanes {
        DrumLanes::Kit => rows.extend(GM_FIRST..=GM_LAST),
        DrumLanes::Used if rows.is_empty() => rows.extend(CORE_KIT),
        DrumLanes::Used => {}
    }
    rows.into_iter().collect()
}

/// The lane under a grid-local y, clamped to the first and last lane.
pub(super) fn drum_row_at(rows: &[u8], local_y: f32, scroll_y: f32, row_h: f32) -> Option<u8> {
    if rows.is_empty() {
        return None;
    }
    let row = ((local_y + scroll_y) / row_h.max(0.0001)).floor() as i64;
    Some(rows[row.clamp(0, rows.len() as i64 - 1) as usize])
}

/// Which lane `pitch` is, top first.
pub(super) fn drum_row_index(rows: &[u8], pitch: u8) -> Option<usize> {
    rows.iter().position(|row| *row == pitch)
}

/// `pitch` moved `rows_down` lanes (negative is up), clamped to the list. A
/// note with no lane stays where it is.
pub(super) fn drum_move(rows: &[u8], pitch: u8, rows_down: i32) -> u8 {
    let Some(index) = drum_row_index(rows, pitch) else {
        return pitch;
    };
    let target = (index as i64 + rows_down as i64).clamp(0, rows.len() as i64 - 1);
    rows[target as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_midi_names_cover_the_percussion_map() {
        assert_eq!(gm_drum_name(36), Some("Kick"));
        assert_eq!(gm_drum_name(38), Some("Snare"));
        assert_eq!(gm_drum_name(42), Some("Closed Hi-Hat"));
        assert_eq!(gm_drum_name(81), Some("Open Triangle"));
        assert_eq!(gm_drum_name(34), None);
        assert_eq!(gm_drum_name(82), None);
    }

    /// No hit is ever without a lane: a note outside the kit gets one, and an
    /// empty clip in Used mode still has a kit to draw on.
    #[test]
    fn every_played_note_has_a_lane() {
        let kit = drum_rows(DrumLanes::Kit, &[20, 36, 100]);
        assert_eq!(kit.first(), Some(&20));
        assert_eq!(kit.last(), Some(&100));
        assert_eq!(kit.len(), 47 + 2);

        assert_eq!(drum_rows(DrumLanes::Used, &[42, 36, 42]), vec![36, 42]);
        assert_eq!(drum_rows(DrumLanes::Used, &[]), CORE_KIT.to_vec());
    }

    #[test]
    fn rows_map_both_ways_and_moves_step_by_lane() {
        let rows = vec![36, 38, 42, 46];
        assert_eq!(drum_row_at(&rows, 0.0, 0.0, 20.0), Some(36));
        assert_eq!(drum_row_at(&rows, 45.0, 0.0, 20.0), Some(42));
        assert_eq!(drum_row_at(&rows, 5.0, 20.0, 20.0), Some(38));
        assert_eq!(drum_row_at(&rows, 999.0, 0.0, 20.0), Some(46), "clamped");
        assert_eq!(drum_row_index(&rows, 42), Some(2));
        // One lane down from the snare is the hat, not the next semitone.
        assert_eq!(drum_move(&rows, 38, 1), 42);
        assert_eq!(drum_move(&rows, 38, -5), 36);
        assert_eq!(drum_move(&rows, 37, 1), 37, "a note with no lane stays");
    }
}
