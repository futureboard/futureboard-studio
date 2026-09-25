//! MIDI articulation model (Sustain / Staccato / Legato / …).
//!
//! Two storage shapes, one vocabulary:
//! - **Per-note** articulation lives directly on [`MidiNoteState::articulation`],
//!   so it copies, moves, splits, and deletes with its note for free.
//! - **Direction** articulation events live on the clip
//!   (`ClipType::Midi { articulations }`) as timeline points: a direction stays
//!   active until the next direction event replaces it (notation semantics).
//!
//! Playback is non-destructive: the stored note start/duration/velocity are
//! never modified. Modifiers are applied only while building the engine
//! project snapshot (see `layout::engine_snapshot`), which both realtime
//! playback and offline export consume — so the two are equivalent by
//! construction and the audio callback never sees articulation logic.

use super::*;

/// Built-in articulation identity. The `u8` tag is the persisted form
/// (project format v25+); `0` is reserved for "no articulation".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ArticulationId {
    Sustain = 1,
    Staccato = 2,
    Staccatissimo = 3,
    Legato = 4,
    Tenuto = 5,
    Accent = 6,
    Marcato = 7,
    Pizzicato = 8,
    Tremolo = 9,
}

impl ArticulationId {
    /// Every built-in articulation, in display order.
    pub const ALL: [ArticulationId; 9] = [
        ArticulationId::Sustain,
        ArticulationId::Staccato,
        ArticulationId::Staccatissimo,
        ArticulationId::Legato,
        ArticulationId::Tenuto,
        ArticulationId::Accent,
        ArticulationId::Marcato,
        ArticulationId::Pizzicato,
        ArticulationId::Tremolo,
    ];

