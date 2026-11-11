//! [`SharedState`]: everything the control and audio threads share.

use super::*;

// ── Shared state (accessed by both control and audio threads) ─────────────────

pub struct SharedState {
    // Meters (audio → control)
    pub peak_l: AtomicU32,
    pub peak_r: AtomicU32,
    pub rms_l: AtomicU32,
    pub rms_r: AtomicU32,

    // Control flags (control → audio, relaxed reads in callback)
    pub test_tone_enabled: AtomicBool,
    pub test_tone_freq: AtomicU32, // Hz as f32 bits
    pub master_volume: AtomicU32,  // linear f32 bits
    pub playing: AtomicBool,
    /// Lifecycle gate for realtime output (see [`AudioEngineState`]).
    pub engine_state: AtomicU8,
    pub position_samples: AtomicU64, // samples elapsed from start
    pub sample_rate: AtomicU32,
    /// The last transport block the output rendered: where it starts, how
    /// long it is, and when it was rendered (ns since `output_clock_epoch`).
    /// `position_samples` already points past that block for the whole time
    /// between two callbacks; see [`SharedState::render_position_now`].
    pub output_block_start: AtomicU64,
    pub output_block_frames: AtomicU64,
    pub output_block_at_ns: AtomicU64,
    pub output_clock_epoch: std::time::Instant,

    // Transport clock (control ↔ audio via commands + Relaxed reads)
    pub bpm_bits: AtomicU64,
    pub time_sig_num: AtomicU32,
    pub time_sig_den: AtomicU32,
    pub loop_enabled: AtomicBool,
    pub loop_start_samples: AtomicU64,
    pub loop_end_samples: AtomicU64,
    pub metronome_enabled: AtomicBool,
    /// Click level as f32 bits — a linear multiplier on the generated click's
    /// own gain. Durable home for the Settings value so a device reopen can
    /// re-seed the fresh stream.
    pub metronome_gain: AtomicU32,
    /// Click timbre as a [`crate::backend::render::MetronomeSound`] code, kept
    /// beside the level for the same reason.
    pub metronome_sound: AtomicU8,

    // Recording (Phase U): input monitor tap + session flag for realtime mix.
    pub recording_active: AtomicBool,
    pub recording_monitor_mix: AtomicBool,
    pub recording_monitor_l: AtomicU32,
    pub recording_monitor_r: AtomicU32,
    /// Samples left in the audible record count-in, published once per block.
    ///
    /// The control thread waits on *this* rather than on a wall-clock timer:
    /// the count-in is being rendered by the audio clock, and a take that
    /// starts off a `std::time` deadline lands wherever the scheduler happened
    /// to wake — which is the jitter a count-in exists to remove.
    pub count_in_remaining_samples: AtomicU64,
    pub live_input_active: AtomicBool,
    pub live_input_l: AtomicU32,
    pub live_input_r: AtomicU32,
    pub live_input_peak_l: AtomicU32,
    pub live_input_peak_r: AtomicU32,

    // ── Live monitoring / input bus (Layers 4, 6, 7) ──────────────────────
    /// Packed source channel indices (`left | right << 32`) tapped by the
    /// monitor path. Publishing the pair in one atomic prevents a callback from
    /// observing half of an old route and half of a new one.
    pub(super) monitor_src_pair: AtomicU64,
    /// `true` when input and output are driven by the same ASIO device clock.
    /// Backend/render code can use this to choose a lower monitor latency target.
    pub monitor_shared_clock: AtomicBool,
    /// Max-hold input peak across *all* capture channels (ASIO session), reset
    /// by the Settings input-test poll via `swap`.
    pub session_input_peak: AtomicU32,
    /// Lock-free stereo bridge from the input callback to the output render
    /// callback. Carries the actual monitored samples (not just a peak).
    pub input_ring: crate::input_ring::InputRing,
    /// `true` when any track has monitoring enabled — gates the output-side
    /// monitor mix so the render callback only taps the ring when needed.
    pub monitor_enabled_any: AtomicBool,
    /// Linear monitor gain applied to the input bus before it reaches master.
    pub monitor_gain: AtomicU32,
    /// Control Room output peaks, post monitor inserts and post monitor
    /// control processor. This is the level of the signal actually sent to the
    /// monitoring hardware output — not the master bus level.
    pub monitor_peak_l: AtomicU32,
    pub monitor_peak_r: AtomicU32,
    /// Peak of the input bus *after* the render callback reads it from the ring
    /// (Layer 4 verification — distinct from the raw `live_input_peak_*`).
    pub input_bus_peak_l: AtomicU32,
    pub input_bus_peak_r: AtomicU32,

