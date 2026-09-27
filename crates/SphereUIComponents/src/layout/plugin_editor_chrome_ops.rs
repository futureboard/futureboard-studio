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

/// Extension of a user preset file.
///
/// The file is the shared FBPST container written by
/// `SpherePluginHost::preset::write_state_preset`: magic, a 24-byte header,
/// JSON metadata naming the plug-in it belongs to, then that plug-in's own
/// opaque state. Nothing here parses or edits the state — a preset a plug-in
/// cannot read back is worse than no preset at all — but the container around
/// it must be unwrapped before the state is handed over.
///
/// `.pst` is safe under the preset root: the scan cache also walks that root for
/// `.pst`, but a user preset carries state and a cache row never does, so the
/// cache reader skips it and Clear Database keeps it
/// (`SpherePluginHost::preset::is_state_preset_file`).
const PRESET_EXTENSION: &str = "pst";

/// Extension user presets were saved with before they became `.pst`. Still
/// listed and loaded, so nothing saved earlier disappears.
const LEGACY_PRESET_EXTENSION: &str = "fbstate";
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

/// One plug-in's user presets, sorted by name: `(name, file)`.
///
/// `.pst` and the older `.fbstate` both count; where one name exists as both,
/// the `.pst` wins so the list never shows a preset twice.
fn list_preset_files(plugin_id: &str) -> Vec<(String, PathBuf)> {
    let Some(dir) = preset_dir(plugin_id) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut presets: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        let modern = ext.eq_ignore_ascii_case(PRESET_EXTENSION);
        if !modern && !ext.eq_ignore_ascii_case(LEGACY_PRESET_EXTENSION) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        match presets.iter_mut().find(|(existing, _)| existing == name) {
            Some(slot) if modern => slot.1 = path,
            Some(_) => {}
            None => presets.push((name.to_string(), path)),
        }
    }
    presets.sort_by_key(|(name, _)| name.to_lowercase());
    presets
}

/// Preset names for one plug-in, in list order.
fn list_presets(plugin_id: &str) -> Vec<String> {
    list_preset_files(plugin_id)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// A file name the OS will take for a preset called `name`.
fn preset_file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let stem = stem.trim().trim_end_matches('.').to_string();
    if stem.is_empty() {
        "Preset".to_string()
    } else {
        stem
    }
}

