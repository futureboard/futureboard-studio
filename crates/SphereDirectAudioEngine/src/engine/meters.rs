//! Test tone, meters, audio diagnostics, recording preview and debug info.

use super::*;

impl EngineInner {
    pub fn set_test_tone(&self, enabled: bool, frequency: f32) {
        self.shared
            .test_tone_enabled
            .store(enabled, Ordering::Relaxed);
        self.shared
            .test_tone_freq
            .store(f32_store(frequency), Ordering::Relaxed);
        // Also send through command queue so the oscillator resets its freq.
        let _ = self.send_command(EngineCommand::SetTestTone { enabled, frequency });
    }

    pub fn get_meters(&self) -> JsMeterSnapshot {
        // `mut` is only exercised by the ASIO input-meter override below.
        #[cfg_attr(not(all(target_os = "windows", feature = "asio")), allow(unused_mut))]
        let mut tracks: Vec<_> = self
            .runtime
            .lock()
            .meter_snapshots()
            .into_iter()
            .map(|meter| JsTrackMeterSnapshot {
                track_id: meter.track_id,
                peak_l: meter.peak_l as f64,
                peak_r: meter.peak_r as f64,
                rms_l: meter.rms_l as f64,
                rms_r: meter.rms_r as f64,
            })
            .collect();

        #[cfg(all(target_os = "windows", feature = "asio"))]
        {
            let input_peaks = self
                .active_stream
                .lock()
                .as_ref()
                .and_then(|stream| stream.as_asio_duplex())
                .map(|handle| handle.take_input_channel_peaks());
            if let (Some(input_peaks), Some(project)) = (input_peaks, self.project.lock().as_ref())
            {
                for meter in &mut tracks {
                    let Some(track) = project.tracks.iter().find(|track| {
                        track.id == meter.track_id
                            && track.track_type == "audio"
                            && (track.armed || track.input_monitor)
                    }) else {
                        continue;
                    };
                    if let Some((peak_l, peak_r)) =
                        routed_input_peaks(&input_peaks, &track.input_source.channels)
                    {
                        meter.peak_l = peak_l as f64;
                        meter.peak_r = peak_r as f64;
                        meter.rms_l = 0.0;
                        meter.rms_r = 0.0;
                    }
                }
            }
        }
        let plugin_outputs = self
            .runtime
            .lock()
            .plugin_output_meter_snapshots()
            .into_iter()
            .map(|meter| JsPluginOutputMeterSnapshot {
                track_id: meter.track_id,
                insert_id: meter.insert_id,
                channel: meter.channel as u32,
                peak: meter.peak as f64,
            })
            .collect();

        JsMeterSnapshot {
            tracks,
            plugin_outputs,
            master_peak_l: f32_load(self.shared.peak_l.load(Ordering::Relaxed)) as f64,
            master_peak_r: f32_load(self.shared.peak_r.load(Ordering::Relaxed)) as f64,
            master_rms_l: f32_load(self.shared.rms_l.load(Ordering::Relaxed)) as f64,
            master_rms_r: f32_load(self.shared.rms_r.load(Ordering::Relaxed)) as f64,
            input_peak_l: f32_load(self.shared.live_input_peak_l.swap(0, Ordering::Relaxed)) as f64,
            input_peak_r: f32_load(self.shared.live_input_peak_r.swap(0, Ordering::Relaxed)) as f64,
            monitor_active: self.shared.monitor_enabled_any.load(Ordering::Relaxed),
            monitor_peak_l: f32_load(self.shared.monitor_peak_l.load(Ordering::Relaxed)) as f64,
            monitor_peak_r: f32_load(self.shared.monitor_peak_r.load(Ordering::Relaxed)) as f64,
        }
    }