    // ── Realtime diagnostics counters (Part 4) ────────────────────────────
    pub input_cb_count: AtomicU64,
    pub output_cb_count: AtomicU64,
    pub input_frames_received: AtomicU64,
    pub monitor_frames_consumed: AtomicU64,
    pub monitor_ring_underruns: AtomicU64,
    pub monitor_ring_overruns: AtomicU64,
    pub record_ring_overruns: AtomicU64,
    pub output_xruns: AtomicU64,
    /// Peak of the record capture (pre-file) for diagnostics.
    pub record_peak: AtomicU32,
    /// Measured output-stream latency (callback → play-out) in seconds, stored
    /// as f32 bits. Published by the output render callback from the cpal
    /// timestamp; read at record-stop to auto-compensate the take position.
    pub output_latency_secs: AtomicU32,
    /// Measured recording-input latency (capture → callback) in seconds, f32
    /// bits. Published by the take's capture callback. Together with
    /// `output_latency_secs` this is the round-trip a recorded clip must shift
    /// earlier by so live overdubs line up with what the player heard.
    pub record_input_latency_secs: AtomicU32,
    /// `true` while the open backend reports its own input/output latencies
    /// (the ASIO driver's `ASIOGetLatencies`). The cpal output callback then
    /// leaves `output_latency_secs` alone: cpal's ASIO timestamp only ever
    /// spans one buffer, so letting it overwrite the driver figure every block
    /// under-reported output latency and hid the input latency entirely.
    pub latency_from_driver: AtomicBool,
    /// Peak actually mixed to the monitor output for diagnostics.
    pub monitor_output_peak: AtomicU32,

    // ── Realtime recording waveform preview (Part 1) ──────────────────────
    /// Finalized preview bins pushed by the recording input callback.
    pub preview_ring: crate::input_ring::PreviewPeakRing,
    /// `true` while a take is feeding the preview ring.
    pub recording_preview_active: AtomicBool,
    /// Monotonic id for the current take (lets the UI discard stale chunks).
    pub recording_preview_id: AtomicU64,
    /// Transport sample at which the take started (preview clip origin).
    pub recording_preview_start_sample: AtomicU64,
    pub recording_preview_sample_rate: AtomicU32,
    pub recording_preview_channels: AtomicU32,
    pub recording_preview_peaks_per_sec: AtomicU32,
    /// One ring per potential simultaneously-recording track. Slot `i` backs
    /// the track id at index `i` of `recording_preview_track_ids` for the
    /// current take. Fixed-length, allocated once (see `Default`); the audio
    /// callback only indexes into existing slots — no realtime allocation.
    /// Purely additive alongside `preview_ring` above (which stays the
    /// single-track legacy/monitor-mix path).
    pub preview_rings: Vec<crate::input_ring::PreviewPeakRing>,
    /// Track ids backing each `preview_rings` slot for the current take.
    /// Written once per take-start and read only from the control thread
    /// (`recording_preview_tracks`/`drain_recording_preview_peaks_for_track`)
    /// — never the audio callback, so a plain mutex is fine here.
    pub recording_preview_track_ids: Mutex<Vec<String>>,

