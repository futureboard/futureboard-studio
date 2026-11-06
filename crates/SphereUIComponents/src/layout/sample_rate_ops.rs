//! Sample-rate change confirmation + safe audio-engine restart.
//!
//! Changing the project sample rate while the audio engine is live would
//! otherwise silently swap the runtime timing rate underneath transport, the
//! plugin host / VST3 process setup, and MIDI/audio scheduling — the source of
//! BPM-sync and VSTi/audio-alignment bugs. To keep everything coherent the
//! change is gated behind an explicit confirmation: Re-open Project / Later /
//! Cancel. Runtime timing keeps using the *active* device rate until the engine
//! is restarted successfully.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Context, Window};

use crate::components::message_box_dialog::{
    open_message_box_window, MessageBoxKind, MessageBoxOptions, MessageBoxResult,
};
use crate::components::progress_dialog::{
    open_progress_dialog_window, ProgressBarValue, ProgressDialogOptions,
};
use crate::components::settings_dialog::UpdateSettingFn;

use super::{
    native_audio_backend_from_driver_type, resolve_output_device_for_backend, StudioLayout,
};

impl StudioLayout {
    /// Settings-update entry point used by the Settings window. Most updates are
    /// persisted and synced immediately, but a *sample-rate* change while the
    /// audio engine is open is intercepted and routed through a confirmation
    /// dialog so the engine's timing rate is never silently restarted under a
    /// live project.
    pub(crate) fn handle_setting_update(
        &mut self,
        updater: UpdateSettingFn,
        cx: &mut Context<Self>,
    ) {
        let before_rate = self
            .settings
            .read(cx)
            .current
            .general
            .project_defaults
            .sample_rate;
        let mut probe = self.settings.read(cx).current.clone();
        updater(&mut probe);
        let after_rate = probe.general.project_defaults.sample_rate;

        if after_rate != before_rate && self.audio_engine_stream_open() {
            // Defer behind explicit confirmation — do NOT persist or sync yet, so
            // Cancel cleanly reverts the dropdown to the current value.
            self.prompt_sample_rate_change(after_rate, cx);
            return;
        }

        self.apply_setting_update(updater, cx);
    }

    /// Persist `updater` to settings and propagate it to live systems. The
    /// normal (non-sample-rate) settings path.
    fn apply_setting_update(&mut self, updater: UpdateSettingFn, cx: &mut Context<Self>) {
        let before_theme = self.settings.read(cx).current.appearance.theme.clone();
        let mut probe = self.settings.read(cx).current.clone();
        updater(&mut probe);
        let next_theme = probe.appearance.theme.clone();
        let _ = self.settings.update(cx, |settings, cx| {
            settings.update_setting(move |s| updater(s), cx);
        });
        if next_theme != before_theme {
            let _ = crate::theme::activate_theme_by_id(&next_theme);
        }
        self.sync_settings_to_systems(cx);
        cx.notify();
    }

    fn audio_engine_stream_open(&self) -> bool {
        self.audio_bridge
            .engine
            .as_ref()
            .map(|engine| engine.stats().stream_open)
            .unwrap_or(false)
    }

    fn persist_sample_rate(&mut self, rate: u32, cx: &mut Context<Self>) {
        let _ = self.settings.update(cx, |settings, cx| {
            settings.update_setting(move |s| s.general.project_defaults.sample_rate = rate, cx);
        });
    }

    /// Change the open project's nominal rate without rewriting the application
    /// default used by future projects.
    pub(crate) fn request_project_sample_rate_change(
        &mut self,
        new_rate: u32,
        cx: &mut Context<Self>,
    ) {
        if !matches!(new_rate, 44_100 | 48_000 | 88_200 | 96_000 | 192_000) {
            return;
        }
        let current = self.timeline.read(cx).state.project_sample_rate;
        if current == new_rate {
            return;
        }
        if self.audio_engine_stream_open() {
            self.prompt_project_sample_rate_change(new_rate, cx);
            return;
        }

        self.commit_project_sample_rate(new_rate, cx);
        if self.audio_bridge.engine.is_some() {
            self.audio_bridge
                .sample_rate_deferred_target
                .store(new_rate, Ordering::Relaxed);
            self.restart_audio_for_sample_rate(new_rate, cx);
        }
    }

    pub(crate) fn reopen_for_current_project_rate_if_needed(&mut self, cx: &mut Context<Self>) {
        let project_rate = self.timeline.read(cx).state.project_sample_rate;
        if self.audio_bridge.engine.is_some() && self.current_audio_sample_rate() != project_rate {
            self.audio_bridge
                .sample_rate_deferred_target
                .store(project_rate, Ordering::Relaxed);
            self.restart_audio_for_sample_rate(project_rate, cx);
        }
    }

