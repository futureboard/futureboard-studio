//! The studio side of the Missing Plug-ins and Files window: which plug-ins
//! and audio files the project refers to but cannot find, and pointing them
//! at the right place.
//!
//! A plug-in is relinked by changing only where its file is, so the insert
//! keeps its saved state, parameters and automation, and is then loaded from
//! there. An audio file is relinked by asset, so every clip on it moves
//! together and its peaks and ARA identity stay its own.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gpui::{App, Context, Window};

use super::StudioLayout;
use crate::components::missing_media_window::{
    open_missing_media_window, MissingKind, MissingMediaCallbacks, MissingRow, MissingStatus,
};
use crate::components::plugin_picker::STUB_PLUGIN_ID;
use crate::components::timeline::timeline_state::{
    AudioImportState, ClipType, InsertPluginFormat, InsertSlotState, PluginRuntimeBackend,
    PluginRuntimeState, TimelineState, MASTER_TRACK_ID,
};
use crate::components::timeline::{waveform_cache, waveform_detail, waveform_samples};

const PLUGIN_ROW: &str = "plugin:";
const AUDIO_ROW: &str = "audio:";

/// Audio files a relocate offers, by extension.
const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "w64", "aif", "aiff", "flac", "mp3", "ogg", "m4a", "caf",
];

/// Entries a folder search reads before it gives up.
const SEARCH_ENTRY_LIMIT: usize = 250_000;

/// What the window follows: everything found missing since it opened, kept
/// after a relink so its row says it was found. Plug-ins by plug-in id,
/// audio files by asset.
#[derive(Default)]
pub(crate) struct MissingTracked {
    plugins: Vec<String>,
    audio: Vec<String>,
}

impl MissingTracked {
    fn add(&mut self, plugins: Vec<String>, audio: Vec<String>) {
        for id in plugins {
            if !self.plugins.contains(&id) {
                self.plugins.push(id);
            }
        }
        for key in audio {
            if !self.audio.contains(&key) {
                self.audio.push(key);
            }
        }
    }
}

/// Every insert slot with its owner's id and name, the master's included.
fn insert_slots(state: &TimelineState) -> impl Iterator<Item = (&str, &str, &InsertSlotState)> {
    state
        .tracks
        .iter()
        .flat_map(|track| {
            track
                .inserts
                .iter()
                .map(move |slot| (track.id.as_str(), track.name.as_str(), slot))
        })
        .chain(
            state
                .master
                .inserts
                .iter()
                .map(|slot| (MASTER_TRACK_ID, "Master", slot)),
        )
}

/// A plug-in Futureboard hosts from a file: not a built-in, not the empty
/// placeholder.
fn hosted_plugin_id(slot: &InsertSlotState) -> Option<&str> {
    slot.plugin_id
        .as_deref()
        .filter(|id| *id != STUB_PLUGIN_ID && !SpherePluginHost::builtin_audio_bridge_supported(id))
}

/// Whether the slot's plug-in file is not where the slot says.
fn plugin_file_missing(slot: &InsertSlotState) -> bool {
    slot.plugin_format
        .is_some_and(InsertPluginFormat::has_module_file)
        && slot.plugin_path.as_ref().is_none_or(|path| !path.exists())
}

/// Every audio clip as (asset key, track name, source path).
fn audio_clips(state: &TimelineState) -> impl Iterator<Item = (&str, &str, &str)> {
    state.tracks.iter().flat_map(|track| {
        track
            .clips
            .iter()
            .filter_map(move |clip| match &clip.clip_type {
                ClipType::Audio {
                    file_id,
                    source_path: Some(path),
                } if !file_id.is_empty() => {
                    Some((file_id.as_str(), track.name.as_str(), path.as_str()))
                }
                _ => None,
            })
    })
}

/// What is missing in `state` now: plug-ins (by id) whose file is not there
/// or that did not load, and audio assets whose file is not there.
fn scan_missing(state: &TimelineState) -> (Vec<String>, Vec<String>) {
    let mut plugins = Vec::new();
    for (_, _, slot) in insert_slots(state) {
        let Some(id) = hosted_plugin_id(slot) else {
            continue;
        };
        let broken = matches!(
            slot.runtime_state,
            PluginRuntimeState::Missing(_) | PluginRuntimeState::Failed(_)
        ) || plugin_file_missing(slot);
        if broken && !plugins.iter().any(|held: &String| held == id) {
            plugins.push(id.to_string());
        }
    }
    let mut checked: HashMap<&str, bool> = HashMap::new();
    let mut audio = Vec::new();
    for (key, _, path) in audio_clips(state) {
        let missing = *checked
            .entry(path)
            .or_insert_with(|| !Path::new(path).exists());
        if missing && !audio.iter().any(|held: &String| held == key) {
            audio.push(key.to_string());
        }
    }
    (plugins, audio)
}

