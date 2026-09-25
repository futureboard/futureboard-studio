use super::*;
use sphere_midi_service::NoteExpression;

/// Smallest allowed note length, in beats (1/32 note). Mirrors the WebUI
/// `MIN_DUR` guard so a note can never collapse to zero width.
pub const MIN_NOTE_BEATS: f32 = 1.0 / 32.0;

/// Default length for a newly created MIDI clip (one 4/4 bar at any BPM).
pub const DEFAULT_MIDI_CLIP_BEATS: f32 = 4.0;

/// Minimum visible MIDI clip length after draw/resize. One 1/32 note keeps the
/// clip positive and hittable without forcing every short gesture to one bar.
pub const MIN_MIDI_CLIP_BEATS: f32 = 1.0 / 8.0;

#[inline]
fn snap_up_beats(value: f32, step: f32) -> f32 {
    if step <= 0.0 {
        return value;
    }
    ((value / step).ceil() * step).max(step)
}

#[derive(Debug, Clone, PartialEq)]
pub struct MidiNoteState {
    /// Stable identity. Persisted in the project file (v26+) so selection,
    /// undo, and clipboard targets survive save/load. Moves keep this id;
    /// copies / duplicates / splits mint a new one via [`MidiNoteState::new`].
    pub id: u64,
    pub pitch: u8,
    pub start: f32,    // beats relative to clip start
    pub duration: f32, // beats
    /// MIDI velocity in 1..=127.
    pub velocity: u8,
    /// Optional Note Off velocity (1..=127). `None` means the host/engine
    /// should use its default release velocity.
    pub release_velocity: Option<u8>,
    /// Muted notes remain in clip data but emit no runtime note event.
    pub muted: bool,
    /// Output channel this note plays back on when the owning track's
    /// [`MidiOutputChannelMode`] is `PerNote`; ignored (but preserved) when
    /// the track forces a `Fixed` channel. Defaults to channel 1.
    pub channel: MidiChannel,
    /// Per-note articulation. `None` falls back to the clip's direction
    /// articulation lane at the note start. Playback-only metadata: it never
    /// alters the stored start/duration/velocity (modifiers apply at engine
    /// snapshot build time). Travels with the note through move / copy /
    /// split / delete because it lives on the note itself.
    pub articulation: Option<ArticulationId>,
    /// Continuous pitch performance for this note: cent deviations relative to
    /// [`Self::pitch`], keyed by beats from [`Self::start`]. `None` means the
    /// note sounds at its notated pitch. Both anchors travel with the note, so
    /// transposing or moving it preserves the expressive shape exactly. See
    /// [`PitchCurve`].
    pub pitch_curve: Option<PitchCurve>,
    /// Protocol-neutral note-owned expression. MIDI channel is only used by
    /// the transport adapter and is not used to identify these curves.
    pub expression: NoteExpression,
    /// Musical accent for this note: how prominent it should feel and by what
    /// means. `None` means the note has never been analysed or drawn and is to
    /// be played as written — the same "absence means unmodified" convention
    /// [`Self::pitch_curve`] uses, so Reset restores a genuinely unaccented
    /// note rather than a row of neutral values. See [`AccentState`].
    pub accent: Option<AccentState>,
}

impl MidiNoteState {
    /// Construct a note with a freshly minted stable id. `pitch` is clamped
    /// to 0..=127, `velocity` to 1..=127, and `duration` to at least
    /// [`MIN_NOTE_BEATS`]. The note is created unmuted with no release velocity.
    pub fn new(pitch: u8, start: f32, duration: f32, velocity: u8) -> Self {
        Self {
            id: next_midi_note_id(),
            pitch: pitch.min(127),
            start: start.max(0.0),
            duration: duration.max(MIN_NOTE_BEATS),
            velocity: velocity.clamp(1, 127),
            release_velocity: None,
            muted: false,
            channel: MidiChannel::default(),
            articulation: None,
            pitch_curve: None,
            expression: NoteExpression::default(),
            accent: None,
        }
    }

