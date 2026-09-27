//! Undo / redo as the studio runs it, and recording the plug-in actions the
//! timeline cannot see.
//!
//! The timeline owns the history and the project model; a step only changes
//! the model. Plug-in instances live outside it — in the plug-in host and the
//! engine — so a step that takes a plug-in out of the project or puts one back
//! needs the studio around it: the leaving instances' live state is written
//! into the command first (so the opposite step restores what the plug-in
//! had, not what it had when the action was recorded), they are unloaded, the
//! step runs, and the arriving ones are loaded with the state the command
//! carries. A step that only changes a plug-in's state hands it that state.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::Context;

use crate::components::edit::{EditCommand, TrackSnapshot};
use crate::components::timeline::timeline_state::{
    InsertChainSnapshot, PendingTrackEdit, TrackEditScope,
};

use super::StudioLayout;

impl StudioLayout {
    /// Edit > Undo / Redo: the ARA plug-in's edits while its editor is the
    /// page on screen, the project's otherwise.
    ///
    /// Never both. An ARA editor's edits live in the plug-in's document, which
    /// the project history cannot see; stepping the project there instead
    /// undid some unrelated arrangement change behind the plug-in's back. The
    /// ARA history is the host's (see `ara_ops::AraHistory`): Melodyne hands
    /// Ctrl+Z to the host in ARA mode rather than undoing anything itself.
    pub(super) fn undo_or_redo(&mut self, undoing: bool, cx: &mut Context<Self>) {
        let ara_page = self.ara_page_on_screen(cx);
        if crate::components::transport_key::key_debug() {
            eprintln!(
                "[Keyboard] studio {} ara_page={ara_page} ara_view_attached={}",
                if undoing { "edit:undo" } else { "edit:redo" },
                self.ara_editor.read(cx).is_attached()
            );
        }
        if !ara_page {
            self.step_edit_history(undoing, cx);
            return;
        }
        let Some(key) = self.ara_panel_target(cx) else {
            return;
        };
        let plugin = self
            .ara_plugin_name(&key.plugin_id)
            .unwrap_or_else(|| key.plugin_id.clone());
        match self
            .ara
            .step_history(&key, undoing, std::time::Instant::now())
        {
            Ok(true) => {
                if crate::components::transport_key::key_debug() {
                    eprintln!(
                        "[Keyboard] ara {} restored in {plugin}",
                        if undoing { "undo" } else { "redo" }
                    );
                }
                // The plug-in's document is part of the project.
                self.mark_dirty();
                cx.notify();
            }
            Ok(false) => {
                let message = if undoing {
                    format!("Nothing to undo in {plugin}")
                } else {
                    format!("Nothing to redo in {plugin}")
                };
                self.show_ara_notice(message, cx);
            }
            Err(error) => self.show_ara_notice(error, cx),
        }
    }

    /// Whether the docked Editor is showing an ARA plug-in's page.
    ///
    /// The same test that keeps the embedded view on screen
    /// (`sync_ara_editor_visibility`), so "on the ARA page" means exactly
    /// what the user sees.
    fn ara_page_on_screen(&self, cx: &gpui::App) -> bool {
        self.panels.bottom_docked
            && matches!(
                self.active_bottom_tab(),
                crate::components::BottomTab::Editor
            )
            && self.clip_editor_panel.read(cx).ara_tab_active()
            && self.ara_panel_target(cx).is_some()
    }

    /// Run one undo (`undoing`) or redo step. Returns whether there was one.
    pub(super) fn step_edit_history(&mut self, undoing: bool, cx: &mut Context<Self>) -> bool {
        let (leaving, arriving) = match self.timeline.read(cx).next_history_step(undoing) {
            Some(step) => (
                step.instances_leaving(undoing),
                step.instances_arriving(undoing),
            ),
            None => return false,
        };

        // Keep what the leaving plug-ins have now, then let them go while their
        // slots still say what they were (an instrument gets its MIDI panic).
        if !leaving.is_empty() {
            let mut states: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
            for (track_id, insert_ids) in by_track(&leaving) {
                states.extend(self.capture_insert_states(&track_id, &insert_ids, cx));
            }
            if !states.is_empty() {
                self.timeline.update(cx, |timeline, _cx| {
                    if let Some(step) = timeline.next_history_step_mut(undoing) {
                        step.refresh_plugin_states(&states);
                    }
                });
            }
            for (track_id, insert_id) in &leaving {
                self.teardown_insert_instance(track_id, insert_id, cx, "history_step");
            }
        }

        let stepped = self.timeline.update(cx, |timeline, cx| {
            if !(if undoing {
                timeline.undo_edit(cx)
            } else {
                timeline.redo_edit(cx)
            }) {
                return None;
            }
            let step = if undoing {
                timeline.last_undone_edit()
            } else {
                timeline.last_redone_edit()
            }?;
            Some((
                step.is_channel_chain_edit(),
                step.plugin_state_target()
                    .map(|(track, insert)| (track.to_string(), insert.to_string())),
                step.touches_routing(),
            ))
        });
        let Some((chain_edit, state_target, routing)) = stepped else {
            return false;
        };
        if routing {
            self.publish_audio_connection_routing(cx);
        }

        // The arriving plug-ins are in the model now, each slot carrying the
        // state the command kept for it.
        for (track_id, insert_ids) in by_track(&arriving) {
            self.load_copied_inserts(&track_id, &insert_ids, cx);
        }
        if chain_edit || !leaving.is_empty() || !arriving.is_empty() {
            let reason = if undoing {
                "undo_channel_chain"
            } else {
                "redo_channel_chain"
            };
            self.after_channel_chain_edit(cx, reason);
        }
        if let Some((track_id, insert_id)) = state_target {
            self.push_insert_state_to_plugin(&track_id, &insert_id, cx);
        }
        true
    }