/// "Vocal, Har b · 3" — the tracks, and how many uses when more than one.
fn used_on(names: &BTreeSet<&str>, count: usize, unit: &str) -> String {
    let names = names.iter().copied().collect::<Vec<_>>().join(", ");
    if count > 1 {
        format!("{names} · {count} {unit}")
    } else {
        names
    }
}

/// Where the scan found `plugin_id` installed in `format`, when that is not
/// `current`.
fn installed_elsewhere(
    catalog: Option<&[SpherePluginHost::RegistryPlugin]>,
    plugin_id: &str,
    format: Option<InsertPluginFormat>,
    current: Option<&Path>,
) -> Option<PathBuf> {
    catalog?
        .iter()
        .filter(|plugin| plugin.class_id.as_deref() == Some(plugin_id) || plugin.id == plugin_id)
        .map(StudioLayout::registry_insert_descriptor)
        .find(|(_, path, found_format, _, _)| {
            Some(*found_format) == format && Some(path.as_path()) != current && path.exists()
        })
        .map(|(_, path, ..)| path)
}

/// The rows for `tracked`, as `state` has them now.
fn missing_rows(
    state: &TimelineState,
    tracked: &MissingTracked,
    catalog: Option<&[SpherePluginHost::RegistryPlugin]>,
) -> Vec<MissingRow> {
    let mut rows = Vec::new();
    for plugin_id in &tracked.plugins {
        let slots: Vec<(&str, &InsertSlotState)> = insert_slots(state)
            .filter(|(_, _, slot)| slot.plugin_id.as_deref() == Some(plugin_id.as_str()))
            .map(|(_, track_name, slot)| (track_name, slot))
            .collect();
        let Some((_, first)) = slots.first() else {
            continue;
        };
        let names: BTreeSet<&str> = slots.iter().map(|(name, _)| *name).collect();
        let failure = slots
            .iter()
            .find_map(|(_, slot)| match &slot.runtime_state {
                PluginRuntimeState::Failed(reason) => Some(reason.clone()),
                PluginRuntimeState::Crashed => Some("The plug-in host crashed".to_string()),
                _ => None,
            });
        let status = if slots.iter().any(|(_, slot)| {
            matches!(slot.runtime_state, PluginRuntimeState::Missing(_))
                || plugin_file_missing(slot)
        }) {
            MissingStatus::Missing
        } else if let Some(reason) = failure {
            MissingStatus::Failed(reason)
        } else if slots.iter().any(|(_, slot)| {
            matches!(
                slot.runtime_state,
                PluginRuntimeState::NotLoaded
                    | PluginRuntimeState::Loading
                    | PluginRuntimeState::Unloaded
            )
        }) {
            MissingStatus::Loading
        } else {
            MissingStatus::Found
        };
        let current = first.plugin_path.as_deref();
        rows.push(MissingRow {
            id: format!("{PLUGIN_ROW}{plugin_id}"),
            kind: MissingKind::Plugin,
            name: first.display_name.clone(),
            detail: first
                .plugin_format
                .map(|format| format.label().to_string())
                .unwrap_or_default(),
            used_on: used_on(&names, slots.len(), "inserts"),
            path: current
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            status,
            installed_at: installed_elsewhere(catalog, plugin_id, first.plugin_format, current)
                .map(|path| path.display().to_string()),
        });
    }
    for asset in &tracked.audio {
        let clips: Vec<(&str, &str)> = audio_clips(state)
            .filter(|(key, _, _)| key == asset)
            .map(|(_, track_name, path)| (track_name, path))
            .collect();
        let Some((_, path)) = clips.first().copied() else {
            continue;
        };
        let names: BTreeSet<&str> = clips.iter().map(|(name, _)| *name).collect();
        let file = Path::new(path);
        rows.push(MissingRow {
            id: format!("{AUDIO_ROW}{asset}"),
            kind: MissingKind::Audio,
            name: file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string()),
            detail: file
                .extension()
                .map(|ext| ext.to_string_lossy().to_uppercase())
                .unwrap_or_default(),
            used_on: used_on(&names, clips.len(), "clips"),
            path: path.to_string(),
            status: if file.exists() {
                MissingStatus::Found
            } else {
                MissingStatus::Missing
            },
            installed_at: None,
        });
    }
    rows
}

