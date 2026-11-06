//! Solfege Audioguard on the Studio side: what the engine's guard did, said in
//! the status bar, and the parts of protection that need the project — turning
//! a plug-in the engine bypassed off in the project, and reconnecting a lost
//! audio device.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Context, Window};

use super::StudioLayout;
use crate::components::background_tasks::{BackgroundJob, BackgroundTaskKind};
use crate::components::timeline::timeline_state::TrackEditScope;

/// How long an Audioguard notice stays in the status bar.
const NOTICE: Duration = Duration::from_secs(8);
/// First wait before reconnecting a lost device, and the longest wait between
/// attempts while it stays missing.
const RECONNECT_FIRST: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Missed blocks within [`OVERLOAD_WINDOW`] of playback that count as the
/// system being overloaded rather than a single hiccup. One is enough while a
/// take records: the take has a gap.
const OVERLOAD_BLOCKS: u64 = 3;
const OVERLOAD_WINDOW: Duration = Duration::from_secs(5);
/// After the dialog is closed, how long before it may come back.
const OVERLOAD_QUIET: Duration = Duration::from_secs(120);

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
    /// The overrun count last seen, and the blocks missed while playing in
    /// the last [`OVERLOAD_WINDOW`], newest last.
    last_overruns: Option<u64>,
    recent_overruns: VecDeque<(Instant, u64)>,
    /// The System Overloaded dialog while it is up, and when it may next show.
    overload_dialog:
        Option<gpui::WindowHandle<crate::components::message_box_dialog::MessageBoxWindow>>,
    overload_quiet_until: Option<Instant>,
}

/// What the System Overloaded dialog says, from what the engine measured.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OverloadReport {
    pub missed_blocks: u64,
    pub window_seconds: u64,
    pub buffer_frames: u32,
    pub sample_rate: u32,
    /// The slowest block since the stream opened, against one block's time.
    pub peak_block_ms: f32,
    pub block_ms: f32,
    pub level: DirectAudio::AudioguardLevel,
    pub recording: bool,
}

/// Which button the user pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverloadAction {
    UseAudioguardHigh,
    AudioSettings,
    ProcessExplorer,
    Close,
}

impl OverloadReport {
    /// The buttons, in order, for this report: no "Use Audioguard High" when
    /// it is already on.
    pub(crate) fn actions(&self) -> Vec<OverloadAction> {
        let mut actions = Vec::with_capacity(4);
        if self.level != DirectAudio::AudioguardLevel::High {
            actions.push(OverloadAction::UseAudioguardHigh);
        }
        actions.extend([
            OverloadAction::AudioSettings,
            OverloadAction::ProcessExplorer,
            OverloadAction::Close,
        ]);
        actions
    }

    pub(crate) fn message_box(&self) -> crate::components::message_box_dialog::MessageBoxOptions {
        use crate::components::message_box_dialog::{MessageBoxKind, MessageBoxOptions};
        let mut detail = format!(
            "{} block{} did not reach the audio device in time in the last {} seconds. \
             Buffer: {} samples ({:.1} ms) at {} Hz; the slowest block took {:.1} ms ({:.0}% \
             of its time). Audioguard: {}.",
            self.missed_blocks,
            if self.missed_blocks == 1 { "" } else { "s" },
            self.window_seconds,
            self.buffer_frames,
            self.block_ms,
            self.sample_rate,
            self.peak_block_ms,
            if self.block_ms > 0.0 {
                self.peak_block_ms / self.block_ms * 100.0
            } else {
                0.0
            },
            self.level.as_str(),
        );
        if self.recording {
            detail.push_str(" The take being recorded may have gaps.");
        }
        detail.push_str(
            "\n\nA larger buffer, Audioguard High, or freezing the heaviest tracks gives the \
             engine more time. Process Explorer shows which plug-ins cost the most. This \
             warning can be turned off in Preferences > Playback.",
        );
        let buttons = self
            .actions()
            .iter()
            .map(|action| {
                match action {
                    OverloadAction::UseAudioguardHigh => "Use Audioguard High",
                    OverloadAction::AudioSettings => "Audio Settings…",
                    OverloadAction::ProcessExplorer => "Process Explorer…",
                    OverloadAction::Close => "OK",
                }
                .to_string()
            })
            .collect::<Vec<_>>();
        let close = buttons.len() - 1;
        MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "System Overloaded".to_string(),
            message: "The audio engine could not keep up, and playback dropped out.".to_string(),
            detail: Some(detail),
            buttons,
            default_id: close,
            cancel_id: Some(close),
        }
    }
}

