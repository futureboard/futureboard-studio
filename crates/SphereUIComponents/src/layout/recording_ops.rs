use gpui::{App, Context};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::components::timeline::timeline_state::{
    AudioClipStretchState, AudioImportState, ClipState, ClipType, MidiControllerLane,
    MidiNoteState, TrackAudioFormat, TrackState, TrackType, MIN_NOTE_BEATS,
};
use crate::components::timeline::waveform_cache::{self, WaveformPeak};
use sphere_midi_service::MidiInputEvent;

use super::{audio_recording_preview_clip_id, RecordingPreviewUi, RecordingUiState, StudioLayout};
use DirectAudio::types::{JsRecordingTrackConfig, JsStartRecordingConfig};

/// Active recording-session UI state — the take's start position, the UI phase,
/// and the live growing-waveform preview. `StudioLayout` decomposition slice;
/// accessed from the recording + transport ops modules.
pub(crate) struct RecordingSessionState {
    /// Beat position when the current recording session started.
    pub start_beat: f32,
    /// Recording UI phase (Idle / Arming / Recording / …).
    pub ui_state: RecordingUiState,
    /// Live recording waveform preview (Part 1), keyed by track id. Holds one
    /// entry per armed audio track currently drawing a growing preview clip
    /// in the arrangement — every simultaneously-armed track gets its own
    /// entry, mirroring `midi.tracks` below for MIDI takes.
    pub preview: HashMap<String, RecordingPreviewUi>,
    /// Native MIDI/instrument track capture. This is UI/control-path state — MIDI
    /// hardware/virtual-keyboard events already arrive on the control router, so
    /// no realtime audio callback work is required.
    pub midi: Option<MidiRecordingTake>,
    /// MIDI preview snapshots are expensive because they mirror the growing
    /// take into timeline state. Event edges render immediately; held-note
    /// growth is capped to 30 Hz so screen capture cannot multiply full-take
    /// clones and broad timeline redraws.
    pub midi_preview_dirty: bool,
    pub midi_preview_updated_at: Instant,
    /// Invalidates an outstanding count-in timer when the user cancels/restarts.
    pub count_in_token: u64,
    /// A take whose audio files the disk writer is still closing.
    ///
    /// Set when the engine detaches the take and cleared by
    /// `poll_recording_finalize`. The UI stays in `Finalizing` in between,
    /// which is now a state the user can actually see, because the thread that
    /// draws is no longer the thread waiting on the disk.
    pub awaiting_finalize: bool,
}

