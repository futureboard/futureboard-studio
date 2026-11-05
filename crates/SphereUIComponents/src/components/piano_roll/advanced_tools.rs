//! The MIDI editor's tool windows — Strum, Humanize, Length, Velocity and
//! Clean Up — each a small window of its own, opened from the editor
//! toolbar's Tools menu (`components::midi_tools_dialog`).
//!
//! A tool works as a live session on the notes it opened on (the selection,
//! or every shown note of the clip when nothing is selected):
//!
//! - opening takes a snapshot of those notes and previews the tool on them;
//! - every setting change recomputes the preview **from the snapshot**, so
//!   moving a knob back really goes back, and the engine hears the preview;
//! - **Accept** keeps the result as one undoable edit (prev = the snapshot);
//! - **Reset** puts the tool's settings back to their defaults;
//! - closing the window any other way puts the snapshot back, as if the tool
//!   had never run.
//!
//! Clicking or typing back in the editor keeps the result first, so an edit
//! made there never lands on top of a preview it would then be reverted with.
//! Clean Up deletes notes, so it previews a count rather than the deletion.
//!
//! The settings live on the piano roll, so a tool reopens as it was left.

use super::*;
use crate::components::timeline::timeline_state::{
    articulate_notes, find_double_notes, find_quiet_notes, find_short_notes, humanize_notes,
    shape_velocities, strum_chords_with, HumanizeSpec, LengthSpec, StrumSpec, VelocityShape,
};

/// The tools, in the Tools menu's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvancedTool {
    Strum,
    Humanize,
    Length,
    Velocity,
    CleanUp,
}

impl AdvancedTool {
    pub const ALL: [Self; 5] = [
        Self::Strum,
        Self::Humanize,
        Self::Length,
        Self::Velocity,
        Self::CleanUp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Strum => "Strum",
            Self::Humanize => "Humanize",
            Self::Length => "Length",
            Self::Velocity => "Velocity",
            Self::CleanUp => "Clean Up",
        }
    }

    /// The window's size: each tool is only as big as its controls.
    pub fn window_size(self) -> (f32, f32) {
        match self {
            Self::Strum => (360.0, 300.0),
            Self::Humanize => (360.0, 268.0),
            Self::Length => (360.0, 268.0),
            Self::Velocity => (360.0, 236.0),
            Self::CleanUp => (360.0, 300.0),
        }
    }

    /// Whether Accept removes notes (and so is styled as destructive).
    pub fn deletes(self) -> bool {
        self == Self::CleanUp
    }
}

/// Every tool's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdvancedToolSettings {
    pub strum: StrumSpec,
    pub humanize: HumanizeSpec,
    pub humanize_seed: u32,
    pub length: LengthSpec,
    pub length_seed: u32,
    pub velocity: VelocityShape,
    pub velocity_seed: u32,
    pub cleanup_doubles: bool,
    pub cleanup_short: bool,
    /// Notes shorter than this go.
    pub cleanup_short_res: GridRes,
    pub cleanup_quiet: bool,
    /// Notes softer than this go.
    pub cleanup_quiet_below: u8,
}

impl Default for AdvancedToolSettings {
    fn default() -> Self {
        Self {
            strum: StrumSpec {
                up: false,
                alternate: false,
                spread_ms: 20.0,
                tension: 0.0,
                keep_ends: true,
                velocity_fade: 0,
            },
            humanize: HumanizeSpec {
                timing_ms: 10.0,
                velocity: 8,
                length_pct: 0.0,
                keep_chords: true,
            },
            humanize_seed: 1,
            length: LengthSpec {
                multiply: 1.0,
                legato: false,
                gap_ms: 0.0,
                variation_pct: 0.0,
            },
            length_seed: 1,
            velocity: VelocityShape {
                offset: 0,
                compress: 0.0,
                random: 0,
            },
            velocity_seed: 1,
            cleanup_doubles: true,
            cleanup_short: false,
            cleanup_short_res: GridRes::SixtyFourth,
            cleanup_quiet: false,
            cleanup_quiet_below: 10,
        }
    }
}