    /// Full-pipeline diagnostics snapshot (Layer 10). Non-destructive — reads
    /// peaks without resetting them so it can be polled alongside `get_meters`.
    pub fn get_audio_diagnostics(&self) -> crate::types::JsAudioDiagnostics {
        use crate::types::{JsAudioDiagnostics, JsTrackInputDiagnostics};
        let st = self.status.lock().clone();
        let input_device_name = self
            .live_input
            .lock()
            .as_ref()
            .and_then(|h| h.device_id.clone());
        let input_running = self.live_input.lock().is_some() && self.shared.input_ring.is_active();

        let raw_input_peak = f32_load(self.shared.live_input_peak_l.load(Ordering::Relaxed)).max(
            f32_load(self.shared.live_input_peak_r.load(Ordering::Relaxed)),
        );
        let input_bus_peak = f32_load(self.shared.input_bus_peak_l.load(Ordering::Relaxed)).max(
            f32_load(self.shared.input_bus_peak_r.load(Ordering::Relaxed)),
        );
        let output_peak = f32_load(self.shared.peak_l.load(Ordering::Relaxed))
            .max(f32_load(self.shared.peak_r.load(Ordering::Relaxed)));

        let runtime = self.runtime.lock();
        let tracks = runtime
            .tracks
            .iter()
            .filter(|t| t.track_type == "audio")
            .map(|t| {
                let meter = t.meter.load(&t.id);
                JsTrackInputDiagnostics {
                    track_id: t.id.clone(),
                    record_armed: t.record_armed,
                    monitor_enabled: t.monitor_enabled,
                    input_source: format!("{:?}", t.input_source),
                    track_input_peak: meter.peak_l.max(meter.peak_r) as f64,
                    track_output_peak: meter.peak_l.max(meter.peak_r) as f64,
                }
            })
            .collect();

        JsAudioDiagnostics {
            backend: self.daux_config.lock().backend.display_name().to_string(),
            input_device_name,
            output_device_name: st.output_device,
            input_stream_running: input_running,
            output_stream_running: st.running,
            input_sample_rate: self.shared.input_ring.sample_rate(),
            input_channels: self.shared.input_ring.channels(),
            raw_input_peak: raw_input_peak as f64,
            input_bus_peak: input_bus_peak as f64,
            output_peak: output_peak as f64,
            tracks,
            input_callback_count: self.shared.input_cb_count.load(Ordering::Relaxed) as f64,
            output_callback_count: self.shared.output_cb_count.load(Ordering::Relaxed) as f64,
            input_frames_received: self.shared.input_frames_received.load(Ordering::Relaxed) as f64,
            monitor_frames_consumed: self.shared.monitor_frames_consumed.load(Ordering::Relaxed)
                as f64,
            monitor_ring_underruns: self.shared.monitor_ring_underruns.load(Ordering::Relaxed)
                as f64,
            monitor_ring_overruns: self.shared.monitor_ring_overruns.load(Ordering::Relaxed) as f64,
            record_ring_overruns: self.shared.record_ring_overruns.load(Ordering::Relaxed) as f64,
            output_xruns: self.shared.output_xruns.load(Ordering::Relaxed) as f64,
            monitor_output_peak: f32_load(self.shared.monitor_output_peak.load(Ordering::Relaxed))
                as f64,
            record_peak: f32_load(self.shared.record_peak.load(Ordering::Relaxed)) as f64,
        }
    }

    /// Metadata + current bin count for the in-progress recording preview.
    pub fn recording_preview_info(&self) -> crate::types::JsRecordingPreviewInfo {
        crate::types::JsRecordingPreviewInfo {
            active: self.shared.recording_preview_active.load(Ordering::Relaxed),
            recording_id: self.shared.recording_preview_id.load(Ordering::Relaxed) as f64,
            start_sample: self
                .shared
                .recording_preview_start_sample
                .load(Ordering::Relaxed) as f64,
            sample_rate: self
                .shared
                .recording_preview_sample_rate
                .load(Ordering::Relaxed),
            channels: self
                .shared
                .recording_preview_channels
                .load(Ordering::Relaxed),
            peaks_per_second: self
                .shared
                .recording_preview_peaks_per_sec
                .load(Ordering::Relaxed),
            peak_count: self.shared.preview_ring.head() as f64,
        }
    }

    /// Drain preview bins in `[from_index, head)`. Cheap clone on the control
    /// thread; never blocks the audio path. Returns an empty vec when there is
    /// nothing new.
    pub fn drain_recording_preview_peaks(
        &self,
        from_index: f64,
    ) -> Vec<crate::types::JsWaveformPeak> {
        drain_preview_ring(&self.shared.preview_ring, from_index)
    }

    /// Per-track metadata for every armed track currently feeding its own
    /// live preview ring (Part 1, multi-track). Replaces
    /// [`Self::recording_preview_info`] for takes with more than one armed
    /// audio track — the UI iterates this instead of guessing one track.
    pub fn recording_preview_tracks(&self) -> Vec<crate::types::JsRecordingPreviewTrackInfo> {
        if !self.shared.recording_preview_active.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let ids = self.shared.recording_preview_track_ids.lock();
        ids.iter()
            .enumerate()
            .map(
                |(slot, track_id)| crate::types::JsRecordingPreviewTrackInfo {
                    track_id: track_id.clone(),
                    recording_id: self.shared.recording_preview_id.load(Ordering::Relaxed) as f64,
                    start_sample: self
                        .shared
                        .recording_preview_start_sample
                        .load(Ordering::Relaxed) as f64,
                    sample_rate: self
                        .shared
                        .recording_preview_sample_rate
                        .load(Ordering::Relaxed),
                    peaks_per_second: self
                        .shared
                        .recording_preview_peaks_per_sec
                        .load(Ordering::Relaxed),
                    peak_count: self.shared.preview_rings[slot].head() as f64,
                },
            )
            .collect()
    }

