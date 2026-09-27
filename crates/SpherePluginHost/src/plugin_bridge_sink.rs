//! Engine-facing realtime sink (Stage 3b): implements DirectAudio's
//! [`PluginBridgeSink`] over the shared-memory [`SharedAudioRegion`].
//!
//! The main app's audio callback (in `DirectAudio`) holds an `Arc<dyn PluginBridgeSink>`
//! and calls these methods per block to read the host's produced output and
//! request the next one. Methods only touch the lock-free shared region
//! (atomics + raw buffer copies), never allocate or lock. Output reads use a
//! handoff grace bounded by a fraction of the *current* block period to absorb
//! cross-process scheduler jitter without spending the device callback's own
//! budget — see [`output_handoff_grace`].

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use DirectAudio::plugin_bridge::{PluginBridgeSink, SharedPluginBridgeSink};

use crate::audio_bridge::{
    BridgeKickEvent, SharedAudioRegion, SharedMidiEvent, MAX_BLOCK_FRAMES, MAX_CHANNELS,
};

#[inline]
fn f32_store(v: f32) -> u32 {
    v.to_bits()
}

#[inline]
fn f32_load(v: u32) -> f32 {
    f32::from_bits(v)
}

/// `FUTUREBOARD_PLUGIN_BRIDGE_DEBUG=1` enables the ring-full drop traces.
/// These pushes run on the engine's audio callback, so stderr is debug-only;
/// drops always count into the shared region's `xrun_count` either way.
fn bridge_debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_BRIDGE_DEBUG").is_some())
}

/// Wraps the engine-side shared audio region as a realtime [`PluginBridgeSink`].
pub struct SharedRegionSink {
    region: Arc<SharedAudioRegion>,
    /// Signaled after every `request_seq` bump so the host's audio producer
    /// wakes immediately instead of polling on a timer tick. `None` falls back
    /// to the host's timeout-paced poll (tests / event creation failure).
    kick: Option<Arc<BridgeKickEvent>>,
    /// `done_seq` of the last block this sink actually returned from
    /// [`PluginBridgeSink::read_output`]. Freshness guard: when the host has
    /// not produced a new block since the last read (its service thread is
    /// stalled behind an editor open/close or a plugin load), `read_output`
    /// returns 0 so the engine bypasses/silences the block instead of
    /// replaying the stale `audio_out` contents every callback.
    last_read_seq: AtomicU64,
    output_channel_peaks: [AtomicU32; MAX_CHANNELS],
    /// Cleared by the studio when the host process serving this region has
    /// gone away. Its `done_seq` will never move again, so waiting for it is
    /// pure loss: with several of its inserts in the graph, every callback
    /// spent its whole period spinning and the entire mix broke up. `None`
    /// (tests, older callers) always waits.
    host_alive: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Share of one block period the sink may spend waiting for a block the host
/// has not published yet.
///
/// Cross-process scheduling can publish the previous block a few microseconds
/// after the device callback begins, and a single non-blocking probe turned
/// that harmless jitter into a full block of silence. The wait that absorbs it
/// runs *on the device callback*, though, so it spends the same budget the rest
/// of the mix has to fit into — and it is paid per bridged instance.
///
/// This used to be a flat 1 ms, justified against a 256-frame / 48 kHz period
/// (5.33 ms). That justification does not survive the buffer sizes people
/// actually play instruments at: 1 ms is 37% of a 128-frame period and 75% of a
/// 64-frame one, so two bridged instruments stalling in the same callback
/// overrun the period outright. A plug-in stall then stops being one silent
/// plug-in block and becomes a device xrun that cuts the whole mix — and the
/// spin actively works against itself, burning the core the host's producer
/// thread needs to finish that very block.
///
/// Deriving it from the live block period keeps the original intent ("well
/// below the period") true at every buffer size instead of only at 256+.
const OUTPUT_HANDOFF_GRACE_DIVISOR: u32 = 8;

/// Ceiling for the grace, so a large buffer does not turn an eighth of its
/// period into many milliseconds of spinning.
const OUTPUT_HANDOFF_GRACE_MAX: Duration = Duration::from_millis(1);

/// Grace for one block at `frames` / `sample_rate`. Pure, so the bound can be
/// checked without a device or a second process.
#[inline]
fn output_handoff_grace(frames: u32, sample_rate: u32) -> Duration {
    if frames == 0 || sample_rate == 0 {
        // No published block geometry yet — nothing to size a wait against.
        return Duration::ZERO;
    }
    let period_nanos = (frames as u64).saturating_mul(1_000_000_000) / sample_rate as u64;
    Duration::from_nanos(period_nanos / OUTPUT_HANDOFF_GRACE_DIVISOR as u64)
        .min(OUTPUT_HANDOFF_GRACE_MAX)
}

impl std::fmt::Debug for SharedRegionSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRegionSink")
            .field("bytes", &self.region.bytes())
            .finish()
    }
}

