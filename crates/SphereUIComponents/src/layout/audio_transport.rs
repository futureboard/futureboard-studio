use gpui::{App, Context, Window};
use sphere_midi_service::{MidiInputEvent, MidiInputRouteStatus, MidiInputSource, MidiInputTarget};

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::components;
use crate::components::edit::TempoStateSnapshot;
use crate::components::mixer_panel::{write_vsti_output_meter_key, VstiOutputMeterState};
use crate::components::timeline::timeline_state::{ClipType, TrackOutputRouting, TrackType};
use crate::components::timeline::Timeline;

use super::engine_snapshot::{build_engine_project_snapshot, log_engine_sync_snapshot};
use super::helpers::{smooth_meter_value, update_meter_clip, update_meter_hold};
use super::transport_freeze_debug::{self, PlayWatchdog};
use super::{ContextMenuRequest, ContextMenuTarget, ContextTarget, OpenPopover, StudioLayout};

/// Whether a Stop has to finalize a take before it stops the transport.
///
/// Every Stop channel — Spacebar, the transport button, ARA, project close, a
/// sample-rate change, a session load — goes through
/// [`StudioLayout::stop_native_playback`], and this is the rule it applies, so
/// they cannot drift apart. Leaving a take running was what kept the writer
/// consuming input after Stop and made the next Record continue the same clip.
///
/// Count-in is the one exclusion: it is cancelled rather than finalized, and
/// `stop_native_recording` routes that case back through the stop path, so
/// excluding it here is also what stops the two recursing.
pub(super) fn stop_must_finalize_recording(
    ui_state: &crate::layout::RecordingUiState,
    recording_active: bool,
) -> bool {
    if matches!(ui_state, crate::layout::RecordingUiState::CountingIn { .. }) {
        return false;
    }
    recording_active
}

/// Watchdog timeout for an in-flight native engine sync. A `load_project` that
/// hangs (deadlock) never returns and would otherwise pin `sync_in_flight` and
/// the "Sync native engine" task forever. Kept below the loading-session
/// `GRAPH_SYNC_WAIT` (20s) so a stuck sync still resolves to a recoverable error
/// before the session-install wait gives up. Generous enough that a large
/// project / plugin instantiation does not trip a false positive.
const SYNC_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(15);

/// Why the transport playhead is being repositioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekReason {
    UserDragStart,
    UserDragging,
    UserDragEnd,
    TimelineClick,
    RewindForward,
    Programmatic,
}

/// Transient state for the vertical BPM scrub gesture (FL Studio–style infinite
/// drag). Extracted from `StudioLayout` as the first god-struct decomposition
/// slice — every access lives in this module. Built via [`Default`] at studio
/// construction; the drag handler overwrites the relevant fields per gesture.
#[derive(Debug)]
pub(crate) struct BpmDragState {
    /// Active drag id (matches `BpmDragSample::drag_id`); `None` when idle.
    pub active_id: Option<u64>,
    /// Previous cursor Y from the last sample — the per-move delta source.
    pub prev_y: f32,
    /// Accumulated signed BPM offset for the active drag.
    pub accum: f32,
    /// Screen-space cursor anchor (physical px) warped back each move so the
    /// drag never stops at the screen edge.
    pub anchor: Option<(i32, i32)>,
    /// Window scale factor captured at drag start.
    pub scale: f32,
    /// `Some(id)` edits one tempo marker; `None` edits the fixed project BPM.
    pub target_point_id: Option<String>,
    /// BPM value captured at drag start — the accumulation base.
    pub start_value: f32,
    /// Whole tempo state captured at drag start. The scrub writes tempo live so
    /// playback follows the pointer; this is what turns the finished gesture
    /// into ONE undo entry instead of one per mouse-move.
    pub gesture_origin: Option<TempoStateSnapshot>,
}

impl Default for BpmDragState {
    fn default() -> Self {
        Self {
            active_id: None,
            prev_y: 0.0,
            accum: 0.0,
            anchor: None,
            scale: 1.0,
            target_point_id: None,
            start_value: 120.0,
            gesture_origin: None,
        }
    }
}

/// Throttle / sync timestamps for engine ↔ UI bridging — last applied playhead,
/// last engine snapshot sync, last meter push, and last tempo commit. `Instant`
/// has no `Default`, so this is built via a manual `Default` (now()). Decomp
/// slice; all access lives in this module.
#[derive(Debug)]
pub(crate) struct EngineSyncState {
    /// Last engine playhead beat pushed into timeline state.
    pub playhead_beat: f32,
    /// Last time the engine snapshot was synced to the UI.
    pub synced_at: Instant,
    /// Last time engine meter levels were pushed into timeline state. Meter
    /// cadence is owned by the frame scheduler and remains independent of the
    /// render-cost profile so VU motion stays even on integrated GPUs.
    pub meter_applied_at: Instant,
    /// Quantised meter signature last pushed to meter-isolated UI regions.
    pub last_meter_notify_sig: u64,
    /// Last rendered footer signature (left/audio/perf pill).
    pub last_status_sig: u64,
    /// Left+audio footer signature for perf-only coalescing.
    pub last_status_left_audio_sig: u64,
    /// Last time footer text was pushed to the status entity.
    pub last_status_poll_at: Instant,
    /// Last time `engine.set_bpm` was sent during a live BPM drag (~30 Hz cap).
    pub bpm_committed_at: Option<Instant>,
    /// Last time the heavy plugin-editor / bridge reconciliation ran. The poll
    /// loop ticks at the display refresh (up to 240 Hz); this caps that
    /// bookkeeping to ~60 Hz so a high-refresh monitor doesn't multiply it.
    pub bridge_reconciled_at: Instant,
    /// Device/port discovery is much heavier than draining MIDI messages and
    /// does not need to run at display refresh.
    pub midi_devices_synced_at: Instant,
    /// `(bar, beat_in_bar)` currently shown by the transport chrome.
    ///
    /// The playhead moves every audio block, but the chrome only renders bar
    /// and beat — at 120 BPM that is two distinct values per second, not one
    /// per display refresh. Notifying the studio root on playhead motion made
    /// GPUI re-lay-out and repaint the entire window (timeline, docks, mixer)
    /// on every tick for a label that had not changed.
    pub displayed_bar_beat: (i64, u16),
}

impl Default for EngineSyncState {
    fn default() -> Self {
        Self {
            playhead_beat: 0.0,
            synced_at: Instant::now(),
            meter_applied_at: Instant::now(),
            last_meter_notify_sig: u64::MAX,
            last_status_sig: u64::MAX,
            last_status_left_audio_sig: u64::MAX,
            last_status_poll_at: Instant::now() - Duration::from_secs(1),
            bpm_committed_at: None,
            bridge_reconciled_at: Instant::now() - Duration::from_secs(1),
            midi_devices_synced_at: Instant::now() - Duration::from_secs(1),
            displayed_bar_beat: (i64::MIN, u16::MAX),
        }
    }
}

/// Audio-engine bridge / sync state — the live engine handle, transport stats,
/// last error, dirty flags driving project/media re-sync, and the background
/// sync handshake (in-flight / pending / play-after-sync). `StudioLayout`
/// decomposition slice. Manual `Default` (project/media start dirty so the first
/// sync runs).
pub(crate) struct AudioBridgeState {
    /// Live native audio engine handle; `None` until opened.
    pub engine: Option<DirectAudio::AudioEngine>,
    /// Whether the engine is currently running.
    pub running: bool,
    /// Last audio error surfaced to the UI.
    pub last_error: Option<String>,
    /// Latest engine transport/stats snapshot.
    pub stats: Option<DirectAudio::EngineStats>,
    /// Signature of the last project synced to the engine (skips redundant syncs).
    pub last_project_signature: Option<String>,
    /// Project graph changed and needs re-sync to the engine.
    pub project_dirty: bool,
    /// Media (clips/assets) changed and needs re-sync.
    pub media_dirty: bool,
    /// True while a background `load_project` (file decode) is running.
    pub sync_in_flight: bool,
    /// Fingerprint of the sync currently in flight (`Some` while `sync_in_flight`).
    /// Coalesce/dedup decisions for new requests are made against this.
    pub running_fingerprint: Option<u64>,
    /// At most one coalesced sync queued behind the in-flight one. Holds the graph
    /// fingerprint captured when it was queued; the reschedule rebuilds a fresh
    /// snapshot and re-checks. `None` when nothing is pending.
    pub pending_fingerprint: Option<u64>,
    /// Reason carried by the coalesced pending sync.
    pub pending_reason: Option<&'static str>,
    /// Preserves force=true for a pending sync queued behind an in-flight sync.
    pub pending_force: bool,
    /// Monotonic id of the in-flight sync. A `complete_audio_project_sync` whose
    /// generation no longer matches (the watchdog timeout fired and superseded it)
    /// is ignored — the freshness guard for an orphaned/hung load thread.
    pub sync_generation: u64,
    /// When the in-flight sync started — drives the watchdog timeout that prevents
    /// a hung `load_project` from pinning `sync_in_flight` (and the task) forever.
    pub sync_started_at: Option<Instant>,
    /// Last graph fingerprint whose load failed. Not auto-retried from the dirty
    /// poll (only a genuine graph change or an explicit forced sync retries it),
    /// so a failing graph cannot spin the resync loop.
    pub last_failed_fingerprint: Option<u64>,
    /// Monotonic UI-side route graph publication version for diagnostics.
    pub route_graph_version: u64,
    pub route_graph_in_flight_version: u64,
    pub route_graph_in_flight_child_channels: usize,
    pub route_graph_in_flight_master_routes: usize,
    /// Fingerprint of only the *routing* half of the snapshot — the tracks and
    /// their inserts, sends, and outputs. `route_graph_version` advances when
    /// this changes, and not merely when the snapshot does.
    ///
    /// Editing a MIDI note changes the snapshot (it carries the notes) but
    /// cannot change the graph, and the version was bumped for it anyway, which
    /// invalidated the mixer tree cache and forced a sidebar rebuild on every
    /// committed note.
    pub routing_fingerprint: Option<u64>,
    /// Fingerprint (cheap hash of the engine snapshot) of the last graph actually
    /// published to the audio thread. `None` until the first publish. Used to skip
    /// a redundant route-graph rebuild / `load_project` when the graph is unchanged
    /// — drives the `engine_dirty_poll` and `audio_sync_pending` dedup.
    pub graph_fingerprint: Option<u64>,
    /// Diagnostics counters (drained via `FUTUREBOARD_PERF_DEBUG` / perf HUD).
    /// `engine_sync` = background syncs actually started; `audio_load_project` =
    /// `engine.load_project` calls dispatched; `route_graph_rebuild` = route graph
    /// version bumps. A deduped (unchanged-graph) sync increments none of these.
    pub engine_sync_count: u64,
    pub audio_load_project_count: u64,
    pub route_graph_rebuild_count: u64,
    /// Single-flight diagnostics (drained via `crate::perf::count`). `request` =
    /// every `schedule_audio_project_sync` call past the engine check; `completed`
    /// / `failed` = terminal sync outcomes; `coalesced` = requests folded into the
    /// running/pending sync without starting a new one; `timeout` = watchdog firings.
    pub sync_request_count: u64,
    pub sync_completed_count: u64,
    pub sync_failed_count: u64,
    pub sync_coalesced_count: u64,
    pub sync_timeout_count: u64,
    /// Snapshot construction serializes the whole project. Coalesce rapid UI
    /// gestures (numeric scrubs, sliders) before doing that control-thread work.
    pub last_sync_snapshot_at: Option<Instant>,
    /// Reason string of the most recent sync request (diagnostics only).
    pub last_sync_reason: &'static str,
    /// Start transport once the current background sync completes.
    pub play_after_sync: bool,
    /// Last `EngineStats::dropout_count` seen, to detect new dropouts in the poll.
    pub last_dropout_count: u64,
    /// While set in the future, the status bar shows a coalesced "Audio dropout
    /// detected" notice. Refreshed each time the dropout counter advances, so a
    /// burst of dropouts shows one steady notice instead of flickering per poll.
    pub dropout_notice_until: Option<Instant>,
    /// Reason of the most recent dropout, for the status notice.
    pub last_dropout_reason: String,
    /// Preferred sample rate the user deferred via the "Later" button (Hz), or 0
    /// when nothing is pending. Shared with the Settings latency provider so the
    /// Preferences "restart pending" warning reflects live state. Treated as
    /// resolved once the active device rate matches it.
    pub sample_rate_deferred_target: Arc<AtomicU32>,
    /// While set in the future, the status bar shows a coalesced sample-rate
    /// notice (e.g. an active-vs-requested mismatch after re-opening).
    pub sample_rate_notice_until: Option<Instant>,
    /// Text of the most recent sample-rate notice.
    pub sample_rate_notice_text: String,
    /// One aggregated surface for Audio Connections / routing warnings. See
    /// [`crate::layout::routing_warnings`] — never one dialog per track.
    pub routing_warnings: crate::layout::routing_warnings::RoutingWarningState,
    /// Earliest instant the control poll may re-attempt opening a stream whose
    /// warm-up failed. `None` = attempt on the next poll.
    pub stream_warm_retry_at: Option<Instant>,
    /// Current retry spacing. Doubles per failed attempt up to
    /// [`STREAM_WARM_MAX_BACKOFF`]; `AudioEngine::start` opens the device on
    /// this thread, so a tight retry would stutter the UI while a device is
    /// genuinely missing.
    pub stream_warm_backoff: Option<Duration>,
    /// Last failure text logged for a retry, so a device that stays missing
    /// reports once instead of once per attempt.
    pub stream_warm_last_error: Option<String>,
}

impl Default for AudioBridgeState {
    fn default() -> Self {
        Self {
            engine: None,
            running: false,
            last_error: None,
            stats: None,
            last_project_signature: None,
            project_dirty: true,
            media_dirty: true,
            sync_in_flight: false,
            running_fingerprint: None,
            pending_fingerprint: None,
            pending_reason: None,
            pending_force: false,
            sync_generation: 0,
            sync_started_at: None,
            last_failed_fingerprint: None,
            route_graph_version: 0,
            route_graph_in_flight_version: 0,
            route_graph_in_flight_child_channels: 0,
            route_graph_in_flight_master_routes: 0,
            routing_fingerprint: None,
            graph_fingerprint: None,
            engine_sync_count: 0,
            audio_load_project_count: 0,
            route_graph_rebuild_count: 0,
            sync_request_count: 0,
            sync_completed_count: 0,
            sync_failed_count: 0,
            sync_coalesced_count: 0,
            sync_timeout_count: 0,
            last_sync_snapshot_at: None,
            last_sync_reason: "init",
            play_after_sync: false,
            last_dropout_count: 0,
            dropout_notice_until: None,
            last_dropout_reason: String::new(),
            sample_rate_deferred_target: Arc::new(AtomicU32::new(0)),
            sample_rate_notice_until: None,
            sample_rate_notice_text: String::new(),
            routing_warnings: Default::default(),
            stream_warm_retry_at: None,
            stream_warm_backoff: None,
            stream_warm_last_error: None,
        }
    }
}

/// Cheap, stable fingerprint of a serialized engine snapshot. Identical graphs
/// hash identically, so a re-sync of an unchanged graph can be skipped without a
/// full string compare. Control-thread only (never the audio callback).
pub(crate) fn graph_fingerprint_of(signature: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    signature.hash(&mut hasher);
    hasher.finish()
}

/// Longest a burst of edits may coalesce before the engine is re-published.
///
/// A scrub, a velocity drag, or a run of drawn notes emits dozens of commits a
/// second, and each one would otherwise cost a full-project snapshot plus a
/// JSON serialize of every note in the project, on the UI thread.
/// How often the plug-in bridge is reconciled and its event queue drained.
///
/// Backstop bookkeeping — closing editors whose track was deleted, forwarding
/// editor events, and picking up a transport key pressed inside an editor. It
/// is UI-sync work, not realtime, so it is capped at ~60 Hz rather than run at
/// the display refresh: on a 144 Hz panel that would otherwise triple it.
const BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(16);

const UI_GESTURE_SYNC_INTERVAL: Duration = Duration::from_millis(75);

/// Should this request build a snapshot now, or ride the open window?
///
/// Leading-edge on purpose: an isolated edit — the common case — publishes on
/// the spot, and only the second and later edits inside one window wait. The
/// caller leaves the graph dirty either way, so the 60 Hz engine poll publishes
/// the burst's final state as soon as the window closes; nothing is dropped.
///
/// `force` skips both gates and is for the paths that must publish an unchanged
/// graph (a device change, a retry after a failed load), not for edit volume.
pub(crate) fn sync_should_build_now(
    force: bool,
    dirty: bool,
    since_last_snapshot: Option<Duration>,
) -> bool {
    if force {
        return true;
    }
    // Nothing to publish. Dirty is cleared the moment a sync commits, so a
    // quiescent graph never reaches the (locking) snapshot build.
    if !dirty {
        return false;
    }
    since_last_snapshot.is_none_or(|elapsed| elapsed >= UI_GESTURE_SYNC_INTERVAL)
}

