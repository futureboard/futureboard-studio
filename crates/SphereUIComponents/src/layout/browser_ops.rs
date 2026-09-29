use gpui::Context;

use std::path::{Path, PathBuf};

use crate::components::file_browser::read_directory;

use super::StudioLayout;
impl StudioLayout {
    /// Ask the engine to audition (preview-play) a browser audio file.
    ///
    /// The Browser keeps audition controls hidden until this path can produce
    /// audible output. Returns whether audio is actually audible.
    pub(crate) fn audition_browser_file(&mut self, path: &Path) -> bool {
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            eprintln!("[browser-preview] no engine; cannot audition");
            return false;
        };
        match engine.audition_file(path.to_string_lossy().into_owned()) {
            Ok(audible) => {
                if audible {
                    self.file_browser.set_preview_playing(path.to_path_buf());
                }
                audible
            }
            Err(error) => {
                eprintln!("[browser-preview] audition error: {error}");
                false
            }
        }
    }

    /// Stop the preview voice and drop the playhead it owned.
    ///
    /// The engine has always exposed this; nothing in the UI called it, so a
    /// browser audition could only be ended by waiting it out.
    pub(crate) fn stop_browser_audition(&mut self) {
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            if let Err(error) = engine.stop_audition() {
                eprintln!("[browser-preview] stop error: {error}");
            }
        }
        self.file_browser.apply_preview_position(None);
    }

    /// The one browser "selection changed" operation.
    ///
    /// Mouse clicks and arrow keys both route through here, so keyboard
    /// navigation decodes peaks and auditions exactly like a click does, and
    /// the newly selected row is always scrolled into view.
    pub(crate) fn apply_browser_selection(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.file_browser.select(path.clone());
        if let Some(index) = self.file_browser.index_of_selected() {
            self.browser_scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
        if crate::components::file_browser::is_audio_path(&path) {
            // Visual mini-waveform preview always decodes on select.
            self.ensure_browser_waveform(path.clone(), cx);
            // Browser selection is also a real audible audition; decode happens
            // off-thread in the engine, so this UI event only queues work and
            // never blocks rendering.
            if self.file_browser.preview_enabled {
                let _ = self.audition_browser_file(&path);
            }
        }
    }

    /// Dispatch a background scan for every expanded folder that has no cached
    /// listing yet. `paths_needing_load` already skips folders that failed, so
    /// one unreadable directory no longer re-queues a scan on every keystroke.
    pub(crate) fn drain_browser_directory_loads(&mut self, cx: &mut Context<Self>) {
        let pending = self.file_browser.paths_needing_load();
        for p in pending {
            self.file_browser.mark_loading(p.clone());
            self.spawn_directory_load(cx, p);
        }
    }

    /// Expand every ancestor of `path` and select it — the breadcrumb jump.
    pub(crate) fn reveal_browser_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.file_browser.reveal_path(&path);
        self.drain_browser_directory_loads(cx);
        self.apply_browser_selection(path, cx);
    }

    /// Pull the Browser preview playhead published by the render callback into
    /// the browser state. Returns whether the preview pane needs a repaint.
    ///
    /// One relaxed atomic read per UI poll — the engine owns the position, so
    /// the pane never runs its own clock and can never drift from the audio.
    pub(crate) fn poll_browser_preview_playhead(&mut self) -> bool {
        let position = self
            .audio_bridge
            .engine
            .as_ref()
            .and_then(|engine| engine.audition_position_seconds())
            .map(|seconds| seconds as f32);
        self.file_browser.apply_preview_position(position)
    }

    /// Ensure the mini waveform peaks for `path` are decoded for the preview
    /// pane. Decode runs on the background executor (never in render); the
    /// result lands in the shared waveform cache the sidebar reads from. Cached
    /// or already-in-flight files are skipped.
    pub(crate) fn ensure_browser_waveform(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        use crate::components::timeline::waveform_cache;
        let key = path.to_string_lossy().to_string();
        if waveform_cache::get_preview_arc(&key).is_some() {
            return; // already decoded
        }
        if !self.file_browser.begin_waveform_load(path.clone()) {
            return; // decode already running
        }
        let decode_path = path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { waveform_cache::decode_and_cache_file(&decode_path).is_some() })
                .await;
            let _ = this.update(cx, move |this, cx| {
                this.file_browser.end_waveform_load(&path);
                if !result {
                    // `decode_and_cache_file` writes *nothing* to the shared
                    // cache when it fails, so `get_file_status` can never
                    // report the failure — without recording it here the
                    // preview pane says "Decoding waveform…" forever.
                    eprintln!(
                        "[browser-preview] waveform decode failed path={}",
                        path.display()
                    );
                    this.file_browser.mark_waveform_failed(path);
                }
                cx.notify();
            });
        })
        .detach();
    }
    /// Read the Instruments tab's list from the plug-in database, off the UI
    /// thread. Only the database: an instrument it does not list — or lists
    /// without a `.pst` — is not offered.
    pub(crate) fn spawn_browser_instruments_load(&mut self, cx: &mut Context<Self>) {
        if self.file_browser.instruments_load
            == crate::components::file_browser::InstrumentsLoad::Loading
        {
            return;
        }
        self.file_browser.mark_instruments_loading();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    use SpherePluginHost::plugin_db;
                    // No database yet is not an error: nothing has been scanned.
                    if !plugin_db::database_exists() {
                        return Ok(Vec::new());
                    }
                    let conn = plugin_db::open_database_readonly()?;
                    // Rows from before the database recorded each `.pst` find
                    // theirs in the scan cache by plug-in id; the cache is read
                    // once, and only if such a row exists.
                    let mut cache: Option<std::collections::HashMap<String, PathBuf>> = None;
                    plugin_db::instrument_presets(&conn, |id| {
                        cache
                            .get_or_insert_with(|| {
                                SpherePluginHost::preset::load_cached_plugins()
                                    .into_iter()
                                    .map(|p| (p.id, p.preset_path))
                                    .collect()
                            })
                            .get(id)
                            .cloned()
                    })
                    .map_err(|error| error.to_string())
                    .map(|list| {
                        list.into_iter()
                            .map(|i| crate::components::file_browser::BrowserInstrument {
                                name: i.name,
                                vendor: i.vendor,
                                format: i.format.label().to_string(),
                                category: i.category,
                                preset_path: i.preset_path,
                            })
                            .collect()
                    })
                })
                .await;
            let _ = this.update(cx, move |this, cx| {
                if let Err(error) = &result {
                    eprintln!("[browser] instrument list failed: {error}");
                }
                this.file_browser.apply_instruments(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a single-level directory scan on the GPUI background executor,
    /// then push the result back into `file_browser.index` on the UI
    /// thread. Never blocks render — this is the only place `read_dir`
    /// is allowed to happen at runtime.
    pub(super) fn spawn_directory_load(&mut self, cx: &mut Context<Self>, path: PathBuf) {
        let started = std::time::Instant::now();
        let path_for_log = path.clone();
        let task_id = format!("metadata-scan:{}", path_for_log.to_string_lossy());
        eprintln!("[indexer] load requested: {}", path_for_log.display());
        self.start_background_task(
            task_id.clone(),
            crate::components::BackgroundTaskKind::MetadataScan,
            "Scan browser folder",
            Some(path_for_log.to_string_lossy().to_string()),
            None,
            false,
        );
        cx.spawn(async move |this, cx| {
            let scan_path = path.clone();
            let task_id_for_update = task_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { read_directory(&scan_path) })
                .await;
            let elapsed = started.elapsed();
            let _ = this.update(cx, move |this, cx| {
                match result {
                    (entries, None) => {
                        eprintln!(
                            "[indexer] load completed: {} ({} entries, {} ms)",
                            path.display(),
                            entries.len(),
                            elapsed.as_millis()
                        );
                        this.file_browser.apply_loaded(path, entries);
                        this.complete_background_task(
                            &task_id_for_update,
                            Some(format!("{} ms", elapsed.as_millis())),
                        );
                    }
                    (_, Some(error)) => {
                        eprintln!(
                            "[indexer] load failed: {} -> {} ({} ms)",
                            path.display(),
                            error,
                            elapsed.as_millis()
                        );
                        this.fail_background_task(&task_id_for_update, error.clone());
                        this.file_browser.apply_error(path, error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
