//! ARA commands on the studio: bind, unbind, edit, sync, save, restore.
//!
//! [`super::ara_ops::AraState`] owns the sessions; this is where the rest of the
//! app reaches them. Every entry point keeps the four things that must agree in
//! step — the clip's binding, the ARA document, the engine's renderer list, and
//! the project snapshot — so no caller has to know the order.

use std::path::Path;

use gpui::Context;
use sphere_ara_host::{AraModelUpdate, AraTransportRequest};

use super::ara_graph;
use super::ara_ops::AraSessionKey;
use super::StudioLayout;
use crate::project::{FutureboardProject, ProjectAraDocument};

/// One ARA-capable plug-in, as the menus present it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AraPluginChoice {
    pub id: String,
    pub name: String,
    pub path: String,
    pub class_id: String,
}

/// How long the teardown waits for the plug-in's editor view to come down
/// before closing its document regardless.
///
/// Sixteen frames was not a budget, it was a guess, and it was short of what
/// a real editor needs: `remove_window` only *asks* for the window, the
/// close runs on the next turn of the platform's message loop, and
/// `IPlugView::removed()` inside a large editor (Melodyne rebuilding its
/// analysis view, say) is not a 16 ms call. Missing the window meant the
/// document was destroyed under a live view, which is the crash-or-hang this
/// whole sequence exists to avoid.
///
/// Two seconds is long enough for that to finish and short enough that a
/// plug-in which is never going to let go does not look like a permanent
/// hang. Polling continues to be cheap: it is one atomic read per frame.
const VIEW_RELEASE_POLLS: u32 = 120;
const VIEW_RELEASE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

impl StudioLayout {
    /// ARA-capable plug-ins from the scanned catalog, sorted for display.
    ///
    /// Empty when ARA is unavailable on this platform, so the menus simply have
    /// nothing to offer rather than listing entries that would fail on click.
    pub(crate) fn ara_plugin_choices(&self) -> Vec<AraPluginChoice> {
        if !super::ara_ops::AraState::is_supported() {
            return Vec::new();
        }
        let Some(available) = self.plugin_catalog.available.as_ref() else {
            return Vec::new();
        };
        let mut choices: Vec<AraPluginChoice> = available
            .iter()
            .filter(|plugin| plugin.supports_ara())
            .filter_map(|plugin| {
                Some(AraPluginChoice {
                    id: plugin.id.clone(),
                    name: plugin.name.clone(),
                    path: plugin.path.to_string_lossy().to_string(),
                    class_id: plugin.class_id.clone()?,
                })
            })
            .collect();
        choices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        choices.dedup_by(|a, b| a.id == b.id);
        choices
    }

    /// The plug-in processing `track_id`, if any.
    pub(crate) fn ara_binding_for_track(
        &self,
        track_id: &str,
        cx: &gpui::App,
    ) -> Option<(String, String)> {
        let state = &self.timeline.read(cx).state;
        let track = state.tracks.iter().find(|track| track.id == track_id)?;
        let binding = track.ara.as_ref()?;
        let name = self
            .ara_plugin_choices()
            .into_iter()
            .find(|choice| choice.id == binding.plugin_id)
            .map(|choice| choice.name)
            .unwrap_or_else(|| binding.plugin_id.clone());
        Some((binding.plugin_id.clone(), name))
    }

    /// The plug-in processing the track that owns `clip_id`.
    pub(crate) fn ara_binding_for_clip(
        &self,
        clip_id: &str,
        cx: &gpui::App,
    ) -> Option<(String, String)> {
        let track_id = self.track_of_clip(clip_id, cx)?;
        self.ara_binding_for_track(&track_id, cx)
    }

    /// Track that owns `clip_id`.
    fn track_of_clip(&self, clip_id: &str, cx: &gpui::App) -> Option<String> {
        let state = &self.timeline.read(cx).state;
        state
            .tracks
            .iter()
            .find(|track| track.clips.iter().any(|clip| clip.id == clip_id))
            .map(|track| track.id.clone())
    }

    /// Whether a track carries at least one audio clip for ARA to work on.
    fn track_has_audio(&self, track_id: &str, cx: &gpui::App) -> bool {
        let state = &self.timeline.read(cx).state;
        state
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .is_some_and(|track| {
                track.clips.iter().any(|clip| {
                    matches!(
                        clip.clip_type,
                        crate::components::timeline::timeline_state::ClipType::Audio { .. }
                    )
                })
            })
    }

