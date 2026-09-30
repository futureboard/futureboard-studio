//! StudioLayout integration for the Export and Render dialogs.
//!
//! Builds a plain engine snapshot + defaults from current project state inside a
//! short UI borrow, then hands them to the external Export window. The Render
//! dialog it opens owns the background job; StudioLayout holds only the Export
//! window's handle — and lends its transport to a realtime render.

use std::sync::Arc;

use gpui::{Bounds, Context};

use super::engine_snapshot::{build_engine_project_snapshot_for_export, volume_norm_to_linear};
use super::StudioLayout;
use crate::export::{
    open_export_arrangement_window, ExportIntent, ExportProjectDefaults, ExportTrackTarget,
    RealtimeTransportHooks,
};

impl StudioLayout {
    pub(super) fn open_export_arrangement_external_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        intent: ExportIntent,
        cx: &mut Context<Self>,
    ) {
        // Focus an already-open export window instead of spawning a second one.
        if let Some(handle) = self.external_windows.export_arrangement.clone() {
            if handle
                .update(cx, |_w, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.export_arrangement = None;
        }

        // Dismiss menus/popovers like the other external-window commands.
        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.overlay.open_popover = None;
        self.overlay.text_context_menu = None;

        // Capture a plain snapshot of project state under a short borrow — the
        // export job receives only this owned data, never a live entity.
        self.refresh_bridge_plugin_states(super::plugin_ops::PluginStateCaptureFor::Export, cx);
        let tl_state = self.timeline.read(cx).state.clone();
        let sample_rate = self.current_audio_sample_rate();
        let project_root = self
            .project_session
            .folder_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        // Global Latency Sync (PDC): stamp the live engine's compensation state +
        // graph generation into the export snapshot so the offline render uses the
        // exact same latency-compensated graph as realtime playback. Falls back to
        // the Playback setting, then the engine default (on) when no engine is up.
        let (pdc_enabled, latency_graph_version) = match self.audio_bridge.engine.as_ref() {
            Some(engine) => (engine.pdc_enabled(), engine.latency_graph_version()),
            None => (
                self.settings.read(cx).current.playback.latency_compensation,
                0,
            ),
        };
        let bridge_sinks = self
            .audio_bridge
            .engine
            .as_ref()
            .map(|engine| engine.plugin_bridge_sinks())
            .unwrap_or_default();
        // Export detaches the live bridge sinks and drives them from the offline
        // worker; the snapshot must keep external-bridge inserts (not in-process
        // native-plugin stubs) so effects/built-ins render through the real DSP.
        let snapshot = build_engine_project_snapshot_for_export(
            &tl_state,
            sample_rate,
            project_root.as_deref(),
            None,
            pdc_enabled,
            latency_graph_version,
        );
        let master_volume = volume_norm_to_linear(tl_state.master.volume);
        let content_end_beat = snapshot
            .clips
            .iter()
            .map(|c| c.start_beat + c.duration_beats)
            .chain(
                snapshot
                    .midi_clips
                    .iter()
                    .map(|c| c.start_beat + c.length_beats),
            )
            .fold(0.0_f64, f64::max);
        let project_name = self.project_session.name.clone();

        let defaults = ExportProjectDefaults {
            project_sample_rate: sample_rate,
            master_volume,
            content_end_beat,
            // The arrangement's range selection is the "Time selection" export
            // range. Passing `None` here is what left that option permanently
            // unavailable; the dialog already handles `Some(..)` and hides the
            // option when there is no range to export.
            time_selection: tl_state
                .arrangement_range
                .as_ref()
                .map(|range| (range.start_beat, range.end_beat))
                .filter(|(start, end)| end > start),
            loop_range: {
                let t = &tl_state.transport;
                if t.loop_end_beats > t.loop_start_beats {
                    Some((t.loop_start_beats as f64, t.loop_end_beats as f64))
                } else {
                    None
                }
            },
            mp3_available: sphere_encoder::mp3_available(),
            track_targets: tl_state
                .tracks
                .iter()
                .filter(|track| {
                    track.track_type
                        != crate::components::timeline::timeline_state::TrackType::Master
                })
                .map(|track| {
                    let vsti_output =
                        crate::components::timeline::timeline_state::is_vsti_output_child_track_id(
                            &track.id,
                        );
                    ExportTrackTarget {
                        id: track.id.clone(),
                        name: track.name.clone(),
                        include_in_multitrack: !track.track_type.is_routing() || vsti_output,
                        kind_label: if vsti_output {
                            "VSTi Out".to_string()
                        } else {
                            track_kind_label(track.track_type).to_string()
                        },
                    }
                })
                .collect(),
            // A realtime render records at the running device's rate; with no
            // stream there is nothing to record.
            live_sample_rate: self
                .audio_bridge
                .engine
                .as_ref()
                .filter(|engine| engine.is_running())
                .map(|_| sample_rate)
                .unwrap_or(0),
        };

        // Default output: <project folder>/Exports/<Name>.wav when the project
        // is saved on disk; otherwise the window falls back to the temp dir.
        let default_output = project_root.as_ref().map(|root| {
            let exports_dir = std::path::Path::new(root).join("Exports");
            // Best-effort: ensure the folder exists so the default path validates.
            let _ = std::fs::create_dir_all(&exports_dir);
            exports_dir.join(format!("{}.wav", sanitize_file_stem(&project_name)))
        });

        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            owner_bounds,
            self.studio_window_bounds(cx),
            cx,
        );

        match open_export_arrangement_window(
            owner_bounds,
            project_name,
            snapshot,
            bridge_sinks,
            self.audio_bridge.engine.clone(),
            defaults,
            default_output,
            intent,
            Some(self.realtime_render_hooks(cx)),
            cx,
        ) {
            Ok(handle) => self.external_windows.export_arrangement = Some(handle),
            Err(err) => eprintln!("[export] failed to open export window: {err}"),
        }
    }
}