/// A file picked inside a VST3 bundle stands for the bundle: the plug-in is
/// the `.vst3` folder, not the binary in its `Contents`.
fn plugin_file_for(picked: PathBuf) -> PathBuf {
    picked
        .ancestors()
        .filter(|ancestor| {
            ancestor.is_dir()
                && ancestor
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("vst3"))
        })
        .last()
        .map(Path::to_path_buf)
        .unwrap_or(picked)
}

/// Walks `root` for files (and `.vst3` bundles) named as in `wanted`
/// (lower-cased), first match each, reading at most [`SEARCH_ENTRY_LIMIT`]
/// entries.
fn find_by_name(root: &Path, wanted: &HashSet<String>) -> HashMap<String, PathBuf> {
    let mut found: HashMap<String, PathBuf> = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    let mut read = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            read += 1;
            if read > SEARCH_ENTRY_LIMIT || found.len() == wanted.len() {
                return found;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_lowercase();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if wanted.contains(&name) && !found.contains_key(&name) {
                found.insert(name, path);
                continue;
            }
            if kind.is_dir() {
                stack.push(path);
            }
        }
    }
    found
}

impl StudioLayout {
    /// Opens the Missing Plug-ins and Files window when the project refers
    /// to anything it cannot find, or brings the open one up to date. Answers
    /// whether anything is missing. `notes` are load problems that are not a
    /// missing file.
    pub(super) fn show_missing_media(
        &mut self,
        notes: Vec<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let (plugins, audio) = scan_missing(&self.timeline.read(cx).state);
        if plugins.is_empty() && audio.is_empty() {
            return false;
        }
        if let Ok(mut tracked) = self.external_windows.missing_media_tracked.lock() {
            tracked.add(plugins, audio);
        }
        let rows = self.missing_media_rows(cx);
        if let Some(handle) = self.external_windows.missing_media {
            if handle
                .update(cx, |view, _window, cx| view.set_rows(rows.clone(), cx))
                .is_ok()
            {
                return true;
            }
            self.external_windows.missing_media = None;
        }

        let studio = cx.weak_entity();
        let callbacks = MissingMediaCallbacks {
            collect: Arc::new({
                let studio = studio.clone();
                move |cx: &mut App| {
                    studio
                        .upgrade()
                        .map(|studio| studio.read(cx).missing_media_rows(cx))
                        .unwrap_or_default()
                }
            }),
            relocate: Arc::new({
                let studio = studio.clone();
                move |row: &str, _window: &mut Window, cx: &mut App| {
                    let row = row.to_string();
                    let _ = studio.update(cx, |layout, cx| layout.relocate_missing(&row, cx));
                }
            }),
            use_installed: Arc::new({
                let studio = studio.clone();
                move |row: &str, _window: &mut Window, cx: &mut App| {
                    let row = row.to_string();
                    let _ = studio.update(cx, |layout, cx| layout.use_installed_plugin(&row, cx));
                }
            }),
            search_folder: Arc::new(move |_window: &mut Window, cx: &mut App| {
                let _ = studio.update(cx, |layout, cx| layout.search_folder_for_missing(cx));
            }),
        };
        let owner_bounds = self.studio_window_bounds(cx);
        match open_missing_media_window(owner_bounds, rows, callbacks, notes, cx) {
            Ok(handle) => self.external_windows.missing_media = Some(handle),
            Err(error) => eprintln!("[missing-media] could not open the window: {error}"),
        }
        true
    }

    /// Forgets what the window followed, for a project just loaded: rows of
    /// the last project's must not turn up as "found" in this one.
    pub(super) fn forget_missing_media(&mut self) {
        if let Ok(mut tracked) = self.external_windows.missing_media_tracked.lock() {
            *tracked = MissingTracked::default();
        }
    }