    /// Hands a whole track to an ARA plug-in and brings every layer in step.
    ///
    /// ARA is a track processor: the plug-in takes the track, and every audio
    /// clip on it becomes one of its playback regions. Replacing an existing
    /// binding tears the old session down first — one track, one ARA plug-in.
    pub(crate) fn bind_track_to_ara(
        &mut self,
        track_id: &str,
        plugin_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(choice) = self
            .ara_plugin_choices()
            .into_iter()
            .find(|choice| choice.id == plugin_id)
        else {
            self.ara.last_error = Some("That ARA plug-in is no longer available.".to_string());
            cx.notify();
            return;
        };
        if let Some((current, _)) = self.ara_binding_for_track(track_id, cx) {
            if current == plugin_id {
                return;
            }
            self.unbind_track_from_ara(track_id, cx);
        }

        let binding = crate::components::timeline::timeline_state::AraTrackBinding {
            plugin_id: choice.id.clone(),
            plugin_path: choice.path.clone(),
            class_id: choice.class_id.clone(),
        };
        let bound = self.timeline.update(cx, |timeline, cx| {
            let Some(track) = timeline
                .state
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
            else {
                return false;
            };
            track.ara = Some(binding);
            cx.notify();
            true
        });
        if !bound {
            return;
        }

        let key = AraSessionKey {
            plugin_id: choice.id.clone(),
            track_id: track_id.to_string(),
        };
        self.sync_ara_session(&key, &choice, cx);
        // The engine must stop mixing this track's clip files in the same pass,
        // or every clip plays twice until the next unrelated sync.
        self.mark_engine_project_dirty();
        // Binding an ARA plug-in *is* the request to work on the track, so its
        // editor comes up with it instead of costing a second trip through the
        // context menu. Only when the session really started: a failed bind has
        // already put its reason in `last_error`, and a panel opened on nothing
        // would bury it behind an empty rectangle.
        if self.ara.processor(&key).is_some() {
            if let Some(clip_id) = self.first_audio_clip_of_track(track_id, cx) {
                self.open_ara_editor(&clip_id, cx);
            }
        }
        cx.notify();
    }