    fn commit_project_sample_rate(&mut self, rate: u32, cx: &mut Context<Self>) {
        let changed = self.timeline.update(cx, |timeline, cx| {
            if timeline.state.project_sample_rate == rate {
                return false;
            }
            timeline.state.project_sample_rate = rate;
            for track in &mut timeline.state.tracks {
                for clip in &mut track.clips {
                    if clip.stretch.original_sample_rate > 0 {
                        clip.stretch.project_sample_rate = rate;
                    }
                }
            }
            cx.notify();
            true
        });
        if changed {
            self.mark_dirty();
            self.audio_bridge.project_dirty = true;
            self.push_project_settings_snapshot_to_window(cx);
            cx.notify();
        }
    }

    fn prompt_project_sample_rate_change(&mut self, new_rate: u32, cx: &mut Context<Self>) {
        let current_active = self.current_audio_sample_rate();
        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Question,
            title: "Change Project Sample Rate?".to_string(),
            message: "The audio engine must reopen before playback can use the new project rate."
                .to_string(),
            detail: Some(format!(
                "Active: {current_active} Hz\nProject: {new_rate} Hz"
            )),
            buttons: vec![
                "Re-open Now".to_string(),
                "Later".to_string(),
                "Cancel".to_string(),
            ],
            default_id: 0,
            cancel_id: Some(2),
        };

        let owner = cx.entity().clone();
        let on_response: Arc<dyn Fn(MessageBoxResult, &mut Window, &mut App) + Send + Sync> =
            Arc::new(move |result, _window, cx| {
                StudioLayout::defer_update(&owner, cx, move |this, cx| {
                    this.resolve_project_sample_rate_change(result.response, new_rate, cx);
                });
            });

