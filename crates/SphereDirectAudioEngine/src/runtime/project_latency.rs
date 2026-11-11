//! RuntimeProject: latency planning and plug-in delay compensation.

use super::*;

impl RuntimeProject {
    /// Plan latency and size the PDC rings on the control thread.
    ///
    /// `build` runs this as part of constructing a project; anything that
    /// assembles a `RuntimeProject` field by field has to run it before the
    /// audio thread can refresh the plan, since
    /// [`Self::refresh_runtime_latency_graph`] rewrites the plan in place and
    /// takes the ring capacity as already reserved.
    pub fn plan_latency_and_pdc(&mut self) {
        self.latency_graph =
            plan_runtime_latency_graph(&mut self.tracks, &self.audio_graph, self.pdc_enabled);
        self.ensure_pdc_delay_capacity();
    }

    /// Latency the track's own strip contributes, observed from the live
    /// processors (bridged inserts add the one-block handshake at the
    /// configured `bridge_period_frames` on top of what the host reports).
    ///
    /// Reads nothing but `track`, so the refresh path can call it while holding
    /// a mutable borrow of `self.tracks` — that is what lets the observed values
    /// be written straight back into each track instead of through a scratch
    /// `Vec` allocated on the audio thread.
    #[inline]
    pub(super) fn track_insert_latency_samples(
        track: &RuntimeTrack,
        bridge_period_frames: u32,
    ) -> u32 {
        // The built-in Soundfont Player sits ahead of the inserts, so its
        // decimation delay is part of the track's path either way.
        let instrument = track
            .soundfont_player
            .as_ref()
            .map(RuntimeSoundfontPlayer::latency_samples)
            .unwrap_or(0);
        if track.inserts.is_empty() {
            return track.plugin_latency_samples.max(instrument);
        }
        track.inserts.iter().fold(instrument, |samples, insert| {
            samples.saturating_add(Self::insert_latency_samples(insert, bridge_period_frames))
        })
    }

    /// Latency one insert adds to its track, or 0 while it is disabled.
    #[inline]
    pub(super) fn insert_latency_samples(insert: &RuntimeInsert, bridge_period_frames: u32) -> u32 {
        if !insert.enabled {
            return 0;
        }
        if insert.kind_tag == RuntimeInsertKind::ExternalBridge {
            return insert.bridge_sink.as_ref().map_or(0, |sink| {
                crate::plugin_bridge::bridged_insert_latency_samples(
                    sink.reported_latency_samples(),
                    bridge_period_frames,
                )
            });
        }
        insert
            .vst3
            .as_ref()
            .filter(|vst3| vst3.is_ready())
            .map_or(0, |vst3| vst3.get_latency_samples().max(0) as u32)
    }

    /// [`Self::track_insert_latency_samples`], also recording on every insert
    /// the latency up to and including it, which is where a multi-out
    /// instrument's child strips are fed from.
    pub(super) fn observe_track_latency_samples(
        track: &mut RuntimeTrack,
        bridge_period_frames: u32,
    ) -> u32 {
        let total = Self::track_insert_latency_samples(track, bridge_period_frames);
        let mut chain = track
            .soundfont_player
            .as_ref()
            .map(RuntimeSoundfontPlayer::latency_samples)
            .unwrap_or(0);
        for insert in &mut track.inserts {
            chain =
                chain.saturating_add(Self::insert_latency_samples(insert, bridge_period_frames));
            insert.chain_latency_samples = chain;
        }
        total
    }

    /// Turns delay compensation on or off in place, from the audio thread.
    ///
    /// Replans the existing latency graph and refits the rings inside the
    /// capacity the control thread reserved (path latencies are planned and
    /// reserved for with compensation off too), then clears them so no track
    /// replays audio delayed by the old plan. Realtime-safe.
    pub fn set_pdc_enabled_realtime(&mut self, enabled: bool) {
        self.pdc_enabled = enabled;
        recompute_runtime_latency_graph(
            &mut self.latency_graph,
            &self.tracks,
            &self.audio_graph,
            enabled,
        );
        self.fit_pdc_delay_capacity_realtime();
        self.reset_pdc_delay_lines();
    }