impl StudioLayout {
    pub(super) fn dispatch_midi_preview_command(
        &mut self,
        command: components::piano_roll::UiMidiPreviewCommand,
        cx: &App,
    ) {
        let ui_start = Instant::now();
        let track_id = command.track_id().to_string();
        let plugin_instance_id = self.resolve_track_instrument_plugin(&track_id, cx);
        let event = match command {
            components::piano_roll::UiMidiPreviewCommand::NoteOn {
                channel,
                pitch,
                velocity,
                ..
            } => MidiInputEvent::NoteOn {
                note: pitch,
                velocity,
                channel,
            },
            components::piano_roll::UiMidiPreviewCommand::NoteOff { channel, pitch, .. } => {
                MidiInputEvent::NoteOff {
                    note: pitch,
                    channel,
                }
            }
            components::piano_roll::UiMidiPreviewCommand::AllNotesOff { .. } => {
                MidiInputEvent::AllNotesOff
            }
            components::piano_roll::UiMidiPreviewCommand::MidiPanic { .. } => MidiInputEvent::Panic,
        };
        let result = self.route_midi_input_event(
            MidiInputSource::PianoRollPreview,
            MidiInputTarget {
                track_id,
                plugin_instance_id,
            },
            event,
            cx,
        );
        if let MidiInputRouteStatus::DispatchFailed(error) = result {
            eprintln!("[EngineMidiPreview] dispatch failed: {error}");
        }
        if crate::forensic_trace::preview_perf_trace_enabled() {
            eprintln!(
                "[preview-perf] ui_handler_ms={:.3}",
                ui_start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    #[cfg(any())]
    pub(super) fn dispatch_midi_preview_command_legacy(
        &mut self,
        command: components::piano_roll::UiMidiPreviewCommand,
        cx: &App,
    ) {
        let ui_start = Instant::now();
        let track_id = match &command {
            components::piano_roll::UiMidiPreviewCommand::NoteOn { track_id, .. }
            | components::piano_roll::UiMidiPreviewCommand::NoteOff { track_id, .. }
            | components::piano_roll::UiMidiPreviewCommand::AllNotesOff { track_id }
            | components::piano_roll::UiMidiPreviewCommand::MidiPanic { track_id } => {
                track_id.clone()
            }
        };
        let bridge_instance = self.resolve_track_instrument_plugin(&track_id, cx);
        // Per-note hot path: never block the GPUI thread on the bridge runtime
        // mutex. A background save / plugin-state capture (`request_plugin_states`,
        // ~1.5s) or a plugin load can hold it, and pressing notes or muting while
        // it was held used to freeze the UI. Probe with `try_lock`; if it is busy
        // but this track has a bridged instrument, route through the lock-free
        // engine command bus anyway (the correct path once a bridge plugin exists).
        let sink_ready = match self
            .plugin_editors
            .bridge_runtime
            .as_ref()
            .map(|rt| rt.try_lock())
        {
            Some(Ok(bridge)) => bridge
                .loaded_instance_ids()
                .into_iter()
                .any(|id| bridge.has_audio_sink(&id)),
            Some(Err(_)) => bridge_instance.is_some(),
            None => false,
        };

        if sink_ready {
            // Shared-memory bridge is live — always route through the main DAW
            // engine (track → mixer → master), even if timeline runtime_backend
            // has not caught up yet.
            let Some(engine) = self.audio_bridge.engine.as_ref() else {
                eprintln!("[midi-preview-ui] engine_command_bus_connected=false");
                return;
            };
            let instance_id = bridge_instance
                .clone()
                .unwrap_or_else(|| "bridge".to_string());
            let result = match &command {
                components::piano_roll::UiMidiPreviewCommand::NoteOn {
                    channel,
                    pitch,
                    velocity,
                    ..
                } => {
                    if crate::forensic_trace::preview_perf_trace_enabled() {
                        eprintln!(
                            "[midi-preview-ui] note_on track={track_id} pitch={pitch} velocity={velocity} source=piano_key"
                        );
                    }
                    engine.plugin_preview_note_on(
                        track_id.clone(),
                        instance_id.clone(),
                        *channel,
                        *pitch,
                        *velocity,
                    )
                }
                components::piano_roll::UiMidiPreviewCommand::NoteOff {
                    channel, pitch, ..
                } => {
                    if crate::forensic_trace::preview_perf_trace_enabled() {
                        eprintln!(
                            "[midi-preview-ui] note_off track={track_id} pitch={pitch} source=piano_key"
                        );
                    }
                    engine.plugin_preview_note_off(
                        track_id.clone(),
                        instance_id.clone(),
                        *channel,
                        *pitch,
                    )
                }
                components::piano_roll::UiMidiPreviewCommand::AllNotesOff { .. } => {
                    engine.plugin_preview_all_notes_off(track_id.clone(), instance_id.clone())
                }
                components::piano_roll::UiMidiPreviewCommand::MidiPanic { .. } => {
                    engine.plugin_preview_all_notes_off(track_id.clone(), instance_id.clone())
                }
            };
            if let Err(error) = result {
                eprintln!("[EngineMidiPreview] bridge dispatch failed: {error}");
            }
            if crate::forensic_trace::preview_perf_trace_enabled() {
                eprintln!(
                    "[preview-perf] ui_handler_ms={:.3}",
                    ui_start.elapsed().as_secs_f64() * 1000.0
                );
            }
            return;
        }

        if let Some(instance_id) = bridge_instance {
            if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
                // try_lock, not lock: this IPC fallback runs on the GPUI thread
                // and must not stall it if a background bridge op holds the mutex.
                // On contention we fall through to the engine MIDI-preview path.
                if let Ok(mut bridge) = runtime.try_lock() {
                    let result = match command {
                        components::piano_roll::UiMidiPreviewCommand::NoteOn {
                            channel,
                            pitch,
                            velocity,
                            ..
                        } => {
                            if crate::forensic_trace::preview_perf_trace_enabled() {
                                eprintln!(
                                    "[midi-preview-ui] note_on source=piano_roll pitch={pitch} velocity={velocity} instance={instance_id} (IPC fallback — DSP bridge pending)"
                                );
                            }
                            bridge.preview_note_on(instance_id.clone(), channel, pitch, velocity)
                        }
                        components::piano_roll::UiMidiPreviewCommand::NoteOff {
                            channel,
                            pitch,
                            ..
                        } => bridge.preview_note_off(instance_id.clone(), channel, pitch),
                        components::piano_roll::UiMidiPreviewCommand::AllNotesOff { .. } => {
                            bridge.preview_all_notes_off(instance_id.clone())
                        }
                        components::piano_roll::UiMidiPreviewCommand::MidiPanic { .. } => {
                            bridge.midi_panic(instance_id.clone())
                        }
                    };
                    if let Err(error) = result {
                        eprintln!("[plugin-bridge] midi preview dispatch failed: {error}");
                    }
                    return;
                }
            }
        }

        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            eprintln!("[PopoutMidiEditor] engine_command_bus_connected=false");
            return;
        };
        // Per-note preview dispatch: a piano-roll click auditions a note, so
        // these lines are charged per click and were unconditional. The trace is
        // the same one `preview_perf_trace_enabled` already gates.
        let preview_trace = crate::forensic_trace::preview_perf_trace_enabled();
        if preview_trace {
            eprintln!("[PopoutMidiEditor] engine_command_bus_connected=true");
        }
        let result = match command {
            components::piano_roll::UiMidiPreviewCommand::NoteOn {
                channel,
                pitch,
                velocity,
                ..
            } => {
                if preview_trace {
                    eprintln!(
                        "[PopoutMidiEditor] active_track_id={} dispatch PreviewNoteOn -> engine",
                        track_id
                    );
                }
                engine.midi_preview_note_on(track_id, channel, pitch, velocity)
            }
            components::piano_roll::UiMidiPreviewCommand::NoteOff { channel, pitch, .. } => {
                if preview_trace {
                    eprintln!(
                        "[PopoutMidiEditor] active_track_id={} dispatch PreviewNoteOff -> engine",
                        track_id
                    );
                }
                engine.midi_preview_note_off(track_id, channel, pitch)
            }
            components::piano_roll::UiMidiPreviewCommand::AllNotesOff { .. } => {
                if preview_trace {
                    eprintln!(
                        "[PopoutMidiEditor] active_track_id={} dispatch PreviewAllNotesOff -> engine",
                        track_id
                    );
                }
                engine.midi_preview_all_notes_off(track_id)
            }
            components::piano_roll::UiMidiPreviewCommand::MidiPanic { .. } => {
                engine.midi_preview_all_notes_off(track_id)
            }
        };
        if let Err(error) = result {
            eprintln!("[EngineMidiPreview] dispatch failed: {error}");
        }
    }

    fn bridge_instrument_instance_id(&self, track_id: &str, cx: &App) -> Option<String> {
        use crate::components::timeline::timeline_state::{PluginRuntimeBackend, TrackType};
        let timeline = self.timeline.read(cx);
        let track = timeline.state.find_track(track_id)?;
        let insert = match track.track_type {
            TrackType::Instrument => track.instrument_insert()?,
            TrackType::Midi => track.inserts.first()?,
            _ => return None,
        };
        if insert.runtime_backend != PluginRuntimeBackend::ExternalBridge {
            return None;
        }
        Some(insert.id.clone())
    }

    pub(super) fn resolve_track_instrument_plugin(
        &self,
        track_id: &str,
        cx: &App,
    ) -> Option<String> {
        let timeline = self.timeline.read(cx);
        let track = timeline.state.find_track(track_id)?;
        if let Some(instance_id) = track.instrument_plugin_instance_id.as_ref() {
            return Some(instance_id.clone());
        }
        if let Some(instance_id) = self.bridge_instrument_instance_id(track_id, cx) {
            return Some(instance_id);
        }
        match track.track_type {
            crate::components::timeline::timeline_state::TrackType::Instrument => track
                .instrument_insert()
                .filter(|slot| slot.plugin_id.is_some())
                .map(|slot| slot.id.clone()),
            crate::components::timeline::timeline_state::TrackType::Midi => track
                .inserts
                .first()
                .filter(|slot| slot.plugin_id.is_some())
                .map(|slot| slot.id.clone()),
            _ => None,
        }
        .or_else(|| {
            self.plugin_editors
                .bridge_runtime
                .as_ref()
                .and_then(|runtime| {
                    runtime
                        .try_lock()
                        .ok()
                        .and_then(|bridge| bridge.loaded_for_track(track_id))
                        .map(|loaded| loaded.descriptor.insert_id)
                })
        })
    }

    pub(super) fn spawn_audio_poll(cx: &mut Context<Self>) {
        // The poll cadence is owned by the frame scheduler (display-synced by
        // default, fixed/battery caps for settings/debug). The loop reads the
        // scheduler's lock-free interval each tick, so a mode change applies on
        // the next iteration. The engine produces position snapshots at
        // audio-block boundaries (~5-10 ms); the UI never repaints faster than
        // the chosen cadence and `interpolated_playhead_beat` smooths between
        // engine snapshots. Until the entity exists we fall back to ~60 Hz.
        const FALLBACK_INTERVAL_NANOS: u64 = 16_666_667; // ~60 Hz
        const IDLE_MIN_INTERVAL_NANOS: u64 = 16_666_667; // cap idle control work at 60 Hz
        const MIDI_RECORD_INTERVAL_NANOS: u64 = 2_000_000; // 2 ms while writing MIDI
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let mut interval_handle: Option<Arc<AtomicU64>> = None;
            let mut transport_active = false;
            let mut midi_recording_active = false;
            // Held only while the cadence below the OS timer tick is actually
            // wanted. Without it `executor.timer(6.9 ms)` waits for the next
            // ~15.6 ms Windows tick, so this loop — which is what schedules
            // every repaint during playback — runs at ~64 Hz at best whatever
            // the frame rate is set to. See
            // [`crate::frame_scheduler::TimerResolutionGuard`].
            let mut fast_timer: Option<crate::frame_scheduler::TimerResolutionGuard> = None;
            loop {
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    break;
                }
                if interval_handle.is_none() {
                    interval_handle = this
                        .update(cx, |this, _| this.frame_scheduler.continuous_nanos_handle())
                        .ok();
                }
                let requested_interval_nanos = interval_handle
                    .as_ref()
                    .map(|h| h.load(Ordering::Relaxed))
                    .filter(|n| *n > 0)
                    .unwrap_or(FALLBACK_INTERVAL_NANOS);
                // High-refresh polling is useful only while the transport is
                // moving. At idle, 60 Hz keeps hardware MIDI latency low while
                // avoiding 120/144/240 registry, meter, and status polls/sec.
                let interval_nanos = if midi_recording_active {
                    // Hardware MIDI arrives on a native callback thread, but
                    // recording is committed by this control loop. Keep the
                    // write path below one audio period without making every
                    // idle Studio poll run at 500 Hz.
                    MIDI_RECORD_INTERVAL_NANOS
                } else if transport_active {
                    requested_interval_nanos
                } else {
                    requested_interval_nanos.max(IDLE_MIN_INTERVAL_NANOS)
                };
                // Acquired and released with the cadence, so an idle Studio
                // does not keep the machine's timer at 1 ms.
                if crate::frame_scheduler::wants_high_timer_resolution(interval_nanos) {
                    fast_timer
                        .get_or_insert_with(crate::frame_scheduler::TimerResolutionGuard::acquire);
                } else {
                    fast_timer = None;
                }
                executor.timer(Duration::from_nanos(interval_nanos)).await;
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    break;
                }
                let Ok((changed, mixer_handle, active, midi_recording)) =
                    this.update(cx, |this, cx| {
                        if crate::shutdown::ShutdownState::global().is_shutting_down() {
                            return (false, None, false, false);
                        }
                        let changed = this.poll_native_audio(cx);
                        let active = this
                            .audio_bridge
                            .stats
                            .as_ref()
                            .map(|stats| stats.transport_playing)
                            .unwrap_or(false);
                        let midi_recording = this.recording.midi.is_some();
                        (
                            changed,
                            this.external_windows.mixer.clone(),
                            active,
                            midi_recording,
                        )
                    })
                else {
                    continue;
                };
                transport_active = active;
                midi_recording_active = midi_recording;
                if changed && !crate::shutdown::ShutdownState::global().is_shutting_down() {
                    crate::perf::record_notify("transport");
                    let studio_id = this.entity_id();
                    let _ = cx.update(|app| app.notify(studio_id));
                    if mixer_handle.is_some() {
                        let _ = this.update(cx, |layout, cx| {
                            layout.push_mixer_meter_snapshot_throttled(cx);
                        });
                    }
                }
                let _ = this.update(cx, |layout, cx| {
                    layout.push_video_player_snapshot_to_window(cx);
                    // Keeps Project Settings in step with tempo/meter edits made
                    // elsewhere (transport display, tempo track). Both pushes
                    // no-op when their window is closed.
                    layout.push_project_settings_snapshot_to_window(cx);
                });
            }
        })
        .detach();
    }

    /// Hardware MIDI gets its own lightweight control poll instead of waiting
    /// behind meters, bridge reconciliation, waveform work, and frame pacing.
    /// It idles at 8 ms and accelerates to 2 ms after activity, avoiding a
    /// permanent high-frequency UI wakeup while keeping live input responsive.
    pub(super) fn spawn_hardware_midi_input_poll(cx: &mut Context<Self>) {
        const MIDI_INPUT_ACTIVE_POLL: Duration = Duration::from_millis(2);
        const MIDI_INPUT_IDLE_POLL: Duration = Duration::from_millis(8);
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let mut interval = MIDI_INPUT_IDLE_POLL;
            loop {
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    break;
                }
                executor.timer(interval).await;
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    break;
                }
                let active = this
                    .update(cx, |this, cx| this.drain_hardware_midi_input(cx))
                    .unwrap_or(false);
                interval = if active {
                    MIDI_INPUT_ACTIVE_POLL
                } else {
                    MIDI_INPUT_IDLE_POLL
                };
            }
        })
        .detach();
    }

    pub(super) fn poll_native_audio(&mut self, cx: &mut Context<Self>) -> bool {
        if crate::shutdown::ShutdownState::global().is_shutting_down() {
            return false;
        }
        let _s = crate::perf::PerfScope::enter("poll_native_audio");
        // The plug-in bridge does not belong to the audio engine and must be
        // pumped even without one. Behind this early return, a session with no
        // device open never drained the host's event queue at all: editors sat
        // on "Loading" forever and every transport key pressed in one piled up
        // to fire as a burst whenever a device finally appeared.
        let bridge_due = self.engine_sync.bridge_reconciled_at.elapsed() >= BRIDGE_POLL_INTERVAL;
        if bridge_due {
            self.engine_sync.bridge_reconciled_at = Instant::now();
            self.reconcile_open_plugin_editors(cx);
            self.poll_plugin_bridge_runtime(cx);
            // Drive native main-owned editor shells: honor OS close + forward resizes.
            self.drive_bridge_editors(cx);
            // The plug-in editor windows' titlebar strips: what they show comes
            // from the engine and the project, and what their controls asked for
            // is applied here rather than in the window.
            self.drain_plugin_editor_chrome_actions(cx);
            self.refresh_plugin_editor_chrome(cx);
            // ARA plug-ins post transport requests and model updates from their
            // own threads; this is where those land on the UI thread.
            self.poll_ara(cx);
            // The one place Space becomes play/pause when it was pressed
            // somewhere GPUI's key bindings could not see it.
            //
            // A plug-in's native child owns every key that reaches it, so the
            // press is claimed before that window's procedure runs — by this
            // process's message hook, by an editor shell's own procedure, by the
            // C++ bridge windows, by CEF, or by the separated plug-in host over
            // IPC. All of them report to one router, which is what makes them
            // one press again; this is the only place that turns presses into
            // commands.
            //
            // The C++ bridge counter is drained here rather than by its claim
            // site because it is process-wide and has none: it is written by the
            // editor window procedures and was only ever read by the plug-in
            // host process, so every Space claimed from an in-process editor was
            // taken away from the plug-in and then dropped.
            for _ in 0..DirectAudio::vst3_processor::take_transport_toggle_requests() {
                crate::components::transport_key::claim(
                    crate::components::transport_key::TransportKeySource::NativeBridge,
                    None,
                );
            }
            if crate::components::transport_key::take() > 0 {
                // Coalesced, not replayed: more than one press waiting here is a
                // burst that piled up behind a plug-in's own modal loop, and
                // replaying it would toggle back to where it started.
                eprintln!("[Keyboard] transport key -> transport:play-pause");
                self.dispatch_command_id("transport:play-pause", cx);
            }
        }
        if self.audio_bridge.engine.is_none() {
            return false;
        }

        // Watchdog: a hung `load_project` must not pin the sync lifecycle. If the
        // in-flight sync has exceeded the timeout, fail it recoverably and free the
        // state so a retry can proceed and the engine returns to ready.
        if self.audio_bridge.sync_in_flight {
            if let Some(started) = self.audio_bridge.sync_started_at {
                if started.elapsed() >= SYNC_WATCHDOG_TIMEOUT {
                    self.timeout_audio_project_sync(cx);
                }
            }
        }

        if self.audio_bridge.project_dirty || self.audio_bridge.media_dirty {
            self.schedule_audio_project_sync(cx, false, "engine_dirty_poll");
        }

        let engine = self.audio_bridge.engine.as_ref().expect("checked above");
        // Throttled raw/bus input-peak trace (gated by FUTUREBOARD_INPUT_DEBUG).
        engine.log_input_debug();
        let stats = engine.stats();
        // State-transition signal — used to notify the root layout even
        // when the transport is paused (e.g. error appears, stream opens).
        let state_changed = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|previous| {
                previous.transport_playing != stats.transport_playing
                    || previous.running != stats.running
                    || previous.last_error != stats.last_error
            })
            .unwrap_or(true);
        self.audio_bridge.running = stats.running;
        self.audio_bridge.last_error = stats.last_error.clone();
        // The session's normal state is an open stream, whether or not the
        // transport is moving: input monitoring, instrument preview and
        // metering all depend on it. Re-assert it here rather than leaving a
        // failed warm-up waiting for the user to press Play.
        if stats.stream_open && stats.running {
            self.clear_stream_warm_retry();
        } else {
            self.retry_audio_stream_warm();
        }

        let engine_beat = stats.position_beats.max(0.0) as f32;
        let sync_changed = (engine_beat - self.engine_sync.playhead_beat).abs() > 0.0001
            || self
                .audio_bridge
                .stats
                .as_ref()
                .map(|previous| previous.transport_playing != stats.transport_playing)
                .unwrap_or(true);
        if sync_changed {
            self.engine_sync.playhead_beat = engine_beat;
            self.engine_sync.synced_at = Instant::now();
        }
        let meter_changed = self.apply_engine_meters(cx);

        // Drain hardware MIDI input on the UI/control poll (never the audio
        // thread). Opens/refreshes enabled ports, then routes into the same
        // preview + recording path as the virtual keyboard.
        if self.engine_sync.midi_devices_synced_at.elapsed() >= Duration::from_millis(500) {
            self.engine_sync.midi_devices_synced_at = Instant::now();
            self.sync_hardware_midi_input_devices(cx);
        }

        // Realtime recording waveform preview (Part 1) — grow the preview clip
        // and append streamed peaks. Self-contained; notifies the timeline.
        self.update_recording_preview(cx);
        // A take detached by Stop finishes on the writer's thread; this is
        // where its files are collected. Cheap when there is none: a bool.
        self.poll_recording_finalize(cx);

        let mut transport_display_changed = false;
        if stats.transport_playing {
            let bpm = {
                let timeline = self.timeline.read(cx);
                timeline.state.bpm
            };
            let interpolated = self.interpolated_playhead_beat(bpm);
            // What the transport chrome will actually display for this playhead.
            // Only a change here justifies repainting the studio root.
            let bar_beat = {
                let timeline = self.timeline.read(cx);
                let bb = timeline
                    .state
                    .time_signature_map
                    .bar_beat_at_beat(interpolated.max(0.0) as f64);
                (bb.bar, bb.beat_in_bar)
            };
            if self.engine_sync.displayed_bar_beat != bar_beat {
                self.engine_sync.displayed_bar_beat = bar_beat;
                transport_display_changed = true;
            }
            let playhead_moved = self.timeline.update(cx, move |timeline, cx| {
                timeline.state.transport.playing = true;
                // No threshold while playing — even sub-pixel beat motion
                // matters for the bar:beat:tick readout in the chrome.
                let next = interpolated.max(0.0);
                // Two kinds of change, and only one of them is the arrangement's
                // business. The playhead moving is a line translating over
                // content that has not changed; everything else here moves the
                // content itself.
                let mut arrangement_dirty = false;
                let mut playhead_moved = false;
                if timeline.state.transport.playhead_beats != next {
                    timeline.state.transport.playhead_beats = next;
                    playhead_moved = true;
                }
                // Follow Track Volume automation: refresh each track's effective
                // volume so the mixer/track-header/inspector fader track the curve
                // during playback. UI-only (faders read `display_volume`), so this
                // never writes the base value or fires a user-edit command.
                // Faders live in the track headers and the mixer, so a curve
                // moving one is a real arrangement change.
                if timeline
                    .state
                    .recompute_effective_volumes(next, "playback_tick")
                {
                    arrangement_dirty = true;
                }
                // Follow-playhead / auto-scroll. Keeps the playhead visible
                // during playback; user-scroll temporarily disables it via
                // `note_user_scrolled`. Cheap — no rebuild, just viewport
                // scroll_x update.
                // Auto-scroll moves the content under the playhead, so this one
                // does have to repaint the arrangement. In Page mode that is a
                // handful of frames per project; in Continuous mode it is every
                // frame, which is the cost of that mode rather than of playback.
                if timeline.state.update_auto_scroll_for_playhead(next) {
                    arrangement_dirty = true;
                }
                if arrangement_dirty {
                    // The repaint recomputes the playhead's x on its way past,
                    // so the overlay does not need telling twice.
                    cx.notify();
                } else if playhead_moved {
                    timeline.publish_playhead(cx);
                }
                playhead_moved
            });
            // The MIDI editors draw the same transport, and nothing else
            // reaches them on a tick. The docked one used to move only when the
            // shell happened to repaint, and the floating window — which has no
            // shell above it at all — stepped rather than swept, which is what
            // a stuttering playhead there actually was.
            //
            // Each roll owns a playhead entity, so this repaints a line and not
            // an editor full of notes. Both are published unconditionally: a
            // roll with no overlay yet, or one whose x has not moved a whole
            // pixel, returns false and costs a comparison.
            if playhead_moved {
                let docked = self.piano_roll.clone();
                let floating = self.piano_roll_floating.clone();
                crate::components::piano_roll::PianoRoll::publish_playhead(&docked, cx);
                crate::components::piano_roll::PianoRoll::publish_playhead(&floating, cx);
            }
        } else {
            let _ = self.timeline.update(cx, |timeline, cx| {
                if timeline.state.transport.playing {
                    timeline.state.transport.playing = false;
                    cx.notify();
                }
            });
            // Transport just stopped: the line stays where it is but is drawn
            // dimmer, and only the overlay needs to hear about it.
            let docked = self.piano_roll.clone();
            let floating = self.piano_roll_floating.clone();
            crate::components::piano_roll::PianoRoll::publish_playhead(&docked, cx);
            crate::components::piano_roll::PianoRoll::publish_playhead(&floating, cx);
        }

        // Coalesced dropout notice: when the realtime dropout counter advances,
        // hold a short status notice (refreshed on each new dropout) instead of
        // spamming the UI. Counters/atomics only — no audio-thread interaction.
        if stats.dropout_count > self.audio_bridge.last_dropout_count {
            let new_dropouts = stats.dropout_count - self.audio_bridge.last_dropout_count;
            self.audio_bridge.last_dropout_count = stats.dropout_count;
            self.audio_bridge.last_dropout_reason = stats.dropout_last_reason.clone();
            self.audio_bridge.dropout_notice_until = Some(Instant::now() + Duration::from_secs(4));
            crate::perf::count("audio_dropout_count", stats.dropout_count);
            crate::perf::count("audio_dropout_recent", new_dropouts);
        }

        self.audio_bridge.stats = Some(stats);

        if meter_changed {
            self.notify_mixer_meter_regions(cx);
        }

        let status_due = state_changed
            || self.engine_sync.last_status_poll_at.elapsed() >= Duration::from_millis(250);
        if status_due {
            self.notify_status_bar_if_changed(cx);
        }

        // Browser sample preview playhead. It is drawn only inside the cached
        // browser sidebar, so it notifies that region directly — folding it
        // into the shell's `changed` return rebuilt every panel in the window
        // at the poll rate for the whole length of an audition.
        if self.poll_browser_preview_playhead() {
            crate::perf::record_notify("browser-preview-playhead");
            self.notify_browser_sidebar(cx);
        }

        // The studio root owns the whole window, so notifying it makes GPUI
        // re-render, re-lay-out, and repaint every panel under it. Measured on
        // a 31-track session that pass is ~31 ms — 97% of the frame — which is
        // why this must be driven by what the root actually *shows*, not by
        // playhead motion.
        //
        // While playing, the only root-visible thing the playhead changes is
        // the chrome's bar.beat readout — and the chrome is now its own cached
        // region, so the beat repaints *it* rather than the shell. The playhead
        // line, auto-scroll, meters, and the status footer all reach their own
        // isolated entities above and repaint without the shell too.
        if transport_display_changed {
            crate::perf::record_notify("transport-readout");
            self.notify_app_chrome(cx);
        }
        state_changed
    }

    /// Block-rate automation evaluation scaffolding. Evaluates each track's
    /// Volume/Pan automation lane at the current playhead beat. Engine
    /// application — pushing the value into the realtime parameter queue — is
    /// the next phase; doing it here at 60 Hz unconditionally would both spam
    /// the engine and fight the UI fader, so for now we only trace under
    /// `FUTUREBOARD_AUTOMATION_DEBUG`. The evaluation itself is real (see
    /// [`evaluate_automation`]) so the wiring point is ready for the param
    /// queue without faking any data.
    pub(super) fn evaluate_block_automation(&self, cx: &mut Context<Self>, beat: f32) {
        use crate::components::timeline::timeline_state::{
            automation_debug_enabled, evaluate_automation, AutomationTarget,
        };
        if !automation_debug_enabled() {
            return;
        }
        let timeline = self.timeline.read(cx);
        for track in &timeline.state.tracks {
            for lane in &track.automation_lanes {
                if lane.points.is_empty()
                    || !matches!(
                        lane.target,
                        AutomationTarget::TrackVolume | AutomationTarget::TrackPan
                    )
                {
                    continue;
                }
                let value =
                    evaluate_automation(&lane.points, beat as f64, lane.target.default_value());
                eprintln!(
                    "[automation] evaluate track={} beat={:.3} value={:.3} target={}",
                    track.id,
                    beat,
                    value,
                    lane.target.display_name()
                );
                // TODO(engine): push `value` into the realtime parameter queue
                // for `track.id` (volume/pan) — lock-free, no allocations on the
                // audio-control path — instead of only tracing here.
            }
        }
    }

    pub(super) fn interpolated_playhead_beat(&self, bpm: f32) -> f32 {
        let playing = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.transport_playing)
            .unwrap_or(false);
        if !playing {
            return self.engine_sync.playhead_beat;
        }
        self.engine_sync.playhead_beat
            + self.engine_sync.synced_at.elapsed().as_secs_f32() * bpm.max(1.0) / 60.0
    }

    /// Update smoothed meter levels in timeline state. Does not call
    /// `cx.notify` — repaints are driven by the audio poll when transport
    /// is active, or by user interaction when idle.
    pub(super) fn apply_engine_meters(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return false;
        };
        // Meter reads are lightweight snapshot pulls; render cadence comes from
        // the frame scheduler so 120/144 Hz displays do not step at a fixed low
        // FPS. Ballistics below use the actual elapsed time, so throttling or
        // brief stalls do not change the apparent attack/release speed.
        // Live input and monitor meters can move while transport is stopped, so
        // they use the meter cadence in every transport state. Region-isolated
        // notifications below keep this from repainting the StudioLayout root.
        let min_interval = self.frame_scheduler.meter_min_interval();
        let now = Instant::now();
        let elapsed = now.duration_since(self.engine_sync.meter_applied_at);
        if elapsed < min_interval {
            return false;
        }
        self.engine_sync.meter_applied_at = now;
        let meter_dt = elapsed.as_secs_f32().clamp(1.0 / 240.0, 0.1);
        // Split the owned meter snapshot into its fields so the timeline closure
        // can take `tracks` by value without cloning `plugin_outputs` (consumed
        // afterwards). Moving fields is cheaper than the previous per-tick
        // `plugin_outputs.clone()` on this playback hot path. `master_peak_*` are
        // `f64` (Copy), so the forensic trace and the closure can both read them.
        let meters = engine.meters();
        let meter_tracks = meters.tracks;
        let plugin_output_meters = meters.plugin_outputs;
        let master_peak_l = meters.master_peak_l;
        let master_peak_r = meters.master_peak_r;
        // Control Room output peaks — measured after the monitor insert chain
        // and the monitor control processor, i.e. the signal actually leaving
        // for the monitoring hardware output. Deliberately not the live input
        // peak: the Control Room monitors the master bus, not a microphone.
        let monitor_peak_l = meters.monitor_peak_l;
        let monitor_peak_r = meters.monitor_peak_r;
        if crate::forensic_trace::forensic_trace_enabled() {
            for track_meter in &meter_tracks {
                let peak = track_meter.peak_l.max(track_meter.peak_r);
                if peak > 0.0001 {
                    eprintln!("[Meter] track={} peak={peak:.6}", track_meter.track_id);
                }
            }
            let master_peak = master_peak_l.max(master_peak_r);
            if master_peak > 0.0001 {
                eprintln!("[Meter] master peak={master_peak:.6}");
            }
        }
        let mut changed = self.timeline.update(cx, |timeline, _cx| {
            let mut changed = false;
            // Resolve every meter against one prebuilt id -> index map, in a
            // borrow that ends before the mutation pass below. The per-meter
            // linear `find` this replaces was O(tracks x meters), and this loop
            // runs at the display refresh: measured at the reported project
            // scale (2,103 channels) it cost 3.1 ms per tick versus 0.1 ms
            // here — several hundred milliseconds per second of UI thread spent
            // purely resolving ids.
            let resolved: Vec<Option<usize>> = {
                let index_by_id = timeline.state.track_index_by_id();
                meter_tracks
                    .iter()
                    .map(|track_meter| {
                        index_by_id.get(track_meter.track_id.as_str()).copied()
                    })
                    .collect()
            };
            for (track_meter, index) in meter_tracks.into_iter().zip(resolved) {
                if let Some(track) = index.and_then(|index| timeline.state.tracks.get_mut(index)) {
                    let next_l = track_meter.peak_l.clamp(0.0, 1.0) as f32;
                    let next_r = track_meter.peak_r.clamp(0.0, 1.0) as f32;
                    if crate::forensic_trace::forensic_trace_enabled()
                        && track.id.starts_with("vsti-out:")
                        && next_l.max(next_r) > 0.0001
                    {
                        let bus_index = track
                            .id
                            .rsplit_once(":bus:")
                            .and_then(|(_, bus)| bus.parse::<u8>().ok())
                            .unwrap_or(0);
                        let plugin_instance_id = track
                            .id
                            .strip_prefix("vsti-out:")
                            .and_then(|rest| rest.split_once(":bus:").map(|(plugin, _)| plugin))
                            .unwrap_or("");
                        eprintln!(
                            "[METER PUBLISH]\naudio_callback_seq=0\nplugin_instance_id={plugin_instance_id}\nbus_index={bus_index}\nmixer_channel_id={}\npeak_l={:.6}\npeak_r={:.6}\nrms_l={:.6}\nrms_r={:.6}\nsubscriber_count=1",
                            track.id,
                            track_meter.peak_l,
                            track_meter.peak_r,
                            track_meter.rms_l,
                            track_meter.rms_r
                        );
                    }
                    changed |= smooth_meter_value(&mut track.meter_level_l, next_l, meter_dt);
                    changed |= smooth_meter_value(&mut track.meter_level_r, next_r, meter_dt);
                    changed |= update_meter_hold(
                        &mut track.meter_peak_hold_l,
                        track.meter_level_l,
                        meter_dt,
                    );
                    changed |= update_meter_hold(
                        &mut track.meter_peak_hold_r,
                        track.meter_level_r,
                        meter_dt,
                    );
                    changed |= update_meter_clip(
                        &mut track.meter_clip,
                        track_meter.peak_l,
                        track_meter.peak_r,
                        track.meter_peak_hold_l.max(track.meter_peak_hold_r),
                    );
                }
            }
            let master = &mut timeline.state.master;
            changed |= smooth_meter_value(
                &mut master.meter_level_l,
                master_peak_l.clamp(0.0, 1.0) as f32,
                meter_dt,
            );
            changed |= smooth_meter_value(
                &mut master.meter_level_r,
                master_peak_r.clamp(0.0, 1.0) as f32,
                meter_dt,
            );
            changed |=
                update_meter_hold(&mut master.meter_peak_hold_l, master.meter_level_l, meter_dt);
            changed |=
                update_meter_hold(&mut master.meter_peak_hold_r, master.meter_level_r, meter_dt);
            changed |= update_meter_clip(
                &mut master.meter_clip,
                master_peak_l,
                master_peak_r,
                master.meter_peak_hold_l.max(master.meter_peak_hold_r),
            );
            let monitor = &mut timeline.state.monitor;
            changed |= smooth_meter_value(
                &mut monitor.meter_level_l,
                monitor_peak_l.clamp(0.0, 1.0) as f32,
                meter_dt,
            );
            changed |= smooth_meter_value(
                &mut monitor.meter_level_r,
                monitor_peak_r.clamp(0.0, 1.0) as f32,
                meter_dt,
            );
            changed |= update_meter_hold(
                &mut monitor.meter_peak_hold_l,
                monitor.meter_level_l,
                meter_dt,
            );
            changed |= update_meter_hold(
                &mut monitor.meter_peak_hold_r,
                monitor.meter_level_r,
                meter_dt,
            );
            changed |= update_meter_clip(
                &mut monitor.meter_clip,
                monitor_peak_l,
                monitor_peak_r,
                monitor.meter_peak_hold_l.max(monitor.meter_peak_hold_r),
            );
            changed
        });
        // Stamp every meter published this tick with the current generation,
        // then prune by generation below. The previous shape built two owned
        // `String`s per plugin output channel per tick (the key, plus a clone
        // into a live-key set); at the scale multi-output VSTi projects reach
        // that measured in the hundreds of microseconds per tick, at the
        // display refresh, on the UI thread.
        let generation = self.mixer_view.vsti_meter_generation.wrapping_add(1);
        self.mixer_view.vsti_meter_generation = generation;
        let mut key_buf = std::mem::take(&mut self.mixer_view.vsti_meter_key_buf);
        for meter in plugin_output_meters {
            let channel = meter.channel.clamp(1, 32) as u8;
            write_vsti_output_meter_key(&mut key_buf, &meter.track_id, &meter.insert_id, channel);
            let meters = &mut self.mixer_view.vsti_output_meters;
            if !meters.contains_key(key_buf.as_str()) {
                meters.insert(key_buf.clone(), VstiOutputMeterState::default());
            }
            let entry = meters
                .get_mut(key_buf.as_str())
                .expect("entry inserted above");
            entry.last_seen = generation;
            let next = meter.peak.clamp(0.0, 1.0) as f32;
            if crate::forensic_trace::forensic_trace_enabled() && next > 0.0001 {
                let bus_index = (channel.saturating_sub(1)) / 2;
                let mixer_channel_id =
                    crate::components::timeline::timeline_state::vsti_output_child_track_id(
                        &meter.insert_id,
                        bus_index,
                    );
                eprintln!(
                    "[METER PUBLISH]\naudio_callback_seq=0\nplugin_instance_id={}\nbus_index={}\nmixer_channel_id={}\npeak_l={:.6}\npeak_r={:.6}\nrms_l=0.000000\nrms_r=0.000000\nsubscriber_count=1",
                    meter.insert_id,
                    bus_index,
                    mixer_channel_id,
                    meter.peak,
                    meter.peak
                );
            }
            changed |= smooth_meter_value(&mut entry.level, next, meter_dt);
            changed |= update_meter_hold(&mut entry.peak_hold, entry.level, meter_dt);
            changed |= update_meter_clip(&mut entry.clip, meter.peak, meter.peak, entry.peak_hold);
        }
        self.mixer_view.vsti_output_meters.retain(|_key, meter| {
            if meter.last_seen == generation {
                return true;
            }
            // Not published this tick — ride the same decay to silence the
            // live-key set drove before, and drop once fully at rest.
            let mut keep = false;
            changed |= smooth_meter_value(&mut meter.level, 0.0, meter_dt);
            changed |= update_meter_hold(&mut meter.peak_hold, meter.level, meter_dt);
            changed |= update_meter_clip(&mut meter.clip, 0.0, 0.0, meter.peak_hold);
            if meter.level > 0.0 || meter.peak_hold > 0.0 || meter.clip {
                keep = true;
            }
            keep
        });
        // Hand the key buffer back so its allocation is reused next tick.
        self.mixer_view.vsti_meter_key_buf = key_buf;
        changed
    }

    /// Queue a background engine sync. `load_project` decodes media on the
    /// caller thread — never invoke it from the UI poll loop or render path.
    pub(crate) fn schedule_audio_project_sync(
        &mut self,
        cx: &mut Context<Self>,
        force: bool,
        reason: &'static str,
    ) {
        // Every committed MIDI note edit forces one of these, on the UI thread,
        // between the click and the frame that draws the note. Scoped so that
        // cost lands under its own name in the perf report instead of inside
        // whichever entity happened to be updating.
        let _scope = crate::perf::PerfScope::enter("EngineProjectSync");
        if crate::shutdown::ShutdownState::global().is_shutting_down() {
            return;
        }
        let Some(engine) = self.audio_bridge.engine.clone() else {
            self.audio_bridge.last_error = Some("audio engine unavailable".to_string());
            return;
        };
        self.audio_bridge.sync_request_count =
            self.audio_bridge.sync_request_count.saturating_add(1);
        crate::perf::count("sync_request_count", self.audio_bridge.sync_request_count);
        self.audio_bridge.last_sync_reason = reason;

        // Cheap gate plus the burst window; see `sync_should_build_now`. Both
        // leave `project_dirty` / `media_dirty` set, so the 60 Hz engine poll
        // publishes the last state of a burst once the window closes.
        if !sync_should_build_now(
            force,
            self.audio_bridge.project_dirty || self.audio_bridge.media_dirty,
            self.audio_bridge
                .last_sync_snapshot_at
                .map(|last| last.elapsed()),
        ) {
            return;
        }
        self.audio_bridge.last_sync_snapshot_at = Some(Instant::now());

        let sample_rate = self.current_audio_sample_rate();
        let graph_version_before = self.audio_bridge.route_graph_version;
        let project_root = self
            .project_folder
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let preferred_input_device = {
            let settings = self.settings.read(cx);
            settings.current.hardware.audio.device_in.clone()
        };
        let preferred_output_device = {
            let settings = self.settings.read(cx);
            settings.current.hardware.audio.device_out.clone()
        };
        if crate::perf::engine_sync_debug_enabled() {
            eprintln!(
                "[AudioSettings] selected input device = {:?}",
                preferred_input_device
            );
            eprintln!(
                "[AudioSettings] selected output device = {:?}",
                preferred_output_device
            );
        }
        // The ARA document has to track the arrangement it renders: a bound clip
        // that moved or was retimed must reach the plug-in before the engine
        // starts asking it for audio at the new position.
        self.refresh_ara_sessions(cx);

        let snapshot = {
            let timeline = self.timeline.read(cx);
            build_engine_project_snapshot(
                &timeline.state,
                sample_rate,
                project_root.as_deref(),
                Some(preferred_input_device.as_str()),
            )
        };
        log_engine_sync_snapshot(
            &snapshot,
            self.audio_bridge.project_dirty || self.audio_bridge.media_dirty,
            reason,
        );
        let signature = serde_json::to_string(&snapshot).unwrap_or_default();
        let fingerprint = graph_fingerprint_of(&signature);
        // A second, narrower fingerprint over the graph-shaped fields only. It
        // costs another serialize, but of the tracks alone — no clips, no notes
        // — and it buys the difference between "the project changed" and "the
        // routing changed", which are not the same question and had one answer.
        let routing_fingerprint = graph_fingerprint_of(
            &serde_json::to_string(&(&snapshot.tracks, &snapshot.routing)).unwrap_or_default(),
        );
        let routing_changed = self.audio_bridge.routing_fingerprint != Some(routing_fingerprint);

        // Single-flight coalescing. A sync is already running; fold this request
        // into at most one pending sync rather than starting a second. Only a
        // genuinely different graph (or an explicit force) needs to run after the
        // current sync — a poll re-fire for the same graph is counted and dropped.
        // Dirty is cleared here because the running/pending sync now represents this
        // request; that is what stops the poll from re-queuing every tick and
        // pinning "Sync native engine" forever.
        if self.audio_bridge.sync_in_flight {
            // "Changed" vs the graph currently being synced and any already-queued
            // pending graph — NOT vs the last *successful* graph, so a revert to a
            // previously-published state mid-sync is still queued (the running sync
            // is publishing a different graph and would otherwise win).
            let changed = force
                || (Some(fingerprint) != self.audio_bridge.running_fingerprint
                    && Some(fingerprint) != self.audio_bridge.pending_fingerprint);
            if changed {
                self.audio_bridge.pending_fingerprint = Some(fingerprint);
                self.audio_bridge.pending_reason = Some(reason);
                self.audio_bridge.pending_force |= force;
                self.queue_background_task(
                    "native-sync-pending",
                    crate::components::BackgroundTaskKind::NativeSync,
                    "Sync native engine",
                    Some("Queued behind current engine sync".to_string()),
                );
            } else {
                self.audio_bridge.sync_coalesced_count =
                    self.audio_bridge.sync_coalesced_count.saturating_add(1);
                crate::perf::count(
                    "sync_coalesced_count",
                    self.audio_bridge.sync_coalesced_count,
                );
            }
            self.audio_bridge.project_dirty = false;
            self.audio_bridge.media_dirty = false;
            return;
        }

        // Graph-unchanged / known-failed dedup. The same graph was already
        // published to the audio thread (or just failed to load), so skip the
        // route-graph rebuild and `load_project` entirely. This covers
        // `engine_dirty_poll` re-firing after a completed sync. Fingerprint is the
        // documented key; the exact signature check guards the (astronomically
        // unlikely) hash collision. A known-failed graph is not auto-retried from
        // the poll — only a real change (different fingerprint) or an explicit
        // forced sync retries it, so a failing load cannot spin the resync loop.
        if !force
            && ((self.audio_bridge.graph_fingerprint == Some(fingerprint)
                && self.audio_bridge.last_project_signature.as_deref() == Some(signature.as_str()))
                || self.audio_bridge.last_failed_fingerprint == Some(fingerprint))
        {
            self.audio_bridge.project_dirty = false;
            self.audio_bridge.media_dirty = false;
            if self.audio_bridge.play_after_sync {
                self.audio_bridge.play_after_sync = false;
                self.start_native_playback(cx);
            }
            return;
        }

        // Commit to a new sync. Clear dirty now (not at completion): the in-flight
        // sync captures this exact graph, so the poll must not re-trigger for it.
        // A real edit during the sync re-dirties and is coalesced into `pending`.
        self.audio_bridge.project_dirty = false;
        self.audio_bridge.media_dirty = false;
        self.audio_bridge.running_fingerprint = Some(fingerprint);
        self.audio_bridge.sync_generation = self.audio_bridge.sync_generation.wrapping_add(1);
        self.audio_bridge.sync_started_at = Some(Instant::now());
        let sync_generation = self.audio_bridge.sync_generation;
        self.audio_bridge.sync_in_flight = true;
        self.audio_bridge.engine_sync_count = self.audio_bridge.engine_sync_count.saturating_add(1);
        crate::perf::count("engine_sync_count", self.audio_bridge.engine_sync_count);
        self.audio_bridge.routing_fingerprint = Some(routing_fingerprint);
        if routing_changed {
            self.audio_bridge.route_graph_version =
                self.audio_bridge.route_graph_version.saturating_add(1);
            self.audio_bridge.route_graph_rebuild_count = self
                .audio_bridge
                .route_graph_rebuild_count
                .saturating_add(1);
            crate::perf::count(
                "route_graph_rebuild_count",
                self.audio_bridge.route_graph_rebuild_count,
            );
        }
        let graph_version_after = self.audio_bridge.route_graph_version;
        let num_plugin_child_channels = snapshot
            .tracks
            .iter()
            .filter(|track| track.id.starts_with("vsti-out:"))
            .count();
        let num_routes_to_master = snapshot
            .tracks
            .iter()
            .filter(|track| track.id.starts_with("vsti-out:") && track.output_track_id.is_none())
            .count();
        self.audio_bridge.route_graph_in_flight_version = graph_version_after;
        self.audio_bridge.route_graph_in_flight_child_channels = num_plugin_child_channels;
        self.audio_bridge.route_graph_in_flight_master_routes = num_routes_to_master;
        // The routing graph just advanced (tracks/buses/sends/plugin child outputs
        // changed). Push the new version to the Mixer Tree Sidebar so it rebuilds
        // once, now — this is what makes the tree appear on first Studio open: the
        // handoff built the cache before the real graph was ready, and nothing else
        // re-triggered it. Meter/fader updates don't reach here (graph unchanged →
        // deduped above), and a content-only sync no longer reaches it either.
        if routing_changed {
            self.refresh_mixer_tree_sidebar_entity(cx);
        }
        if routing_changed && crate::perf::engine_sync_debug_enabled() {
            eprintln!(
                "[ROUTE GRAPH REBUILD]\nreason={reason}\ngraph_version_before={graph_version_before}\ngraph_version_after={graph_version_after}\nnum_plugin_child_channels={num_plugin_child_channels}\nnum_routes_to_master={num_routes_to_master}\npublished_to_audio_thread=false"
            );
        }
        self.start_background_task(
            "native-sync",
            crate::components::BackgroundTaskKind::NativeSync,
            "Sync native engine",
            Some(reason.to_string()),
            None,
            false,
        );
        self.audio_bridge.audio_load_project_count =
            self.audio_bridge.audio_load_project_count.saturating_add(1);
        crate::perf::count(
            "audio_load_project_count",
            self.audio_bridge.audio_load_project_count,
        );
        let owner = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            // On the background executor, and awaited there.
            //
            // This used to spawn an OS thread and immediately `join()` it. But
            // `Context::spawn` runs its future on the *foreground* executor —
            // the UI thread — so the join parked the interface for the whole of
            // `load_project`: the graph rebuild, plug-in re-instantiation and
            // media decode. Every committed MIDI note that got past the sync
            // throttle paid that, which is what made drawing notes feel like it
            // was waiting on the audio backend. It was.
            //
            // `catch_unwind` keeps the guarantee the `join()` gave: a panic in
            // the load is reported as a failed sync rather than taking the
            // process with it.
            let result = cx
                .background_executor()
                .spawn(async move {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                        engine.load_project(snapshot)
                    }))
                    .unwrap_or_else(|_| {
                        Err(DirectAudio::SphereAudioError::NativeError(
                            "audio project load panicked".to_string(),
                        ))
                    })
                })
                .await;
            let _ = owner.update(cx, |this, cx| {
                this.complete_audio_project_sync(cx, result, signature, sync_generation);
            });
            if !crate::shutdown::ShutdownState::global().is_shutting_down() {
                let studio_id = owner.entity_id();
                let _ = cx.update(|app| app.notify(studio_id));
            }
        })
        .detach();
    }

    pub(super) fn complete_audio_project_sync(
        &mut self,
        cx: &mut Context<Self>,
        result: Result<(), DirectAudio::SphereAudioError>,
        signature: String,
        generation: u64,
    ) {
        // Freshness guard: ignore a completion that belongs to a superseded sync.
        // The watchdog timeout (`timeout_audio_project_sync`) bumps the generation
        // and frees the lifecycle when a `load_project` hangs; if that orphaned
        // thread later returns, this stale completion must not clobber the current
        // state or re-flag a task that already reached a terminal status.
        if generation != self.audio_bridge.sync_generation {
            return;
        }
        self.audio_bridge.sync_in_flight = false;
        self.audio_bridge.sync_started_at = None;
        let finished_fingerprint = self.audio_bridge.running_fingerprint.take();
        match result {
            Ok(()) => {
                self.audio_bridge.sync_completed_count =
                    self.audio_bridge.sync_completed_count.saturating_add(1);
                crate::perf::count(
                    "sync_completed_count",
                    self.audio_bridge.sync_completed_count,
                );
                eprintln!(
                    "[ROUTE GRAPH REBUILD]\nreason=audio_project_sync_complete\ngraph_version_before={}\ngraph_version_after={}\nnum_plugin_child_channels={}\nnum_routes_to_master={}\npublished_to_audio_thread=true",
                    self.audio_bridge
                        .route_graph_in_flight_version
                        .saturating_sub(1),
                    self.audio_bridge.route_graph_in_flight_version,
                    self.audio_bridge.route_graph_in_flight_child_channels,
                    self.audio_bridge.route_graph_in_flight_master_routes
                );
                // Record the published graph's fingerprint so a later
                // engine_dirty_poll for the identical graph is deduped in
                // `schedule_audio_project_sync` (no second rebuild). A successful
                // publish also clears any prior failed-graph marker.
                self.audio_bridge.graph_fingerprint =
                    finished_fingerprint.or(Some(graph_fingerprint_of(&signature)));
                self.audio_bridge.last_project_signature = Some(signature);
                self.audio_bridge.last_failed_fingerprint = None;
                self.audio_bridge.last_error = None;
                self.complete_background_task(
                    "native-sync",
                    Some("Engine graph ready".to_string()),
                );
            }
            Err(error) => {
                self.audio_bridge.sync_failed_count =
                    self.audio_bridge.sync_failed_count.saturating_add(1);
                crate::perf::count("sync_failed_count", self.audio_bridge.sync_failed_count);
                // Remember the failing graph so the poll does not re-run it every
                // tick; a real change or an explicit forced retry resets this.
                self.audio_bridge.last_failed_fingerprint = finished_fingerprint;
                self.audio_bridge.last_error = Some(error.to_string());
                eprintln!("[audio] load_project failed: {error}");
                self.fail_background_task("native-sync", error.to_string());
            }
        }
        // Dirty was already cleared when the sync was committed; a real edit during
        // the sync re-dirtied and was coalesced into `pending_fingerprint` below.

        // Phase 2b: read back per-insert instantiation status now that the
        // runtime graph reflects this snapshot. A native-plugin insert that
        // the engine reports as not-ready failed to instantiate — flip its
        // UI slot to `Failed` (no panic; just surfaces the error).
        self.apply_engine_insert_statuses(cx);

        // Project load can finish plugin restore before the audio engine is
        // warm; re-bind bridge sinks whenever a graph swap completes.
        if self.sync_plugin_bridge_sinks_to_engine(cx, "audio_project_sync_complete") {
            eprintln!("[PluginRestore] graph snapshot swapped generation=post_sync");
        }

        // Reschedule a coalesced pending request — but only if it would actually do
        // something. The captured pending fingerprint is a cheap pre-gate; the real
        // dedup happens inside `schedule_audio_project_sync` against a freshly built
        // snapshot. Re-flag dirty so that call clears the cheap dirty gate.
        let pending = self.audio_bridge.pending_fingerprint.take();
        let pending_force = std::mem::take(&mut self.audio_bridge.pending_force);
        let pending_reason = self
            .audio_bridge
            .pending_reason
            .take()
            .unwrap_or("audio_sync_pending");
        if pending.is_some() {
            self.complete_background_task("native-sync-pending", None);
        }
        let needs_pending = match pending {
            Some(pf) => {
                pending_force
                    || (self.audio_bridge.graph_fingerprint != Some(pf)
                        && self.audio_bridge.last_failed_fingerprint != Some(pf))
            }
            None => false,
        };
        if needs_pending {
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, pending_force, pending_reason);
            return;
        }

        if self.audio_bridge.play_after_sync {
            self.audio_bridge.play_after_sync = false;
            self.start_native_playback(cx);
        }
    }

    /// Watchdog: free the sync lifecycle when a `load_project` hangs (a true
    /// deadlock never returns, so the spawned join would otherwise pin
    /// `sync_in_flight` and the "Sync native engine" task forever). Bumping the
    /// generation makes the eventual (orphaned) completion a no-op. The engine
    /// thread stays alive; the user can retry via any forced sync / reopening
    /// audio settings. Driven from `poll_native_audio`.
    pub(super) fn timeout_audio_project_sync(&mut self, cx: &mut Context<Self>) {
        if !self.audio_bridge.sync_in_flight {
            return;
        }
        self.audio_bridge.sync_generation = self.audio_bridge.sync_generation.wrapping_add(1);
        self.audio_bridge.sync_in_flight = false;
        self.audio_bridge.sync_started_at = None;
        self.audio_bridge.last_failed_fingerprint = self.audio_bridge.running_fingerprint.take();
        self.audio_bridge.pending_fingerprint = None;
        self.audio_bridge.pending_reason = None;
        self.audio_bridge.pending_force = false;
        self.audio_bridge.project_dirty = false;
        self.audio_bridge.media_dirty = false;
        self.audio_bridge.sync_timeout_count =
            self.audio_bridge.sync_timeout_count.saturating_add(1);
        crate::perf::count("sync_timeout_count", self.audio_bridge.sync_timeout_count);
        self.audio_bridge.last_error = Some("native engine sync timed out".to_string());
        eprintln!(
            "[audio] native engine sync timed out after {}s (recoverable) reason={}",
            SYNC_WATCHDOG_TIMEOUT.as_secs(),
            self.audio_bridge.last_sync_reason
        );
        self.fail_background_task("native-sync", "Native engine sync timed out (recoverable)");
        self.complete_background_task("native-sync-pending", None);
        cx.notify();
    }

    /// Read structured per-insert status from the engine and reconcile each
    /// UI slot's `load_status` (Phase 2b). Only native-plugin inserts are
    /// reconciled — built-ins are always live, and the stub / unscanned slots
    /// aren't sent to the engine so they keep their optimistic UI status.
    /// Runs on the UI thread right after a sync completes — not in the poll
    /// loop — so the runtime mutex is locked at most once per project change.
    pub(super) fn apply_engine_insert_statuses(&mut self, cx: &mut Context<Self>) {
        use crate::components::timeline::timeline_state::InsertLoadStatus;
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return;
        };
        let statuses = engine.insert_statuses();
        if statuses.is_empty() {
            return;
        }
        let mut changed = false;
        self.timeline.update(cx, |timeline, _cx| {
            for st in &statuses {
                if !st.native {
                    continue;
                }
                let status = if st.ready {
                    InsertLoadStatus::Ready
                } else {
                    InsertLoadStatus::Failed("Plugin failed to instantiate".to_string())
                };
                if timeline
                    .state
                    .set_insert_load_status(&st.track_id, &st.insert_id, status)
                {
                    changed = true;
                }
            }
        });
        if changed {
            cx.notify();
        }
    }

    pub(super) fn mark_engine_project_dirty(&mut self) {
        self.audio_bridge.project_dirty = true;
    }

    /// Push an insert's effective bypass state (`enabled && !bypassed`) to the
    /// live engine as the runtime "enabled" insert param. The plugin instance
    /// stays loaded; the render pass skips (or resumes) it from the next block.
    /// This is the cheap live path for bypass/enable toggles — callers must NOT
    /// also set `audio_bridge.project_dirty`, which would trigger a full
    /// `load_project` graph rebuild for a non-structural change.
    pub(super) fn push_insert_enabled_to_engine(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.audio_bridge.engine.clone() else {
            return;
        };
        let effective = self
            .timeline
            .read(cx)
            .state
            .find_insert_slot(track_id, insert_id)
            .map(|slot| slot.enabled && !slot.bypassed);
        let Some(effective) = effective else {
            return;
        };
        if let Err(error) = engine.set_insert_param(
            track_id.to_string(),
            insert_id.to_string(),
            "enabled".to_string(),
            if effective { 1.0 } else { 0.0 },
        ) {
            eprintln!(
                "[audio] insert enabled update failed: track={track_id} insert={insert_id} error={error}"
            );
        }
    }

    pub(crate) fn mark_engine_media_dirty(&mut self) {
        self.audio_bridge.project_dirty = true;
        self.audio_bridge.media_dirty = true;
    }

    /// First retry spacing after a failed warm-up. Short, because the usual
    /// cause is transient: an in-studio project switch closes the previous
    /// device and the replacement engine opens before the driver released it.
    const STREAM_WARM_FIRST_RETRY: Duration = Duration::from_millis(300);
    /// Ceiling for the retry spacing once a device is persistently unavailable.
    const STREAM_WARM_MAX_BACKOFF: Duration = Duration::from_secs(3);

    /// Bring the session back to its normal state — an open, running stream —
    /// after a warm-up that failed at install.
    ///
    /// `build_and_warm_audio_engine` deliberately does not fail the session
    /// when `AudioEngine::start` fails; it logs "will retry on first Play" and
    /// hands back an engine whose stream is closed. But the only caller of
    /// [`Self::ensure_audio_stream_warm`] was the transport, so the retry
    /// waited on a *user gesture*: nothing was audible — no input monitoring,
    /// no instrument preview, no metering — until someone pressed Play. The
    /// retry belongs on the session's own control poll.
    ///
    /// Control thread only, and never from the audio callback.
    fn retry_audio_stream_warm(&mut self) {
        if crate::shutdown::ShutdownState::global().is_shutting_down() {
            return;
        }
        // A session still installing owns the device handoff; opening one here
        // would race the loader's own engine.
        if !self.session_install_status.is_ready() {
            return;
        }
        let now = Instant::now();
        if self
            .audio_bridge
            .stream_warm_retry_at
            .is_some_and(|next| now < next)
        {
            return;
        }
        let backoff = self
            .audio_bridge
            .stream_warm_backoff
            .map(|previous| (previous * 2).min(Self::STREAM_WARM_MAX_BACKOFF))
            .unwrap_or(Self::STREAM_WARM_FIRST_RETRY);
        self.audio_bridge.stream_warm_backoff = Some(backoff);
        self.audio_bridge.stream_warm_retry_at = Some(now + backoff);

        if self.ensure_audio_stream_warm() {
            eprintln!("[audio] stream opened by session retry after a failed warm-up");
            self.clear_stream_warm_retry();
            return;
        }
        // Report a device that stays unavailable once, not once per attempt.
        if self.audio_bridge.last_error != self.audio_bridge.stream_warm_last_error {
            self.audio_bridge
                .stream_warm_last_error
                .clone_from(&self.audio_bridge.last_error);
            eprintln!(
                "[audio] stream still closed after retry: {}",
                self.audio_bridge
                    .last_error
                    .as_deref()
                    .unwrap_or("unknown error")
            );
        }
    }

    /// Called once the stream is confirmed open, so the next session (or the
    /// next device loss) retries promptly instead of inheriting a long backoff.
    fn clear_stream_warm_retry(&mut self) {
        self.audio_bridge.stream_warm_retry_at = None;
        self.audio_bridge.stream_warm_backoff = None;
        self.audio_bridge.stream_warm_last_error = None;
    }

    pub(super) fn ensure_audio_stream_warm(&mut self) -> bool {
        transport_freeze_debug::log("ensure_audio_stream_warm enter");
        let stream_ready = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.stream_open && stats.running)
            .unwrap_or(false)
            || self.audio_bridge.running;
        if stream_ready {
            transport_freeze_debug::log("ensure_audio_stream_warm already warm");
            return true;
        }

        let Some(engine) = self.audio_bridge.engine.as_mut() else {
            self.audio_bridge.last_error = Some("audio engine unavailable".to_string());
            transport_freeze_debug::log("ensure_audio_stream_warm no engine");
            return false;
        };
        transport_freeze_debug::log("ensure_audio_stream_warm before engine.start");
        // `AudioEngine::start` resumes an open stream without reopening the
        // device or rebuilding/decoding the runtime graph on this thread.
        match engine.start() {
            Ok(()) => {
                transport_freeze_debug::log("ensure_audio_stream_warm after engine.start ok");
                self.audio_bridge.stats = Some(engine.stats());
                self.audio_bridge.running = true;
                self.audio_bridge.last_error = None;
                true
            }
            Err(error) => {
                self.audio_bridge.running = false;
                self.audio_bridge.last_error = Some(error.to_string());
                eprintln!("[audio] stream warm-up failed: {error}");
                transport_freeze_debug::log("ensure_audio_stream_warm engine.start failed");
                false
            }
        }
    }

    fn start_hardware_midi_playback(&mut self, playhead_beats: f32, cx: &mut Context<Self>) {
        let (start_sample, sample_rate) = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| (stats.position_samples, stats.sample_rate.max(1)))
            .unwrap_or_else(|| {
                let sample_rate = self.current_audio_sample_rate().max(1);
                let start_sample = {
                    let timeline = self.timeline.read(cx);
                    timeline.state.tempo_map.samples_at_beat(
                        playhead_beats.max(0.0) as f64,
                        timeline.state.bpm as f64,
                        sample_rate as f64,
                    )
                };
                (start_sample, sample_rate)
            });
        let mut events = self.build_hardware_midi_events(playhead_beats, sample_rate, cx);
        for event in &mut events {
            if event.delay_seconds <= 0.0 && event.absolute_sample < start_sample {
                event.absolute_sample = start_sample;
            }
        }
        if std::env::var_os("FUTUREBOARD_MIDI_OUTPUT_DEBUG").is_some() {
            eprintln!(
                "[midi-output] start hardware playback events={} playhead_beats={:.3} start_sample={} sample_rate={}",
                events.len(),
                playhead_beats,
                start_sample,
                sample_rate,
            );
        }
        self.hardware_midi_playback
            .start_at_sample(events, start_sample, sample_rate);
    }

    fn stop_hardware_midi_playback(&mut self) {
        self.hardware_midi_playback.stop();
    }

    fn build_hardware_midi_events(
        &self,
        playhead_beats: f32,
        sample_rate: u32,
        cx: &mut Context<Self>,
    ) -> Vec<sphere_midi_service::HardwareMidiEvent> {
        let enabled_outputs = {
            let settings = self.settings.read(cx);
            let detected = crate::device_registry::cached_midi_devices();
            sphere_midi_service::resolve_midi_devices(
                &settings.current.hardware.midi.devices,
                &detected,
            )
            .into_iter()
            .filter(|d| {
                d.enabled
                    && d.connected
                    && matches!(
                        d.direction,
                        crate::settings::MidiDeviceDirection::Output
                            | crate::settings::MidiDeviceDirection::InputOutput
                    )
            })
            .flat_map(|d| [d.id, d.name])
            .collect::<HashSet<_>>()
        };
        if enabled_outputs.is_empty() {
            return Vec::new();
        }

        let timeline = self.timeline.read(cx);
        let state = &timeline.state;
        let base_bpm = state.bpm as f64;
        let playhead_seconds = state
            .tempo_map
            .seconds_at_beat(playhead_beats.max(0.0) as f64, base_bpm);
        let has_solo = state.tracks.iter().any(|track| track.solo);
        let mut events = Vec::new();

        for track in &state.tracks {
            if track.track_type != TrackType::Midi || track.muted || (has_solo && !track.solo) {
                continue;
            }
            let TrackOutputRouting::HardwareOutput { device_id, .. } = &track.routing.output else {
                continue;
            };
            if !enabled_outputs.contains(device_id) {
                continue;
            }
            let output_mode = track.routing.output_channel_mode();
            for clip in &track.clips {
                if clip.muted || clip.start_beat + clip.duration_beats <= playhead_beats {
                    continue;
                }
                let ClipType::Midi { notes, .. } = &clip.clip_type else {
                    continue;
                };
                for note in notes.iter().filter(|note| !note.muted) {
                    let note_start = clip.start_beat + note.start.max(0.0);
                    let note_end = (note_start + note.duration.max(0.0))
                        .min(clip.start_beat + clip.duration_beats);
                    if note_end <= playhead_beats || note_end <= note_start {
                        continue;
                    }
                    let channel = output_mode.resolve(note.channel).raw().min(15);
                    let pitch = note.pitch.min(127);
                    let velocity = note.velocity.clamp(1, 127);
                    if note_start <= playhead_beats {
                        events.push(sphere_midi_service::HardwareMidiEvent {
                            device_id: device_id.clone(),
                            delay_seconds: 0.0,
                            beat: playhead_beats.max(0.0) as f64,
                            absolute_sample: state.tempo_map.samples_at_beat(
                                playhead_beats.max(0.0) as f64,
                                base_bpm,
                                sample_rate as f64,
                            ),
                            message: vec![0x90 | channel, pitch, velocity],
                        });
                    } else {
                        let start_seconds = state
                            .tempo_map
                            .seconds_at_beat(note_start.max(0.0) as f64, base_bpm);
                        events.push(sphere_midi_service::HardwareMidiEvent {
                            device_id: device_id.clone(),
                            delay_seconds: (start_seconds - playhead_seconds).max(0.0),
                            beat: note_start as f64,
                            absolute_sample: state.tempo_map.samples_at_beat(
                                note_start.max(0.0) as f64,
                                base_bpm,
                                sample_rate as f64,
                            ),
                            message: vec![0x90 | channel, pitch, velocity],
                        });
                    }
                    let end_seconds = state
                        .tempo_map
                        .seconds_at_beat(note_end.max(0.0) as f64, base_bpm);
                    events.push(sphere_midi_service::HardwareMidiEvent {
                        device_id: device_id.clone(),
                        delay_seconds: (end_seconds - playhead_seconds).max(0.0),
                        beat: note_end as f64,
                        absolute_sample: state.tempo_map.samples_at_beat(
                            note_end.max(0.0) as f64,
                            base_bpm,
                            sample_rate as f64,
                        ),
                        message: vec![0x80 | channel, pitch, 0],
                    });
                }
            }
        }

        events
    }

    pub(super) fn current_audio_sample_rate(&self) -> u32 {
        self.audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.sample_rate)
            .filter(|sample_rate| *sample_rate > 0)
            .or_else(|| {
                self.audio_bridge
                    .engine
                    .as_ref()
                    .map(|engine| engine.config().sample_rate)
                    .filter(|sample_rate| *sample_rate > 0)
            })
            .unwrap_or(48_000)
    }

    pub(super) fn start_native_playback(&mut self, cx: &mut Context<Self>) {
        if !self.session_install_status.is_ready() {
            eprintln!("[SessionLoad] play blocked during session install");
            return;
        }
        transport_freeze_debug::reset_sequence();
        transport_freeze_debug::log("Play requested");
        eprintln!("[transport] Play requested");
        {
            let timeline = self.timeline.read(cx);
            crate::forensic_trace::dump_midi_model(&timeline.state);
        }

        if self.audio_bridge.engine.is_none() {
            self.audio_bridge.last_error = Some("audio engine unavailable".to_string());
            transport_freeze_debug::log("abort: no audio engine");
            return;
        }

        let already_playing = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.transport_playing)
            .unwrap_or(false);
        if already_playing {
            transport_freeze_debug::log("abort: already playing (idempotent)");
            return;
        }

        transport_freeze_debug::log("before sync/dirty gates");
        if self.audio_bridge.sync_in_flight {
            self.audio_bridge.play_after_sync = true;
            transport_freeze_debug::log("defer: audio_sync_in_flight");
            return;
        }
        if self.audio_bridge.project_dirty || self.audio_bridge.media_dirty {
            self.audio_bridge.play_after_sync = true;
            transport_freeze_debug::log("defer: schedule_audio_project_sync");
            self.schedule_audio_project_sync(cx, false, "transport_play_pending_sync");
            return;
        }

        transport_freeze_debug::log("before ensure_audio_stream_warm");
        if !self.ensure_audio_stream_warm() {
            transport_freeze_debug::log("abort: stream warm-up failed");
            return;
        }

        transport_freeze_debug::log("after ensure_audio_stream_warm");

        // Read transport state once; do not hold timeline lease across engine I/O.
        let (
            playhead_beats,
            bpm,
            tempo_points,
            metronome_enabled,
            loop_enabled,
            loop_start_beats,
            loop_end_beats,
            ts_points,
        ) = {
            transport_freeze_debug::log("before timeline.read");
            let timeline = self.timeline.read(cx);
            transport_freeze_debug::log("after timeline.read");
            let tempo_points = timeline
                .state
                .tempo_map
                .points
                .iter()
                .map(|p| DirectAudio::types::EngineTempoPointSnapshot {
                    beat: p.beat,
                    bpm: p.bpm,
                    curve: p.curve.to_tag(),
                })
                .collect::<Vec<_>>();
            let ts_points = timeline
                .state
                .time_signature_map
                .points
                .iter()
                .map(
                    |p| DirectAudio::time_signature_map::RuntimeTimeSignaturePointSnapshot {
                        beat: p.beat,
                        numerator: p.numerator,
                        denominator: p.denominator,
                        grouping: p.effective_grouping(),
                    },
                )
                .collect::<Vec<_>>();
            (
                timeline.state.transport.playhead_beats,
                timeline.state.bpm,
                tempo_points,
                timeline.state.transport.metronome_enabled,
                timeline.state.transport.loop_enabled,
                timeline.state.transport.loop_start_beats,
                timeline.state.transport.loop_end_beats,
                ts_points,
            )
        };

        // Click level and timbre live in Settings, not the timeline, and a fresh
        // stream has neither — push them on the same edge as the enable.
        let (click_volume, click_sound) = {
            let metronome = &self.settings.read(cx).current.recording.metronome;
            (metronome.volume, metronome.sound_type.clone())
        };

        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            transport_freeze_debug::log("abort: engine handle missing");
            return;
        };

        transport_freeze_debug::log("before engine transport sync");
        if let Err(error) = engine.set_tempo_map(bpm as f64, tempo_points) {
            if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                eprintln!("[audio] set tempo map failed: {error}");
            }
        }
        if let Err(error) = engine.set_time_signature_map(ts_points) {
            if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                eprintln!("[audio] set time signature map failed: {error}");
            }
        }
        if let Err(error) = engine.set_metronome_enabled(metronome_enabled) {
            if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                eprintln!("[audio] set metronome failed: {error}");
            }
        }
        if let Err(error) = engine.set_metronome_voice(click_volume, &click_sound) {
            if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                eprintln!("[audio] set metronome voice failed: {error}");
            }
        }
        let (loop_start_seconds, loop_end_seconds, playhead_seconds) = {
            let state = &self.timeline.read(cx).state;
            let base = state.bpm as f64;
            (
                state
                    .tempo_map
                    .seconds_at_beat(loop_start_beats as f64, base),
                state.tempo_map.seconds_at_beat(loop_end_beats as f64, base),
                state
                    .tempo_map
                    .seconds_at_beat(playhead_beats.max(0.0) as f64, base),
            )
        };
        if let Err(error) = engine.set_loop(loop_enabled, loop_start_seconds, loop_end_seconds) {
            if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                eprintln!("[audio] set loop failed: {error}");
            }
        }
        transport_freeze_debug::log("after engine transport sync");

        transport_freeze_debug::log("before engine.seek");
        if let Err(error) = engine.seek(playhead_seconds) {
            self.audio_bridge.last_error = Some(error.to_string());
            eprintln!("[audio] seek before play failed: {error}");
            transport_freeze_debug::log("abort: seek failed");
            return;
        }
        transport_freeze_debug::log("after engine.seek");

        if std::env::var_os("FUTUREBOARD_PLAYBACK_DEBUG").is_some() {
            let debug = engine.debug_snapshot();
            eprintln!(
                "[playback] starting: sr={} loaded_clips={} ready_clips={} seek_seconds={:.3} bpm={:.1}",
                self.current_audio_sample_rate(),
                debug.loaded_clips,
                debug.ready_clips,
                playhead_seconds,
                bpm
            );
            if debug.loaded_clips == 0 {
                eprintln!(
                    "[playback] WARNING: no clips in realtime runtime — verify that imported clips \
                     have a non-empty media path"
                );
            } else if debug.ready_clips == 0 {
                eprintln!(
                    "[playback] WARNING: clips loaded but none decoded — check earlier \
                     '[SphereAudio] clip ... decode FAILED' lines"
                );
            }
        }

        transport_freeze_debug::log("before engine.play");
        if let Err(error) = engine.play() {
            self.audio_bridge.last_error = Some(error.to_string());
            eprintln!("[audio] play failed: {error}");
            transport_freeze_debug::log("abort: engine.play failed");
            return;
        }
        transport_freeze_debug::log("after engine.play");

        let stats = engine.stats();
        self.audio_bridge.stats = Some(stats);
        self.start_hardware_midi_playback(playhead_beats, cx);
        transport_freeze_debug::log("before timeline.update playing=true");
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.state.transport.playing = true;
            cx.notify();
        });
        transport_freeze_debug::log("after timeline.update");
        transport_freeze_debug::log("returning from start_native_playback");

        if let Some(watchdog) = PlayWatchdog::start() {
            let owner = cx.entity().clone();
            cx.spawn(async move |_this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let _ = owner.update(cx, |this, _cx| {
                    let playing = this
                        .audio_bridge
                        .stats
                        .as_ref()
                        .map(|stats| stats.transport_playing)
                        .unwrap_or(false);
                    watchdog.check(playing);
                });
            })
            .detach();
        }
    }

    pub(super) fn sync_transport_controls(&mut self, cx: &mut Context<Self>) {
        self.sync_metronome_controls(cx);
        self.sync_loop_controls(cx);
    }

    pub(super) fn sync_metronome_controls(&mut self, cx: &mut Context<Self>) {
        if self.audio_bridge.engine.is_none() {
            return;
        }
        // The tempo goes as the real map, never as a bare BPM. This used to call
        // `set_bpm`, which is `set_tempo_map(bpm, [])` — it wiped every tempo
        // point and made the audio thread rebuild and re-sort every MIDI event
        // list, on every settings change, for a metronome sync.
        self.sync_tempo_map_to_engine(cx);

        let enabled = self.timeline.read(cx).state.transport.metronome_enabled;
        let (click_volume, click_sound) = {
            let metronome = &self.settings.read(cx).current.recording.metronome;
            (metronome.volume, metronome.sound_type.clone())
        };
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            if let Err(error) = engine.set_metronome_enabled(enabled) {
                if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                    eprintln!("[audio] set metronome failed: {error}");
                }
            }
            if let Err(error) = engine.set_metronome_voice(click_volume, &click_sound) {
                if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                    eprintln!("[audio] set metronome voice failed: {error}");
                }
            }
        }
        self.sync_time_signature_map_to_engine(cx);
    }

    pub(super) fn sync_loop_controls(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return;
        };
        let (enabled, start_seconds, end_seconds) = {
            let timeline = self.timeline.read(cx);
            let transport = &timeline.state.transport;
            let base = timeline.state.bpm.max(1.0) as f64;
            (
                transport.loop_enabled,
                timeline
                    .state
                    .tempo_map
                    .seconds_at_beat(transport.loop_start_beats as f64, base),
                timeline
                    .state
                    .tempo_map
                    .seconds_at_beat(transport.loop_end_beats as f64, base),
            )
        };
        if let Err(error) = engine.set_loop(enabled, start_seconds, end_seconds) {
            if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                eprintln!("[audio] set loop failed: {error}");
            }
        }
    }

    /// Apply a BPM drag sample. The drag is screen-edge independent: on
    /// Windows/macOS/Linux (X11 or Wayland+XWayland) the OS cursor is warped
    /// back to a fixed anchor every move, so vertical dragging accumulates
    /// unbounded relative motion (true DAW-style infinite scrubbing). Pure
    /// Wayland without XWayland falls back to window-relative deltas. Tempo
    /// safety: when automation exists the drag edits the active tempo marker
    /// at the playhead instead of the fixed project BPM.
    pub(super) fn apply_bpm_drag_sample(
        &mut self,
        sample: components::BpmDragSample,
        cx: &mut Context<Self>,
    ) {
        let new_drag = self.bpm_drag.active_id != Some(sample.drag_id);
        if new_drag {
            self.bpm_drag.active_id = Some(sample.drag_id);
            self.bpm_drag.prev_y = sample.cur_y;
            self.bpm_drag.accum = 0.0;
            self.engine_sync.bpm_committed_at = None;
            // Capture the cursor anchor + window scale for warp-based dragging.
            self.bpm_drag.anchor = cursor_pos_phys();
            self.bpm_drag.scale = cx
                .windows()
                .first()
                .and_then(|w| w.update(cx, |_, window, _| window.scale_factor()).ok())
                .unwrap_or(1.0)
                .max(0.1);
            // Decide what this drag edits, and capture the start value there.
            let (target_point_id, start_value) = {
                let state = &self.timeline.read(cx).state;
                if state.tempo_has_automation() {
                    let beat = state.transport.playhead_beats as f64;
                    match state.tempo_map.point_id_at_or_before_beat(beat) {
                        Some(id) => {
                            let bpm = state
                                .tempo_map
                                .points
                                .iter()
                                .find(|p| p.id == id)
                                .map(|p| p.bpm as f32)
                                .unwrap_or(state.bpm);
                            (Some(id.to_string()), bpm)
                        }
                        // Before the first marker → edit the fixed base tempo.
                        None => (None, state.bpm),
                    }
                } else {
                    (None, state.bpm)
                }
            };
            self.bpm_drag.target_point_id = target_point_id;
            self.bpm_drag.start_value = start_value;
            self.bpm_drag.gesture_origin = Some(self.capture_tempo_state(cx));
            if components::bpm_debug_enabled() {
                eprintln!(
                    "[transport-bpm] drag_start id={} start_value={:.2} target_point_id={:?} scale={:.2} anchor={:?}",
                    sample.drag_id,
                    start_value,
                    self.bpm_drag.target_point_id,
                    self.bpm_drag.scale,
                    self.bpm_drag.anchor
                );
            }
            return;
        }

        // Relative vertical movement in logical px. Prefer the warped OS cursor
        // delta (edge-independent); fall back to the window-relative position.
        let dy_logical = match self.warp_drag_delta_y() {
            Some(dy) => dy,
            None => {
                let raw_delta = sample.cur_y - self.bpm_drag.prev_y;
                if raw_delta.abs() < components::BPM_DRAG_DEADZONE_PX {
                    return;
                }
                self.bpm_drag.prev_y = sample.cur_y;
                raw_delta
            }
        };
        if dy_logical == 0.0 {
            return;
        }

        let coarse = sample.control || sample.platform || sample.alt;
        let sensitivity = components::bpm_drag_sensitivity(sample.shift, coarse);
        // Up = positive BPM change. Screen Y grows downward, so negate.
        self.bpm_drag.accum -= dy_logical * sensitivity;

        let raw = self.bpm_drag.start_value + self.bpm_drag.accum;
        let clamped = raw.clamp(components::BPM_MIN, components::BPM_MAX);
        let snapped = if sample.shift {
            (clamped * 10.0).round() / 10.0
        } else if coarse {
            (clamped / 5.0).round() * 5.0
        } else {
            clamped.round()
        };

        let now = Instant::now();
        let engine_due = match self.engine_sync.bpm_committed_at {
            Some(t) => now.duration_since(t) >= Duration::from_millis(33),
            None => true,
        };
        let target_point_id = self.bpm_drag.target_point_id.clone();
        self.apply_bpm_value(snapped, target_point_id.as_deref(), engine_due, cx);
        if engine_due {
            self.engine_sync.bpm_committed_at = Some(now);
        }
    }

    /// Edge-independent vertical drag delta (logical px) using OS cursor warp.
    /// Reads the absolute cursor, computes the delta from the anchor, then
    /// re-pins the cursor to the anchor so it can never reach the screen edge.
    /// Returns `None` on platforms without cursor warp so callers fall back to
    /// window-relative deltas.
    fn warp_drag_delta_y(&mut self) -> Option<f32> {
        let anchor = self.bpm_drag.anchor?;
        let cur = cursor_pos_phys()?;
        let dy_phys = cur.1 - anchor.1;
        if dy_phys == 0 {
            // Echo from our own warp, or no motion.
            return Some(0.0);
        }
        set_cursor_pos_phys(anchor.0, anchor.1);
        Some(dy_phys as f32 / self.bpm_drag.scale)
    }

    /// Cancel the active BPM drag, restoring the value captured at drag start.
    /// Wired to Escape. Records nothing: the tempo is back where it began.
    pub(super) fn cancel_bpm_drag(&mut self, cx: &mut Context<Self>) -> bool {
        if self.bpm_drag.active_id.is_none() {
            return false;
        }
        let start = self.bpm_drag.start_value;
        let target_point_id = self.bpm_drag.target_point_id.clone();
        self.apply_bpm_value(start, target_point_id.as_deref(), true, cx);
        self.end_bpm_drag();
        true
    }

    /// Pointer release after a BPM scrub: close the gesture as one undo entry.
    ///
    /// Idempotent — a release with no scrub in flight does nothing, which is
    /// what the off-element mouse-up handler relies on.
    pub(super) fn commit_bpm_drag(&mut self, cx: &mut Context<Self>) {
        let Some(prev) = self.bpm_drag.gesture_origin.take() else {
            return;
        };
        self.end_bpm_drag();
        self.record_tempo_edit("Set Tempo", prev, cx);
    }

    /// Clears transient BPM-drag bookkeeping. The next `on_drag` gets a fresh
    /// `drag_id`, so this is only needed for explicit cancel/commit.
    pub(super) fn end_bpm_drag(&mut self) {
        self.bpm_drag.active_id = None;
        self.bpm_drag.anchor = None;
        self.bpm_drag.accum = 0.0;
        self.bpm_drag.gesture_origin = None;
    }

    /// Tempo snapshot of the current project state. Pair with
    /// [`Self::record_tempo_edit`] around any tempo mutation.
    pub(super) fn capture_tempo_state(&self, cx: &App) -> TempoStateSnapshot {
        TempoStateSnapshot::capture(&self.timeline.read(cx).state)
    }

    /// Fold a repeated tempo gesture's later steps into the entry it already
    /// pushed. Returns `false` when there is nothing to extend.
    pub(super) fn amend_tempo_edit(&mut self, label: &'static str, cx: &mut Context<Self>) -> bool {
        self.timeline.update(cx, |timeline, cx| {
            timeline.amend_or_record_tempo_edit(label, cx)
        })
    }

    /// Record an already-applied tempo change as one undo entry.
    pub(super) fn record_tempo_edit(
        &mut self,
        label: &'static str,
        prev: TempoStateSnapshot,
        cx: &mut Context<Self>,
    ) -> bool {
        let changed = self.timeline.update(cx, |timeline, cx| {
            timeline.record_tempo_edit(label, prev, cx)
        });
        if changed {
            cx.notify();
        }
        changed
    }

    /// Apply a BPM value to the correct target: a tempo marker (mapped mode) or
    /// the fixed project BPM. Centralizes the tempo-safety rule so dragging,
    /// inline edit, and menu commands all behave consistently.
    pub(super) fn apply_bpm_value(
        &mut self,
        bpm: f32,
        target_point_id: Option<&str>,
        commit_to_engine: bool,
        cx: &mut Context<Self>,
    ) {
        let bpm = bpm.clamp(components::BPM_MIN, components::BPM_MAX);
        match target_point_id {
            None => self.set_native_bpm_inner(bpm, commit_to_engine, cx),
            Some(point_id) => {
                let changed = self.timeline.update(cx, |timeline, cx| {
                    let updated = timeline
                        .state
                        .tempo_map
                        .update_point_bpm_by_id(point_id, bpm as f64);
                    if updated {
                        cx.notify();
                    }
                    updated
                });
                if changed {
                    self.mark_dirty();
                    if commit_to_engine {
                        self.sync_tempo_map_to_engine(cx);
                    }
                    cx.notify();
                }
            }
        }
    }

    /// Apply a new BPM from the transport BPM drag. Updates the timeline
    /// state and sends a lightweight `set_bpm` to the engine — never reloads
    /// the project. Skips notify when the value would round to the same
    /// stored value to avoid notify-spam during mouse motion.
    pub(super) fn set_native_bpm(&mut self, bpm: f32, cx: &mut Context<Self>) {
        self.set_native_bpm_inner(bpm, true, cx);
    }

    pub(super) fn set_native_bpm_inner(
        &mut self,
        bpm: f32,
        commit_to_engine: bool,
        cx: &mut Context<Self>,
    ) {
        let bpm = bpm.clamp(components::BPM_MIN, components::BPM_MAX);
        let changed = self.timeline.update(cx, |timeline, cx| {
            if (timeline.state.bpm - bpm).abs() > 0.005 {
                // Captured under the old tempo, reapplied under the new one.
                let linear_anchors = timeline.state.capture_linear_clip_anchors();
                timeline.state.bpm = bpm;
                // An audio clip's bar count is a function of the tempo: a
                // tempo-synced clip keeps its bars and moves in seconds, every
                // other mode keeps its seconds and moves in bars. Re-deriving
                // here is what keeps the drawn clip and the clip you can grab
                // the same object after a tempo change.
                timeline.state.reconcile_audio_clip_lengths();
                // Linear tracks hold wall-clock time: their clips move in beats
                // so they stay where they were on the clock.
                timeline.state.reapply_linear_clip_anchors(&linear_anchors);
                cx.notify();
                true
            } else {
                false
            }
        });
        if !changed {
            return;
        }
        if commit_to_engine {
            if let Some(engine) = self.audio_bridge.engine.as_ref() {
                if let Err(error) = engine.set_bpm(bpm as f64) {
                    if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                        eprintln!("[audio] set BPM failed: {error}");
                    }
                }
            }
            if std::env::var_os("FUTUREBOARD_TRANSPORT_DEBUG").is_some() {
                eprintln!("[transport-bpm] commit bpm={:.2}", bpm);
            }
        }
        cx.notify();
    }

    /// Open the compact tempo menu (anchored at screen `x`,`y`) from the
    /// transport BPM display. Reuses the shared context-menu overlay.
    pub(super) fn open_tempo_menu(
        &mut self,
        window: &Window,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.try_open_context_menu(
            ContextMenuRequest::from_window(
                window,
                x,
                y,
                ContextMenuTarget::Extended(ContextTarget::Tempo),
            ),
            cx,
        );
    }

    /// Add a tempo marker at the current playhead using the effective BPM there.
    /// This is the primary way to introduce tempo automation from the transport.
    pub(super) fn add_tempo_marker_at_playhead(&mut self, cx: &mut Context<Self>) {
        self.edit_tempo_state(
            "Add Tempo Marker",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats as f64;
                let bpm = timeline.state.effective_bpm_at_beat(beat);
                timeline.state.tempo_map.add_or_update_point(
                    beat,
                    bpm,
                    crate::components::timeline::timeline_state::TempoCurve::Hold,
                );
            },
            cx,
        );
    }

    /// Run one Tempo-lane mutation and record it as a single undo entry.
    ///
    /// The closure mutates `timeline.state` directly; the snapshot taken around
    /// it turns the whole command into one reversible step. Returns `false` when
    /// the command changed nothing, so a no-op never pushes a dead history entry
    /// or re-syncs the engine.
    ///
    /// Recording routes through `EditImpact::TempoMap`, which is what marks the
    /// project dirty and pushes the new map to the engine — that is why there is
    /// no explicit `mark_dirty` / `sync_tempo_map_to_engine` here, and why undo
    /// now reaches the engine the same way the original edit did.
    pub(super) fn edit_tempo_state(
        &mut self,
        label: &'static str,
        edit: impl FnOnce(&mut Timeline),
        cx: &mut Context<Self>,
    ) -> bool {
        let changed = self.timeline.update(cx, |timeline, cx| {
            let prev = timeline.capture_tempo_state();
            // Tempo-map edits move the clock under every clip. Linear tracks
            // hold their wall-clock position across that, so their anchors are
            // read before the edit and reapplied under the new map.
            let linear_anchors = timeline.state.capture_linear_clip_anchors();
            edit(timeline);
            // An audio clip's bar count is a function of the audio it plays and
            // the tempo map over it, so a marker edit re-derives it — the same
            // pass undo runs. Without it the clip keeps the bar count it had
            // under the old map and the picture parts company with the audio.
            timeline.state.reconcile_audio_clip_lengths();
            timeline.state.reapply_linear_clip_anchors(&linear_anchors);
            let changed = timeline.record_tempo_edit(label, prev, cx);
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            cx.notify();
        }
        changed
    }

    /// Run one arrangement-marker mutation and record it as a single undo entry.
    fn edit_markers(
        &mut self,
        label: &'static str,
        edit: impl FnOnce(&mut Timeline),
        cx: &mut Context<Self>,
    ) -> bool {
        let changed = self.timeline.update(cx, |timeline, cx| {
            let prev = timeline.state.markers.clone();
            edit(timeline);
            let changed = timeline.record_marker_edit(label, prev, cx);
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
        changed
    }

    /// Run one arrangement-region mutation and record it as a single undo entry.
    fn edit_regions(
        &mut self,
        label: &'static str,
        edit: impl FnOnce(&mut Timeline),
        cx: &mut Context<Self>,
    ) -> bool {
        let changed = self.timeline.update(cx, |timeline, cx| {
            let prev = timeline.state.regions.clone();
            edit(timeline);
            let changed = timeline.record_region_edit(label, prev, cx);
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
        changed
    }

    pub(super) fn add_marker_at_beat_command(&mut self, beat: f64, cx: &mut Context<Self>) {
        self.edit_markers(
            "Add Marker",
            |timeline| {
                timeline.state.add_marker_at_beat(beat);
            },
            cx,
        );
    }

    pub(super) fn add_region_at_beat_command(&mut self, beat: f64, cx: &mut Context<Self>) {
        self.edit_regions(
            "Add Region",
            |timeline| {
                timeline.state.add_region_at_beat(beat);
            },
            cx,
        );
    }

    pub(super) fn add_marker_at_playhead_command(&mut self, cx: &mut Context<Self>) {
        self.edit_markers(
            "Add Marker",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats.max(0.0) as f64;
                let id = timeline.state.add_marker_at_beat(beat);
                timeline.state.select_marker(&id);
            },
            cx,
        );
    }

    pub(super) fn add_region_at_playhead_command(&mut self, cx: &mut Context<Self>) {
        self.edit_regions(
            "Add Region",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats.max(0.0) as f64;
                let id = timeline.state.add_region_at_beat(beat);
                timeline.state.select_region(&id);
            },
            cx,
        );
    }

    /// Right-click straight onto a conductor-lane item: delete it.
    ///
    /// Returns `true` when something was deleted, which is what tells the
    /// caller to skip the context menu. A press on empty lane space names no
    /// id, so it falls through and the menu opens as before — that menu is
    /// where "add a marker here" lives and it must stay reachable.
    ///
    /// Routed through the same commands the menu's own Delete items use, so the
    /// undo entry, the tempo/meter map rebuild and the engine resync are
    /// identical however the delete was asked for.
    pub(super) fn delete_context_target(
        &mut self,
        target: &ContextTarget,
        cx: &mut Context<Self>,
    ) -> bool {
        match target {
            ContextTarget::MarkerTrack {
                marker_id: Some(id),
                ..
            } => {
                self.delete_marker_command(&id.clone(), cx);
                true
            }
            ContextTarget::RegionTrack {
                region_id: Some(id),
                ..
            } => {
                self.delete_region_command(&id.clone(), cx);
                true
            }
            ContextTarget::TempoTrack {
                point_id: Some(id), ..
            } => {
                self.delete_tempo_point(&id.clone(), cx);
                true
            }
            ContextTarget::TimeSignatureTrack {
                point_id: Some(id), ..
            } => {
                self.delete_time_signature_point(&id.clone(), cx);
                true
            }
            _ => false,
        }
    }

    pub(super) fn delete_marker_command(&mut self, id: &str, cx: &mut Context<Self>) {
        self.edit_markers(
            "Delete Marker",
            |timeline| {
                timeline.state.delete_marker(id);
            },
            cx,
        );
    }

    pub(super) fn delete_region_command(&mut self, id: &str, cx: &mut Context<Self>) {
        self.edit_regions(
            "Delete Region",
            |timeline| {
                timeline.state.delete_region(id);
            },
            cx,
        );
    }

    pub(super) fn move_marker_to_playhead_command(&mut self, id: &str, cx: &mut Context<Self>) {
        self.edit_markers(
            "Move Marker",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats.max(0.0) as f64;
                timeline.state.move_marker(id, beat);
            },
            cx,
        );
    }

    pub(super) fn move_region_to_playhead_command(&mut self, id: &str, cx: &mut Context<Self>) {
        self.edit_regions(
            "Move Region",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats.max(0.0) as f64;
                timeline.state.move_region(id, beat);
            },
            cx,
        );
    }

    pub(super) fn clear_markers_command(&mut self, cx: &mut Context<Self>) {
        self.edit_markers(
            "Delete All Markers",
            |timeline| {
                timeline.state.markers.clear();
                timeline.state.clear_marker_selection();
            },
            cx,
        );
    }

    pub(super) fn clear_regions_command(&mut self, cx: &mut Context<Self>) {
        self.edit_regions(
            "Delete All Regions",
            |timeline| {
                timeline.state.regions.clear();
                timeline.state.clear_region_selection();
            },
            cx,
        );
    }

    /// Move the playhead to a marker. Seeking is transport state, not an edit,
    /// so this deliberately records no history entry.
    pub(super) fn seek_to_marker_command(&mut self, id: &str, cx: &mut Context<Self>) {
        let beat = self
            .timeline
            .read(cx)
            .state
            .marker(id)
            .map(|marker| marker.beat as f32);
        if let Some(beat) = beat {
            self.timeline.update(cx, |timeline, cx| {
                timeline.state.select_marker(id);
                timeline.seek_to_exact_beat(beat, SeekReason::TimelineClick, cx);
            });
            cx.notify();
        }
    }

    /// Set the transport loop to a region's span. Loop range is control state,
    /// so this marks view-only dirty rather than entering edit history —
    /// matching the ruler's loop drag.
    pub(super) fn set_loop_to_region_command(&mut self, id: &str, cx: &mut Context<Self>) {
        let range = self
            .timeline
            .read(cx)
            .state
            .region(id)
            .map(|region| region.normalized_range());
        let Some((start, end)) = range else {
            return;
        };
        self.timeline.update(cx, |timeline, cx| {
            let transport = &mut timeline.state.transport;
            transport.loop_start_beats = start as f32;
            transport.loop_end_beats = (end as f32).max(start as f32 + 1.0e-3);
            transport.loop_enabled = true;
            cx.notify();
        });
        self.mark_dirty_view_only();
        self.sync_loop_controls(cx);
        cx.notify();
    }

    /// Time Signature counterpart of [`Self::edit_tempo_state`].
    pub(super) fn edit_time_signature_state(
        &mut self,
        label: &'static str,
        edit: impl FnOnce(&mut Timeline),
        cx: &mut Context<Self>,
    ) -> bool {
        let changed = self.timeline.update(cx, |timeline, cx| {
            let prev = timeline.capture_time_signature_state();
            edit(timeline);
            let changed = timeline.record_time_signature_edit(label, prev, cx);
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            cx.notify();
        }
        changed
    }

    /// Remove all tempo automation, keeping the playhead BPM as a single fixed
    /// marker at beat 0.
    pub(super) fn clear_tempo_automation(&mut self, cx: &mut Context<Self>) {
        self.edit_tempo_state(
            "Clear Tempo Automation",
            |timeline| {
                let bpm = timeline.state.effective_bpm_at_playhead();
                if timeline.state.tempo_map.points.len() == 1
                    && timeline
                        .state
                        .tempo_map
                        .points
                        .first()
                        .is_some_and(|p| p.beat <= 1e-6 && (p.bpm - bpm).abs() < 1e-6)
                {
                    return;
                }
                timeline.state.bpm = bpm as f32;
                timeline.state.tempo_map.reset_to_single_point(
                    0.0,
                    bpm,
                    crate::components::timeline::timeline_state::TempoCurve::Hold,
                );
                timeline.state.selected_tempo_point_id = None;
            },
            cx,
        );
    }

    pub(super) fn show_tempo_track(&mut self, cx: &mut Context<Self>) {
        self.timeline.update(cx, |timeline, cx| {
            timeline.state.show_tempo_track_lane();
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn hide_tempo_track(&mut self, cx: &mut Context<Self>) {
        self.timeline.update(cx, |timeline, cx| {
            timeline.state.hide_tempo_track_lane();
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn tempo_track_context_position(&self) -> Option<(f64, f64)> {
        match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::TempoTrack { beat, bpm, .. }) => {
                    Some((*beat, *bpm))
                }
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn tempo_track_context_point_id(&self) -> Option<String> {
        match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::TempoTrack { point_id, .. }) => {
                    point_id.clone()
                }
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn add_tempo_point_at_lane(&mut self, beat: f64, bpm: f64, cx: &mut Context<Self>) {
        self.edit_tempo_state(
            "Add Tempo Marker",
            |timeline| {
                if let Some(id) = timeline.state.add_tempo_point(beat, bpm) {
                    timeline.state.select_tempo_point(&id);
                }
            },
            cx,
        );
    }

    pub(super) fn set_fixed_tempo_from_lane(
        &mut self,
        beat: f64,
        bpm: f64,
        cx: &mut Context<Self>,
    ) {
        self.edit_tempo_state(
            "Set Fixed Tempo",
            |timeline| {
                timeline.state.set_fixed_tempo_from_beat(beat, bpm);
                timeline.state.bpm = bpm as f32;
            },
            cx,
        );
    }

    pub(super) fn delete_tempo_point(&mut self, id: &str, cx: &mut Context<Self>) {
        self.edit_tempo_state(
            "Delete Tempo Marker",
            |timeline| {
                timeline.state.delete_tempo_point(id);
            },
            cx,
        );
    }

    pub(super) fn set_tempo_point_curve(
        &mut self,
        id: &str,
        curve: crate::components::timeline::timeline_state::TempoCurve,
        cx: &mut Context<Self>,
    ) {
        self.edit_tempo_state(
            "Set Tempo Curve",
            |timeline| {
                timeline.state.set_tempo_point_curve(id, curve);
            },
            cx,
        );
    }

    /// Convert fixed-tempo mode into a tempo map by seeding an initial marker at
    /// beat 0 using the current project BPM. No-op if automation already exists.
    pub(super) fn create_tempo_automation(&mut self, cx: &mut Context<Self>) {
        self.edit_tempo_state(
            "Create Tempo Automation",
            |timeline| {
                if timeline.state.tempo_map.has_automation() {
                    return;
                }
                let bpm = timeline.state.bpm as f64;
                timeline.state.tempo_map.add_or_update_point(
                    0.0,
                    bpm,
                    crate::components::timeline::timeline_state::TempoCurve::Hold,
                );
            },
            cx,
        );
    }

    /// Insert a tempo marker at an explicit beat using the effective BPM there
    /// (used by the timeline ruler context menu). `create_first` seeds tempo
    /// automation if none exists yet.
    pub(super) fn add_tempo_point_at_beat(
        &mut self,
        beat: f64,
        create_first: bool,
        cx: &mut Context<Self>,
    ) {
        let beat = beat.max(0.0);
        // One entry covers the seeded beat-0 anchor and the new marker together,
        // so a single undo returns the project to fixed tempo.
        self.edit_tempo_state(
            "Add Tempo Marker",
            |timeline| {
                use crate::components::timeline::timeline_state::TempoCurve;
                if !timeline.state.tempo_map.has_automation() && create_first {
                    // Seed an anchor at beat 0 so the new marker reads as a change
                    // from the project's fixed tempo.
                    let base = timeline.state.bpm as f64;
                    if beat > 1e-6 {
                        timeline
                            .state
                            .tempo_map
                            .add_or_update_point(0.0, base, TempoCurve::Hold);
                    }
                }
                let bpm = timeline.state.effective_bpm_at_beat(beat);
                timeline
                    .state
                    .tempo_map
                    .add_or_update_point(beat, bpm, TempoCurve::Hold);
            },
            cx,
        );
    }

    /// Open the inline numeric BPM editor, seeded with the current effective
    /// BPM at the playhead. Keys are routed by the layout's key handler while
    /// `bpm_editing` is set, so no separate focus grab is needed.
    pub(super) fn begin_bpm_edit(&mut self, cx: &mut Context<Self>) {
        // A drag and an edit are mutually exclusive.
        self.end_bpm_drag();
        let bpm = self.timeline.read(cx).state.effective_bpm_at_playhead();
        // The typed editor and the scrub drag both resolve through the effective
        // BPM at the playhead, so they share one value source. The session owns
        // range/format/commit policy; the draft text stays in `bpm_input.value`.
        let format = crate::components::numeric_edit::NumberFormat::decimal(
            components::BPM_MIN as f64,
            components::BPM_MAX as f64,
            2,
        );
        let session = crate::components::numeric_edit::NumericEditSession::begin(
            bpm,
            format,
            crate::components::numeric_edit::CommitPolicy::OnEnter,
        );
        self.tempo_edit.bpm_input.set_value(session.original_text());
        self.tempo_edit.bpm_input.select_all();
        self.tempo_edit.bpm_session = Some(session);
        self.tempo_edit.bpm_editing = true;
        cx.notify();
    }

    /// Nudge the inline BPM draft by `steps` arrow-key increments (fractional
    /// multipliers allowed for coarse/fine). No-op unless the editor is open.
    /// Returns `true` when the draft changed so the caller can consume the key.
    pub(super) fn nudge_bpm_edit(&mut self, steps: f64, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.tempo_edit.bpm_session else {
            return false;
        };
        let next = session.nudge_draft(&self.tempo_edit.bpm_input.value, steps);
        self.tempo_edit.bpm_input.set_value(next);
        self.tempo_edit.bpm_input.select_all();
        cx.notify();
        true
    }

    /// Commit the inline BPM editor: parse the field and apply to the active
    /// tempo target (marker at playhead when automation exists, else fixed BPM).
    pub(super) fn commit_bpm_edit(&mut self, cx: &mut Context<Self>) {
        if !self.tempo_edit.bpm_editing {
            return;
        }
        // Parse + clamp through the shared numeric session so an invalid or
        // intermediate draft ("", ".", "abc") commits nothing (no model
        // mutation), and a valid draft yields exactly one clamped value.
        let parsed = self
            .tempo_edit
            .bpm_session
            .and_then(|session| session.commit(&self.tempo_edit.bpm_input.value))
            .map(|bpm| bpm as f32);
        self.tempo_edit.bpm_editing = false;
        self.tempo_edit.bpm_session = None;
        if let Some(bpm) = parsed {
            let target_point_id = {
                let state = &self.timeline.read(cx).state;
                if state.tempo_has_automation() {
                    let beat = state.transport.playhead_beats as f64;
                    state
                        .tempo_map
                        .point_id_at_or_before_beat(beat)
                        .map(|id| id.to_string())
                } else {
                    None
                }
            };
            let prev = self.capture_tempo_state(cx);
            self.apply_bpm_value(bpm, target_point_id.as_deref(), true, cx);
            self.record_tempo_edit("Set Tempo", prev, cx);
        }
        cx.notify();
    }

    /// Cancel the inline BPM editor without applying.
    pub(super) fn cancel_bpm_edit(&mut self, cx: &mut Context<Self>) {
        if !self.tempo_edit.bpm_editing {
            return;
        }
        self.tempo_edit.bpm_editing = false;
        self.tempo_edit.bpm_session = None;
        cx.notify();
    }

    /// Push the project TempoMap to the audio engine so playback, metronome,
    /// and MIDI scheduling use the authoritative hold-mode segments.
    pub(super) fn sync_tempo_map_to_engine(&mut self, cx: &mut Context<Self>) {
        let (default_bpm, points) = {
            let state = &self.timeline.read(cx).state;
            (
                state.bpm as f64,
                state
                    .tempo_map
                    .points
                    .iter()
                    .map(|p| DirectAudio::types::EngineTempoPointSnapshot {
                        beat: p.beat,
                        bpm: p.bpm,
                        curve: p.curve.to_tag(),
                    })
                    .collect::<Vec<_>>(),
            )
        };
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            if let Err(error) = engine.set_tempo_map(default_bpm, points) {
                if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                    eprintln!("[audio] sync tempo map failed: {error}");
                }
            }
        }
    }

    pub(super) fn sync_time_signature_map_to_engine(&mut self, cx: &mut Context<Self>) {
        let points = {
            let state = &self.timeline.read(cx).state;
            state
                .time_signature_map
                .points
                .iter()
                .map(
                    |p| DirectAudio::time_signature_map::RuntimeTimeSignaturePointSnapshot {
                        beat: p.beat,
                        numerator: p.numerator,
                        denominator: p.denominator,
                        grouping: p.effective_grouping(),
                    },
                )
                .collect::<Vec<_>>()
        };
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            if let Err(error) = engine.set_time_signature_map(points) {
                if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                    eprintln!("[audio] sync time signature map failed: {error}");
                }
            }
        }
    }

    pub(super) fn open_time_signature_menu(
        &mut self,
        window: &Window,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.try_open_context_menu(
            ContextMenuRequest::from_window(
                window,
                x,
                y,
                ContextMenuTarget::Extended(ContextTarget::TimeSignature),
            ),
            cx,
        );
    }

    pub(super) fn show_time_signature_track(&mut self, cx: &mut Context<Self>) {
        self.timeline.update(cx, |timeline, cx| {
            timeline.state.show_time_signature_track_lane();
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn hide_time_signature_track(&mut self, cx: &mut Context<Self>) {
        self.timeline.update(cx, |timeline, cx| {
            timeline.state.hide_time_signature_track_lane();
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn add_time_signature_marker_at_playhead(&mut self, cx: &mut Context<Self>) {
        self.edit_time_signature_state(
            "Add Time Signature",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats as f64;
                let pt = timeline
                    .state
                    .time_signature_map
                    .time_signature_at_beat(beat);
                timeline
                    .state
                    .add_time_signature_point(beat, pt.numerator, pt.denominator);
            },
            cx,
        );
    }

    pub(super) fn add_time_signature_point_at_beat(&mut self, beat: f64, cx: &mut Context<Self>) {
        let beat = beat.max(0.0);
        self.edit_time_signature_state(
            "Add Time Signature",
            |timeline| {
                let pt = timeline
                    .state
                    .time_signature_map
                    .time_signature_at_beat(beat);
                timeline
                    .state
                    .add_time_signature_point(beat, pt.numerator, pt.denominator);
            },
            cx,
        );
    }

    pub(super) fn clear_time_signature_markers(&mut self, cx: &mut Context<Self>) {
        self.edit_time_signature_state(
            "Clear Time Signatures",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats as f64;
                timeline.state.clear_time_signature_markers(beat);
            },
            cx,
        );
    }

    pub(super) fn delete_time_signature_point(&mut self, id: &str, cx: &mut Context<Self>) {
        self.edit_time_signature_state(
            "Delete Time Signature",
            |timeline| {
                timeline.state.delete_time_signature_point(id);
            },
            cx,
        );
    }

    pub(super) fn move_time_signature_point_to_playhead(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) {
        self.edit_time_signature_state(
            "Move Time Signature",
            |timeline| {
                let beat = timeline.state.transport.playhead_beats as f64;
                timeline.state.move_time_signature_point(id, beat);
            },
            cx,
        );
    }

    pub(super) fn ts_track_context_position(&self) -> Option<f64> {
        match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(
                    ContextTarget::TimeSignatureTrack { beat, .. }
                    | ContextTarget::TimeSignaturePoint { beat, .. },
                ) => Some(*beat),
                ContextMenuTarget::Extended(ContextTarget::TimelineRuler { beat }) => Some(*beat),
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn ts_track_context_point_id(&self) -> Option<String> {
        match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::TimeSignatureTrack {
                    point_id, ..
                }) => point_id.clone(),
                ContextMenuTarget::Extended(ContextTarget::TimeSignaturePoint {
                    point_id, ..
                }) => Some(point_id.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn begin_ts_edit(&mut self, point_id: Option<String>, cx: &mut Context<Self>) {
        let (num, den) = {
            let state = &self.timeline.read(cx).state;
            if let Some(id) = point_id
                .as_deref()
                .or(state.selected_time_signature_point_id.as_deref())
            {
                if let Some(pt) = state.time_signature_map.points.iter().find(|p| p.id == id) {
                    (pt.numerator, pt.denominator)
                } else {
                    let pt = state.time_signature_at_playhead();
                    (pt.numerator, pt.denominator)
                }
            } else {
                let pt = state.time_signature_at_playhead();
                (pt.numerator, pt.denominator)
            }
        };
        self.tempo_edit.ts_edit_point_id = point_id.or_else(|| {
            self.timeline
                .read(cx)
                .state
                .selected_time_signature_point_id
                .clone()
        });
        self.tempo_edit.ts_num_input.set_value(num.to_string());
        self.tempo_edit.ts_den_input.set_value(den.to_string());
        self.tempo_edit.ts_num_input.select_all();
        self.tempo_edit.ts_editing = true;
        self.tempo_edit.ts_edit_focus_num = true;
        cx.notify();
    }

    pub(super) fn commit_ts_edit(&mut self, cx: &mut Context<Self>) {
        if !self.tempo_edit.ts_editing {
            return;
        }
        let num = self
            .tempo_edit
            .ts_num_input
            .value
            .trim()
            .parse::<u16>()
            .ok();
        let den = self
            .tempo_edit
            .ts_den_input
            .value
            .trim()
            .parse::<u16>()
            .ok();
        self.tempo_edit.ts_editing = false;
        self.tempo_edit.ts_edit_focus_num = true;
        let Some(num) = num.filter(|n| (1..=64).contains(n)) else {
            cx.notify();
            return;
        };
        let den = den
            .map(crate::components::timeline::timeline_state::normalize_time_signature_denominator)
            .filter(|d| {
                crate::components::timeline::timeline_state::TS_ALLOWED_DENOMINATORS.contains(d)
            });
        let Some(den) = den else {
            cx.notify();
            return;
        };

        let point_id = self.tempo_edit.ts_edit_point_id.clone();
        self.edit_time_signature_state(
            "Edit Time Signature",
            |timeline| {
                if let Some(id) = point_id {
                    timeline.state.update_time_signature_point(&id, num, den);
                } else {
                    let beat = timeline.state.transport.playhead_beats as f64;
                    timeline.state.add_time_signature_point(beat, num, den);
                }
            },
            cx,
        );
        self.tempo_edit.ts_edit_point_id = None;
        cx.notify();
    }

    pub(super) fn cancel_ts_edit(&mut self, cx: &mut Context<Self>) {
        if !self.tempo_edit.ts_editing {
            return;
        }
        self.tempo_edit.ts_editing = false;
        self.tempo_edit.ts_edit_point_id = None;
        self.tempo_edit.ts_edit_focus_num = true;
        cx.notify();
    }

    /// The one Stop.
    ///
    /// Spacebar, the transport Stop button, an ARA host request, a project
    /// close, a sample-rate change and a session load all land here, and every
    /// one of them has to produce the same lifecycle. A take that is running
    /// must be finalized — writer flushed, file closed, clip committed, session
    /// destroyed — never left writing into a file nobody will finish, which is
    /// what made a second Record continue the first one's clip.
    ///
    /// Count-in is deliberately excluded: it is cancelled rather than
    /// finalized, and `stop_native_recording` routes that case back through
    /// here, so excluding it is also what keeps the two from recursing.
    pub(super) fn stop_native_playback(&mut self, cx: &mut Context<Self>) {
        if stop_must_finalize_recording(&self.recording.ui_state, self.is_recording_active(cx)) {
            self.stop_native_recording(cx);
            return;
        }
        self.stop_hardware_midi_playback();
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return;
        };
        if let Err(error) = engine.pause() {
            self.audio_bridge.last_error = Some(error.to_string());
            eprintln!("[audio] stop transport failed: {error}");
            return;
        }
        self.audio_bridge.stats = Some(engine.stats());
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.state.transport.playing = false;
            cx.notify();
        });
    }

    pub(super) fn set_playhead_scrub_active(&mut self, active: bool, _cx: &mut Context<Self>) {
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            let _ = engine.set_metronome_suspended(active);
        }
    }

    pub(super) fn seek_native_playhead(&mut self, cx: &mut Context<Self>, beat: f32) {
        self.seek_native_playhead_with_reason(cx, beat, SeekReason::Programmatic);
    }

    pub(super) fn seek_native_playhead_with_reason(
        &mut self,
        cx: &mut Context<Self>,
        beat: f32,
        reason: SeekReason,
    ) {
        let beat = beat.max(0.0);
        let was_playing = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.transport_playing)
            .unwrap_or(false);
        self.stop_hardware_midi_playback();
        // Beat -> seconds through the tempo map, the conversion
        // `start_native_playback` already uses. A static `beat * 60 / bpm` lands
        // somewhere else entirely under tempo automation, and the metronome
        // re-arms its schedule from wherever the seek actually landed.
        let seconds = {
            let state = &self.timeline.read(cx).state;
            state
                .tempo_map
                .seconds_at_beat(beat as f64, state.bpm.max(1.0) as f64)
        };
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            match reason {
                SeekReason::UserDragStart | SeekReason::UserDragging => {
                    let _ = engine.set_metronome_suspended(true);
                }
                SeekReason::UserDragEnd
                | SeekReason::TimelineClick
                | SeekReason::RewindForward
                | SeekReason::Programmatic => {
                    let _ = engine.set_metronome_suspended(false);
                }
            }
            if let Err(error) = engine.seek(seconds) {
                self.audio_bridge.last_error = Some(error.to_string());
                eprintln!("[audio] seek failed: {error}");
            }
        }
        self.engine_sync.playhead_beat = beat;
        self.engine_sync.synced_at = Instant::now();
        let _ = self.timeline.update(cx, move |timeline, cx| {
            timeline.state.transport.playhead_beats = beat;
            // Preview Track Volume automation at the new playhead so a stopped
            // seek updates the fader/inspector to the value under the cursor.
            timeline.state.recompute_effective_volumes(beat, "seek");
            cx.notify();
        });
        if was_playing {
            self.start_hardware_midi_playback(beat, cx);
        }
    }
}

/// Current OS cursor position in physical screen pixels, if the platform
/// supports querying it. Used to drive screen-edge-independent BPM scrubbing.
#[cfg(target_os = "windows")]
fn cursor_pos_phys() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x, p.y))
}