impl StudioLayout {
    /// How a realtime render borrows this studio's transport.
    fn realtime_render_hooks(&self, cx: &mut Context<Self>) -> RealtimeTransportHooks {
        let start_owner = cx.entity().downgrade();
        let finish_owner = start_owner.clone();
        RealtimeTransportHooks {
            start: Arc::new(move |start_seconds, arm, cx| {
                start_owner
                    .update(cx, |this, cx| {
                        this.begin_realtime_render_transport(start_seconds, arm, cx)
                    })
                    .map_err(|_| "the studio is closed".to_string())?
            }),
            finish: Arc::new(move |cx| {
                let _ = finish_owner.update(cx, |this, cx| this.end_realtime_render_transport(cx));
            }),
        }
    }

    /// Take the transport for a realtime render: stop whatever plays, turn the
    /// loop and the click off (a render that wrapped or clicked would record
    /// both), play from `start_seconds`, and arm the capture only once the
    /// old playback has stopped, so none of it is recorded.
    fn begin_realtime_render_transport(
        &mut self,
        start_seconds: f64,
        arm: Box<dyn FnOnce()>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.is_recording_active(cx) {
            return Err("stop recording before a realtime render".to_string());
        }
        if self.audio_bridge.sync_in_flight
            || self.audio_bridge.project_dirty
            || self.audio_bridge.media_dirty
        {
            // The live graph must be the project the dialog captured.
            self.schedule_audio_project_sync(cx, true, "realtime_render");
            return Err(
                "the audio engine is still taking the latest edits; render again in a moment"
                    .to_string(),
            );
        }
        self.stop_native_playback(cx);
        if !self.ensure_audio_stream_warm() {
            return Err("the audio device is not running".to_string());
        }
        let playhead = self.timeline.read(cx).state.transport.playhead_beats;
        let engine = self
            .audio_bridge
            .engine
            .as_ref()
            .ok_or_else(|| "no audio engine is running".to_string())?;
        let _ = engine.set_loop(false, 0.0, 0.0);
        let _ = engine.set_metronome_suspended(true);
        engine
            .seek(start_seconds)
            .map_err(|error| error.to_string())?;
        // The seek has to land before recording is armed, or the blocks the
        // old position still plays would open the file.
        let _ = engine.wait_for_command_barrier(std::time::Duration::from_millis(500));
        arm();
        if let Err(error) = engine.play() {
            let _ = engine.set_metronome_suspended(false);
            self.sync_loop_controls(cx);
            return Err(error.to_string());
        }
        self.realtime_render_playhead = Some(playhead);
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.state.transport.playing = true;
            cx.notify();
        });
        Ok(())
    }

    /// Give the transport back after a realtime render: stopped, with the
    /// loop and click as they were, and the playhead where it was.
    fn end_realtime_render_transport(&mut self, cx: &mut Context<Self>) {
        self.stop_native_playback(cx);
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            let _ = engine.set_metronome_suspended(false);
        }
        self.sync_loop_controls(cx);
        if let Some(beat) = self.realtime_render_playhead.take() {
            self.seek_native_playhead(cx, beat);
        }
    }
}

/// Strip characters that are illegal in file names so the default export path is
/// always valid.
fn sanitize_file_stem(name: &str) -> String {
    let trimmed = name.trim();
    let stem: String = trimmed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if stem.is_empty() {
        "Export".to_string()
    } else {
        stem
    }
}

/// What kind of channel a track is, as the Export dialog's channel list says it.
fn track_kind_label(
    track_type: crate::components::timeline::timeline_state::TrackType,
) -> &'static str {
    use crate::components::timeline::timeline_state::TrackType;
    match track_type {
        TrackType::Audio => "Audio",
        TrackType::Instrument => "Instrument",
        TrackType::Midi => "MIDI",
        TrackType::Bus => "Bus",
        TrackType::Return => "Return",
        TrackType::Group => "Folder",
        TrackType::Master => "Master",
        TrackType::Video => "Video",
    }
}