impl SharedRegionSink {
    pub fn new(region: Arc<SharedAudioRegion>) -> Self {
        Self::with_kick(region, None)
    }

    pub fn with_kick(region: Arc<SharedAudioRegion>, kick: Option<Arc<BridgeKickEvent>>) -> Self {
        Self {
            region,
            kick,
            last_read_seq: AtomicU64::new(0),
            output_channel_peaks: std::array::from_fn(|_| AtomicU32::new(0)),
            host_alive: None,
        }
    }

    /// Wrap as the trait object the engine holds.
    pub fn into_shared(
        region: Arc<SharedAudioRegion>,
        kick: Option<Arc<BridgeKickEvent>>,
    ) -> SharedPluginBridgeSink {
        Arc::new(Self::with_kick(region, kick))
    }

    /// [`Self::into_shared`] for a sink that stops waiting on its host once
    /// `host_alive` is cleared.
    pub fn into_shared_with_liveness(
        region: Arc<SharedAudioRegion>,
        kick: Option<Arc<BridgeKickEvent>>,
        host_alive: Arc<std::sync::atomic::AtomicBool>,
    ) -> SharedPluginBridgeSink {
        let mut sink = Self::with_kick(region, kick);
        sink.host_alive = Some(host_alive);
        Arc::new(sink)
    }

    #[inline]
    fn fresh_done_seq(&self) -> Option<u64> {
        let bridge = self.region.bridge();
        let last = self.last_read_seq.load(Ordering::Relaxed);
        let mut done = bridge.done_seq.load(Ordering::Acquire);

        // Request 0 means no host block has ever been kicked (startup), so
        // there is nothing useful to wait for yet.
        let host_alive = self
            .host_alive
            .as_ref()
            .is_none_or(|alive| alive.load(Ordering::Relaxed));
        if host_alive && done == last && bridge.request_seq.load(Ordering::Acquire) != 0 {
            let grace = output_handoff_grace(
                bridge.block_frames.load(Ordering::Relaxed),
                bridge.sample_rate.load(Ordering::Relaxed),
            );
            if !grace.is_zero() {
                let deadline = Instant::now() + grace;
                while done == last && Instant::now() < deadline {
                    std::hint::spin_loop();
                    done = bridge.done_seq.load(Ordering::Acquire);
                }
            }
        }

        if done == last {
            bridge.xrun_count.fetch_add(1, Ordering::Relaxed);
            None
        } else {
            self.last_read_seq.store(done, Ordering::Relaxed);
            Some(done)
        }
    }
}

impl PluginBridgeSink for SharedRegionSink {
    fn dsp_ready(&self) -> bool {
        self.region.bridge().dsp_output_ready()
    }

    fn plugin_output_channels(&self) -> u32 {
        self.region.bridge().plugin_output_channels()
    }

    fn request_in_flight(&self) -> bool {
        let bridge = self.region.bridge();
        bridge.request_seq.load(Ordering::Acquire) != bridge.done_seq.load(Ordering::Acquire)
    }

    fn read_output(&self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) -> usize {
        self.read_output_for_channels(out_l, out_r, frames, &[])
    }