/// Re-pin the OS cursor to `(x, y)` physical pixels so an active scrub never
/// reaches the screen edge.
#[cfg(target_os = "windows")]
fn set_cursor_pos_phys(x: i32, y: i32) {
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

#[cfg(target_os = "linux")]
fn cursor_pos_phys() -> Option<(i32, i32)> {
    x11_cursor::query()
}

#[cfg(target_os = "linux")]
fn set_cursor_pos_phys(x: i32, y: i32) {
    x11_cursor::warp(x, y);
}

#[cfg(target_os = "macos")]
fn cursor_pos_phys() -> Option<(i32, i32)> {
    macos_cursor::query()
}

#[cfg(target_os = "macos")]
fn set_cursor_pos_phys(x: i32, y: i32) {
    macos_cursor::warp(x, y);
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn cursor_pos_phys() -> Option<(i32, i32)> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn set_cursor_pos_phys(_x: i32, _y: i32) {}

/// Cached X11 connection for BPM cursor query/warp. Opened once.
///
/// Works on native X11 and on Wayland-with-XWayland (`DISPLAY` set). Pure
/// Wayland without XWayland leaves the connection unavailable; callers then
/// fall back to window-relative deltas (full Wayland pointer-lock needs gpui
/// compositor integration and is not wired here).
#[cfg(target_os = "linux")]
mod x11_cursor {
    use std::sync::{Mutex, OnceLock};
    use x11_dl::xlib::{self, Display, Window as XWindow};

    struct Conn {
        xlib: xlib::Xlib,
        display: *mut Display,
        root: XWindow,
    }

    // Serialized behind a Mutex; UI-thread BPM scrub is the only consumer.
    unsafe impl Send for Conn {}

    fn conn() -> Option<&'static Mutex<Conn>> {
        static CONN: OnceLock<Option<Mutex<Conn>>> = OnceLock::new();
        CONN.get_or_init(|| {
            let xlib = xlib::Xlib::open().ok()?;
            unsafe {
                let display = (xlib.XOpenDisplay)(std::ptr::null());
                if display.is_null() {
                    return None;
                }
                let root = (xlib.XDefaultRootWindow)(display);
                Some(Mutex::new(Conn {
                    xlib,
                    display,
                    root,
                }))
            }
        })
        .as_ref()
    }

    pub(super) fn query() -> Option<(i32, i32)> {
        let conn = conn()?.lock().ok()?;
        unsafe {
            let mut root_return = 0;
            let mut child_return = 0;
            let mut root_x = 0;
            let mut root_y = 0;
            let mut win_x = 0;
            let mut win_y = 0;
            let mut mask = 0u32;
            let ok = (conn.xlib.XQueryPointer)(
                conn.display,
                conn.root,
                &mut root_return,
                &mut child_return,
                &mut root_x,
                &mut root_y,
                &mut win_x,
                &mut win_y,
                &mut mask,
            );
            if ok == xlib::False {
                None
            } else {
                Some((root_x, root_y))
            }
        }
    }

    pub(super) fn warp(x: i32, y: i32) {
        let Some(lock) = conn() else {
            return;
        };
        let Ok(conn) = lock.lock() else {
            return;
        };
        unsafe {
            (conn.xlib.XWarpPointer)(conn.display, 0, conn.root, 0, 0, 0, 0, x, y);
            (conn.xlib.XFlush)(conn.display);
        }
    }
}

/// CoreGraphics cursor query/warp for edge-independent BPM scrubbing on macOS.
#[cfg(target_os = "macos")]
mod macos_cursor {
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
        fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
        fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *mut std::ffi::c_void);
    }

    pub(super) fn query() -> Option<(i32, i32)> {
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            if event.is_null() {
                return None;
            }
            let point = CGEventGetLocation(event);
            CFRelease(event);
            Some((point.x.round() as i32, point.y.round() as i32))
        }
    }

    pub(super) fn warp(x: i32, y: i32) {
        unsafe {
            // Keep OS mouse events flowing after a warp (otherwise macOS can
            // suppress the next few moves and the scrub feels sticky).
            let _ = CGAssociateMouseAndMouseCursorPosition(true);
            let _ = CGWarpMouseCursorPosition(CGPoint {
                x: f64::from(x),
                y: f64::from(y),
            });
        }
    }
}

