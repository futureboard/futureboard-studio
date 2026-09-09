//! Full-duplex ASIO session (Windows, Professional Edition builds only).
//!
//! One ASIO driver is one duplex device with one buffer set and one
//! buffer-switch lifecycle. This module opens the selected driver **once**,
//! builds the input and output streams from the **same** device instance (so
//! CPAL prepares one union buffer set instead of disposing the other side's
//! buffers), and keeps both running for the life of the session:
//!
//! ```text
//! ASIO buffer switch
//!   ├─ input callback  → monitor ring + meters + preview bins + record sink
//!   └─ output callback → drain_commands + fill_output_f32 (normal DAW graph)
//! ```
//!
//! Everything that used to require opening a second stream — selecting a track
//! input, enabling monitoring, arming, starting a take — is now either an
//! atomic store (`SharedState` monitor-source pair, ring active flags) or a
//! bounded command to the input callback ([`AsioInputCommand`]). The driver
//! and its buffers are never touched after open, which is what keeps playback
//! running while inputs are used.
//!
//! Realtime rules in the input callback: atomics, a bounded `try_recv` command
//! drain, and preallocated block-pool sends only. Record sinks stay boxed while
//! moving through the callback and trash channel, then are dropped on the
//! control thread. A fixed retirement slot backpressures command draining if
//! that channel is full or disconnected.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::platform::{AsioDevice, AsioDriverEvent, AsioDriverEventGuard};
use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::backend::cpal_backend::build_typed_stream;
use crate::backend::{AsioChannelInfo, AsioSessionCaps, DauxDeviceConfig};
use crate::command::EngineCommand;
use crate::engine::{f32_store, SharedState};
use crate::error::SphereAudioError;
use crate::runtime::RuntimeProject;

/// Commands for the persistent input callback. Bounded and lock-free; the
/// callback drains them at block start.
pub enum AsioInputCommand {
    /// Install the record fan-out for a take. Ownership stays boxed so neither
    /// installation nor retirement deallocates on the callback.
    SetRecordSink(Box<RecordSink>),
    /// Remove the record fan-out (take stopped/cancelled).
    ClearRecordSink,
}

/// Everything the input callback needs to feed one recording take. Created by
/// `start_recording`, carried into the callback via [`AsioInputCommand`], and
/// returned boxed through the trash channel when replaced/cleared so the
/// `Sender` disconnect (which finalizes the disk writer) and the box
/// deallocation both happen off the audio thread.
pub struct RecordSink {
    pub audio_tx: Sender<Vec<i32>>,
    pub free_rx: Receiver<Vec<i32>>,
    pub free_tx: Sender<Vec<i32>>,
    /// Preview-waveform bin width in frames (from the take's sample rate).
    pub samples_per_bin: usize,
    /// Gate take-only paths on the transport while monitoring remains live.
    pub capture_on_transport: bool,
    /// Blocks dropped because the pool/queue was exhausted (shared with the
    /// recording session for the end-of-take report).
    pub dropped_blocks: Arc<AtomicU64>,
    /// One accumulator per armed track, in `shared.recording_preview_track_ids`
    /// order, so the callback can fill `shared.preview_rings[slot]` — the rings
    /// the UI actually drains for the growing waveform.
    ///
    /// It lives on the sink rather than in the callback because the callback is
    /// built when the ASIO session opens, long before anyone knows which tracks
    /// a take will arm. Allocated once on the control thread and only mutated
    /// in place afterwards, so the realtime path still never allocates.
    pub preview_accums: Vec<PreviewAccum>,
}

/// Per-track preview bin accumulator. Mirrors the own-stream path's private
/// accumulator in `recording.rs`; it is public here only because the sink that
/// owns it crosses the module boundary.
pub struct PreviewAccum {
    /// Input channel indices this track records, as the disk writer sees them —
    /// not the global monitor pair, or every track's waveform would be the same
    /// picture.
    pub l_ch: usize,
    pub r_ch: usize,
    pub bin_min: f32,
    pub bin_max: f32,
    pub bin_sumsq: f32,
    pub bin_count: usize,
}

impl PreviewAccum {
    pub fn new(l_ch: usize, r_ch: usize) -> Self {
        Self {
            l_ch,
            r_ch,
            bin_min: f32::MAX,
            bin_max: f32::MIN,
            bin_sumsq: 0.0,
            bin_count: 0,
        }
    }

    #[inline]
    fn reset(&mut self) {
        self.bin_min = f32::MAX;
        self.bin_max = f32::MIN;
        self.bin_sumsq = 0.0;
        self.bin_count = 0;
    }
}

/// Per-hardware-channel input peaks. Storage is allocated when the session
/// opens; the ASIO callback only updates atomics from a preallocated scratch
/// block, and the control thread consumes/reset peaks for track meters.
pub struct AsioInputMeterBank {
    peaks: Box<[std::sync::atomic::AtomicU32]>,
}