    /// Earliest audio clip on a track, which is what the ARA editor opens on.
    ///
    /// The panel resolves its target from the clip selection, so auto-opening
    /// needs a clip to select; the earliest one is the one the user is looking
    /// at when they bind the track.
    fn first_audio_clip_of_track(&self, track_id: &str, cx: &gpui::App) -> Option<String> {
        let state = &self.timeline.read(cx).state;
        let track = state.tracks.iter().find(|track| track.id == track_id)?;
        track
            .clips
            .iter()
            .filter(|clip| {
                matches!(
                    clip.clip_type,
                    crate::components::timeline::timeline_state::ClipType::Audio { .. }
                )
            })
            .min_by(|a, b| {
                a.start_beat
                    .partial_cmp(&b.start_beat)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|clip| clip.id.clone())
    }

    /// Removes a track's ARA plug-in and closes its session.
    ///
    /// The session outlives the editor on purpose. A plug-in's view draws from
    /// the ARA document, and neither host of that view lets go synchronously —
    /// the docked panel tears down on a deferred tick (`DestroyWindow`
    /// re-enters GPUI, so it cannot run inline) and a popped-out window
    /// releases its view when GPUI actually removes it. Destroying the document
    /// while a view is still open on it takes the app down, so the close is
    /// pushed past the detach handshake, the same way [`Self::pop_out_ara_editor`]
    /// waits before asking the controller for a second view.
    pub(crate) fn unbind_track_from_ara(&mut self, track_id: &str, cx: &mut Context<Self>) {
        let Some((plugin_id, _)) = self.ara_binding_for_track(track_id, cx) else {
            return;
        };
        // Cleared first: while the track still names a plug-in, the next layout
        // render would re-target the panel at the session being torn down and
        // re-attach the view behind the teardown.
        self.timeline.update(cx, |timeline, cx| {
            if let Some(track) = timeline
                .state
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
            {
                track.ara = None;
                cx.notify();
            }
        });

        let key = AraSessionKey {
            plugin_id,
            track_id: track_id.to_string(),
        };
        self.close_ara_editor(&key, cx);
        self.ara_editor_popped_out = false;
        self.ara_editor
            .update(cx, |host, cx| host.request_detach(cx));
        self.mark_engine_project_dirty();
        cx.notify();

        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let mut released = false;
            for _ in 0..VIEW_RELEASE_POLLS {
                // Both owners, not just the docked one. A popped-out editor
                // holds its view in a window of its own, and the docked host
                // reports "not attached" for the whole time it is popped out —
                // so waiting on that alone let the very first poll answer
                // "released" and destroy the document under the window that
                // still had a view on it.
                released = this
                    .update(cx, |layout, cx| {
                        !layout.ara_editor.read(cx).is_attached()
                            && !layout.ara.view_is_attached(&key)
                    })
                    .unwrap_or(true);
                if released {
                    break;
                }
                executor.timer(VIEW_RELEASE_POLL_INTERVAL).await;
            }
            if !released {
                // Said out loud, and not behind a debug flag. Closing the
                // document while a view is still on it is the state that hangs
                // or crashes the app, and the teardown below is about to do it
                // anyway — `AraSessions::close` detaches the view itself first,
                // but if that call is the one that never returns, this is the
                // line that says the app was already in the bad state before it
                // started. A freeze report with this line above it and no
                // `[ara-close] ... done` line after it names the culprit
                // exactly; without it the log looks like a clean teardown that
                // simply stopped.
                eprintln!(
                    "[ara-close] WARNING: '{}' still holds its editor view after {:?}; \
                     closing the document anyway",
                    key.plugin_id,
                    VIEW_RELEASE_POLL_INTERVAL * VIEW_RELEASE_POLLS,
                );
            }
            let _ = this.update(cx, |layout, cx| {
                let Some(engine) = layout.audio_bridge.engine.clone() else {
                    return;
                };
                layout.ara.close(&engine, &key);
                cx.notify();
            });
        })
        .detach();
    }

    /// Rebuilds and re-applies the ARA document for one session.
    fn sync_ara_session(
        &mut self,
        key: &AraSessionKey,
        choice: &AraPluginChoice,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.audio_bridge.engine.clone() else {
            self.ara.last_error =
                Some("The audio engine is not running, so ARA cannot start.".to_string());
            return;
        };
        let state = self.timeline.read(cx).state.clone();
        let timeline = ara_graph::musical_timeline(&state);
        // Probe every source through the engine's own decoder, so the plug-in is
        // told the file's real rate and channel count rather than the project's.
        let mut shape_of = |path: &str| match DirectAudio::probe_audio_file(Path::new(path)) {
            Ok(info) => Some((
                info.sample_rate as f64,
                info.total_frames as i64,
                info.channels as i32,
            )),
            Err(error) => {
                ara_trace(&format!("probe failed for '{path}': {error}"));
                None
            }
        };
        let view = ara_graph::project_view(&state, key, &mut shape_of);
        ara_trace(&format!(
            "graph: sources={} regions={} sequences={} media={}",
            view.graph.sources.len(),
            view.graph.regions.len(),
            view.graph.sequences.len(),
            view.media_paths.len()
        ));

        if let Err(error) = self.ara.apply(
            &engine,
            key,
            &choice.name,
            &choice.path,
            &choice.class_id,
            &timeline,
            &view.graph,
            view.media_paths,
        ) {
            self.ara.last_error = Some(format!("{}: {error}", choice.name));
            eprintln!("[ARA] {} failed: {error}", choice.name);
            return;
        }

        // ARA has no host-to-plug-in transport call: the plug-in reads the
        // host's position and playing state out of the process context it is
        // handed per block. Until the stream is open it is handed none, so its
        // editor playhead sits wherever it was left and its own play button
        // toggles against a stale transport state. Warming here means a freshly
        // bound plug-in is in step before the first Play instead of after it.
        // Failure is not fatal to the binding — the document is already applied,
        // and `ensure_audio_stream_warm` has reported why.
        let _ = self.ensure_audio_stream_warm();
    }

    /// Re-applies every live ARA session against the current project.
    ///
    /// Called after edits that move or retime bound clips: the plug-in's regions
    /// have to follow the arrangement or it renders at the old position.
    pub(crate) fn refresh_ara_sessions(&mut self, cx: &mut Context<Self>) {
        if !self.ara.is_active() {
            return;
        }
        let keys: Vec<AraSessionKey> = self.ara.keys().cloned().collect();
        let choices = self.ara_plugin_choices();
        for key in keys {
            // A session whose track no longer names it is on its way out — see
            // `unbind_track_from_ara`, which clears the binding first and closes
            // the session once the plug-in's view has let go. Re-applying it
            // here would edit a model that is being torn down.
            if self
                .ara_binding_for_track(&key.track_id, cx)
                .is_none_or(|(plugin_id, _)| plugin_id != key.plugin_id)
            {
                continue;
            }
            if let Some(choice) = choices.iter().find(|choice| choice.id == key.plugin_id) {
                let choice = choice.clone();
                self.sync_ara_session(&key, &choice, cx);
            }
        }
    }

    /// Opens the bound plug-in's editor for a clip.
    /// Shows the bound plug-in's editor in the docked Editor panel.
    ///
    /// ARA's editor lives in the dock, not in a window of its own: a plug-in
    /// exposes one `IEditController`, and one controller opens one view, so the
    /// panel and a floating window can never both host it. The window is the
    /// pop-out mode only — see [`Self::pop_out_ara_editor`].
    ///
    /// The clip is selected first because the panel resolves its target from
    /// the selection; a right-click on an unselected clip would otherwise open
    /// the panel on whatever else happened to be selected.
    pub(crate) fn open_ara_editor(&mut self, clip_id: &str, cx: &mut Context<Self>) {
        ara_trace("open_ara_editor (dock)");
        let Some((plugin_id, plugin_name)) = self.ara_binding_for_clip(clip_id, cx) else {
            ara_trace("open_ara_editor: clip has no ARA binding");
            return;
        };
        let Some(track_id) = self.track_of_clip(clip_id, cx) else {
            ara_trace("open_ara_editor: clip has no track");
            return;
        };
        let key = AraSessionKey {
            plugin_id,
            track_id,
        };
        if self.ara.processor(&key).is_none() {
            self.ara.last_error = Some(format!("{plugin_name} is not running for this clip."));
            cx.notify();
            return;
        }
        if self.ara_editor_popped_out {
            // Already in its own window; bring that forward instead of trying
            // to open a second view of the same controller.
            self.open_ara_editor_window(clip_id, cx);
            return;
        }
        self.timeline.update(cx, |timeline, cx| {
            timeline.state.select_clip(clip_id);
            cx.notify();
        });
        self.panels.bottom_docked = true;
        self.set_active_bottom_tab(crate::components::BottomTab::Editor, cx);
        cx.notify();
    }

    /// Opens the bound plug-in's editor in its own window (pop-out mode).
    ///
    /// Answers whether a window is now open for it — including "already was",
    /// which is still the editor being where it should be. The caller uses this
    /// to decide whether the panel may keep claiming the editor is popped out.
    fn open_ara_editor_window(&mut self, clip_id: &str, cx: &mut Context<Self>) -> bool {
        ara_trace("open_ara_editor_window (pop-out)");
        let Some((plugin_id, plugin_name)) = self.ara_binding_for_clip(clip_id, cx) else {
            return false;
        };
        let Some(track_id) = self.track_of_clip(clip_id, cx) else {
            return false;
        };
        let key = AraSessionKey {
            plugin_id,
            track_id,
        };
        let Some(processor) = self.ara.processor(&key) else {
            self.ara.last_error = Some(format!("{plugin_name} is not running for this clip."));
            cx.notify();
            return false;
        };
        let editor_key = ara_editor_key(&key);
        if self.plugin_editors.open.contains_key(&editor_key) {
            return true;
        }
        let owner_bounds = match self.studio_window_bounds(cx) {
            Some(bounds) => bounds,
            None => return false,
        };
        match crate::components::plugin_editor_window::open_plugin_editor_window(
            owner_bounds,
            key.track_id.clone(),
            ara_insert_id(&key.plugin_id),
            plugin_name,
            Some(processor),
            None,
            // ARA instances are hosted here, never behind the bridge.
            true,
            cx,
        ) {
            Ok(handle) => {
                self.plugin_editors.open.insert(editor_key, handle);
                true
            }
            Err(error) => {
                self.ara.last_error = Some(format!("ARA editor could not open: {error}"));
                cx.notify();
                false
            }
        }
    }

    fn close_ara_editor(&mut self, key: &AraSessionKey, cx: &mut Context<Self>) {
        if let Some(handle) = self.plugin_editors.open.remove(&ara_editor_key(key)) {
            let _ = handle.update(cx, |_editor, window, _cx| window.remove_window());
        }
    }

    /// Applies whatever the ARA plug-ins posted since the last frame.
    pub(crate) fn poll_ara(&mut self, cx: &mut Context<Self>) {
        // A popped-out editor closed from its own title bar leaves no signal
        // here; without this the panel would stay blank and the button would
        // stay stuck on "Dock".
        //
        // Not while an open is still in flight. Opening is deferred behind the
        // docked view's detach, and for those frames "popped out with no window"
        // is the normal state of an editor on its way up — cancelling it here is
        // why the window never appeared.
        if self.ara_editor_popped_out
            && !self.ara_editor_open_pending
            && !self.ara_editor_window_open(cx)
        {
            self.ara_editor_popped_out = false;
            cx.notify();
        }
        if !self.ara.is_active() {
            return;
        }
        for request in self.ara.take_transport_requests() {
            // Paired with the trace on the ARA host side, this says whether a
            // request the plug-in made actually reached the transport.
            ara_trace(&format!("applying transport request {request:?}"));
            match request {
                AraTransportRequest::Start => self.start_native_playback(cx),
                AraTransportRequest::Stop => self.stop_native_playback(cx),
                AraTransportRequest::SetPosition(seconds) => {
                    let beat = self
                        .timeline
                        .read(cx)
                        .state
                        .seconds_to_beats(seconds.max(0.0));
                    self.seek_native_playhead(cx, beat);
                }
                // Cycle changes arrive as seconds; the loop range is authored in
                // beats, so they are applied through the same conversion the
                // ruler uses rather than written to the engine directly.
                AraTransportRequest::SetCycleRange { start, duration } => {
                    self.timeline.update(cx, |timeline, cx| {
                        let state = &mut timeline.state;
                        state.transport.loop_start_beats = state.seconds_to_beats(start.max(0.0));
                        state.transport.loop_end_beats =
                            state.seconds_to_beats((start + duration).max(0.0));
                        cx.notify();
                    });
                    self.sync_loop_controls(cx);
                }
                AraTransportRequest::EnableCycle(enabled) => {
                    self.timeline.update(cx, |timeline, cx| {
                        timeline.state.transport.loop_enabled = enabled;
                        cx.notify();
                    });
                    self.sync_loop_controls(cx);
                }
            }
        }

        let mut dirty = false;
        for update in self.ara.take_model_updates() {
            match update {
                // The plug-in changed its own persistent state, so the project
                // has unsaved work even though nothing in the timeline moved.
                AraModelUpdate::DocumentDataChanged => dirty = true,
                AraModelUpdate::AnalysisProgress { .. }
                | AraModelUpdate::SourceContentChanged { .. }
                | AraModelUpdate::ModificationContentChanged { .. }
                | AraModelUpdate::RegionContentChanged { .. } => {}
            }
        }
        if dirty {
            self.project_session.mark_dirty();
            cx.notify();
        }
    }

    /// Writes every live ARA document into the project being saved.
    pub(crate) fn attach_ara_archives(&mut self, project: &mut FutureboardProject) {
        project.ara_documents = self
            .ara
            .store_archives()
            .into_iter()
            .map(|(key, archive_id, data)| ProjectAraDocument {
                plugin_id: key.plugin_id,
                track_id: key.track_id,
                archive_id,
                data,
            })
            .collect();
    }

    /// Parks a loaded project's ARA archives and opens the sessions its clips
    /// are bound to.
    ///
    /// Restoring runs inside the session open, before regions are assigned and
    /// before playback, which is where ARA requires it.
    pub(crate) fn restore_ara_archives(
        &mut self,
        project: &FutureboardProject,
        cx: &mut Context<Self>,
    ) {
        if let Some(engine) = self.audio_bridge.engine.clone() {
            self.ara.close_all(&engine);
        }
        self.ara
            .load_archives(project.ara_documents.iter().map(|document| {
                (
                    AraSessionKey {
                        plugin_id: document.plugin_id.clone(),
                        track_id: document.track_id.clone(),
                    },
                    document.archive_id.clone(),
                    document.data.clone(),
                )
            }));

        // One session per ARA track. A parked archive whose track no longer has
        // that plug-in stays parked and is saved back untouched rather than
        // silently discarded.
        let mut wanted: Vec<AraSessionKey> = Vec::new();
        for track in &project.tracks {
            if let Some(binding) = track.ara.as_ref() {
                let key = AraSessionKey {
                    plugin_id: binding.plugin_id.clone(),
                    track_id: track.id.clone(),
                };
                if !wanted.contains(&key) {
                    wanted.push(key);
                }
            }
        }
        let choices = self.ara_plugin_choices();
        for key in wanted {
            match choices.iter().find(|choice| choice.id == key.plugin_id) {
                Some(choice) => {
                    let choice = choice.clone();
                    self.sync_ara_session(&key, &choice, cx);
                }
                None => {
                    self.ara.last_error = Some(format!(
                        "This project uses an ARA plug-in that is not installed ({}). Its clips \
                         play from their source files until it is available again.",
                        key.plugin_id
                    ));
                }
            }
        }
    }
}