    // ── Callback watchdog (audio-hang spec §12; audio → control) ──────────
    /// Duration of the most recent output callback, microseconds.
    pub last_callback_us: AtomicU32,
    /// SoundFont voices sounding across every track at the end of the last
    /// audio callback. Published by the callback with a single relaxed store
    /// and read by the control thread for the transport's load meter.
    pub active_voices: AtomicU32,
    /// Worst output-callback duration seen since stream open, microseconds.
    pub max_callback_us: AtomicU32,
    /// Blocks that exceeded the 2 ms debug threshold.
    pub slow_callback_count: AtomicU64,
    /// The latest slow block the callback flagged, for the control thread to
    /// log ([`EngineInner::log_callback_diagnostics`]). The callback never
    /// formats or writes the line itself.
    pub slow_block_report: SlowBlockReport,

    // ── Dropout protection (Part 2; audio ↔ control) ──────────────────────
    /// Active [`DropoutProtectionMode`] as `u8` (control → audio). Default
    /// `Medium`. Read by [`record_output_callback_timing`] to pick the warn
    /// fraction; reserved for the deferred render-ahead safety buffer.
    pub dropout_protection_mode: AtomicU8,
    /// Blocks flagged as dropout-risk (elapsed ≥ mode fraction of the deadline).
    pub dropout_count: AtomicU64,
    /// [`DropoutReason`] of the most recent flagged block, as `u8`.
    pub dropout_last_reason: AtomicU8,
    /// Per-block wall-clock budget published each callback, microseconds.
    pub callback_deadline_us: AtomicU32,

    // ── Audioguard (audio ↔ control; see `crate::audioguard`) ─────────────
    /// Frames rendered ahead into the Audioguard ring and not played yet.
    /// While non-zero, `position_samples` is the render cursor, ahead of what
    /// is heard, and [`SharedState::playback_position`] reads the cursor the
    /// played blocks left instead.
    pub guard_pending_frames: AtomicU64,
    /// Transport cursor right after the last frame the device played from the
    /// ring.
    pub guard_played_cursor: AtomicU64,
    /// The guard thread is rendering ahead.
    pub guard_engaged: AtomicBool,
    /// How far ahead it is aiming, frames.
    pub guard_ahead_frames: AtomicU64,
    /// Device blocks the ring could not supply in time.
    pub guard_misses: AtomicU64,
    /// Blocks rendered in the device callback that took longer than the block
    /// lasts: the device ran out of audio, whatever the protection level. The
    /// at-risk count (`dropout_count`) also holds blocks merely close to it.
    pub callback_overruns: AtomicU64,
    /// Times the output guard muted the device output, and why it last did
    /// (an [`crate::audioguard::OutputGuardReason`] as `u8`).
    pub guard_output_mutes: AtomicU64,
    pub guard_output_reason: AtomicU8,
    /// Bumped by every played note, controller or pitch bend the callback
    /// receives (MIDI keyboard, virtual keyboard, plug-in preview), so the
    /// guard can step aside while someone is performing.
    pub live_events: AtomicU64,