impl Default for RecordingSessionState {
    fn default() -> Self {
        Self {
            start_beat: 0.0,
            ui_state: RecordingUiState::Idle,
            preview: HashMap::new(),
            midi: None,
            midi_preview_dirty: false,
            midi_preview_updated_at: Instant::now() - Duration::from_secs(1),
            count_in_token: 0,
            awaiting_finalize: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MidiRecordingTake {
    pub start_beat: f32,
    pub tracks: HashMap<String, MidiRecordingTrack>,
}

#[derive(Debug, Clone)]
pub(crate) struct MidiRecordingTrack {
    pub track_id: String,
    pub track_name: String,
    pub notes: Vec<MidiNoteState>,
    pub active_notes: HashMap<(u8, u8), ActiveMidiNote>,
}

fn midi_recording_preview_clip_id(track_id: &str) -> String {
    format!("__recording_midi_preview__:{track_id}")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ActiveMidiNote {
    pub pitch: u8,
    pub velocity: u8,
    pub start_beat: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct MidiRecordingResult {
    pub track_id: String,
    pub track_name: String,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub notes: Vec<MidiNoteState>,
}

impl StudioLayout {
    pub(super) fn toggle_native_recording(&mut self, cx: &mut Context<Self>) {
        if matches!(self.recording.ui_state, RecordingUiState::CountingIn { .. }) {
            self.cancel_record_count_in(cx);
            return;
        }
        let recording = self.is_recording_active(cx);
        if recording {
            self.stop_native_recording(cx);
        } else {
            self.start_native_recording(cx);
        }
    }

    /// Whether any track is record-armed.
    ///
    /// What Space consults before deciding a press means Record rather than
    /// Play. Arm state only, not input or monitoring: a track can be armed with
    /// nothing plugged into it, and the transport should still behave the way
    /// the arm button says it will.
    pub(super) fn any_track_record_armed(&self, cx: &Context<Self>) -> bool {
        self.timeline
            .read(cx)
            .state
            .tracks
            .iter()
            .any(|track| track.armed)
    }

    pub(super) fn start_native_recording(&mut self, cx: &mut Context<Self>) {
        let (count_in_enabled, count_in_bars, playing, bpm, beats_per_bar, target_beat) = {
            let timeline = self.timeline.read(cx);
            let metronome = &self.settings.read(cx).current.recording.metronome;
            (
                metronome.count_in_enabled,
                metronome.count_in_bars.clamp(1, 4),
                timeline.state.transport.playing,
                timeline.state.effective_bpm_at_playhead().max(1.0) as f32,
                timeline.state.time_signature_at_playhead().numerator.max(1) as f32,
                timeline.state.transport.playhead_beats.max(0.0),
            )
        };
        if count_in_enabled && !playing {
            self.begin_record_count_in(count_in_bars, bpm, beats_per_bar, target_beat, cx);
            return;
        }
        self.start_native_recording_now(true, cx);
    }

    /// Run the record count-in, then start the take **at** `target_beat`.
    ///
    /// The count-in is a pre-roll *in place*: the playhead stays on the beat
    /// about to be recorded and the engine clicks the player in beside it. It
    /// used to be a transport roll through the bars before the record start,
    /// which meant it did nothing at all from bar 1 — there is no earlier
    /// timeline there, and the engine metronome is scheduled off the playhead
    /// and is silent while stopped. That is the most common way to start a
    /// take, and it was the one that counted in silence.
    ///
    /// Two things happen during the click that used to happen after it:
    ///
    /// * the take is **armed** — files created, writers built, the record sink
    ///   installed — so the downbeat costs nothing but starting the transport.
    ///   The take is gated on the transport (`capture_on_transport`), so
    ///   arming early captures nothing early.
    /// * the wait is measured on the **engine's own sample clock**
    ///   ([`count_in_remaining_samples`]), not on a wall-clock timer. A take
    ///   that starts off a `std::time` deadline lands wherever the scheduler
    ///   woke, which is exactly the jitter a count-in exists to remove.
    ///
    /// [`count_in_remaining_samples`]: DirectAudio::AudioEngine::count_in_remaining_samples
    fn begin_record_count_in(
        &mut self,
        bars: u32,
        bpm: f32,
        beats_per_bar: f32,
        target_beat: f32,
        cx: &mut Context<Self>,
    ) {
        self.recording.count_in_token = self.recording.count_in_token.wrapping_add(1);
        let token = self.recording.count_in_token;

        let beats_per_bar = beats_per_bar.max(1.0).round() as u32;
        let bars = bars.max(1);
        let beats = bars.saturating_mul(beats_per_bar).max(1);
        let target_beat = target_beat.max(0.0);

        // The click is rendered by the output callback, so the stream has to be
        // running before the count-in is asked for — otherwise the first bar is
        // spent opening the device.
        self.ensure_audio_stream_warm();

        let started = self
            .audio_bridge
            .engine
            .as_ref()
            .map(|engine| {
                engine
                    .start_count_in(beats, beats_per_bar, target_beat as f64)
                    .map_err(|error| error.to_string())
            })
            .unwrap_or_else(|| Err("audio engine unavailable".to_string()));

        // How long one click is, in the engine's own samples. Taken from the
        // engine rather than recomputed from the tempo map: it already resolved
        // the spacing when it accepted the count-in, and two derivations of the
        // same number are two chances to disagree by a beat.
        let samples_per_beat = self
            .audio_bridge
            .engine
            .as_ref()
            .map(|engine| engine.count_in_remaining_samples())
            .filter(|total| *total > 0)
            .map(|total| total / beats.max(1) as u64)
            .unwrap_or(0)
            .max(1);

        self.recording.ui_state = RecordingUiState::CountingIn {
            bars,
            beats_total: beats,
            beats_left: beats,
        };
        cx.notify();

        // Arm now, play at the downbeat.
        self.start_native_recording_now(false, cx);
        if !matches!(self.recording.ui_state, RecordingUiState::CountingIn { .. }) {
            // Arming failed and reported why; there is no take to count into.
            if let Some(engine) = self.audio_bridge.engine.as_ref() {
                let _ = engine.cancel_count_in();
            }
            return;
        }

        // Fallback deadline when the engine could not run the click. The take
        // still starts on time; the player just does not hear the count.
        let fallback_seconds = {
            let timeline = self.timeline.read(cx);
            let base = timeline.state.bpm.max(1.0) as f64;
            let map = &timeline.state.tempo_map;
            let per_beat = map.seconds_at_beat(target_beat as f64 + 1.0, base).max(0.0)
                - map.seconds_at_beat(target_beat as f64, base);
            let per_beat = if per_beat > 0.0 {
                per_beat
            } else {
                60.0 / bpm.max(1.0) as f64
            };
            (per_beat * beats as f64).max(0.05) as f32
        };
        if let Err(error) = &started {
            eprintln!("[recording] count-in click unavailable: {error}");
        }
        let engine_clocked = started.is_ok();

        let owner = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let deadline =
                std::time::Instant::now() + Duration::from_secs_f32(fallback_seconds + 1.0);
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(if engine_clocked { 5 } else { 20 }))
                    .await;
                let finished = owner.update(cx, |this, _cx| {
                    if this.recording.count_in_token != token
                        || !matches!(this.recording.ui_state, RecordingUiState::CountingIn { .. })
                    {
                        // Cancelled, or superseded by a newer Record.
                        return None;
                    }
                    let remaining = this
                        .audio_bridge
                        .engine
                        .as_ref()
                        .map(|engine| engine.count_in_remaining_samples());

                    // Publish the number the player is waiting on. Rounded up:
                    // with two and a half beats to go the count still reads 3,
                    // because the click for that beat has not sounded yet.
                    if let Some(remaining) = remaining {
                        let left = remaining.div_ceil(samples_per_beat.max(1)) as u32;
                        let left = left.min(beats);
                        if let RecordingUiState::CountingIn { beats_left, .. } =
                            &mut this.recording.ui_state
                        {
                            if *beats_left != left {
                                *beats_left = left;
                                _cx.notify();
                            }
                        }
                    }

                    // A stalled or absent engine must not leave the UI
                    // counting forever — the deadline always ends it.
                    let done = match remaining {
                        Some(remaining) if engine_clocked => remaining == 0,
                        _ => std::time::Instant::now() >= deadline,
                    };
                    Some(done || std::time::Instant::now() >= deadline)
                });
                match finished {
                    None => return,
                    Some(false) => continue,
                    Some(true) => break,
                }
            }
            let _ = owner.update(cx, |this, cx| {
                if this.recording.count_in_token != token
                    || !matches!(this.recording.ui_state, RecordingUiState::CountingIn { .. })
                {
                    return;
                }
                this.recording.ui_state = RecordingUiState::Recording;
                this.start_native_playback(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_record_count_in(&mut self, cx: &mut Context<Self>) {
        self.recording.count_in_token = self.recording.count_in_token.wrapping_add(1);
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            let _ = engine.cancel_count_in();
        }
        // The take was armed when the count-in began, so cancelling has to
        // discard it — otherwise the writers stay open on a take nobody asked
        // to keep and the next Record is refused as "already recording".
        self.discard_armed_count_in_take(cx);
        self.stop_native_playback(cx);
        self.recording.ui_state = RecordingUiState::Idle;
        cx.notify();
    }

    /// Throw away a take that was armed for a count-in that never reached its
    /// downbeat. Nothing was captured — the take is gated on the transport and
    /// the transport never rolled — so the files it created are deleted rather
    /// than committed as an empty clip.
    fn discard_armed_count_in_take(&mut self, cx: &mut Context<Self>) {
        self.recording.midi = None;
        self.clear_midi_recording_previews(cx);
        let Some(engine) = self.audio_bridge.engine.clone() else {
            return;
        };
        if !engine.recording_status().active {
            return;
        }
        match engine.stop_recording() {
            Ok(results) => {
                for result in results {
                    if result.file_path.is_empty() {
                        continue;
                    }
                    let _ = std::fs::remove_file(&result.file_path);
                    if !result.metadata_path.is_empty() {
                        let _ = std::fs::remove_file(&result.metadata_path);
                    }
                }
            }
            Err(error) => eprintln!("[recording] discarding cancelled count-in take: {error}"),
        }
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.state.transport.recording = false;
            cx.notify();
        });
    }

    /// Arm the take and, unless `start_transport` is false, roll.
    ///
    /// A count-in arms without rolling: the writers, the files and the record
    /// sink are all built while the click runs, so the downbeat costs nothing
    /// but a transport start. The take itself is gated on the transport, so
    /// arming early captures nothing early.
    fn start_native_recording_now(&mut self, start_transport: bool, cx: &mut Context<Self>) {
        if start_transport {
            self.recording.ui_state = RecordingUiState::Preparing;
        }
        cx.notify();

        // Clone the engine handle (cheap, Arc-backed) so the local `engine`
        // borrow targets `engine_handle` instead of `self`. That lets the
        // `save_before_recording` auto-save run *after* every record
        // precondition is validated (see below), rather than before — clicking
        // Record on a project that cannot start a take (no engine, no armed
        // audio track, device conflict) must not silently save the project.
        let Some(engine_handle) = self.audio_bridge.engine.clone() else {
            self.fail_recording_start("audio engine unavailable", cx);
            return;
        };
        let engine = &engine_handle;

        let input_devices: Vec<RecordingInputDevice> = engine
            .list_input_devices()
            .into_iter()
            .map(|d| RecordingInputDevice {
                id: d.id,
                name: d.name,
                channels: d.channels,
                is_default: d.is_default,
            })
            .collect();

        // Auto-arm the record target. Both the "R" shortcut and the transport
        // Record button route here; the most common reason Record appears to do
        // nothing is a selected-but-not-armed track, which then fails the armed
        // check below and reports only into `last_error`. When nothing is armed,
        // arm the selected track (if it can record) so the ordinary "click a
        // track, press Record" flow just works instead of silently failing.
        let already_armed = {
            let timeline = self.timeline.read(cx);
            timeline.state.tracks.iter().any(|t| {
                t.armed
                    && matches!(
                        t.track_type,
                        TrackType::Audio | TrackType::Midi | TrackType::Instrument
                    )
            })
        };
        if !already_armed {
            let arm_target = {
                let timeline = self.timeline.read(cx);
                timeline
                    .state
                    .selection
                    .selected_track_id
                    .clone()
                    .filter(|id| {
                        timeline
                            .state
                            .find_track(id)
                            .map(|t| {
                                matches!(
                                    t.track_type,
                                    TrackType::Audio | TrackType::Midi | TrackType::Instrument
                                )
                            })
                            .unwrap_or(false)
                    })
            };
            if let Some(id) = arm_target {
                self.timeline.update(cx, |timeline, cx| {
                    timeline.state.toggle_track_arm(&id);
                    cx.notify();
                });
            }
        }

        let (bpm, start_beat, sample_rate, input_device_name, midi_tracks) = {
            let timeline = self.timeline.read(cx);
            let settings = self.settings.read(cx);
            let audio_armed = timeline
                .state
                .tracks
                .iter()
                .any(|t| t.armed && t.track_type == TrackType::Audio);
            let midi_tracks = timeline
                .state
                .tracks
                .iter()
                .filter(|t| {
                    t.armed && matches!(t.track_type, TrackType::Midi | TrackType::Instrument)
                })
                .map(|track| MidiRecordingTrack {
                    track_id: track.id.clone(),
                    track_name: track.name.clone(),
                    notes: Vec::new(),
                    active_notes: HashMap::new(),
                })
                .collect::<Vec<_>>();
            if !audio_armed && midi_tracks.is_empty() {
                self.fail_recording_start("no armed audio or MIDI tracks", cx);
                return;
            }
            (
                timeline.state.bpm,
                timeline.state.transport.playhead_beats,
                self.current_audio_sample_rate(),
                settings.current.hardware.audio.device_in.clone(),
                midi_tracks,
            )
        };

        let selected_input_device =
            select_recording_input_device(&input_devices, &input_device_name);

        let (tracks, explicit_device_id, monitor_channels): (
            Vec<JsRecordingTrackConfig>,
            Option<String>,
            Vec<u32>,
        ) = {
            let timeline = self.timeline.read(cx);
            let connections_for_recording = timeline.state.audio_connections.clone();
            let mut explicit_device_id: Option<String> = None;
            let mut monitor_channels = Vec::new();
            let mut configs = Vec::new();
            for track in timeline
                .state
                .tracks
                .iter()
                .filter(|t| t.armed && t.track_type == TrackType::Audio)
            {
                let route = match recording_input_channels_checked(
                    track,
                    &connections_for_recording,
                    &input_devices,
                    selected_input_device,
                ) {
                    Ok(route) => route,
                    Err(error) => {
                        self.fail_recording_start(&error, cx);
                        return;
                    }
                };
                if let Some(device_id) = route.device_id.clone() {
                    match explicit_device_id.as_ref() {
                        Some(existing) if existing != &device_id => {
                            self.fail_recording_start(
                                "armed tracks use different input devices; record one input device at a time",
                                cx,
                            );
                            return;
                        }
                        None => explicit_device_id = Some(device_id),
                        _ => {}
                    }
                }
                if monitor_channels.is_empty() && track.input_monitor.is_active(track.armed) {
                    monitor_channels = route.channels.clone();
                }
                configs.push(JsRecordingTrackConfig {
                    track_id: track.id.clone(),
                    input_channels: route.channels,
                    name: track.name.clone(),
                });
            }
            (configs, explicit_device_id, monitor_channels)
        };

        if !tracks.is_empty() {
            let Some(_project_root) = self.project_folder.as_ref() else {
                self.fail_recording_start("save the project to a folder before recording", cx);
                return;
            };

            // Every audio-record precondition has now passed (engine present, at
            // least one armed audio track, input devices/channels resolved). Only
            // now honour `save_before_recording`: the auto-save must never fire
            // for a Record click that won't actually start an audio take.
            let save_before_recording = self
                .settings
                .read(cx)
                .current
                .recording
                .audio
                .save_before_recording;
            if save_before_recording && self.project_session.is_dirty {
                let Some(project_path) = self.project_session.project_file_path.clone() else {
                    self.fail_recording_start("save the project to a folder before recording", cx);
                    return;
                };
                if !self.do_save_project(&project_path, cx) {
                    self.fail_recording_start("could not save the project before recording", cx);
                    return;
                }
            }
        }

        let input_device_id = explicit_device_id.or_else(|| {
            selected_input_device
                .as_ref()
                .map(|device| device.id.clone())
                .or_else(|| resolve_input_device_id(engine, &input_device_name))
        });
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
            .to_string();
        // A take id must be new on every Record, including two presses inside
        // the same millisecond: the id names the file and the take, and a
        // collision is how a second take lands on the first one's material.
        let session_id = {
            static RECORD_SESSION_SEQ: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let seq = RECORD_SESSION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("rec-{timestamp}-{seq}")
        };
        let project_name = self.project_session.name.clone();

        if !midi_tracks.is_empty() {
            self.recording.midi = Some(MidiRecordingTake {
                start_beat: start_beat.max(0.0),
                tracks: midi_tracks
                    .into_iter()
                    .map(|track| (track.track_id.clone(), track))
                    .collect(),
            });
            self.recording.midi_preview_dirty = true;
            self.recording.midi_preview_updated_at = Instant::now() - Duration::from_secs(1);
        } else {
            self.recording.midi = None;
        }

        if !tracks.is_empty() {
            let Some(project_root) = self.project_folder.as_ref() else {
                self.recording.midi = None;
                self.fail_recording_start("save the project to a folder before recording", cx);
                return;
            };
            let config = JsStartRecordingConfig {
                project_root: project_root.to_string_lossy().into_owned(),
                project_name,
                session_id,
                timestamp,
                bpm: bpm.max(1.0) as f64,
                start_beat: start_beat.max(0.0) as f64,
                capture_on_transport: Some(true),
                sample_rate: sample_rate.max(1),
                input_device_id,
                tracks,
                monitor_mix: !monitor_channels.is_empty(),
                monitor_channels,
            };

            if let Err(error) = engine.start_recording(config) {
                self.recording.midi = None;
                self.fail_recording_start(&error.to_string(), cx);
                return;
            }
        }

        self.recording.start_beat = start_beat.max(0.0);
        if let Some(take) = self.recording.midi.as_ref() {
            let preview_tracks: Vec<(String, String)> = take
                .tracks
                .values()
                .map(|track| {
                    (
                        midi_recording_preview_clip_id(&track.track_id),
                        track.track_id.clone(),
                    )
                })
                .collect();
            let start_beat = self.recording.start_beat;
            let _ = self.timeline.update(cx, |timeline, cx| {
                for (clip_id, track_id) in &preview_tracks {
                    timeline
                        .state
                        .begin_midi_recording_preview_clip(clip_id, track_id, start_beat);
                }
                if !preview_tracks.is_empty() {
                    cx.notify();
                }
            });
        }
        self.audio_bridge.last_error = None;
        if start_transport {
            self.recording.ui_state = RecordingUiState::Recording;
        }
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.state.transport.recording = true;
            cx.notify();
        });

        if !start_transport {
            // Armed and waiting for the count-in's downbeat.
            return;
        }
        let playing = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|s| s.transport_playing)
            .unwrap_or(false);
        if !playing {
            self.start_native_playback(cx);
        }
    }

    pub(super) fn stop_native_recording(&mut self, cx: &mut Context<Self>) {
        if matches!(self.recording.ui_state, RecordingUiState::CountingIn { .. }) {
            self.cancel_record_count_in(cx);
            return;
        }
        // Stopping a take used to block this thread until the disk writer had
        // drained and closed its files — an unbounded wait on I/O, on the
        // thread that draws, which is the application freezing at the end of
        // every take.
        //
        // It does not any more. The engine detaches the take here, which is
        // immediate, and the files are collected by `poll_recording_finalize`
        // from the audio tick that already runs. `FUTUREBOARD_RECORDING_DEBUG=1`
        // still prints where the time goes, because the point of this is that
        // none of the steps below are allowed to grow one.
        let stop_debug = std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some();
        let stop_started = std::time::Instant::now();
        let mut mark = stop_started;
        let mut step = |label: &str| {
            if stop_debug {
                let now = std::time::Instant::now();
                eprintln!(
                    "[recording] stop {label}: {:.1} ms (total {:.1} ms)",
                    now.duration_since(mark).as_secs_f64() * 1000.0,
                    now.duration_since(stop_started).as_secs_f64() * 1000.0,
                );
                mark = now;
            }
        };

        self.stop_recording_transport_ui(cx);
        self.recording.ui_state = RecordingUiState::Finalizing;
        cx.notify();
        step("transport ui");

        self.clear_midi_recording_previews(cx);
        let midi_results = self.finish_midi_recording_take(cx);
        let midi_committed = !midi_results.is_empty();
        step("midi take");

        if let Some(engine) = self.audio_bridge.engine.clone() {
            let engine_recording = engine.recording_status().active;
            if let Err(error) = engine.pause() {
                self.audio_bridge.last_error = Some(error.to_string());
                eprintln!("[audio] stop transport while recording failed: {error}");
            }
            self.audio_bridge.stats = Some(engine.stats());
            step("engine pause");

            if engine_recording {
                // Detach only. The writer closes the files on its own thread,
                // and `poll_recording_finalize` picks them up.
                if let Err(error) = engine.begin_stop_recording() {
                    self.audio_bridge.last_error = Some(error.to_string());
                    self.recording.ui_state = RecordingUiState::Failed {
                        reason: error.to_string(),
                    };
                    eprintln!("[recording] stop failed: {error}");
                    return;
                }
                step("engine detach take");
                self.recording.awaiting_finalize = true;
            }
        } else if midi_results.is_empty() {
            self.recording.ui_state = RecordingUiState::Failed {
                reason: "audio engine unavailable".to_string(),
            };
            cx.notify();
            return;
        }

        self.commit_midi_recording_results(cx, midi_results);
        step("commit midi results");

        if !matches!(self.recording.ui_state, RecordingUiState::Failed { .. }) {
            // Still finalizing means the audio files are not closed yet, so the
            // take is not done and the state must not say it is. The poll below
            // moves it to Idle when the writer hands the files over.
            if !self.recording.awaiting_finalize {
                self.recording.ui_state = RecordingUiState::Idle;
            }
            if midi_committed {
                self.audio_bridge.project_dirty = true;
                self.audio_bridge.media_dirty = true;
                self.schedule_audio_project_sync(cx, true, "recording_commit");
            }
            cx.notify();
        }
    }

    /// Collect a detached take's files once the disk writer has closed them.
    ///
    /// Called from the audio tick, which already runs every frame. Cheap when
    /// there is nothing to collect: one `Option` check, then a `try_recv` that
    /// never waits.
    pub(super) fn poll_recording_finalize(&mut self, cx: &mut Context<Self>) {
        if !self.recording.awaiting_finalize {
            return;
        }
        let Some(engine) = self.audio_bridge.engine.clone() else {
            self.recording.awaiting_finalize = false;
            return;
        };
        let Some(outcome) = engine.poll_stop_recording() else {
            return;
        };
        self.recording.awaiting_finalize = false;

        match outcome {
            Ok(results) => {
                if std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some() {
                    eprintln!("[recording] finalize collected {} file(s)", results.len());
                }
                self.commit_recording_results(cx, results);
                if !matches!(self.recording.ui_state, RecordingUiState::Failed { .. }) {
                    self.recording.ui_state = RecordingUiState::Idle;
                }
            }
            Err(error) => {
                self.audio_bridge.last_error = Some(error.to_string());
                self.recording.ui_state = RecordingUiState::Failed {
                    reason: error.to_string(),
                };
                eprintln!("[recording] finalize failed: {error}");
            }
        }
        cx.notify();
    }

    fn stop_recording_transport_ui(&mut self, cx: &mut Context<Self>) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            let mut changed = false;
            if timeline.state.transport.recording {
                timeline.state.transport.recording = false;
                changed = true;
            }
            if timeline.state.transport.playing {
                timeline.state.transport.playing = false;
                changed = true;
            }
            if changed {
                cx.notify();
            }
        });
        self.finish_recording_preview(cx);
    }

    pub(super) fn capture_midi_record_event(
        &mut self,
        track_id: &str,
        event: &MidiInputEvent,
        cx: &App,
    ) {
        self.capture_midi_record_event_at(track_id, event, None, cx);
    }

    pub(super) fn capture_midi_record_event_at(
        &mut self,
        track_id: &str,
        event: &MidiInputEvent,
        event_beat: Option<f32>,
        cx: &App,
    ) {
        let current_beat =
            event_beat.unwrap_or_else(|| self.timeline.read(cx).state.transport.playhead_beats);
        let Some(take) = self.recording.midi.as_mut() else {
            return;
        };
        let relative_beat = (current_beat - take.start_beat).max(0.0);
        let Some(track) = take.tracks.get_mut(track_id) else {
            return;
        };

        match *event {
            MidiInputEvent::NoteOn {
                note,
                velocity,
                channel,
            } => {
                let pitch = note.min(127);
                let velocity = velocity.min(127);
                let channel = channel.min(15);
                let key = (channel, pitch);
                if let Some(active) = track.active_notes.remove(&key) {
                    push_recorded_midi_note(track, active, relative_beat);
                }
                track.active_notes.insert(
                    key,
                    ActiveMidiNote {
                        pitch,
                        velocity,
                        start_beat: relative_beat,
                    },
                );
            }
            MidiInputEvent::NoteOff { note, channel } => {
                let key = (channel.min(15), note.min(127));
                if let Some(active) = track.active_notes.remove(&key) {
                    push_recorded_midi_note(track, active, relative_beat);
                }
            }
            MidiInputEvent::AllNotesOff | MidiInputEvent::Panic => {
                close_all_recorded_midi_notes(track, relative_beat);
            }
            MidiInputEvent::ControlChange { .. } => {}
        }
        self.recording.midi_preview_dirty = true;
    }

    fn finish_midi_recording_take(&mut self, cx: &mut Context<Self>) -> Vec<MidiRecordingResult> {
        let end_beat = self.timeline.read(cx).state.transport.playhead_beats;
        let Some(mut take) = self.recording.midi.take() else {
            return Vec::new();
        };
        let relative_end = (end_beat - take.start_beat).max(0.0);
        let mut results = Vec::new();
        for (_, mut track) in take.tracks.drain() {
            close_all_recorded_midi_notes(&mut track, relative_end);
            if track.notes.is_empty() {
                continue;
            }
            let note_end = track
                .notes
                .iter()
                .map(|note| note.start + note.duration)
                .fold(0.0_f32, f32::max);
            results.push(MidiRecordingResult {
                track_id: track.track_id,
                track_name: track.track_name,
                start_beat: take.start_beat,
                duration_beats: relative_end.max(note_end).max(MIN_NOTE_BEATS),
                notes: track.notes,
            });
        }
        results
    }

    fn commit_midi_recording_results(
        &mut self,
        cx: &mut Context<Self>,
        results: Vec<MidiRecordingResult>,
    ) {
        if results.is_empty() {
            return;
        }

        self.timeline.update(cx, |timeline, cx| {
            let mut selected_clip_ids = Vec::new();
            let mut selected_track_id = None;
            for result in results {
                let clip_id = timeline.state.next_clip_id();
                let clip = ClipState {
                    id: clip_id.clone(),
                    name: format!("{} MIDI Recording", result.track_name),
                    start_beat: result.start_beat.max(0.0),
                    duration_beats: result.duration_beats.max(MIN_NOTE_BEATS),
                    source_duration_seconds: None,
                    offset_beats: 0.0,
                    gain: 1.0,
                    clip_type: ClipType::Midi {
                        notes: result.notes,
                        controller_lanes: Vec::<MidiControllerLane>::new(),
                        sysex_events: Vec::new(),
                        articulations: Vec::new(),
                    },
                    muted: false,
                    audio_import: AudioImportState::default(),
                    stretch: AudioClipStretchState::default(),
                };
                if let Some(track) = timeline
                    .state
                    .tracks
                    .iter_mut()
                    .find(|track| track.id == result.track_id)
                {
                    track.clips.push(clip);
                    selected_track_id = Some(track.id.clone());
                    selected_clip_ids.push(clip_id);
                }
            }
            if !selected_clip_ids.is_empty() {
                timeline.state.selection.selected_track_id = selected_track_id;
                timeline.state.selection.selected_clip_ids = selected_clip_ids;
                cx.notify();
            }
        });
    }

    fn commit_recording_results(
        &mut self,
        cx: &mut Context<Self>,
        results: Vec<DirectAudio::types::JsRecordingResult>,
    ) {
        let bpm = self.timeline.read(cx).state.bpm;
        let timeline = self.timeline.clone();
        let (generate_waveforms, recording_offset_ms) = {
            let settings = self.settings.read(cx);
            (
                settings
                    .current
                    .recording
                    .audio
                    .generate_waveform_after_record,
                settings.current.recording.audio.recording_offset_ms,
            )
        };
        let recording_offset_beats = recording_offset_ms as f32 / 1000.0 * bpm / 60.0;
        let mut import_paths: Vec<(PathBuf, String)> = Vec::new();
        let mut failed_tracks: Vec<String> = Vec::new();
        // One stamp for the whole pass, so every track's take from this Record
        // reads as the same moment — which is what they are.
        let take_stamp = take_timestamp_label();

        let _ = self.timeline.update(cx, |timeline, cx| {
            for result in &results {
                if !result.success {
                    failed_tracks.push(format!(
                        "{}: {}",
                        result.track_id,
                        result.error.as_deref().unwrap_or("unknown error")
                    ));
                    eprintln!(
                        "[recording] track {} failed: {}",
                        result.track_id,
                        result.error.as_deref().unwrap_or("unknown error")
                    );
                    continue;
                }
                let clip_name = Path::new(&result.relative_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Recording")
                    .to_string();
                // Auto latency compensation: pull the take earlier by the
                // engine-measured round-trip so an overdub lands where the
                // performer heard it. The manual `recording_offset_ms` is layered
                // on top for per-rig fine-tuning (it may be negative or positive).
                let latency_beats = result.latency_seconds as f32 * bpm / 60.0;
                let placed_beat =
                    (result.start_beat as f32 - latency_beats + recording_offset_beats).max(0.0);
                if latency_beats != 0.0
                    && std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some()
                {
                    eprintln!(
                        "[recording] latency comp track={} raw_start={:.3} latency={:.1}ms (-{:.3} beats) manual_offset={:.3} beats -> start={:.3}",
                        result.track_id,
                        result.start_beat,
                        result.latency_seconds * 1000.0,
                        latency_beats,
                        recording_offset_beats,
                        placed_beat
                    );
                }
                let clip_id = timeline.state.insert_recorded_clip(
                    &result.track_id,
                    result.file_path.clone(),
                    clip_name,
                    placed_beat,
                    result.duration_seconds,
                    bpm,
                );
                // Register the pass as a take. A second pass over the same bars
                // becomes the active take and mutes the one it replaced, which
                // is what makes Record twice a comp instead of two clips
                // playing over each other.
                timeline.state.register_recorded_take(
                    &result.track_id,
                    &clip_id,
                    take_stamp.clone(),
                );
                if generate_waveforms {
                    import_paths.push((PathBuf::from(&result.file_path), result.file_path.clone()));
                }
                eprintln!(
                    "[recording] clip created id={clip_id} track={} path={}",
                    result.track_id, result.relative_path
                );
            }
            cx.notify();
        });

        if !failed_tracks.is_empty() {
            let reason = format!("recording finalize failed ({})", failed_tracks.join("; "));
            self.audio_bridge.last_error = Some(reason.clone());
            self.recording.ui_state = RecordingUiState::Failed { reason };
            cx.notify();
            return;
        }

        self.audio_bridge.project_dirty = true;
        self.audio_bridge.media_dirty = true;
        self.schedule_audio_project_sync(cx, true, "recording_commit");

        for (path, path_key) in import_paths {
            self.spawn_timeline_audio_import_jobs(cx, timeline.clone(), path, path_key);
        }
    }

    /// Poll the engine's per-track recording preview rings and keep every
    /// armed audio track's temporary preview clip + streamed waveform in sync
    /// (Part 1, multi-track). Called every audio tick.
    pub(super) fn update_recording_preview(&mut self, cx: &mut Context<Self>) {
        self.update_midi_recording_previews(cx);
        if !self.timeline.read(cx).state.transport.recording {
            if !self.recording.preview.is_empty()
                && std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some()
            {
                eprintln!("[RecordingPreviewUI] ignored_late_peaks transport_recording=false");
            }
            self.finish_recording_preview(cx);
            return;
        }
        let Some(engine) = self.audio_bridge.engine.clone() else {
            return;
        };
        // Authoritative per-track list: the engine resolved exactly which
        // armed tracks (and their channels) are feeding a live preview ring
        // when the take started — no need to re-derive it from timeline
        // arm state here.
        let track_infos = engine.recording_preview_tracks();
        if track_infos.is_empty() {
            self.finish_recording_preview(cx);
            return;
        }

        // Drop any tracked preview whose track id fell out of the engine's
        // active set (take ended) or whose take id changed (stale from a
        // previous take reusing the same track).
        let stale_track_ids: Vec<String> = self
            .recording
            .preview
            .iter()
            .filter(|(id, p)| {
                track_infos
                    .iter()
                    .find(|t| &t.track_id == *id)
                    .map(|t| t.recording_id as u64 != p.recording_id)
                    .unwrap_or(true)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for track_id in stale_track_ids {
            self.teardown_recording_preview_track(&track_id, cx);
        }

        // (Re)initialize the preview clip for any track not yet tracked.
        for info in &track_infos {
            if self.recording.preview.contains_key(&info.track_id) {
                continue;
            }
            let rec_id = info.recording_id as u64;
            let start_beat = self.recording.start_beat.max(0.0);
            let clip_id = audio_recording_preview_clip_id(&info.track_id);
            waveform_cache::clear_recording_preview(&clip_id);
            if std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some() {
                eprintln!(
                    "[RecordingPreviewUI] started id={rec_id} track_id={} clip_id={clip_id}",
                    info.track_id
                );
            }
            let (cid, tid) = (clip_id.clone(), info.track_id.clone());
            self.timeline.update(cx, |timeline, cx| {
                timeline
                    .state
                    .begin_recording_preview_clip(&cid, &tid, start_beat);
                cx.notify();
            });
            self.recording.preview.insert(
                info.track_id.clone(),
                RecordingPreviewUi {
                    clip_id,
                    recording_id: rec_id,
                    track_id: info.track_id.clone(),
                    start_beat,
                    sample_rate: info.sample_rate,
                    peaks_per_second: info.peaks_per_second.max(1),
                    drained: 0,
                    peaks: Vec::new(),
                },
            );
        }

        let bpm = {
            let timeline = self.timeline.read(cx);
            timeline.state.bpm
        };

        // Drain newly produced peaks per track and republish each waveform.
        let mut length_updates: Vec<(String, f32)> = Vec::new();
        for info in &track_infos {
            let Some(p) = self.recording.preview.get_mut(&info.track_id) else {
                continue;
            };
            let new = engine
                .drain_recording_preview_peaks_for_track(info.track_id.clone(), p.drained as f64);
            if !new.is_empty() {
                p.drained += new.len() as u64;
                if std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some() {
                    eprintln!(
                        "[RecordingPreviewUI] peaks id={} track_id={} count={}",
                        p.recording_id,
                        p.track_id,
                        new.len()
                    );
                }
                p.peaks.extend(new.into_iter().map(|q| WaveformPeak {
                    min: q.min as f32,
                    max: q.max as f32,
                }));
                let wf = waveform_cache::preview_from_recording_peaks(
                    &p.peaks,
                    p.sample_rate,
                    p.peaks_per_second,
                );
                waveform_cache::set_recording_preview(&p.clip_id, std::sync::Arc::new(wf));
            }
            let length_seconds = p.peaks.len() as f64 / p.peaks_per_second.max(1) as f64;
            let length_beats = (length_seconds * bpm.max(1.0) as f64 / 60.0) as f32;
            length_updates.push((p.clip_id.clone(), length_beats));
        }
        if !length_updates.is_empty() {
            self.timeline.update(cx, |timeline, cx| {
                let mut changed = false;
                for (clip_id, length_beats) in length_updates {
                    changed |= timeline
                        .state
                        .set_recording_preview_clip_length(&clip_id, length_beats);
                }
                if changed {
                    cx.notify();
                }
            });
        }
    }

    /// Tear down one track's live recording preview clip + registry entry.
    fn teardown_recording_preview_track(&mut self, track_id: &str, cx: &mut Context<Self>) {
        let Some(preview) = self.recording.preview.remove(track_id) else {
            return;
        };
        if std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some() {
            eprintln!(
                "[RecordingPreviewUI] finished id={} track_id={track_id}",
                preview.recording_id
            );
        }
        waveform_cache::clear_recording_preview(&preview.clip_id);
        let clip_id = preview.clip_id;
        self.timeline.update(cx, |timeline, cx| {
            if timeline.state.remove_recording_preview_clip(&clip_id) {
                cx.notify();
            }
        });
    }

    /// Tear down every tracked live recording preview clip + registry entry.
    pub(super) fn finish_recording_preview(&mut self, cx: &mut Context<Self>) {
        let track_ids: Vec<String> = self.recording.preview.keys().cloned().collect();
        for track_id in track_ids {
            self.teardown_recording_preview_track(&track_id, cx);
        }
    }

    /// Mirror captured MIDI notes into temporary arrangement clips. This runs
    /// on the UI/control tick, never in the audio callback, and includes held
    /// notes with a growing duration so the user sees a note immediately on
    /// NoteOn rather than only after NoteOff/stop.
    fn update_midi_recording_previews(&mut self, cx: &mut Context<Self>) {
        const MIDI_PREVIEW_INTERVAL: Duration = Duration::from_millis(33);
        if self.recording.midi.is_none()
            || (!self.recording.midi_preview_dirty
                && self.recording.midi_preview_updated_at.elapsed() < MIDI_PREVIEW_INTERVAL)
        {
            return;
        }
        self.recording.midi_preview_dirty = false;
        self.recording.midi_preview_updated_at = Instant::now();
        let now = self.timeline.read(cx).state.transport.playhead_beats;
        let previews = self.recording.midi.as_ref().map(|take| {
            let relative_beat = (now - take.start_beat).max(0.0);
            take.tracks
                .values()
                .map(|track| {
                    let mut notes = track.notes.clone();
                    notes.extend(track.active_notes.values().map(|active| {
                        MidiNoteState::new(
                            active.pitch,
                            active.start_beat.max(0.0),
                            (relative_beat - active.start_beat).max(MIN_NOTE_BEATS),
                            active.velocity,
                        )
                    }));
                    (
                        midi_recording_preview_clip_id(&track.track_id),
                        relative_beat.max(MIN_NOTE_BEATS),
                        notes,
                    )
                })
                .collect::<Vec<_>>()
        });
        let Some(previews) = previews else {
            return;
        };
        let _ = self.timeline.update(cx, |timeline, cx| {
            let mut changed = false;
            for (clip_id, duration_beats, notes) in previews {
                changed |= timeline.state.update_midi_recording_preview_clip(
                    &clip_id,
                    duration_beats,
                    notes,
                );
            }
            if changed {
                cx.notify();
            }
        });
    }

    fn clear_midi_recording_previews(&mut self, cx: &mut Context<Self>) {
        let preview_ids = self
            .recording
            .midi
            .as_ref()
            .map(|take| {
                take.tracks
                    .keys()
                    .map(|track_id| midi_recording_preview_clip_id(track_id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if preview_ids.is_empty() {
            return;
        }
        let _ = self.timeline.update(cx, |timeline, cx| {
            let mut changed = false;
            for clip_id in &preview_ids {
                changed |= timeline.state.remove_recording_preview_clip(clip_id);
            }
            if changed {
                cx.notify();
            }
        });
    }

    fn fail_recording_start(&mut self, message: &str, cx: &mut Context<Self>) {
        self.audio_bridge.last_error = Some(message.to_string());
        self.recording.ui_state = RecordingUiState::Failed {
            reason: message.to_string(),
        };
        self.recording.midi = None;
        eprintln!("[recording] start blocked: {message}");
        cx.notify();
    }
}

/// `HH:MM` local time for a take row's secondary line.
///
/// A label, never a value: it says which pass this was in a list of passes from
/// the same session, and the hour and minute are enough for that. Derived from
/// the wall clock without a date library — the seconds since the epoch modulo a
/// day, offset by nothing, which is UTC. Good enough to order and tell apart
/// takes recorded minutes from each other.
fn take_timestamp_label() -> String {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let seconds_today = now.as_secs() % 86_400;
    format!(
        "{:02}:{:02}",
        seconds_today / 3_600,
        (seconds_today / 60) % 60
    )
}

fn push_recorded_midi_note(track: &mut MidiRecordingTrack, active: ActiveMidiNote, end_beat: f32) {
    let duration = (end_beat - active.start_beat).max(MIN_NOTE_BEATS);
    track.notes.push(MidiNoteState::new(
        active.pitch,
        active.start_beat.max(0.0),
        duration,
        active.velocity,
    ));
}

fn close_all_recorded_midi_notes(track: &mut MidiRecordingTrack, end_beat: f32) {
    let active_notes = std::mem::take(&mut track.active_notes);
    for active in active_notes.into_values() {
        push_recorded_midi_note(track, active, end_beat);
    }
}

impl StudioLayout {
    /// `(device name, output channel count)` for the currently selected global
    /// output device, used to populate hardware output routes in the Inspector.
    pub(super) fn selected_output_device_channels(
        &self,
        cx: &Context<Self>,
    ) -> Option<(String, u32)> {
        let engine = self.audio_bridge.engine.as_ref()?;
        let wanted = self
            .settings
            .read(cx)
            .current
            .hardware
            .audio
            .device_out
            .clone();
        let devices = engine.list_output_devices();
        if !wanted.trim().is_empty() {
            if let Some(d) = devices.iter().find(|d| d.name == wanted || d.id == wanted) {
                return Some((d.name.clone(), d.channels));
            }
        }
        devices
            .iter()
            .find(|d| d.is_default)
            .or_else(|| devices.first())
            .map(|d| (d.name.clone(), d.channels))
    }
}

#[derive(Clone, Debug)]
struct RecordingInputDevice {
    id: String,
    name: String,
    channels: u32,
    is_default: bool,
}

#[derive(Clone, Debug)]
struct RecordingInputRoute {
    device_id: Option<String>,
    channels: Vec<u32>,
}

fn recording_input_channels_checked(
    track: &TrackState,
    connections: &crate::audio_connections::AudioConnectionRegistry,
    devices: &[RecordingInputDevice],
    selected_device: Option<&RecordingInputDevice>,
) -> Result<RecordingInputRoute, String> {
    // A track stores only a logical connection id; the registry is the only
    // layer that knows physical ports. MIDI never reaches here — it stays on
    // `routing.midi_input`.
    let Some(connection_id) = track.routing.audio_input_connection_id.as_ref() else {
        // No Input means no hardware capture. Not "the selected device", not
        // "channels 1-2": arming and recording a track with no input connection
        // must never open a microphone the user did not choose.
        let _ = selected_device;
        return Err(format!(
            "{} has No Input. Assign an input audio connection before recording.",
            track.name
        ));
    };

    let Some(connection) = connections.get(connection_id) else {
        return Err(format!(
            "{} references an audio input connection that no longer exists.",
            track.name
        ));
    };
    if !connection.status.is_usable() {
        return Err(format!(
            "{} input connection \"{}\" is unavailable ({}).",
            track.name,
            connection.name,
            connection.status.label()
        ));
    }
    let Some(device_id) = connection.device_id.as_deref() else {
        return Err(format!(
            "{} input connection \"{}\" has no audio device assigned.",
            track.name, connection.name
        ));
    };

    // Ordered channel list — index 0 is Left. Never sorted.
    let channels: Vec<u32> = (0..connection.channel_layout.channel_count())
        .map(|logical| {
            connection
                .binding(logical)
                .map(|binding| binding.physical_port_id.port_index)
                .ok_or_else(|| {
                    format!(
                        "{} input connection \"{}\" is missing a port for {}.",
                        track.name,
                        connection.name,
                        connection.channel_layout.channel_label(logical)
                    )
                })
        })
        .collect::<Result<_, _>>()?;

    if channels.is_empty() {
        return Err(format!("{} has no input channels selected.", track.name));
    }
    match track.routing.audio_format {
        TrackAudioFormat::Mono if channels.len() != 1 => {
            return Err(format!(
                "{} is mono but \"{}\" is a stereo/multi input bus. Choose a mono connection.",
                track.name, connection.name
            ));
        }
        TrackAudioFormat::Stereo if !(1..=2).contains(&channels.len()) => {
            return Err(format!(
                "{} is stereo but \"{}\" has an incompatible channel count.",
                track.name, connection.name
            ));
        }
        _ => {}
    }

    let device = find_recording_input_device(devices, device_id)
        .ok_or_else(|| format!("{} input device is unavailable: {}", track.name, device_id))?;
    validate_channels(&track.name, device, &channels)?;
    Ok(RecordingInputRoute {
        device_id: Some(device.id.clone()),
        channels,
    })
}

fn validate_channels(
    track_name: &str,
    device: &RecordingInputDevice,
    channels: &[u32],
) -> Result<(), String> {
    for channel in channels {
        if *channel >= device.channels {
            return Err(format!(
                "{} input channel {} is unavailable on {}.",
                track_name,
                channel + 1,
                device.name
            ));
        }
    }
    Ok(())
}

fn select_recording_input_device<'a>(
    devices: &'a [RecordingInputDevice],
    wanted: &str,
) -> Option<&'a RecordingInputDevice> {
    if !wanted.trim().is_empty() {
        if let Some(device) = find_recording_input_device(devices, wanted) {
            return Some(device);
        }
    }
    devices
        .iter()
        .find(|device| device.is_default)
        .or_else(|| devices.first())
}

fn find_recording_input_device<'a>(
    devices: &'a [RecordingInputDevice],
    id_or_name: &str,
) -> Option<&'a RecordingInputDevice> {
    devices
        .iter()
        .find(|device| device.id == id_or_name || device.name == id_or_name)
}

fn resolve_input_device_id(engine: &DirectAudio::AudioEngine, device_name: &str) -> Option<String> {
    if device_name.trim().is_empty() {
        return None;
    }
    engine
        .list_input_devices()
        .into_iter()
        .find(|d| d.name == device_name || d.id == device_name)
        .map(|d| d.id)
        .or_else(|| Some(device_name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::{AudioConnectionRegistry, AvailablePorts, PhysicalInputChoice};
    use crate::components::timeline::timeline_state::TimelineState;

    fn input_device(channels: u32) -> RecordingInputDevice {
        RecordingInputDevice {
            id: "default-input".to_string(),
            name: "Default Input".to_string(),
            channels,
            is_default: true,
        }
    }

    fn ports(channels: u32) -> AvailablePorts {
        AvailablePorts::for_device("default-input", "Default Input", channels, 2)
    }

    /// Assign a track an audio input connection covering `channels`, going
    /// through the same bridge the Inspector uses.
    fn assign(
        timeline: &mut TimelineState,
        track_id: &str,
        channels: Vec<u32>,
        device_channels: u32,
    ) {
        let ports = ports(device_channels);
        let id = timeline
            .audio_connections
            .get_or_create_audio_connection_for_physical_input(
                &PhysicalInputChoice::Ports {
                    device_id: "default-input".to_string(),
                    channels,
                },
                &ports,
            );
        assert!(id.is_some());
        timeline.set_track_audio_input_connection(track_id, id);
    }

    /// No Input means no hardware capture. A fresh track may be armed, but
    /// recording it must report the missing assignment rather than quietly
    /// opening the default microphone.
    #[test]
    fn a_track_with_no_input_does_not_capture_the_default_device() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        let track = timeline
            .find_track(&track_id)
            .expect("new audio track should exist");
        assert!(
            track.routing.audio_input_connection_id.is_none(),
            "a fresh track starts with No Input"
        );

        let device = input_device(2);
        let error = recording_input_channels_checked(
            track,
            &AudioConnectionRegistry::new(),
            std::slice::from_ref(&device),
            Some(&device),
        )
        .expect_err("No Input must not resolve to a hardware route");
        assert!(
            error.contains("No Input"),
            "the reason must name the missing assignment: {error}"
        );
    }

    /// Audio recording resolves through the connection and nothing else: the
    /// track stores an id, the registry owns the hardware, and MIDI is not
    /// consulted at any point.
    #[test]
    fn the_recording_input_path_is_connection_only_and_leaves_midi_alone() {
        use crate::components::timeline::timeline_state::TrackMidiInputRouting;

        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        timeline.set_track_midi_input(
            &track_id,
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string(),
            },
        );
        assign(&mut timeline, &track_id, vec![2, 3], 4);

        let device = input_device(4);
        let track = timeline.find_track(&track_id).unwrap();
        let route = recording_input_channels_checked(
            track,
            &timeline.audio_connections,
            std::slice::from_ref(&device),
            Some(&device),
        )
        .expect("an assigned connection resolves");

        // The route came from the connection's bindings, in order.
        assert_eq!(route.device_id.as_deref(), Some("default-input"));
        assert_eq!(route.channels, vec![2, 3]);
        // The MIDI assignment is untouched by anything the audio path did.
        assert_eq!(
            timeline.find_track(&track_id).unwrap().routing.midi_input,
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string()
            }
        );
    }

    /// Clearing the audio input must not disturb MIDI, and vice versa.
    #[test]
    fn audio_and_midi_input_assignments_are_independent() {
        use crate::components::timeline::timeline_state::TrackMidiInputRouting;

        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        assign(&mut timeline, &track_id, vec![0], 2);
        timeline.set_track_midi_input(&track_id, TrackMidiInputRouting::AllInputs);

        timeline.set_track_audio_input_connection(&track_id, None);
        assert_eq!(
            timeline.find_track(&track_id).unwrap().routing.midi_input,
            TrackMidiInputRouting::AllInputs,
            "clearing the audio input must not touch MIDI routing"
        );

        assign(&mut timeline, &track_id, vec![1], 2);
        timeline.set_track_midi_input(&track_id, TrackMidiInputRouting::None);
        assert!(
            timeline
                .find_track(&track_id)
                .unwrap()
                .routing
                .audio_input_connection_id
                .is_some(),
            "clearing MIDI routing must not touch the audio connection"
        );
    }

    /// An unavailable connection records nothing and says why — it never falls
    /// back to another device.
    #[test]
    fn an_unavailable_connection_reports_rather_than_falling_back() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        assign(&mut timeline, &track_id, vec![0, 1], 2);
        timeline
            .audio_connections
            .revalidate(&crate::audio_connections::AvailablePorts::default());

        let device = input_device(2);
        let track = timeline.find_track(&track_id).unwrap();
        let error = recording_input_channels_checked(
            track,
            &timeline.audio_connections,
            std::slice::from_ref(&device),
            Some(&device),
        )
        .expect_err("an unavailable connection must not resolve");
        assert!(error.contains("unavailable"), "{error}");
    }

    /// The same rule at the engine boundary: an unassigned track publishes an
    /// empty source, so the callback captures nothing at all.
    #[test]
    fn no_input_publishes_an_empty_engine_source() {
        use crate::layout::engine_snapshot::build_engine_input_source;

        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        let track = timeline.find_track(&track_id).unwrap();
        let source = build_engine_input_source(track, &AudioConnectionRegistry::new(), &ports(2));
        assert_eq!(source.device_id, None);
        assert!(source.channels.is_empty());
    }

    #[test]
    fn recording_resolves_a_mono_connection_to_one_physical_channel() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        assign(&mut timeline, &track_id, vec![2], 4);

        let device = input_device(4);
        let track = timeline.find_track(&track_id).unwrap();
        let route = recording_input_channels_checked(
            track,
            &timeline.audio_connections,
            std::slice::from_ref(&device),
            Some(&device),
        )
        .expect("stereo tracks may record one input channel");
        assert_eq!(route.channels, vec![2]);
    }

    /// Ordered Left/Right must survive into the recorded route.
    #[test]
    fn recording_preserves_ordered_stereo_channels() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        assign(&mut timeline, &track_id, vec![1, 2], 4);

        let device = input_device(4);
        let track = timeline.find_track(&track_id).unwrap();
        let route = recording_input_channels_checked(
            track,
            &timeline.audio_connections,
            std::slice::from_ref(&device),
            Some(&device),
        )
        .expect("stereo tracks may record two input channels");
        assert_eq!(route.channels, vec![1, 2]);

        // And the reversed pair is a genuinely different route, not the same
        // connection with sorted channels.
        assign(&mut timeline, &track_id, vec![2, 1], 4);
        let track = timeline.find_track(&track_id).unwrap();
        let reversed = recording_input_channels_checked(
            track,
            &timeline.audio_connections,
            std::slice::from_ref(&device),
            Some(&device),
        )
        .expect("reversed pair still resolves");
        assert_eq!(reversed.channels, vec![2, 1]);
    }

    #[test]
    fn a_mono_track_rejects_a_stereo_connection() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        assign(&mut timeline, &track_id, vec![1, 2], 4);
        timeline.set_track_audio_format(&track_id, TrackAudioFormat::Mono);

        let device = input_device(4);
        let track = timeline.find_track(&track_id).unwrap();
        assert!(recording_input_channels_checked(
            track,
            &timeline.audio_connections,
            std::slice::from_ref(&device),
            Some(&device),
        )
        .is_err());
    }

    /// A connection whose device vanished must fail clearly rather than record
    /// from an unrelated input.
    #[test]
    fn a_disconnected_connection_fails_instead_of_recording_the_wrong_input() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        assign(&mut timeline, &track_id, vec![0, 1], 4);
        // Device disappears.
        timeline
            .audio_connections
            .revalidate(&AvailablePorts::default());

        let device = input_device(4);
        let track = timeline.find_track(&track_id).unwrap();
        let error = recording_input_channels_checked(
            track,
            &timeline.audio_connections,
            std::slice::from_ref(&device),
            Some(&device),
        )
        .expect_err("an unavailable connection must not silently fall back");
        assert!(error.contains("unavailable"));
    }

    /// MIDI routing is a separate field and is untouched by audio assignment.
    #[test]
    fn audio_and_midi_input_assignments_coexist_on_one_track() {
        use crate::components::timeline::timeline_state::TrackMidiInputRouting;

        let mut timeline = TimelineState::default();
        let track_id = timeline.create_audio_track();
        assign(&mut timeline, &track_id, vec![0], 4);
        timeline.set_track_midi_input(
            &track_id,
            TrackMidiInputRouting::MidiDevice {
                device_id: "MPK Mini".to_string(),
            },
        );

        let track = timeline.find_track(&track_id).unwrap();
        assert!(track.routing.audio_input_connection_id.is_some());
        assert_eq!(
            track.routing.midi_input,
            TrackMidiInputRouting::MidiDevice {
                device_id: "MPK Mini".to_string()
            }
        );
    }
}
