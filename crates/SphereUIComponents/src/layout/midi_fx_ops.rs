//! StudioLayout integration for MIDI FX: the Inspector's MIDI FX section and
//! the device editor windows.
//!
//! Adding or removing a device is structure: it is recorded as one undo step
//! and the engine graph is synced (the running chain is handed over, so the
//! other devices keep playing). Bypass and parameter changes go to the
//! running chain live (`AudioEngine::set_midi_fx_*`), with no rebuild; they
//! fall back to a sync only when no chain is running yet.

use std::sync::Arc;

use gpui::{App, Context, Window};
use sphere_midi_fx::DeviceKind;

use super::StudioLayout;
use crate::components::midi_fx_editor::open_midi_fx_editor;
use crate::components::timeline::timeline_state::{MidiFxSlotState, TrackEditScope};

impl StudioLayout {
    pub(crate) fn toggle_midi_fx_picker(&mut self, track_id: &str, cx: &mut Context<Self>) {
        self.midi_fx_picker_open = match self.midi_fx_picker_open.take() {
            Some(open) if open == track_id => None,
            _ => Some(track_id.to_string()),
        };
        cx.notify();
    }

    /// Escape closes the "Add MIDI FX" dropdown.
    pub(super) fn handle_midi_fx_picker_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.midi_fx_picker_open.is_none() || event.keystroke.key != "escape" {
            return false;
        }
        self.midi_fx_picker_open = None;
        cx.notify();
        true
    }

    pub(crate) fn add_midi_fx(&mut self, track_id: &str, kind: DeviceKind, cx: &mut Context<Self>) {
        let edit = self.begin_track_edit(TrackEditScope::channel(track_id), cx);
        let added = self
            .timeline
            .update(cx, |timeline, _| timeline.state.add_midi_fx(track_id, kind));
        self.commit_track_edit("Add MIDI FX", edit, cx);
        self.midi_fx_picker_open = None;
        if let Some(slot_id) = added {
            self.mark_dirty();
            self.open_midi_fx_editor(track_id, &slot_id, cx);
        }
        cx.notify();
    }

    /// The MIDI-effect plug-ins the device list offers: scanned plug-ins
    /// that turn MIDI into MIDI and that the plug-in host can load, as
    /// `(registry id, name)`, by name. Empty without the plug-in host bridge,
    /// the only place a hosted MIDI effect runs.
    pub(crate) fn midi_fx_plugin_choices(&self) -> Vec<(String, String)> {
        if !super::plugin_bridge_runtime::bridge_enabled() {
            return Vec::new();
        }
        let mut choices: Vec<(String, String)> = self
            .plugin_catalog
            .available
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|plugin| {
                plugin.is_midi_effect()
                    && plugin.supports_insert()
                    && plugin.scan_status.is_usable()
                    && !plugin.is_builtin()
            })
            .map(|plugin| (plugin.id.clone(), plugin.name.clone()))
            .collect();
        choices.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        choices
    }

    /// Add the scanned plug-in `registry_id` at the end of `track_id`'s MIDI
    /// FX chain and load it in the plug-in host. One undo step.
    pub(crate) fn add_midi_fx_plugin(
        &mut self,
        track_id: &str,
        registry_id: &str,
        cx: &mut Context<Self>,
    ) {
        use crate::components::timeline::timeline_state::InsertPluginFormat;
        use SpherePluginHost::PluginFormat as RegFmt;
        let Some(plugin) = self
            .plugin_catalog
            .available
            .as_deref()
            .and_then(|plugins| plugins.iter().find(|p| p.id == registry_id))
            .filter(|plugin| plugin.is_midi_effect())
            .cloned()
        else {
            return;
        };
        let format = match plugin.format {
            RegFmt::Vst3 => InsertPluginFormat::Vst3,
            RegFmt::Vst2 => InsertPluginFormat::Vst2,
            RegFmt::Clap => InsertPluginFormat::Clap,
            RegFmt::Au => InsertPluginFormat::Au,
            RegFmt::Lv2 => InsertPluginFormat::Lv2,
            RegFmt::Unknown => InsertPluginFormat::Unknown,
        };
        let edit = self.begin_track_edit(TrackEditScope::channel(track_id), cx);
        let added = self.timeline.update(cx, |timeline, _| {
            timeline.state.add_midi_fx_plugin(
                track_id,
                plugin.class_id.clone().unwrap_or_else(|| plugin.id.clone()),
                Some(plugin.path.clone()),
                format,
                Some(plugin.vendor.clone()).filter(|vendor| !vendor.trim().is_empty()),
                plugin.name.clone(),
            )
        });
        self.commit_track_edit("Add MIDI FX", edit, cx);
        self.midi_fx_picker_open = None;
        if let Some((_, instance_id)) = added {
            self.mark_dirty();
            // Registers the sink with the engine and syncs the graph once
            // the host has it.
            self.load_bridge_insert_for_slot(track_id, &instance_id, cx);
        }
        cx.notify();
    }

    pub(crate) fn remove_midi_fx(&mut self, track_id: &str, slot_id: &str, cx: &mut Context<Self>) {
        let instance = self
            .midi_fx_slot(track_id, slot_id, cx)
            .and_then(|slot| slot.plugin)
            .map(|plugin| plugin.id);
        if let Some(instance) = &instance {
            // Undo brings the plug-in back as it sounds now.
            self.store_live_plugin_states(track_id, std::slice::from_ref(instance), cx);
        }
        let edit = self.begin_track_edit(TrackEditScope::channel(track_id), cx);
        if let Some(instance) = &instance {
            self.teardown_insert_instance(track_id, instance, cx, "midi_fx_remove");
        }
        let removed = self.timeline.update(cx, |timeline, _| {
            timeline.state.remove_midi_fx(track_id, slot_id)
        });
        self.commit_track_edit("Remove MIDI FX", edit, cx);
        if removed {
            // The editor shows "removed" until closed; the engine releases the
            // device's notes when the rebuilt graph lands.
            self.refresh_midi_fx_editors(cx);
            self.mark_dirty();
        }
        cx.notify();
    }

    pub(crate) fn toggle_midi_fx(&mut self, track_id: &str, slot_id: &str, cx: &mut Context<Self>) {
        let Some(enabled) = self.midi_fx_slot(track_id, slot_id, cx).map(|s| !s.enabled) else {
            return;
        };
        let edit = self.begin_track_edit(TrackEditScope::channel(track_id), cx);
        self.timeline.update(cx, |timeline, _| {
            timeline
                .state
                .set_midi_fx_enabled(track_id, slot_id, enabled)
        });
        self.commit_track_edit("Bypass MIDI FX", edit, cx);
        let live = self
            .audio_bridge
            .engine
            .as_ref()
            .is_some_and(|engine| engine.set_midi_fx_enabled(track_id, slot_id, enabled));
        self.mark_settings_changed(live);
        self.refresh_midi_fx_editors(cx);
        cx.notify();
    }

    /// A value from an editor (one knob drag folds into one undo step).
    pub(crate) fn set_midi_fx_param(
        &mut self,
        track_id: &str,
        slot_id: &str,
        param_id: &str,
        value: f32,
        cx: &mut Context<Self>,
    ) {
        let edit = self.begin_track_edit(TrackEditScope::channel(track_id), cx);
        let stored = self.timeline.update(cx, |timeline, _| {
            timeline
                .state
                .set_midi_fx_param(track_id, slot_id, param_id, value)
        });
        self.commit_track_edit_folded("Change MIDI FX", edit, cx);
        let Some(stored) = stored else {
            return;
        };
        let live =
            self.audio_bridge.engine.as_ref().is_some_and(|engine| {
                engine.set_midi_fx_param(track_id, slot_id, param_id, stored)
            });
        self.mark_settings_changed(live);
        cx.notify();
    }

    /// A bypass or parameter change is saved with the project; the engine
    /// already has it when it reached the running chain, otherwise it needs a
    /// sync.
    fn mark_settings_changed(&mut self, applied_live: bool) {
        if applied_live {
            self.mark_dirty_view_only();
        } else {
            self.mark_dirty();
        }
    }

    fn midi_fx_slot(&self, track_id: &str, slot_id: &str, cx: &App) -> Option<MidiFxSlotState> {
        read_slot(&self.timeline, track_id, slot_id, cx)
    }

    /// Open a slot's editor: the plug-in's own for a hosted plug-in, the
    /// device editor otherwise.
    pub(crate) fn open_midi_fx_slot(
        &mut self,
        track_id: &str,
        slot_id: &str,
        cx: &mut Context<Self>,
    ) {
        use crate::components::timeline::timeline_state::PluginRuntimeState;
        let Some(slot) = self.midi_fx_slot(track_id, slot_id, cx) else {
            return;
        };
        let Some(plugin) = slot.plugin else {
            self.open_midi_fx_editor(track_id, slot_id, cx);
            return;
        };
        match plugin.runtime_state {
            PluginRuntimeState::NotLoaded | PluginRuntimeState::Unloaded => {
                // Loads it; the row shows it loading, and a click once it is
                // ready opens the editor.
                self.load_bridge_insert_for_slot(track_id, &plugin.id, cx);
            }
            PluginRuntimeState::Loading
            | PluginRuntimeState::Missing(_)
            | PluginRuntimeState::Failed(_)
            | PluginRuntimeState::Crashed => {}
            _ => {
                // Opened on a later turn, past the frame on the stack (see
                // `open_insert_editor`).
                let Some(owner_bounds) = self.studio_window_bounds(cx) else {
                    return;
                };
                let key = (track_id.to_string(), plugin.id.clone());
                let track = track_id.to_string();
                let executor = cx.background_executor().clone();
                cx.spawn(async move |this, cx| {
                    executor.timer(std::time::Duration::from_millis(1)).await;
                    let _ = this.update(cx, |layout, cx| {
                        layout.open_bridged_editor_window(
                            owner_bounds,
                            key,
                            track,
                            plugin.id,
                            plugin.display_name,
                            cx,
                        );
                    });
                })
                .detach();
            }
        }
    }

    /// Open (or bring forward) the editor of a slot.
    pub(crate) fn open_midi_fx_editor(
        &mut self,
        track_id: &str,
        slot_id: &str,
        cx: &mut Context<Self>,
    ) {
        let key = (track_id.to_string(), slot_id.to_string());
        if let Some(handle) = self.midi_fx_editors.get(&key) {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.midi_fx_editors.remove(&key);
        }
        let Some(slot) = self.midi_fx_slot(track_id, slot_id, cx) else {
            return;
        };
        let Some(kind) = slot.kind() else {
            return;
        };
        let i18n = crate::i18n::I18n::from_app(cx);
        let track_name = self
            .timeline
            .read(cx)
            .state
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let title = format!("{} — {}", i18n.tr(kind.label()), track_name);

        let timeline = self.timeline.clone();
        let (read_track, read_slot_id) = key.clone();
        let read: crate::components::midi_fx_editor::ReadSlot =
            Arc::new(move |cx: &App| read_slot(&timeline, &read_track, &read_slot_id, cx));
        let owner = cx.entity().clone();
        let (param_track, param_slot) = key.clone();
        let on_param: crate::components::midi_fx_editor::OnParam = Arc::new(
            move |param_id: &str, value: f32, _window: &mut Window, cx: &mut App| {
                let _ = owner.update(cx, |layout, cx| {
                    layout.set_midi_fx_param(&param_track, &param_slot, param_id, value, cx);
                });
            },
        );
        let closer = cx.entity().clone();
        let close_key = key.clone();
        let on_close = Arc::new(move |_window: &mut Window, cx: &mut App| {
            let close_key = close_key.clone();
            let _ = closer.update(cx, |layout, _| {
                layout.midi_fx_editors.remove(&close_key);
            });
        });
        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        match open_midi_fx_editor(
            owner_bounds,
            title,
            kind,
            i18n,
            read,
            on_param,
            on_close,
            cx,
        ) {
            Ok(handle) => {
                self.midi_fx_editors.insert(key, handle);
            }
            Err(err) => eprintln!("[MidiFx] failed to open the editor: {err}"),
        }
    }

    /// Redraw the open editors (their values come from the timeline).
    pub(crate) fn refresh_midi_fx_editors(&mut self, cx: &mut Context<Self>) {
        self.midi_fx_editors.retain(|_, handle| {
            handle
                .update(cx, |editor, _, cx| {
                    let _ = editor;
                    cx.notify();
                })
                .is_ok()
        });
    }
}

fn read_slot(
    timeline: &gpui::Entity<crate::components::timeline::timeline::Timeline>,
    track_id: &str,
    slot_id: &str,
    cx: &App,
) -> Option<MidiFxSlotState> {
    timeline
        .read(cx)
        .state
        .tracks
        .iter()
        .find(|t| t.id == track_id)?
        .midi_fx
        .iter()
        .find(|s| s.id == slot_id)
        .cloned()
}