impl AdvancedToolSettings {
    /// Put one tool's settings back to their defaults.
    pub fn reset(&mut self, tool: AdvancedTool) {
        let d = Self::default();
        match tool {
            AdvancedTool::Strum => self.strum = d.strum,
            AdvancedTool::Humanize => {
                self.humanize = d.humanize;
                self.humanize_seed = d.humanize_seed;
            }
            AdvancedTool::Length => {
                self.length = d.length;
                self.length_seed = d.length_seed;
            }
            AdvancedTool::Velocity => {
                self.velocity = d.velocity;
                self.velocity_seed = d.velocity_seed;
            }
            AdvancedTool::CleanUp => {
                self.cleanup_doubles = d.cleanup_doubles;
                self.cleanup_short = d.cleanup_short;
                self.cleanup_short_res = d.cleanup_short_res;
                self.cleanup_quiet = d.cleanup_quiet;
                self.cleanup_quiet_below = d.cleanup_quiet_below;
            }
        }
    }

    /// `false` when Clean Up has nothing ticked.
    pub fn cleanup_selected(&self) -> bool {
        self.cleanup_doubles || self.cleanup_short || self.cleanup_quiet
    }
}

/// Short-note thresholds Clean Up offers.
pub const CLEANUP_SHORT_CHOICES: [(GridRes, &str); 4] = [
    (GridRes::SixtyFourth, "1/64"),
    (GridRes::ThirtySecond, "1/32"),
    (GridRes::Sixteenth, "1/16"),
    (GridRes::Eighth, "1/8"),
];

/// An open tool window and the notes it is previewing on.
pub(super) struct ToolSession {
    tool: AdvancedTool,
    clip_id: String,
    /// The target notes as they were when the tool opened.
    original: Vec<MidiNoteState>,
    /// The clip's length when the tool opened. A preview may grow the clip
    /// to fit; cancelling puts this back, and Accept's undo step carries it.
    original_clip_beats: Option<f32>,
    window: Option<gpui::WindowHandle<crate::components::midi_tools_dialog::MidiToolWindow>>,
    /// Clean Up: how many notes Accept would delete.
    doomed: usize,
}

impl PianoRoll {
    pub fn advanced_settings(&self) -> &AdvancedToolSettings {
        &self.advanced
    }

    /// The tool whose window is open, if any.
    pub fn active_tool(&self) -> Option<AdvancedTool> {
        self.tool_session.as_ref().map(|s| s.tool)
    }

    /// What the open tool is working on, for its window: `(notes, label)`.
    pub fn tool_session_summary(&self) -> (usize, String) {
        let Some(session) = &self.tool_session else {
            return (0, "No notes".to_string());
        };
        let count = session.original.len();
        if session.tool == AdvancedTool::CleanUp {
            return (
                session.doomed,
                match session.doomed {
                    0 => format!("Nothing to delete in {} notes", group_thousands(count)),
                    1 => "1 note will be deleted".to_string(),
                    n => format!("{} notes will be deleted", group_thousands(n)),
                },
            );
        }
        let label = match count {
            0 => "No notes to work on".to_string(),
            1 => "1 note".to_string(),
            n => format!("{} notes", group_thousands(n)),
        };
        (count, label)
    }