    // DAUx diagnostics (incremented by audio thread, read by control thread)
    pub glitch_count: AtomicU64,
    /// Device-level underruns reported by the backend (ALSA xruns and the
    /// equivalent elsewhere). Distinct from `output_xruns`, which counts the
    /// *software* monitor ring running dry: this one means the hardware ran out
    /// of samples, so the user heard a click.
    ///
    /// No backend can report these yet, so it reads 0 everywhere: cpal recovers
    /// from an ALSA xrun inside its own stream worker without telling the error
    /// callback, and the other hosts have no equivalent signal. Wiring it up
    /// needs a backend that can observe the underrun first.
    pub device_xruns: AtomicU64,
    pub mmcss_active: AtomicBool,
    /// Set by a backend when the audio device disappears mid-stream (USB
    /// unplugged, default device changed, exclusive-mode timeout). Read by the
    /// control thread to surface a DeviceLost state and trigger recovery.
    pub device_lost: AtomicBool,
    /// Settings → Audio → Shared Headroom (control → audio). While set, a
    /// WASAPI Shared output stream lowers what it hands Windows by
    /// [`crate::backend::cpal_backend::SHARED_HEADROOM_DB`] right before the
    /// device-format conversion, so peaks above 0 dBFS reach the shared-mode
    /// mixer below the ceiling it limits at. Every other stream ignores it, and
    /// meters, recording and export never see it. Read once per block; kept
    /// here so a device reopen keeps it.
    pub shared_headroom: AtomicBool,
    /// Playback plugin delay compensation (Phase W). Settings → Playback.
    pub pdc_enabled: AtomicBool,
    /// Whether a take bypasses delay compensation while it records
    /// (Settings → Recording). Read when a take starts.
    pub record_pdc_bypass_enabled: AtomicBool,
    /// A take is recording with delay compensation bypassed. The audio
    /// thread follows it in place ([`crate::runtime::RuntimeProject::follow_pdc_bypass`]).
    pub record_pdc_bypass: AtomicBool,
    /// Monotonic generation of the latency-compensation graph (bumped on PDC
    /// toggle). Stamped into the offline-export snapshot for graph-version parity.
    pub latency_graph_version: AtomicU64,
    /// Bridge handshake period, frames: the value delay compensation and the
    /// latency report charge each bridged insert for its one-block handshake.
    /// Seeded with the buffer size the open stream reported (control → audio,
    /// [`SharedState::set_stream_bridge_period`]), which for several backends
    /// is only the size that was *requested*; the audio thread replaces it
    /// with the block size the device actually delivers once that is steady
    /// (#408). 0 until a stream reports one; read it through
    /// [`SharedState::bridge_period_frames_for_block`].
    pub bridge_period_frames: AtomicU32,
    /// Audio-thread bookkeeping for [`Self::bridge_period_frames`]: the last
    /// delivered block size that differs from the period (high 32 bits) and
    /// how many blocks in a row had it (low 32 bits).
    pub bridge_block_run: AtomicU64,
    /// Monotonic id of the newest Browser sample-preview request (control →
    /// audio). Decodes run off the realtime thread and finish out of order, so
    /// both the decode worker and the callback compare against this before a
    /// source is allowed to start playing.
    pub audition_request_token: AtomicU64,
    /// Playhead of the Browser sample preview, in microseconds of the source
    /// file, published once per callback (audio → control). [`u64::MAX`] means
    /// nothing is auditioning. Integer microseconds keep this a single relaxed
    /// store with no float-bit dance on the realtime path.
    pub audition_position_us: AtomicU64,

    /// Audio Jam bridge. Remote streams are written into it by the jam receive
    /// thread and read by the render pass; publish taps go the other way. It
    /// lives here so the audio callback reaches it through the `Arc` it already
    /// holds, and so the engine keeps owning the device a jam would otherwise
    /// try to open for itself.
    pub jam_bus: crate::jam_bus::JamAudioBus,
}

/// Blocks in a row of one size, different from the bridge period, after
/// which that size is taken as the period the device really delivers
/// ([`SharedState::bridge_period_frames_for_block`]).
pub const BRIDGE_PERIOD_STEADY_BLOCKS: u32 = 8;

/// Sentinel stored in [`SharedState::audition_position_us`] when no Browser
/// preview is playing.
pub const AUDITION_POSITION_IDLE: u64 = u64::MAX;

/// The persisted metronome-volume position that means "unity" — the click level
/// Studio shipped with before the control was connected to anything. Settings
/// defaults to exactly this, so connecting the slider does not quietly change
/// anyone's click level; moving it up is headroom, moving it down attenuates.
pub const METRONOME_UNITY_VOLUME: f32 = 0.8;