        if let Err(error) = open_message_box_window(owner_bounds, options, on_response, cx) {
            eprintln!("[project-settings] sample-rate dialog unavailable: {error}");
            self.resolve_project_sample_rate_change(1, new_rate, cx);
        }
    }

    fn resolve_project_sample_rate_change(
        &mut self,
        response: usize,
        new_rate: u32,
        cx: &mut Context<Self>,
    ) {
        match response {
            0 => {
                self.commit_project_sample_rate(new_rate, cx);
                self.audio_bridge
                    .sample_rate_deferred_target
                    .store(new_rate, Ordering::Relaxed);
                self.restart_audio_for_sample_rate(new_rate, cx);
            }
            1 => {
                self.commit_project_sample_rate(new_rate, cx);
                self.audio_bridge
                    .sample_rate_deferred_target
                    .store(new_rate, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn prompt_sample_rate_change(&mut self, new_rate: u32, cx: &mut Context<Self>) {
        let current_active = self.current_audio_sample_rate();
        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Question,
            title: "Re-open Project?".to_string(),
            message:
                "Changing the sample rate requires restarting the audio engine and \
                      re-opening the current project so plugins, timing, and playback stay in sync."
                    .to_string(),
            detail: Some(format!(
                "Current sample rate: {current_active} Hz\nNew requested sample rate: {new_rate} Hz"
            )),
            buttons: vec![
                "Re-open Project".to_string(),
                "Later".to_string(),
                "Cancel".to_string(),
            ],
            default_id: 0,
            cancel_id: Some(2),
        };

        let owner = cx.entity().clone();
        let on_response: Arc<dyn Fn(MessageBoxResult, &mut Window, &mut App) + Send + Sync> =
            Arc::new(move |result, _window, cx| {
                StudioLayout::defer_update(&owner, cx, move |this, cx| {
                    this.resolve_sample_rate_change(result.response, new_rate, cx);
                });
            });

        if let Err(err) = open_message_box_window(owner_bounds, options, on_response, cx) {
            // Dialog surface unavailable — fall back to persisting the preference
            // without restarting (treated like "Later").
            eprintln!("[audio-device] sample-rate dialog unavailable: {err}");
            self.resolve_sample_rate_change(1, new_rate, cx);
        }
    }

    fn resolve_sample_rate_change(
        &mut self,
        response: usize,
        new_rate: u32,
        cx: &mut Context<Self>,
    ) {
        match response {
            // Re-open Project.
            0 => {
                self.persist_sample_rate(new_rate, cx);
                self.audio_bridge
                    .sample_rate_deferred_target
                    .store(0, Ordering::Relaxed);
                self.restart_audio_for_sample_rate(new_rate, cx);
            }
            // Later: persist the preference but keep the engine — and all runtime
            // timing — on the current active rate until a future restart.
            1 => {
                self.persist_sample_rate(new_rate, cx);
                self.audio_bridge
                    .sample_rate_deferred_target
                    .store(new_rate, Ordering::Relaxed);
                eprintln!(
                    "[audio-device] sample-rate change deferred (Later): requested={new_rate} active={}",
                    self.current_audio_sample_rate()
                );
                cx.notify();
            }
            // Cancel: nothing persisted — the dropdown reverts because the schema
            // is unchanged.
            _ => {}
        }
    }

    /// Re-open Project: stop transport/recording, show a loading dialog, reopen
    /// the audio device at the requested rate, then rebuild the project runtime
    /// against the actual active rate.
    fn restart_audio_for_sample_rate(&mut self, new_rate: u32, cx: &mut Context<Self>) {
        // Stop playback / recording before tearing the device down.
        if self.is_recording_active(cx) {
            self.stop_native_recording(cx);
        } else {
            self.stop_native_playback(cx);
        }

        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        let dialog = ProgressDialogOptions::default()
            .title("Re-opening Project")
            .heading(format!("Re-opening project at {new_rate} Hz"))
            .detail("Restarting the audio engine and re-initializing plug-ins…")
            .progress(ProgressBarValue::Indeterminate)
            .hide_percent();
        let progress = open_progress_dialog_window(owner_bounds, dialog, None, cx).ok();

        let owner = cx.entity().clone();
        let job = crate::components::background_tasks::BackgroundJob::start(
            crate::components::BackgroundTaskKind::SampleRateConversion,
            format!("Re-open project at {new_rate} Hz"),
        );
        job.set_detail("Restarting the audio engine and plug-ins");
        cx.spawn(async move |_this, cx| {
            // Let the loading dialog paint before the (brief, blocking) reopen.
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let active = owner.update(cx, |this, cx| {
                this.reopen_audio_with_sample_rate(new_rate, cx);
                this.audio_bridge
                    .stats
                    .as_ref()
                    .map(|stats| stats.sample_rate)
            });
            match active {
                Some(rate) if rate == new_rate => {
                    job.complete(Some(format!("Running at {rate} Hz")))
                }
                Some(rate) => job.fail(format!(
                    "The device runs at {rate} Hz, not the {new_rate} Hz asked for"
                )),
                None => job.fail("The audio engine did not come back"),
            }
            if let Some(handle) = progress {
                let _ = handle.update(cx, |_view, window, _cx| window.remove_window());
            }
        })
        .detach();
    }

    /// Reopen the audio device at `new_rate`, then rebuild the project runtime so
    /// transport, plugin hosts / VST3 process setup, and MIDI/audio scheduling
    /// all adopt the *actual* active rate reported by the device. Surfaces a
    /// non-blocking warning if the device could not honor the requested rate.
    pub(crate) fn reopen_audio_with_sample_rate(&mut self, new_rate: u32, cx: &mut Context<Self>) {
        let schema = self.settings.read(cx).current.clone();
        let backend = native_audio_backend_from_driver_type(&schema.hardware.audio.driver_type);

        // Build + apply the reopen with the engine borrow tightly scoped so the
        // self-mutating follow-up (`schedule_audio_project_sync`) is unambiguous.
        let reopen = {
            let Some(engine) = self.audio_bridge.engine.as_mut() else {
                return;
            };
            let output_device = resolve_output_device_for_backend(
                engine,
                backend,
                &schema.hardware.audio.device_out,
            );
            let desired_config = DirectAudio::EngineConfig {
                sample_rate: new_rate,
                buffer_size: schema.general.project_defaults.buffer_size,
                channels: 2,
                backend,
                input_device: None,
                output_device,
            };
            eprintln!("[audio-device] re-opening for sample-rate change requested={new_rate}");
            engine
                .reopen_with_config(desired_config)
                .map(|()| engine.stats())
                .map_err(|error| error.to_string())
        };

        match reopen {
            Ok(stats) => {
                let active = stats.sample_rate;
                self.audio_bridge.stats = Some(stats);
                self.audio_bridge.running = true;
                self.audio_bridge.last_error = None;
                self.audio_bridge.project_dirty = true;
                eprintln!("[audio-device] re-opened: requested={new_rate} active={active}");

                // Resolve the deferred ("Later") target once the device actually
                // reports the requested rate.
                if active == new_rate {
                    self.audio_bridge
                        .sample_rate_deferred_target
                        .store(0, Ordering::Relaxed);
                }

                // Rebuild the project runtime (transport / VST3 setupProcessing /
                // MIDI scheduling) against the new active rate.
                self.schedule_audio_project_sync(cx, true, "sample_rate_reopen");

                if active != new_rate {
                    self.set_sample_rate_notice(format!(
                        "Audio device opened at {active} Hz (requested {new_rate} Hz)."
                    ));
                }
            }
            Err(message) => {
                eprintln!("[audio-device] re-open failed: {message}");
                self.audio_bridge.last_error = Some(message);
                self.audio_bridge.stats = self
                    .audio_bridge
                    .engine
                    .as_ref()
                    .map(|engine| engine.stats());
            }
        }
        cx.notify();
    }

    fn set_sample_rate_notice(&mut self, text: String) {
        self.audio_bridge.sample_rate_notice_text = text;
        self.audio_bridge.sample_rate_notice_until = Some(Instant::now() + Duration::from_secs(6));
    }
}
