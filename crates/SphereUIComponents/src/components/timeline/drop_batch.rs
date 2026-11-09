//! Where files dropped on the arrangement land, and in what order (#385).
//!
//! A dropped MIDI file is read and parsed on the background executor (#364),
//! so a drop no longer finishes the moment it is made. Everything that decides
//! where its files land is resolved when the drop is made, from the view the
//! user dropped onto, and carried through the parse:
//!
//! - [`DropPlacement`] is a snapped start beat and the stable id of the track
//!   under the cursor — not pixels, which the scroll and zoom of a later
//!   moment would read differently, and not a row index, which a track added,
//!   moved or deleted meanwhile would shift onto another track.
//! - [`DropBatch`] holds the files of one drop and applies them strictly in
//!   drop order as each prefix becomes ready, so the order clips and tracks
//!   are made in never follows parse time, and a file that fails leaves the
//!   cursor's placement to the next one, as a synchronous import did.
//!
//! The Timeline keeps its pending batches by drop and discards them all when
//! the project changes, so an import never lands in a project it was not
//! dropped into.

use std::path::PathBuf;

use super::midi_import::ImportedMidiTrack;
use super::timeline_state::{TimelineState, TrackType};

/// Where one dropped file lands, resolved from the view at drop time.
#[derive(Debug, Clone, PartialEq)]
pub struct DropPlacement {
    /// Start beat, snapped when the drop was made.
    pub start_beat: f32,
    /// The track under the cursor when the drop was made, by id. `None` (the
    /// drop was below the last track, or this is a later file of a
    /// multi-file drop) asks for a new track.
    pub track_id: Option<String>,
}

/// The two placements of one drop: the first file that lands takes the
/// cursor's, every later one a new track of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTargets {
    pub at_cursor: DropPlacement,
    pub new_track: DropPlacement,
}

impl DropTargets {
    pub fn placement(&self, force_new_track: bool) -> &DropPlacement {
        if force_new_track {
            &self.new_track
        } else {
            &self.at_cursor
        }
    }
}

enum Slot<T> {
    /// Waiting for its parse.
    Pending,
    Ready(T),
    Applied,
}

/// The files of one drop, in drop order.
///
/// A file is either ready when dropped (audio, video) or pending until its
/// parse finishes (MIDI). [`Self::apply_ready`] applies the ready prefix and
/// stops at the first file still pending; a file behind it waits even if it
/// is ready, because where it lands depends on whether the files ahead of it
/// landed.
pub struct DropBatch<T> {
    slots: Vec<Slot<T>>,
    next: usize,
    force_new_track: bool,
}

impl<T> Default for DropBatch<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            next: 0,
            force_new_track: false,
        }
    }
}

impl<T> DropBatch<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the next file of the drop: `Some` when it is ready now, `None`
    /// when it waits for [`Self::resolve`]. Returns its index.
    pub fn push(&mut self, ready: Option<T>) -> usize {
        self.slots.push(match ready {
            Some(file) => Slot::Ready(file),
            None => Slot::Pending,
        });
        self.slots.len() - 1
    }

    /// A pending file is ready. Returns `false` for an index that is not
    /// pending (out of range, or already resolved).
    pub fn resolve(&mut self, index: usize, file: T) -> bool {
        match self.slots.get_mut(index) {
            Some(slot @ Slot::Pending) => {
                *slot = Slot::Ready(file);
                true
            }
            _ => false,
        }
    }

    /// Apply every ready file at the head of the batch, in drop order.
    ///
    /// `apply` gets the file and whether it must take a new track, and
    /// returns whether it landed. A file that landed sends every later file
    /// to a new track; one that did not (a parse error, nothing to import)
    /// leaves the cursor's placement to the next file. Returns how many files
    /// were applied.
    pub fn apply_ready(&mut self, mut apply: impl FnMut(T, bool) -> bool) -> usize {
        let mut applied = 0;
        while let Some(slot) = self.slots.get_mut(self.next) {
            if matches!(slot, Slot::Pending) {
                break;
            }
            let Slot::Ready(file) = std::mem::replace(slot, Slot::Applied) else {
                unreachable!("slots before `next` are applied, at `next` not yet");
            };
            self.next += 1;
            applied += 1;
            if apply(file, self.force_new_track) {
                self.force_new_track = true;
            }
        }
        applied
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Every file has been applied.
    pub fn is_finished(&self) -> bool {
        self.next == self.slots.len()
    }
}