    /// Drain one track's preview bins in `[from_index, head)`. Returns an
    /// empty vec when `track_id` has no active preview slot (untracked, or
    /// the take ended) or there is nothing new.
    pub fn drain_recording_preview_peaks_for_track(
        &self,
        track_id: &str,
        from_index: f64,
    ) -> Vec<crate::types::JsWaveformPeak> {
        let slot = self
            .shared
            .recording_preview_track_ids
            .lock()
            .iter()
            .position(|id| id == track_id);
        let Some(slot) = slot else {
            return Vec::new();
        };
        drain_preview_ring(&self.shared.preview_rings[slot], from_index)
    }

    /// Control thread: log what the audio callback published since the last
    /// call — the latest slow block (the callback no longer prints it), and
    /// any instrument MIDI block list the callback had to grow. A few atomic
    /// loads when there is nothing new; call it from a poll loop.
    pub fn log_callback_diagnostics(&self) {
        if let Some((block, reports)) = self.shared.slow_block_report.take() {
            eprintln!(
                "[AudioCallback] slow block severity={} duration_us={} deadline_us={} load={}% state={} frames={} reports_since_last_log={reports}",
                block.severity(),
                block.elapsed_us,
                block.deadline_us,
                block.load_pct(),
                block.state.as_str(),
                block.frames,
            );
        }
        static MIDI_GROWTHS_LOGGED: AtomicU64 = AtomicU64::new(0);
        let growths = crate::runtime::midi_block_list_growths();
        let logged = MIDI_GROWTHS_LOGGED.swap(growths, Ordering::Relaxed);
        if growths != logged {
            eprintln!(
                "[AudioCallback] the callback grew an instrument MIDI block list {} time(s) (total {growths}): a graph or schedule reached it without its list sized off the audio thread",
                growths.wrapping_sub(logged)
            );
        }
    }