    /// The window's rows as the project has them now.
    fn missing_media_rows(&self, cx: &App) -> Vec<MissingRow> {
        let Ok(tracked) = self.external_windows.missing_media_tracked.lock() else {
            return Vec::new();
        };
        missing_rows(
            &self.timeline.read(cx).state,
            &tracked,
            self.plugin_catalog.available.as_deref(),
        )
    }

    /// Hands the open window the rows as they are now.
    fn push_missing_media_rows(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.external_windows.missing_media else {
            return;
        };
        let rows = self.missing_media_rows(cx);
        if handle
            .update(cx, |view, _window, cx| view.set_rows(rows, cx))
            .is_err()
        {
            self.external_windows.missing_media = None;
        }
    }

    fn say_in_missing_media(&mut self, status: String, cx: &mut Context<Self>) {
        if let Some(handle) = self.external_windows.missing_media {
            let _ = handle.update(cx, |view, _window, cx| view.set_status(status, cx));
        }
    }

    /// Points every insert of `plugin_id` at `path` and loads it from there.
    fn relink_plugin(&mut self, plugin_id: &str, path: PathBuf, cx: &mut Context<Self>) {
        let slots: Vec<(String, String)> = insert_slots(&self.timeline.read(cx).state)
            .filter(|(_, _, slot)| slot.plugin_id.as_deref() == Some(plugin_id))
            .map(|(track_id, _, slot)| (track_id.to_string(), slot.id.clone()))
            .collect();
        if slots.is_empty() {
            return;
        }
        let bridged = super::plugin_bridge_runtime::bridge_enabled();
        let _ = self.timeline.update(cx, |timeline, _cx| {
            for (track_id, slot_id) in &slots {
                timeline
                    .state
                    .relink_insert_plugin(track_id, slot_id, path.clone());
                if bridged {
                    timeline.state.set_insert_runtime(
                        track_id,
                        slot_id,
                        PluginRuntimeBackend::ExternalBridge,
                        PluginRuntimeState::NotLoaded,
                        None,
                    );
                }
            }
        });
        if bridged {
            for (track_id, slot_id) in &slots {
                self.load_bridge_insert_for_slot(track_id, slot_id, cx);
            }
        }
        self.mark_dirty();
        self.schedule_audio_project_sync(cx, true, "missing_media_relink_plugin");
        self.push_missing_media_rows(cx);
        cx.notify();
    }

    /// Points every clip on audio asset `asset` at `path` and reads it.
    fn relink_audio(&mut self, asset: &str, path: PathBuf, cx: &mut Context<Self>) {
        let path_string = path.to_string_lossy().into_owned();
        let changed = self.timeline.update(cx, |timeline, cx| {
            let changed = timeline.state.retarget_audio_source(asset, &path_string);
            if changed {
                timeline
                    .state
                    .set_audio_import_for_asset(asset, AudioImportState::Pending);
                timeline.mark_media_changed(cx);
            }
            changed
        });
        if !changed {
            return;
        }
        waveform_cache::invalidate_file(&path_string);
        waveform_detail::forget_asset(&path_string);
        waveform_samples::forget_asset(&path_string);
        self.spawn_timeline_audio_import_jobs(cx, self.timeline.clone(), path, path_string);
        self.mark_engine_media_dirty();
        self.mark_dirty();
        self.schedule_audio_project_sync(cx, true, "missing_media_relink_audio");
        self.push_missing_media_rows(cx);
        cx.notify();
    }

    /// Relinks a row's plug-in to where the scan found it installed.
    fn use_installed_plugin(&mut self, row: &str, cx: &mut Context<Self>) {
        let Some(plugin_id) = row.strip_prefix(PLUGIN_ROW) else {
            return;
        };
        let installed = {
            let state = &self.timeline.read(cx).state;
            insert_slots(state)
                .find(|(_, _, slot)| slot.plugin_id.as_deref() == Some(plugin_id))
                .and_then(|(_, _, slot)| {
                    installed_elsewhere(
                        self.plugin_catalog.available.as_deref(),
                        plugin_id,
                        slot.plugin_format,
                        slot.plugin_path.as_deref(),
                    )
                })
        };
        if let Some(path) = installed {
            self.relink_plugin(plugin_id, path, cx);
        }
    }