impl Default for SharedState {
    fn default() -> Self {
        Self {
            peak_l: AtomicU32::new(f32_store(0.0)),
            peak_r: AtomicU32::new(f32_store(0.0)),
            rms_l: AtomicU32::new(f32_store(0.0)),
            rms_r: AtomicU32::new(f32_store(0.0)),
            test_tone_enabled: AtomicBool::new(false),
            test_tone_freq: AtomicU32::new(f32_store(440.0)),
            master_volume: AtomicU32::new(f32_store(1.0)),
            playing: AtomicBool::new(false),
            engine_state: AtomicU8::new(AudioEngineState::Paused as u8),
            position_samples: AtomicU64::new(0),
            sample_rate: AtomicU32::new(44100),
            output_block_start: AtomicU64::new(0),
            output_block_frames: AtomicU64::new(0),
            output_block_at_ns: AtomicU64::new(0),
            output_clock_epoch: std::time::Instant::now(),
            bpm_bits: AtomicU64::new(120.0_f64.to_bits()),
            time_sig_num: AtomicU32::new(4),
            time_sig_den: AtomicU32::new(4),
            loop_enabled: AtomicBool::new(false),
            loop_start_samples: AtomicU64::new(0),
            loop_end_samples: AtomicU64::new(0),
            metronome_enabled: AtomicBool::new(false),
            metronome_gain: AtomicU32::new(f32_store(1.0)),
            metronome_sound: AtomicU8::new(0),
            recording_active: AtomicBool::new(false),
            recording_monitor_mix: AtomicBool::new(false),
            recording_monitor_l: AtomicU32::new(f32_store(0.0)),
            recording_monitor_r: AtomicU32::new(f32_store(0.0)),
            count_in_remaining_samples: AtomicU64::new(0),
            live_input_active: AtomicBool::new(false),
            live_input_l: AtomicU32::new(f32_store(0.0)),
            live_input_r: AtomicU32::new(f32_store(0.0)),
            live_input_peak_l: AtomicU32::new(f32_store(0.0)),
            live_input_peak_r: AtomicU32::new(f32_store(0.0)),
            monitor_src_pair: AtomicU64::new(pack_monitor_source_pair(0, 1)),
            monitor_shared_clock: AtomicBool::new(false),
            session_input_peak: AtomicU32::new(f32_store(0.0)),
            input_ring: crate::input_ring::InputRing::default(),
            monitor_enabled_any: AtomicBool::new(false),
            monitor_gain: AtomicU32::new(f32_store(1.0)),
            monitor_peak_l: AtomicU32::new(f32_store(0.0)),
            monitor_peak_r: AtomicU32::new(f32_store(0.0)),
            input_bus_peak_l: AtomicU32::new(f32_store(0.0)),
            input_bus_peak_r: AtomicU32::new(f32_store(0.0)),
            input_cb_count: AtomicU64::new(0),
            output_cb_count: AtomicU64::new(0),
            input_frames_received: AtomicU64::new(0),
            monitor_frames_consumed: AtomicU64::new(0),
            monitor_ring_underruns: AtomicU64::new(0),
            monitor_ring_overruns: AtomicU64::new(0),
            record_ring_overruns: AtomicU64::new(0),
            output_xruns: AtomicU64::new(0),
            record_peak: AtomicU32::new(f32_store(0.0)),
            output_latency_secs: AtomicU32::new(f32_store(0.0)),
            record_input_latency_secs: AtomicU32::new(f32_store(0.0)),
            latency_from_driver: AtomicBool::new(false),
            monitor_output_peak: AtomicU32::new(f32_store(0.0)),
            preview_ring: crate::input_ring::PreviewPeakRing::default(),
            recording_preview_active: AtomicBool::new(false),
            recording_preview_id: AtomicU64::new(0),
            recording_preview_start_sample: AtomicU64::new(0),
            recording_preview_sample_rate: AtomicU32::new(0),
            recording_preview_channels: AtomicU32::new(0),
            recording_preview_peaks_per_sec: AtomicU32::new(0),
            preview_rings: (0..crate::input_ring::MAX_RECORDING_PREVIEW_TRACKS)
                .map(|_| crate::input_ring::PreviewPeakRing::default())
                .collect(),
            recording_preview_track_ids: Mutex::new(Vec::new()),
            last_callback_us: AtomicU32::new(0),
            active_voices: AtomicU32::new(0),
            max_callback_us: AtomicU32::new(0),
            slow_callback_count: AtomicU64::new(0),
            slow_block_report: SlowBlockReport::default(),
            dropout_protection_mode: AtomicU8::new(AudioguardLevel::Normal as u8),
            guard_pending_frames: AtomicU64::new(0),
            guard_played_cursor: AtomicU64::new(0),
            guard_engaged: AtomicBool::new(false),
            guard_ahead_frames: AtomicU64::new(0),
            guard_misses: AtomicU64::new(0),
            callback_overruns: AtomicU64::new(0),
            guard_output_mutes: AtomicU64::new(0),
            guard_output_reason: AtomicU8::new(0),
            live_events: AtomicU64::new(0),
            dropout_count: AtomicU64::new(0),
            dropout_last_reason: AtomicU8::new(DropoutReason::None as u8),
            callback_deadline_us: AtomicU32::new(0),
            glitch_count: AtomicU64::new(0),
            device_xruns: AtomicU64::new(0),
            mmcss_active: AtomicBool::new(false),
            device_lost: AtomicBool::new(false),
            shared_headroom: AtomicBool::new(false),
            pdc_enabled: AtomicBool::new(true),
            record_pdc_bypass_enabled: AtomicBool::new(true),
            record_pdc_bypass: AtomicBool::new(false),
            latency_graph_version: AtomicU64::new(1),
            bridge_period_frames: AtomicU32::new(0),
            bridge_block_run: AtomicU64::new(0),
            audition_request_token: AtomicU64::new(0),
            audition_position_us: AtomicU64::new(AUDITION_POSITION_IDLE),
            jam_bus: crate::jam_bus::JamAudioBus::new(),
        }
    }
}

