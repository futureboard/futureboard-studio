//! DAUx cpal backend — wraps CPAL for WASAPI Shared / CoreAudio / ALSA.
//!
//! This is the "Auto" / "WasapiShared" / "CoreAudio" / "Alsa" backend.
//! On each platform cpal picks the best native API:
//!   - Windows  → WASAPI Shared event-driven
//!   - macOS    → CoreAudio
//!   - Linux    → ALSA
//!
//! On Windows the audio thread gets MMCSS "Pro Audio" priority if
//! `config.mmcss_priority` is true. On Linux it is promoted to `SCHED_FIFO`
//! unconditionally (best effort) — see `rt_priority`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SampleFormat, SizedSample};
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::backend::render::{drain_commands, fill_output_f32, LocalAudioState};
use crate::backend::DauxDeviceConfig;
use crate::command::EngineCommand;
use crate::dsp::dither::{DitheredOutput, OutputDither};
use crate::engine::SharedState;
use crate::error::SphereAudioError;
use crate::runtime::RuntimeProject;
use crate::types::JsAudioDeviceInfo;

// ─────────────────────────────────────────────────────────────────────────────

pub struct CpalStreamHandle {
    stream: cpal::Stream,
    pub cmd_tx: Sender<EngineCommand>,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub device_name: String,
    pub backend_name: String,
}

// Safety: see engine.rs — stream is only touched on the JS/main thread under Mutex.
unsafe impl Send for CpalStreamHandle {}
unsafe impl Sync for CpalStreamHandle {}

impl CpalStreamHandle {
    pub fn play(&self) -> Result<(), String> {
        self.stream.play().map_err(|e| e.to_string())
    }
    pub fn pause(&self) -> Result<(), String> {
        self.stream.pause().map_err(|e| e.to_string())
    }
}

// ─────────────────────────────────────────────────────────────────────────────

pub fn list_output_devices() -> Vec<JsAudioDeviceInfo> {
    crate::device::list_output_devices()
}

pub fn list_input_devices() -> Vec<JsAudioDeviceInfo> {
    crate::device::list_input_devices()
}