    /// Asks for a row's file, then relinks it.
    #[cfg(feature = "native-dialogs")]
    fn relocate_missing(&mut self, row: &str, cx: &mut Context<Self>) {
        let (title, filter, extensions, start): (String, &str, Vec<&str>, Option<PathBuf>) =
            if let Some(plugin_id) = row.strip_prefix(PLUGIN_ROW) {
                let state = &self.timeline.read(cx).state;
                let Some((_, _, slot)) = insert_slots(state)
                    .find(|(_, _, slot)| slot.plugin_id.as_deref() == Some(plugin_id))
                else {
                    return;
                };
                let extensions = match slot.plugin_format {
                    Some(InsertPluginFormat::Clap) => vec!["clap"],
                    Some(InsertPluginFormat::Vst2) => vec!["dll", "vst"],
                    _ => vec!["vst3"],
                };
                (
                    format!("Locate {}", slot.display_name),
                    "Plug-in",
                    extensions,
                    slot.plugin_path.as_deref().and_then(existing_ancestor),
                )
            } else if let Some(asset) = row.strip_prefix(AUDIO_ROW) {
                let state = &self.timeline.read(cx).state;
                let Some((_, _, path)) = audio_clips(state).find(|(key, _, _)| *key == asset)
                else {
                    return;
                };
                let name = Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (
                    format!("Locate {name}"),
                    "Audio",
                    AUDIO_EXTENSIONS.to_vec(),
                    existing_ancestor(Path::new(path))
                        .or_else(|| self.project_session.folder_path.clone()),
                )
            } else {
                return;
            };
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title(title)
            .add_filter(filter, &extensions);
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        let row = row.to_string();
        cx.spawn(async move |this, cx| {
            let Some(picked) = dialog.pick_file().await else {
                return;
            };
            let picked = picked.path().to_path_buf();
            let _ = this.update(cx, |layout, cx| {
                if let Some(plugin_id) = row.strip_prefix(PLUGIN_ROW) {
                    // Picked by hand, so loaded even when no scan has seen it
                    // (a project alone never gets that far).
                    let module = plugin_file_for(picked);
                    crate::plugin_module_gate::trust_user_choice(&module);
                    layout.relink_plugin(plugin_id, module, cx);
                } else if let Some(asset) = row.strip_prefix(AUDIO_ROW) {
                    layout.relink_audio(asset, picked, cx);
                }
            });
        })
        .detach();
    }

    #[cfg(not(feature = "native-dialogs"))]
    fn relocate_missing(&mut self, _row: &str, cx: &mut Context<Self>) {
        self.say_in_missing_media(
            "This build has no file dialog to relocate with.".to_string(),
            cx,
        );
    }

    /// Asks for a folder, finds each missing file in it by name, and relinks
    /// what it finds.
    #[cfg(feature = "native-dialogs")]
    fn search_folder_for_missing(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<(String, String)> = self
            .missing_media_rows(cx)
            .into_iter()
            .filter(|row| row.status == MissingStatus::Missing)
            .filter_map(|row| {
                let name = Path::new(&row.path).file_name()?;
                Some((row.id, name.to_string_lossy().to_lowercase()))
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let dialog = rfd::AsyncFileDialog::new().set_title("Search Folder for Missing Files");
        cx.spawn(async move |this, cx| {
            let Some(folder) = dialog.pick_folder().await else {
                return;
            };
            let root = folder.path().to_path_buf();
            let names: HashSet<String> = wanted.iter().map(|(_, name)| name.clone()).collect();
            let _ = this.update(cx, |layout, cx| {
                layout.say_in_missing_media(format!("Searching {}…", root.display()), cx);
            });
            let search_root = root.clone();
            let job = crate::components::background_tasks::BackgroundJob::start(
                crate::components::BackgroundTaskKind::MediaSearch,
                "Search for missing files",
            );
            job.set_detail(root.display().to_string());
            let found = cx
                .background_executor()
                .spawn(async move { find_by_name(&search_root, &names) })
                .await;
            job.complete(Some(format!("Found {} of {}", found.len(), wanted.len())));
            let _ = this.update(cx, |layout, cx| {
                let mut relinked = 0;
                for (row, name) in &wanted {
                    let Some(path) = found.get(name) else {
                        continue;
                    };
                    if let Some(plugin_id) = row.strip_prefix(PLUGIN_ROW) {
                        layout.relink_plugin(plugin_id, plugin_file_for(path.clone()), cx);
                    } else if let Some(asset) = row.strip_prefix(AUDIO_ROW) {
                        layout.relink_audio(asset, path.clone(), cx);
                    }
                    relinked += 1;
                }
                layout.say_in_missing_media(
                    format!(
                        "Found {relinked} of {} missing in {}.",
                        wanted.len(),
                        root.display()
                    ),
                    cx,
                );
            });
        })
        .detach();
    }

    #[cfg(not(feature = "native-dialogs"))]
    fn search_folder_for_missing(&mut self, cx: &mut Context<Self>) {
        self.say_in_missing_media(
            "This build has no folder dialog to search with.".to_string(),
            cx,
        );
    }
}

/// The nearest folder above `path` that exists, to start a file dialog in.
#[cfg(feature = "native-dialogs")]
fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.is_dir())
        .map(Path::to_path_buf)
}

