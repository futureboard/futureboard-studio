//! An editing session: the note document, its undo history, and the engine
//! that plays it.
//!
//! The session owns the truth (the [`VoxDocument`]); the engine gets a copy
//! after every change, so what is drawn and what plays never disagree. Every
//! front end — Studio's tuner panel, the desktop app, the VST3/ARA plug-in —
//! edits through this type, so they share one undo model and one file format
//! with each other (and the phone's note JSON).
//!
//! Not realtime: everything here runs on the UI thread. The engine is reached
//! through [`Shared::lock`], held only for the copy.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::edit::{
    self, MIN_CUT_SEC, Macro, MacroContext, TimingEdge, clamp_timing, join_notes,
    nearest_scale_note, relabel, reset_note, split_note, unique_id,
};
use crate::engine::{CantoAnalysisResult, CantoNote, DEFAULT_BPM, KEY_NAMES};
use crate::scale::Scale;
use crate::shared::Shared;

/// Undo steps kept; older ones are dropped.
pub const HISTORY_LIMIT: usize = 200;

/// Everything a user edit can change. Serialized as the session file /
/// plug-in state; notes use the same JSON as the phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoxDocument {
    pub notes: Vec<CantoNote>,
    /// Key as a pitch class, 0 = C.
    pub key: u8,
    pub scale: Scale,
    /// Global pitch correction toward the scale, 0..=1.
    pub correct: f32,
    pub bpm: f32,
    /// Counter behind new note ids (cut / join).
    #[serde(default)]
    pub id_seq: u32,
}

impl Default for VoxDocument {
    fn default() -> Self {
        Self {
            notes: Vec::new(),
            key: 9,
            scale: Scale::Minor,
            correct: 0.0,
            bpm: DEFAULT_BPM,
            id_seq: 0,
        }
    }
}

impl VoxDocument {
    /// The document an analysis produced: detected notes, key and tempo.
    pub fn from_analysis(result: &CantoAnalysisResult) -> Self {
        let key = KEY_NAMES.iter().position(|k| *k == result.key).unwrap_or(9) as u8;
        let scale = match result.mode.as_str() {
            "major" => Scale::Major,
            "penta" => Scale::MinorPentatonic,
            _ => Scale::Minor,
        };
        Self {
            notes: result.notes.clone(),
            key,
            scale,
            correct: 0.0,
            bpm: result.bpm,
            id_seq: 0,
        }
    }

    /// The document an engine holds (after a host loaded a take into it).
    pub fn from_engine(engine: &crate::engine::CantoEngine) -> Self {
        Self {
            notes: engine.notes.clone(),
            key: engine.key % 12,
            scale: engine.scale,
            correct: engine.correct_amount,
            bpm: engine.bpm,
            id_seq: 0,
        }
    }

    /// This document as an analysis, to install with audio
    /// ([`crate::CantoEngine::install_analyzed`]) without analysing again.
    pub fn to_analysis(&self) -> crate::engine::Analysis {
        crate::engine::Analysis {
            key: self.key % 12,
            scale: self.scale,
            bpm: self.bpm,
            notes: self.notes.clone(),
        }
    }

    /// Copies the document into `engine` (key, scale, tempo, notes).
    pub fn apply_to(&self, engine: &mut crate::engine::CantoEngine) {
        engine.key = self.key % 12;
        engine.scale = self.scale;
        engine.bpm = self.bpm;
        engine.correct_amount = self.correct;
        engine.set_notes(self.notes.clone());
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }

    pub fn key_name(&self) -> &'static str {
        KEY_NAMES[self.key as usize % 12]
    }

    /// Pulls every unpinned pitched note toward the scale by `correct`.
    pub fn reapply_correct(&mut self) {
        for n in self.notes.iter_mut().filter(|n| !n.unpitched && !n.pinned) {
            let target = nearest_scale_note(n.detected, self.key, self.scale) as f32;
            n.pitch = n.detected + (target - n.detected) * self.correct;
        }
        relabel(&mut self.notes, self.key);
    }
}

/// One undo step: the document as it was before the edit named `label`.
#[derive(Debug, Clone)]
struct Step {
    label: String,
    doc: VoxDocument,
}

/// A row of the history list, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    pub label: String,
    /// Position to pass to [`VoxSession::jump_to`].
    pub position: usize,
    pub current: bool,
}

pub struct VoxSession {
    doc: VoxDocument,
    undo: Vec<Step>,
    redo: Vec<Step>,
    /// Open gesture: the document before it, until [`Self::commit`].
    open: Option<Step>,
    shared: Arc<Shared>,
    /// Length of the source audio in seconds (timing edits stay inside it).
    duration: f32,
    revision: u64,
}