#[cfg(test)]
mod transport_repaint_tests {
    use crate::components::timeline::timeline_state::{TimeSignatureMap, TimelineState};

    /// The studio root repaints the whole window, so during playback it must
    /// follow the chrome's bar.beat readout rather than raw playhead motion.
    /// At 120 BPM in 4/4 that is 2 distinct values per second — against a poll
    /// loop running at the display refresh, this is the difference between a
    /// couple of full-window relayouts per second and 60-144 of them.
    #[test]
    fn bar_beat_changes_far_less_often_than_the_playhead() {
        let mut map = TimeSignatureMap::new();
        map.add_or_update_point(0.0, 4, 4);

        let bar_beat = |beat: f64| {
            let bb = map.bar_beat_at_beat(beat);
            (bb.bar, bb.beat_in_bar)
        };

        // One second of playback at 120 BPM = 2 beats, polled at 144 Hz.
        let beats_per_second = 2.0_f64;
        let polls = 144;
        let mut distinct = 0usize;
        let mut previous = (i64::MIN, u16::MAX);
        for poll in 0..polls {
            let beat = beats_per_second * (poll as f64 / polls as f64);
            let current = bar_beat(beat);
            if current != previous {
                distinct += 1;
                previous = current;
            }
        }
        assert_eq!(
            distinct, 2,
            "only the beat boundaries should repaint the shell, got {distinct}"
        );
    }

