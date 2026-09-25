use super::*;

/// Raise a monotonic id counter so the next mint is strictly greater than `seen`.
fn observe_counter(counter: &std::sync::atomic::AtomicU64, seen: u64) {
    use std::sync::atomic::Ordering;
    if seen == 0 {
        return;
    }
    let mut current = counter.load(Ordering::Relaxed);
    while current <= seen {
        match counter.compare_exchange_weak(current, seen + 1, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

fn mint_counter(counter: &std::sync::atomic::AtomicU64) -> u64 {
    use std::sync::atomic::Ordering;
    counter.fetch_add(1, Ordering::Relaxed)
}

fn counter_midi_note() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    &COUNTER
}

fn counter_controller_point() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    &COUNTER
}

fn counter_pitch_point() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    &COUNTER
}

fn counter_automation_point() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    &COUNTER
}

/// Monotonic source of pitch-curve point identities. Persisted from project
/// format v38 onward so a pitch point keeps its identity across save/load,
/// undo, and note move/copy/split.
pub fn next_pitch_point_id() -> u64 {
    mint_counter(counter_pitch_point())
}

/// Ensure subsequent pitch-point mints do not collide with a loaded id.
pub fn observe_pitch_point_id(id: u64) {
    observe_counter(counter_pitch_point(), id);
}

/// Monotonic source of automation-point identities. Persisted from project
/// format v26 onward; older files mint fresh ids on load.
pub fn next_automation_point_id() -> u64 {
    mint_counter(counter_automation_point())
}

/// Ensure subsequent automation-point mints do not collide with a loaded id.
pub fn observe_automation_point_id(id: u64) {
    observe_counter(counter_automation_point(), id);
}

/// Monotonic source of MIDI note identities. Persisted from project format v26
/// onward so selection, undo, and clipboard targets survive save/load. Copies
/// and duplicates must call [`next_midi_note_id`] for a new identity; moves
/// keep the existing id.
pub fn next_midi_note_id() -> u64 {
    mint_counter(counter_midi_note())
}

/// Ensure subsequent note mints do not collide with a loaded id.
pub fn observe_midi_note_id(id: u64) {
    observe_counter(counter_midi_note(), id);
}

/// Monotonic source of controller-point identities. Persisted from project
/// format v26 onward (same lifecycle as note ids).
pub fn next_controller_point_id() -> u64 {
    mint_counter(counter_controller_point())
}

/// Ensure subsequent controller-point mints do not collide with a loaded id.
pub fn observe_controller_point_id(id: u64) {
    observe_counter(counter_controller_point(), id);
}

/// Monotonic source of stable tempo-point identities. Persisted in project
/// files so edits target a point by id even after the user drags it to a new
/// beat position.
pub fn next_tempo_point_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("tempo-{ts:x}-{seq:x}")
}

pub fn next_time_signature_point_id() -> TimeSignaturePointId {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("ts-{ts:x}-{seq:x}")
}

pub fn next_timeline_marker_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("marker-{ts:x}-{seq:x}")
}

pub fn next_timeline_region_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("region-{ts:x}-{seq:x}")
}

/// Stable project identity for chord, lyric, and section events. Timestamp plus
/// a process-local sequence avoids collisions with loaded projects without an
/// observe pass, while preserving IDs across save/load and undo/redo.
pub fn next_song_text_event_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("song-text-{ts:x}-{seq:x}")
}

fn counter_midi_edit_revision() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    &COUNTER
}

/// The current MIDI edit revision.
///
/// A monotonic counter bumped whenever anything takes a mutable borrow of a
/// clip's notes or articulation events. It exists so that a view which caches
/// something derived from those — the Pitch editor's evaluated trajectory, for
/// one — can ask "is what I have still current" in a single integer compare.
///
/// The alternative it replaces was comparing the cached `Vec<MidiNoteState>`
/// against the live one field by field. That walks every note *and every pitch
/// point of every note*, once per frame, and it was measured at **five times
/// the cost of simply rebuilding the thing it was trying to avoid rebuilding**
/// — a cache whose validity check is more expensive than a miss.
///
/// Deliberately global rather than per clip. A mutation anywhere invalidates
/// every derived cache, which over-invalidates when two clips are open at once
/// and is the safe direction: a cache that refreshes too often is slow, and one
/// that refreshes too rarely draws the wrong thing.
pub fn midi_edit_revision() -> u64 {
    use std::sync::atomic::Ordering;
    counter_midi_edit_revision().load(Ordering::Relaxed)
}

/// Record that MIDI clip content may have changed.
///
/// Called from the mutable accessors themselves, so no caller has to remember
/// to. Taking the borrow is treated as having mutated: a caller that borrows
/// and changes nothing costs one wasted rebuild, which is the direction that
/// cannot draw a stale curve.
pub(crate) fn bump_midi_edit_revision() {
    use std::sync::atomic::Ordering;
    counter_midi_edit_revision().fetch_add(1, Ordering::Relaxed);
}

/// Last revision at which each clip's MIDI content was (possibly) mutated.
fn midi_clip_revisions() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static REVISIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, u64>>,
    > = std::sync::OnceLock::new();
    REVISIONS.get_or_init(Default::default)
}

/// Record that one clip's MIDI content may have changed: bumps the global
/// [`midi_edit_revision`] (views that span clips still see it) and stamps
/// `clip_id` with the new value for [`midi_clip_revision`].
pub(crate) fn bump_midi_clip_revision(clip_id: &str) {
    use std::sync::atomic::Ordering;
    let revision = counter_midi_edit_revision().fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut revisions) = midi_clip_revisions().lock() {
        revisions.insert(clip_id.to_string(), revision);
    }
}

/// The revision at which `clip_id`'s notes, articulations or SysEx last took a
/// mutable borrow; `0` for a clip that never has this session.
///
/// For caches that belong to one clip — the arrangement's note previews. Keyed
/// on the global revision they were rebuilt for *every* visible MIDI clip on
/// every edit to any one of them, including on each mouse move of a velocity
/// drag.
pub fn midi_clip_revision(clip_id: &str) -> u64 {
    midi_clip_revisions()
        .lock()
        .ok()
        .and_then(|revisions| revisions.get(clip_id).copied())
        .unwrap_or(0)
}

#[cfg(test)]
mod midi_clip_revision_tests {
    use super::*;

    /// An edit to one clip moves that clip's revision and the global one, and
    /// leaves every other clip's cached previews valid.
    #[test]
    fn editing_one_clip_does_not_invalidate_another() {
        let other_before = midi_clip_revision("rev-test-other");
        let global_before = midi_edit_revision();
        bump_midi_clip_revision("rev-test-edited");
        let edited = midi_clip_revision("rev-test-edited");
        assert!(edited > global_before);
        assert!(midi_edit_revision() > global_before);
        assert_eq!(midi_clip_revision("rev-test-other"), other_before);
        bump_midi_clip_revision("rev-test-edited");
        assert!(midi_clip_revision("rev-test-edited") > edited);
    }
}