    fn read_output_for_channels(
        &self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        frames: usize,
        enabled_channels: &[u8],
    ) -> usize {
        // The bare main pair `[1, 2]` is the forced default the UI can never
        // deselect. It means "no explicit multi-out routing", not "main bus
        // only". Some multi-output instruments move audio from the main bus to
        // aux buses, so folding only `[1, 2]` can be silent. Treat the bare
        // default like an empty selection and fold every reported channel. An
        // explicit selection that adds aux channels is `!= [1, 2]` and is still
        // honored.
        let enabled_channels: &[u8] = if matches!(enabled_channels, [1, 2]) {
            &[]
        } else {
            enabled_channels
        };
        let bridge = self.region.bridge();
        // Freshness guard: only hand a produced block to the engine once. The
        // bounded handoff absorbs scheduler jitter; a genuinely stalled host
        // still returns 0 and is bypassed/silenced. Never replay stale output.
        if self.fresh_done_seq().is_none() {
            return 0;
        }
        let output_channels = bridge
            .plugin_output_channels()
            .min(crate::audio_bridge::MAX_CHANNELS as u32) as usize;
        let mut peaks = [0.0f32; MAX_CHANNELS];
        // SAFETY: the engine consumes `audio_out` while it holds the block (the
        // host waits on `done_seq` before producing the next one).
        let read = unsafe {
            bridge
                .audio_out
                .read_downmixed_to_stereo_selected_with_peaks(
                    out_l,
                    out_r,
                    frames,
                    output_channels,
                    enabled_channels,
                    &mut peaks,
                )
        };
        for (index, peak) in peaks.into_iter().enumerate() {
            let peak = if index < output_channels { peak } else { 0.0 };
            self.output_channel_peaks[index].store(f32_store(peak), Ordering::Relaxed);
        }
        read
    }

    fn read_output_multichannel(
        &self,
        out_interleaved: &mut [f32],
        frames: usize,
    ) -> (usize, usize) {
        let bridge = self.region.bridge();
        // Same freshness guard as `read_output_for_channels`: hand a produced
        // block to the engine exactly once. The caller folds the main pair and
        // scatters child pairs from this single read.
        if self.fresh_done_seq().is_none() {
            return (0, 0);
        }
        let channels = bridge.plugin_output_channels().min(MAX_CHANNELS as u32) as usize;
        let frames = frames.min(crate::audio_bridge::MAX_BLOCK_FRAMES);
        let len = (frames * channels).min(out_interleaved.len());
        // SAFETY: the engine owns `audio_out` while it holds the block.
        unsafe {
            bridge.audio_out.read_interleaved(out_interleaved, len);
        }
        // Refresh per-channel meter peaks for the same fresh block so the mixer
        // sub-strips stay live (the fold read path is skipped in multi-out mode).
        for ch in 0..channels.min(MAX_CHANNELS) {
            let mut peak = 0.0f32;
            let mut i = ch;
            while i < len {
                peak = peak.max(out_interleaved[i].abs());
                i += channels;
            }
            self.output_channel_peaks[ch].store(f32_store(peak), Ordering::Relaxed);
        }
        for ch in channels..MAX_CHANNELS {
            self.output_channel_peaks[ch].store(f32_store(0.0), Ordering::Relaxed);
        }
        (frames, channels)
    }