    /// Throttled (≈500 ms) raw-input-peak trace, gated by
    /// `FUTUREBOARD_INPUT_DEBUG`. Called from the control thread (never the
    /// audio callback) so the realtime path stays allocation/IO-free.
    pub fn log_input_debug_throttled(&self) {
        if !input_debug_enabled() {
            return;
        }
        static LAST: OnceLock<Mutex<std::time::Instant>> = OnceLock::new();
        let last = LAST.get_or_init(|| {
            Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(1))
        });
        {
            let mut guard = last.lock();
            if guard.elapsed() < std::time::Duration::from_millis(500) {
                return;
            }
            *guard = std::time::Instant::now();
        }
        let s = &self.shared;
        let raw = f32_load(s.live_input_peak_l.load(Ordering::Relaxed))
            .max(f32_load(s.live_input_peak_r.load(Ordering::Relaxed)));
        let bus = f32_load(s.input_bus_peak_l.load(Ordering::Relaxed))
            .max(f32_load(s.input_bus_peak_r.load(Ordering::Relaxed)));
        let head = s.preview_ring.head();
        eprintln!(
            "[AudioRealtime] input_cb={} output_cb={} input_frames={} monitor_consumed={} \
             monitor_underruns={} monitor_overruns={} record_overruns={} output_xruns={} \
             raw_input_peak={raw:.4} monitor_output_peak={:.4} record_peak={:.4} input_stream={} monitor_any={}",
            s.input_cb_count.load(Ordering::Relaxed),
            s.output_cb_count.load(Ordering::Relaxed),
            s.input_frames_received.load(Ordering::Relaxed),
            s.monitor_frames_consumed.load(Ordering::Relaxed),
            s.monitor_ring_underruns.load(Ordering::Relaxed),
            s.monitor_ring_overruns.load(Ordering::Relaxed),
            s.record_ring_overruns.load(Ordering::Relaxed),
            s.output_xruns.load(Ordering::Relaxed),
            f32_load(s.monitor_output_peak.load(Ordering::Relaxed)),
            f32_load(s.record_peak.load(Ordering::Relaxed)),
            s.input_ring.is_active(),
            s.monitor_enabled_any.load(Ordering::Relaxed),
        );
        let _ = bus;
        if s.recording_preview_active.load(Ordering::Relaxed) {
            let sr = s
                .recording_preview_sample_rate
                .load(Ordering::Relaxed)
                .max(1);
            let pps = s
                .recording_preview_peaks_per_sec
                .load(Ordering::Relaxed)
                .max(1);
            eprintln!(
                "[RecordingPreview] recording_id={} peaks={} preview_duration_sec={:.2} sr={sr} pps={pps}",
                s.recording_preview_id.load(Ordering::Relaxed),
                head,
                head as f64 / pps as f64,
            );
        }
    }

    pub fn get_debug_info(&self) -> JsEngineDebugInfo {
        // `project` before `runtime`, the order `load_project` takes them in.
        let project = self.project.lock();
        let runtime = self.runtime.lock();
        let sample_rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1);

        let ready_clips = runtime
            .clips
            .iter()
            .filter(|c| c.source.frames() > 0)
            .count() as u32;
        let clip_summaries: Vec<String> = runtime
            .clips
            .iter()
            .map(|c| {
                format!(
                    "id={} track={} start={:.3}s dur={:.3}s frames={} gain={:.2}",
                    c.id,
                    c.track_id,
                    c.start_sample as f64 / sample_rate as f64,
                    c.duration_samples as f64 / sample_rate as f64,
                    c.source.frames(),
                    c.gain,
                )
            })
            .collect();
        let insert_summaries: Vec<String> = runtime
            .tracks
            .iter()
            .flat_map(|track| {
                track.inserts.iter().map(move |insert| {
                    let format = insert
                        .params
                        .get("format")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let path = insert
                        .params
                        .get("path")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let vst3_ready = insert
                        .vst3
                        .as_ref()
                        .map(|processor| processor.is_ready())
                        .unwrap_or(false);
                    let (process_count, input_peak, output_peak, diff_peak) = insert
                        .vst3
                        .as_ref()
                        .map(|processor| {
                            (
                                processor.process_count(),
                                processor.last_input_peak(),
                                processor.last_output_peak(),
                                processor.last_difference_peak(),
                            )
                        })
                        .unwrap_or((0, 0.0, 0.0, 0.0));
                    format!(
                        concat!(
                            "track={} insert={} kind={} format={} enabled={}",
                            "vst3Ready={} processCount={}",
                            "inPeak={:.4} outPeak={:.4} diffPeak={:.6} path={}"
                        ),
                        track.id,
                        insert.id,
                        insert.kind,
                        format,
                        insert.enabled,
                        vst3_ready,
                        process_count,
                        input_peak,
                        output_peak,
                        diff_peak,
                        path,
                    )
                })
            })
            .collect();

        let graph_rejected_route_summaries: Vec<String> = runtime
            .audio_graph
            .rejected_routes
            .iter()
            .map(|route| {
                format!(
                    "{} -> {} ({:?}): {}",
                    route.from_track_id, route.to_track_id, route.kind, route.reason
                )
            })
            .collect();

        let transport = self.transport_snapshot();
        let disk = crate::streaming_source::diagnostics();

        JsEngineDebugInfo {
            project_id: project.as_ref().map(|p| p.project_id.clone()),
            loaded_tracks: runtime.tracks.len() as u32,
            loaded_clips: runtime.clips.len() as u32,
            ready_clips,
            is_playing: transport.playing,
            position_seconds: transport.position_seconds,
            position_beats: transport.position_beats,
            loop_enabled: transport.loop_enabled,
            has_solo: runtime.has_solo,
            clip_summaries,
            insert_summaries,
            disk_underruns: disk.underruns as f64,
            disk_stream_active_sources: disk.active_sources as f64,
            disk_stream_cache_reads: disk.cache_reads as f64,
            disk_stream_cache_hits: disk.cache_hits as f64,
            disk_stream_cache_misses: disk.cache_misses as f64,
            disk_stream_cache_memory_used_mb: disk.cache_memory_used_bytes as f64
                / (1024.0 * 1024.0),
            disk_stream_cache_memory_budget_mb: disk.cache_memory_budget_bytes as f64
                / (1024.0 * 1024.0),
            disk_stream_blocks_decoded: disk.blocks_decoded as f64,
            disk_stream_frames_decoded: disk.frames_decoded as f64,
            graph_node_count: runtime.audio_graph.nodes.len() as u32,
            graph_pass1_count: runtime.audio_graph.pass1_source_indices.len() as u32,
            graph_pass2_count: runtime.audio_graph.pass2_routing_indices.len() as u32,
            graph_rejected_route_count: runtime.audio_graph.rejected_routes.len() as u32,
            graph_rejected_route_summaries,
        }
    }
}
