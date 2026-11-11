//! Sending commands to the audio thread, and the runtime a new stream starts from.

use super::*;

impl EngineInner {
    /// Block the *calling control thread* (never the callback) until the audio
    /// callback has drained every command sent before this call, or `timeout`
    /// elapses. Returns `true` on a confirmed ack. `false` means the barrier was
    /// not confirmed — no stream is open, the callback is stalled, or the device
    /// is paused — and the caller should fall back to its own grace handling.
    ///
    /// Used by offline export to confirm `SetPluginBridgeSink(None)` handoffs
    /// have reached the realtime graph before the export worker starts driving
    /// the shared bridge, replacing a guessed fixed sleep.
    pub fn wait_for_command_barrier(&self, timeout: std::time::Duration) -> bool {
        let ack = Arc::new(AtomicBool::new(false));
        if self
            .send_command(EngineCommand::CommandBarrier { ack: ack.clone() })
            .is_err()
        {
            return false;
        }
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if ack.load(Ordering::Acquire) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        ack.load(Ordering::Acquire)
    }

    /// The graph a new stream starts with. A clone of the cached graph keeps
    /// the sample rate its sample positions were computed at: the backend
    /// rescales clip durations, fades and warp segments from that rate to the
    /// device's with `retarget_sample_rate`, so relabelling it here (it used to
    /// be overwritten with `max(rate, 44100)`) made a switch away from a
    /// sub-44.1 kHz device scale from the wrong rate (#379).
    pub(super) fn get_initial_runtime(&self, sample_rate_override: Option<u32>) -> RuntimeProject {
        // Only a graph built here (or an empty one) takes this rate; the
        // 44.1 kHz floor is a default for before any device reported a rate.
        let sr = sample_rate_override
            .unwrap_or_else(|| self.shared.sample_rate.load(Ordering::Relaxed).max(44100));

        // Prefer the live runtime built by `load_project`. Rebuilding from the
        // snapshot here re-decodes media on the caller thread and has frozen
        // the native UI when Play re-opened the device after a sync.
        {
            let cached = self.runtime.lock().clone();
            if !cached.tracks.is_empty() {
                return cached;
            }
        }

        self.project
            .lock()
            .as_ref()
            .map(|snapshot| {
                let mut audio_cache = self.audio_cache.lock();
                match RuntimeProject::build(snapshot, sr, &mut audio_cache, None, self.pdc_enabled()) {
                    Ok(mut runtime) => {
                        runtime.resolve_jam_inputs(snapshot, &self.shared.jam_bus);
                            runtime.resolve_loopback_inputs(snapshot);
                        runtime
                    }
                    Err(e) => {
                        eprintln!(
                            "[SphereAudio] get_initial_runtime: invalid routing graph ({e}), keeping previous runtime"
                        );
                        // Has no tracks (a cached graph with tracks returned
                        // above), so no sample positions to keep a rate for.
                        let mut runtime = self.runtime.lock().clone();
                        runtime.sample_rate = sr;
                        runtime
                    }
                }
            })
            .unwrap_or_else(|| {
                let mut runtime = self.runtime.lock().clone();
                runtime.sample_rate = sr;
                runtime
            })
    }

    pub(super) fn commit_stream_open(
        &self,
        sr: u32,
        bs: u32,
        device_name: String,
        backend_name: String,
    ) {
        self.shared.sample_rate.store(sr, Ordering::Relaxed);
        let mut st = self.status.lock();
        st.stream_open = true;
        st.running = false;
        st.sample_rate = sr;
        st.buffer_size = bs;
        // Bridged-insert compensation follows the stream's period, not the
        // size of each callback; 0 lets the stream resolve it on its first
        // block, and a steady different size the device delivers replaces it.
        self.shared.set_stream_bridge_period(bs);
        st.output_device = Some(device_name);
        st.last_error = None;
        eprintln!("[audio-engine] active_sample_rate={sr} block_size={bs}");
        eprintln!("[DAUx] Stream committed: backend={backend_name} sr={sr} buf={bs}");
    }

    pub(super) fn cmd_sender(&self) -> Option<Sender<EngineCommand>> {
        if let Some(stream) = self.active_stream.lock().as_ref() {
            if let Some(tx) = stream.cmd_tx() {
                return Some(tx.clone());
            }
        }
        self.cmd_tx.lock().as_ref().cloned()
    }