    fn output_channel_peak(&self, channel: u8) -> f32 {
        let index = channel.saturating_sub(1) as usize;
        self.output_channel_peaks
            .get(index)
            .map(|peak| f32_load(peak.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    fn push_midi(&self, status: u8, data1: u8, data2: u8, sample_offset: u32) {
        let bridge = self.region.bridge();
        let ok = bridge.midi.try_push(SharedMidiEvent {
            sample_offset,
            status,
            data1,
            data2,
            _pad: 0,
        });
        if !ok {
            bridge.xrun_count.fetch_add(1, Ordering::Relaxed);
            if bridge_debug_enabled() {
                eprintln!(
                    "[plugin-dsp-midi] ring_full dropped status=0x{status:02X} pitch={data1} velocity={data2}"
                );
            }
        }
    }

    fn push_param(&self, param_id: u32, value: f32, sample_offset: u32) {
        let bridge = self.region.bridge();
        let ok = bridge
            .params
            .try_push(crate::audio_bridge::SharedParamEvent {
                sample_offset,
                param_id,
                value,
                _pad: 0,
            });
        if !ok {
            bridge.xrun_count.fetch_add(1, Ordering::Relaxed);
            if bridge_debug_enabled() {
                eprintln!("[plugin-param] ring_full dropped param_id={param_id} value={value:.4}");
            }
        }
    }

    fn write_input(&self, in_l: &[f32], in_r: &[f32], frames: usize) {
        // SAFETY: the engine owns `audio_in` only after the host published
        // `done_seq` for the previous request (consumed by `read_output` above
        // in `apply_external_bridge_insert_block`) and before this side bumps
        // `request_seq` again.
        unsafe {
            self.region
                .bridge()
                .audio_in
                .write_deinterleaved(in_l, in_r, frames);
        }
    }

    fn reported_latency_samples(&self) -> u32 {
        self.region
            .bridge()
            .latency_samples
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn reported_process_load(&self) -> Option<f32> {
        use std::sync::atomic::Ordering;
        let bridge = self.region.bridge();
        let micros = bridge.last_process_micros.load(Ordering::Relaxed);
        let frames = bridge.block_frames.load(Ordering::Relaxed);
        let sample_rate = bridge.sample_rate.load(Ordering::Relaxed);
        if frames == 0 || sample_rate == 0 {
            return None;
        }
        // The block's own wall-clock budget, not a fixed millisecond count: the
        // same duration means very different things at 64 frames and at 1024.
        let deadline_micros = frames as f64 * 1_000_000.0 / sample_rate as f64;
        if deadline_micros <= 0.0 {
            return None;
        }
        Some((micros as f64 / deadline_micros) as f32)
    }

    fn set_transport(&self, ctx: &DirectAudio::vst3_processor::RuntimeTransportContext) {
        self.region
            .bridge()
            .store_transport(&crate::audio_bridge::BridgeTransport {
                tempo_bpm: ctx.tempo_bpm,
                time_sig_num: ctx.time_sig_num,
                time_sig_den: ctx.time_sig_den,
                project_time_samples: ctx.project_time_samples,
                ppq_position: ctx.ppq_position,
                bar_position_ppq: ctx.bar_position_ppq,
                playing: ctx.playing,
                recording: ctx.recording,
            });
    }

    fn request_block(&self, frames: u32) {
        let bridge = self.region.bridge();
        bridge
            .block_frames
            .store(frames.min(MAX_BLOCK_FRAMES as u32), Ordering::Relaxed);
        bridge.request_seq.fetch_add(1, Ordering::Release);
        // Wake the host producer for this request. `SetEvent` never blocks —
        // it is the same class of kernel signal the OS itself uses to drive
        // the WASAPI period callback — so the audio thread stays wait-free.
        // Ordering: signaled strictly after the `request_seq` Release bump so
        // a woken producer always observes the new request.
        if let Some(kick) = &self.kick {
            kick.set();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_bridge::SharedAudioRegion;

    /// The wait runs on the device callback, so it must stay a small slice of
    /// the block period at *every* buffer size — a flat millisecond was 75% of
    /// a 64-frame period, which is how one stalled plug-in took the whole mix
    /// down with it.
    #[test]
    fn handoff_grace_stays_a_small_slice_of_the_block_period() {
        for frames in [32u32, 64, 128, 256, 512] {
            let period = Duration::from_nanos(frames as u64 * 1_000_000_000 / 48_000);
            let grace = output_handoff_grace(frames, 48_000);
            assert!(
                grace * (OUTPUT_HANDOFF_GRACE_DIVISOR / 2) < period,
                "grace {grace:?} is not well below the {frames}-frame period {period:?}"
            );
        }
    }

    /// A large buffer must not turn an eighth of its period into milliseconds
    /// of spinning.
    #[test]
    fn handoff_grace_is_capped_for_large_buffers() {
        assert_eq!(
            output_handoff_grace(4096, 48_000),
            OUTPUT_HANDOFF_GRACE_MAX,
            "an eighth of an 85ms period must clamp to the ceiling"
        );
    }

    /// Before the first block is published there is no geometry to size a wait
    /// against, and spinning on nothing would just burn the callback.
    #[test]
    fn handoff_grace_is_zero_without_block_geometry() {
        assert!(output_handoff_grace(0, 48_000).is_zero());
        assert!(output_handoff_grace(128, 0).is_zero());
    }

    /// The engine-side sink reads exactly what the host produced into `audio_out`
    /// (deinterleaved), and `request_block` drives the handshake — validates the
    /// realtime path without a real plugin or second process.
    #[test]
    fn sink_reads_host_output_and_requests_block() {
        let region = Arc::new(SharedAudioRegion::new_in_process());
        region.bridge().init_header(48_000, 256, 2);
        let sink = SharedRegionSink::new(region.clone());

        // Host produces a known interleaved block: L = i, R = -i.
        let frames = 4usize;
        let mut interleaved = vec![0.0f32; frames * 2];
        for i in 0..frames {
            interleaved[i * 2] = i as f32;
            interleaved[i * 2 + 1] = -(i as f32);
        }
        // SAFETY: single-threaded test; no concurrent reader.
        unsafe { region.bridge().audio_out.write_interleaved(&interleaved) };
        // The host publishes the produced block by bumping `done_seq`.
        region.bridge().done_seq.store(1, Ordering::Release);

        let mut out_l = [0.0f32; 8];
        let mut out_r = [0.0f32; 8];
        let got = sink.read_output(&mut out_l[..frames], &mut out_r[..frames], frames);
        assert_eq!(got, frames);
        for i in 0..frames {
            assert_eq!(out_l[i], i as f32);
            assert_eq!(out_r[i], -(i as f32));
        }

        // Freshness guard: the same produced block is never handed out twice —
        // a stalled host yields 0 (engine bypasses) instead of stale audio.
        let again = sink.read_output(&mut out_l[..frames], &mut out_r[..frames], frames);
        assert_eq!(again, 0);
        assert_eq!(region.bridge().xrun_count.load(Ordering::Relaxed), 1);
        // Once the host produces the next block, reads resume.
        region.bridge().done_seq.store(2, Ordering::Release);
        let fresh = sink.read_output(&mut out_l[..frames], &mut out_r[..frames], frames);
        assert_eq!(fresh, frames);

        // request_block sets block_frames and advances request_seq (the host's
        // service loop fires when request_seq != done_seq).
        let before = region.bridge().request_seq.load(Ordering::Acquire);
        sink.request_block(frames as u32);
        assert_eq!(
            region.bridge().block_frames.load(Ordering::Relaxed),
            frames as u32
        );
        assert_eq!(
            region.bridge().request_seq.load(Ordering::Acquire),
            before + 1
        );
    }

    /// Manual scheduler/xrun stress probe. It drives the exact shared-region
    /// request/response path while CPU competitors yield and spin alongside
    /// the synthetic producer. Keep it ignored in the default suite because
    /// its miss count is intentionally machine/load dependent.
    ///
    /// Run with:
    /// `cargo test -p sphere-plugin-host realtime_bridge_under_cpu_contention -- --ignored --nocapture`
    #[test]
    #[ignore = "manual realtime scheduler/xrun stress probe"]
    fn realtime_bridge_under_cpu_contention() {
        const BLOCKS: u32 = 5_000;
        const FRAMES: usize = 64;

        let region = Arc::new(SharedAudioRegion::new_in_process());
        region.bridge().init_header(48_000, FRAMES as u32, 2);
        let sink = SharedRegionSink::new(region.clone());
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let producer_region = region.clone();
        let producer_running = running.clone();
        let producer = std::thread::spawn(move || {
            let output = [0.25f32; FRAMES * 2];
            let mut last = 0;
            while producer_running.load(Ordering::Relaxed) {
                let request = producer_region.bridge().request_seq.load(Ordering::Acquire);
                if request != last {
                    // SAFETY: this is the only producer and publication occurs
                    // after the full fixed-size write.
                    unsafe {
                        producer_region
                            .bridge()
                            .audio_out
                            .write_interleaved(&output)
                    };
                    producer_region
                        .bridge()
                        .done_seq
                        .store(request, Ordering::Release);
                    last = request;
                } else {
                    std::hint::spin_loop();
                }
            }
        });

        let mut competitors = Vec::new();
        for _ in 0..2 {
            let running = running.clone();
            competitors.push(std::thread::spawn(move || {
                let mut value = 1u64;
                while running.load(Ordering::Relaxed) {
                    value = value
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    std::hint::black_box(value);
                    if value & 0x3fff == 0 {
                        std::thread::yield_now();
                    }
                }
            }));
        }

        let mut completed = 0u32;
        let mut left = [0.0f32; FRAMES];
        let mut right = [0.0f32; FRAMES];
        for _ in 0..BLOCKS {
            sink.request_block(FRAMES as u32);
            if sink.read_output(&mut left, &mut right, FRAMES) == FRAMES {
                completed += 1;
                assert!(left.iter().all(|sample| *sample == 0.25));
                assert!(right.iter().all(|sample| *sample == 0.25));
            }
        }

        running.store(false, Ordering::Relaxed);
        producer.join().expect("synthetic producer panicked");
        for competitor in competitors {
            competitor.join().expect("CPU competitor panicked");
        }

        let xruns = region.bridge().xrun_count.load(Ordering::Relaxed);
        eprintln!(
            "realtime bridge stress os={} blocks={BLOCKS} completed={completed} xruns={xruns}",
            std::env::consts::OS
        );
        assert_eq!(
            completed.saturating_add(xruns),
            BLOCKS,
            "every block must be either fresh output or an explicitly counted xrun"
        );
    }

    #[test]
    fn sink_push_midi_lands_in_ring() {
        let region = Arc::new(SharedAudioRegion::new_in_process());
        region.bridge().init_header(48_000, 256, 2);
        let sink = SharedRegionSink::new(region.clone());

        sink.push_midi(0x90, 60, 100, 7);
        let ev = region.bridge().midi.try_pop().expect("event queued");
        assert_eq!(ev.status, 0x90);
        assert_eq!(ev.data1, 60);
        assert_eq!(ev.data2, 100);
        assert_eq!(ev.sample_offset, 7);
    }

    /// The bare default `[1, 2]` selection must behave like "fold all" so a
    /// multi-out instrument that silences its main bus (audio on aux channels)
    /// stays audible.
    #[test]
    fn sink_bare_main_pair_folds_all_channels() {
        let region = Arc::new(SharedAudioRegion::new_in_process());
        region.bridge().init_header(48_000, 256, 4);
        region.bridge().set_plugin_output_channels(4);
        let sink = SharedRegionSink::new(region.clone());

        // Main bus (ch 1/2) silent; aux bus (ch 3/4) carries the signal.
        let frames = 2usize;
        let interleaved = [
            0.0f32, 0.0, 1.0, 2.0, // frame 0
            0.0, 0.0, 3.0, 4.0, // frame 1
        ];
        // SAFETY: single-threaded test; no concurrent reader.
        unsafe { region.bridge().audio_out.write_interleaved(&interleaved) };
        region.bridge().done_seq.store(1, Ordering::Release);

        let mut out_l = [0.0f32; 2];
        let mut out_r = [0.0f32; 2];
        let got = sink.read_output_for_channels(&mut out_l, &mut out_r, frames, &[1, 2]);
        assert_eq!(got, frames);
        // [1,2] is treated as "fold all": the aux pair is mixed in at pair gain,
        // so the output is non-silent even though the main bus is silent.
        assert!(
            out_l[0] > 0.0,
            "aux channel must reach L (got {})",
            out_l[0]
        );
        assert!(
            out_r[0] > 0.0,
            "aux channel must reach R (got {})",
            out_r[0]
        );
    }

    /// The multichannel read returns the raw interleaved block once (frames,
    /// channels) and obeys the freshness guard — the foundation for multi-out
    /// fold + scatter from a single read.
    #[test]
    fn sink_read_output_multichannel_once_per_block() {
        let region = Arc::new(SharedAudioRegion::new_in_process());
        region.bridge().init_header(48_000, 256, 4);
        region.bridge().set_plugin_output_channels(4);
        let sink = SharedRegionSink::new(region.clone());

        let frames = 2usize;
        let interleaved = [
            0.0f32, 0.0, 1.0, 2.0, // frame 0: main silent, aux loud
            0.0, 0.0, 3.0, 4.0, // frame 1
        ];
        // SAFETY: single-threaded test; no concurrent reader.
        unsafe { region.bridge().audio_out.write_interleaved(&interleaved) };
        region.bridge().done_seq.store(1, Ordering::Release);

        let mut out = [0.0f32; 8];
        let (got_frames, got_channels) = sink.read_output_multichannel(&mut out, frames);
        assert_eq!((got_frames, got_channels), (frames, 4));
        assert_eq!(&out[..8], &interleaved[..8]);
        // Per-channel meter peaks updated from the same block.
        assert_eq!(sink.output_channel_peak(3), 3.0); // ch3 max(1,3)
        assert_eq!(sink.output_channel_peak(4), 4.0); // ch4 max(2,4)
                                                      // Freshness guard: the same block is not handed out twice.
        let again = sink.read_output_multichannel(&mut out, frames);
        assert_eq!(again, (0, 0));
    }

    #[test]
    fn sink_push_param_lands_in_ring() {
        let region = Arc::new(SharedAudioRegion::new_in_process());
        region.bridge().init_header(48_000, 256, 2);
        let sink = SharedRegionSink::new(region.clone());

        sink.push_param(12_345, 0.75, 3);
        let ev = region.bridge().params.try_pop().expect("param queued");
        assert_eq!(ev.param_id, 12_345);
        assert_eq!(ev.value, 0.75);
        assert_eq!(ev.sample_offset, 3);
    }
}
