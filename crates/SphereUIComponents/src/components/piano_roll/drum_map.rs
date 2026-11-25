//! The drum editor's rows: which note each lane plays and what it is called.
//!
//! In drum mode the pitch axis is a list of lanes, not 128 keys. A lane is a
//! MIDI note, top to bottom in ascending order — kick first, as a drum map
//! lists its kit — or reversed ([`DrumOrder`]), named by the General MIDI percussion map unless the track's
//! own instrument says otherwise (the built-in Drum Sampler's pads, by their
//! samples). Every row mapping the editor does in drum mode goes through here,
//! so the lane list, the grid rows, the hits and hit-testing agree.
//!
//! A lane is only a note, so any instrument played by note — the Drum
//! Sampler, a hosted VSTi drum kit — plays the steps: they are the clip's own
//! notes at the lane's pitch, on the track's note channel (the channel the
//! Piano Roll draws on, `TrackRoutingState::default_note_channel`), not
//! forced to General MIDI's channel 10. Most VSTi kits listen on channel 1 or
//! omni; set the track's MIDI channel to 10 for a GM module's drum part. A
//! kit mapped off the GM notes is fitted with Lane Note, which keeps the
//! track's lane list (`TrackState::step_lanes`).

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

/// The General MIDI percussion map in the groups a kit is set up in, for
/// the Lane Note picker. Every note from 35 to 81 is in exactly one group.
pub(super) const GM_GROUPS: [(&str, &[u8]); 5] = [
    ("Kick & Snare", &[35, 36, 37, 38, 39, 40]),
    ("Hi-Hats", &[42, 44, 46]),
    ("Toms", &[41, 43, 45, 47, 48, 50]),
    ("Cymbals", &[49, 51, 52, 53, 55, 57, 59]),
    (
        "Percussion",
        &[
            54, 56, 58, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78,
            79, 80, 81,
        ],
    ),
];

/// The General MIDI name of a percussion note, if it has one.
pub(super) fn gm_drum_name(pitch: u8) -> Option<&'static str> {
    (GM_FIRST..=GM_LAST)
        .contains(&pitch)
        .then(|| GM_DRUMS[(pitch - GM_FIRST) as usize])
}

/// The track's drum lanes, ascending: every note the clip plays, plus the
/// track's own lane list (`kit`, what Lane Note picked) — or, while it has
/// none and the clip plays nothing, a starter kit. This is the Used view
/// and what a Lane Note change rewrites.
pub(super) fn track_lanes(kit: &[u8], used: &[u8]) -> Vec<u8> {
    let mut lanes: BTreeSet<u8> = used.iter().copied().collect();
    if !kit.is_empty() {
        lanes.extend(kit.iter().map(|note| note & 0x7F));
    } else if lanes.is_empty() {
        lanes.extend(CORE_KIT);
    }
    lanes.into_iter().collect()
}

/// Whether `note` is already a lane or holds hits, so a lane cannot be set to
/// it: two lanes on one note would be one row, and moving hits onto another
/// lane's hits would merge them.
pub(super) fn lane_note_taken(lanes: &[u8], hits: &[u8], note: u8) -> bool {
    lanes.contains(&note) || hits.contains(&note)
}

/// `lanes` with lane `from` given note `to`, ascending: the track's new lane
/// list. `None` when nothing changes or `to` is taken (see
/// [`lane_note_taken`]); `hits` is every note the track's clips play.
pub(super) fn relabeled_lanes(lanes: &[u8], hits: &[u8], from: u8, to: u8) -> Option<Vec<u8>> {
    if from == to || to > 127 || lane_note_taken(lanes, hits, to) {
        return None;
    }
    let mut next: BTreeSet<u8> = lanes.iter().copied().filter(|n| *n != from).collect();
    next.insert(to);
    Some(next.into_iter().collect())
}

/// Which lanes the drum editor lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrumLanes {
    /// The whole General MIDI kit, plus any other note the clip plays.
    Kit,
    /// Only the notes the clip plays (a starter kit when it plays none).
    Used,
}

/// Which way the lanes run, top to bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DrumOrder {
    /// Low notes first: the kick at the top, as a drum map lists its kit.
    #[default]
    LowFirst,
    /// High notes first: cymbals at the top, kick at the bottom, the way the
    /// piano roll runs its keys.
    HighFirst,
}

impl DrumOrder {
    pub fn reversed(self) -> Self {
        match self {
            Self::LowFirst => Self::HighFirst,
            Self::HighFirst => Self::LowFirst,
        }
    }
}