/// Insert id used for an ARA instance's editor window.
///
/// ARA instances are not inserts, but the editor window is keyed by
/// `track::insert`, so they get a reserved namespace that cannot collide with a
/// real insert id.
fn ara_insert_id(plugin_id: &str) -> String {
    format!("ara:{plugin_id}")
}

fn ara_editor_key(key: &AraSessionKey) -> (String, String) {
    (key.track_id.clone(), ara_insert_id(&key.plugin_id))
}

// ── Docked editor panel ──────────────────────────────────────────────────────

impl StudioLayout {
    /// The ARA session the Editor panel should be showing, if any.
    ///
    /// One predicate, read by both the panel router and the layout's own
    /// visibility pass, so the embedded view and the panel that reserves space
    /// for it can never disagree about whether it is on screen.
    pub(crate) fn ara_panel_target(&self, cx: &gpui::App) -> Option<AraSessionKey> {
        if self.ara_editor_popped_out {
            // The plug-in owns its own window right now; the panel must not park
            // a second view over the dock.
            return None;
        }
        self.selected_ara_session_key(cx)
    }

    /// Session key for the track the selected clip sits on, when it has ARA.
    fn selected_ara_session_key(&self, cx: &gpui::App) -> Option<AraSessionKey> {
        let state = &self.timeline.read(cx).state;
        let clip_id = state.selection.selected_clip_ids.first()?;
        let (track, _) = state.find_clip(clip_id)?;
        let binding = track.ara.as_ref()?;
        Some(AraSessionKey {
            plugin_id: binding.plugin_id.clone(),
            track_id: track.id.clone(),
        })
    }