impl AsioInputMeterBank {
    fn new(channels: usize) -> Self {
        Self {
            peaks: (0..channels)
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }

    fn publish(&self, block_peaks: &[f32]) {
        for (peak, value) in self.peaks.iter().zip(block_peaks.iter().copied()) {
            let mut current = peak.load(Ordering::Relaxed);
            loop {
                let current_value = f32::from_bits(current);
                if current_value >= value {
                    break;
                }
                match peak.compare_exchange_weak(
                    current,
                    value.to_bits(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(next) => current = next,
                }
            }
        }
    }

    pub fn take_peaks(&self) -> Vec<f32> {
        self.peaks
            .iter()
            .map(|peak| f32::from_bits(peak.swap(0, Ordering::Relaxed)))
            .collect()
    }
}

/// Driver lifecycle notifications, converted to flags the control thread
/// polls. Duplicate reset requests coalesce into one pending flag.
#[derive(Default)]
pub struct AsioDriverEventFlags {
    pub reset_requested: AtomicBool,
    pub latencies_changed: AtomicBool,
    pub resync_count: AtomicU64,
}

/// The open duplex session. Dropping it tears the session down in order:
/// streams first (their `Drop` deregisters the buffer callbacks), then the
/// device — whose last driver handle runs `ASIOStop`/`ASIODisposeBuffers`/
/// `ASIOExit` and fully unloads the driver.
pub struct AsioDuplexHandle {
    // Field order is teardown order. Deregister callbacks before disconnecting
    // any channel endpoint they capture; keep the driver device until last.
    input_stream: Option<cpal::Stream>,
    output_stream: cpal::Stream,
    _event_guard: AsioDriverEventGuard,
    pub cmd_tx: Sender<EngineCommand>,
    pub input_cmd_tx: Sender<AsioInputCommand>,
    /// Control-thread side of record-sink disposal (see [`RecordSink`]).
    pub trash_rx: Receiver<Box<RecordSink>>,
    pub input_meters: Arc<AsioInputMeterBank>,
    pub events: Arc<AsioDriverEventFlags>,
    pub caps: AsioSessionCaps,
    pub device_name: String,
    pub sample_rate: u32,
    pub buffer_size: u32,
    /// Set when the driver has inputs but the input stream could not open —
    /// playback proceeds, and this is surfaced as a status-bar diagnostic.
    pub input_warning: Option<String>,
    device: AsioDevice,
}

// Safety: same contract as `CpalStreamHandle` — the handle lives inside
// `EngineInner` behind a parking_lot Mutex and is only touched from the
// control thread. The streams' callbacks communicate exclusively through
// `Arc<SharedState>` atomics and bounded channels.
unsafe impl Send for AsioDuplexHandle {}
unsafe impl Sync for AsioDuplexHandle {}

impl AsioDuplexHandle {
    /// Resume the output side (transport-level start). The input side always
    /// runs so meters and recording work while the engine is "stopped".
    pub fn play(&self) -> Result<(), String> {
        self.output_stream.play().map_err(|e| e.to_string())
    }

    /// Pause the output side. The paused CPAL callback writes silence into the
    /// hardware buffers; input keeps running.
    pub fn pause(&self) -> Result<(), String> {
        self.output_stream.pause().map_err(|e| e.to_string())
    }

    pub fn has_input(&self) -> bool {
        self.input_stream.is_some()
    }

    /// Open the driver's control panel (control thread only).
    pub fn open_control_panel(&self) -> Result<(), String> {
        self.device.asio_open_control_panel()
    }

    /// Re-query driver latencies (e.g. after a `LatenciesChanged` message).
    pub fn refresh_latencies(&self) -> Option<(u32, u32)> {
        self.device
            .asio_latencies()
            .map(|latencies| (latencies.input, latencies.output))
    }

    /// Consume a pending driver reset request (coalesced).
    pub fn take_reset_request(&self) -> bool {
        self.events.reset_requested.swap(false, Ordering::SeqCst)
    }

    pub fn take_latencies_changed(&self) -> bool {
        self.events.latencies_changed.swap(false, Ordering::SeqCst)
    }

    /// Drain record sinks the input callback discarded, dropping them here on
    /// the control thread (disconnects their disk writers).
    pub fn drain_trashed_sinks(&self) -> usize {
        let mut drained = 0;
        while self.trash_rx.try_recv().is_ok() {
            drained += 1;
        }
        drained
    }

    pub fn take_input_channel_peaks(&self) -> Vec<f32> {
        self.input_meters.take_peaks()
    }
}

/// Snap a requested buffer size into the driver's constraints: clamp to
/// `[min, max]`, then honour granularity (`-1` = powers of two, `> 0` = steps
/// from `min`). `None` picks the driver-preferred size.
pub(crate) fn snap_buffer_size(
    requested: Option<u32>,
    info: &cpal::platform::AsioBufferSizeInfo,
) -> u32 {
    let preferred = info.preferred.max(1);
    let Some(requested) = requested.filter(|&frames| frames > 0) else {
        return preferred;
    };
    let min = info.min.max(1);
    let max = info.max.max(min);
    let clamped = requested.clamp(min, max);
    match info.granularity {
        0 => preferred,
        -1 => {
            // Powers of two between min and max, choosing the nearest.
            let mut best = min.next_power_of_two().clamp(min, max);
            let mut candidate = best;
            while candidate <= max {
                if candidate.abs_diff(clamped) < best.abs_diff(clamped) {
                    best = candidate;
                }
                match candidate.checked_mul(2) {
                    Some(next) => candidate = next,
                    None => break,
                }
            }
            best
        }
        granularity if granularity > 0 => {
            let step = granularity as u32;
            let offset = clamped.saturating_sub(min);
            let snapped = min + (offset + step / 2) / step * step;
            snapped.clamp(min, max)
        }
        _ => clamped,
    }
}

/// How many output channels the session opens on the driver: every one of them,
/// not just the first pair.
///
/// The session already advertises the full output count in [`AsioSessionCaps`],
/// so the Audio Connections panel offers Out 3-4, Out 5-6 and the rest — but the
/// stream itself used to be capped at two channels. That put every such pair out
/// of range of `resolved_output_pair`, and the Control Room then declined to
/// write rather than write to the wrong place, so choosing an alternate monitor
/// pair on an ASIO interface produced no sound at all.
///
/// `reported` is the driver's own count from `asio_channel_counts`;
/// `default_config` is what cpal exposes on the default output config, and cpal
/// refuses to build a stream wider than that. A driver can disagree with itself
/// between the two, so take the smaller rather than failing an open over a count
/// nobody asked for.
fn openable_output_channels(reported: u32, default_config: u16) -> u16 {
    reported
        .min(u32::from(default_config))
        .min(u32::from(u16::MAX)) as u16
}

/// Open the full-duplex session. Control thread only; the engine must have
/// closed any previous stream first (the driver loader refuses to open a
/// second session while one is active).
pub(crate) fn open_duplex(
    host: &cpal::Host,
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<AsioDuplexHandle, SphereAudioError> {
    let fail = |stage: &str, detail: String| {
        SphereAudioError::StreamOpenFailed(format!("ASIO [{stage}]: {detail}"))
    };

    let asio_host = host.as_asio().ok_or_else(|| {
        fail(
            "host",
            "internal error: non-ASIO host passed to the ASIO session opener".into(),
        )
    })?;

    // ── Driver + capability queries ───────────────────────────────────────
    let device = asio_host
        .asio_device_by_name(config.output_device_id.as_deref())
        .map_err(|error| fail("driver-load", error))?;
    let driver_name = device
        .name()
        .unwrap_or_else(|_| "Unknown ASIO driver".to_string());

    let (in_channels, out_channels) = device
        .asio_channel_counts()
        .map_err(|error| fail("channel-query", error))?;
    if out_channels == 0 {
        return Err(fail(
            "channel-query",
            format!("driver '{driver_name}' reports no output channels"),
        ));
    }

    let buffer_info = device
        .asio_buffer_size_info()
        .map_err(|error| fail("buffer-query", error))?;
    let requested_buffer = config.buffer_size.filter(|&frames| frames > 0);
    let buffer_frames = snap_buffer_size(requested_buffer, &buffer_info);
    if let Some(requested) = requested_buffer {
        if requested != buffer_frames {
            eprintln!(
                "[DAUx ASIO] requested buffer {requested} is outside driver constraints \
                 (min={} max={} preferred={} granularity={}); using {buffer_frames}",
                buffer_info.min, buffer_info.max, buffer_info.preferred, buffer_info.granularity
            );
        }
    }

    let driver_rate = device
        .asio_current_sample_rate()
        .map_err(|error| fail("sample-rate-query", error))?;
    let sample_rate = config
        .sample_rate
        .filter(|&rate| rate > 0)
        .unwrap_or(driver_rate);

    let default_output = device.default_output_config().map_err(|error| {
        fail(
            "format-query",
            format!("driver '{driver_name}' output format unsupported: {error}"),
        )
    })?;
    let output_format = default_output.sample_format();

    let stream_output_channels = openable_output_channels(out_channels, default_output.channels());
    if stream_output_channels == 0 {
        return Err(fail(
            "channel-query",
            format!("driver '{driver_name}' offers no usable output channels"),
        ));
    }
    if u32::from(stream_output_channels) != out_channels {
        eprintln!(
            "[DAUx ASIO] driver '{driver_name}' reports {out_channels} outputs but its \
             default config exposes {}; opening {stream_output_channels}",
            default_output.channels()
        );
    }

    // ── Streams (input first, then output; both from the same device) ─────
    //
    // CPAL's second build re-creates the ASIO buffers as the union of both
    // directions; passing identical rate/size keeps the two sides coherent.
    shared.sample_rate.store(sample_rate, Ordering::Relaxed);
    let (cmd_tx, cmd_rx) = bounded::<EngineCommand>(512);
    let (input_cmd_tx, input_cmd_rx) = bounded::<AsioInputCommand>(8);
    let (trash_tx, trash_rx) = bounded::<Box<RecordSink>>(8);
    let input_meters = Arc::new(AsioInputMeterBank::new(in_channels as usize));

    let platform_device: cpal::Device = device.clone().into();

    let mut input_warning = None;
    let mut input_sample_format = None;
    let input_stream = if in_channels > 0 {
        let input_result = device
            .default_input_config()
            .map_err(|error| format!("input format unsupported: {error}"))
            .and_then(|input_config| {
                let sample_format = input_config.sample_format();
                input_sample_format = Some(sample_format);
                let stream_config = cpal::StreamConfig {
                    channels: in_channels.min(u16::MAX as u32) as u16,
                    sample_rate: cpal::SampleRate(sample_rate),
                    buffer_size: cpal::BufferSize::Fixed(buffer_frames),
                };
                build_input_fanout_stream(
                    &platform_device,
                    &stream_config,
                    sample_format,
                    Arc::clone(&shared),
                    Arc::clone(&input_meters),
                    input_cmd_rx,
                    trash_tx,
                )
            });
        match input_result {
            Ok(stream) => Some(stream),
            Err(error) => {
                // Playback must not die because inputs are unavailable, but the
                // degradation is loud: status-bar diagnostic + recording will
                // refuse to start with the same reason.
                let warning = format!(
                    "ASIO input unavailable on '{driver_name}' \
                     ({in_channels} channel(s) reported): {error}"
                );
                eprintln!("[DAUx ASIO] {warning}");
                input_warning = Some(warning);
                None
            }
        }
    } else {
        None
    };

    let output_config = cpal::StreamConfig {
        channels: stream_output_channels,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Fixed(buffer_frames),
    };
    let output_stream = build_typed_stream(
        &platform_device,
        &output_config,
        output_format,
        cmd_rx,
        Arc::clone(&shared),
        initial_runtime,
        glitch_counter,
        config.mmcss_priority,
    )
    .map_err(|error| fail("output-open", error))?;

    // Input runs for the life of the session (meters/recording work while the
    // transport is stopped); output starts paused until `EngineInner::start`.
    if let Some(stream) = &input_stream {
        stream
            .play()
            .map_err(|error| fail("input-start", error.to_string()))?;
    }

    // ── Post-open queries (valid once buffers exist) ──────────────────────
    let (input_latency, output_latency) = device
        .asio_latencies()
        .map(|latencies| (latencies.input, latencies.output))
        .unwrap_or((0, 0));

    let effective_in_channels = if input_stream.is_some() {
        in_channels
    } else {
        0
    };
    let channel_info = |is_input: bool,
                        count: u32,
                        sample_format: Option<cpal::SampleFormat>|
     -> Vec<AsioChannelInfo> {
        let mut channels: Vec<_> = (0..count)
            .map(|index| {
                let description = device.asio_channel_description(is_input, index);
                let name = description
                    .as_ref()
                    .map(|desc| desc.name.trim())
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!(
                            "{} {}",
                            if is_input { "Input" } else { "Output" },
                            index + 1
                        )
                    });
                AsioChannelInfo {
                    index,
                    name,
                    active: description.map(|desc| desc.is_active).unwrap_or(true),
                    sample_format: sample_format
                        .map(|format| format!("{format:?}"))
                        .unwrap_or_else(|| "Unknown".to_string()),
                    stereo_pair: None,
                }
            })
            .collect();
        for pair_start in (0..channels.len()).step_by(2) {
            let Some(pair_end) = pair_start.checked_add(1) else {
                break;
            };
            if pair_end < channels.len() && channels[pair_start].active && channels[pair_end].active
            {
                let pair = (pair_start / 2) as u32;
                channels[pair_start].stereo_pair = Some(pair);
                channels[pair_end].stereo_pair = Some(pair);
            }
        }
        channels
    };

    let caps = AsioSessionCaps {
        // `Device::name()` is driver-defined and some ASIO implementations
        // return a friendly name that differs from the asiolist id used to open
        // them. Preserve the requested id so capability inventory can attach
        // these channels to the same stable device exposed in Settings.
        driver: config
            .output_device_id
            .clone()
            .unwrap_or_else(|| driver_name.clone()),
        sample_rate,
        buffer_size: buffer_frames,
        input_channels: effective_in_channels,
        output_channels: out_channels,
        input_channel_info: channel_info(true, effective_in_channels, input_sample_format),
        output_channel_info: channel_info(false, out_channels, Some(output_format)),
        input_latency_samples: input_latency,
        output_latency_samples: output_latency,
    };

    // ── Driver lifecycle messages → coalesced control flags ───────────────
    let events = Arc::new(AsioDriverEventFlags::default());
    let event_flags = Arc::clone(&events);
    let event_guard = device.asio_on_driver_event(move |event| match event {
        AsioDriverEvent::ResetRequest => {
            event_flags.reset_requested.store(true, Ordering::SeqCst);
        }
        AsioDriverEvent::LatenciesChanged => {
            event_flags.latencies_changed.store(true, Ordering::SeqCst);
        }
        AsioDriverEvent::ResyncRequest => {
            event_flags.resync_count.fetch_add(1, Ordering::SeqCst);
        }
        AsioDriverEvent::Other => {}
    });

    eprintln!(
        "[DAUx ASIO] session open: driver='{driver_name}' sr={sample_rate} buffer={buffer_frames} \
         in={effective_in_channels} out={stream_output_channels}/{out_channels} \
         latency_in={input_latency} latency_out={output_latency}"
    );

    Ok(AsioDuplexHandle {
        input_stream,
        output_stream,
        _event_guard: event_guard,
        cmd_tx,
        input_cmd_tx,
        trash_rx,
        input_meters,
        events,
        caps,
        device_name: driver_name,
        sample_rate,
        buffer_size: buffer_frames,
        input_warning,
        device,
    })
}

// ── Input fan-out ─────────────────────────────────────────────────────────────

/// Native-format input sample: convertible to the monitor-path f32 and the
/// record-path full-scale i32 without allocation.
trait AsioInputSample: cpal::SizedSample + Copy + Send + 'static {
    fn to_monitor_f32(self) -> f32;
    fn to_record_i32(self) -> i32;
}