    /// Restore a note from persisted project data, observing the id so later
    /// mints cannot collide.
    pub fn from_persisted(
        id: u64,
        pitch: u8,
        start: f32,
        duration: f32,
        velocity: u8,
        release_velocity: Option<u8>,
    ) -> Self {
        let id = if id == 0 {
            next_midi_note_id()
        } else {
            observe_midi_note_id(id);
            id
        };
        Self {
            id,
            pitch: pitch.min(127),
            start: start.max(0.0),
            duration: duration.max(MIN_NOTE_BEATS),
            velocity: velocity.clamp(1, 127),
            release_velocity: release_velocity.map(|v| v.clamp(1, 127)).filter(|&v| v > 0),
            muted: false,
            channel: MidiChannel::default(),
            articulation: None,
            pitch_curve: None,
            expression: NoteExpression::default(),
            accent: None,
        }
    }
}

impl TimelineState {
    // ── MIDI clip / note mutations ────────────────────────────────────────
    // Single source of truth for piano-roll edits. The piano-roll editor calls
    // these inside `Timeline::update` and then marks the project dirty so the
    // engine sync + autosave see the change. Notes are stored relative to the
    // clip start (matches the WebUI model). Every mutation clamps to valid
    // ranges so a bad gesture can never produce an out-of-range note.

    /// Grid step in beats for snapping clip bounds (matches arrangement snap).
    pub fn midi_snap_step_beats(&self) -> f32 {
        let bpb = self.beats_per_bar();
        if !self.snap_to_grid || self.grid_division == SnapDivision::Off {
            return 0.25;
        }
        match self.grid_division {
            SnapDivision::Auto => {
                self.get_grid_sub_beats(self.viewport.pixels_per_second * self.seconds_per_beat())
            }
            SnapDivision::Bar1 => bpb,
            other => other.step_beats(bpb),
        }
        .max(1.0 / 32.0)
    }

    fn next_midi_clip_display_name(&self) -> String {
        let mut count = 0u32;
        for track in &self.tracks {
            for clip in &track.clips {
                if matches!(clip.clip_type, ClipType::Midi { .. }) {
                    count += 1;
                }
            }
        }
        format!("MIDI {}", count + 1)
    }

    /// Expand `clip.duration_beats` so every note fits inside the clip, with
    /// optional grid padding. Does not shrink. Returns `true` if length changed.
    pub fn ensure_midi_clip_contains_notes(clip: &mut ClipState, snap_beats: f32) -> bool {
        let ClipType::Midi { notes, .. } = &clip.clip_type else {
            return false;
        };
        let max_note_end = notes
            .iter()
            .map(|n| n.start.max(0.0) + n.duration.max(MIN_NOTE_BEATS))
            .fold(0.0f32, f32::max);
        let min_len = DEFAULT_MIDI_CLIP_BEATS.max(MIN_MIDI_CLIP_BEATS);
        let needed =
            snap_up_beats(max_note_end.max(min_len), snap_beats.max(1.0 / 32.0)).max(min_len);
        if needed > clip.duration_beats + 1.0e-4 {
            let old = clip.duration_beats;
            clip.duration_beats = needed;
            if midi_debug_enabled() {
                eprintln!(
                    "[midi] clip auto-expanded clip={} old_len={:.3} new_len={:.3} notes={}",
                    clip.id,
                    old,
                    needed,
                    notes.len()
                );
            }
            return true;
        }
        false
    }