    /// The live plug-in instance for one session, for the embedded editor.
    pub(crate) fn ara_processor(
        &self,
        key: &AraSessionKey,
    ) -> Option<DirectAudio::Vst3RuntimeProcessor> {
        self.ara.processor(key)
    }

    /// The last thing an ARA session complained about.
    ///
    /// Sessions fail on the control thread — a plug-in that will not load, an
    /// engine that was not running — long before anything is drawn, so the panel
    /// reads this to explain itself instead of showing an empty region.
    pub(crate) fn ara_last_error(&self) -> Option<String> {
        self.ara.last_error.clone()
    }

    /// Display name of an ARA plug-in from the scanned catalog.
    pub(crate) fn ara_plugin_name(&self, plugin_id: &str) -> Option<String> {
        self.ara_plugin_choices()
            .into_iter()
            .find(|choice| choice.id == plugin_id)
            .map(|choice| choice.name)
    }

    /// Whether the Editor tab currently has an ARA plug-in to show.
    pub(crate) fn ara_editor_panel_active(&self, cx: &gpui::App) -> bool {
        self.ara_panel_target(cx).is_some()
    }

    /// Whether the bound plug-in is currently in its own window.
    pub(crate) fn ara_editor_is_popped_out(&self) -> bool {
        self.ara_editor_popped_out
    }