impl AsioInputSample for i16 {
    #[inline]
    fn to_monitor_f32(self) -> f32 {
        self as f32 / 32_768.0
    }
    #[inline]
    fn to_record_i32(self) -> i32 {
        (self as i32) << 16
    }
}

impl AsioInputSample for i32 {
    #[inline]
    fn to_monitor_f32(self) -> f32 {
        self as f32 / 2_147_483_648.0
    }
    #[inline]
    fn to_record_i32(self) -> i32 {
        self
    }
}

impl AsioInputSample for f32 {
    #[inline]
    fn to_monitor_f32(self) -> f32 {
        self
    }
    #[inline]
    fn to_record_i32(self) -> i32 {
        crate::recording::f32_to_s32(self)
    }
}

impl AsioInputSample for f64 {
    #[inline]
    fn to_monitor_f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn to_record_i32(self) -> i32 {
        crate::recording::f32_to_s32(self as f32)
    }
}

fn build_input_fanout_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    shared: Arc<SharedState>,
    input_meters: Arc<AsioInputMeterBank>,
    input_cmd_rx: Receiver<AsioInputCommand>,
    trash_tx: Sender<Box<RecordSink>>,
) -> Result<cpal::Stream, String> {
    match sample_format {
        cpal::SampleFormat::I16 => build_input_fanout_typed::<i16>(
            device,
            config,
            shared,
            input_meters,
            input_cmd_rx,
            trash_tx,
        ),
        cpal::SampleFormat::I32 => build_input_fanout_typed::<i32>(
            device,
            config,
            shared,
            input_meters,
            input_cmd_rx,
            trash_tx,
        ),
        cpal::SampleFormat::F32 => build_input_fanout_typed::<f32>(
            device,
            config,
            shared,
            input_meters,
            input_cmd_rx,
            trash_tx,
        ),
        cpal::SampleFormat::F64 => build_input_fanout_typed::<f64>(
            device,
            config,
            shared,
            input_meters,
            input_cmd_rx,
            trash_tx,
        ),
        other => Err(format!("unsupported ASIO input sample format {other}")),
    }
}