    /// Persisted tag. `0` means "none" and is never a valid `ArticulationId`.
    pub fn to_tag(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::to_tag`]. Unknown tags (including `0`) decode to
    /// `None` so newer files degrade to "no articulation" instead of failing.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Sustain),
            2 => Some(Self::Staccato),
            3 => Some(Self::Staccatissimo),
            4 => Some(Self::Legato),
            5 => Some(Self::Tenuto),
            6 => Some(Self::Accent),
            7 => Some(Self::Marcato),
            8 => Some(Self::Pizzicato),
            9 => Some(Self::Tremolo),
            _ => None,
        }
    }

    /// Central registry lookup — the single source of articulation behavior.
    pub fn definition(self) -> &'static ArticulationDefinition {
        let idx = self.to_tag() as usize - 1;
        &ARTICULATION_REGISTRY[idx]
    }

    pub fn name(self) -> &'static str {
        self.definition().name
    }

    pub fn short_name(self) -> &'static str {
        self.definition().short_name
    }
}

/// Whether an articulation event applies until replaced (direction) or to one
/// specific note (per-note). Kept as vocabulary for the editor/UI; the storage
/// location (clip event list vs. note field) is what actually encodes the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArticulationMode {
    Direction,
    PerNote,
}

/// How an articulation reaches the instrument. Built-ins are pure playback
/// modifiers today; the keyswitch / CC / program-change variants exist so a
/// later articulation-map feature can bind them without a model change. They
/// are intentionally NOT persisted anywhere yet — the registry is code, and
/// serializing unconsumed trigger data would be fake support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArticulationTrigger {
    /// Alter scheduled note gate/velocity non-destructively (built-in default).
    PlaybackModifier,
    KeySwitch {
        note: u8,
        channel: Option<u8>,
        pre_trigger_ticks: i64,
    },
    ControlChange {
        controller: u8,
        value: u8,
        channel: Option<u8>,
    },
    ProgramChange {
        program: u8,
        channel: Option<u8>,
    },
}

/// Non-destructive playback shaping for one articulation. Applied at engine
/// snapshot build time; the project note data is never rewritten.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArticulationPlayback {
    /// Fraction of the stored note duration that actually sounds (`0..=1`+).
    pub gate_ratio: f32,
    /// Signed velocity offset added to the stored velocity, then clamped 1..=127.
    pub velocity_delta: i16,
    /// Legato: extend the gate to overlap the next note by this many beats.
    /// `0.0` for non-legato articulations.
    pub legato_overlap_beats: f32,
}

/// One built-in articulation: identity, labels, and playback behavior.
/// Definitions are centralized here — UI and snapshot code must consult the
/// registry rather than hardcoding per-articulation behavior.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArticulationDefinition {
    pub id: ArticulationId,
    pub name: &'static str,
    /// Compact label for lane regions / note badges ("Stac.", "Leg.", …).
    pub short_name: &'static str,
    pub playback: ArticulationPlayback,
    pub trigger: ArticulationTrigger,
}

/// Built-in registry, indexed by `tag - 1`. Order must match
/// [`ArticulationId::ALL`] / the tag values.
pub static ARTICULATION_REGISTRY: [ArticulationDefinition; 9] = [
    ArticulationDefinition {
        id: ArticulationId::Sustain,
        name: "Sustain",
        short_name: "Sus.",
        playback: ArticulationPlayback {
            gate_ratio: 0.98,
            velocity_delta: 0,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    ArticulationDefinition {
        id: ArticulationId::Staccato,
        name: "Staccato",
        short_name: "Stac.",
        playback: ArticulationPlayback {
            gate_ratio: 0.45,
            velocity_delta: 0,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    ArticulationDefinition {
        id: ArticulationId::Staccatissimo,
        name: "Staccatissimo",
        short_name: "Stacss.",
        playback: ArticulationPlayback {
            gate_ratio: 0.2,
            velocity_delta: 0,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    ArticulationDefinition {
        id: ArticulationId::Legato,
        name: "Legato",
        short_name: "Leg.",
        playback: ArticulationPlayback {
            gate_ratio: 1.0,
            velocity_delta: 0,
            legato_overlap_beats: LEGATO_OVERLAP_BEATS,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    ArticulationDefinition {
        id: ArticulationId::Tenuto,
        name: "Tenuto",
        short_name: "Ten.",
        playback: ArticulationPlayback {
            gate_ratio: 1.0,
            velocity_delta: 0,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    ArticulationDefinition {
        id: ArticulationId::Accent,
        name: "Accent",
        short_name: "Acc.",
        playback: ArticulationPlayback {
            gate_ratio: 1.0,
            velocity_delta: 20,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    ArticulationDefinition {
        id: ArticulationId::Marcato,
        name: "Marcato",
        short_name: "Marc.",
        playback: ArticulationPlayback {
            gate_ratio: 0.85,
            velocity_delta: 24,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    // Playing techniques rather than score markings. They carry no gate or
    // velocity shaping: the recorded pizzicato already decays on its own and
    // the recorded tremolo already repeats, so imposing a notation envelope on
    // top would fight the sample instead of describing it. They exist so the
    // editor can *say* which technique a note is played with — without them
    // the two recorded techniques in a bank like Solo Violin are unreachable
    // from the arrangement, which is 44% of that bank's recordings.
    ArticulationDefinition {
        id: ArticulationId::Pizzicato,
        name: "Pizzicato",
        short_name: "Pizz.",
        playback: ArticulationPlayback {
            gate_ratio: 1.0,
            velocity_delta: 0,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
    ArticulationDefinition {
        id: ArticulationId::Tremolo,
        name: "Tremolo",
        short_name: "Trem.",
        playback: ArticulationPlayback {
            gate_ratio: 1.0,
            velocity_delta: 0,
            legato_overlap_beats: 0.0,
        },
        trigger: ArticulationTrigger::PlaybackModifier,
    },
];

/// Default legato overlap (beats). 1/64 note at any tempo — small enough not
/// to smear chords, large enough for a VSTi legato detector to see the overlap.
pub const LEGATO_OVERLAP_BEATS: f32 = 1.0 / 16.0 * 0.25;

/// A direction articulation event on a MIDI clip's articulation lane. Active
/// from `beat` until the next event (or the clip end). `id` is transient —
/// minted fresh on create and on project load, exactly like MIDI note ids —
/// so it is never persisted or used as a stable cross-session identifier.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiArticulationEvent {
    /// Transient editor identity (selection / drag targets). Not serialized.
    pub id: u64,
    /// Beats relative to the clip start.
    pub beat: f32,
    pub articulation: ArticulationId,
}

impl MidiArticulationEvent {
    /// Construct an event with a freshly minted transient id; `beat` clamps
    /// to `>= 0`.
    pub fn new(beat: f32, articulation: ArticulationId) -> Self {
        Self {
            id: next_articulation_event_id(),
            beat: beat.max(0.0),
            articulation,
        }
    }
}

/// Source of transient identities for articulation events (not serialized;
/// minted fresh on create and on project load, like [`next_midi_note_id`]).
pub fn next_articulation_event_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Articulation chasing: the direction articulation in effect at `beat` —
/// the latest event with `event.beat <= beat` (ties broken by list order).
/// `events` need not be sorted; clips keep them sorted but this stays correct
/// mid-edit either way. This is the single lookup used both by the snapshot
/// builder (playback) and the editor (lane display), so starting playback
/// mid-clip, seeking, or looping always resolves the same articulation.
pub fn direction_articulation_at(
    events: &[MidiArticulationEvent],
    beat: f32,
) -> Option<ArticulationId> {
    let mut best: Option<&MidiArticulationEvent> = None;
    for event in events {
        if event.beat <= beat + 1.0e-4 && best.is_none_or(|b| event.beat >= b.beat) {
            best = Some(event);
        }
    }
    best.map(|e| e.articulation)
}

/// Effective articulation for one note: an explicit per-note articulation wins;
/// otherwise the note chases the clip's direction lane at its start beat.
pub fn resolve_note_articulation(
    note: &MidiNoteState,
    direction_events: &[MidiArticulationEvent],
) -> Option<ArticulationId> {
    note.articulation
        .or_else(|| direction_articulation_at(direction_events, note.start))
}

/// Non-destructive playback shaping: returns the `(duration_beats, velocity)`
/// to schedule for a note under `articulation`. The stored note is untouched.
///
/// - Gate: `duration * gate_ratio`, floored at [`MIN_NOTE_BEATS`] so a note can
///   never collapse to zero/negative length.
/// - Legato: extends the gate to `next_note_start - start + overlap`, so the
///   note always reaches (and slightly overlaps) the next note. Without a
///   following note the plain gate applies — no unbounded tails, and the
///   note-off is still emitted, so stop/seek/loop cleanup is unchanged.
/// - Velocity: `velocity + delta`, clamped to MIDI 1..=127.
pub fn apply_articulation_playback(
    start_beats: f32,
    duration_beats: f32,
    velocity: u8,
    articulation: ArticulationId,
    next_note_start_beats: Option<f32>,
) -> (f32, u8) {
    let def = articulation.definition();
    let playback = def.playback;
    let base = duration_beats.max(MIN_NOTE_BEATS);
    let mut gated = (base * playback.gate_ratio.max(0.0)).max(MIN_NOTE_BEATS);
    if playback.legato_overlap_beats > 0.0 {
        if let Some(next_start) = next_note_start_beats {
            let to_next = next_start - start_beats;
            if to_next > 0.0 {
                gated = (to_next + playback.legato_overlap_beats).max(MIN_NOTE_BEATS);
            }
        }
    }
    let velocity = (velocity as i32 + playback.velocity_delta as i32).clamp(1, 127) as u8;
    (gated, velocity)
}

impl TimelineState {
    // ── MIDI articulation lane (direction events) ─────────────────────────
    // Mirrors the controller-lane helpers: read access, full-list snapshot for
    // undo prev/next, and a full-list setter the `SetMidiArticulations` edit
    // command drives. Point edits (insert/move/delete) are expressed as
    // snapshot → mutate → snapshot by the piano-roll gestures.

    /// Borrow a MIDI clip's direction articulation events (sorted by beat).
    pub fn midi_clip_articulations(&self, clip_id: &str) -> Option<&Vec<MidiArticulationEvent>> {
        for track in &self.tracks {
            for clip in &track.clips {
                if clip.id == clip_id {
                    if let ClipType::Midi { articulations, .. } = &clip.clip_type {
                        return Some(articulations);
                    }
                }
            }
        }
        None
    }

    fn midi_clip_articulations_mut(
        &mut self,
        clip_id: &str,
    ) -> Option<&mut Vec<MidiArticulationEvent>> {
        bump_midi_clip_revision(clip_id);
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.id == clip_id {
                    if let ClipType::Midi { articulations, .. } = &mut clip.clip_type {
                        return Some(articulations);
                    }
                }
            }
        }
        None
    }

    /// Clone of a clip's articulation events (for undo prev/next snapshots).
    pub fn articulations_snapshot(&self, clip_id: &str) -> Vec<MidiArticulationEvent> {
        self.midi_clip_articulations(clip_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace a clip's direction articulation events (undo command payload).
    /// Events are kept sorted by beat.
    pub fn set_midi_articulations(&mut self, clip_id: &str, events: Vec<MidiArticulationEvent>) {
        if let Some(list) = self.midi_clip_articulations_mut(clip_id) {
            *list = events;
            list.sort_by(|a, b| a.beat.total_cmp(&b.beat).then_with(|| a.id.cmp(&b.id)));
        }
    }

    /// Insert a direction articulation event; replaces an existing event at
    /// (approximately) the same beat so the lane never holds two simultaneous
    /// directions. Returns the new event id.
    pub fn add_midi_articulation(
        &mut self,
        clip_id: &str,
        beat: f32,
        articulation: ArticulationId,
    ) -> Option<u64> {
        let list = self.midi_clip_articulations_mut(clip_id)?;
        const EPS: f32 = 1.0e-3;
        list.retain(|e| (e.beat - beat.max(0.0)).abs() > EPS);
        let event = MidiArticulationEvent::new(beat, articulation);
        let id = event.id;
        list.push(event);
        list.sort_by(|a, b| a.beat.total_cmp(&b.beat).then_with(|| a.id.cmp(&b.id)));
        Some(id)
    }

    /// Move a direction articulation event to a new beat. Returns `true` when
    /// the event changed.
    pub fn move_midi_articulation(&mut self, clip_id: &str, id: u64, beat: f32) -> bool {
        let Some(list) = self.midi_clip_articulations_mut(clip_id) else {
            return false;
        };
        let Some(event) = list.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        let beat = beat.max(0.0);
        if (event.beat - beat).abs() <= 1.0e-5 {
            return false;
        }
        event.beat = beat;
        list.sort_by(|a, b| a.beat.total_cmp(&b.beat).then_with(|| a.id.cmp(&b.id)));
        true
    }

    /// Delete direction articulation events by id. Returns how many were removed.
    pub fn delete_midi_articulations(&mut self, clip_id: &str, ids: &[u64]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let Some(list) = self.midi_clip_articulations_mut(clip_id) else {
            return 0;
        };
        let before = list.len();
        list.retain(|e| !ids.contains(&e.id));
        before - list.len()
    }

    /// Set (or clear, with `None`) the per-note articulation on the given note
    /// ids. Returns the number of notes changed. Never touches start /
    /// duration / velocity — articulation is playback-only metadata.
    pub fn set_midi_notes_articulation(
        &mut self,
        clip_id: &str,
        ids: &[u64],
        articulation: Option<ArticulationId>,
    ) -> usize {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) && note.articulation != articulation {
                note.articulation = articulation;
                changed += 1;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(beat: f32, articulation: ArticulationId) -> MidiArticulationEvent {
        MidiArticulationEvent::new(beat, articulation)
    }

    #[test]
    fn registry_tags_roundtrip_and_zero_is_none() {
        for id in ArticulationId::ALL {
            assert_eq!(ArticulationId::from_tag(id.to_tag()), Some(id));
            assert_eq!(id.definition().id, id);
        }
        assert_eq!(ArticulationId::from_tag(0), None);
        assert_eq!(ArticulationId::from_tag(200), None);
    }

    #[test]
    fn direction_chasing_picks_latest_event_at_or_before_beat() {
        let events = vec![
            event(0.0, ArticulationId::Sustain),
            event(4.0, ArticulationId::Staccato),
            event(8.0, ArticulationId::Legato),
        ];
        // Mid-clip lookups (start-from-middle / seek / loop restart all reduce
        // to this beat query).
        assert_eq!(
            direction_articulation_at(&events, 0.0),
            Some(ArticulationId::Sustain)
        );
        assert_eq!(
            direction_articulation_at(&events, 3.99),
            Some(ArticulationId::Sustain)
        );
        assert_eq!(
            direction_articulation_at(&events, 4.0),
            Some(ArticulationId::Staccato)
        );
        assert_eq!(
            direction_articulation_at(&events, 100.0),
            Some(ArticulationId::Legato)
        );
        // Before the first event: no direction in effect.
        let later_only = vec![event(2.0, ArticulationId::Staccato)];
        assert_eq!(direction_articulation_at(&later_only, 1.0), None);
    }

    #[test]
    fn per_note_articulation_wins_over_direction() {
        let events = vec![event(0.0, ArticulationId::Staccato)];
        let mut note = MidiNoteState::new(60, 1.0, 1.0, 100);
        assert_eq!(
            resolve_note_articulation(&note, &events),
            Some(ArticulationId::Staccato)
        );
        note.articulation = Some(ArticulationId::Tenuto);
        assert_eq!(
            resolve_note_articulation(&note, &events),
            Some(ArticulationId::Tenuto)
        );
    }

    #[test]
    fn playback_gates_clamp_to_min_note_length_and_velocity_range() {
        // Staccatissimo on an already-minimal note must not collapse to zero.
        let (len, vel) = apply_articulation_playback(
            0.0,
            MIN_NOTE_BEATS,
            1,
            ArticulationId::Staccatissimo,
            None,
        );
        assert!(len >= MIN_NOTE_BEATS);
        assert!((1..=127).contains(&vel));
        // Accent near the ceiling clamps at 127; velocity floor stays >= 1.
        let (_, vel) = apply_articulation_playback(0.0, 1.0, 127, ArticulationId::Accent, None);
        assert_eq!(vel, 127);
    }

    #[test]
    fn legato_overlap_reaches_past_next_note_start() {
        let (len, _) =
            apply_articulation_playback(0.0, 0.5, 100, ArticulationId::Legato, Some(2.0));
        assert!((len - (2.0 + LEGATO_OVERLAP_BEATS)).abs() < 1e-6);
        // Next note in the past (defensive): keep the plain gate.
        let (len, _) =
            apply_articulation_playback(4.0, 0.5, 100, ArticulationId::Legato, Some(2.0));
        assert!((len - 0.5).abs() < 1e-6);
    }

    #[test]
    fn state_helpers_insert_move_delete_and_replace_events() {
        let mut state = TimelineState::default();
        let track_id = state.create_midi_track();
        let clip_id = state.create_midi_clip(&track_id, 0.0, 8.0).unwrap();

        let a = state
            .add_midi_articulation(&clip_id, 4.0, ArticulationId::Staccato)
            .unwrap();
        let b = state
            .add_midi_articulation(&clip_id, 0.0, ArticulationId::Sustain)
            .unwrap();
        let events = state.midi_clip_articulations(&clip_id).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].beat < events[1].beat, "events stay beat-sorted");

        // Inserting at (nearly) the same beat replaces, never duplicates.
        let c = state
            .add_midi_articulation(&clip_id, 4.0, ArticulationId::Legato)
            .unwrap();
        let events = state.articulations_snapshot(&clip_id);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.id != a));
        assert_eq!(
            direction_articulation_at(&events, 5.0),
            Some(ArticulationId::Legato)
        );

        assert!(state.move_midi_articulation(&clip_id, c, 6.0));
        assert_eq!(
            direction_articulation_at(&state.articulations_snapshot(&clip_id), 5.0),
            Some(ArticulationId::Sustain)
        );

        assert_eq!(state.delete_midi_articulations(&clip_id, &[b, c]), 2);
        assert!(state.midi_clip_articulations(&clip_id).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_note_removes_its_per_note_articulation_with_it() {
        let mut state = TimelineState::default();
        let track_id = state.create_midi_track();
        let clip_id = state.create_midi_clip(&track_id, 0.0, 8.0).unwrap();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        state.set_midi_notes_articulation(&clip_id, &[id], Some(ArticulationId::Accent));
        assert_eq!(state.delete_midi_notes(&clip_id, &[id]), 1);
        assert!(state.midi_clip_notes(&clip_id).unwrap().is_empty());
        // Direction lane untouched by note deletes.
        state.add_midi_articulation(&clip_id, 0.0, ArticulationId::Sustain);
        let note = state.add_midi_note(&clip_id, 62, 1.0, 1.0, 100).unwrap();
        state.delete_midi_notes(&clip_id, &[note]);
        assert_eq!(state.midi_clip_articulations(&clip_id).unwrap().len(), 1);
    }
}