impl SharedState {
    /// Publish the buffer size a newly opened stream reported (0: none) as
    /// the bridge period, forgetting what the previous stream delivered.
    /// Control thread, before the stream's first callback.
    pub fn set_stream_bridge_period(&self, frames: u32) {
        self.bridge_block_run.store(0, Ordering::Relaxed);
        self.bridge_period_frames.store(frames, Ordering::Relaxed);
    }

    /// The bridge handshake period for a callback rendering `block_frames`.
    ///
    /// The period the open stream reported, whatever size this particular
    /// callback is — until the device shows it delivers another size: after
    /// [`BRIDGE_PERIOD_STEADY_BLOCKS`] blocks in a row of one size that
    /// differs from the period, that size becomes the period (#408). Several
    /// backends can only report the size they *requested* (cpal WASAPI
    /// shared, CoreAudio, the legacy cpal path), and the device may
    /// negotiate another one; charging the requested size left bridged
    /// compensation wrong for the whole session. A size that varies from
    /// block to block (WASAPI shared/event mode, partial renders) never forms
    /// such a run, so it still never reaches delay compensation (#377).
    ///
    /// A stream that reported no size takes its first block as its period.
    /// The latency report reads the same value.
    ///
    /// Relaxed loads, and a store only while a different size is being seen.
    /// Realtime-safe.
    #[inline]
    pub fn bridge_period_frames_for_block(&self, block_frames: u32) -> u32 {
        let period = self.bridge_period_frames.load(Ordering::Relaxed);
        if block_frames == 0 {
            return period;
        }
        if period == 0 {
            return match self.bridge_period_frames.compare_exchange(
                0,
                block_frames,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => block_frames,
                Err(current) => current,
            };
        }
        let run = self.bridge_block_run.load(Ordering::Relaxed);
        if block_frames == period {
            if run != 0 {
                self.bridge_block_run.store(0, Ordering::Relaxed);
            }
            return period;
        }
        let count = if (run >> 32) as u32 == block_frames {
            (run as u32).saturating_add(1)
        } else {
            1
        };
        if count >= BRIDGE_PERIOD_STEADY_BLOCKS {
            self.bridge_period_frames
                .store(block_frames, Ordering::Relaxed);
            self.bridge_block_run.store(0, Ordering::Relaxed);
            return block_frames;
        }
        self.bridge_block_run.store(
            (u64::from(block_frames) << 32) | u64::from(count),
            Ordering::Relaxed,
        );
        period
    }