    /// Ring length one track needs to serve the planned compensation.
    #[inline]
    pub(super) fn pdc_buffer_frames(max_path_latency_samples: u32) -> usize {
        max_path_latency_samples.max(1) as usize + DEFAULT_AUDIO_BLOCK_CAPACITY
    }

    /// Fit one track's rings to `frames`. Allocates only when `frames` exceeds
    /// the vectors' capacity, so callers that have checked capacity first can
    /// use it on the audio thread.
    pub(super) fn fit_track_pdc_len(track: &mut RuntimeTrack, frames: usize) {
        Self::fit_pdc_ring_len(
            &mut track.pdc_delay_l,
            &mut track.pdc_delay_r,
            &mut track.pdc_write_pos,
            frames,
        );
    }

    /// [`Self::fit_track_pdc_len`] for any stereo ring (track or send).
    pub(super) fn fit_pdc_ring_len(
        l: &mut Vec<f32>,
        r: &mut Vec<f32>,
        write_pos: &mut usize,
        frames: usize,
    ) {
        let old_len = l.len();
        if old_len < frames {
            // Growing the ring invalidates the write cursor — reset so we
            // never read uninitialized slots. Same-size refreshes (common
            // when bridge latency reports tick) must leave the cursor alone
            // or mid-playback compensation clicks.
            l.resize(frames, 0.0);
            r.resize(frames, 0.0);
            *write_pos = 0;
        } else if old_len > frames {
            l.truncate(frames);
            r.truncate(frames);
            if frames > 0 {
                *write_pos %= frames;
            } else {
                *write_pos = 0;
            }
        }
    }

    /// Control-thread PDC ring sizing. Allocates.
    ///
    /// Capacity is reserved to the next power of two above what the plan needs
    /// while the length stays exact. Later growth — a bridged plugin reporting
    /// more latency mid-session — is then a length change inside memory this
    /// thread already reserved, which is what lets
    /// [`Self::fit_pdc_delay_capacity_realtime`] run without an allocator.
    pub(super) fn ensure_pdc_delay_capacity(&mut self) {
        let frames = Self::pdc_buffer_frames(self.latency_graph.max_path_latency_samples);
        let reserved = frames.next_power_of_two();
        for track in &mut self.tracks {
            let extra_l = reserved.saturating_sub(track.pdc_delay_l.len());
            let extra_r = reserved.saturating_sub(track.pdc_delay_r.len());
            track.pdc_delay_l.reserve_exact(extra_l);
            track.pdc_delay_r.reserve_exact(extra_r);
            Self::fit_track_pdc_len(track, frames);
            for send in &mut track.sends {
                let line = &mut send.pdc;
                line.l.reserve_exact(reserved.saturating_sub(line.l.len()));
                line.r.reserve_exact(reserved.saturating_sub(line.r.len()));
                Self::fit_pdc_ring_len(&mut line.l, &mut line.r, &mut line.write_pos, frames);
            }
        }
    }