/// One file of a drop on the arrangement.
pub(crate) enum DroppedFile {
    /// A MIDI file, once read and parsed. The `Err` is a status-bar sentence.
    Midi {
        path: PathBuf,
        parsed: Result<Vec<ImportedMidiTrack>, String>,
    },
    Audio(PathBuf),
    Video(PathBuf),
}

/// A drop that still has files waiting for their parse.
pub(crate) struct PendingFileDrop {
    pub targets: DropTargets,
    pub batch: DropBatch<DroppedFile>,
}

impl TimelineState {
    /// Where a file dropped at lane x `lane_x` and track-viewport y
    /// `viewport_y` lands, read from the view as it is now.
    pub fn drop_placement_at(&self, lane_x: f32, viewport_y: f32) -> DropPlacement {
        DropPlacement {
            start_beat: self.snap_beats(self.x_to_beats(lane_x.max(0.0))).max(0.0),
            track_id: self.lane_y_to_track_id(viewport_y),
        }
    }

    /// Where every file after the first one that lands goes: a new track, at
    /// the left edge of the view.
    pub fn new_track_drop_placement(&self) -> DropPlacement {
        DropPlacement {
            start_beat: self.snap_beats(self.x_to_beats(0.0)).max(0.0),
            track_id: None,
        }
    }