    /// Sub-beat motion must not trip the comparison, or the optimization is
    /// silently a no-op.
    #[test]
    fn sub_beat_motion_holds_the_same_display_value() {
        let state = TimelineState::default();
        let at = |beat: f64| {
            let bb = state.time_signature_map.bar_beat_at_beat(beat);
            (bb.bar, bb.beat_in_bar)
        };
        assert_eq!(at(4.01), at(4.99), "same beat, no shell repaint");
        assert_ne!(at(4.99), at(5.01), "beat boundary does repaint");
    }
}

#[cfg(test)]
mod snapshot_volume_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{volume, TimelineState};

    fn track_volume_in_snapshot(state: &TimelineState, track_id: &str) -> f32 {
        build_engine_project_snapshot(state, 48_000, None, None)
            .tracks
            .into_iter()
            .find(|track| track.id == track_id)
            .expect("track in snapshot")
            .volume
    }

    /// What the engine is told has to be what the fader shows. A drag holds its
    /// value in the preview map and only writes the track on release, so a sync
    /// landing mid-drag used to publish the value the user had already moved
    /// away from — undoing the live param push and making the track loud again
    /// until they let go.
    #[test]
    fn an_in_flight_fader_publishes_what_the_user_is_holding() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let loud = track_volume_in_snapshot(&state, &track);

        // Mid-drag: the press starts the preview at the current value, the
        // move takes it somewhere else, and the track itself is untouched.
        let start_norm = state.find_track(&track).expect("track").volume;
        let quiet_norm = volume::db_to_norm(-24.0);
        state.begin_track_volume_preview(&track, start_norm);
        assert!(state.set_track_volume_preview(&track, quiet_norm));
        let published = track_volume_in_snapshot(&state, &track);
        assert!(
            published < loud,
            "snapshot published {published} while the fader was held at -24 dB (was {loud})"
        );
    }

    /// And on release it stays there — the commit writes the track and the
    /// preview goes away, so the two agree rather than swapping which one wins.
    #[test]
    fn the_committed_fader_publishes_the_same_value() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let start_norm = state.find_track(&track).expect("track").volume;
        let quiet_norm = volume::db_to_norm(-24.0);
        state.begin_track_volume_preview(&track, start_norm);
        assert!(state.set_track_volume_preview(&track, quiet_norm));
        let held = track_volume_in_snapshot(&state, &track);

        state
            .commit_track_volume_preview(&track)
            .expect("a preview was in flight");
        let committed = track_volume_in_snapshot(&state, &track);
        assert!(
            (held - committed).abs() < 1.0e-6,
            "release changed the published volume from {held} to {committed}"
        );
    }

    /// A track nobody is touching still publishes its own volume.
    #[test]
    fn an_untouched_track_publishes_its_base_volume() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        state.set_track_volume(&track, volume::db_to_norm(-6.0));
        let published = track_volume_in_snapshot(&state, &track);
        let expected =
            super::super::engine_snapshot::volume_norm_to_linear(volume::db_to_norm(-6.0));
        assert!((published - expected).abs() < 1.0e-6);
    }
}