/// `path` with a `.pst` extension, whatever the dialog handed back. Appended
/// rather than `set_extension`, which would eat the "2" of "Lead v1.2".
fn with_preset_extension(path: PathBuf) -> PathBuf {
    let has_it = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(PRESET_EXTENSION));
    if has_it {
        return path;
    }
    let mut raw = path.into_os_string();
    raw.push(".");
    raw.push(PRESET_EXTENSION);
    PathBuf::from(raw)
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
            can_paste: false,
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
        let can_paste = self
            .plugin_editors
            .state_clipboard
            .as_ref()
            .is_some_and(|copied| slot.plugin_id.as_deref() == Some(copied.plugin_id.as_str()));
        Some(PluginEditorChrome {
            can_paste,
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
    pub(super) fn select_plugin_editor_tab(
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
                let edit = self.begin_track_edit(
                    crate::components::timeline::timeline_state::TrackEditScope::channel(track_id),
                    cx,
                );
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
                self.commit_track_edit(
                    if active {
                        "Activate Plug-in"
                    } else {
                        "Deactivate Plug-in"
                    },
                    edit,
                    cx,
                );
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
            PluginEditorAction::ImportPreset => {
                self.import_plugin_editor_preset(track_id, insert_id, cx);
            }
            PluginEditorAction::RevealPresetFolder => {
                if let Some(dir) = self
                    .insert_plugin_id(track_id, insert_id, cx)
                    .and_then(|plugin_id| preset_dir(&plugin_id))
                {
                    let _ = std::fs::create_dir_all(&dir);
                    super::helpers::reveal_path(&dir);
                }
            }
            PluginEditorAction::SelectPreset(index) => {
                self.load_plugin_editor_preset(track_id, insert_id, index, cx);
            }
            PluginEditorAction::CopyState => {
                self.copy_plugin_editor_state(track_id, insert_id, cx);
            }
            PluginEditorAction::PasteState => {
                self.paste_plugin_editor_state(track_id, insert_id, cx);
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
        let presets = list_preset_files(&plugin_id);
        let Some((name, path)) = presets.get(index).cloned() else {
            return;
        };
        let key = (track_id.to_string(), insert_id.to_string());
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
        let state_len = state.len();
        // Undo goes back to what the plug-in had before, not to the last
        // saved state; stepping through presets is one step (the history
        // folds consecutive loads on one insert).
        let before = self.capture_insert_state(track_id, insert_id, cx);
        if !self.apply_plugin_state_to_insert(track_id, insert_id, std::sync::Arc::new(state), cx) {
            return;
        }
        self.record_insert_state("Load Preset", track_id, insert_id, before, cx);
        self.plugin_editors.preset_selection.insert(key, index);
        eprintln!("[plugin-preset] loaded '{name}' for insert={insert_id} bytes={state_len}");
        cx.notify();
    }

    /// Hands `state` to the plug-in as opaque state, and into the project so a
    /// save keeps what is actually loaded. `false` when the plug-in host
    /// refused it, in which case the project is left as it was.
    fn apply_plugin_state_to_insert(
        &mut self,
        track_id: &str,
        insert_id: &str,
        state: std::sync::Arc<Vec<u8>>,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref().cloned() {
            if let Ok(mut runtime) = runtime.lock() {
                if let Err(error) = runtime.send_plugin_state(insert_id, &state) {
                    eprintln!("[plugin-state] SetPluginState failed insert={insert_id}: {error}");
                    return false;
                }
            }
        }
        self.timeline.update(cx, |timeline, cx| {
            if let Some(slots) = timeline.state.insert_slots_mut(track_id) {
                if let Some(slot) = slots.iter_mut().find(|slot| slot.id == insert_id) {
                    slot.vst3_state = Some(state);
                }
            }
            cx.notify();
        });
        self.mark_dirty_view_only();
        true
    }

    /// Takes this insert's current state for a later Paste.
    fn copy_plugin_editor_state(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some((plugin_id, plugin_name)) = self
            .timeline
            .read(cx)
            .state
            .find_insert_slot(track_id, insert_id)
            .and_then(|slot| Some((slot.plugin_id.clone()?, slot.display_name.clone())))
        else {
            return;
        };
        let Some(state) = self
            .capture_insert_state(track_id, insert_id, cx)
            .filter(|state| !state.is_empty())
        else {
            eprintln!(
                "[plugin-state] nothing to copy from insert={insert_id}: the plug-in returned no \
                 state"
            );
            return;
        };
        eprintln!(
            "[plugin-state] copied insert={insert_id} plugin={plugin_id} bytes={}",
            state.len()
        );
        self.plugin_editors.state_clipboard = Some(super::plugin_ops::PluginStateClipboard {
            plugin_id,
            plugin_name,
            state,
        });
        cx.notify();
    }

    /// Loads the copied state into this insert, when it came from the same
    /// plug-in. The preset strip then names no preset: what is loaded is no
    /// longer any one of them.
    fn paste_plugin_editor_state(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(copied) = self.plugin_editors.state_clipboard.clone() else {
            return;
        };
        let Some(plugin_id) = self.insert_plugin_id(track_id, insert_id, cx) else {
            return;
        };
        // The chrome only offers Paste for a match; this is the same rule for
        // an action that was queued before the clipboard changed.
        if plugin_id != copied.plugin_id {
            eprintln!(
                "[plugin-state] refusing paste into insert={insert_id}: the copy holds state \
                 for '{}' ({}), not '{plugin_id}'",
                copied.plugin_id, copied.plugin_name
            );
            return;
        }
        let bytes = copied.state.len();
        let before = self.capture_insert_state(track_id, insert_id, cx);
        if !self.apply_plugin_state_to_insert(track_id, insert_id, copied.state, cx) {
            return;
        }
        self.record_insert_state("Paste Plug-in State", track_id, insert_id, before, cx);
        self.plugin_editors
            .preset_selection
            .remove(&(track_id.to_string(), insert_id.to_string()));
        eprintln!("[plugin-state] pasted into insert={insert_id} bytes={bytes}");
        cx.notify();
    }

    /// Saves the plug-in's current sound as a `.pst` preset file, wherever the
    /// user puts it through the OS Save dialog.
    ///
    /// The state is captured before the dialog opens: what gets saved is the
    /// sound the user heard when they clicked Save, not whatever the plug-in
    /// drifted to while the dialog was up. The dialog starts in this plug-in's
    /// preset folder, which is what the popover lists; a file saved anywhere
    /// else is still a preset, it just is not in that list.
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
        let Some(state) = self
            .capture_insert_state(track_id, insert_id, cx)
            .filter(|state| !state.is_empty())
        else {
            eprintln!(
                "[plugin-preset] nothing to save for insert={insert_id}: the plug-in returned no \
                 state"
            );
            return;
        };
        let plugin_name = self
            .timeline
            .read(cx)
            .state
            .find_insert_slot(track_id, insert_id)
            .map(|slot| slot.display_name.clone())
            .unwrap_or_else(|| plugin_id.clone());
        // Suggest the loaded preset's name — saving over it is the common case
        // — or the next free "Preset n".
        let existing = list_presets(&plugin_id);
        let suggested = self
            .plugin_editors
            .preset_selection
            .get(&(track_id.to_string(), insert_id.to_string()))
            .and_then(|&index| existing.get(index).cloned())
            .unwrap_or_else(|| {
                (existing.len() + 1..)
                    .map(|n| format!("Preset {n}"))
                    .find(|name| !existing.contains(name))
                    .unwrap_or_else(|| "Preset".to_string())
            });
        let _ = std::fs::create_dir_all(&dir);
        let preset = SpherePluginHost::preset::StatePreset {
            plugin_id,
            plugin_name,
            state: state.as_ref().clone(),
        };
        self.save_preset_through_dialog(track_id, insert_id, dir, suggested, preset, cx);
    }

    #[cfg(feature = "native-dialogs")]
    fn save_preset_through_dialog(
        &mut self,
        track_id: &str,
        insert_id: &str,
        dir: PathBuf,
        suggested: String,
        preset: SpherePluginHost::preset::StatePreset,
        cx: &mut Context<Self>,
    ) {
        let file_name = format!("{}.{PRESET_EXTENSION}", preset_file_stem(&suggested));
        let title = format!("Save {} Preset", preset.plugin_name);
        let base = rfd::AsyncFileDialog::new()
            .set_title(title)
            .set_directory(&dir)
            .set_file_name(file_name)
            .add_filter("Futureboard Preset", &[PRESET_EXTENSION]);
        let dialog = self.parent_dialog_to_editor(track_id, base, cx);
        let key = (track_id.to_string(), insert_id.to_string());
        cx.spawn(async move |this, cx| {
            let Some(handle) = dialog.save_file().await else {
                return; // cancelled
            };
            let path = with_preset_extension(handle.path().to_path_buf());
            if let Err(error) = SpherePluginHost::preset::write_state_preset(&path, &preset) {
                eprintln!(
                    "[plugin-preset] could not write {}: {error}",
                    path.display()
                );
                // A save the user asked for that silently does nothing is
                // worse than an extra dialog.
                rfd::AsyncMessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("Save Preset")
                    .set_description(format!("Could not save {}:\n{error}", path.display()))
                    .show()
                    .await;
                return;
            }
            eprintln!(
                "[plugin-preset] saved {} bytes={}",
                path.display(),
                preset.state.len()
            );
            let _ = this.update(cx, |layout, cx| {
                layout.select_saved_preset(key, &preset.plugin_id, &path, cx);
            });
        })
        .detach();
    }

    /// Without native dialogs there is nowhere to ask for a path, so the preset
    /// goes into the plug-in's folder under the suggested name.
    #[cfg(not(feature = "native-dialogs"))]
    fn save_preset_through_dialog(
        &mut self,
        track_id: &str,
        insert_id: &str,
        dir: PathBuf,
        suggested: String,
        preset: SpherePluginHost::preset::StatePreset,
        cx: &mut Context<Self>,
    ) {
        let path = dir.join(format!(
            "{}.{PRESET_EXTENSION}",
            preset_file_stem(&suggested)
        ));
        if let Err(error) = SpherePluginHost::preset::write_state_preset(&path, &preset) {
            eprintln!(
                "[plugin-preset] could not write {}: {error}",
                path.display()
            );
            return;
        }
        let key = (track_id.to_string(), insert_id.to_string());
        self.select_saved_preset(key, &preset.plugin_id, &path, cx);
    }

    /// Marks a just-written preset as the loaded one, when it landed in the
    /// folder the popover lists.
    fn select_saved_preset(
        &mut self,
        key: (String, String),
        plugin_id: &str,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let position = list_preset_files(plugin_id)
            .iter()
            .position(|(_, listed)| listed == path);
        match position {
            Some(position) => {
                self.plugin_editors.preset_selection.insert(key, position);
            }
            None => {
                self.plugin_editors.preset_selection.remove(&key);
            }
        }
        cx.notify();
    }

    /// Loads a preset file from anywhere, through the OS Open dialog, and
    /// files a copy in this plug-in's preset folder so it is in the list from
    /// then on.
    #[cfg(feature = "native-dialogs")]
    fn import_plugin_editor_preset(
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
        let _ = std::fs::create_dir_all(&dir);
        let base = rfd::AsyncFileDialog::new()
            .set_title("Import Preset")
            .set_directory(&dir)
            .add_filter(
                "Futureboard Preset",
                &[PRESET_EXTENSION, LEGACY_PRESET_EXTENSION],
            );
        let dialog = self.parent_dialog_to_editor(track_id, base, cx);
        let (track_id, insert_id) = (track_id.to_string(), insert_id.to_string());
        cx.spawn(async move |this, cx| {
            let Some(handle) = dialog.pick_file().await else {
                return;
            };
            let source = handle.path().to_path_buf();
            let refusal = match SpherePluginHost::preset::read_state_preset(&source) {
                Err(error) => Some(format!(
                    "{} is not a plug-in preset:\n{error}",
                    source.display()
                )),
                Ok((owner, _)) if owner != plugin_id => Some(format!(
                    "{} was saved from a different plug-in ({owner}). Presets only load into \
                     the plug-in that made them.",
                    source.display()
                )),
                Ok(_) => None,
            };
            if let Some(message) = refusal {
                rfd::AsyncMessageDialog::new()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title("Import Preset")
                    .set_description(message)
                    .show()
                    .await;
                return;
            }
            let _ = this.update(cx, |layout, cx| {
                layout.import_preset_file(&track_id, &insert_id, &plugin_id, &dir, &source, cx);
            });
        })
        .detach();
    }

    #[cfg(not(feature = "native-dialogs"))]
    fn import_plugin_editor_preset(
        &mut self,
        _track_id: &str,
        _insert_id: &str,
        _cx: &mut Context<Self>,
    ) {
        eprintln!("[plugin-preset] native file dialogs are disabled in this build");
    }

    /// Loads an already-validated preset file into the insert, and keeps a copy
    /// in the plug-in's folder unless it already lives there.
    #[cfg(feature = "native-dialogs")]
    fn import_preset_file(
        &mut self,
        track_id: &str,
        insert_id: &str,
        plugin_id: &str,
        dir: &std::path::Path,
        source: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let Ok((_, state)) = SpherePluginHost::preset::read_state_preset(source) else {
            return;
        };
        let filed = if source.parent() == Some(dir) {
            source.to_path_buf()
        } else {
            let stem = source
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(preset_file_stem)
                .unwrap_or_else(|| "Imported".to_string());
            let target = (1..)
                .map(|n| {
                    let name = if n == 1 {
                        stem.clone()
                    } else {
                        format!("{stem} {n}")
                    };
                    dir.join(format!("{name}.{PRESET_EXTENSION}"))
                })
                .find(|candidate| !candidate.exists())
                .unwrap_or_else(|| dir.join(format!("{stem}.{PRESET_EXTENSION}")));
            let plugin_name = self
                .timeline
                .read(cx)
                .state
                .find_insert_slot(track_id, insert_id)
                .map(|slot| slot.display_name.clone())
                .unwrap_or_else(|| plugin_id.to_string());
            match SpherePluginHost::preset::write_state_preset(
                &target,
                &SpherePluginHost::preset::StatePreset {
                    plugin_id: plugin_id.to_string(),
                    plugin_name,
                    state: state.clone(),
                },
            ) {
                Ok(()) => target,
                Err(error) => {
                    // Loading still goes ahead: the user asked for the sound,
                    // and only the filing failed.
                    eprintln!(
                        "[plugin-preset] could not file {} in {}: {error}",
                        source.display(),
                        dir.display()
                    );
                    source.to_path_buf()
                }
            }
        };
        let before = self.capture_insert_state(track_id, insert_id, cx);
        if !self.apply_plugin_state_to_insert(track_id, insert_id, std::sync::Arc::new(state), cx) {
            return;
        }
        self.record_insert_state("Import Preset", track_id, insert_id, before, cx);
        self.select_saved_preset(
            (track_id.to_string(), insert_id.to_string()),
            plugin_id,
            &filed,
            cx,
        );
        eprintln!("[plugin-preset] imported {}", source.display());
    }

    /// Parents a file dialog to this channel's editor window.
    ///
    /// The editor is a topmost window, so a dialog without an owner opens
    /// *behind* the editor the user just clicked Save in — present, modal, and
    /// invisible.
    #[cfg(feature = "native-dialogs")]
    fn parent_dialog_to_editor(
        &self,
        track_id: &str,
        dialog: rfd::AsyncFileDialog,
        cx: &mut Context<Self>,
    ) -> rfd::AsyncFileDialog {
        let Some(handle) = self.plugin_editor_window_for(track_id) else {
            return dialog;
        };
        let mut dialog = Some(dialog);
        let parented = handle
            .update(cx, |_editor, window, _cx| {
                dialog.take().map(|dialog| dialog.set_parent(window))
            })
            .ok()
            .flatten();
        parented
            .or(dialog)
            .unwrap_or_else(rfd::AsyncFileDialog::new)
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