/// Shared by the open window and the studio: see [`MissingTracked`].
pub(crate) type SharedMissingTracked = Arc<Mutex<MissingTracked>>;

/// Load warnings the window lists under its table: everything but a missing
/// plug-in, which has a row of its own.
pub(super) fn missing_media_notes(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .filter(|warning| !warning.starts_with("Missing plugin on "))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_inside_a_vst3_bundle_stands_for_the_bundle() {
        let root = std::env::temp_dir().join(format!("fb-missing-{}", std::process::id()));
        let inner = root
            .join("Saturn 2.vst3")
            .join("Contents")
            .join("x86_64-win");
        std::fs::create_dir_all(&inner).unwrap();
        let binary = inner.join("Saturn 2.vst3");
        std::fs::write(&binary, b"").unwrap();

        assert_eq!(plugin_file_for(binary), root.join("Saturn 2.vst3"));
        let clap = root.join("Saturn 2.clap");
        assert_eq!(plugin_file_for(clap.clone()), clap);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_search_finds_files_and_bundles_by_name() {
        let root = std::env::temp_dir().join(format!("fb-search-{}", std::process::id()));
        std::fs::create_dir_all(root.join("CLAP")).unwrap();
        std::fs::create_dir_all(root.join("VST3").join("Pro-Q 3.vst3").join("Contents")).unwrap();
        std::fs::create_dir_all(root.join("Audio").join("Takes")).unwrap();
        std::fs::write(root.join("CLAP").join("FabFilter Saturn 2.clap"), b"").unwrap();
        std::fs::write(root.join("Audio").join("Takes").join("Vocal 1.wav"), b"").unwrap();

        let wanted: HashSet<String> = ["fabfilter saturn 2.clap", "pro-q 3.vst3", "vocal 1.wav"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let found = find_by_name(&root, &wanted);

        assert_eq!(found.len(), 3);
        assert_eq!(
            found["fabfilter saturn 2.clap"],
            root.join("CLAP").join("FabFilter Saturn 2.clap")
        );
        assert_eq!(
            found["pro-q 3.vst3"],
            root.join("VST3").join("Pro-Q 3.vst3")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_audio_is_one_row_per_asset_and_found_once_its_file_is_there() {
        use crate::components::timeline::timeline_state::{
            CreateTrackOptions, InputMonitorMode, TrackType,
        };
        let mut state = TimelineState::default();
        state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Vocal".to_string(),
            color: gpui::rgba(0x336699ff),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let path = std::env::temp_dir().join(format!("fb-missing-take-{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let clip = |id: &str| crate::components::timeline::timeline_state::ClipState {
            id: id.to_string(),
            name: "Take".to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "asset-1".to_string(),
                source_path: Some(path.to_string_lossy().into_owned()),
            },
            muted: false,
            audio_import: AudioImportState::Pending,
            stretch: Default::default(),
        };
        state.tracks[0].clips = vec![clip("clip-1"), clip("clip-2")];

        let (plugins, audio) = scan_missing(&state);
        assert!(plugins.is_empty());
        assert_eq!(audio, vec!["asset-1".to_string()]);
        let mut tracked = MissingTracked::default();
        tracked.add(plugins, audio);
        let rows = missing_rows(&state, &tracked, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, MissingStatus::Missing);
        assert!(rows[0].used_on.ends_with("· 2 clips"));

        std::fs::write(&path, b"").unwrap();
        let rows = missing_rows(&state, &tracked, None);
        assert_eq!(
            rows[0].status,
            MissingStatus::Found,
            "still listed, now found"
        );
        let _ = std::fs::remove_file(&path);
    }
}