/// The lanes, top to bottom, in `order`. `used` is every note the clip
/// plays; none of them is ever left without a lane, so no hit is hidden.
/// `kit` is the track's own lane list (see [`track_lanes`]).
pub(super) fn drum_rows(lanes: DrumLanes, order: DrumOrder, used: &[u8], kit: &[u8]) -> Vec<u8> {
    let mut rows: BTreeSet<u8> = track_lanes(kit, used).into_iter().collect();
    if lanes == DrumLanes::Kit {
        rows.extend(GM_FIRST..=GM_LAST);
    }
    match order {
        DrumOrder::LowFirst => rows.into_iter().collect(),
        DrumOrder::HighFirst => rows.into_iter().rev().collect(),
    }
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
        let kit = drum_rows(DrumLanes::Kit, DrumOrder::LowFirst, &[20, 36, 100], &[]);
        assert_eq!(kit.first(), Some(&20));
        assert_eq!(kit.last(), Some(&100));
        assert_eq!(kit.len(), 47 + 2);

        assert_eq!(
            drum_rows(DrumLanes::Used, DrumOrder::LowFirst, &[42, 36, 42], &[]),
            vec![36, 42]
        );
        assert_eq!(
            drum_rows(DrumLanes::Used, DrumOrder::LowFirst, &[], &[]),
            CORE_KIT.to_vec()
        );
    }

    /// Reversed, the same lanes run the other way, and a move down still goes
    /// to the lane drawn below.
    #[test]
    fn reversed_order_runs_high_notes_first() {
        let rows = drum_rows(DrumLanes::Used, DrumOrder::HighFirst, &[36, 42, 38], &[]);
        assert_eq!(rows, vec![42, 38, 36]);
        assert_eq!(drum_move(&rows, 42, 1), 38);
        assert_eq!(drum_row_at(&rows, 0.0, 0.0, 20.0), Some(42));
        let kit = drum_rows(DrumLanes::Kit, DrumOrder::HighFirst, &[], &[]);
        assert_eq!(kit.first(), Some(&GM_LAST));
        assert_eq!(kit.last(), Some(&GM_FIRST));
        assert_eq!(DrumOrder::HighFirst.reversed(), DrumOrder::LowFirst);
    }

    /// The picker's groups hold the whole percussion map once each, and the
    /// kit pieces a VSTi drum kit is mapped by sit where GM puts them.
    #[test]
    fn gm_groups_cover_every_percussion_note_once() {
        let mut seen: Vec<u8> = GM_GROUPS
            .iter()
            .flat_map(|(_, notes)| notes.iter().copied())
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (GM_FIRST..=GM_LAST).collect::<Vec<_>>());
        let group_of = |note: u8| {
            GM_GROUPS
                .iter()
                .find(|(_, notes)| notes.contains(&note))
                .map(|(name, _)| *name)
        };
        for kick_or_snare in [35, 36, 37, 38, 39, 40] {
            assert_eq!(group_of(kick_or_snare), Some("Kick & Snare"));
        }
        for hat in [42, 44, 46] {
            assert_eq!(group_of(hat), Some("Hi-Hats"));
        }
        for tom in [41, 43, 45, 47, 48, 50] {
            assert_eq!(group_of(tom), Some("Toms"));
        }
        for cymbal in [49, 51, 57] {
            assert_eq!(group_of(cymbal), Some("Cymbals"));
        }
        assert_eq!(gm_drum_name(35), Some("Acoustic Kick"));
        assert_eq!(gm_drum_name(37), Some("Side Stick"));
        assert_eq!(gm_drum_name(39), Some("Hand Clap"));
        assert_eq!(gm_drum_name(44), Some("Pedal Hi-Hat"));
        assert_eq!(gm_drum_name(46), Some("Open Hi-Hat"));
        assert_eq!(gm_drum_name(49), Some("Crash 1"));
        assert_eq!(gm_drum_name(51), Some("Ride 1"));
        assert_eq!(gm_drum_name(57), Some("Crash 2"));
    }

    /// The track's own lane list replaces the starter kit and keeps its
    /// lanes even with no hits on them; played notes always get a lane.
    #[test]
    fn a_tracks_lane_list_shows_its_empty_lanes() {
        assert_eq!(track_lanes(&[], &[]), CORE_KIT.to_vec());
        assert_eq!(track_lanes(&[], &[38, 36]), vec![36, 38]);
        assert_eq!(track_lanes(&[24, 36, 44], &[]), vec![24, 36, 44]);
        assert_eq!(track_lanes(&[24, 36], &[38]), vec![24, 36, 38]);
        assert_eq!(
            drum_rows(DrumLanes::Used, DrumOrder::LowFirst, &[], &[24, 44]),
            vec![24, 44]
        );
        let kit = drum_rows(DrumLanes::Kit, DrumOrder::LowFirst, &[], &[24]);
        assert_eq!(kit.first(), Some(&24), "a non-GM lane is listed in Kit too");
        assert_eq!(kit.len(), 47 + 1);
    }

    /// Lane Note swaps one lane's note in the list; a note that is already a
    /// lane, or holds hits anywhere on the track, is refused.
    #[test]
    fn a_lane_takes_a_free_note_only() {
        let lanes = track_lanes(&[], &[]);
        assert_eq!(
            relabeled_lanes(&lanes, &[], 42, 44),
            Some(vec![36, 38, 44, 46, 49, 51])
        );
        assert_eq!(relabeled_lanes(&lanes, &[], 42, 42), None, "no change");
        assert_eq!(relabeled_lanes(&lanes, &[], 42, 46), None, "another lane");
        assert_eq!(relabeled_lanes(&lanes, &[40], 42, 40), None, "hits there");
        assert!(lane_note_taken(&lanes, &[40], 40));
        assert!(!lane_note_taken(&lanes, &[40], 41));
        // A Kit-view lane that is not on the track's list (an empty GM lane)
        // is free to take.
        assert_eq!(
            relabeled_lanes(&[36, 38], &[36], 38, 40),
            Some(vec![36, 40])
        );
        // Any MIDI note, not only GM ones: a kit mapped below the GM range.
        assert_eq!(relabeled_lanes(&[36, 38], &[], 36, 24), Some(vec![24, 38]));
        assert_eq!(relabeled_lanes(&[36], &[], 36, 128), None);
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