/// Whether the missed blocks in the window count as an overload.
pub(crate) fn is_overloaded(recent: u64, recording: bool) -> bool {
    recent >= OVERLOAD_BLOCKS || (recording && recent > 0)
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
        self.watch_for_overload(status, cx);
    }

    /// Count the blocks the device missed while the transport played, and
    /// show the System Overloaded dialog when they add up to an overload.
    fn watch_for_overload(
        &mut self,
        status: DirectAudio::audioguard::AudioguardStatus,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        let fresh = self
            .audioguard
            .last_overruns
            .map_or(0, |last| status.overruns.saturating_sub(last));
        self.audioguard.last_overruns = Some(status.overruns);
        let Some(stats) = self.audio_bridge.stats.clone() else {
            return;
        };
        let recent = &mut self.audioguard.recent_overruns;
        if fresh > 0 && stats.transport_playing {
            recent.push_back((now, fresh));
        }
        while recent
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > OVERLOAD_WINDOW)
        {
            recent.pop_front();
        }
        let missed: u64 = recent.iter().map(|(_, count)| count).sum();
        let recording = self.is_recording_active(cx);
        if !is_overloaded(missed, recording)
            || !self.settings.read(cx).current.playback.warn_on_overload
            || self.audioguard.overload_dialog.is_some()
            || self
                .audioguard
                .overload_quiet_until
                .is_some_and(|until| now < until)
        {
            return;
        }
        let sample_rate = stats.sample_rate.max(1);
        let report = OverloadReport {
            missed_blocks: missed,
            window_seconds: OVERLOAD_WINDOW.as_secs(),
            buffer_frames: stats.buffer_size,
            sample_rate,
            peak_block_ms: stats.callback_max_us as f32 / 1000.0,
            block_ms: stats.buffer_size as f32 * 1000.0 / sample_rate as f32,
            level: status.level,
            recording,
        };
        self.audioguard.recent_overruns.clear();
        self.open_overload_dialog(report, cx);
    }

    fn open_overload_dialog(&mut self, report: OverloadReport, cx: &mut Context<Self>) {
        let actions = report.actions();
        let owner = cx.entity().clone();
        let on_response: crate::components::message_box_dialog::MessageBoxResponseCb =
            Arc::new(move |result, _window: &mut Window, cx: &mut App| {
                let action = actions
                    .get(result.response)
                    .copied()
                    .unwrap_or(OverloadAction::Close);
                StudioLayout::defer_update(&owner, cx, move |this, cx| {
                    this.audioguard.overload_dialog = None;
                    this.audioguard.overload_quiet_until = Some(Instant::now() + OVERLOAD_QUIET);
                    this.run_overload_action(action, cx);
                });
            });
        let owner_bounds = self.studio_window_bounds(cx);
        match crate::components::message_box_dialog::open_message_box_window(
            owner_bounds,
            report.message_box(),
            on_response,
            cx,
        ) {
            Ok(handle) => self.audioguard.overload_dialog = Some(handle),
            Err(error) => eprintln!("[Audioguard] System Overloaded dialog unavailable: {error}"),
        }
    }

    fn run_overload_action(&mut self, action: OverloadAction, cx: &mut Context<Self>) {
        let owner_bounds = self.studio_window_bounds(cx);
        match action {
            OverloadAction::UseAudioguardHigh => {
                self.settings.update(cx, |settings, cx| {
                    settings.update_setting(
                        |schema| {
                            schema.playback.audioguard = crate::settings::AudioguardLevel::High
                        },
                        cx,
                    );
                });
                if let Some(engine) = self.audio_bridge.engine.as_ref() {
                    engine.set_dropout_protection_mode(DirectAudio::AudioguardLevel::High);
                }
                self.show_audioguard_notice("Audioguard is set to High".to_string());
            }
            OverloadAction::AudioSettings => self.open_settings_dialog_on_tab(
                owner_bounds,
                Some(crate::components::settings_dialog::SettingsTab::Audio),
                cx,
            ),
            OverloadAction::ProcessExplorer => self.open_process_explorer_window(owner_bounds, cx),
            OverloadAction::Close => {}
        }
        cx.notify();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn report(level: DirectAudio::AudioguardLevel, recording: bool) -> OverloadReport {
        OverloadReport {
            missed_blocks: 4,
            window_seconds: 5,
            buffer_frames: 256,
            sample_rate: 48_000,
            peak_block_ms: 7.1,
            block_ms: 256.0 * 1000.0 / 48_000.0,
            level,
            recording,
        }
    }

    /// A single missed block while playing is a hiccup; three in the window
    /// are an overload. While a take records, one is already a gap in it.
    #[test]
    fn overload_needs_several_missed_blocks_unless_a_take_records() {
        assert!(!is_overloaded(0, false));
        assert!(!is_overloaded(2, false));
        assert!(is_overloaded(3, false));
        assert!(is_overloaded(1, true));
        assert!(!is_overloaded(0, true));
    }

    #[test]
    fn the_dialog_offers_audioguard_high_only_when_it_is_not_on() {
        let normal = report(DirectAudio::AudioguardLevel::Normal, false);
        assert_eq!(normal.actions()[0], OverloadAction::UseAudioguardHigh);
        let high = report(DirectAudio::AudioguardLevel::High, false);
        assert!(!high.actions().contains(&OverloadAction::UseAudioguardHigh));
        // Every button maps to an action, and OK is the default and Escape.
        for report in [normal, high] {
            let options = report.message_box();
            assert_eq!(options.buttons.len(), report.actions().len());
            assert_eq!(options.buttons.last().map(String::as_str), Some("OK"));
            assert_eq!(options.default_id, options.buttons.len() - 1);
            assert_eq!(options.cancel_id, Some(options.buttons.len() - 1));
        }
    }

    #[test]
    fn the_dialog_says_what_was_measured() {
        let detail = report(DirectAudio::AudioguardLevel::Normal, true)
            .message_box()
            .detail
            .expect("a detail");
        assert!(detail.contains("4 blocks"), "{detail}");
        assert!(
            detail.contains("256 samples (5.3 ms) at 48000 Hz"),
            "{detail}"
        );
        assert!(detail.contains("7.1 ms (133%"), "{detail}");
        assert!(detail.contains("Audioguard: Normal"), "{detail}");
        assert!(detail.contains("may have gaps"), "{detail}");
        let playing = report(DirectAudio::AudioguardLevel::Normal, false)
            .message_box()
            .detail
            .unwrap();
        assert!(!playing.contains("may have gaps"));
    }
}
