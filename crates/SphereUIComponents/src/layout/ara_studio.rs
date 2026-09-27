//! ARA commands on the studio: bind, unbind, edit, sync, save, restore.
//!
//! [`super::ara_ops::AraState`] owns the sessions; this is where the rest of the
//! app reaches them. Every entry point keeps the four things that must agree in
//! step — the clip's binding, the ARA document, the engine's renderer list, and
//! the project snapshot — so no caller has to know the order.

use std::path::Path;

use gpui::Context;
use sphere_ara_host::AraTransportRequest;

use super::ara_graph;
use super::ara_ops::{AraParked, AraSavedDocuments, AraSessionKey, DeferredRestore, SavedArchive};
use super::StudioLayout;
use crate::project::{FutureboardProject, ProjectAraDeferred, ProjectAraDocument};

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
                // Without an engine nothing renders, and the session still has
                // to go: its document belongs to a track that no longer has it.
                let engine = layout.audio_bridge.engine.clone();
                layout.ara.close(engine.as_ref(), &key);
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

        let applied = self.ara.apply(
            &engine,
            key,
            &choice.name,
            &choice.path,
            &choice.class_id,
            &timeline,
            &view.graph,
            view.media_paths,
            &view.offline,
        );
        self.show_ara_notices(cx);
        if let Err(error) = applied {
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
                    self.commit_loop_change(cx);
                }
                AraTransportRequest::EnableCycle(enabled) => {
                    self.timeline.update(cx, |timeline, cx| {
                        timeline.state.transport.loop_enabled = enabled;
                        cx.notify();
                    });
                    self.commit_loop_change(cx);
                }
            }
        }

        // A plug-in may only report model changes from inside this call, which
        // the host owes it periodically — without it none of the updates below
        // would ever arrive. They are recorded synchronously, so the poll right
        // after sees them, and judges them as this periodic poll's rather than
        // as the answer to a later host edit.
        self.ara.notify_model_updates();
        if self.ara.poll_documents(std::time::Instant::now()) {
            // The plug-in changed its own persistent state, so the project has
            // unsaved work even though nothing in the timeline moved. It is
            // saved with the project, not played by the engine: the same
            // view-only mark every such writer uses, which keeps the session
            // the close prompt reads and the switcher's "Unsaved changes" in
            // step.
            self.mark_dirty_view_only();
            cx.notify();
        }
    }

    /// Publishes the project's musical context — tempo, meter and key — to
    /// every live ARA session, without touching their graphs.
    ///
    /// For edits that change nothing the engine plays (the project key), so
    /// no engine sync would carry them: each document is told what changed and
    /// nothing else, with no renderer suspension and no region churn. Every
    /// per-track document gets the same timeline, built once.
    pub(crate) fn refresh_ara_musical_contexts(&mut self, cx: &mut Context<Self>) {
        if !self.ara.is_active() {
            return;
        }
        let (timeline, live) = {
            let state = &self.timeline.read(cx).state;
            // A session whose track no longer names it is on its way out, as
            // in `refresh_ara_sessions`; it is left alone.
            let live: Vec<AraSessionKey> = self
                .ara
                .keys()
                .filter(|key| {
                    state.tracks.iter().any(|track| {
                        track.id == key.track_id
                            && track
                                .ara
                                .as_ref()
                                .is_some_and(|binding| binding.plugin_id == key.plugin_id)
                    })
                })
                .cloned()
                .collect();
            (ara_graph::musical_timeline(state), live)
        };
        for key in live {
            if let Err(error) = self.ara.update_musical_timeline(&key, &timeline) {
                let name = self
                    .ara
                    .plugin_name(&key)
                    .unwrap_or(key.plugin_id.as_str())
                    .to_owned();
                eprintln!("[ARA] {name}: musical context not updated: {error}");
                self.ara.last_error = Some(format!("{name}: {error}"));
            }
        }
    }

    /// Writes every ARA document into the project being saved: each live
    /// session's, every one still parked, every one kept back for audio that
    /// is offline, and every orphan.
    pub(crate) fn attach_ara_archives(&mut self, project: &mut FutureboardProject) {
        let saved = self.ara.store_archives();
        let to_project = |(key, archive): (AraSessionKey, SavedArchive)| ProjectAraDocument {
            plugin_id: key.plugin_id,
            track_id: key.track_id,
            archive_id: archive.archive_id,
            data: archive.data,
            written_with: archive.written_with,
            stored_now: archive.stored_now,
        };
        project.ara_documents = saved.documents.into_iter().map(to_project).collect();
        project.ara_orphans = saved.orphans.into_iter().map(to_project).collect();
        project.ara_deferred = saved
            .deferred
            .into_iter()
            .map(|(key, held)| ProjectAraDeferred {
                // Always saved back verbatim (see `AraState::defer`).
                document: to_project((key, held.archive)),
                remaining_sources: held.remaining,
            })
            .collect();
    }

    /// Parks a loaded project's ARA archives and opens the sessions its clips
    /// are bound to.
    ///
    /// Restoring runs inside the session open, before regions are assigned and
    /// before playback, which is where ARA requires it. Everything the previous
    /// project had open or parked goes first. Callers run this only once the
    /// loaded project is installed for good (after the integrity check), so a
    /// load that fails never takes the live project's documents with it.
    pub(crate) fn restore_ara_archives(
        &mut self,
        project: &FutureboardProject,
        cx: &mut Context<Self>,
    ) {
        self.close_all_ara_sessions(cx);
        let key = |document: &ProjectAraDocument| AraSessionKey {
            plugin_id: document.plugin_id.clone(),
            track_id: document.track_id.clone(),
        };
        let archive = |document: &ProjectAraDocument| SavedArchive {
            archive_id: document.archive_id.clone(),
            data: document.data.clone(),
            written_with: document.written_with.clone(),
            stored_now: false,
        };
        let saved = AraSavedDocuments {
            documents: project
                .ara_documents
                .iter()
                .map(|document| (key(document), archive(document)))
                .collect(),
            orphans: project
                .ara_orphans
                .iter()
                .map(|document| (key(document), archive(document)))
                .collect(),
            deferred: project
                .ara_deferred
                .iter()
                .map(|held| {
                    (
                        key(&held.document),
                        DeferredRestore {
                            archive: archive(&held.document),
                            remaining: held.remaining_sources.clone(),
                        },
                    )
                })
                .collect(),
        };
        // What the plan compares a moved source's audio by: the content the
        // project's own asset records hold for each asset id.
        let fingerprints = project
            .assets
            .iter()
            .filter_map(|asset| {
                let token = asset.source_fingerprint.as_deref()?;
                Some((
                    asset.id.clone(),
                    crate::project::io::content_fingerprint(token)?,
                ))
            })
            .collect();
        self.ara.load_archives(saved, fingerprints);

        self.open_pending_ara_sessions(cx);
        // Until a session is live its clips play from their files (see
        // `sync_engine_ara_rendering`); resync so the engine hears that now.
        self.mark_engine_project_dirty();
    }

    /// Closes every ARA session with its editors and forgets every saved ARA
    /// document: the project they belong to is being replaced or closed.
    ///
    /// Without this a New, a Close or a template kept the previous project's
    /// plug-ins running, and the next save wrote their documents into the new
    /// project; a track of the same id even got the old document back.
    ///
    /// The sequence is the one [`Self::unbind_track_from_ara`] follows, for
    /// every session at once:
    ///
    /// 1. The editors are asked to let go: popped-out windows are removed and
    ///    the docked panel's detach is requested. Neither happens inline (the
    ///    panel detaches on a deferred tick, a window when GPUI removes it).
    /// 2. The sessions leave the project synchronously
    ///    ([`super::ara_ops::AraState::retire_all`]): renderers out of the
    ///    engine behind the callback barrier, parked documents and orphans
    ///    forgotten. Callers load the next project right after, and its
    ///    sessions (on the same track ids, often) must not find these.
    /// 3. Each document is destroyed only once no view holds it any more (not
    ///    on the docked panel, not by the plug-in's own account), polled every
    ///    frame; a view that never lets go is closed anyway after the same
    ///    wait the unbind path allows, and the log says so. Destroying the
    ///    document under a live view is what took the app down.
    ///
    /// The status bar's ARA notice goes too: it was about that project's
    /// documents (every path that replaces or closes a project, `reset_project`
    /// included, comes through here).
    pub(crate) fn close_all_ara_sessions(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<AraSessionKey> = self.ara.keys().cloned().collect();
        self.close_ara_editors(&keys, cx);
        let engine = self.audio_bridge.engine.clone();
        self.ara
            .retire_all(engine.as_ref(), std::time::Instant::now());
        self.finish_retired_ara_sessions(cx);
        self.clear_ara_notice(cx);
    }

    /// Step 1 of [`Self::close_all_ara_sessions`]: asks every editor on
    /// these sessions to let go.
    fn close_ara_editors(&mut self, keys: &[AraSessionKey], cx: &mut Context<Self>) {
        for key in keys {
            self.close_ara_editor(key, cx);
        }
        if !keys.is_empty() {
            self.ara_editor_popped_out = false;
            self.ara_editor_open_pending = false;
            self.ara_editor
                .update(cx, |host, cx| host.request_detach(cx));
        }
    }

    /// Step 3 of [`Self::close_all_ara_sessions`]: destroys each retired
    /// document once its editor views have let go.
    fn finish_retired_ara_sessions(&mut self, cx: &mut Context<Self>) {
        let patience = VIEW_RELEASE_POLL_INTERVAL * VIEW_RELEASE_POLLS;
        let this_frame = move |layout: &mut Self, cx: &mut Context<Self>| {
            let docked = layout.ara_editor.read(cx);
            layout
                .ara
                .finish_retired(std::time::Instant::now(), patience, |handle| {
                    !docked.holds_instance(handle)
                })
        };
        if !this_frame(self, cx) {
            return;
        }
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            // One poll past the patience, so the last session retired is
            // always either released or overdue by the end.
            for _ in 0..=VIEW_RELEASE_POLLS {
                executor.timer(VIEW_RELEASE_POLL_INTERVAL).await;
                let waiting = this
                    .update(cx, |layout, cx| this_frame(layout, cx))
                    .unwrap_or(false);
                if !waiting {
                    break;
                }
            }
        })
        .detach();
    }

    /// Puts back the ARA documents of a project whose replacement failed,
    /// when the switch had already closed its sessions or parked the other
    /// project's documents. A switch that failed before touching them leaves
    /// the live sessions running.
    pub(crate) fn restore_parked_ara(&mut self, parked: AraParked, cx: &mut Context<Self>) {
        if self.ara.holds(&parked) {
            return;
        }
        let keys: Vec<AraSessionKey> = self.ara.keys().cloned().collect();
        self.close_ara_editors(&keys, cx);
        let engine = self.audio_bridge.engine.clone();
        self.ara
            .roll_back(parked, engine.as_ref(), std::time::Instant::now());
        self.finish_retired_ara_sessions(cx);
        self.open_pending_ara_sessions(cx);
        self.mark_engine_project_dirty();
    }

    /// Shows what the ARA sessions had to tell the user about saved
    /// documents, in the status bar: never a dialog, the project is usable.
    fn show_ara_notices(&mut self, cx: &mut Context<Self>) {
        // Drained after every session's sync, so several are one track's:
        // part of its audio offline and the rest not matched, say. Each is
        // a sentence naming its track.
        let notices = self.ara.take_notices();
        if notices.is_empty() {
            return;
        }
        self.show_ara_notice(notices.join(" "), cx);
    }

    /// Opens the session of every ARA-bound track that has none yet.
    ///
    /// A project is restored before either of the things a session needs is
    /// guaranteed to exist: the plug-in catalog and the audio engine both load
    /// asynchronously, and a project opened from the Welcome screen reaches
    /// the studio ahead of both. Restoring once and giving up left those
    /// tracks with no plug-in — nothing to show, nothing rendering — so this
    /// runs again when the catalog and the engine arrive. Archives stay
    /// parked until their session opens, so nothing is lost while waiting.
    ///
    /// One session per ARA track. A parked archive whose track no longer has
    /// that plug-in stays parked and is saved back untouched rather than
    /// silently discarded.
    pub(crate) fn open_pending_ara_sessions(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<AraSessionKey> = {
            let state = &self.timeline.read(cx).state;
            let mut keys: Vec<AraSessionKey> = Vec::new();
            for track in &state.tracks {
                let Some(binding) = track.ara.as_ref() else {
                    continue;
                };
                let key = AraSessionKey {
                    plugin_id: binding.plugin_id.clone(),
                    track_id: track.id.clone(),
                };
                if self.ara.processor(&key).is_none() && !keys.contains(&key) {
                    keys.push(key);
                }
            }
            keys
        };
        if pending.is_empty() {
            return;
        }
        // Not failures yet — just early. The catalog load and the engine
        // start both call back in here.
        if self.plugin_catalog.available.is_none() || self.audio_bridge.engine.is_none() {
            ara_trace(&format!(
                "{} ARA session(s) waiting for the {}",
                pending.len(),
                if self.plugin_catalog.available.is_none() {
                    "plug-in catalog"
                } else {
                    "audio engine"
                }
            ));
            return;
        }
        let choices = self.ara_plugin_choices();
        let mut opened = false;
        for key in pending {
            match choices.iter().find(|choice| choice.id == key.plugin_id) {
                Some(choice) => {
                    let choice = choice.clone();
                    self.sync_ara_session(&key, &choice, cx);
                    opened |= self.ara.processor(&key).is_some();
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
        if opened {
            // The engine switches those tracks from their files to the plug-in.
            self.mark_engine_project_dirty();
        }
        cx.notify();
    }

    /// Clears `ara_rendered` on clips whose track has no live ARA session.
    ///
    /// The snapshot is built from the timeline alone, where a binding is
    /// enough to mark a clip as the plug-in's to render. With no session
    /// behind it — still opening, or its plug-in missing — that silenced the
    /// track, where the promise (and the error message) is that it plays from
    /// its files until the plug-in is back.
    pub(crate) fn sync_engine_ara_rendering(
        &self,
        snapshot: &mut DirectAudio::types::EngineProjectSnapshot,
    ) {
        let live: std::collections::HashSet<&str> =
            self.ara.keys().map(|key| key.track_id.as_str()).collect();
        clear_ara_rendering_without_session(snapshot, &live);
    }
}

/// See [`StudioLayout::sync_engine_ara_rendering`].
fn clear_ara_rendering_without_session(
    snapshot: &mut DirectAudio::types::EngineProjectSnapshot,
    live_tracks: &std::collections::HashSet<&str>,
) {
    for clip in snapshot.clips.iter_mut() {
        if clip.ara_rendered && !live_tracks.contains(clip.track_id.as_str()) {
            clip.ara_rendered = false;
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

    /// Session key for the current ARA track.
    ///
    /// Clip selection is preferred because it identifies the exact edit target.
    /// Selecting a track clears `selected_clip_ids`, though, so the primary
    /// current track must be used as the fallback for the docked Editor tab.
    fn selected_ara_session_key(&self, cx: &gpui::App) -> Option<AraSessionKey> {
        let state = &self.timeline.read(cx).state;
        Self::selected_ara_session_key_from_state(state)
    }

    fn selected_ara_session_key_from_state(
        state: &crate::components::timeline::timeline_state::TimelineState,
    ) -> Option<AraSessionKey> {
        let clip_track = state
            .selection
            .selected_clip_ids
            .first()
            .and_then(|clip_id| state.find_clip(clip_id).map(|(track, _)| track))
            .filter(|track| track.ara.is_some());
        let current_track = state
            .selection
            .selected_track_id
            .as_deref()
            .and_then(|track_id| state.tracks.iter().find(|track| track.id == track_id));
        let track = clip_track.or(current_track)?;
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

    /// The handle of that instance, for the embedded editor to tell whether
    /// the view it holds is on it (see `AraState::instance_handle`).
    pub(crate) fn ara_instance_handle(&self, key: &AraSessionKey) -> Option<usize> {
        self.ara.instance_handle(key)
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
            && self.clip_editor_panel.read(cx).ara_tab_active()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{AraTrackBinding, TimelineState};

    #[test]
    fn current_track_is_an_ara_target_without_a_selected_clip() {
        let mut state = TimelineState::demo_project();
        let track_id = state.tracks[0].id.clone();
        state.tracks[0].ara = Some(AraTrackBinding {
            plugin_id: "test-ara".to_string(),
            plugin_path: "/tmp/test-ara.vst3".to_string(),
            class_id: "test-class".to_string(),
        });
        state.select_track(&track_id);

        assert_eq!(
            StudioLayout::selected_ara_session_key_from_state(&state),
            Some(AraSessionKey {
                plugin_id: "test-ara".to_string(),
                track_id,
            })
        );
    }

    #[test]
    fn no_current_or_clip_ara_selection_has_no_target() {
        let state = TimelineState::default();
        assert_eq!(
            StudioLayout::selected_ara_session_key_from_state(&state),
            None
        );
    }

    fn clip_on(track: &str) -> DirectAudio::types::EngineClipSnapshot {
        let mut clip: DirectAudio::types::EngineClipSnapshot = serde_json::from_str(
            r#"{"id":"c","trackId":"","assetId":"","startBeat":0,"durationBeats":1,"offsetSeconds":0,"gain":1}"#,
        )
        .expect("minimal clip snapshot");
        clip.track_id = track.to_string();
        clip.ara_rendered = true;
        clip
    }

    #[test]
    fn a_bound_track_without_a_live_session_plays_its_files() {
        let mut snapshot = crate::layout::engine_snapshot::build_engine_project_snapshot(
            &crate::components::timeline::timeline_state::TimelineState::default(),
            48_000,
            None,
            None,
        );
        snapshot.clips = vec![clip_on("live"), clip_on("waiting")];
        let live: std::collections::HashSet<&str> = ["live"].into_iter().collect();
        clear_ara_rendering_without_session(&mut snapshot, &live);
        assert!(
            snapshot.clips[0].ara_rendered,
            "the plug-in renders a live track"
        );
        assert!(
            !snapshot.clips[1].ara_rendered,
            "a track still waiting plays its files"
        );
    }
}