impl VoxSession {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            doc: VoxDocument::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            open: None,
            shared,
            duration: 0.0,
            revision: 0,
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn doc(&self) -> &VoxDocument {
        &self.doc
    }

    pub fn notes(&self) -> &[CantoNote] {
        &self.doc.notes
    }

    pub fn duration(&self) -> f32 {
        self.duration
    }

    /// Bumped on every change, for render caches.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Starts over on a newly loaded take: `doc` (a saved document, or the
    /// analysis's) with an empty history.
    pub fn reset(&mut self, doc: VoxDocument, duration: f32) {
        self.doc = doc;
        self.duration = duration;
        self.undo.clear();
        self.redo.clear();
        self.open = None;
        self.sync_all();
    }

    // ---- Gestures and history ------------------------------------------

    /// Opens an undo step named `label` (a drag, a slider, a dialog). Edits
    /// until [`Self::commit`] become one step. A second `begin` while one is
    /// open keeps the first.
    pub fn begin(&mut self, label: &str) {
        if self.open.is_none() {
            self.open = Some(Step {
                label: label.to_string(),
                doc: self.doc.clone(),
            });
        }
    }

    /// Closes the open step; a gesture that changed nothing leaves no step.
    pub fn commit(&mut self) {
        let Some(step) = self.open.take() else {
            return;
        };
        if step.doc == self.doc {
            return;
        }
        self.undo.push(step);
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Abandons the open step and restores the document from before it.
    pub fn cancel(&mut self) {
        if let Some(step) = self.open.take() {
            self.doc = step.doc;
            self.sync_all();
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// One complete edit as one undo step.
    pub fn edit<R>(&mut self, label: &str, change: impl FnOnce(&mut VoxDocument) -> R) -> R {
        let nested = self.open.is_some();
        self.begin(label);
        let out = change(&mut self.doc);
        relabel(&mut self.doc.notes, self.doc.key);
        self.sync_all();
        if !nested {
            self.commit();
        }
        out
    }

    /// Mutable access to one note inside an open gesture. Call
    /// [`Self::sync_note`] after changing it.
    pub fn note_mut(&mut self, index: usize) -> Option<&mut CantoNote> {
        debug_assert!(self.open.is_some(), "note_mut outside a gesture");
        self.doc.notes.get_mut(index)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|s| s.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }

    pub fn undo(&mut self) -> bool {
        self.commit();
        let Some(step) = self.undo.pop() else {
            return false;
        };
        let now = std::mem::replace(&mut self.doc, step.doc);
        self.redo.push(Step {
            label: step.label,
            doc: now,
        });
        self.sync_all();
        true
    }

    pub fn redo(&mut self) -> bool {
        self.commit();
        let Some(step) = self.redo.pop() else {
            return false;
        };
        let now = std::mem::replace(&mut self.doc, step.doc);
        self.undo.push(Step {
            label: step.label,
            doc: now,
        });
        self.sync_all();
        true
    }

    /// Every step, oldest first: position 0 is the loaded take, position `k`
    /// is the state after the `k`-th edit.
    pub fn history(&self) -> Vec<HistoryRow> {
        let done = self.undo.len();
        let mut rows = vec![HistoryRow {
            label: String::new(),
            position: 0,
            current: done == 0,
        }];
        for (i, step) in self.undo.iter().enumerate() {
            rows.push(HistoryRow {
                label: step.label.clone(),
                position: i + 1,
                current: i + 1 == done,
            });
        }
        for (i, step) in self.redo.iter().rev().enumerate() {
            rows.push(HistoryRow {
                label: step.label.clone(),
                position: done + i + 1,
                current: false,
            });
        }
        rows
    }

    /// Moves to history `position`, applying only the final state.
    pub fn jump_to(&mut self, position: usize) {
        self.commit();
        let mut moved = false;
        while self.undo.len() > position {
            let step = self.undo.pop().unwrap();
            let now = std::mem::replace(&mut self.doc, step.doc);
            self.redo.push(Step {
                label: step.label,
                doc: now,
            });
            moved = true;
        }
        while self.undo.len() < position {
            let Some(step) = self.redo.pop() else {
                break;
            };
            let now = std::mem::replace(&mut self.doc, step.doc);
            self.undo.push(Step {
                label: step.label,
                doc: now,
            });
            moved = true;
        }
        if moved {
            self.sync_all();
        }
    }

    // ---- Engine sync ---------------------------------------------------

    /// Copies the whole document to the engine.
    pub fn sync_all(&mut self) {
        self.revision += 1;
        let notes = self.doc.notes.clone();
        let mut engine = self.shared.lock();
        engine.key = self.doc.key % 12;
        engine.scale = self.doc.scale;
        engine.bpm = self.doc.bpm;
        engine.correct_amount = self.doc.correct;
        engine.set_notes(notes);
    }

    /// Copies one note to the engine (a drag on that note).
    pub fn sync_note(&mut self, index: usize) {
        self.revision += 1;
        let Some(note) = self.doc.notes.get(index).cloned() else {
            return;
        };
        let mut engine = self.shared.lock();
        if let Some(slot) = engine.notes.get_mut(index) {
            *slot = note;
        }
    }

    // ---- Edits -----------------------------------------------------------

    pub fn set_key(&mut self, key: u8) {
        if self.doc.key == key % 12 {
            return;
        }
        let label = format!("key:{}", KEY_NAMES[key as usize % 12]);
        self.edit(&label, |doc| {
            doc.key = key % 12;
            doc.reapply_correct();
        });
    }

    pub fn set_scale(&mut self, scale: Scale) {
        if self.doc.scale == scale {
            return;
        }
        self.edit(&format!("mode:{}", scale.label()), |doc| {
            doc.scale = scale;
            doc.reapply_correct();
        });
    }

    /// Correction amount; call inside a gesture (`begin("correct")`) while
    /// dragging, or on its own for one step.
    pub fn set_correct(&mut self, amount: f32) {
        self.edit("correct", |doc| {
            doc.correct = amount.clamp(0.0, 1.0);
            doc.reapply_correct();
        });
    }

    /// Cuts note `index` at `time` seconds. Returns the second half's index.
    pub fn cut(&mut self, index: usize, time: f32) -> Option<usize> {
        let note = self.doc.notes.get(index)?.clone();
        let mut seq = self.doc.id_seq;
        let ids = (
            unique_id(&self.doc.notes, &note.id, &mut seq),
            unique_id(&self.doc.notes, &note.id, &mut seq),
        );
        let (a, b) = split_note(&note, time, ids, MIN_CUT_SEC)?;
        self.edit("cut", |doc| {
            doc.id_seq = seq;
            doc.notes.splice(index..=index, [a, b]);
        });
        Some(index + 1)
    }

    /// Joins note `index` with the next one. Returns the joined note's index.
    pub fn join_next(&mut self, index: usize) -> Option<usize> {
        let a = self.doc.notes.get(index)?.clone();
        let b = self.doc.notes.get(index + 1)?.clone();
        let mut seq = self.doc.id_seq;
        let id = unique_id(&self.doc.notes, &a.id, &mut seq);
        let joined = join_notes(&a, &b, id);
        self.edit("join", |doc| {
            doc.id_seq = seq;
            doc.notes.splice(index..=index + 1, [joined]);
        });
        Some(index)
    }

    pub fn toggle_mute(&mut self, index: usize) {
        let Some(muted) = self.doc.notes.get(index).map(|n| n.muted) else {
            return;
        };
        self.edit(if muted { "unmute" } else { "mute" }, |doc| {
            doc.notes[index].muted = !muted;
        });
    }

    pub fn reset_note(&mut self, index: usize) {
        if index >= self.doc.notes.len() {
            return;
        }
        self.edit("reset", |doc| {
            let key = doc.key;
            reset_note(&mut doc.notes[index], key);
        });
    }

    /// Snaps one note's centre fully onto the scale ("tune note").
    pub fn tune_note(&mut self, index: usize) {
        let Some(n) = self.doc.notes.get(index) else {
            return;
        };
        if n.unpitched {
            return;
        }
        let target = nearest_scale_note(n.pitch, self.doc.key, self.doc.scale) as f32;
        self.edit("tune", |doc| {
            let n = &mut doc.notes[index];
            n.pitch = target;
            n.pinned = true;
        });
    }

    pub fn set_ornament(&mut self, index: usize, style: u8) {
        if index >= self.doc.notes.len() {
            return;
        }
        self.edit("orn", |doc| {
            edit::set_ornament_style(&mut doc.notes[index], style)
        });
    }

    pub fn clear_expression(&mut self, index: usize) {
        if index >= self.doc.notes.len() {
            return;
        }
        self.edit("exprClear", |doc| {
            let n = &mut doc.notes[index];
            n.orn_style = crate::engine::ORN_NONE;
            n.accent = 0.0;
        });
    }

    /// Runs a macro over `only` (or every note). Returns how many changed.
    pub fn apply_macro(&mut self, only: Option<usize>, op: Macro) -> usize {
        let cx = MacroContext {
            key: self.doc.key,
            scale: self.doc.scale,
            bpm: self.doc.bpm,
            duration: self.duration,
        };
        self.edit(&op.label(), |doc| {
            edit::apply_macro(&mut doc.notes, only, op, cx)
        })
    }

    /// Clamps a note after a timing drag (inside the open gesture).
    pub fn clamp_timing(&mut self, index: usize, edge: TimingEdge) {
        let duration = self.duration;
        clamp_timing(&mut self.doc.notes, index, edge, duration);
    }

    /// Refreshes lyrics after a pitch drag (inside the open gesture).
    pub fn relabel(&mut self) {
        relabel(&mut self.doc.notes, self.doc.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::CantoEngine;

    fn session_with_notes() -> VoxSession {
        let shared = Arc::new(Shared::new(CantoEngine::new(48_000.0)));
        let mut s = VoxSession::new(shared);
        let note = |id: &str, start: f32, detected: f32| CantoNote {
            id: id.into(),
            lyric: String::new(),
            unpitched: false,
            detected,
            pitch: detected,
            detected_start: start,
            start,
            detected_dur: 0.5,
            dur: 0.5,
            modulation: 1.0,
            formant: 0.0,
            gain_db: 0.0,
            detected_gain: 0.0,
            pinned: false,
            muted: false,
            orn_style: 0,
            orn_depth: 0.0,
            orn_speed: 0.0,
            accent: 0.0,
            scoop: 0.0,
            drift: 0.0,
            vib: 0.0,
            vib_cycles: 0.0,
            vib_phase: 0.0,
            contour: vec![0.0; crate::engine::CONTOUR_POINTS],
            envelope: vec![1.0; crate::engine::CONTOUR_POINTS],
        };
        s.reset(
            VoxDocument {
                notes: vec![note("a", 0.0, 60.3), note("b", 0.6, 64.4)],
                key: 0,
                scale: Scale::Major,
                ..VoxDocument::default()
            },
            2.0,
        );
        s
    }

    #[test]
    fn edits_reach_the_engine_and_undo_restores_them() {
        let mut s = session_with_notes();
        s.set_correct(1.0);
        assert_eq!(s.notes()[0].pitch, 60.0);
        assert_eq!(s.shared().lock().notes[0].pitch, 60.0);
        let second = s.cut(1, 0.85).unwrap();
        assert_eq!(s.notes().len(), 3);
        assert_eq!(second, 2);
        assert_eq!(s.shared().lock().notes.len(), 3);
        assert_eq!(s.undo_label(), Some("cut"));

        assert!(s.undo());
        assert_eq!(s.notes().len(), 2);
        assert_eq!(s.shared().lock().notes.len(), 2);
        assert!(s.undo());
        assert_eq!(s.shared().lock().notes[0].pitch, 60.3);
        assert!(!s.can_undo());
        assert!(s.redo() && s.redo());
        assert_eq!(s.notes().len(), 3);
    }

    #[test]
    fn a_gesture_is_one_step_and_a_no_op_leaves_none() {
        let mut s = session_with_notes();
        s.begin("tool:pitch");
        for k in 0..10 {
            s.note_mut(0).unwrap().pitch = 60.3 + k as f32 * 0.1;
            s.sync_note(0);
        }
        s.commit();
        assert_eq!(s.history().len(), 2);
        assert!((s.shared().lock().notes[0].pitch - 61.2).abs() < 1e-4);

        s.begin("tool:pitch");
        s.commit();
        assert_eq!(s.history().len(), 2);
    }

    #[test]
    fn jumping_through_history_lands_on_each_state() {
        let mut s = session_with_notes();
        s.toggle_mute(0);
        s.set_key(2);
        s.join_next(0);
        assert_eq!(s.notes().len(), 1);
        s.jump_to(1);
        assert_eq!(s.notes().len(), 2);
        assert!(s.notes()[0].muted);
        assert_eq!(s.doc().key, 0);
        let rows = s.history();
        assert_eq!(rows.len(), 4);
        assert!(rows[1].current);
        assert_eq!(rows[3].label, "join");
        s.jump_to(3);
        assert_eq!(s.notes().len(), 1);
        assert_eq!(s.doc().key, 2);
    }

    #[test]
    fn document_round_trips_through_json() {
        let s = session_with_notes();
        let json = s.doc().to_json();
        assert_eq!(VoxDocument::from_json(&json).as_ref(), Some(s.doc()));
    }
}