#[cfg(test)]
mod routing_fingerprint_tests {
    use super::*;
    use crate::components::timeline::timeline_state::TimelineState;

    /// The two fingerprints the sync compares: the whole snapshot, and the
    /// routing half that `route_graph_version` is supposed to track.
    fn fingerprints(state: &TimelineState) -> (u64, u64) {
        let snapshot = build_engine_project_snapshot(state, 48_000, None, None);
        let full = graph_fingerprint_of(&serde_json::to_string(&snapshot).unwrap_or_default());
        let routing = graph_fingerprint_of(
            &serde_json::to_string(&(&snapshot.tracks, &snapshot.routing)).unwrap_or_default(),
        );
        (full, routing)
    }

    /// The whole point: writing a note changes the project the engine must load,
    /// but cannot change the graph — so the mixer tree's cache generation must
    /// not move, and the sidebar must not rebuild.
    #[test]
    fn writing_a_note_changes_the_project_but_not_the_routing() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_midi_track();
        let clip = state.create_midi_clip(&track, 0.0, 8.0).expect("midi clip");
        let (full_before, routing_before) = fingerprints(&state);

        state
            .add_midi_note(&clip, 60, 0.0, 1.0, 100)
            .expect("note added");
        let (full_after, routing_after) = fingerprints(&state);

