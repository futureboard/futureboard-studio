//! Studio side of the plug-in editor window's chrome.
//!
//! The window renders the strip; everything it shows and everything it asks for
//! is owned here, because the insert, the engine and the preset files are the
//! studio's. See [`crate::components::plugin_editor_chrome`] for the view.

use std::path::PathBuf;

use gpui::Context;

use crate::components::plugin_editor_chrome::{
    PluginEditorAction, PluginEditorChrome, PluginEditorTab,
};
use crate::layout::StudioLayout;

/// Extension for a stored preset file.
///
/// The file itself is the shared FBPST container written by
/// `SpherePluginHost::preset::write_state_preset`: magic, a 24-byte header,
/// JSON metadata naming the plug-in it belongs to, then that plug-in's own
/// opaque state. Nothing here parses or edits the state — a preset a plug-in
/// cannot read back is worse than no preset at all — but the container around
/// it must be unwrapped before the state is handed over.
///
/// It is deliberately **not** `.pst`, even though the container is the one every
/// `.pst` uses: `SpherePluginHost::preset::clear_all_presets` and
/// `load_cached_plugins` walk the whole preset root recursively and treat every
/// `.pst` under it as a scan-cache row. User presets live inside that root, so
/// renaming the extension would list them as phantom plug-ins and delete them
/// with "Clear Plugin Cache".
const PRESET_EXTENSION: &str = "fbstate";

/// How an ARA editor's window is filed among the insert editors.
///
/// It has no insert slot of its own — it is bound to a clip — so the studio
/// gives it a key of this shape (`ara_studio::ara_insert_id`) to sit alongside
/// them. Recognising it here is what tells an ARA editor's chrome from an
/// insert's.
const ARA_INSERT_PREFIX: &str = "ara:";

/// Whether an editor key belongs to an ARA session rather than an insert slot.
///
/// One predicate for every place that has to tell them apart: the chrome an ARA
/// editor gets, and the stale-editor sweep that would otherwise tear one down
/// for having no slot behind it.
pub(super) fn is_ara_editor_key(insert_id: &str) -> bool {
    insert_id.starts_with(ARA_INSERT_PREFIX)
}

/// Set to keep the pre-GPUI Win32 editor shell for bridged inserts.
///
/// The editor moved into a GPUI window so its chrome — the insert it belongs
/// to, active, presets, what it costs — can live in the titlebar strip. This is
/// the way back if that window misbehaves on a machine, not a supported second
/// mode: it will go once the GPUI path has been through a release.
pub(super) fn legacy_native_editor_shell() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_EDITOR_LEGACY_SHELL").is_some())
}

/// Where one plug-in's user presets live.
///
/// Under the registry's preset root so presets sit beside everything else the
/// plug-in system writes, in a folder named for the plug-in rather than the
/// insert: a preset saved on one track is meant to be reachable from another.
fn preset_dir(plugin_id: &str) -> Option<PathBuf> {
    let safe: String = plugin_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        return None;
    }
    Some(
        SpherePluginHost::registry::default_preset_root()
            .join("User")
            .join(safe),
    )
}

/// Preset names for one plug-in, sorted, without extensions.
fn list_presets(plugin_id: &str) -> Vec<String> {
    let Some(dir) = preset_dir(plugin_id) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(PRESET_EXTENSION) {
                return None;
            }
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names
}