    /// Change the settings; the open tool re-previews from its snapshot.
    pub fn update_advanced_settings(
        &mut self,
        change: impl FnOnce(&mut AdvancedToolSettings),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.advanced);
        self.preview_tool(cx);
        cx.notify();
    }

    /// Reset the open tool's settings to their defaults and re-preview.
    pub fn reset_active_tool(&mut self, cx: &mut Context<Self>) {
        if let Some(tool) = self.active_tool() {
            self.update_advanced_settings(|s| s.reset(tool), cx);
        }
    }

    /// The edited clip's id, from any context that can read entities.
    pub(super) fn editing_clip_id_in(&self, cx: &gpui::App) -> Option<String> {
        let tl = self.timeline.read(cx);
        let cid = tl.state.selection.selected_clip_ids.first()?.clone();
        tl.state.midi_clip_notes(&cid).map(|_| cid)
    }

    /// `ms` at a clip-local beat, in beats, through the project tempo map.
    fn ms_to_beats_fn(&self, cx: &gpui::App) -> impl Fn(f32, f32) -> f32 + 'static {
        let origin = self.edit_origin() as f64;
        let (tempo, bpm) = {
            let tl = self.timeline.read(cx);
            (tl.state.tempo_map.clone(), tl.state.bpm as f64)
        };
        move |at: f32, ms: f32| {
            let beat = origin + at as f64;
            let seconds = tempo.seconds_at_beat(beat, bpm) + ms as f64 / 1000.0;
            (tempo.beat_at_seconds(seconds.max(0.0), bpm) - beat) as f32
        }
    }

    /// Open `tool` on the current target notes. A tool already open keeps its
    /// result first, the way clicking back in the editor does.
    pub(super) fn open_tool_window(
        &mut self,
        tool: AdvancedTool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.accept_active_tool(cx);
        if !self.begin_tool_session(tool, cx) {
            return;
        }
        let owner = Some(window.bounds());
        let roll = cx.entity();
        // Deferred: GPUI draws a new window once before `open_window`
        // returns, and the tool window's render reads this editor, which is
        // leased for the length of this update. Opening it here panics.
        cx.defer(move |cx| {
            let opened = crate::components::midi_tools_dialog::open_midi_tool_window(
                owner,
                roll.clone(),
                tool,
                cx,
            );
            roll.update(cx, |roll, cx| roll.attach_tool_window(tool, opened, cx));
        });
        cx.notify();
    }

    /// Hand the session the window [`Self::open_tool_window`] opened. A
    /// session that ended (or changed tool) before the window came up closes
    /// it instead; a window that did not open ends the session.
    fn attach_tool_window(
        &mut self,
        tool: AdvancedTool,
        opened: Result<
            gpui::WindowHandle<crate::components::midi_tools_dialog::MidiToolWindow>,
            String,
        >,
        cx: &mut Context<Self>,
    ) {
        let session = self
            .tool_session
            .as_mut()
            .filter(|session| session.tool == tool && session.window.is_none());
        match (opened, session) {
            (Ok(handle), Some(session)) => session.window = Some(handle),
            (Ok(handle), None) => close_tool_window(Some(handle), cx),
            (Err(error), session) => {
                eprintln!(
                    "[MIDI Editor] {} window did not open: {error}",
                    tool.label()
                );
                if session.is_some() {
                    self.cancel_active_tool(cx);
                }
            }
        }
        cx.notify();
    }

    /// Preview scenes: start `tool` on the target notes without a window.
    #[cfg(feature = "ui-preview")]
    pub fn preview_begin_tool(&mut self, tool: AdvancedTool, cx: &mut Context<Self>) {
        self.begin_tool_session(tool, cx);
    }

    /// Snapshot the target notes and preview `tool` on them. `false` when
    /// no MIDI clip is open.
    fn begin_tool_session(&mut self, tool: AdvancedTool, cx: &mut Context<Self>) -> bool {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return false;
        };
        let ids: HashSet<u64> = self.tool_target_ids(cx).into_iter().collect();
        let original: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| ids.contains(&n.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let original_clip_beats = self.timeline.read(cx).state.clip_duration_beats(&clip_id);
        self.tool_session = Some(ToolSession {
            tool,
            clip_id,
            original,
            original_clip_beats,
            window: None,
            doomed: 0,
        });
        self.preview_tool(cx);
        true
    }

    /// Recompute the open tool's preview from its snapshot and show it (and
    /// let the engine hear it). No undo entry: Accept records one.
    fn preview_tool(&mut self, cx: &mut Context<Self>) {
        let settings = self.advanced;
        let to_beats = self.ms_to_beats_fn(cx);
        let Some(session) = self.tool_session.as_mut() else {
            return;
        };
        let ids: HashSet<u64> = session.original.iter().map(|n| n.id).collect();
        if session.tool == AdvancedTool::CleanUp {
            session.doomed = if settings.cleanup_selected() {
                cleanup_ids(&session.original, &ids, &settings).len()
            } else {
                0
            };
            return;
        }
        let mut next = session.original.clone();
        match session.tool {
            AdvancedTool::Strum => {
                strum_chords_with(&mut next, &ids, settings.strum, &to_beats);
            }
            AdvancedTool::Humanize => {
                humanize_notes(
                    &mut next,
                    &ids,
                    settings.humanize,
                    seed_bits(settings.humanize_seed),
                    &to_beats,
                );
            }
            AdvancedTool::Length => {
                articulate_notes(
                    &mut next,
                    &ids,
                    settings.length,
                    seed_bits(settings.length_seed),
                    &to_beats,
                );
            }
            AdvancedTool::Velocity => {
                shape_velocities(
                    &mut next,
                    &ids,
                    settings.velocity,
                    seed_bits(settings.velocity_seed),
                );
            }
            AdvancedTool::CleanUp => {}
        }
        let clip_id = session.clip_id.clone();
        let base = session.original_clip_beats;
        self.write_notes_live(&clip_id, &next, base, cx);
    }

    /// Write note fields for a preview: drawn now and heard by the engine,
    /// but not an undo step and not an unsaved change. The clip starts from
    /// its length when the tool opened and grows only as far as this preview
    /// needs, so turning a knob back also gives the length back.
    fn write_notes_live(
        &mut self,
        clip_id: &str,
        notes: &[MidiNoteState],
        base_clip_beats: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        self.timeline.update(cx, |tl, tcx| {
            if let Some(beats) = base_clip_beats {
                tl.state.set_clip_length_beats(clip_id, beats);
            }
            tl.state.overwrite_midi_notes(clip_id, notes);
            tl.mark_midi_preview_changed(tcx);
            tcx.notify();
        });
    }

    /// Drop a tool session whose clip is gone (the project was closed or
    /// replaced under it): nothing to put back, and nothing to keep.
    pub(super) fn drop_stale_tool_session(&mut self, cx: &mut Context<Self>) {
        let stale = self.tool_session.as_ref().is_some_and(|session| {
            self.timeline
                .read(cx)
                .state
                .midi_clip_notes(&session.clip_id)
                .is_none()
        });
        if stale {
            if let Some(session) = self.tool_session.take() {
                close_tool_window(session.window, cx);
            }
        }
    }

    /// Keep the open tool's result as one undo step and close its window.
    pub fn accept_active_tool(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.tool_session.take() else {
            return;
        };
        close_tool_window(session.window, cx);
        if session.tool == AdvancedTool::CleanUp {
            self.delete_cleanup_notes(&session, cx);
            cx.notify();
            return;
        }
        let ids: Vec<u64> = session.original.iter().map(|n| n.id).collect();
        let next = self.snapshot_notes(cx, &session.clip_id, &ids);
        let next_len = self
            .timeline
            .read(cx)
            .state
            .clip_duration_beats(&session.clip_id);
        // Notes deleted elsewhere while the tool was open drop out of both
        // sides, so the undo step only names notes that still exist.
        let present: HashSet<u64> = next.iter().map(|n| n.id).collect();
        let prev: Vec<MidiNoteState> = session
            .original
            .into_iter()
            .filter(|n| present.contains(&n.id))
            .collect();
        // The preview never marked the project unsaved; recording the step
        // does, exactly once, and names it for Edit > Undo.
        self.record_note_transform(
            session.tool.label(),
            session.clip_id,
            prev,
            next,
            session.original_clip_beats,
            next_len,
            cx,
        );
        cx.notify();
    }

    /// Put the notes (and the clip's length) back as they were when the
    /// tool opened, and close it. Leaves no undo step and no unsaved change.
    /// Returns whether a tool was open.
    pub fn cancel_active_tool(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.tool_session.take() else {
            return false;
        };
        close_tool_window(session.window, cx);
        if session.tool != AdvancedTool::CleanUp {
            self.write_notes_live(
                &session.clip_id,
                &session.original,
                session.original_clip_beats,
                cx,
            );
        }
        cx.notify();
        true
    }

    fn delete_cleanup_notes(&mut self, session: &ToolSession, cx: &mut Context<Self>) {
        let settings = self.advanced;
        if !settings.cleanup_selected() {
            return;
        }
        let ids: HashSet<u64> = session.original.iter().map(|n| n.id).collect();
        let doomed_ids: HashSet<u64> = cleanup_ids(&session.original, &ids, &settings)
            .into_iter()
            .collect();
        let doomed: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&session.clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| doomed_ids.contains(&n.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if doomed.is_empty() {
            return;
        }
        for note in &doomed {
            self.selection.remove(&note.id);
        }
        self.cleanup_midi_before_destructive_edit("tool_cleanup", cx);
        self.run_edit_command(
            EditCommand::DeleteMidiNotes {
                clip_id: session.clip_id.clone(),
                notes: doomed,
            },
            cx,
        );
    }
}