    /// Moves the bound plug-in's editor out of the dock into its own window.
    ///
    /// The panel view is released first: two live views of one `IEditController`
    /// is not something a plug-in has to tolerate.
    pub(crate) fn pop_out_ara_editor(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.selected_ara_clip_id(cx) else {
            return;
        };
        self.ara_editor
            .update(cx, |host, cx| host.request_detach(cx));
        self.ara_editor_popped_out = true;
        self.ara_editor_open_pending = true;
        // The docked view comes down on a deferred tick, and the plug-in has
        // only one view to give: opening the window before that detach lands
        // would ask the same controller for a second one. Wait for the panel to
        // let go, then open.
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            for _ in 0..16 {
                let released = this
                    .update(cx, |layout, cx| !layout.ara_editor.read(cx).is_attached())
                    .unwrap_or(true);
                if released {
                    break;
                }
                executor.timer(std::time::Duration::from_millis(16)).await;
            }
            let _ = this.update(cx, |layout, cx| {
                // Still wanted? A pop-in during the wait cancels this.
                let wanted = layout.ara_editor_popped_out;
                let opened = wanted && layout.open_ara_editor_window(&clip_id, cx);
                layout.ara_editor_open_pending = false;
                if wanted && !opened {
                    // The editor did not open, so the panel must not go on
                    // claiming it is in a window somewhere.
                    layout.ara_editor_popped_out = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Returns the popped-out editor to the dock.
    pub(crate) fn pop_in_ara_editor(&mut self, cx: &mut Context<Self>) {
        // Whatever the deferred open was about to do, the user has changed
        // their mind — its own guard reads this.
        self.ara_editor_open_pending = false;
        let Some(key) = self.ara_editor_window_key(cx) else {
            self.ara_editor_popped_out = false;
            cx.notify();
            return;
        };
        self.close_ara_editor(&key, cx);
        self.ara_editor_popped_out = false;
        // The dock has to be showing for the view to have somewhere to go.
        self.panels.bottom_docked = true;
        self.set_active_bottom_tab(crate::components::BottomTab::Editor, cx);
        cx.notify();
    }

    /// Selected clip id, when its track is processed by an ARA plug-in.
    fn selected_ara_clip_id(&self, cx: &gpui::App) -> Option<String> {
        let state = &self.timeline.read(cx).state;
        let clip_id = state.selection.selected_clip_ids.first()?;
        let (track, _) = state.find_clip(clip_id)?;
        track.ara.as_ref().map(|_| clip_id.clone())
    }

    /// Whether the popped-out editor window is still open.
    fn ara_editor_window_open(&self, cx: &gpui::App) -> bool {
        self.ara_editor_window_key(cx)
            .is_some_and(|key| self.plugin_editors.open.contains_key(&ara_editor_key(&key)))
    }

    /// Session key of the track whose editor is popped out.
    fn ara_editor_window_key(&self, cx: &gpui::App) -> Option<AraSessionKey> {
        self.selected_ara_session_key(cx)
    }

    /// Detaches the docked view whenever the panel is not showing it.
    ///
    /// Called once per layout render. A native child window is not part of the
    /// GPUI tree, so nothing else would take it off screen when the dock is
    /// hidden or another tab is selected — it would simply float there.
    pub(crate) fn sync_ara_editor_visibility(&mut self, cx: &mut Context<Self>) {
        let showing = self.panels.bottom_docked
            && matches!(
                self.active_bottom_tab(),
                crate::components::BottomTab::Editor
            )
            && self.ara_panel_target(cx).is_some();
        if !showing && self.ara_editor.read(cx).is_attached() {
            // Deferred: this runs inside the layout's draw, and tearing a
            // plug-in window down there can re-enter GPUI.
            self.ara_editor
                .update(cx, |host, cx| host.request_detach(cx));
        }
    }
}

/// One-off trace for the ARA editor entry points, gated like every other
/// plug-in view diagnostic.
fn ara_trace(line: &str) {
    if std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some() {
        eprintln!("[ara-panel] {line}");
    }
}