    /// The track a dropped file lands on: the placement's, while it still
    /// exists and is a kind `accepts`. `None` when the caller must make one —
    /// the drop asked for a new track, it was over a track of another kind,
    /// or its track has been deleted since.
    pub fn drop_target_track(
        &self,
        placement: &DropPlacement,
        accepts: impl Fn(&TrackType) -> bool,
    ) -> Option<String> {
        let track_id = placement.track_id.as_deref()?;
        self.tracks
            .iter()
            .find(|track| track.id == track_id && accepts(&track.track_type))
            .map(|track| track.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies a batch the way a drop does, recording each file with the
    /// placement it got. `None` files fail (a MIDI file that did not parse).
    fn apply(batch: &mut DropBatch<Option<&'static str>>, log: &mut Vec<(&'static str, bool)>) {
        batch.apply_ready(|file, force_new_track| match file {
            Some(name) => {
                log.push((name, force_new_track));
                true
            }
            None => false,
        });
    }

    #[test]
    fn files_apply_in_drop_order_whatever_order_their_parses_finish_in() {
        let mut batch = DropBatch::new();
        let a = batch.push(None);
        let b = batch.push(None);
        let c = batch.push(None);
        let mut log = Vec::new();

        assert!(batch.resolve(c, Some("c")));
        apply(&mut batch, &mut log);
        assert!(log.is_empty(), "c waits for a and b");

        assert!(batch.resolve(a, Some("a")));
        apply(&mut batch, &mut log);
        assert_eq!(log, vec![("a", false)]);

        assert!(batch.resolve(b, Some("b")));
        apply(&mut batch, &mut log);
        assert_eq!(log, vec![("a", false), ("b", true), ("c", true)]);
        assert!(batch.is_finished());
    }

    #[test]
    fn a_failed_file_leaves_the_cursor_to_the_next_one() {
        let mut batch = DropBatch::new();
        let bad = batch.push(None);
        let good = batch.push(None);
        let later = batch.push(None);
        let mut log = Vec::new();

        batch.resolve(later, Some("later"));
        batch.resolve(good, Some("good"));
        batch.resolve(bad, None);
        apply(&mut batch, &mut log);

        // As if the failed file had not been dropped at all.
        assert_eq!(log, vec![("good", false), ("later", true)]);
        assert!(batch.is_finished());
    }

    #[test]
    fn a_ready_file_behind_a_pending_one_waits_for_it() {
        let mut batch = DropBatch::new();
        batch.push(Some(Some("audio-1")));
        let midi = batch.push(None);
        batch.push(Some(Some("audio-2")));
        let mut log = Vec::new();

        apply(&mut batch, &mut log);
        assert_eq!(log, vec![("audio-1", false)], "only the ready prefix lands");
        assert!(!batch.is_finished());

        batch.resolve(midi, None);
        apply(&mut batch, &mut log);
        assert_eq!(log, vec![("audio-1", false), ("audio-2", true)]);
        assert!(batch.is_finished());
    }

    #[test]
    fn resolve_ignores_a_file_that_is_not_pending() {
        let mut batch = DropBatch::new();
        let ready = batch.push(Some(Some("audio")));
        let midi = batch.push(None);
        assert!(!batch.resolve(ready, Some("other")));
        assert!(!batch.resolve(7, Some("other")));
        assert!(batch.resolve(midi, Some("midi")));
        assert!(!batch.resolve(midi, Some("again")));

        let mut log = Vec::new();
        apply(&mut batch, &mut log);
        assert_eq!(log, vec![("audio", false), ("midi", true)]);
    }

    #[test]
    fn placement_is_the_cursor_until_a_file_lands() {
        let targets = DropTargets {
            at_cursor: DropPlacement {
                start_beat: 32.0,
                track_id: Some("track-3".to_string()),
            },
            new_track: DropPlacement {
                start_beat: 0.0,
                track_id: None,
            },
        };
        assert_eq!(targets.placement(false), &targets.at_cursor);
        assert_eq!(targets.placement(true), &targets.new_track);
    }

    fn midi_track(state: &mut TimelineState) -> String {
        state.create_midi_track()
    }

    #[test]
    fn a_drop_resolves_the_track_under_the_cursor_by_id() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first = midi_track(&mut state);
        midi_track(&mut state);

        let placement = state.drop_placement_at(0.0, 1.0);
        assert_eq!(placement.track_id.as_deref(), Some(first.as_str()));
        assert!(placement.start_beat >= 0.0);

        let below = state.drop_placement_at(0.0, 1.0e9);
        assert_eq!(
            below.track_id, None,
            "below the last track asks for a new one"
        );
    }

    #[test]
    fn the_target_track_survives_a_change_of_view_and_falls_back_when_deleted() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first = midi_track(&mut state);
        let second = midi_track(&mut state);
        let placement = state.drop_placement_at(0.0, 1.0);
        assert_eq!(placement.track_id.as_deref(), Some(first.as_str()));

        // Scrolling and zooming after the drop change nothing it resolved.
        state.viewport.scroll_y += 500.0;
        state.viewport.scroll_x += 500.0;
        let is_midi = |kind: &TrackType| matches!(kind, TrackType::Midi | TrackType::Instrument);
        assert_eq!(
            state.drop_target_track(&placement, is_midi).as_deref(),
            Some(first.as_str())
        );

        // Another kind of track asks for a new one.
        assert_eq!(
            state.drop_target_track(&placement, |kind| matches!(kind, TrackType::Audio)),
            None
        );

        // A deleted track is not replaced by whatever now sits at its row.
        state.viewport.scroll_y -= 500.0;
        state.viewport.scroll_x -= 500.0;
        state.tracks.retain(|track| track.id != first);
        midi_track(&mut state);
        assert_eq!(
            state.lane_y_to_track_id(1.0).as_deref(),
            Some(second.as_str()),
            "the row the drop was on now holds another track"
        );
        assert_eq!(state.drop_target_track(&placement, is_midi), None);
    }
}