        assert_ne!(
            full_before, full_after,
            "the engine still has to be told about the note"
        );
        assert_eq!(
            routing_before, routing_after,
            "a note is not a routing change"
        );
    }

    /// A real graph change still has to move the routing fingerprint, or the
    /// mixer tree would keep a stale cache forever.
    #[test]
    fn adding_a_track_changes_the_routing() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let _ = state.create_midi_track();
        let (_, routing_before) = fingerprints(&state);

        let _ = state.create_audio_track();
        let (_, routing_after) = fingerprints(&state);

        assert_ne!(routing_before, routing_after);
    }

    /// Mute is carried in the track snapshot, so it is a routing change and the
    /// mixer strip that draws it has to see the new generation.
    #[test]
    fn muting_a_track_changes_the_routing() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let (_, routing_before) = fingerprints(&state);

        assert!(state.set_track_mute(&track, true));
        let (_, routing_after) = fingerprints(&state);

        assert_ne!(routing_before, routing_after);
    }
}

#[cfg(test)]
mod sync_gate_tests {
    use super::*;

    /// The common case: one edit, nothing recent, publish now. This is what
    /// makes the coalescing window safe to apply to MIDI edits at all — a note
    /// written on its own still reaches the scheduler on the spot.
    #[test]
    fn an_isolated_edit_publishes_immediately() {
        assert!(sync_should_build_now(false, true, None));
        assert!(sync_should_build_now(
            false,
            true,
            Some(UI_GESTURE_SYNC_INTERVAL)
        ));
        assert!(sync_should_build_now(
            false,
            true,
            Some(UI_GESTURE_SYNC_INTERVAL * 2)
        ));
    }