    /// Hand an insert's stored state (`vst3_state`) to its live plug-in, after
    /// a step changed it. The preset strip then names no preset: what is
    /// loaded is whatever the step restored.
    fn push_insert_state_to_plugin(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some((plugin_id, state)) = self
            .timeline
            .read(cx)
            .state
            .find_insert_slot(track_id, insert_id)
            .and_then(|slot| Some((slot.plugin_id.clone()?, slot.vst3_state.clone()?)))
        else {
            return;
        };
        if SpherePluginHost::builtin_audio_bridge_supported(&plugin_id) {
            crate::components::builtin_plugin_editor::builtin_state_seed(
                &plugin_id, insert_id, &state,
            );
            self.replay_builtin_insert_state(insert_id, cx);
            self.refresh_builtin_editor_sidebars(cx);
        } else if let Some(runtime) = self.plugin_editors.bridge_runtime.clone() {
            if let Ok(mut runtime) = runtime.lock() {
                if let Err(error) = runtime.send_plugin_state(insert_id, &state) {
                    eprintln!("[history] SetPluginState failed insert={insert_id}: {error}");
                }
            }
        }
        self.plugin_editors
            .preset_selection
            .remove(&(track_id.to_string(), insert_id.to_string()));
        self.mark_dirty_view_only();
        cx.notify();
    }

    /// Write each of `insert_ids`' live plug-in state into its slot on
    /// `track_id`, so a snapshot taken next keeps what the plug-in has now
    /// rather than what was last saved.
    pub(super) fn store_live_plugin_states(
        &mut self,
        track_id: &str,
        insert_ids: &[String],
        cx: &mut Context<Self>,
    ) {
        if insert_ids.is_empty() {
            return;
        }
        let states = self.capture_insert_states(track_id, insert_ids, cx);
        if states.is_empty() {
            return;
        }
        self.timeline.update(cx, |timeline, _cx| {
            if let Some(slots) = timeline.state.insert_slots_mut(track_id) {
                for slot in slots {
                    if let Some(state) = states.get(&slot.id) {
                        slot.vst3_state = Some(state.clone());
                    }
                }
            }
        });
    }

    /// The chains of `track_ids` as they stand, for [`Self::record_insert_chains`].
    pub(super) fn capture_insert_chains(
        &self,
        track_ids: &[&str],
        cx: &Context<Self>,
    ) -> Vec<InsertChainSnapshot> {
        let state = &self.timeline.read(cx).state;
        track_ids
            .iter()
            .filter_map(|track_id| state.capture_insert_chain(track_id))
            .collect()
    }

    /// Record an insert-chain action that has already happened as one undo
    /// step: `prev` as captured before it, the same channels as they are
    /// now after it. Nothing is recorded when nothing changed.
    pub(super) fn record_insert_chains(
        &mut self,
        label: &'static str,
        prev: Vec<InsertChainSnapshot>,
        cx: &mut Context<Self>,
    ) {
        let track_ids: Vec<String> = prev.iter().map(|chain| chain.track_id.clone()).collect();
        let ids: Vec<&str> = track_ids.iter().map(String::as_str).collect();
        let next = self.capture_insert_chains(&ids, cx);
        let changed = prev
            .iter()
            .zip(&next)
            .any(|(before, after)| before.inserts != after.inserts)
            || prev.len() != next.len();
        if !changed {
            return;
        }
        self.timeline.update(cx, |timeline, cx| {
            timeline
                .record_executed_command(EditCommand::SetInsertChains { label, prev, next }, cx);
        });
    }

    /// Record a track Duplicate Track has just added.
    pub(super) fn record_duplicated_track(&mut self, track_id: &str, cx: &mut Context<Self>) {
        self.timeline.update(cx, |timeline, cx| {
            let Some(snapshot) = TrackSnapshot::capture(&timeline.state, track_id) else {
                return;
            };
            let height = timeline.state.track_view_layout.height_for(track_id);
            timeline.record_executed_command(EditCommand::DuplicateTrack { snapshot, height }, cx);
        });
    }