    /// Realtime PDC ring sizing: fits the rings to a freshly recomputed plan
    /// without ever reaching the allocator.
    ///
    /// Returns `false` when the plan now needs more than the capacity the
    /// control thread reserved. The rings are then left as they are instead of
    /// grown; `apply_pdc_delay_block` passes a track whose ring cannot hold its
    /// delay through undelayed, so the cost is a track that is briefly
    /// uncompensated rather than an allocation inside the audio callback. The
    /// next graph swap reserves the larger buffer on the control thread.
    pub(super) fn fit_pdc_delay_capacity_realtime(&mut self) -> bool {
        let frames = Self::pdc_buffer_frames(self.latency_graph.max_path_latency_samples);
        let fits = self.tracks.iter().all(|track| {
            frames <= track.pdc_delay_l.capacity()
                && frames <= track.pdc_delay_r.capacity()
                && track
                    .sends
                    .iter()
                    .all(|send| frames <= send.pdc.l.capacity() && frames <= send.pdc.r.capacity())
        });
        if !fits {
            return false;
        }
        // Grow only. A ring's length is not its delay, so a shorter plan is
        // served as well by the longer ring, and keeping the length keeps every
        // sample at the position it was written to. Resizing it — as a bridged
        // plug-in's latency following the buffer size used to on every change —
        // rewound the cursor over stale audio, replayed at the wrong offset.
        for track in &mut self.tracks {
            if track.pdc_delay_l.len() < frames {
                Self::fit_track_pdc_len(track, frames);
                track.pdc_delay_l.fill(0.0);
                track.pdc_delay_r.fill(0.0);
            }
            for send in &mut track.sends {
                let line = &mut send.pdc;
                if line.l.len() < frames {
                    Self::fit_pdc_ring_len(&mut line.l, &mut line.r, &mut line.write_pos, frames);
                    line.l.fill(0.0);
                    line.r.fill(0.0);
                }
            }
        }
        true
    }

    /// Clear every track's PDC delay-line ring and rewind its write cursor.
    ///
    /// Realtime playback reuses one persistent `RuntimeProject`, so the PDC delay
    /// lines retain whatever audio they last held. On a transport (re)start or a
    /// seek that stale audio would be emitted for the first `max_path` samples of
    /// the compensated (lower-latency) tracks, desyncing them from the
    /// uncompensated / plugin-latency tracks at the very start of playback — the
    /// exact "audio vs VSTi track out of sync in realtime, fine in export"
    /// symptom. Offline export never hits this because it builds a *fresh*
    /// runtime (zeroed delay lines) and primes them with a warmup pre-roll. This
    /// gives realtime the same clean, settled start.
    ///
    /// Realtime-safe: only zero-fills preallocated buffers (no allocation, no
    /// locking). Called from the command drain on Start/Seek — never per block.
    pub fn reset_pdc_delay_lines(&mut self) {
        for track in &mut self.tracks {
            track.pdc_delay_l.fill(0.0);
            track.pdc_delay_r.fill(0.0);
            track.pdc_write_pos = 0;
            for send in &mut track.sends {
                send.pdc.l.fill(0.0);
                send.pdc.r.fill(0.0);
                send.pdc.write_pos = 0;
            }
        }
    }

    /// One-shot dump of the resolved latency-compensation graph used by the
    /// realtime callback (and, identically, by offline export). Gated by the
    /// caller behind `FUTUREBOARD_PDC_DEBUG`; prints from whatever thread invokes
    /// it (Start/Seek command drain), so it must stay flag-gated and one-shot —
    /// never a per-block log. Mirrors the `[export-latency]` dump so realtime and
    /// export compensation values can be compared field-by-field.
    pub fn dump_latency_compensation_graph(&self, context: &str) {
        let lg = &self.latency_graph;
        eprintln!(
            "[realtime-latency] context={context} pdc_enabled={} graph_max_latency_samples={} \
             master_insert_latency_samples={} tracks={}",
            self.pdc_enabled,
            lg.max_path_latency_samples,
            lg.master_plugin_latency,
            self.tracks.len(),
        );
        for (idx, track) in self.tracks.iter().enumerate() {
            eprintln!(
                "[realtime-latency] track={} track_type={} track_reported_latency_samples={} \
                 track_total_latency_samples={} track_compensation_delay_samples={} \
                 realtime_delay_line_size_samples={}",
                track.id,
                track.track_type,
                lg.track_plugin_latency.get(idx).copied().unwrap_or(0),
                lg.track_output_latency.get(idx).copied().unwrap_or(0),
                lg.track_pdc_delay.get(idx).copied().unwrap_or(0),
                track.pdc_delay_l.len(),
            );
        }
    }