/// Open a cpal output stream with the given `DauxDeviceConfig`.
/// Returns the stream handle on success.
pub fn open(
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<CpalStreamHandle, SphereAudioError> {
    open_on_host(
        &cpal::default_host(),
        config,
        shared,
        initial_runtime,
        glitch_counter,
    )
}

/// Open the explicit Windows WASAPI Shared host. CPAL's Windows default is
/// currently WASAPI too, but selecting the host by id keeps the persisted
/// “Aggregate System” backend from being redirected to a driver host by a
/// future CPAL default-host change.
#[cfg(target_os = "windows")]
pub fn open_wasapi_shared(
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<CpalStreamHandle, SphereAudioError> {
    let host = cpal::host_from_id(cpal::HostId::Wasapi)
        .map_err(|error| SphereAudioError::BackendUnavailable(format!("WASAPI Shared: {error}")))?;
    open_on_host(&host, config, shared, initial_runtime, glitch_counter)
}

#[cfg(not(target_os = "windows"))]
pub fn open_wasapi_shared(
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<CpalStreamHandle, SphereAudioError> {
    open(config, shared, initial_runtime, glitch_counter)
}

/// Open through a specific CPAL host while reusing the DAUx render kernel.
/// Host/device discovery and stream creation are control-thread operations.
pub(crate) fn open_on_host(
    host: &cpal::Host,
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<CpalStreamHandle, SphereAudioError> {
    let (dev, dev_name) =
        crate::device::resolve_output_device_for_host(host, config.output_device_id.as_deref())
            .map_err(SphereAudioError::DeviceNotFound)?;

    let backend_name = host.id().name().to_string();

    // Build stream config candidates.
    let default_supported = dev
        .default_output_config()
        .map_err(|e| SphereAudioError::StreamOpenFailed(e.to_string()))?;
    let sample_format = default_supported.sample_format();
    let default_cfg = default_supported.config();

    // Apply requested sample rate / buffer size overrides.
    let requested_period = config
        .buffer_size
        .map(|bs| if config.safe_mode { bs.max(512) } else { bs });

    // `(label, cpal config, frames the callback is expected to receive)`.
    // The third element is what gets reported as the active buffer size: on
    // ALSA the `Fixed` value is the whole PCM ring, not the callback block, so
    // the two diverge (see `period_candidates`).
    let candidates: Vec<(&str, cpal::StreamConfig, u32)> = {
        let mut v = Vec::new();
        if config.sample_rate.is_some() || requested_period.is_some() {
            let mut base = default_cfg.clone();
            if let Some(sr) = config.sample_rate {
                base.sample_rate = cpal::SampleRate(sr);
            }
            match requested_period {
                Some(period) => {
                    for (label, fixed_frames, callback_frames) in period_candidates(period) {
                        let mut c = base.clone();
                        c.buffer_size = BufferSize::Fixed(fixed_frames);
                        v.push((label, c, callback_frames));
                    }
                }
                None => v.push(("requested", base, 0)),
            }
        }
        v.push(("default", default_cfg, 0));
        v
    };

    let mut last_error = None;

    for (label, stream_config, callback_frames) in &candidates {
        shared
            .sample_rate
            .store(stream_config.sample_rate.0, Ordering::Relaxed);

        let (tx, rx) = bounded::<EngineCommand>(512);

        match build_typed_stream(
            &dev,
            stream_config,
            sample_format,
            rx,
            Arc::clone(&shared),
            initial_runtime.clone(),
            Arc::clone(&glitch_counter),
            config.mmcss_priority,
        ) {
            Ok(stream) => {
                // `callback_frames` is what the chosen candidate *predicts* the
                // callback block will be (on ALSA, derived from the ring — see
                // `period_candidates`; elsewhere, the requested size itself).
                // The ALSA backend can report what it actually negotiated via
                // `negotiated_buffer()` (reads back `snd_pcm_hw_params`), which
                // is the real figure when it disagrees with the prediction —
                // dmix/PipeWire's ALSA plugin can round `_near` calls away from
                // what was asked for. `BufferSize::Default` means we never
                // asked, so report 0 rather than guess when no negotiated value
                // is available either.
                let predicted = match stream_config.buffer_size {
                    BufferSize::Fixed(_) => *callback_frames,
                    BufferSize::Default => 0,
                };
                let buf_size = match stream.negotiated_buffer() {
                    Some(negotiated) => {
                        if negotiated.period_frames != predicted && predicted != 0 {
                            eprintln!(
                                "[DAUx cpal] {label}: negotiated period {} frames (requested {predicted})",
                                negotiated.period_frames
                            );
                        }
                        negotiated.period_frames
                    }
                    None => predicted,
                };
                return Ok(CpalStreamHandle {
                    stream,
                    cmd_tx: tx,
                    sample_rate: stream_config.sample_rate.0,
                    buffer_size: buf_size,
                    device_name: dev_name,
                    backend_name,
                });
            }
            Err(e) => {
                last_error = Some(format!("{label} config failed: {e}"));
            }
        }
    }

    Err(SphereAudioError::StreamOpenFailed(
        last_error.unwrap_or_else(|| "no candidates available".into()),
    ))
}

// ── Requested-buffer-size candidates ─────────────────────────────────────────

/// Number of periods cpal's ALSA ring is asked to hold.
///
/// cpal derives `period = buffer / 4` (`set_period_size_near`) from whatever
/// `BufferSize::Fixed` it is handed, so a 4-period ring is what makes the
/// callback block come back equal to the requested period.
#[cfg(target_os = "linux")]
pub(crate) const ALSA_PERIODS_PER_BUFFER: u32 = 4;

/// Translate a requested *period* (callback block size, which is what the UI
/// and every other platform mean by "buffer size") into the `BufferSize::Fixed`
/// values worth trying, in preference order.
///
/// Returns `(label, fixed value handed to cpal, frames the callback receives)`.
///
/// On ALSA those last two differ: cpal passes `Fixed` straight to
/// `snd_pcm_hw_params_set_buffer_size` — the whole PCM ring — and derives the
/// period as a quarter of it. Requesting 256 there therefore produced 64-frame
/// wakeups backed by only 5.3 ms of headroom at 48 kHz, roughly a quarter of
/// the safety margin the same setting buys on WASAPI Shared or CoreAudio.
///
/// Shared by output open and **input** open so capture does not fall back to
/// ALSA's default 100 ms ring while playback runs at the UI buffer size.
#[cfg(target_os = "linux")]
pub(crate) fn period_candidates(period: u32) -> Vec<(&'static str, u32, u32)> {
    // The callback is handed everything available, which after a late wakeup is
    // the *entire* ring — measured, not assumed: a 4096-frame ring delivers
    // 4096-frame blocks. `request_block` clamps at `MAX_BRIDGE_BLOCK_FRAMES`,
    // so a ring beyond that would silently truncate bridged plugin audio.
    // Trade periods away rather than exceed it: large buffer sizes still get a
    // 2- or 1-period ring, which is what they had before.
    const MAX_RING: u32 = crate::plugin_bridge::MAX_BRIDGE_BLOCK_FRAMES as u32;
    let periods = (MAX_RING / period.max(1)).clamp(1, ALSA_PERIODS_PER_BUFFER);
    let ring = period.saturating_mul(periods);
    let mut candidates = vec![
        // Preferred: as many periods as fit, so the callback block == the
        // requested period and the rest absorbs scheduling jitter.
        ("requested (ALSA multi-period ring)", ring, period),
    ];
    // `set_buffer_size` is an *exact* match on ALSA, and dmix / the PipeWire
    // ALSA plugin routinely refuse the larger ring above. Jumping straight
    // from a 4-period ring to the single-period "bare ring" below quarters
    // the callback size (`period` -> `period/4`) — worse headroom on exactly
    // the systems most likely to reject the preferred candidate. Try a
    // 2-period ring first: still rejectable on the same exact-match grounds,
    // but only halves the callback instead of quartering it.
    //
    // Only worth trying when it's actually smaller than the preferred ring
    // above (`periods > 2`); otherwise it would just repeat the same value.
    if periods > 2 {
        candidates.push((
            "requested (ALSA 2-period ring)",
            period.saturating_mul(2),
            (period / 2).max(1),
        ));
    }
    // Last resort before surrendering to the device default (25 ms period /
    // 100 ms ring): the bare requested value as the whole ring.
    candidates.push((
        "requested (ALSA bare ring)",
        period,
        (period / ALSA_PERIODS_PER_BUFFER).max(1),
    ));
    candidates
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn period_candidates(period: u32) -> Vec<(&'static str, u32, u32)> {
    vec![("requested", period, period)]
}

/// Promote a cpal ALSA capture thread the same way output is promoted.
/// Callers announce from the first callback (see `rt_priority::Announcer`).
#[cfg(target_os = "linux")]
pub(crate) fn spawn_capture_rt_promoter() -> rt_priority::Announcer {
    rt_priority::spawn_promoter()
}

// ── Stream builders ───────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_typed_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    cmd_rx: Receiver<EngineCommand>,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
    mmcss_priority: bool,
) -> Result<cpal::Stream, String> {
    macro_rules! build_for {
        ($T:ty) => {
            build_stream_typed::<$T>(
                device,
                config,
                cmd_rx,
                shared,
                initial_runtime,
                glitch_counter,
                mmcss_priority,
            )
        };
    }
    match sample_format {
        SampleFormat::I8 => build_for!(i8),
        SampleFormat::I16 => build_for!(i16),
        SampleFormat::I32 => build_for!(i32),
        SampleFormat::I64 => build_for!(i64),
        SampleFormat::U8 => build_for!(u8),
        SampleFormat::U16 => build_for!(u16),
        SampleFormat::U32 => build_for!(u32),
        SampleFormat::U64 => build_for!(u64),
        SampleFormat::F32 => build_for!(f32),
        SampleFormat::F64 => build_for!(f64),
        fmt => Err(format!("unsupported sample format: {fmt}")),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    cmd_rx: Receiver<EngineCommand>,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
    mmcss_priority: bool,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + Sample + FromSample<f32> + DitheredOutput,
{
    let output_sample_rate = config.sample_rate.0;
    // Word-length reduction state for integer device formats (ASIO Int32LSB /
    // Int16LSB, WASAPI Exclusive integer modes). Owned by the callback; one
    // xorshift per stream, advanced per sample, never reseeded mid-stream.
    let mut dither = OutputDither::new();
    let sr = output_sample_rate as f64;
    let ch = config.channels as usize;
    let mut runtime = initial_runtime;
    runtime.retarget_sample_rate(output_sample_rate);
    #[cfg(target_os = "windows")]
    let mut mmcss_set = false;
    #[cfg(not(target_os = "windows"))]
    let mmcss_set = false;
    // Linux: cpal spawns `cpal_alsa_out` at plain SCHED_OTHER, so the audio
    // thread is preempted by the compositor, plugin editors and the scanner
    // like any other thread. The first callback announces its tid and a helper
    // thread does the promotion — see `rt_priority`.
    #[cfg(target_os = "linux")]
    let rt_announcer = rt_priority::spawn_promoter();
    #[cfg(target_os = "linux")]
    let mut rt_announced = false;
    // Preallocate before stream start: the callback must never grow this on the
    // audio thread. The requested buffer size is only a *hint* — WASAPI Shared
    // (and some other cpal backends) round the period up to the device's
    // shared-mode engine period, so the real callback block is frequently
    // larger than the `Fixed` size we asked for (e.g. ~480 frames at 48 kHz
    // even when 256 was requested). Sizing the scratch to exactly the requested
    // frames made every oversized block trip the silence guard below, so the
    // stream opened and "ran" but never emitted audio — unlike ASIO, which
    // hands back precisely the requested buffer. Preallocate a generous upper
    // bound so realistic shared-mode blocks always fit; the guard below stays
    // only as a last-resort safety net.
    //
    // The floor is rate-relative rather than a flat 8192 frames because cpal's
    // ALSA `BufferSize::Default` asks for a 100 ms ring, and the callback is
    // handed everything available — the whole ring on the first block and after
    // every xrun recovery. At 96/192 kHz a flat 8192 was smaller than that ring,
    // so the guard below fired on exactly those blocks and emitted silence.
    let default_scratch_frames = (output_sample_rate as usize / 5).max(8_192);
    let requested_frames = match config.buffer_size {
        BufferSize::Fixed(frames) => frames as usize,
        BufferSize::Default => default_scratch_frames,
    };
    let scratch_frames = requested_frames.max(default_scratch_frames);
    let mut local = LocalAudioState::with_monitor_capacity(sr, scratch_frames);
    let mut f32_scratch = vec![0.0f32; scratch_frames.saturating_mul(ch)];

    // Separate handle for the error callback (the data callback moves `shared`).
    let err_shared = Arc::clone(&shared);
    let callback_glitch_counter = Arc::clone(&glitch_counter);

    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                // Publish the device's own callback→playout delay. This is the
                // true output latency (the whole ring on ALSA, not one period),
                // and record-stop compensation reads it to line overdubs up.
                // The legacy callback path already did this; the DAUx path was
                // dropping `info` on the floor, leaving it stuck at 0.
                //
                // Not when the backend reports its own latency (ASIO): cpal's
                // ASIO timestamp spans exactly one buffer, so storing it here
                // every block would overwrite the driver's full-path figure.
                let ts = info.timestamp();
                if !shared.latency_from_driver.load(Ordering::Relaxed) {
                    if let Some(delay) = ts.playback.duration_since(&ts.callback) {
                        shared.output_latency_secs.store(
                            crate::engine::f32_store(delay.as_secs_f32()),
                            Ordering::Relaxed,
                        );
                    }
                }

                // ── Set MMCSS on first callback invocation ────────────────────
                #[cfg(target_os = "windows")]
                if mmcss_priority && !mmcss_set {
                    mmcss_set = set_mmcss_pro_audio();
                }
                #[cfg(not(target_os = "windows"))]
                let _ = mmcss_priority; // suppress unused warning

                #[cfg(target_os = "linux")]
                if !rt_announced {
                    rt_announced = true;
                    rt_announcer.announce_current_thread();
                }

                // ── Drain command queue ───────────────────────────────────────
                drain_commands(
                    &cmd_rx,
                    &mut runtime,
                    &shared,
                    &mut local,
                    output_sample_rate,
                );

                // ── Fill via shared f32 kernel ────────────────────────────────
                let frames_needed = data.len() / ch.max(1);
                let f32_len = frames_needed * ch;
                if f32_len > f32_scratch.len() {
                    for sample in data.iter_mut() {
                        *sample = T::from_sample(0.0);
                    }
                    callback_glitch_counter.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                // `fill_output_f32` fully overwrites every sample of its output
                // slice on every reachable path (silence branch, audition-only
                // branch, and the render paths all zero-then-write or write
                // every frame) — no pre-zero needed here.
                let scratch = &mut f32_scratch[..f32_len];

                fill_output_f32(scratch, ch, &mut runtime, &shared, &mut local);

                // ── Convert f32 → T ───────────────────────────────────────────
                // Not `from_sample`: that truncates toward zero, which is a bias
                // plus signal-correlated distortion at the device word length —
                // audible as a dirty floor on quiet material and fade tails.
                // Integer targets are rounded with TPDF dither at their own
                // resolution (24 valid bits for 32-bit words); floats pass
                // through. See `dsp::dither`.
                for (dst, src) in data.iter_mut().zip(scratch.iter()) {
                    *dst = T::dithered_from_f32(*src, &mut dither);
                }
                let _ = mmcss_set; // suppress unused (non-windows)
            },
            move |err| {
                glitch_counter.fetch_add(1, Ordering::Relaxed);
                match err {
                    // Device vanished mid-stream (unplug / default-device
                    // change): flag it so the control thread can surface
                    // DeviceLost and attempt recovery.
                    cpal::StreamError::DeviceNotAvailable => {
                        eprintln!("[DAUx cpal] Stream error: {err}");
                        err_shared.device_lost.store(true, Ordering::Relaxed);
                    }
                    // The vendored ALSA backend detects the driver-level xrun
                    // itself (recovering via `snd_pcm_prepare`/`try_recover`)
                    // and reports it here instead of swallowing it — this is
                    // the only place a real ALSA underrun becomes visible to
                    // the UI, distinct from `glitch_counter`'s broader
                    // "something clipped a block" signal.
                    cpal::StreamError::Underrun => {
                        err_shared.device_xruns.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => eprintln!("[DAUx cpal] Stream error: {err}"),
                }
            },
            None,
        )
        .map_err(|e| e.to_string())?;

    Ok(stream)
}

// ── Realtime scheduling helper (Linux only) ──────────────────────────────────

/// Promote cpal's ALSA callback thread out of `SCHED_OTHER`.
///
/// Windows claims MMCSS "Pro Audio" and macOS joins a CoreAudio workgroup, but
/// cpal's ALSA backend spawns `cpal_alsa_out` as an ordinary thread
/// (`external/cpal/src/host/alsa/mod.rs`). At default priority the callback
/// competes with the GPUI compositor, CEF plugin editors and the plugin
/// scanner, which is the dominant source of Linux dropouts.
///
/// Deliberately not gated on `DauxDeviceConfig::mmcss_priority`: that flag is
/// Windows-specific and is hardcoded false on the native path
/// (`native.rs`). Realtime scheduling is what an audio callback needs
/// regardless, and every step here degrades gracefully when the user has no
/// realtime budget.
#[cfg(target_os = "linux")]
mod rt_priority {
    use std::time::Duration;

    use crossbeam_channel::{bounded, Sender};

    /// `SCHED_FIFO` priorities to try directly, most preferred first.
    ///
    /// 76 clears the GPUI Linux dispatcher's realtime threads (priority 65 in
    /// `gpui_linux::dispatcher::spawn_realtime`) so UI work can never preempt
    /// audio. The lower rungs cover partially-provisioned systems.
    ///
    /// Kernel RT throttling (`sched_rt_runtime_us`, 95% by default) keeps a
    /// runaway callback from locking the machine.
    const FIFO_PRIORITIES: [libc::c_int; 4] = [76, 66, 20, 5];

    /// Nice value requested when no realtime budget is available at all.
    const FALLBACK_NICE: libc::c_int = -10;

    /// How long the helper waits for the stream to produce its first callback
    /// before giving up, so a stream that never starts cannot leak the thread.
    const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Handle the audio thread uses to announce itself for promotion.
    pub struct Announcer(Sender<libc::pid_t>);

    impl Announcer {
        /// Realtime-safe: one `gettid` syscall plus a non-blocking send into a
        /// preallocated single-slot channel. Every part that can block — the
        /// scheduler syscalls and the RealtimeKit D-Bus round trip — runs on
        /// the helper thread, never here.
        pub fn announce_current_thread(&self) {
            // SAFETY: `SYS_gettid` takes no arguments and cannot fail.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
            let _ = self.0.try_send(tid);
        }
    }

    /// Spawn the helper that promotes the audio thread once it announces its
    /// tid. Returns immediately; the helper exits after a single attempt.
    ///
    /// The RealtimeKit D-Bus connection is opened here, before the helper
    /// blocks waiting for the announce — not inside `rtkit::promote` — so the
    /// (single-digit-to-tens-of-ms) connect handshake happens during stream
    /// setup instead of racing the audio thread's first few callbacks. On a
    /// stock desktop (`RLIMIT_RTPRIO` 0) RealtimeKit is the only promotion
    /// route, and until it completes the callback thread is plain
    /// `SCHED_OTHER`, preemptable by GPUI's own `SCHED_FIFO` compositor
    /// thread — exactly the window this closes.
    pub fn spawn_promoter() -> Announcer {
        let (tx, rx) = bounded(1);
        let spawned = std::thread::Builder::new()
            .name("daux-rt-promote".into())
            .spawn(move || {
                let prewarmed_bus = rtkit::connect_system_bus();
                if let Ok(tid) = rx.recv_timeout(ANNOUNCE_TIMEOUT) {
                    let _ = promote(tid, prewarmed_bus);
                }
            });
        if let Err(e) = &spawned {
            eprintln!("[DAUx] could not spawn realtime promoter: {e}");
        }
        Announcer(tx)
    }

    /// What the promotion actually achieved.
    #[derive(Debug, PartialEq, Eq)]
    pub enum Outcome {
        /// Direct `sched_setscheduler`, using our own `RLIMIT_RTPRIO` budget.
        Fifo(libc::c_int),
        /// Granted by RealtimeKit, which always hands out `SCHED_RR`.
        RoundRobin(i32),
        /// No realtime budget anywhere; settled for a better nice value.
        Nice(libc::c_int),
        /// Still `SCHED_OTHER` — dropouts are expected.
        Unchanged,
    }

    fn promote(tid: libc::pid_t, prewarmed_bus: Option<zbus::blocking::Connection>) -> Outcome {
        if let Some(priority) = try_sched_fifo(tid) {
            eprintln!("[DAUx] audio thread promoted to SCHED_FIFO priority {priority}");
            return Outcome::Fifo(priority);
        }
        // Expected on a stock desktop: RLIMIT_RTPRIO is 0 there, so the direct
        // syscall above is always refused and RealtimeKit is the only route.
        match rtkit::promote(tid, prewarmed_bus) {
            Ok(priority) => {
                eprintln!(
                    "[DAUx] audio thread promoted to SCHED_RR priority {priority} \
                     via RealtimeKit"
                );
                return Outcome::RoundRobin(priority);
            }
            Err(e) => eprintln!("[DAUx] RealtimeKit promotion unavailable: {e}"),
        }
        match try_nice(tid) {
            Some(nice) => {
                eprintln!(
                    "[DAUx] no realtime scheduling available; audio thread running at nice {nice}"
                );
                Outcome::Nice(nice)
            }
            None => {
                eprintln!(
                    "[DAUx] audio thread stuck at default priority — expect dropouts. \
                     Install the 'realtime-privileges' package and join the 'realtime' \
                     group, or launch from a desktop session so RealtimeKit can grant it."
                );
                Outcome::Unchanged
            }
        }
    }

    /// Returns the priority that was accepted, if any.
    ///
    /// Uses `sched_setscheduler` rather than `pthread_setschedparam` so the
    /// helper thread can promote the audio thread by tid without running any
    /// of this on the audio thread itself.
    fn try_sched_fifo(tid: libc::pid_t) -> Option<libc::c_int> {
        for priority in FIFO_PRIORITIES {
            // SAFETY: every `sched_param` member is valid zero-initialized.
            let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
            param.sched_priority = priority;
            // SAFETY: `tid` names a live thread and `param` is initialized.
            let rc = unsafe { libc::sched_setscheduler(tid, libc::SCHED_FIFO, &param) };
            if rc == 0 {
                return Some(priority);
            }
        }
        None
    }

    /// Returns the nice value that was accepted, if any.
    fn try_nice(tid: libc::pid_t) -> Option<libc::c_int> {
        // Linux departs from POSIX here: `PRIO_PROCESS` with a *thread* id
        // applies to that thread alone, which is what we want — the control
        // thread must keep its default niceness.
        // SAFETY: `tid` names a live thread.
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, FALLBACK_NICE) };
        (rc == 0).then_some(FALLBACK_NICE)
    }

    /// RealtimeKit hands out `SCHED_FIFO` to unprivileged processes over the
    /// system bus. It is what a stock PipeWire desktop ships instead of a
    /// per-user `RLIMIT_RTPRIO`, and is how PipeWire itself gets realtime.
    mod rtkit {
        const SERVICE: &str = "org.freedesktop.RealtimeKit1";
        const PATH: &str = "/org/freedesktop/RealtimeKit1";

        /// RealtimeKit refuses callers that leave `RLIMIT_RTTIME` unlimited —
        /// the limit is what bounds a runaway realtime thread. Used only when
        /// the daemon advertises no maximum of its own.
        const FALLBACK_RTTIME_US: u64 = 200_000;

        /// Opened by `spawn_promoter` before it waits for the tid announce, so
        /// the handshake is off the promotion critical path. Falls back to a
        /// fresh connection in `promote` if this failed (daemon/bus not up
        /// yet at helper-thread start) — same behavior as before, just no
        /// longer the common case.
        pub fn connect_system_bus() -> Option<zbus::blocking::Connection> {
            zbus::blocking::Connection::system().ok()
        }

        pub fn promote(
            tid: libc::pid_t,
            prewarmed_bus: Option<zbus::blocking::Connection>,
        ) -> Result<i32, String> {
            let conn = match prewarmed_bus {
                Some(conn) => conn,
                None => zbus::blocking::Connection::system()
                    .map_err(|e| format!("system bus unavailable: {e}"))?,
            };
            let proxy = zbus::blocking::Proxy::new(&conn, SERVICE, PATH, SERVICE)
                .map_err(|e| format!("proxy setup failed: {e}"))?;

            // The daemon caps what it will grant; asking above the cap is a
            // hard error rather than a clamp, so read it first.
            let max_priority: i32 = proxy
                .get_property("MaxRealtimePriority")
                .map_err(|e| format!("daemon not reachable: {e}"))?;
            if max_priority < 1 {
                return Err("daemon grants no realtime priority".into());
            }
            let rttime_max: i64 = proxy.get_property("RTTimeUSecMax").unwrap_or(0);

            set_rttime_limit(if rttime_max > 0 {
                rttime_max as u64
            } else {
                FALLBACK_RTTIME_US
            })?;

            // SAFETY: always safe to call.
            let pid = unsafe { libc::getpid() } as u64;
            proxy
                .call_method(
                    "MakeThreadRealtimeWithPID",
                    &(pid, tid as u64, max_priority as u32),
                )
                .map_err(|e| format!("MakeThreadRealtimeWithPID failed: {e}"))?;
            Ok(max_priority)
        }

        /// `setrlimit` is process-wide on Linux, but `RLIMIT_RTTIME` only
        /// constrains threads that are actually `SCHED_FIFO`/`SCHED_RR`, so
        /// this does not affect the control or UI threads.
        fn set_rttime_limit(usec: u64) -> Result<(), String> {
            let limit = libc::rlimit {
                rlim_cur: usec,
                rlim_max: usec,
            };
            // SAFETY: `limit` is a fully initialized `rlimit`.
            let rc = unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &limit) };
            if rc == 0 {
                Ok(())
            } else {
                Err(format!(
                    "could not set RLIMIT_RTTIME: {}",
                    std::io::Error::last_os_error()
                ))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::Outcome;
        use crossbeam_channel::bounded;

        /// Whatever mechanism `promote` reports having used must be the policy
        /// the kernel actually reports for that thread afterwards.
        ///
        /// This is the assertion that matters: an earlier draft claimed
        /// `SCHED_FIFO` for the RealtimeKit path, but RealtimeKit only ever
        /// hands out `SCHED_RR`.
        ///
        /// `Nice`/`Unchanged` are real outcomes on an unprovisioned machine
        /// (no `RLIMIT_RTPRIO`, and RealtimeKit's polkit rules grant nothing to
        /// a process outside a logind session), so they are reported rather
        /// than failed — there is nothing to promote *with* in that case.
        #[test]
        fn reported_outcome_matches_the_kernel_policy() {
            // Keep a worker alive so there is a real, running tid to promote.
            let (release_tx, release_rx) = bounded::<()>(0);
            let (ready_tx, ready_rx) = bounded(1);
            let worker = std::thread::spawn(move || {
                // SAFETY: `SYS_gettid` takes no arguments and cannot fail.
                let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
                ready_tx.send(tid).expect("test receiver is alive");
                let _ = release_rx.recv();
            });
            let tid = ready_rx.recv().expect("worker should report its tid");

            let outcome = super::promote(tid, super::rtkit::connect_system_bus());
            // SAFETY: `tid` names the still-parked worker thread.
            let policy = unsafe { libc::sched_getscheduler(tid) };

            let expected = match outcome {
                Outcome::Fifo(_) => Some(libc::SCHED_FIFO),
                Outcome::RoundRobin(_) => Some(libc::SCHED_RR),
                Outcome::Nice(_) | Outcome::Unchanged => Some(libc::SCHED_OTHER),
            };
            assert_eq!(
                Some(policy),
                expected,
                "promote() reported {outcome:?} but the kernel reports policy {policy}"
            );

            if matches!(outcome, Outcome::Nice(_) | Outcome::Unchanged) {
                eprintln!(
                    "note: this machine grants no realtime scheduling ({outcome:?}); \
                     the promotion path itself was still exercised"
                );
            }

            drop(release_tx);
            worker.join().expect("worker thread panicked");
        }
    }
}

// ── MMCSS helper (Windows only) ───────────────────────────────────────────────

/// Set MMCSS "Pro Audio" priority on the calling thread.
/// Returns true on success.  Called once per audio thread on first callback.
#[cfg(target_os = "windows")]
fn set_mmcss_pro_audio() -> bool {
    // Use raw extern declaration to avoid windows crate feature-flag issues.
    #[link(name = "avrt")]
    extern "system" {
        fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
    }

    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut task_index = 0u32;
    unsafe {
        let handle = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index);
        let ok = handle != 0;
        if ok {
            eprintln!("[DAUx] MMCSS 'Pro Audio' priority set (index={task_index})");
        } else {
            eprintln!("[DAUx] MMCSS set failed (may require elevated privileges)");
        }
        ok
    }
}

#[cfg(test)]
mod buffer_size_tests {
    use super::period_candidates;

    /// The reported buffer size must be the block the callback actually
    /// receives, not the value handed to cpal — those differ on ALSA.
    #[test]
    fn reported_size_is_the_callback_block() {
        for (_, fixed, callback) in period_candidates(256) {
            assert!(fixed > 0 && callback > 0, "no candidate may report zero");
            #[cfg(target_os = "linux")]
            assert_eq!(
                callback,
                fixed / super::ALSA_PERIODS_PER_BUFFER,
                "cpal derives the ALSA period as a quarter of the ring"
            );
            #[cfg(not(target_os = "linux"))]
            assert_eq!(callback, fixed, "the two agree off ALSA");
        }
    }

    /// The preferred ALSA candidate must ask for a ring four times the
    /// requested period, so the callback block comes back at the requested
    /// size instead of a quarter of it. This is the actual stutter fix.
    ///
    /// Verified against real hardware: requesting 256 yields a 64-frame period
    /// and underruns on an idle callback, while requesting 1024 yields the
    /// 256-frame period we actually want, with none.
    #[cfg(target_os = "linux")]
    #[test]
    fn preferred_alsa_candidate_preserves_the_requested_period() {
        let (_, fixed, callback) = period_candidates(256)[0];
        assert_eq!(fixed, 1024, "ring should hold four 256-frame periods");
        assert_eq!(callback, 256, "callback block should match the request");
    }

    /// A callback can be handed the whole ring, and `request_block` clamps at
    /// `MAX_BRIDGE_BLOCK_FRAMES`, so no ring may exceed it — otherwise bridged
    /// plugin audio loses the tail of every late block.
    #[cfg(target_os = "linux")]
    #[test]
    fn ring_never_exceeds_what_the_plugin_bridge_can_carry() {
        let max = crate::plugin_bridge::MAX_BRIDGE_BLOCK_FRAMES as u32;
        for period in [32, 64, 128, 256, 512, 1024, 2048, 4096] {
            for (label, fixed, _) in period_candidates(period) {
                assert!(
                    fixed <= max.max(period),
                    "{label}: ring {fixed} exceeds the {max}-frame bridge block \
                     for a {period}-frame period"
                );
            }
        }
    }

    /// Large periods must still gain whatever headroom fits rather than being
    /// forced back to a single-period ring.
    #[cfg(target_os = "linux")]
    #[test]
    fn large_periods_keep_the_headroom_that_fits() {
        assert_eq!(period_candidates(512)[0].1, 2048, "512 fits four periods");
        assert_eq!(period_candidates(1024)[0].1, 2048, "1024 fits two");
        assert_eq!(period_candidates(2048)[0].1, 2048, "2048 fits one");
    }

    /// A bare-ring retry must still be offered, since `set_buffer_size` is an
    /// exact match on ALSA and the larger ring is often refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn alsa_falls_back_before_surrendering_to_the_device_default() {
        let candidates = period_candidates(256);
        assert_eq!(
            candidates.len(),
            3,
            "a 2-period and a bare retry must exist"
        );
        assert_eq!(candidates[2].1, 256, "final retry asks for the bare value");
    }

    /// The fallback between the preferred 4-period ring and the bare ring
    /// must not jump straight from a full-size callback to a quarter-size
    /// one: a 2-period ring (half-size callback) belongs in between.
    #[cfg(target_os = "linux")]
    #[test]
    fn middle_candidate_only_halves_the_callback() {
        let candidates = period_candidates(256);
        let (_, mid_fixed, mid_callback) = candidates[1];
        assert_eq!(mid_fixed, 512, "2-period ring holds two 256-frame periods");
        assert_eq!(mid_callback, 128, "callback halves instead of quartering");

        // When the preferred ring already has 2 or fewer periods (large
        // requests), the 2-period middle candidate would duplicate it, so it
        // must be omitted rather than repeated.
        assert_eq!(
            period_candidates(1024).len(),
            2,
            "no middle candidate when the preferred ring already has 2 periods"
        );
        assert_eq!(
            period_candidates(2048).len(),
            2,
            "no middle candidate when the preferred ring already has 1 period"
        );
    }

    /// Small periods must not report a zero-frame callback block, which would
    /// zero out the reported latency.
    #[test]
    fn tiny_periods_never_report_zero() {
        for (_, _, callback) in period_candidates(2) {
            assert!(callback >= 1, "callback frames must stay positive");
        }
    }
}
