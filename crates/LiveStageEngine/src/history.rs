//! Engine-level undo and redo, shared by every client.
//!
//! Snapshot based: before an undoable edit the engine keeps a copy of the
//! mix (and the session name), labelled for people ("Fader Kick"). Undoing
//! swaps the current mix for the copy; the engine then brings its audio
//! state in line the way a recall does (see `LiveEngine::reconcile`).
//! Continuous edits to one target coalesce: a fader drag is one step.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::session::{Id, Layer, MatrixFeed, MixState, StripRef};

/// Steps kept; the oldest goes first.
pub const UNDO_DEPTH: usize = 100;
/// Edits to the same target closer together than this are one step.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(600);

/// What the undo and redo buttons show.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    /// The label of the step an undo would take back.
    pub undo: Option<String>,
    /// The label of the step a redo would make again.
    pub redo: Option<String>,
    pub undo_depth: usize,
    pub redo_depth: usize,
}

/// What a step restores: the mix, the session name (the master's name) and
/// the custom layers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Snapshot {
    pub mix: MixState,
    pub name: String,
    pub layers: Vec<Layer>,
}

/// The thing a continuous edit moves. Two edits coalesce only when they
/// move the same one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EditTarget {
    Fader(StripRef),
    Pan(StripRef),
    Trim(Id),
    Send(Id, Id),
    /// A matrix and one of its sources.
    MatrixSend(Id, MatrixFeed),
    /// A strip and the processing section that changed ("EQ", "Comp" …).
    Processing(StripRef, &'static str),
    InsertParam(Id, u32),
    InsertParams(Id, Vec<u32>),
    DcaLevel(usize),
}

struct Step {
    label: String,
    snapshot: Snapshot,
}

#[derive(Default)]
pub(crate) struct UndoHistory {
    undo: VecDeque<Step>,
    redo: Vec<Step>,
    /// The step continuous edits still add to: its target and when it was
    /// last moved.
    open: Option<(EditTarget, Instant)>,
}

impl UndoHistory {
    /// Whether an edit of `target` at `now` belongs to the open step.
    pub fn coalesces(&self, target: Option<&EditTarget>, now: Instant) -> bool {
        match (target, &self.open) {
            (Some(target), Some((open, at))) => {
                open == target && now.saturating_duration_since(*at) <= COALESCE_WINDOW
            }
            _ => false,
        }
    }

    /// The open step took another edit at `now`.
    pub fn touch(&mut self, now: Instant) {
        if let Some((_, at)) = &mut self.open {
            *at = now;
        }
    }

    /// A new step: `snapshot` is the mix before the edit. Clears redo.
    pub fn push(
        &mut self,
        label: String,
        snapshot: Snapshot,
        target: Option<EditTarget>,
        now: Instant,
    ) {
        self.redo.clear();
        self.undo.push_back(Step { label, snapshot });
        while self.undo.len() > UNDO_DEPTH {
            self.undo.pop_front();
        }
        self.open = target.map(|target| (target, now));
    }

    /// The step to undo: its label and the mix to go back to. `current`
    /// becomes the redo step.
    pub fn undo(&mut self, current: Snapshot) -> Option<(String, Snapshot)> {
        self.open = None;
        let step = self.undo.pop_back()?;
        self.redo.push(Step {
            label: step.label.clone(),
            snapshot: current,
        });
        Some((step.label, step.snapshot))
    }

    /// The step to redo, as [`Self::undo`].
    pub fn redo(&mut self, current: Snapshot) -> Option<(String, Snapshot)> {
        self.open = None;
        let step = self.redo.pop()?;
        self.undo.push_back(Step {
            label: step.label.clone(),
            snapshot: current,
        });
        Some((step.label, step.snapshot))
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn summary(&self) -> History {
        History {
            undo: self.undo.back().map(|s| s.label.clone()),
            redo: self.redo.last().map(|s| s.label.clone()),
            undo_depth: self.undo.len(),
            redo_depth: self.redo.len(),
        }
    }
}