    /// Record a plug-in state change that has already been handed to the
    /// plug-in: `prev` is what it had before.
    pub(super) fn record_insert_state(
        &mut self,
        label: &'static str,
        track_id: &str,
        insert_id: &str,
        prev: Option<Arc<Vec<u8>>>,
        cx: &mut Context<Self>,
    ) {
        self.timeline.update(cx, |timeline, cx| {
            let next = timeline
                .state
                .find_insert_slot(track_id, insert_id)
                .and_then(|slot| slot.vst3_state.clone());
            if next == prev {
                return;
            }
            timeline.record_insert_state_command(
                EditCommand::SetInsertState {
                    label,
                    track_id: track_id.to_string(),
                    insert_id: insert_id.to_string(),
                    prev,
                    next,
                },
                cx,
            );
        });
    }

    /// Open a track edit (see `Timeline::begin_track_edit`) for an action
    /// about to change what `scope` covers.
    pub(super) fn begin_track_edit(
        &self,
        scope: TrackEditScope,
        cx: &gpui::App,
    ) -> PendingTrackEdit {
        self.timeline.read(cx).begin_track_edit(scope)
    }

    /// Record the action since [`Self::begin_track_edit`] as one undo step.
    pub(super) fn commit_track_edit(
        &mut self,
        label: &'static str,
        pending: PendingTrackEdit,
        cx: &mut Context<Self>,
    ) {
        self.timeline.update(cx, |timeline, cx| {
            timeline.commit_track_edit(label, pending, false, cx);
        });
    }

    /// [`Self::commit_track_edit`] for one sample of a continuous gesture
    /// (a colour drag, a slider): folds into the previous step when that was
    /// the same gesture on the same tracks.
    pub(super) fn commit_track_edit_folded(
        &mut self,
        label: &'static str,
        pending: PendingTrackEdit,
        cx: &mut Context<Self>,
    ) {
        self.timeline.update(cx, |timeline, cx| {
            timeline.commit_track_edit(label, pending, true, cx);
        });
    }

    /// The selected tracks and the primary, for an action that applies to the
    /// selection.
    pub(super) fn selected_track_ids(&self, cx: &gpui::App) -> Vec<String> {
        let selection = &self.timeline.read(cx).state.selection;
        let mut ids = selection.selected_track_ids.clone();
        if let Some(primary) = selection.selected_track_id.as_ref() {
            if !ids.contains(primary) {
                ids.push(primary.clone());
            }
        }
        ids
    }

    /// A channel and the VSTi output channels of its inserts: the scope of an
    /// action that can add or remove those output channels.
    pub(super) fn channel_with_outputs(&self, track_id: &str, cx: &gpui::App) -> TrackEditScope {
        use crate::components::timeline::timeline_state::vsti_output_child_insert_id;
        let state = &self.timeline.read(cx).state;
        let mut scope = TrackEditScope::channel(track_id);
        if let Some(slots) = state.insert_slots(track_id) {
            for track in &state.tracks {
                if vsti_output_child_insert_id(&track.id)
                    .is_some_and(|owner| slots.iter().any(|slot| slot.id == owner))
                {
                    scope.tracks.push(track.id.clone());
                }
            }
        }
        scope
    }

    /// Every track id, for an action that can reach any track.
    pub(super) fn all_track_ids(&self, cx: &gpui::App) -> Vec<String> {
        self.timeline
            .read(cx)
            .state
            .tracks
            .iter()
            .map(|track| track.id.clone())
            .collect()
    }

    /// Undo and Redo as the Edit menu shows them: available only when there
    /// is a step to take, and naming it.
    pub(super) fn history_menu_states(
        &self,
        cx: &gpui::App,
    ) -> Vec<crate::menu::CommandMenuState<'static>> {
        if self.ara_page_on_screen(cx) {
            if let Some(key) = self.ara_panel_target(cx) {
                let (undo, redo) = self.ara.history_depth(&key);
                let plugin = self
                    .ara_plugin_name(&key.plugin_id)
                    .unwrap_or_else(|| key.plugin_id.clone());
                return vec![
                    crate::menu::CommandMenuState {
                        command: "edit:undo",
                        enabled: undo > 0,
                        detail: Some(format!("{plugin} Edit")),
                    },
                    crate::menu::CommandMenuState {
                        command: "edit:redo",
                        enabled: redo > 0,
                        detail: Some(format!("{plugin} Edit")),
                    },
                ];
            }
        }
        let (undo, redo) = self.timeline.read(cx).history_labels();
        vec![
            crate::menu::CommandMenuState {
                command: "edit:undo",
                enabled: undo.is_some(),
                detail: undo.map(str::to_string),
            },
            crate::menu::CommandMenuState {
                command: "edit:redo",
                enabled: redo.is_some(),
                detail: redo.map(str::to_string),
            },
        ]
    }
}

/// `(track, insert)` pairs grouped by track, in first-seen order.
fn by_track(pairs: &[(String, String)]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for (track_id, insert_id) in pairs {
        match groups.iter_mut().find(|(track, _)| track == track_id) {
            Some((_, ids)) => ids.push(insert_id.clone()),
            None => groups.push((track_id.clone(), vec![insert_id.clone()])),
        }
    }
    groups
}