/// Callback-owned record sink plus one fixed retirement slot. If retirement
/// cannot reach the control thread, command draining pauses with every box
/// still owned by the active slot, retirement slot, or bounded command queue.
/// No sink or box is dropped/deallocated by [`drain_commands`].
#[derive(Default)]
struct CallbackRecordSinkState {
    active: Option<Box<RecordSink>>,
    pending_retirement: Option<Box<RecordSink>>,
}

impl CallbackRecordSinkState {
    fn active(&self) -> Option<&RecordSink> {
        self.active.as_deref()
    }

    /// The per-track preview accumulators of the live take, if there is one.
    /// Empty for a take that armed no track the preview can describe.
    fn preview_accums_mut(&mut self) -> &mut [PreviewAccum] {
        match self.active.as_deref_mut() {
            Some(sink) => sink.preview_accums.as_mut_slice(),
            None => &mut [],
        }
    }

    fn drain_commands(
        &mut self,
        input_cmd_rx: &Receiver<AsioInputCommand>,
        trash_tx: &Sender<Box<RecordSink>>,
    ) {
        if let Some(sink) = self.pending_retirement.take() {
            if let Err(error) = trash_tx.try_send(sink) {
                self.pending_retirement = Some(error.into_inner());
            }
        }

        while self.pending_retirement.is_none() {
            let Ok(command) = input_cmd_rx.try_recv() else {
                break;
            };
            let retired = match command {
                AsioInputCommand::SetRecordSink(sink) => self.active.replace(sink),
                AsioInputCommand::ClearRecordSink => self.active.take(),
            };
            if let Some(sink) = retired {
                if let Err(error) = trash_tx.try_send(sink) {
                    self.pending_retirement = Some(error.into_inner());
                }
            }
        }
    }
}