    /// Expand a MIDI clip's length so it contains all of its notes, snapping the
    /// new length up to the current grid. Never shrinks — note deletes leave the
    /// clip length untouched (expansion is sticky). Returns `true` if the length
    /// grew. This is the single auto-expand entry point for note edits.
    pub fn expand_clip_to_contain_notes(&mut self, clip_id: &str) -> bool {
        let step = self.midi_snap_step_beats();
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.id == clip_id {
                    return Self::ensure_midi_clip_contains_notes(clip, step);
                }
            }
        }
        false
    }

    /// Create an empty MIDI clip on `track_id` at `start_beat` (snapped by the
    /// caller if desired). Returns the new clip id, or `None` if the track is
    /// missing. The clip is selected so the editor can pick it up immediately.
    pub fn create_midi_clip(
        &mut self,
        track_id: &str,
        start_beat: f32,
        length_beats: f32,
    ) -> Option<String> {
        let clip = self.build_midi_clip(track_id, start_beat, length_beats)?;
        let clip_id = clip.id.clone();
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.clips.push(clip);
        }
        self.selection.selected_track_id = Some(track_id.to_string());
        self.selection.selected_clip_ids = vec![clip_id.clone()];
        if crate::forensic_trace::midi_model_trace_enabled() {
            eprintln!(
                "[midi-model] clip_created track={track_id} clip={clip_id} \
                 start_beats={start_beat:.3} length_beats={length_beats:.3}"
            );
        }
        Some(clip_id)
    }

    /// Borrow the notes of a MIDI clip by id.
    pub fn midi_clip_notes(&self, clip_id: &str) -> Option<&Vec<MidiNoteState>> {
        for track in &self.tracks {
            for clip in &track.clips {
                if clip.id == clip_id {
                    if let ClipType::Midi { notes, .. } = &clip.clip_type {
                        return Some(notes);
                    }
                }
            }
        }
        None
    }

    pub(crate) fn midi_clip_notes_mut(&mut self, clip_id: &str) -> Option<&mut Vec<MidiNoteState>> {
        // Every mutation of a clip's notes passes through here, which makes it
        // the one place a derived-view cache can be told to refresh. See
        // `midi_edit_revision`.
        bump_midi_clip_revision(clip_id);
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.id == clip_id {
                    if let ClipType::Midi { notes, .. } = &mut clip.clip_type {
                        return Some(notes);
                    }
                }
            }
        }
        None
    }

    /// Clamp a note start/duration so it fits inside `clip_len`. Returns `None`
    /// when the note would lie entirely outside the clip.
    pub fn clamp_note_to_clip_bounds(
        start: f32,
        duration: f32,
        clip_len: f32,
    ) -> Option<(f32, f32)> {
        let start = start.max(0.0);
        if start >= clip_len {
            return None;
        }
        let max_dur = (clip_len - start).max(MIN_NOTE_BEATS);
        let duration = duration.max(MIN_NOTE_BEATS).min(max_dur);
        if start + duration > clip_len + 1.0e-4 {
            return None;
        }
        Some((start, duration))
    }

    /// Create a MIDI clip, returning the full clip state for undo commands.
    pub fn build_midi_clip(
        &mut self,
        track_id: &str,
        start_beat: f32,
        length_beats: f32,
    ) -> Option<ClipState> {
        if !self.tracks.iter().any(|t| t.id == track_id) {
            return None;
        }
        let clip_id = self.next_clip_id();
        let name = self.next_midi_clip_display_name();
        let len = length_beats.max(MIN_MIDI_CLIP_BEATS);
        Some(ClipState {
            id: clip_id,
            name,
            start_beat: start_beat.max(0.0),
            duration_beats: len,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Midi {
                notes: Vec::new(),
                controller_lanes: Vec::new(),
                sysex_events: Vec::new(),
                articulations: Vec::new(),
            },
            muted: false,
            audio_import: AudioImportState::default(),
            stretch: AudioClipStretchState::default(),
        })
    }

    pub fn build_imported_midi_clip(
        &mut self,
        track_id: &str,
        name: String,
        start_beat: f32,
        imported: crate::components::timeline::midi_import::ImportedMidiClip,
    ) -> Option<ClipState> {
        if !self.tracks.iter().any(|t| t.id == track_id) {
            return None;
        }
        let len = snap_up_beats(
            imported.duration_beats.max(MIN_MIDI_CLIP_BEATS),
            self.midi_snap_step_beats().max(MIN_NOTE_BEATS),
        )
        .max(MIN_MIDI_CLIP_BEATS);
        Some(ClipState {
            id: self.next_clip_id(),
            name,
            start_beat: start_beat.max(0.0),
            duration_beats: len,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Midi {
                notes: imported.notes,
                controller_lanes: imported.controller_lanes,
                sysex_events: imported
                    .sysex_events
                    .into_iter()
                    .map(|event| MidiSysExEvent {
                        kind: match event.kind {
                            crate::components::timeline::midi_import::ImportedSysExKind::Normal => {
                                MidiSysExKind::Normal
                            }
                            crate::components::timeline::midi_import::ImportedSysExKind::Escaped => {
                                MidiSysExKind::Escaped
                            }
                        },
                        tick: event.absolute_tick,
                        beat: event.beat,
                        data: event.data,
                    })
                    .collect(),
                articulations: Vec::new(),
            },
            muted: false,
            audio_import: AudioImportState::default(),
            stretch: AudioClipStretchState::default(),
        })
    }

    /// Add a note to a MIDI clip. Returns the new note id.
    pub fn add_midi_note(
        &mut self,
        clip_id: &str,
        pitch: u8,
        start: f32,
        duration: f32,
        velocity: u8,
    ) -> Option<u64> {
        let clip_len = self.clip_duration_beats(clip_id)?;
        let (start, duration) = Self::clamp_note_to_clip_bounds(start, duration, clip_len)?;
        let note = MidiNoteState::new(pitch, start, duration, velocity);
        let id = note.id;
        let notes = self.midi_clip_notes_mut(clip_id)?;
        notes.push(note);
        if crate::forensic_trace::midi_model_trace_enabled() {
            eprintln!(
                "[midi-model] note_added clip={clip_id} pitch={} start_beats={start:.3} \
                 length_beats={duration:.3} velocity={}",
                pitch.min(127),
                velocity.clamp(1, 127)
            );
        }
        Some(id)
    }

    /// Apply absolute start/pitch to a set of notes (move gesture). Each tuple
    /// is `(note_id, new_start_beats, new_pitch)`.
    pub fn move_midi_notes(&mut self, clip_id: &str, updates: &[(u64, f32, u8)]) {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return;
        };
        // Notes keep their duration and may pass the current clip end; the clip
        // is auto-expanded below so a moved note never lives outside its clip.
        // Start clamps to clip-local beat 0; pitch clamps to 0..=127.
        for (id, new_start, new_pitch) in updates {
            if let Some(note) = notes.iter_mut().find(|n| n.id == *id) {
                note.start = new_start.max(0.0);
                note.pitch = (*new_pitch).min(127);
            }
        }
        self.expand_clip_to_contain_notes(clip_id);
        if midi_debug_enabled() {
            eprintln!("[midi] move_notes clip={} count={}", clip_id, updates.len());
        }
    }

    /// Set a note's length (resize gesture), clamped to [`MIN_NOTE_BEATS`].
    pub fn resize_midi_note(&mut self, clip_id: &str, id: u64, new_duration: f32) {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return;
        };
        // Right-edge resize may grow the note past the clip end; the clip is
        // auto-expanded below rather than the note being clamped to fit.
        if let Some(note) = notes.iter_mut().find(|n| n.id == id) {
            note.duration = new_duration.max(MIN_NOTE_BEATS);
            if midi_debug_enabled() {
                eprintln!(
                    "[midi] resize_note clip={} id={} dur={:.3}",
                    clip_id, id, note.duration
                );
            }
        }
        self.expand_clip_to_contain_notes(clip_id);
    }

    /// Delete the given note ids from a MIDI clip. Returns how many were removed.
    pub fn delete_midi_notes(&mut self, clip_id: &str, ids: &[u64]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let ids: std::collections::HashSet<u64> = ids.iter().copied().collect();
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let before = notes.len();
        notes.retain(|n| !ids.contains(&n.id));
        let removed = before - notes.len();
        if removed > 0 && midi_debug_enabled() {
            eprintln!("[midi] delete_notes clip={} removed={}", clip_id, removed);
        }
        removed
    }

    /// Set a note's velocity (1..=127).
    pub fn set_midi_note_velocity(&mut self, clip_id: &str, id: u64, velocity: u8) {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return;
        };
        if let Some(note) = notes.iter_mut().find(|n| n.id == id) {
            note.velocity = velocity.clamp(1, 127);
        }
    }

    /// Set a note's pitch (0..=127). Returns true when the note changed.
    pub fn set_midi_note_pitch(&mut self, clip_id: &str, id: u64, pitch: u8) -> bool {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return false;
        };
        let Some(note) = notes.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        let pitch = pitch.min(127);
        if note.pitch == pitch {
            return false;
        }
        note.pitch = pitch;
        true
    }

    /// Set a note's start in clip-local beats. Returns true when the note changed.
    pub fn set_midi_note_start(&mut self, clip_id: &str, id: u64, start: f32) -> bool {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return false;
        };
        let Some(note) = notes.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        let start = start.max(0.0);
        if (note.start - start).abs() <= 1.0e-4 {
            return false;
        }
        note.start = start;
        self.expand_clip_to_contain_notes(clip_id);
        true
    }

    /// Set a note's length in beats. Returns true when the note changed.
    pub fn set_midi_note_length(&mut self, clip_id: &str, id: u64, duration: f32) -> bool {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return false;
        };
        let Some(note) = notes.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        let duration = duration.max(MIN_NOTE_BEATS);
        if (note.duration - duration).abs() <= 1.0e-4 {
            return false;
        }
        note.duration = duration;
        self.expand_clip_to_contain_notes(clip_id);
        true
    }

    /// Set velocity for selected notes. Returns the number of notes changed.
    pub fn set_midi_notes_velocity_bulk(
        &mut self,
        clip_id: &str,
        ids: &[u64],
        velocity: u8,
    ) -> usize {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let velocity = velocity.clamp(1, 127);
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) && note.velocity != velocity {
                note.velocity = velocity;
                changed += 1;
            }
        }
        changed
    }

    /// Set the accent prominence of a set of notes, marking them hand-edited.
    ///
    /// A note that has never been analysed gains a neutral accent first, so a
    /// drag on an un-analysed clip is a way of authoring accents by hand rather
    /// than a no-op that waits for a model. The three sub-components follow
    /// prominence — see [`AccentState::with_prominence`] — so a dragged note
    /// keeps whatever character the analyser gave it while changing degree.
    pub fn set_midi_notes_accent_bulk(
        &mut self,
        clip_id: &str,
        ids: &[u64],
        prominence: f32,
    ) -> usize {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let prominence = prominence.clamp(0.0, 1.0);
        let mut changed = 0;
        for note in notes.iter_mut() {
            if !ids.contains(&note.id) {
                continue;
            }
            let next = note
                .accent
                .unwrap_or_else(AccentState::neutral)
                .with_prominence(prominence);
            if note.accent != Some(next) {
                note.accent = Some(next);
                changed += 1;
            }
        }
        changed
    }

    /// Remove the accent from a set of notes, returning them to "as written".
    ///
    /// Distinct from setting the accent to neutral: an absent accent is a note
    /// that has never been analysed, and a re-analysis that preserves manual
    /// edits will fill it in. A note deliberately set to neutral by hand will
    /// not be.
    pub fn clear_midi_notes_accent(&mut self, clip_id: &str, ids: &[u64]) -> usize {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) && note.accent.take().is_some() {
                changed += 1;
            }
        }
        changed
    }

    /// Overwrite the mutable fields of existing notes from full snapshots,
    /// matched by id. Used by the `EditMidiNotes` undo command — the note set is
    /// not changed, only field values. Auto-expands the clip afterwards.
    pub fn overwrite_midi_notes(&mut self, clip_id: &str, states: &[MidiNoteState]) {
        if let Some(notes) = self.midi_clip_notes_mut(clip_id) {
            for s in states {
                if let Some(note) = notes.iter_mut().find(|n| n.id == s.id) {
                    note.pitch = s.pitch;
                    note.start = s.start;
                    note.duration = s.duration;
                    note.velocity = s.velocity;
                    note.muted = s.muted;
                    note.channel = s.channel;
                    note.articulation = s.articulation;
                    // Pitch expression is part of the note, so an
                    // `EditMidiNotes` entry undoes/redoes it with everything
                    // else the same gesture touched.
                    note.pitch_curve = s.pitch_curve.clone();
                    // Protocol-neutral note expression travels with the
                    // snapshot too, so expression drawing and transforms are
                    // one undoable note edit just like pitch performance.
                    note.expression = s.expression.clone();
                    // So is accent, and for the same reason. Without this line
                    // Analyze Accent would apply and never come back: the
                    // command records a before/after note snapshot and undo
                    // restores every field this function names.
                    note.accent = s.accent;
                }
            }
        }
        self.expand_clip_to_contain_notes(clip_id);
    }

    /// Set the muted flag on the given note ids. Returns the number changed.
    pub fn set_midi_notes_muted(&mut self, clip_id: &str, ids: &[u64], muted: bool) -> usize {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) && note.muted != muted {
                note.muted = muted;
                changed += 1;
            }
        }
        changed
    }

    /// Transpose selected notes by semitones. Returns the number of notes changed.
    pub fn transpose_midi_notes(&mut self, clip_id: &str, ids: &[u64], semitones: i32) -> usize {
        if semitones == 0 {
            return 0;
        }
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) {
                let pitch = (note.pitch as i32 + semitones).clamp(0, 127) as u8;
                if note.pitch != pitch {
                    note.pitch = pitch;
                    changed += 1;
                }
            }
        }
        changed
    }

    /// Set the MIDI output channel on the given note ids. Returns the number
    /// of notes changed.
    pub fn set_midi_notes_channel(
        &mut self,
        clip_id: &str,
        ids: &[u64],
        channel: MidiChannel,
    ) -> usize {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) && note.channel != channel {
                note.channel = channel;
                changed += 1;
            }
        }
        changed
    }

    /// Shift the given note ids' MIDI channel by `delta`, clamped to
    /// channel 1..=16. Returns the number of notes changed.
    pub fn nudge_midi_notes_channel(&mut self, clip_id: &str, ids: &[u64], delta: i32) -> usize {
        if delta == 0 {
            return 0;
        }
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) {
                let next =
                    MidiChannel::from_ui((note.channel.ui() as i32 + delta).clamp(1, 16) as u8);
                if note.channel != next {
                    note.channel = next;
                    changed += 1;
                }
            }
        }
        changed
    }

    /// Snap the given note ids' pitches to the nearest pitch in `scale`.
    /// Returns the number of notes changed. A no-op for a `Chromatic` scale,
    /// since every pitch is already "in scale".
    pub fn snap_midi_notes_to_scale(
        &mut self,
        clip_id: &str,
        ids: &[u64],
        scale: MidiScale,
    ) -> usize {
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return 0;
        };
        let mut changed = 0;
        for note in notes.iter_mut() {
            if ids.contains(&note.id) {
                let pitch = scale.nearest_pitch(note.pitch);
                if note.pitch != pitch {
                    note.pitch = pitch;
                    changed += 1;
                }
            }
        }
        changed
    }

    /// Quantize the given note starts (or all notes when `ids` is empty) to the
    /// supplied grid step in beats. Rounds to the nearest step.
    pub fn quantize_midi_notes(&mut self, clip_id: &str, ids: &[u64], step_beats: f32) {
        if step_beats <= 0.0 {
            return;
        }
        let Some(notes) = self.midi_clip_notes_mut(clip_id) else {
            return;
        };
        let mut count = 0;
        for note in notes.iter_mut() {
            if ids.is_empty() || ids.contains(&note.id) {
                note.start = ((note.start / step_beats).round() * step_beats).max(0.0);
                count += 1;
            }
        }
        if midi_debug_enabled() {
            eprintln!(
                "[midi] quantize clip={} count={} step={:.4}",
                clip_id, count, step_beats
            );
        }
    }

    pub fn import_midi_at(
        &mut self,
        clip_name: String,
        imported: crate::components::timeline::midi_import::ImportedMidiClip,
        drop_x: f32,
        drop_y: f32,
    ) -> Option<(String, ClipState)> {
        self.import_midi_tracks_at(
            clip_name,
            vec![
                crate::components::timeline::midi_import::ImportedMidiTrack {
                    name: None,
                    channel_hint: None,
                    clip: imported,
                },
            ],
            drop_x,
            drop_y,
        )
        .into_iter()
        .next()
    }

    pub fn import_midi_tracks_at(
        &mut self,
        file_stem: String,
        imported_tracks: Vec<crate::components::timeline::midi_import::ImportedMidiTrack>,
        drop_x: f32,
        drop_y: f32,
    ) -> Vec<(String, ClipState)> {
        let start_beat = self.snap_beats(self.x_to_beats(drop_x.max(0.0))).max(0.0);
        let mut markers = Vec::new();
        let mut musical_tracks = Vec::new();
        for track in imported_tracks {
            markers.extend(track.clip.markers.iter().cloned());
            if imported_midi_clip_has_payload(&track.clip) {
                musical_tracks.push(track);
            }
        }
        if musical_tracks.is_empty() {
            return Vec::new();
        }

        self.import_midi_markers(start_beat, &markers);

        let first_track_id = match self.track_index_at_y(drop_y) {
            Some(idx)
                if matches!(
                    self.tracks[idx].track_type,
                    TrackType::Midi | TrackType::Instrument
                ) =>
            {
                self.tracks[idx].id.clone()
            }
            _ => self.create_midi_track(),
        };

        let multi_track = musical_tracks.len() > 1;
        let mut clips = Vec::with_capacity(musical_tracks.len());
        // `next_clip_id()` only scans clips already attached to `self.tracks`.
        // Clips built in this loop aren't pushed onto their track until the
        // caller applies the returned batch (`BatchCreateClips`), so calling
        // `next_clip_id()` per iteration would hand out the *same* id to every
        // channel-split clip. Reserve one id up front and increment it
        // locally so each clip in the batch gets a distinct id — otherwise
        // clips across split tracks share an id and selecting one selects
        // all of them (they compare equal).
        let mut next_id_num = next_clip_id_number(&self.tracks);
        for (index, imported_track) in musical_tracks.into_iter().enumerate() {
            let track_id = if index == 0 {
                first_track_id.clone()
            } else {
                self.create_midi_track()
            };
            if let Some(channel) = imported_track.channel_hint {
                self.set_track_midi_channel(&track_id, Some(channel.ui()));
            }
            let channel_hint = imported_track.channel_hint;
            let clip_name = imported_track
                .name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| {
                    if let Some(channel) = channel_hint {
                        format!("{} Ch {}", file_stem, channel.ui())
                    } else if multi_track {
                        format!("{} T{}", file_stem, index + 1)
                    } else {
                        file_stem.clone()
                    }
                });
            if let Some(mut clip) =
                self.build_imported_midi_clip(&track_id, clip_name, start_beat, imported_track.clip)
            {
                clip.id = format!("clip-{next_id_num}");
                next_id_num += 1;
                clips.push((track_id, clip));
            }
        }
        clips
    }
}

fn imported_midi_clip_has_payload(
    clip: &crate::components::timeline::midi_import::ImportedMidiClip,
) -> bool {
    !clip.notes.is_empty() || !clip.controller_lanes.is_empty() || !clip.sysex_events.is_empty()
}