    /// A burst rides the open window. The caller keeps the graph dirty, so the
    /// engine poll publishes the burst's last state once it closes.
    #[test]
    fn a_burst_rides_the_open_window() {
        assert!(!sync_should_build_now(false, true, Some(Duration::ZERO)));
        assert!(!sync_should_build_now(
            false,
            true,
            Some(UI_GESTURE_SYNC_INTERVAL - Duration::from_millis(1))
        ));
    }

    /// A quiescent graph must never reach the snapshot build, or the 60 Hz poll
    /// would rebuild the whole project every tick.
    #[test]
    fn a_clean_graph_is_never_rebuilt() {
        assert!(!sync_should_build_now(false, false, None));
        assert!(!sync_should_build_now(
            false,
            false,
            Some(UI_GESTURE_SYNC_INTERVAL * 10)
        ));
    }

    /// Force is for publishing an unchanged graph (device change, retry), so it
    /// has to beat both gates.
    #[test]
    fn force_beats_both_gates() {
        assert!(sync_should_build_now(true, false, Some(Duration::ZERO)));
        assert!(sync_should_build_now(true, true, Some(Duration::ZERO)));
    }
}

#[cfg(test)]
mod stop_lifecycle_tests {
    use super::stop_must_finalize_recording;
    use crate::layout::RecordingUiState;

    /// Test 3 / Test 5: Spacebar and the Stop button both reach the same rule,
    /// so a running take is finalized either way and the writer stops consuming
    /// input at the Stop rather than at whatever the caller remembered to do.
    #[test]
    fn a_running_take_is_finalized_by_every_stop_channel() {
        for state in [
            RecordingUiState::Preparing,
            RecordingUiState::Recording,
            RecordingUiState::Finalizing,
        ] {
            assert!(
                stop_must_finalize_recording(&state, true),
                "{state:?} must finalize"
            );
        }
    }

    /// A count-in has no take yet: it is cancelled, and routing it back into the
    /// finalize path would recurse instead of stopping.
    #[test]
    fn a_count_in_is_cancelled_rather_than_finalized() {
        assert!(!stop_must_finalize_recording(
            &RecordingUiState::CountingIn {
                bars: 2,
                beats_total: 8,
                beats_left: 8,
            },
            true
        ));
    }

    /// Plain playback stop stays plain: nothing to flush, nothing to commit.
    #[test]
    fn stopping_playback_alone_finalizes_nothing() {
        assert!(!stop_must_finalize_recording(
            &RecordingUiState::Idle,
            false
        ));
        assert!(!stop_must_finalize_recording(
            &RecordingUiState::Failed {
                reason: "x".to_string()
            },
            false
        ));
    }
}
