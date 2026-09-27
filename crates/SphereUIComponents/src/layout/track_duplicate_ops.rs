//! Duplicate Track (with or without its effects), and Copy / Paste FX Chain.
//!
//! The model builds the copies (`timeline_state::track_clone`); this is the
//! studio's half: capturing the plug-ins' state as they have it now, tearing
//! down what a paste replaces, loading the new instances and syncing the
//! engine. Each action is one undo step; `history_ops` unloads and reloads
//! the instances a step takes away or brings back.

use gpui::Context;

use crate::components::context_menu::ContextMenuEntry;

use super::plugin_ops::FxChainClipboard;
use super::StudioLayout;

impl StudioLayout {
    /// Duplicate the context (or selected) track right below it, with its
    /// clips, settings and plug-ins — or, without `with_fx`, with only its
    /// instrument. The clone is selected.
    pub(super) fn duplicate_context_track(&mut self, with_fx: bool, cx: &mut Context<Self>) {
        let Some(source_id) = self.context_track_id_or_selected(cx) else {
            return;
        };
        let insert_ids: Vec<String> = {
            let state = &self.timeline.read(cx).state;
            let Some(track) = state.find_track(&source_id).filter(|t| t.can_be_cloned()) else {
                return;
            };
            track.inserts[track.clone_insert_range(with_fx)]
                .iter()
                .map(|slot| slot.id.clone())
                .collect()
        };
        let states = self.capture_insert_states(&source_id, &insert_ids, cx);
        let clone = self.timeline.update(cx, |timeline, cx| {
            let clone = timeline.state.clone_track(&source_id, with_fx, &states)?;
            timeline.state.select_track(&clone.track_id);
            cx.notify();
            Some(clone)
        });
        let Some(clone) = clone else {
            return;
        };
        eprintln!(
            "[TrackDuplicate] track={source_id} -> track={} with_fx={with_fx} plugins={} \
             states={}",
            clone.track_id,
            clone.inserts.len(),
            states.len()
        );
        self.record_duplicated_track(&clone.track_id, cx);
        let copies: Vec<String> = clone.inserts.iter().map(|(_, copy)| copy.clone()).collect();
        self.load_copied_inserts(&clone.track_id, &copies, cx);
        self.after_channel_chain_edit(cx, "duplicate_track");
    }

    /// Take the context (or selected) track's effects, with their state as
    /// the plug-ins have it now, for a later Paste FX Chain.
    pub(super) fn copy_context_fx_chain(&mut self, cx: &mut Context<Self>) {
        let Some(track_id) = self.context_track_id_or_selected(cx) else {
            return;
        };
        let (source_name, mut effects) = {
            let state = &self.timeline.read(cx).state;
            let Some(track) = state.find_track(&track_id) else {
                return;
            };
            (track.name.clone(), state.fx_chain_effects(&track_id))
        };
        if effects.is_empty() {
            return;
        }
        let ids: Vec<String> = effects.iter().map(|slot| slot.id.clone()).collect();
        let mut states = self.capture_insert_states(&track_id, &ids, cx);
        for effect in &mut effects {
            if let Some(state) = states.remove(&effect.id) {
                effect.vst3_state = Some(state);
            }
        }
        eprintln!(
            "[FxChain] copied track={track_id} effects={}",
            effects.len()
        );
        self.plugin_editors.fx_chain_clipboard = Some(FxChainClipboard {
            source_name,
            effects,
        });
        cx.notify();
    }

    /// Replace the context (or selected) track's effects with new instances
    /// of the copied chain. Its instrument stays.
    pub(super) fn paste_context_fx_chain(&mut self, cx: &mut Context<Self>) {
        let Some(track_id) = self.context_track_id_or_selected(cx) else {
            return;
        };
        let Some(copied) = self.plugin_editors.fx_chain_clipboard.clone() else {
            return;
        };
        let replaced = {
            let state = &self.timeline.read(cx).state;
            if !state.can_take_fx_chain(&track_id, copied.effects.len()) {
                eprintln!(
                    "[FxChain] refused paste on track={track_id}: {} effects do not fit",
                    copied.effects.len()
                );
                return;
            }
            state.fx_chain_ids(&track_id)
        };
        // The undo step keeps the replaced effects as they sound now.
        self.store_live_plugin_states(&track_id, &replaced, cx);
        let before = self.capture_insert_chains(&[track_id.as_str()], cx);
        // Released before the model forgets them, as a removal would.
        for insert_id in &replaced {
            self.teardown_insert_instance(&track_id, insert_id, cx, "paste_fx_chain");
        }
        let copies = self.timeline.update(cx, |timeline, cx| {
            let copies = timeline.state.replace_fx_chain(&track_id, &copied.effects);
            cx.notify();
            copies
        });
        let Some(copies) = copies else {
            return;
        };
        eprintln!(
            "[FxChain] pasted from '{}' onto track={track_id} replaced={} effects={}",
            copied.source_name,
            replaced.len(),
            copies.len()
        );
        self.record_insert_chains("Paste FX Chain", before, cx);
        self.load_copied_inserts(&track_id, &copies, cx);
        self.after_channel_chain_edit(cx, "paste_fx_chain");
    }

    /// The Duplicate / FX Chain entries of a track's context menu, shared by
    /// the arrangement header and the mixer strip. Each is offered disabled
    /// where it would do nothing on this track.
    pub(super) fn track_copy_menu_entries(
        &self,
        track_id: &str,
        cx: &Context<Self>,
    ) -> Vec<ContextMenuEntry> {
        let state = &self.timeline.read(cx).state;
        let clonable = state
            .find_track(track_id)
            .is_some_and(|track| track.can_be_cloned());
        let has_effects = !state.fx_chain_effects(track_id).is_empty();
        let paste = self.plugin_editors.fx_chain_clipboard.as_ref();
        let paste_label = paste
            .map(|copied| format!("Paste FX Chain from \"{}\"", copied.source_name))
            .unwrap_or_else(|| "Paste FX Chain".to_string());
        let can_paste =
            paste.is_some_and(|copied| state.can_take_fx_chain(track_id, copied.effects.len()));
        let entry = |label: String, command: &str, enabled: bool| {
            if enabled {
                ContextMenuEntry::item(label, command)
            } else {
                ContextMenuEntry::disabled_item(label, command)
            }
        };
        vec![
            entry("Duplicate Track".into(), "track:duplicate", clonable),
            entry(
                "Duplicate Track without FX".into(),
                "track:duplicate-no-fx",
                clonable,
            ),
            entry("Copy FX Chain".into(), "track:copy-fx-chain", has_effects),
            entry(paste_label, "track:paste-fx-chain", can_paste),
        ]
    }
}