    /// Refresh PDC planning when runtime-only bridge latency changes.
    ///
    /// Runs per block from the audio callback, so both halves are realtime-safe.
    /// The steady-state half scans a bounded slice of tracks and only reads
    /// atomics. The recompute half — reached whenever an observed latency differs
    /// from the active plan — rewrites the existing plan in place through
    /// [`recompute_runtime_latency_graph`] and refits the delay lines inside the
    /// capacity the control thread reserved.
    ///
    /// It used to rebuild instead: a `Vec` of observed values, a fresh plan
    /// behind `HashMap<String, usize>` with a cloned id per track, a delay-line
    /// resize, and two `std::env::var_os` calls — every one of them on the audio
    /// thread. Plugins that report a fixed latency never reached it, but a
    /// bridged plugin whose reported latency tracks something live (a network
    /// jitter buffer, say) re-entered it as often as its latency moved, and every
    /// visit was an allocation and an environment lock inside the callback.
    ///
    /// `bridge_period_frames` is the *configured* bridge period
    /// ([`Self::bridge_period_frames`]), never the frame count of the callback
    /// being rendered (#377). Charged at the callback size, every change in
    /// block size — WASAPI shared/event mode, variable-period drivers, partial
    /// renders — read as a bridged plug-in's latency changing: the plan was
    /// recomputed and every dry track's delay jumped with the block size. A
    /// replan now happens only when a plug-in reports a new latency, the
    /// configured period changes, or a graph swap brings new tracks.
    pub fn refresh_runtime_latency_graph(&mut self, bridge_period_frames: u32) -> bool {
        const TRACKS_PER_CALLBACK: usize = 64;
        let track_count = self.tracks.len();
        if track_count == 0 {
            return false;
        }
        let scan_count = track_count.min(TRACKS_PER_CALLBACK);
        let scan_start = self.audio_graph.latency_scan_cursor.min(track_count - 1);
        let mut changed = false;
        let mut scanned = 0usize;
        for offset in 0..scan_count {
            let idx = (scan_start + offset) % track_count;
            let observed =
                Self::track_insert_latency_samples(&self.tracks[idx], bridge_period_frames);
            changed = self
                .latency_graph
                .track_plugin_latency
                .get(idx)
                .copied()
                .unwrap_or(0)
                != observed;
            scanned += 1;
            if changed {
                break;
            }
        }
        self.audio_graph.latency_scan_cursor = (scan_start + scanned) % track_count;
        if !changed {
            return false;
        }

        for track in &mut self.tracks {
            track.plugin_latency_samples =
                Self::observe_track_latency_samples(track, bridge_period_frames);
        }
        recompute_runtime_latency_graph(
            &mut self.latency_graph,
            &self.tracks,
            &self.audio_graph,
            self.pdc_enabled,
        );
        self.latency_replan_count = self.latency_replan_count.wrapping_add(1);
        let refitted = self.fit_pdc_delay_capacity_realtime();

        if pdc_debug_enabled() {
            if !refitted {
                eprintln!(
                    "[pdc] delay-line capacity short for max_path={} — tracks stay uncompensated \
                     until the next graph swap",
                    self.latency_graph.max_path_latency_samples
                );
            }
            eprintln!(
                "[pdc] refreshed max_path={} pdc_enabled={}",
                self.latency_graph.max_path_latency_samples, self.pdc_enabled
            );
            for (idx, track) in self.tracks.iter().enumerate() {
                eprintln!(
                    "[pdc] track={} plugin={} output={} delay={}",
                    track.id,
                    self.latency_graph.track_plugin_latency[idx],
                    self.latency_graph.track_output_latency[idx],
                    self.latency_graph.track_pdc_delay[idx],
                );
            }
        }
        true
    }
}