/// The ids Clean Up would delete from `notes` with `settings`.
fn cleanup_ids(
    notes: &[MidiNoteState],
    ids: &HashSet<u64>,
    settings: &AdvancedToolSettings,
) -> Vec<u64> {
    let mut out: HashSet<u64> = HashSet::new();
    if settings.cleanup_doubles {
        out.extend(find_double_notes(notes, ids));
    }
    if settings.cleanup_short {
        let min = settings.cleanup_short_res.beats().max(MIN_NOTE_BEATS);
        out.extend(find_short_notes(notes, ids, min));
    }
    if settings.cleanup_quiet {
        out.extend(find_quiet_notes(notes, ids, settings.cleanup_quiet_below));
    }
    out.into_iter().collect()
}

/// Spread a small user-facing seed over all 64 bits.
fn seed_bits(seed: u32) -> u64 {
    (seed as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xD1B5_4A32_D192_ED03)
}

fn close_tool_window(
    window: Option<gpui::WindowHandle<crate::components::midi_tools_dialog::MidiToolWindow>>,
    cx: &mut gpui::App,
) {
    if let Some(window) = window {
        // Deferred: this runs inside the tool window's own update when its
        // buttons call it, and a window cannot remove itself mid-update.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_touches_only_its_own_tool() {
        let mut s = AdvancedToolSettings::default();
        s.strum.spread_ms = 80.0;
        s.velocity.offset = 30;
        s.reset(AdvancedTool::Strum);
        assert_eq!(s.strum, AdvancedToolSettings::default().strum);
        assert_eq!(s.velocity.offset, 30);
    }

    #[test]
    fn clean_up_needs_something_ticked() {
        let mut s = AdvancedToolSettings::default();
        s.cleanup_doubles = false;
        assert!(!s.cleanup_selected());
        s.cleanup_quiet = true;
        assert!(s.cleanup_selected());
        assert!(AdvancedTool::CleanUp.deletes());
        assert!(!AdvancedTool::Strum.deletes());
    }

    #[test]
    fn neighbouring_seeds_spread_apart() {
        assert_ne!(seed_bits(1), seed_bits(2));
        assert_ne!(seed_bits(0), 0);
    }
}