impl StudioLayout {
    /// Pushes fresh chrome into every open plug-in editor window.
    ///
    /// Called from the same poll that drives the bridge, so the readouts follow
    /// the engine without a timer of their own. An unchanged chrome notifies
    /// nothing, so a steady CPU reading does not repaint the window.
    pub(super) fn refresh_plugin_editor_chrome(&mut self, cx: &mut Context<Self>) {
        if self.plugin_editors.open.is_empty() {
            return;
        }
        let sample_rate = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.sample_rate)
            .unwrap_or(0);
        let handles: Vec<_> = self.plugin_editors.open.values().cloned().collect();
        for handle in handles {
            let Ok(Some(chrome)) = handle.update(cx, |editor, _window, _cx| {
                let (track_id, insert_id) = editor.insert_key();
                Some((track_id.to_string(), insert_id.to_string()))
            }) else {
                continue;
            };
            let (track_id, insert_id) = chrome;
            // An ARA editor's window is in `open` like any other, but it has no
            // insert slot behind it — it is bound to a clip. It gets its own
            // chrome, which is the plug-in's name and nothing it cannot back up.
            let Some(chrome) = self
                .plugin_editor_chrome_for(&track_id, &insert_id, sample_rate, cx)
                .or_else(|| self.ara_editor_chrome_for(&track_id, &insert_id))
            else {
                continue;
            };
            let tabs = self
                .ara_editor_tabs_for(&insert_id, &chrome)
                .unwrap_or_else(|| self.plugin_editor_tabs_for(&track_id, cx));
            let _ = handle.update(cx, |editor, _window, cx| {
                editor.set_chrome(chrome, cx);
                editor.set_tabs(tabs, cx);
            });
        }
    }

    /// Chrome for an ARA editor window.
    ///
    /// An ARA plug-in is bound to a clip, not to an insert slot: there is no
    /// bypass to toggle, no per-slot CPU or latency to read, and no
    /// insert-keyed preset list. So this fills in the name and leaves the rest
    /// empty, and the window drops its control row rather than drawing five
    /// controls with nothing behind them.
    ///
    /// `None` for anything that is not an ARA editor, which is what makes this
    /// usable as a fallback after the insert lookup.
    fn ara_editor_chrome_for(&self, track_id: &str, insert_id: &str) -> Option<PluginEditorChrome> {
        let plugin_id = insert_id.strip_prefix(ARA_INSERT_PREFIX)?;
        let key = crate::layout::ara_ops::AraSessionKey {
            plugin_id: plugin_id.to_string(),
            track_id: track_id.to_string(),
        };
        let plugin_name = self
            .ara
            .plugin_name(&key)
            .map(str::to_string)
            .unwrap_or_else(|| plugin_id.to_string());
        Some(PluginEditorChrome {
            plugin_name,
            track_name: String::new(),
            // Not an insert, so not a slot number. The titlebar drops the
            // "Insert n" suffix for a 0 rather than inventing a position.
            insert_number: 0,
            active: true,
            latency_samples: 0,
            sample_rate: 0,
            cpu_load: None,
            presets: Vec::new(),
            preset_index: None,
        })
    }

    /// The tab list for an ARA editor: the one plug-in it is bound to.
    ///
    /// `None` for anything that is not an ARA editor. A channel's insert tabs
    /// would be the wrong list here — those editors are somewhere else entirely
    /// and switching to one from this window is not a thing that can happen.
    fn ara_editor_tabs_for(
        &self,
        insert_id: &str,
        chrome: &PluginEditorChrome,
    ) -> Option<Vec<PluginEditorTab>> {
        insert_id.strip_prefix(ARA_INSERT_PREFIX)?;
        Some(vec![PluginEditorTab {
            insert_id: insert_id.to_string(),
            display_name: chrome.plugin_name.clone(),
            insert_number: 0,
        }])
    }

    /// Builds one insert's chrome from the project and the engine.
    fn plugin_editor_chrome_for(
        &self,
        track_id: &str,
        insert_id: &str,
        sample_rate: u32,
        cx: &Context<Self>,
    ) -> Option<PluginEditorChrome> {
        let state = &self.timeline.read(cx).state;
        let slots = state.insert_slots(track_id)?;
        let (index, slot) = slots
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.id == insert_id)?;
        let presets = slot
            .plugin_id
            .as_deref()
            .map(list_presets)
            .unwrap_or_default();
        // A bridged plug-in reports through its shared region, which the host
        // writes and the bridge runtime holds; the engine's control-side graph
        // is a clone from when the stream opened and never sees either value.
        // An in-process insert has no region, so that one does come from the
        // engine.
        let bridged = self
            .plugin_editors
            .bridge_runtime
            .as_ref()
            .and_then(|runtime| runtime.lock().ok()?.instance_load(insert_id));
        let (cpu_load, latency_samples) = match bridged {
            Some((share, latency)) => (share, latency),
            None => {
                let load = self
                    .audio_bridge
                    .engine
                    .as_ref()
                    .and_then(|engine| engine.insert_load(track_id, insert_id));
                (
                    load.map(|(share, _)| share),
                    load.map(|(_, latency)| latency).unwrap_or(0),
                )
            }
        };
        let track_name = state
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.name.clone())
            .unwrap_or_default();
        Some(PluginEditorChrome {
            plugin_name: slot.display_name.clone(),
            track_name,
            insert_number: index + 1,
            active: slot.enabled && !slot.bypassed,
            latency_samples,
            sample_rate,
            cpu_load,
            preset_index: self
                .plugin_editors
                .preset_selection
                .get(&(track_id.to_string(), insert_id.to_string()))
                .copied()
                .filter(|index| *index < presets.len()),
            presets,
        })
    }

    /// The plug-ins open on one channel, in slot order.
    ///
    /// A tab exists for every insert the user has opened an editor for on this
    /// channel; the window shows one of them at a time. Slot order rather than
    /// the order they were opened, because that is the order the audio actually
    /// goes through them in.
    fn plugin_editor_tabs_for(&self, track_id: &str, cx: &Context<Self>) -> Vec<PluginEditorTab> {
        let Some(open) = self.plugin_editors.editor_tabs.get(track_id) else {
            return Vec::new();
        };
        if open.is_empty() {
            return Vec::new();
        }
        let state = &self.timeline.read(cx).state;
        let Some(slots) = state.insert_slots(track_id) else {
            return Vec::new();
        };
        slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| open.iter().any(|id| id == &slot.id))
            .map(|(index, slot)| PluginEditorTab {
                insert_id: slot.id.clone(),
                display_name: slot.display_name.clone(),
                insert_number: index + 1,
            })
            .collect()
    }

    /// Brings one of a channel's open plug-ins to the front of its window.
    fn select_plugin_editor_tab(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = self.plugin_editor_window_for(track_id) else {
            return;
        };
        let display_name = self
            .timeline
            .read(cx)
            .state
            .insert_slots(track_id)
            .and_then(|slots| {
                slots
                    .iter()
                    .find(|slot| slot.id == insert_id)
                    .map(|slot| slot.display_name.clone())
            })
            .unwrap_or_else(|| insert_id.to_string());
        let _ = handle.update(cx, |editor, window, cx| {
            editor.activate_tab(insert_id, &display_name, window, cx);
        });
    }

    /// Closes one tab. The last one closes the window with it.
    fn close_plugin_editor_tab(&mut self, track_id: &str, insert_id: &str, cx: &mut Context<Self>) {
        let remaining: Vec<String> = {
            let tabs = self
                .plugin_editors
                .editor_tabs
                .entry(track_id.to_string())
                .or_default();
            tabs.retain(|id| id != insert_id);
            tabs.clone()
        };
        let was_active = self
            .plugin_editor_window_for(track_id)
            .and_then(|handle| {
                handle
                    .update(cx, |editor, _window, _cx| {
                        editor.insert_key().1 == insert_id
                    })
                    .ok()
            })
            .unwrap_or(false);
        // The plug-in keeps processing; only its editor closed.
        self.close_bridge_editor(cx, track_id, insert_id);
        let Some(handle) = self.plugin_editor_window_for(track_id) else {
            return;
        };
        let Some(next) = remaining.first().cloned() else {
            self.plugin_editors.editor_tabs.remove(track_id);
            self.plugin_editors
                .open
                .retain(|(track, _), _| track != track_id);
            // The preset list is a window of its own; it goes before the editor
            // it hangs from, so no platform is left holding an orphan.
            let _ = handle.update(cx, |editor, window, cx| {
                editor.close_preset_menu(cx);
                window.remove_window();
            });
            return;
        };
        if was_active {
            self.select_plugin_editor_tab(track_id, &next, cx);
        }
        cx.notify();
    }

    /// The editor window hosting one channel, whichever insert opened it.
    pub(super) fn plugin_editor_window_for(
        &self,
        track_id: &str,
    ) -> Option<gpui::WindowHandle<crate::components::plugin_editor_window::PluginEditorWindow>>
    {
        self.plugin_editors
            .open
            .iter()
            .find(|((track, _), _)| track == track_id)
            .map(|(_, handle)| *handle)
    }

    /// Applies whatever the chrome's controls asked for since the last poll.
    pub(super) fn drain_plugin_editor_chrome_actions(&mut self, cx: &mut Context<Self>) {
        if self.plugin_editors.open.is_empty() {
            return;
        }
        let handles: Vec<_> = self.plugin_editors.open.values().cloned().collect();
        let mut requests: Vec<(String, String, PluginEditorAction)> = Vec::new();
        for handle in handles {
            let _ = handle.update(cx, |editor, _window, _cx| {
                let (track_id, insert_id) = editor.insert_key();
                let (track_id, insert_id) = (track_id.to_string(), insert_id.to_string());
                for action in editor.take_chrome_actions() {
                    requests.push((track_id.clone(), insert_id.clone(), action));
                }
            });
        }
        for (track_id, insert_id, action) in requests {
            self.apply_plugin_editor_action(&track_id, &insert_id, action, cx);
        }
    }

    fn apply_plugin_editor_action(
        &mut self,
        track_id: &str,
        insert_id: &str,
        action: PluginEditorAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            PluginEditorAction::SetActive(active) => {
                // The same live path the inspector's own toggle uses: the
                // runtime "enabled" param, no graph rebuild.
                let changed = self.timeline.update(cx, |timeline, cx| {
                    let Some(slots) = timeline.state.insert_slots_mut(track_id) else {
                        return false;
                    };
                    let Some(slot) = slots.iter_mut().find(|slot| slot.id == insert_id) else {
                        return false;
                    };
                    if slot.enabled == active && !slot.bypassed {
                        return false;
                    }
                    slot.enabled = active;
                    // Bypass and disable say the same thing from the editor's
                    // side, so turning the plug-in back on clears both.
                    if active {
                        slot.bypassed = false;
                    }
                    cx.notify();
                    true
                });
                if changed {
                    self.push_insert_enabled_to_engine(track_id, insert_id, cx);
                    self.mark_dirty_view_only();
                    self.push_mixer_snapshot_to_window(cx);
                    cx.notify();
                }
            }
            PluginEditorAction::StepPreset(delta) => {
                self.step_plugin_editor_preset(track_id, insert_id, delta, cx);
            }
            PluginEditorAction::SavePreset => {
                self.save_plugin_editor_preset(track_id, insert_id, cx);
            }
            PluginEditorAction::SelectPreset(index) => {
                self.load_plugin_editor_preset(track_id, insert_id, index, cx);
            }
            // Window state; it never reaches here.
            PluginEditorAction::TogglePresetMenu(_) => {}
            PluginEditorAction::SelectTab(target) => {
                self.select_plugin_editor_tab(track_id, &target, cx);
            }
            PluginEditorAction::CloseTab(target) => {
                self.close_plugin_editor_tab(track_id, &target, cx);
            }
        }
    }

    /// Moves through the preset list and loads what it lands on.
    fn step_plugin_editor_preset(
        &mut self,
        track_id: &str,
        insert_id: &str,
        delta: i32,
        cx: &mut Context<Self>,
    ) {
        let Some(plugin_id) = self.insert_plugin_id(track_id, insert_id, cx) else {
            return;
        };
        let count = list_presets(&plugin_id).len() as i64;
        if count == 0 {
            return;
        }
        let key = (track_id.to_string(), insert_id.to_string());
        let current = self
            .plugin_editors
            .preset_selection
            .get(&key)
            .copied()
            .unwrap_or(0) as i64;
        let index = (((current + delta as i64) % count + count) % count) as usize;
        self.load_plugin_editor_preset(track_id, insert_id, index, cx);
    }

    /// Loads one preset by position in the list.
    fn load_plugin_editor_preset(
        &mut self,
        track_id: &str,
        insert_id: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(plugin_id) = self.insert_plugin_id(track_id, insert_id, cx) else {
            return;
        };
        let presets = list_presets(&plugin_id);
        if index >= presets.len() {
            return;
        }
        let key = (track_id.to_string(), insert_id.to_string());
        let Some(dir) = preset_dir(&plugin_id) else {
            return;
        };
        let path = dir.join(format!("{}.{PRESET_EXTENSION}", presets[index]));
        // Through the container reader, never `fs::read`. The file is an FBPST
        // container and its state starts past the magic, the 24-byte header and
        // the JSON metadata; handing the whole file to `send_plugin_state` gave
        // the plug-in a payload it cannot parse, which is a preset that silently
        // does nothing or worse.
        let (preset_plugin_id, state) = match SpherePluginHost::preset::read_state_preset(&path) {
            Ok(preset) => preset,
            Err(error) => {
                eprintln!("[plugin-preset] could not read {}: {error}", path.display());
                return;
            }
        };
        // The container says which plug-in produced the state, and state handed
        // to a different plug-in is not a preset, it is corruption. Presets are
        // filed per plug-in, so a mismatch means a file was moved or a plug-in
        // id changed — either way the load stops here.
        if preset_plugin_id != plugin_id {
            eprintln!(
                "[plugin-preset] refusing {}: it holds state for '{preset_plugin_id}', not \
                 '{plugin_id}'",
                path.display()
            );
            return;
        }
        // Straight back to the plug-in as opaque state, and into the project so
        // a save keeps what is actually loaded.
        if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref().cloned() {
            if let Ok(mut runtime) = runtime.lock() {
                if let Err(error) = runtime.send_plugin_state(insert_id, &state) {
                    eprintln!("[plugin-preset] SetPluginState failed: {error}");
                    return;
                }
            }
        }
        let state_len = state.len();
        let stored = std::sync::Arc::new(state);
        self.timeline.update(cx, |timeline, cx| {
            if let Some(slots) = timeline.state.insert_slots_mut(track_id) {
                if let Some(slot) = slots.iter_mut().find(|slot| slot.id == insert_id) {
                    slot.vst3_state = Some(stored.clone());
                }
            }
            cx.notify();
        });
        self.plugin_editors.preset_selection.insert(key, index);
        self.mark_dirty_view_only();
        eprintln!(
            "[plugin-preset] loaded '{}' for insert={insert_id} bytes={state_len}",
            presets[index]
        );
        cx.notify();
    }

    /// Captures the plug-in's current state and writes it as a new preset.
    fn save_plugin_editor_preset(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(plugin_id) = self.insert_plugin_id(track_id, insert_id, cx) else {
            return;
        };
        let Some(dir) = preset_dir(&plugin_id) else {
            return;
        };
        // Asked for fresh rather than reusing the project's copy: the point of
        // saving from the editor is to keep what the user just dialled in, and
        // the project's copy is only as new as the last save.
        let captured = self
            .plugin_editors
            .bridge_runtime
            .as_ref()
            .cloned()
            .and_then(|runtime| {
                runtime.lock().ok().map(|mut runtime| {
                    runtime.request_plugin_states(
                        std::slice::from_ref(&insert_id.to_string()),
                        std::time::Duration::from_millis(1500),
                    )
                })
            })
            .and_then(|mut states| states.remove(insert_id));
        let bytes = match captured {
            Some(bytes) if !bytes.is_empty() => bytes,
            _ => {
                self.ara.last_error = None;
                eprintln!(
                    "[plugin-preset] nothing to save for insert={insert_id}: the plug-in \
                     returned no state"
                );
                return;
            }
        };

        // Numbered rather than prompting: the editor has no room for a dialog,
        // and a preset the user can rename on disk beats one they cannot save.
        let existing = list_presets(&plugin_id);
        let mut index = existing.len() + 1;
        let path = loop {
            let candidate = dir.join(format!("Preset {index}.{PRESET_EXTENSION}"));
            if !candidate.exists() {
                break candidate;
            }
            index += 1;
        };
        let plugin_name = self
            .timeline
            .read(cx)
            .state
            .insert_slots(track_id)
            .and_then(|slots| {
                slots
                    .iter()
                    .find(|slot| slot.id == insert_id)
                    .map(|slot| slot.display_name.clone())
            })
            .unwrap_or_else(|| plugin_id.clone());
        if let Err(error) = SpherePluginHost::preset::write_state_preset(
            &path,
            &SpherePluginHost::preset::StatePreset {
                plugin_id: plugin_id.clone(),
                plugin_name,
                state: bytes.clone(),
            },
        ) {
            eprintln!(
                "[plugin-preset] could not write {}: {error}",
                path.display()
            );
            return;
        }
        eprintln!(
            "[plugin-preset] saved {} bytes={}",
            path.display(),
            bytes.len()
        );
        let names = list_presets(&plugin_id);
        if let Some(position) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| names.iter().position(|name| name == stem))
        {
            self.plugin_editors
                .preset_selection
                .insert((track_id.to_string(), insert_id.to_string()), position);
        }
        cx.notify();
    }

    fn insert_plugin_id(
        &self,
        track_id: &str,
        insert_id: &str,
        cx: &Context<Self>,
    ) -> Option<String> {
        let state = &self.timeline.read(cx).state;
        state
            .insert_slots(track_id)?
            .iter()
            .find(|slot| slot.id == insert_id)
            .and_then(|slot| slot.plugin_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ARA key has to be told from an insert key, and both places that ask
    /// have to get the same answer.
    ///
    /// The sweep in `reconcile_open_plugin_editors` tears down any editor whose
    /// insert slot is gone. An ARA editor never had one — it is bound to a clip
    /// — so without this predicate every ARA editor window is stale the instant
    /// it opens, and gets torn down along with the plug-in behind it.
    #[test]
    fn an_ara_editor_key_is_not_an_insert_key() {
        assert!(is_ara_editor_key("ara:com.celemony.melodyne"));
        assert!(is_ara_editor_key(ARA_INSERT_PREFIX));
        assert!(!is_ara_editor_key("insert-1"));
        assert!(!is_ara_editor_key(""));
        // Not a prefix match anywhere else in the string.
        assert!(!is_ara_editor_key("track:ara:thing"));
    }
}