    /// Note the transport block the output callback is about to advance past.
    /// Three atomic stores and a clock read; realtime-safe.
    #[inline]
    pub(crate) fn note_output_block(&self, start: u64, frames: u64) {
        let at = self.output_clock_epoch.elapsed().as_nanos() as u64;
        self.output_block_at_ns.store(at, Ordering::Relaxed);
        self.output_block_frames.store(frames, Ordering::Relaxed);
        self.output_block_start.store(start, Ordering::Release);
    }

    /// The transport cursor of what the device is playing: `position_samples`,
    /// or — while Audioguard has blocks rendered ahead — the cursor the last
    /// played block left, which is behind it by the ring. Control-side readers
    /// (playhead, take start) use this; the render side keeps the raw cursor.
    #[inline]
    pub fn playback_position(&self) -> u64 {
        if self.guard_pending_frames.load(Ordering::Acquire) > 0 {
            self.guard_played_cursor.load(Ordering::Relaxed)
        } else {
            self.position_samples.load(Ordering::Relaxed)
        }
    }

    /// The transport sample the output is rendering right now.
    ///
    /// `position_samples` is advanced at the end of each output callback, so
    /// between two callbacks it names the start of the *next* block. Read from
    /// the input side, that is up to one block ahead of the block whose
    /// play-out the measured output latency refers to, and a take stamped with
    /// it landed that much late. This interpolates into the last rendered
    /// block by the time since it was rendered instead.
    ///
    /// Falls back to `position_samples` whenever the block clock does not
    /// describe the current position: nothing rendered yet, a seek or loop
    /// wrap since, or a callback that is overdue.
    pub(crate) fn render_position_now(&self) -> u64 {
        let position = self.playback_position();
        let start = self.output_block_start.load(Ordering::Acquire);
        let frames = self.output_block_frames.load(Ordering::Relaxed);
        let at = self.output_block_at_ns.load(Ordering::Relaxed);
        let sample_rate = self.sample_rate.load(Ordering::Relaxed) as u64;
        if frames == 0 || sample_rate == 0 || start.saturating_add(frames) != position {
            return position;
        }
        let now = self.output_clock_epoch.elapsed().as_nanos() as u64;
        let elapsed = now.saturating_sub(at);
        let block_ns = frames.saturating_mul(1_000_000_000) / sample_rate;
        if elapsed > block_ns.saturating_mul(2) {
            return position;
        }
        let offset = (elapsed.saturating_mul(sample_rate) / 1_000_000_000).min(frames);
        start + offset
    }

    /// Publish the Browser preview playhead from the render callback. One
    /// relaxed store per block; `None` parks the sentinel so the UI stops
    /// drawing a playhead the moment the voice retires.
    #[inline]
    pub(crate) fn publish_audition_position(&self, seconds: Option<f64>) {
        let value = match seconds {
            Some(seconds) if seconds >= 0.0 => {
                ((seconds * 1_000_000.0) as u64).min(AUDITION_POSITION_IDLE - 1)
            }
            _ => AUDITION_POSITION_IDLE,
        };
        self.audition_position_us.store(value, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn set_monitor_source_pair(&self, left: u32, right: u32) {
        self.monitor_src_pair
            .store(pack_monitor_source_pair(left, right), Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn monitor_source_pair(&self) -> (u32, u32) {
        unpack_monitor_source_pair(self.monitor_src_pair.load(Ordering::Relaxed))
    }
}