/// One persistent input callback, four fan-out paths (mirrors the recording
/// stream contract in `recording.rs`):
///   1. monitor  → `shared.input_ring` (drained by the output render callback)
///   2. meters   → peak atomics (+ `session_input_peak` for the Settings test)
///   3. preview  → min/max/rms bins → `shared.preview_ring` (the monitor mix)
///               and one ring per armed track in `shared.preview_rings`, which
///               is what the growing on-timeline waveform is drawn from
///   4. record   → preallocated block pool → bounded channel → disk writer
///
/// Realtime-safe: no allocation, no locks, no syscalls. Sink swaps arrive over
/// a bounded channel; discarded boxed sinks leave via `trash_tx` so their box
/// allocation and channel `Sender`s drop on the control thread.
fn build_input_fanout_typed<T: AsioInputSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<SharedState>,
    input_meters: Arc<AsioInputMeterBank>,
    input_cmd_rx: Receiver<AsioInputCommand>,
    trash_tx: Sender<Box<RecordSink>>,
) -> Result<cpal::Stream, String> {
    use crate::engine::atomic_max_f32_bits;
    use crate::input_ring::WaveformPeak;

    let channels = config.channels.max(1) as usize;

    let mut record_sink = CallbackRecordSinkState::default();
    // Preview accumulator — callback-local state, no allocation per block.
    let mut bin_min = f32::MAX;
    let mut bin_max = f32::MIN;
    let mut bin_sumsq = 0.0f32;
    let mut bin_count = 0usize;
    let mut channel_peaks = vec![0.0f32; channels];

    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _info| {
                // 1. Drain sink-swap commands (bounded, lock-free). A failed
                // retirement is retained locally and backpressures this drain.
                record_sink.drain_commands(&input_cmd_rx, &trash_tx);

                let frames = data.len() / channels;
                shared.input_cb_count.fetch_add(1, Ordering::Relaxed);
                shared
                    .input_frames_received
                    .fetch_add(frames as u64, Ordering::Relaxed);

                let (mon_l_ch, mon_r_ch) = shared.monitor_source_pair();
                let mon_l_ch = mon_l_ch as usize;
                let mon_r_ch = mon_r_ch as usize;
                let ring_active = shared.input_ring.is_active();
                let recording_active = shared.recording_active.load(Ordering::Relaxed);
                let transport_playing = shared.playing.load(Ordering::Relaxed);
                let capture_active = record_sink
                    .active()
                    .map(|sink| {
                        crate::recording::should_capture_recording(
                            recording_active,
                            sink.capture_on_transport,
                            transport_playing,
                        )
                    })
                    .unwrap_or(false);
                let preview_active =
                    capture_active && shared.recording_preview_active.load(Ordering::Relaxed);
                let samples_per_bin = record_sink
                    .active()
                    .map(|sink| sink.samples_per_bin)
                    .unwrap_or(0);
                // Borrowed for the whole frame loop below. Taken here, before
                // the loop, so the loop itself does nothing but arithmetic and
                // atomic stores.
                let preview_accums = record_sink.preview_accums_mut();

                let mut raw_peak_l = 0.0f32;
                let mut raw_peak_r = 0.0f32;
                let mut last_l = 0.0f32;
                let mut last_r = 0.0f32;
                let mut session_peak = 0.0f32;
                channel_peaks.fill(0.0);

                for frame in data.chunks(channels) {
                    let first = frame.first().copied().map(T::to_monitor_f32).unwrap_or(0.0);
                    // Unclamped: the monitor ring and the input meters carry
                    // what the driver delivered. A float ASIO driver can hand
                    // back samples past full scale, and clipping them here made
                    // both the monitor feed and the meter under-report it.
                    let l = frame
                        .get(mon_l_ch)
                        .copied()
                        .map(T::to_monitor_f32)
                        .unwrap_or(first);
                    let r = frame
                        .get(mon_r_ch)
                        .copied()
                        .map(T::to_monitor_f32)
                        .unwrap_or(l);
                    last_l = l;
                    last_r = r;
                    raw_peak_l = raw_peak_l.max(l.abs());
                    raw_peak_r = raw_peak_r.max(r.abs());

                    // Monitor bridge → output render callback. Gated on the
                    // ring's active flag so the consumer's cursor management
                    // matches the producer exactly.
                    if ring_active {
                        shared.input_ring.write_stereo(l, r);
                    }

                    // Preview bins for the in-progress take (mono mix of the
                    // monitored channels), same protocol as recording.rs.
                    if preview_active && samples_per_bin > 0 {
                        let m = (l + r) * 0.5;
                        bin_min = bin_min.min(m);
                        bin_max = bin_max.max(m);
                        bin_sumsq += m * m;
                        bin_count += 1;
                        if bin_count >= samples_per_bin {
                            let rms = (bin_sumsq / bin_count as f32).sqrt();
                            shared.preview_ring.push(WaveformPeak {
                                min: bin_min,
                                max: bin_max,
                                rms,
                            });
                            bin_min = f32::MAX;
                            bin_max = f32::MIN;
                            bin_sumsq = 0.0;
                            bin_count = 0;
                        }
                    } else {
                        bin_min = f32::MAX;
                        bin_max = f32::MIN;
                        bin_sumsq = 0.0;
                        bin_count = 0;
                    }

                    // Per-track preview bins: each armed track gets a mono mix
                    // of *its own* input channels, matching what the disk
                    // writer captures for it. Without this the ASIO tap filled
                    // only the monitor-mix ring above, and the per-track rings
                    // the UI drains stayed empty — so a take recorded through
                    // ASIO drew no waveform at all while it ran.
                    if preview_active && samples_per_bin > 0 {
                        for (slot, acc) in preview_accums.iter_mut().enumerate() {
                            let tl = frame
                                .get(acc.l_ch)
                                .copied()
                                .map(T::to_monitor_f32)
                                .unwrap_or(0.0)
                                .clamp(-1.0, 1.0);
                            let tr = frame
                                .get(acc.r_ch)
                                .copied()
                                .map(T::to_monitor_f32)
                                .unwrap_or(tl)
                                .clamp(-1.0, 1.0);
                            let tm = (tl + tr) * 0.5;
                            acc.bin_min = acc.bin_min.min(tm);
                            acc.bin_max = acc.bin_max.max(tm);
                            acc.bin_sumsq += tm * tm;
                            acc.bin_count += 1;
                            if acc.bin_count >= samples_per_bin {
                                let rms = (acc.bin_sumsq / acc.bin_count as f32).sqrt();
                                shared.preview_rings[slot].push(WaveformPeak {
                                    min: acc.bin_min,
                                    max: acc.bin_max,
                                    rms,
                                });
                                acc.reset();
                            }
                        }
                    } else {
                        for acc in preview_accums.iter_mut() {
                            acc.reset();
                        }
                    }

                    // Session-wide input peak across all channels (Settings
                    // input test + diagnostics).
                    for (channel, &sample) in frame.iter().enumerate() {
                        let value = T::to_monitor_f32(sample).abs();
                        session_peak = session_peak.max(value);
                        channel_peaks[channel] = channel_peaks[channel].max(value);
                    }
                }
                input_meters.publish(&channel_peaks);

                shared
                    .live_input_l
                    .store(f32_store(last_l), Ordering::Relaxed);
                shared
                    .live_input_r
                    .store(f32_store(last_r), Ordering::Relaxed);
                atomic_max_f32_bits(&shared.live_input_peak_l, raw_peak_l);
                atomic_max_f32_bits(&shared.live_input_peak_r, raw_peak_r);
                atomic_max_f32_bits(&shared.session_input_peak, session_peak);
                if capture_active {
                    atomic_max_f32_bits(&shared.record_peak, session_peak);
                }
                if ring_active {
                    shared.live_input_active.store(true, Ordering::Relaxed);
                }

                // 4. Record fan-out → disk writer (only while a take is live).
                if capture_active {
                    if let Some(sink) = record_sink.active() {
                        match sink.free_rx.try_recv() {
                            Ok(mut block) => {
                                if block.capacity() < data.len() {
                                    sink.dropped_blocks.fetch_add(1, Ordering::Relaxed);
                                    shared.record_ring_overruns.fetch_add(1, Ordering::Relaxed);
                                    let _ = sink.free_tx.try_send(block);
                                } else {
                                    block.clear();
                                    block.extend(data.iter().copied().map(T::to_record_i32));
                                    if let Err(error) = sink.audio_tx.try_send(block) {
                                        sink.dropped_blocks.fetch_add(1, Ordering::Relaxed);
                                        shared.record_ring_overruns.fetch_add(1, Ordering::Relaxed);
                                        let _ = sink.free_tx.try_send(error.into_inner());
                                    }
                                }
                            }
                            Err(_) => {
                                sink.dropped_blocks.fetch_add(1, Ordering::Relaxed);
                                shared.record_ring_overruns.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            },
            |error| eprintln!("[DAUx ASIO] input stream error: {error}"),
            None,
        )
        .map_err(|error| format!("cannot open ASIO input stream: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        openable_output_channels, snap_buffer_size, AsioInputCommand, AsioInputMeterBank,
        CallbackRecordSinkState, RecordSink,
    };
    use cpal::platform::AsioBufferSizeInfo;
    use crossbeam_channel::{bounded, Receiver, TryRecvError};
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    fn info(min: u32, max: u32, preferred: u32, granularity: i32) -> AsioBufferSizeInfo {
        AsioBufferSizeInfo {
            min,
            max,
            preferred,
            granularity,
        }
    }

    fn record_sink() -> (Box<RecordSink>, Receiver<Vec<i32>>) {
        let (audio_tx, audio_rx) = bounded(1);
        let (free_tx, free_rx) = bounded(1);
        (
            Box::new(RecordSink {
                audio_tx,
                free_rx,
                free_tx,
                samples_per_bin: 1,
                capture_on_transport: false,
                dropped_blocks: Arc::new(AtomicU64::new(0)),
                preview_accums: Vec::new(),
            }),
            audio_rx,
        )
    }

    fn assert_sink_is_alive(audio_rx: &Receiver<Vec<i32>>) {
        assert!(matches!(audio_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    fn assert_sink_was_dropped(audio_rx: &Receiver<Vec<i32>>) {
        assert!(matches!(
            audio_rx.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn every_driver_output_is_opened_not_just_the_first_pair() {
        // An 18-out interface opens all 18, so the Control Room can reach
        // Out 3-4 and everything above it.
        assert_eq!(openable_output_channels(18, 18), 18);
        assert_eq!(openable_output_channels(2, 2), 2);
        // A mono-out driver still opens its one channel.
        assert_eq!(openable_output_channels(1, 1), 1);
    }

    #[test]
    fn a_driver_that_disagrees_with_itself_opens_the_narrower_count() {
        // cpal refuses a stream wider than its own default config, so the open
        // must not ask for more than that even when the driver claims more.
        assert_eq!(openable_output_channels(64, 8), 8);
        // The other direction is just as possible; never open more than the
        // driver reports either.
        assert_eq!(openable_output_channels(8, 64), 8);
        assert_eq!(openable_output_channels(0, 8), 0);
    }

    #[test]
    fn input_meter_bank_keeps_channels_independent_and_resets_on_read() {
        let meters = AsioInputMeterBank::new(3);
        meters.publish(&[0.25, 0.75, 0.5]);
        meters.publish(&[0.5, 0.25, 1.0]);

        assert_eq!(meters.take_peaks(), vec![0.5, 0.75, 1.0]);
        assert_eq!(meters.take_peaks(), vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn full_trash_queue_retains_sinks_and_backpressures_commands() {
        let (input_cmd_tx, input_cmd_rx) = bounded(4);
        let (trash_tx, trash_rx) = bounded(1);
        let mut state = CallbackRecordSinkState::default();

        let (blocker, blocker_audio_rx) = record_sink();
        assert!(trash_tx.try_send(blocker).is_ok());

        let (first, first_audio_rx) = record_sink();
        assert!(input_cmd_tx
            .try_send(AsioInputCommand::SetRecordSink(first))
            .is_ok());
        state.drain_commands(&input_cmd_rx, &trash_tx);
        assert_sink_is_alive(&first_audio_rx);

        let (second, second_audio_rx) = record_sink();
        assert!(input_cmd_tx
            .try_send(AsioInputCommand::SetRecordSink(second))
            .is_ok());
        state.drain_commands(&input_cmd_rx, &trash_tx);
        assert!(state.active.is_some());
        assert!(state.pending_retirement.is_some());
        assert_sink_is_alive(&first_audio_rx);
        assert_sink_is_alive(&second_audio_rx);

        assert!(input_cmd_tx
            .try_send(AsioInputCommand::ClearRecordSink)
            .is_ok());
        state.drain_commands(&input_cmd_rx, &trash_tx);
        assert_eq!(input_cmd_rx.len(), 1);
        assert_sink_is_alive(&first_audio_rx);
        assert_sink_is_alive(&second_audio_rx);

        drop(trash_rx.try_recv().expect("blocking sink must be queued"));
        assert_sink_was_dropped(&blocker_audio_rx);

        state.drain_commands(&input_cmd_rx, &trash_tx);
        assert_eq!(input_cmd_rx.len(), 0);
        assert!(state.active.is_none());
        assert!(state.pending_retirement.is_some());
        assert_sink_is_alive(&first_audio_rx);
        assert_sink_is_alive(&second_audio_rx);

        drop(trash_rx.try_recv().expect("first sink must retire first"));
        assert_sink_was_dropped(&first_audio_rx);
        state.drain_commands(&input_cmd_rx, &trash_tx);
        assert!(state.pending_retirement.is_none());
        drop(trash_rx.try_recv().expect("second sink must retire last"));
        assert_sink_was_dropped(&second_audio_rx);
    }

    #[test]
    fn disconnected_trash_queue_retains_sink_and_queued_command() {
        let (input_cmd_tx, input_cmd_rx) = bounded(4);
        let (trash_tx, trash_rx) = bounded(1);
        let mut state = CallbackRecordSinkState::default();
        drop(trash_rx);

        let (first, first_audio_rx) = record_sink();
        let (second, second_audio_rx) = record_sink();
        assert!(input_cmd_tx
            .try_send(AsioInputCommand::SetRecordSink(first))
            .is_ok());
        assert!(input_cmd_tx
            .try_send(AsioInputCommand::ClearRecordSink)
            .is_ok());
        assert!(input_cmd_tx
            .try_send(AsioInputCommand::SetRecordSink(second))
            .is_ok());

        state.drain_commands(&input_cmd_rx, &trash_tx);
        assert_eq!(input_cmd_rx.len(), 1);
        assert!(state.active.is_none());
        assert!(state.pending_retirement.is_some());
        assert_sink_is_alive(&first_audio_rx);
        assert_sink_is_alive(&second_audio_rx);

        state.drain_commands(&input_cmd_rx, &trash_tx);
        assert_eq!(input_cmd_rx.len(), 1);
        assert_sink_is_alive(&first_audio_rx);
        assert_sink_is_alive(&second_audio_rx);

        drop(state);
        assert_sink_was_dropped(&first_audio_rx);
        drop(input_cmd_tx);
        drop(input_cmd_rx);
        assert_sink_was_dropped(&second_audio_rx);
    }

    #[test]
    fn default_uses_preferred() {
        assert_eq!(snap_buffer_size(None, &info(64, 2048, 512, -1)), 512);
        assert_eq!(snap_buffer_size(Some(0), &info(64, 2048, 512, -1)), 512);
    }

    #[test]
    fn clamps_to_driver_range() {
        assert_eq!(snap_buffer_size(Some(16), &info(64, 2048, 512, -1)), 64);
        assert_eq!(
            snap_buffer_size(Some(1 << 20), &info(64, 2048, 512, -1)),
            2048
        );
    }

    #[test]
    fn power_of_two_granularity_snaps_to_nearest() {
        assert_eq!(snap_buffer_size(Some(200), &info(64, 2048, 512, -1)), 256);
        assert_eq!(snap_buffer_size(Some(96), &info(64, 2048, 512, -1)), 64);
        assert_eq!(snap_buffer_size(Some(97), &info(64, 2048, 512, -1)), 128);
    }

    #[test]
    fn linear_granularity_steps_from_min() {
        // min=48, step=16 → valid sizes 48, 64, 80, ...
        assert_eq!(snap_buffer_size(Some(70), &info(48, 1024, 128, 16)), 64);
        assert_eq!(snap_buffer_size(Some(73), &info(48, 1024, 128, 16)), 80);
    }

    #[test]
    fn fixed_size_driver_always_uses_preferred() {
        assert_eq!(snap_buffer_size(Some(256), &info(512, 512, 512, 0)), 512);
    }
}
