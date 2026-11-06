//! Solfege Audioguard on the Studio side: what the engine's guard did, said in
//! the status bar, and the parts of protection that need the project — turning
//! a plug-in the engine bypassed off in the project, and reconnecting a lost
//! audio device.

use std::time::{Duration, Instant};

use gpui::Context;

use super::StudioLayout;
use crate::components::background_tasks::{BackgroundJob, BackgroundTaskKind};
use crate::components::timeline::timeline_state::TrackEditScope;

/// How long an Audioguard notice stays in the status bar.
const NOTICE: Duration = Duration::from_secs(8);
/// First wait before reconnecting a lost device, and the longest wait between
/// attempts while it stays missing.
const RECONNECT_FIRST: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Studio-side Audioguard state.
#[derive(Default)]
pub(crate) struct AudioguardUiState {
    /// Output mutes already told about.
    last_output_mutes: u64,
    /// Status bar notice and when it expires.
    notice: Option<(String, Instant)>,
    /// A device reconnect is running on a background thread.
    reconnecting: bool,
    /// When to try next, and the wait after that.
    next_reconnect_at: Option<Instant>,
    reconnect_backoff: Option<Duration>,
    /// The Background Tasks entry for the reconnect, while the device is lost.
    reconnect_job: Option<BackgroundJob>,
}

impl StudioLayout {
    /// The Audioguard notice in the status bar, while it lasts.
    pub(super) fn active_audioguard_notice(&self) -> Option<&str> {
        self.audioguard
            .notice
            .as_ref()
            .filter(|(_, until)| *until > Instant::now())
            .map(|(text, _)| text.as_str())
    }

    fn show_audioguard_notice(&mut self, text: String) {
        self.audioguard.notice = Some((text, Instant::now() + NOTICE));
    }

    /// Read what the guard did since the last status tick. Control thread,
    /// from the status poll (every 250 ms); atomic reads plus one short lock
    /// of the engine's control mirror.
    pub(super) fn poll_audioguard(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.audio_bridge.engine.clone() else {
            return;
        };
        let status = engine.audioguard_status();

        if status.output_mutes > self.audioguard.last_output_mutes {
            self.audioguard.last_output_mutes = status.output_mutes;
            let why = match status.last_output_reason {
                DirectAudio::audioguard::OutputGuardReason::Burst => "a burst far above full scale",
                _ => "invalid samples",
            };
            self.show_audioguard_notice(format!(
                "Audioguard muted the output for a moment: it carried {why}"
            ));
        }

        for tripped in engine.take_audioguard_tripped_inserts() {
            self.bypass_tripped_insert(tripped, cx);
        }

        self.reconnect_lost_device(engine, status.level, cx);
    }

    /// Show a plug-in Audioguard bypassed as bypassed in the project too, so
    /// the mixer says what is playing and the user can switch it back on (which
    /// re-arms it in the engine). One undo step.
    fn bypass_tripped_insert(
        &mut self,
        tripped: DirectAudio::audioguard::TrippedInsert,
        cx: &mut Context<Self>,
    ) {
        let (name, track_name, already_bypassed) = {
            let state = &self.timeline.read(cx).state;
            let Some(slot) = state.find_insert_slot(&tripped.track_id, &tripped.insert_id) else {
                return;
            };
            let track_name = state
                .find_track(&tripped.track_id)
                .map(|track| track.name.clone())
                .unwrap_or_default();
            (
                slot.display_name.clone(),
                track_name,
                slot.bypassed || !slot.enabled,
            )
        };
        if !already_bypassed {
            let edit = self.begin_track_edit(TrackEditScope::channel(&tripped.track_id), cx);
            self.timeline.update(cx, |timeline, cx| {
                timeline
                    .state
                    .toggle_insert_bypass(&tripped.track_id, &tripped.insert_id);
                cx.notify();
            });
            self.commit_track_edit("Audioguard Bypass", edit, cx);
            self.push_insert_enabled_to_engine(&tripped.track_id, &tripped.insert_id, cx);
            self.mark_dirty_view_only();
            self.push_mixer_snapshot_to_window(cx);
        }
        let why = match tripped.reason {
            DirectAudio::audioguard::InsertTrip::MissedDeadlines => "it kept missing its deadline",
            _ => "it kept producing invalid audio",
        };
        let on = if track_name.is_empty() {
            String::new()
        } else {
            format!(" on {track_name}")
        };
        self.show_audioguard_notice(format!("Audioguard bypassed {name}{on}: {why}"));
        cx.notify();
    }

    /// While the device is lost, reopen it in the background with a growing
    /// wait between attempts. Off leaves a lost device to the user.
    fn reconnect_lost_device(
        &mut self,
        engine: DirectAudio::AudioEngine,
        level: DirectAudio::AudioguardLevel,
        cx: &mut Context<Self>,
    ) {
        let lost = self
            .audio_bridge
            .stats
            .as_ref()
            .is_some_and(|stats| stats.device_lost);
        if !lost {
            if let Some(job) = self.audioguard.reconnect_job.take() {
                job.complete(Some("The audio device is back".to_string()));
                self.show_audioguard_notice("Audioguard reconnected the audio device".to_string());
            }
            self.audioguard.next_reconnect_at = None;
            self.audioguard.reconnect_backoff = None;
            return;
        }
        if level == DirectAudio::AudioguardLevel::Off || self.audioguard.reconnecting {
            return;
        }
        let now = Instant::now();
        let due = match self.audioguard.next_reconnect_at {
            Some(at) => now >= at,
            None => {
                // First sight of the loss: say so, and give the device a
                // moment to come back on its own before the first attempt.
                self.audioguard.next_reconnect_at = Some(now + RECONNECT_FIRST);
                self.audioguard.reconnect_job = Some(BackgroundJob::start(
                    BackgroundTaskKind::NativeSync,
                    "Reconnect the audio device",
                ));
                self.show_audioguard_notice(
                    "The audio device was lost: Audioguard is reconnecting".to_string(),
                );
                false
            }
        };
        if !due {
            return;
        }
        self.audioguard.reconnecting = true;
        let backoff = self
            .audioguard
            .reconnect_backoff
            .map_or(RECONNECT_FIRST, |wait| (wait * 2).min(RECONNECT_MAX));
        self.audioguard.reconnect_backoff = Some(backoff);
        if let Some(job) = self.audioguard.reconnect_job.as_ref() {
            job.set_detail(format!(
                "Next attempt in {} s if this one fails",
                backoff.as_secs()
            ));
        }
        // Opening a device can take a while; never on the UI thread.
        let task = cx
            .background_executor()
            .spawn(async move { engine.recover_device().map_err(|error| error.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.audioguard.reconnecting = false;
                this.audioguard.next_reconnect_at = Some(Instant::now() + backoff);
                if let Err(error) = result {
                    eprintln!("[Audioguard] device reconnect failed: {error}");
                }
                cx.notify();
            });
        })
        .detach();
    }
}