    pub(super) fn send_command(&self, cmd: EngineCommand) -> Result<(), SphereAudioError> {
        if transport_freeze_debug_enabled() {
            let label = match &cmd {
                EngineCommand::LoadProject(_) => "LoadProject",
                EngineCommand::ReplaceMidi(_) => "ReplaceMidi",
                EngineCommand::SetTestTone { .. } => "SetTestTone",
                EngineCommand::SetMasterVolume { .. } => "SetMasterVolume",
                EngineCommand::SetTrackVolume { .. } => "SetTrackVolume",
                EngineCommand::SetTrackPan { .. } => "SetTrackPan",
                EngineCommand::SetTrackMute { .. } => "SetTrackMute",
                EngineCommand::SetTrackSolo { .. } => "SetTrackSolo",
                EngineCommand::SetTrackInputState { .. } => "SetTrackInputState",
                EngineCommand::SetTrackLoopbackPublish { .. } => "SetTrackLoopbackPublish",
                EngineCommand::SetTrackJamPublish { .. } => "SetTrackJamPublish",
                EngineCommand::SetJamMultitrackPairs { .. } => "SetJamMultitrackPairs",
                EngineCommand::SetRenderCapture(_) => "SetRenderCapture",
                EngineCommand::SetLoopbackRecord(_) => "SetLoopbackRecord",
                EngineCommand::SetTrackPreviewMode { .. } => "SetTrackPreviewMode",
                EngineCommand::SetSoundfontChannels { .. } => "SetSoundfontChannels",
                EngineCommand::SetInsertParam { .. } => "SetInsertParam",
                EngineCommand::SetMonitorSource { .. } => "SetMonitorSource",
                EngineCommand::SetMonitorControl { .. } => "SetMonitorControl",
                EngineCommand::SetMonitorOutput { .. } => "SetMonitorOutput",
                EngineCommand::SetListeningSimulation { .. } => "SetListeningSimulation",
                EngineCommand::SetHardwareOutputOwnership { .. } => "SetHardwareOutputOwnership",
                EngineCommand::SetTrackListen { .. } => "SetTrackListen",
                EngineCommand::SetTrackSpatial { .. } => "SetTrackSpatial",
                EngineCommand::SetClipGain { .. } => "SetClipGain",
                EngineCommand::SetTrackSendLevel { .. } => "SetTrackSendLevel",
                EngineCommand::ClearAllListen => "ClearAllListen",
                EngineCommand::MidiPreviewNoteOn { .. } => "MidiPreviewNoteOn",
                EngineCommand::MidiPreviewNoteOff { .. } => "MidiPreviewNoteOff",
                EngineCommand::MidiPreviewControlChange { .. } => "MidiPreviewControlChange",
                EngineCommand::MidiPreviewAllNotesOff { .. } => "MidiPreviewAllNotesOff",
                EngineCommand::PluginPreviewNoteOn { .. } => "PluginPreviewNoteOn",
                EngineCommand::PluginPreviewNoteOff { .. } => "PluginPreviewNoteOff",
                EngineCommand::PluginPreviewControlChange { .. } => "PluginPreviewControlChange",
                EngineCommand::PluginPreviewAllNotesOff { .. } => "PluginPreviewAllNotesOff",
                EngineCommand::PluginPreviewPitchBend { .. } => "PluginPreviewPitchBend",
                EngineCommand::StartTransport => "StartTransport",
                EngineCommand::StopTransport => "StopTransport",
                EngineCommand::Seek { .. } => "Seek",
                EngineCommand::SetMetronomeEnabled(_) => "SetMetronomeEnabled",
                EngineCommand::SetMetronomeSuspended(_) => "SetMetronomeSuspended",
                EngineCommand::SetMetronomeVoice { .. } => "SetMetronomeVoice",
                EngineCommand::StartCountIn { .. } => "StartCountIn",
                EngineCommand::CancelCountIn => "CancelCountIn",
                EngineCommand::SetBpm(_) => "SetBpm",
                EngineCommand::SetTempoMap(_) => "SetTempoMap",
                EngineCommand::SetTimeSignatureMap(_) => "SetTimeSignatureMap",
                EngineCommand::SetLoop { .. } => "SetLoop",
                EngineCommand::SetPluginBridgeSink { .. } => "SetPluginBridgeSink",
                EngineCommand::SetAraRenderers { .. } => "SetAraRenderers",
                EngineCommand::CommandBarrier { .. } => "CommandBarrier",
                EngineCommand::SetBridgeEditorActive(_) => "SetBridgeEditorActive",
                EngineCommand::StartAudition { .. } => "StartAudition",
                EngineCommand::StopAudition => "StopAudition",
            };
            eprintln!("[play-debug engine] send_command {label}");
        }
        let tx = self.cmd_sender().ok_or(SphereAudioError::EngineNotOpen)?;
        tx.try_send(cmd)
            .map_err(|e| SphereAudioError::NativeError(e.to_string()))
    }
}
