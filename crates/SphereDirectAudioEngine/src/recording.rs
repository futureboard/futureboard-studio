//! Audio recording session management.
//!
//! # Architecture
//!
//! ```text
//! JS control thread
//!   └─ start_recording()
//!        ├─ opens cpal input stream  ──► input callback
//!        │                                 └─ try_recv(pool) + try_send(block) ──► bounded channel
//!        └─ spawns disk writer thread ──► recv(block) → RaufWriter → .rauf
//!
//! JS control thread
//!   └─ stop_recording()
//!        ├─ drop(input_stream)  →  channel closes  →  disk writer exits loop
//!        └─ recv(results)  →  return to caller
//! ```
//!
//! The audio callback does not write files, encode containers, or block on disk
//! I/O. The recording path uses a bounded preallocated block pool and drops on
//! backpressure instead of allocating or blocking.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::bounded;
use sphere_encoder::rauf::{RaufConfig, RaufSampleFormat, RaufWriter, RAUF_FLAG_HAS_SIDECAR};

use crate::error::SphereAudioError;
use crate::types::{JsRecordingResult, JsStartRecordingConfig};

// ── Unique recording counter for collision-free filenames ─────────────────────

static RECORD_COUNTER: AtomicU64 = AtomicU64::new(1);

// ── Internal types ────────────────────────────────────────────────────────────

struct TrackWriterState {
    track_id: String,
    track_name: String,
    writer: RaufWriter,
    /// 0-based indices into the interleaved input block to capture.
    input_channels: Vec<usize>,
    /// Number of channels written to the output RAUF (= input_channels.len()).
    out_channels: u16,
    final_path: PathBuf,
    relative_path: String,
    sidecar_path: PathBuf,
    sidecar_relative_path: String,
    take_id: String,
    project_start_sample: u64,
    error: Option<String>,
}

pub struct RecordingResult {
    pub track_id: String,
    pub file_path: String,
    pub relative_path: String,
    pub start_beat: f64,
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub channels: u32,
    pub metadata_path: String,
    pub sample_format: String,
    pub success: bool,
    pub error: Option<String>,
}

/// Where the take's samples come from.
pub(crate) enum CaptureSource {
    /// A dedicated cpal input stream owned by this session. The fallback for a
    /// take that starts with no live input stream open, or one whose geometry
    /// cannot serve the take. Dropping it stops capture and disconnects the
    /// audio channel. The field is a pure keep-alive/drop guard — never read.
    #[allow(dead_code)]
    OwnStream(cpal::Stream),
    /// The persistent live input stream feeds the take through an installed
    /// [`crate::capture_tap::RecordSink`]. Nothing in the device path moves
    /// when the take starts or stops, which is the whole point: opening and
    /// closing a capture client mid-session is audible at both ends.
    LiveStreamTap,
    /// The persistent ASIO session input callback feeds the take through an
    /// installed [`crate::backend::asio_session::RecordSink`]. The engine
    /// detaches the sink via `AsioInputCommand::ClearRecordSink`; the writer
    /// additionally exits on `stop_flag` so a stalled driver can never wedge
    /// finalization.
    #[cfg(all(target_os = "windows", feature = "asio"))]
    AsioSessionTap,
}

pub struct RecordingSession {
    pub(crate) capture: CaptureSource,
    /// Receives finalized per-track results from the disk writer thread.
    pub results_rx: std::sync::mpsc::Receiver<Vec<RecordingResult>>,
    /// Tells the disk writer to finalize even if its senders are still alive.
    pub stop_flag: Arc<AtomicBool>,
    pub start_beat: f64,
    pub sample_rate: u32,
    pub track_count: usize,
    pub recording_active: Arc<AtomicBool>,
    pub dropped_blocks: Arc<AtomicU64>,
    pub started_at: std::time::Instant,
    pub shared: Arc<crate::engine::SharedState>,
    /// Transport position when the take was armed — the sample its RAUF header
    /// and `start_beat` were written against.
    pub armed_start_sample: u64,
    /// Transport position of the first block actually captured, or
    /// [`u64::MAX`] if none was. See
    /// [`crate::capture_tap::RecordSink::first_capture_sample`]; the difference
    /// between the two is how far after its declared start the take really
    /// began, and it is corrected out at finalization.
    pub first_capture_sample: Arc<AtomicU64>,
}

impl RecordingSession {
    /// Whether this take taps the persistent ASIO session input.
    pub fn is_asio_tap(&self) -> bool {
        #[cfg(all(target_os = "windows", feature = "asio"))]
        {
            matches!(self.capture, CaptureSource::AsioSessionTap)
        }
        #[cfg(not(all(target_os = "windows", feature = "asio")))]
        {
            false
        }
    }

    /// Whether this take taps the persistent cpal live input stream.
    pub fn is_live_stream_tap(&self) -> bool {
        matches!(self.capture, CaptureSource::LiveStreamTap)
    }

    /// Whether the take owns the capture device for its duration.
    ///
    /// Only an own-stream take does. A tapped take borrows a stream that
    /// existed before it and outlives it, so ending one must not reset the
    /// input ring or clear the monitor flags out from under monitoring that was
    /// never the take's to turn off.
    pub fn owns_capture_stream(&self) -> bool {
        matches!(self.capture, CaptureSource::OwnStream(_))
    }
}

// Safety: cpal::Stream is !Send due to a PhantomData marker on Windows (COM
// thread affinity).  We only access RecordingSession from the JS/control thread
// under a parking_lot::Mutex — never from the audio thread.
unsafe impl Send for RecordingSession {}
unsafe impl Sync for RecordingSession {}

// ── Filename helpers ──────────────────────────────────────────────────────────

fn sanitize_filename(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    if safe.trim().is_empty() {
        "Recording".to_string()
    } else {
        safe.trim().to_string()
    }
}

/// Returns a unique path inside `dir` that does not already exist.
///
/// Filename contract:
/// `{ProjectName}-{timestamp}-{takenumber}.{ext}`
fn unique_recording_path(
    dir: &Path,
    project_name: &str,
    timestamp: &str,
    extension: &str,
) -> PathBuf {
    let project_name = sanitize_filename(project_name);
    let timestamp = sanitize_filename(timestamp);
    let extension = extension.trim_start_matches('.').trim();
    let extension = if extension.is_empty() {
        "rauf"
    } else {
        extension
    };

    loop {
        let n = RECORD_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Zero-pad to 4 digits so alphabetical sort matches recording order.
        let filename = format!("{project_name}-{timestamp}-{n:04}.{extension}");
        let path = dir.join(filename);
        if !path.exists() {
            return path;
        }
    }
}

fn make_take_id(session_id: &str, track_index: u64) -> [u8; 16] {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in session_id.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let counter = RECORD_COUNTER
        .load(Ordering::Relaxed)
        .wrapping_add(track_index);
    let mut id = [0u8; 16];
    id[0..8].copy_from_slice(&hash.to_le_bytes());
    id[8..16].copy_from_slice(&counter.to_le_bytes());
    id
}

fn format_take_id(take_id: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in take_id {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

// ── Device lookup ─────────────────────────────────────────────────────────────

pub fn find_input_device(device_id: Option<&str>) -> Result<cpal::Device, SphereAudioError> {
    let host = cpal::default_host();
    find_input_device_for_host(&host, device_id)
}

pub(crate) fn find_input_device_for_host(
    host: &cpal::Host,
    device_id: Option<&str>,
) -> Result<cpal::Device, SphereAudioError> {
    if host.id().name().eq_ignore_ascii_case("ASIO") {
        // ASIO capture taps the duplex session's input callback; resolving a
        // second device here would clobber the session's buffer set.
        return Err(SphereAudioError::NativeError(
            "ASIO input devices are never opened separately from the session".into(),
        ));
    }
    if let Some(id) = device_id {
        if !id.is_empty() {
            let open_id = crate::device::resolve_open_id(host, id, true);
            let mut devices = host
                .input_devices()
                .map_err(|e| SphereAudioError::NativeError(e.to_string()))?;
            if let Some(dev) = devices.find(|d| d.name().as_deref().ok() == Some(open_id.as_str()))
            {
                return Ok(dev);
            }
            return Err(SphereAudioError::NativeError(format!(
                "Input device not found: '{id}'"
            )));
        }
    }
    host.default_input_device()
        .ok_or_else(|| SphereAudioError::NativeError("No default input device".to_string()))
}

// ── RAUF / disk writer thread ─────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn disk_writer_thread(
    audio_rx: crossbeam_channel::Receiver<Vec<i32>>,
    free_tx: crossbeam_channel::Sender<Vec<i32>>,
    mut writers: Vec<TrackWriterState>,
    sample_rate: u32,
    input_ch: usize, // channels per interleaved input frame
    start_beat: f64,
    finalize_tx: std::sync::mpsc::Sender<Vec<RecordingResult>>,
    stop_flag: Arc<AtomicBool>,
) {
    let mut total_frames = 0u64;

    // Drain audio blocks until the sender disconnects (own-stream capture) or
    // the stop flag is raised with no data pending (ASIO tap capture, where a
    // stalled driver could otherwise keep the sender alive forever).
    loop {
        let mut block = match audio_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(block) => block,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        let frames = block.len().checked_div(input_ch).unwrap_or(0);
        if frames == 0 {
            block.clear();
            let _ = free_tx.try_send(block);
            continue;
        }
        for w in &mut writers {
            let mut selected = Vec::with_capacity(frames * w.input_channels.len());
            for f in 0..frames {
                for &ch in &w.input_channels {
                    let s = if ch < input_ch {
                        block[f * input_ch + ch]
                    } else {
                        0
                    };
                    selected.push(s);
                }
            }
            if w.error.is_none() {
                if let Err(error) = w.writer.write_s32le_interleaved(&selected) {
                    w.error = Some(error.to_string());
                }
            }
        }
        total_frames += frames as u64;
        block.clear();
        let _ = free_tx.try_send(block);
    }

    let duration_seconds = if sample_rate > 0 {
        total_frames as f64 / sample_rate as f64
    } else {
        0.0
    };

    let mut results = Vec::with_capacity(writers.len());

    for mut w in writers {
        w.writer.set_flags(RAUF_FLAG_HAS_SIDECAR);
        let sidecar = RaufSidecarData {
            sidecar_path: w.sidecar_path.clone(),
            relative_path: w.relative_path.clone(),
            take_id: w.take_id.clone(),
            track_id: w.track_id.clone(),
            track_name: w.track_name.clone(),
            project_start_sample: w.project_start_sample,
            out_channels: w.out_channels,
        };
        let write_error = w.error.take();
        let finalized = w.writer.finalize();
        let frames_recorded = finalized
            .as_ref()
            .map(|report| report.frames_written)
            .unwrap_or(0);
        let sidecar_result =
            write_rauf_sidecar(&sidecar, sample_rate, frames_recorded, true, false);
        let ok = write_error.is_none() && finalized.is_ok() && sidecar_result.is_ok();
        let error = write_error.or_else(|| {
            finalized
                .err()
                .map(|error| error.to_string())
                .or_else(|| sidecar_result.err().map(|error| error.to_string()))
        });
        results.push(RecordingResult {
            track_id: w.track_id,
            file_path: w.final_path.to_string_lossy().into_owned(),
            relative_path: w.relative_path,
            start_beat,
            duration_seconds,
            sample_rate,
            channels: w.out_channels as u32,
            metadata_path: w.sidecar_relative_path,
            sample_format: "s32le".to_string(),
            success: ok,
            error,
        });
    }

    let _ = finalize_tx.send(results);
}

struct RaufSidecarData {
    sidecar_path: PathBuf,
    relative_path: String,
    take_id: String,
    track_id: String,
    track_name: String,
    project_start_sample: u64,
    out_channels: u16,
}

fn write_rauf_sidecar(
    w: &RaufSidecarData,
    sample_rate: u32,
    frames_recorded: u64,
    finalized: bool,
    recovered: bool,
) -> std::io::Result<()> {
    let audio_file = Path::new(&w.relative_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("take.rauf");
    let peak_file = format!(
        "{}.peak",
        Path::new(audio_file)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("take")
    );
    let metadata = serde_json::json!({
        "format": "futureboard.rauf.sidecar",
        "version": 1,
        "audio_file": audio_file,
        "take_id": w.take_id,
        "track_id": w.track_id,
        "track_name": w.track_name,
        "record_mode": "live_input",
        "project_start_sample": w.project_start_sample,
        "sample_rate": sample_rate,
        "channels": w.out_channels,
        "sample_format": "s32le",
        "interleaved": true,
        "frames_recorded": frames_recorded,
        "finalized": finalized,
        "recovered": recovered,
        "peak_file": peak_file,
    });
    let text = serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&w.sidecar_path, text)
}

/// Realtime-safe max into an f32-bits atomic (no allocation, no lock).
#[inline]
fn atomic_max_bits(target: &AtomicU32, value: f32) {
    let value = value.max(0.0);
    let mut cur = target.load(Ordering::Relaxed);
    loop {
        if value <= f32::from_bits(cur) {
            break;
        }
        match target.compare_exchange_weak(
            cur,
            value.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(c) => cur = c,
        }
    }
}

/// Whether this callback should feed take-only capture paths.
///
/// Pure and realtime-safe: callers provide callback-local atomic snapshots.

/// Which armed tracks the growing waveform can describe, and on which input
/// channels, in `shared.preview_rings` slot order.
///
/// Shared by both capture paths on purpose. They had grown separate ideas of
/// this — the own-stream path derived it and the ASIO tap did not derive it at
/// all, so a take recorded through ASIO published no preview tracks and the UI
/// tore its growing clip down on the first poll. One derivation is the only way
/// the two paths can stay honest about it.
///
/// A track whose first input channel is outside the device's channel count is
/// skipped rather than clamped: it is not recording anything the preview could
/// draw, and clamping would draw somebody else's signal under its name. A mono
/// track previews its one channel on both sides.
pub(crate) fn preview_tracks_for(
    tracks: &[crate::types::JsRecordingTrackConfig],
    input_channels: usize,
) -> Vec<(String, usize, usize)> {
    tracks
        .iter()
        .take(crate::input_ring::MAX_RECORDING_PREVIEW_TRACKS)
        .filter_map(|t| {
            let l = *t.input_channels.first()? as usize;
            if l >= input_channels {
                return None;
            }
            let r = t
                .input_channels
                .get(1)
                .map(|&c| c as usize)
                .filter(|&c| c < input_channels)
                .unwrap_or(l);
            Some((t.track_id.clone(), l, r))
        })
        .collect()
}

#[inline]
pub(crate) fn should_capture_recording(
    recording_active: bool,
    capture_on_transport: bool,
    transport_playing: bool,
) -> bool {
    recording_active && (!capture_on_transport || transport_playing)
}

fn reset_failed_own_stream_start(shared: &crate::engine::SharedState) {
    shared.recording_active.store(false, Ordering::Relaxed);
    shared.recording_monitor_mix.store(false, Ordering::Relaxed);
    shared
        .recording_preview_active
        .store(false, Ordering::Relaxed);
    shared.recording_preview_track_ids.lock().clear();
    shared.live_input_active.store(false, Ordering::Relaxed);
    shared.input_ring.set_active(false, 0, 0);
    shared.monitor_enabled_any.store(false, Ordering::Relaxed);
    shared.monitor_shared_clock.store(false, Ordering::Relaxed);
    shared
        .record_peak
        .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
}

// ── Input stream builder (f32 samples) ───────────────────────────────────────

/// Build the single recording capture stream. Its one realtime callback fans
/// out to five independent paths, none of which blocks another:
///   1. monitor       → `shared.input_ring` (read by the output render callback)
///   2. record        → `tx` channel → disk-writer worker thread
///   3. preview        → min/max/rms bins → `shared.preview_ring` (drained by UI)
///      - per-track preview → one ring per armed track → `shared.preview_rings`
///   4. meters/diag    → raw input peak + lightweight counters
///
/// Realtime-safe: the record path uses a bounded preallocated block pool and
/// drops when the pool or writer queue is full; the monitor/preview/meter paths
/// are atomics-only.
#[allow(clippy::too_many_arguments)]
fn build_f32_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: crossbeam_channel::Sender<Vec<i32>>,
    free_rx: crossbeam_channel::Receiver<Vec<i32>>,
    free_tx: crossbeam_channel::Sender<Vec<i32>>,
    active: Arc<AtomicBool>,
    dropped_blocks: Arc<AtomicU64>,
    shared: Arc<crate::engine::SharedState>,
    channels: usize,
    monitor_channels: Vec<usize>,
    preview_tracks: Vec<(String, usize, usize)>,
    samples_per_bin: usize,
    capture_on_transport: bool,
    first_capture_sample: Arc<AtomicU64>,
) -> Result<cpal::Stream, SphereAudioError> {
    use crate::engine::{f32_load, f32_store};
    use crate::input_ring::WaveformPeak;

    let mon_l_ch = monitor_channels.first().copied().unwrap_or(0);
    let mon_r_ch = monitor_channels.get(1).copied().unwrap_or(mon_l_ch);
    let samples_per_bin = samples_per_bin.max(1);

    // Preview accumulator — captured (FnMut) state, no allocation per callback.
    let mut bin_min = f32::MAX;
    let mut bin_max = f32::MIN;
    let mut bin_sumsq = 0.0f32;
    let mut bin_count = 0usize;

    // Per-track preview accumulators (Part 1, multi-track) — one entry per
    // armed track, built once here and moved into the closure below. Fixed
    // length for the whole take; mutated in place per frame, never
    // grown/shrunk, so indexing `shared.preview_rings` stays allocation-free.
    struct PreviewAccum {
        l_ch: usize,
        r_ch: usize,
        bin_min: f32,
        bin_max: f32,
        bin_sumsq: f32,
        bin_count: usize,
    }
    let mut track_accums: Vec<PreviewAccum> = preview_tracks
        .iter()
        .map(|&(_, l, r)| PreviewAccum {
            l_ch: l,
            r_ch: r,
            bin_min: f32::MAX,
            bin_max: f32::MIN,
            bin_sumsq: 0.0,
            bin_count: 0,
        })
        .collect();

    #[cfg(target_os = "linux")]
    let rt_announcer = crate::backend::cpal_backend::spawn_capture_rt_promoter();
    #[cfg(target_os = "linux")]
    let mut rt_announced = false;

    device
        .build_input_stream::<f32, _, _>(
            config,
            move |data: &[f32], info| {
                #[cfg(target_os = "linux")]
                if !rt_announced {
                    rt_announcer.announce_current_thread();
                    rt_announced = true;
                }
                let ch = channels.max(1);
                let frames = data.len() / ch;
                shared.input_cb_count.fetch_add(1, Ordering::Relaxed);

                // Publish the capture latency (ADC → callback) so the committed
                // take can be pulled earlier by the real input delay. One atomic
                // store from cpal's own timestamps — realtime-safe.
                let in_ts = info.timestamp();
                if let Some(delay) = in_ts.callback.duration_since(&in_ts.capture) {
                    shared
                        .record_input_latency_secs
                        .store(f32_store(delay.as_secs_f32()), Ordering::Relaxed);
                }
                shared
                    .input_frames_received
                    .fetch_add(frames as u64, Ordering::Relaxed);
                let capture_active = should_capture_recording(
                    active.load(Ordering::Relaxed),
                    capture_on_transport,
                    shared.playing.load(Ordering::Relaxed),
                );
                if capture_active {
                    // Stamp where the audio really begins — see
                    // `RecordingSession::first_capture_sample`.
                    let _ = first_capture_sample.compare_exchange(
                        u64::MAX,
                        shared.position_samples.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                }

                let mut raw_peak_l = 0.0f32;
                let mut raw_peak_r = 0.0f32;
                let mut last_l = 0.0f32;
                let mut last_r = 0.0f32;
                let mut rec_peak = 0.0f32;

                for frame in data.chunks(ch) {
                    let first = frame.first().copied().unwrap_or(0.0);
                    let l = frame
                        .get(mon_l_ch)
                        .copied()
                        .unwrap_or(first)
                        .clamp(-1.0, 1.0);
                    let r = frame.get(mon_r_ch).copied().unwrap_or(l).clamp(-1.0, 1.0);
                    last_l = l;
                    last_r = r;
                    raw_peak_l = raw_peak_l.max(l.abs());
                    raw_peak_r = raw_peak_r.max(r.abs());

                    // 1. Monitor bridge → output render callback.
                    shared.input_ring.write_stereo(l, r);

                    // 3. Preview bins (mono mix of the monitored channels).
                    // Guard with the same session-active flag as the writer so
                    // stopping a take cannot publish late bins while the UI is
                    // finalizing the committed clip.
                    if capture_active {
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

                    // 3b. Per-track preview bins (Part 1, multi-track): each
                    // armed track gets its own mono mix of its own selected
                    // input channels, mirroring what the disk writer captures
                    // for that track — not the single global monitor mix
                    // above. Bounded loop (one entry per armed track, capped
                    // at MAX_RECORDING_PREVIEW_TRACKS), plain arithmetic, no
                    // allocation.
                    if capture_active {
                        for (slot, acc) in track_accums.iter_mut().enumerate() {
                            let tl = frame.get(acc.l_ch).copied().unwrap_or(0.0).clamp(-1.0, 1.0);
                            let tr = frame.get(acc.r_ch).copied().unwrap_or(tl).clamp(-1.0, 1.0);
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
                                acc.bin_min = f32::MAX;
                                acc.bin_max = f32::MIN;
                                acc.bin_sumsq = 0.0;
                                acc.bin_count = 0;
                            }
                        }
                    } else {
                        for acc in track_accums.iter_mut() {
                            acc.bin_min = f32::MAX;
                            acc.bin_max = f32::MIN;
                            acc.bin_sumsq = 0.0;
                            acc.bin_count = 0;
                        }
                    }

                    // 4. Record peak across all channels (diagnostics).
                    if capture_active {
                        for &s in frame {
                            rec_peak = rec_peak.max(s.abs());
                        }
                    }
                }

                // Meters / diagnostics atomics.
                shared
                    .live_input_l
                    .store(f32_store(last_l), Ordering::Relaxed);
                shared
                    .live_input_r
                    .store(f32_store(last_r), Ordering::Relaxed);
                atomic_max_bits(&shared.live_input_peak_l, raw_peak_l);
                atomic_max_bits(&shared.live_input_peak_r, raw_peak_r);
                shared.live_input_active.store(true, Ordering::Relaxed);
                if capture_active {
                    let prev_rec = f32_load(shared.record_peak.load(Ordering::Relaxed)) * 0.9;
                    shared
                        .record_peak
                        .store(f32_store(prev_rec.max(rec_peak)), Ordering::Relaxed);
                }

                // 2. Record path → disk writer worker (only while armed/active).
                if capture_active {
                    match free_rx.try_recv() {
                        Ok(mut block) => {
                            if block.capacity() < data.len() {
                                dropped_blocks.fetch_add(1, Ordering::Relaxed);
                                shared.record_ring_overruns.fetch_add(1, Ordering::Relaxed);
                                let _ = free_tx.try_send(block);
                                return;
                            }
                            block.clear();
                            block.extend(data.iter().copied().map(f32_to_s32));
                            if let Err(error) = tx.try_send(block) {
                                dropped_blocks.fetch_add(1, Ordering::Relaxed);
                                shared.record_ring_overruns.fetch_add(1, Ordering::Relaxed);
                                let _ = free_tx.try_send(error.into_inner());
                            }
                        }
                        Err(_) => {
                            dropped_blocks.fetch_add(1, Ordering::Relaxed);
                            shared.record_ring_overruns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            },
            |err| eprintln!("[SphereAudio] Input stream error: {err}"),
            None,
        )
        .map_err(|e| SphereAudioError::NativeError(format!("Cannot open input stream: {e}")))
}

#[inline]
pub(crate) fn f32_to_s32(sample: f32) -> i32 {
    let x = sample.clamp(-1.0, 1.0);
    if x >= 0.0 {
        (x * i32::MAX as f32) as i32
    } else {
        (x * -(i32::MIN as f32)) as i32
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Open an input stream and begin recording armed tracks.
pub fn start_recording(
    config: JsStartRecordingConfig,
    shared: Arc<crate::engine::SharedState>,
    monitor_mix: bool,
) -> Result<RecordingSession, SphereAudioError> {
    let device = find_input_device(config.input_device_id.as_deref())?;
    start_recording_with_device(config, shared, monitor_mix, device, None)
}

/// Create the `Recordings` folder and build one RAUF writer per armed track,
/// validating each track's selected channels against `input_ch` (channels per
/// interleaved capture frame). Shared by the own-stream and ASIO-tap paths.
fn build_track_writers(
    config: &JsStartRecordingConfig,
    shared: &crate::engine::SharedState,
    input_ch: usize,
    sample_rate: u32,
) -> Result<Vec<TrackWriterState>, SphereAudioError> {
    if config.tracks.is_empty() {
        return Err(SphereAudioError::NativeError(
            "No armed tracks — nothing to record".to_string(),
        ));
    }

    // Ensure directory structure exists.
    let project_root = Path::new(&config.project_root);
    let recordings_dir = project_root.join("Recordings");
    std::fs::create_dir_all(&recordings_dir).map_err(|e| {
        SphereAudioError::NativeError(format!("Cannot create recordings folder: {e}"))
    })?;
    let project_start_sample = shared.position_samples.load(Ordering::Relaxed);

    let mut track_writers: Vec<TrackWriterState> = Vec::new();
    for (track_index, track) in config.tracks.iter().enumerate() {
        let project_name = if config.project_name.trim().is_empty() {
            "Recording"
        } else {
            config.project_name.as_str()
        };
        let timestamp = if config.timestamp.trim().is_empty() {
            config.session_id.as_str()
        } else {
            config.timestamp.as_str()
        };
        let final_path = unique_recording_path(&recordings_dir, project_name, timestamp, "rauf");
        let filename = final_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "recording.rauf".to_string());
        let relative_path = format!("recordings/{filename}");
        let sidecar_path = final_path.with_extension("rauf.json");
        let sidecar_filename = sidecar_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "recording.rauf.json".to_string());
        let sidecar_relative_path = format!("recordings/{sidecar_filename}");

        let in_chs: Vec<usize> = track.input_channels.iter().map(|&c| c as usize).collect();
        if in_chs.is_empty() {
            return Err(SphereAudioError::NativeError(format!(
                "{} has no input channels selected",
                track.name
            )));
        }
        if let Some(channel) = in_chs.iter().find(|&&channel| channel >= input_ch) {
            return Err(SphereAudioError::NativeError(format!(
                "{} input channel {} is unavailable on the active input device ({input_ch} channel(s))",
                track.name,
                channel + 1
            )));
        }
        let out_channels = in_chs.len().max(1) as u16;
        let take_id = make_take_id(&config.session_id, track_index as u64);
        let writer = RaufWriter::create(
            &final_path,
            RaufConfig {
                sample_rate,
                channels: out_channels,
                sample_format: RaufSampleFormat::S32,
                interleaved: true,
                project_start_sample,
                take_id,
            },
        )
        .map_err(|e| {
            SphereAudioError::NativeError(format!("Cannot create RAUF recording file: {e}"))
        })?;

        track_writers.push(TrackWriterState {
            track_id: track.track_id.clone(),
            track_name: track.name.clone(),
            writer,
            input_channels: in_chs,
            out_channels,
            final_path,
            relative_path,
            sidecar_path,
            sidecar_relative_path,
            take_id: format_take_id(take_id),
            project_start_sample,
            error: None,
        });
    }
    Ok(track_writers)
}

/// Block pool + bounded audio channel + disk-writer thread. `pool_block_samples`
/// is the per-block capacity in interleaved samples; blocks are preallocated so
/// the capture callback never grows one on the audio thread.
struct RecordingPipeline {
    audio_tx: crossbeam_channel::Sender<Vec<i32>>,
    free_rx: crossbeam_channel::Receiver<Vec<i32>>,
    free_tx: crossbeam_channel::Sender<Vec<i32>>,
    finalize_rx: std::sync::mpsc::Receiver<Vec<RecordingResult>>,
    stop_flag: Arc<AtomicBool>,
}

fn spawn_recording_pipeline(
    track_writers: Vec<TrackWriterState>,
    sample_rate: u32,
    input_ch: usize,
    start_beat: f64,
    pool_block_samples: usize,
    pool_blocks: usize,
) -> RecordingPipeline {
    // Bounded channel: if the disk writer falls behind, `try_send` drops the
    // block rather than blocking the audio callback.
    let (audio_tx, audio_rx) = bounded::<Vec<i32>>(pool_blocks);
    let (free_tx, free_rx) = bounded::<Vec<i32>>(pool_blocks);
    for _ in 0..pool_blocks {
        let _ = free_tx.try_send(Vec::with_capacity(pool_block_samples.max(1)));
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let (finalize_tx, finalize_rx) = std::sync::mpsc::channel();
    let writer_free_tx = free_tx.clone();
    let writer_stop_flag = Arc::clone(&stop_flag);
    std::thread::spawn(move || {
        disk_writer_thread(
            audio_rx,
            writer_free_tx,
            track_writers,
            sample_rate,
            input_ch,
            start_beat,
            finalize_tx,
            writer_stop_flag,
        );
    });

    RecordingPipeline {
        audio_tx,
        free_rx,
        free_tx,
        finalize_rx,
        stop_flag,
    }
}

pub(crate) fn start_recording_with_device(
    config: JsStartRecordingConfig,
    shared: Arc<crate::engine::SharedState>,
    monitor_mix: bool,
    device: cpal::Device,
    preferred_period: Option<u32>,
) -> Result<RecordingSession, SphereAudioError> {
    let default_cfg = device
        .default_input_config()
        .map_err(|e| SphereAudioError::NativeError(format!("Input device config error: {e}")))?;

    let preferred_sample_rate = {
        let out_sr = shared.sample_rate.load(Ordering::Relaxed);
        (out_sr > 0).then_some(out_sr)
    };
    let candidates = crate::device::input_stream_config_candidates(
        &default_cfg,
        preferred_period,
        preferred_sample_rate,
    );

    let mut last_error = None;
    for (label, stream_config) in candidates {
        match start_recording_with_config(
            &config,
            Arc::clone(&shared),
            monitor_mix,
            &device,
            stream_config.clone(),
            label,
        ) {
            Ok(session) => return Ok(session),
            Err(error) => {
                last_error = Some(format!("{label}: {error}"));
            }
        }
    }
    Err(SphereAudioError::NativeError(format!(
        "Cannot open input stream: {}",
        last_error.unwrap_or_else(|| "no candidates".into())
    )))
}

fn start_recording_with_config(
    config: &JsStartRecordingConfig,
    shared: Arc<crate::engine::SharedState>,
    monitor_mix: bool,
    device: &cpal::Device,
    stream_config: cpal::StreamConfig,
    candidate_label: &str,
) -> Result<RecordingSession, SphereAudioError> {
    let input_ch = stream_config.channels as usize;
    let sample_rate = stream_config.sample_rate.0;

    let track_writers = build_track_writers(config, &shared, input_ch, sample_rate)?;
    let track_count = track_writers.len();

    let max_record_block_samples = input_ch.saturating_mul(8192).max(input_ch.max(1));
    let pipeline = spawn_recording_pipeline(
        track_writers,
        sample_rate,
        input_ch,
        config.start_beat,
        max_record_block_samples,
        512,
    );
    let RecordingPipeline {
        audio_tx,
        free_rx,
        free_tx,
        finalize_rx,
        stop_flag,
    } = pipeline;
    let start_beat = config.start_beat;
    let capture_on_transport = config.capture_on_transport.unwrap_or(false);

    let recording_active = Arc::new(AtomicBool::new(true));
    let dropped_blocks = Arc::new(AtomicU64::new(0));
    shared.recording_active.store(true, Ordering::Relaxed);
    shared
        .record_peak
        .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
    shared
        .recording_monitor_mix
        .store(monitor_mix, Ordering::Relaxed);
    let monitor_channels: Vec<usize> = config
        .monitor_channels
        .iter()
        .copied()
        .filter_map(|channel| {
            let channel = channel as usize;
            (channel < input_ch).then_some(channel)
        })
        .collect();

    const PREVIEW_PEAKS_PER_SEC: u32 = 150;
    let samples_per_bin = (sample_rate / PREVIEW_PEAKS_PER_SEC).max(1) as usize;
    let preview_channels = monitor_channels.len().max(1) as u32;
    let start_sample = shared.position_samples.load(Ordering::Relaxed);

    shared.preview_ring.reset();
    shared.recording_preview_id.fetch_add(1, Ordering::Relaxed);
    shared
        .recording_preview_start_sample
        .store(start_sample, Ordering::Relaxed);
    shared
        .recording_preview_sample_rate
        .store(sample_rate, Ordering::Relaxed);
    shared
        .recording_preview_channels
        .store(preview_channels, Ordering::Relaxed);
    shared
        .recording_preview_peaks_per_sec
        .store(PREVIEW_PEAKS_PER_SEC, Ordering::Relaxed);
    shared
        .recording_preview_active
        .store(true, Ordering::Relaxed);

    let preview_tracks = preview_tracks_for(&config.tracks, input_ch);
    *shared.recording_preview_track_ids.lock() =
        preview_tracks.iter().map(|(id, ..)| id.clone()).collect();
    for slot in 0..preview_tracks.len() {
        shared.preview_rings[slot].reset();
    }

    shared.monitor_shared_clock.store(false, Ordering::Relaxed);
    shared
        .input_ring
        .set_active(true, input_ch as u32, sample_rate);
    shared
        .monitor_enabled_any
        .store(monitor_mix, Ordering::Relaxed);

    let first_capture_sample = Arc::new(AtomicU64::new(u64::MAX));
    let input_stream = match build_f32_input_stream(
        device,
        &stream_config,
        audio_tx,
        free_rx,
        free_tx,
        Arc::clone(&recording_active),
        Arc::clone(&dropped_blocks),
        Arc::clone(&shared),
        input_ch,
        monitor_channels,
        preview_tracks,
        samples_per_bin,
        capture_on_transport,
        Arc::clone(&first_capture_sample),
    ) {
        Ok(stream) => stream,
        Err(error) => {
            recording_active.store(false, Ordering::Relaxed);
            reset_failed_own_stream_start(&shared);
            return Err(error);
        }
    };

    if let Err(error) = input_stream.play() {
        recording_active.store(false, Ordering::Relaxed);
        reset_failed_own_stream_start(&shared);
        return Err(SphereAudioError::NativeError(format!(
            "Cannot start input stream: {error}"
        )));
    }

    eprintln!(
        "[SphereAudio] Recording started via '{candidate_label}': {track_count} track(s), \
         {input_ch}ch input @ {sample_rate} Hz buffer={:?}",
        stream_config.buffer_size
    );

    Ok(RecordingSession {
        capture: CaptureSource::OwnStream(input_stream),
        results_rx: finalize_rx,
        stop_flag,
        start_beat,
        sample_rate,
        track_count,
        recording_active,
        dropped_blocks,
        started_at: std::time::Instant::now(),
        shared,
        armed_start_sample: start_sample,
        first_capture_sample,
    })
}

/// Begin a take that taps the persistent cpal live input stream instead of
/// opening one of its own.
///
/// The preferred path on every cpal backend. `input_ch` and `sample_rate` are
/// the *live stream's* geometry, not a fresh negotiation — the take records
/// exactly what is already being monitored, which is also why the channel
/// validation in [`build_track_writers`] is meaningful here.
///
/// The ring and monitor flags are deliberately untouched: the stream they
/// describe existed before this take and will outlive it.
pub(crate) fn start_recording_live_tap(
    config: JsStartRecordingConfig,
    shared: Arc<crate::engine::SharedState>,
    monitor_mix: bool,
    input_ch: usize,
    sample_rate: u32,
) -> Result<(RecordingSession, crate::capture_tap::RecordSink), SphereAudioError> {
    let track_writers = build_track_writers(&config, &shared, input_ch, sample_rate)?;
    let track_count = track_writers.len();

    let max_record_block_samples = input_ch.saturating_mul(8192).max(input_ch.max(1));
    let pipeline = spawn_recording_pipeline(
        track_writers,
        sample_rate,
        input_ch,
        config.start_beat,
        max_record_block_samples,
        512,
    );

    let recording_active = Arc::new(AtomicBool::new(true));
    let dropped_blocks = Arc::new(AtomicU64::new(0));
    shared.recording_active.store(true, Ordering::Relaxed);
    shared
        .record_peak
        .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
    shared
        .recording_monitor_mix
        .store(monitor_mix, Ordering::Relaxed);

    const PREVIEW_PEAKS_PER_SEC: u32 = 150;
    let samples_per_bin = (sample_rate / PREVIEW_PEAKS_PER_SEC).max(1) as usize;
    let monitor_channels: Vec<usize> = config
        .monitor_channels
        .iter()
        .copied()
        .filter_map(|channel| {
            let channel = channel as usize;
            (channel < input_ch).then_some(channel)
        })
        .collect();
    let mon_l = monitor_channels.first().copied().unwrap_or(0);
    let mon_r = monitor_channels.get(1).copied().unwrap_or(mon_l);
    let preview_channels = monitor_channels.len().max(1) as u32;
    let start_sample = shared.position_samples.load(Ordering::Relaxed);

    shared.preview_ring.reset();
    let preview_tracks = preview_tracks_for(&config.tracks, input_ch);
    {
        let mut ids = shared.recording_preview_track_ids.lock();
        ids.clear();
        for (slot, (track_id, _, _)) in preview_tracks.iter().enumerate() {
            shared.preview_rings[slot].reset();
            ids.push(track_id.clone());
        }
    }
    shared.recording_preview_id.fetch_add(1, Ordering::Relaxed);
    shared
        .recording_preview_start_sample
        .store(start_sample, Ordering::Relaxed);
    shared
        .recording_preview_sample_rate
        .store(sample_rate, Ordering::Relaxed);
    shared
        .recording_preview_channels
        .store(preview_channels, Ordering::Relaxed);
    shared
        .recording_preview_peaks_per_sec
        .store(PREVIEW_PEAKS_PER_SEC, Ordering::Relaxed);
    shared
        .recording_preview_active
        .store(true, Ordering::Relaxed);

    let first_capture_sample = Arc::new(AtomicU64::new(u64::MAX));
    let sink = crate::capture_tap::RecordSink {
        audio_tx: pipeline.audio_tx,
        free_rx: pipeline.free_rx,
        free_tx: pipeline.free_tx,
        samples_per_bin,
        capture_on_transport: config.capture_on_transport.unwrap_or(false),
        dropped_blocks: Arc::clone(&dropped_blocks),
        active: Arc::clone(&recording_active),
        first_capture_sample: Arc::clone(&first_capture_sample),
        monitor: crate::capture_tap::PreviewAccum::new(mon_l, mon_r),
        tracks: preview_tracks
            .iter()
            .map(|&(_, l, r)| crate::capture_tap::PreviewAccum::new(l, r))
            .collect(),
    };

    eprintln!(
        "[SphereAudio] Recording started (live-stream tap): {track_count} track(s), \
         {input_ch}ch input @ {sample_rate} Hz"
    );

    Ok((
        RecordingSession {
            capture: CaptureSource::LiveStreamTap,
            results_rx: pipeline.finalize_rx,
            stop_flag: pipeline.stop_flag,
            start_beat: config.start_beat,
            sample_rate,
            track_count,
            recording_active,
            dropped_blocks,
            started_at: std::time::Instant::now(),
            shared,
            armed_start_sample: start_sample,
            first_capture_sample: Arc::clone(&first_capture_sample),
        },
        sink,
    ))
}

/// Begin a take that taps the persistent ASIO session input instead of opening
/// a stream. Builds the writers + disk pipeline and returns the session along
/// with the [`RecordSink`] the engine must install into the session's input
/// callback (`AsioInputCommand::SetRecordSink`).
///
/// The ring/monitor flags are deliberately *not* touched here — with ASIO they
/// are owned by the engine's input-routing sync, and a take must not disturb
/// monitoring state.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn start_recording_asio_tap(
    config: JsStartRecordingConfig,
    shared: Arc<crate::engine::SharedState>,
    monitor_mix: bool,
    input_ch: u32,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<(RecordingSession, crate::backend::asio_session::RecordSink), SphereAudioError> {
    let input_ch = input_ch as usize;
    let track_writers = build_track_writers(&config, &shared, input_ch, sample_rate)?;
    let track_count = track_writers.len();

    // Pool sizing: ASIO block size is fixed and known, so size blocks to the
    // real callback length (with headroom) instead of the WASAPI worst case —
    // keeps a 32-in interface from preallocating hundreds of megabytes.
    let block_samples = input_ch
        .saturating_mul((buffer_frames.max(1) as usize).saturating_mul(4).max(4096))
        .max(input_ch.max(1));
    let pipeline = spawn_recording_pipeline(
        track_writers,
        sample_rate,
        input_ch,
        config.start_beat,
        block_samples,
        256,
    );

    let recording_active = Arc::new(AtomicBool::new(true));
    let dropped_blocks = Arc::new(AtomicU64::new(0));
    shared.recording_active.store(true, Ordering::Relaxed);
    shared
        .record_peak
        .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
    shared
        .recording_monitor_mix
        .store(monitor_mix, Ordering::Relaxed);

    // Realtime preview metadata (same contract as the own-stream path).
    const PREVIEW_PEAKS_PER_SEC: u32 = 150;
    let samples_per_bin = (sample_rate / PREVIEW_PEAKS_PER_SEC).max(1) as usize;
    let preview_channels = config.monitor_channels.len().clamp(1, 2) as u32;
    let start_sample = shared.position_samples.load(Ordering::Relaxed);
    shared.preview_ring.reset();

    // Which armed tracks the growing waveform can describe, and on which input
    // channels. The own-stream path has always published this; the ASIO tap did
    // not, so `recording_preview_tracks()` reported nothing and the UI tore its
    // preview down on the first poll of every take.
    let preview_tracks = preview_tracks_for(&config.tracks, input_ch);
    {
        let mut ids = shared.recording_preview_track_ids.lock();
        ids.clear();
        for (slot, (track_id, _, _)) in preview_tracks.iter().enumerate() {
            shared.preview_rings[slot].reset();
            ids.push(track_id.clone());
        }
    }
    shared.recording_preview_id.fetch_add(1, Ordering::Relaxed);
    shared
        .recording_preview_start_sample
        .store(start_sample, Ordering::Relaxed);
    shared
        .recording_preview_sample_rate
        .store(sample_rate, Ordering::Relaxed);
    shared
        .recording_preview_channels
        .store(preview_channels, Ordering::Relaxed);
    shared
        .recording_preview_peaks_per_sec
        .store(PREVIEW_PEAKS_PER_SEC, Ordering::Relaxed);
    shared
        .recording_preview_active
        .store(true, Ordering::Relaxed);

    // The ASIO session's own sink does not stamp its first captured block, so a
    // take on that path is placed at the sample it was armed on. The driver's
    // callback is fixed-size and already running, which is the case where the
    // two are closest together.
    let first_capture_sample = Arc::new(AtomicU64::new(u64::MAX));
    let sink = crate::backend::asio_session::RecordSink {
        audio_tx: pipeline.audio_tx,
        free_rx: pipeline.free_rx,
        free_tx: pipeline.free_tx,
        samples_per_bin,
        capture_on_transport: config.capture_on_transport.unwrap_or(false),
        dropped_blocks: Arc::clone(&dropped_blocks),
        preview_accums: preview_tracks
            .iter()
            .map(|&(_, l, r)| crate::backend::asio_session::PreviewAccum::new(l, r))
            .collect(),
    };

    eprintln!(
        "[SphereAudio] Recording started (ASIO tap): {track_count} track(s), \
         {input_ch}ch input @ {sample_rate} Hz"
    );

    Ok((
        RecordingSession {
            capture: CaptureSource::AsioSessionTap,
            results_rx: pipeline.finalize_rx,
            stop_flag: pipeline.stop_flag,
            start_beat: config.start_beat,
            sample_rate,
            track_count,
            recording_active,
            dropped_blocks,
            started_at: std::time::Instant::now(),
            shared,
            armed_start_sample: start_sample,
            first_capture_sample: Arc::clone(&first_capture_sample),
        },
        sink,
    ))
}

/// Stop recording, finalize RAUF files, and return per-track results.
/// A take whose files the disk writer is still finalizing.
///
/// Detaching a take is immediate — flags cleared, capture dropped — but the
/// writer then has to drain whatever is still queued and close its files, and
/// that is disk work of unbounded duration. It used to be waited on inline,
/// which meant the caller's thread stopped until the disk was done; on the UI
/// thread that is the application freezing at the end of every take.
///
/// So the wait is a value. [`begin_stop_recording`] hands one back, and the
/// caller chooses: [`Self::poll`] to check without waiting, or [`Self::wait`]
/// to block as before.
pub struct PendingRecordingStop {
    results_rx: std::sync::mpsc::Receiver<Vec<RecordingResult>>,
    shared: std::sync::Arc<crate::engine::SharedState>,
    dropped_blocks: u64,
    /// Guards against a `poll` after the results were already taken, which
    /// would otherwise report a finalization timeout for a take that finished.
    finished: bool,
}

impl PendingRecordingStop {
    /// Results if the writer has finished, `None` while it is still going.
    ///
    /// Never blocks. Returns `Some(Err(..))` only when the writer died without
    /// sending — a disconnected channel is a failed take, not a slow one.
    pub fn poll(&mut self) -> Option<Result<Vec<JsRecordingResult>, SphereAudioError>> {
        if self.finished {
            return None;
        }
        match self.results_rx.try_recv() {
            Ok(results) => {
                self.finished = true;
                Some(Ok(self.finish(results)))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.finished = true;
                Some(Err(SphereAudioError::NativeError(
                    "Recording writer stopped without finalizing the take".to_string(),
                )))
            }
        }
    }

    /// Block until the writer finishes, as the inline path always did.
    pub fn wait(mut self) -> Result<Vec<JsRecordingResult>, SphereAudioError> {
        if self.finished {
            return Ok(Vec::new());
        }
        // Wait up to 60 s for the disk writer to flush and finalize.
        let results = self
            .results_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .map_err(|e| {
                SphereAudioError::NativeError(format!("Recording finalization timed out: {e}"))
            })?;
        self.finished = true;
        Ok(self.finish(results))
    }

    /// Everything that happens once the files are closed. Identical whichever
    /// way the caller waited, which is the point of it being one function.
    fn finish(&self, mut results: Vec<RecordingResult>) -> Vec<JsRecordingResult> {
        // Round-trip latency the take must shift earlier by: the output play-out
        // delay the performer heard plus the ADC→callback capture delay. Both are
        // published from cpal timestamps during the take; either is 0 when the
        // backend gives no timestamp, in which case no compensation is applied.
        let latency_seconds = {
            let out =
                crate::engine::f32_load(self.shared.output_latency_secs.load(Ordering::Relaxed));
            let inp = crate::engine::f32_load(
                self.shared
                    .record_input_latency_secs
                    .load(Ordering::Relaxed),
            );
            (out + inp).clamp(0.0, 1.0) as f64
        };

        eprintln!(
            "[SphereAudio] Recording stopped: {} file(s) finalized (round-trip latency {:.1} ms)",
            results.len(),
            latency_seconds * 1000.0
        );

        if self.dropped_blocks > 0 {
            let dropped_blocks = self.dropped_blocks;
            for result in &mut results {
                result.success = false;
                result.error = Some(format!(
                    "Recording writer could not keep up; dropped {dropped_blocks} input block(s)"
                ));
            }
        }

        results
            .into_iter()
            .map(|r| JsRecordingResult {
                track_id: r.track_id,
                file_path: r.file_path,
                relative_path: r.relative_path,
                start_beat: r.start_beat,
                duration_seconds: r.duration_seconds,
                sample_rate: r.sample_rate,
                channels: r.channels,
                metadata_path: r.metadata_path,
                sample_format: r.sample_format,
                latency_seconds,
                success: r.success,
                error: r.error,
            })
            .collect()
    }
}

/// Detach a take and hand back its finalization.
///
/// Everything here is immediate: flags cleared, the capture source dropped, the
/// stop flag raised. What is left is the writer draining, and that is the
/// returned handle's business.
pub fn begin_stop_recording(session: RecordingSession) -> PendingRecordingStop {
    let owns_stream = session.owns_capture_stream();
    // Tell the callback to stop sending.
    session.recording_active.store(false, Ordering::Relaxed);
    session
        .shared
        .recording_active
        .store(false, Ordering::Relaxed);
    session
        .shared
        .recording_monitor_mix
        .store(false, Ordering::Relaxed);
    session
        .shared
        .recording_preview_active
        .store(false, Ordering::Relaxed);
    if owns_stream {
        // Own-stream capture doubled as the monitor source; releasing it must
        // also release the ring. A tapped take (ASIO session, or the persistent
        // cpal live input) borrows a stream whose ring and monitor flags are
        // owned by input-routing sync and survive the take — clearing them here
        // would silence monitoring the moment a take ends.
        session.shared.input_ring.set_active(false, 0, 0);
        session
            .shared
            .monitor_enabled_any
            .store(false, Ordering::Relaxed);
    }
    let dropped_blocks = session.dropped_blocks.load(Ordering::Relaxed);

    // Raise the stop flag first (the ASIO tap writer exits on it even if the
    // driver stalls), then drop the capture source: for an own stream this
    // disconnects `audio_tx` (it lived inside the closure), which causes the
    // disk writer's `recv` to return Disconnected → loop exits.
    session.stop_flag.store(true, Ordering::Relaxed);
    drop(session.capture);

    PendingRecordingStop {
        results_rx: session.results_rx,
        shared: session.shared,
        dropped_blocks,
        finished: false,
    }
}

/// Detach a take and wait for its files, on this thread.
///
/// The behaviour every caller had before the split, kept for the ones that
/// genuinely want to block — a shutdown path has nothing else to do, and a test
/// wants the files on the next line.
pub fn stop_recording(
    session: RecordingSession,
) -> Result<Vec<JsRecordingResult>, SphereAudioError> {
    begin_stop_recording(session).wait()
}

#[cfg(test)]
mod tests {
    use super::{reset_failed_own_stream_start, should_capture_recording};
    use crate::engine::{f32_load, f32_store, SharedState};
    use std::sync::atomic::Ordering;

    #[test]
    fn direct_recording_captures_without_transport() {
        assert!(should_capture_recording(true, false, false));
        assert!(should_capture_recording(true, false, true));
    }

    #[test]
    fn transport_gated_recording_waits_for_playback() {
        assert!(!should_capture_recording(true, true, false));
        assert!(should_capture_recording(true, true, true));
        assert!(!should_capture_recording(false, true, true));
    }

    /// The rule that makes "file size must not grow after Stop" hold for every
    /// Stop channel: the session's own capture flag is cleared by transport
    /// stop (`EngineInner::pause` → `end_recording_capture`), and a cleared
    /// flag ends capture whatever the transport is doing. Before that, a Stop
    /// that did not also call `stop_recording` left this true forever, the file
    /// kept growing, and the next Record continued the same take.
    #[test]
    fn clearing_the_session_flag_ends_capture_on_every_path() {
        for capture_on_transport in [false, true] {
            for transport_playing in [false, true] {
                assert!(
                    !should_capture_recording(false, capture_on_transport, transport_playing),
                    "capture_on_transport={capture_on_transport} playing={transport_playing}"
                );
            }
        }
    }

    #[test]
    fn failed_own_stream_start_clears_recording_state() {
        let shared = SharedState::default();
        shared.recording_active.store(true, Ordering::Relaxed);
        shared.recording_monitor_mix.store(true, Ordering::Relaxed);
        shared
            .recording_preview_active
            .store(true, Ordering::Relaxed);
        shared.live_input_active.store(true, Ordering::Relaxed);
        shared.monitor_enabled_any.store(true, Ordering::Relaxed);
        shared.record_peak.store(f32_store(0.75), Ordering::Relaxed);
        shared.input_ring.set_active(true, 2, 48_000);

        reset_failed_own_stream_start(&shared);

        assert!(!shared.recording_active.load(Ordering::Relaxed));
        assert!(!shared.recording_monitor_mix.load(Ordering::Relaxed));
        assert!(!shared.recording_preview_active.load(Ordering::Relaxed));
        assert!(!shared.live_input_active.load(Ordering::Relaxed));
        assert!(!shared.monitor_enabled_any.load(Ordering::Relaxed));
        assert!(!shared.input_ring.is_active());
        assert_eq!(f32_load(shared.record_peak.load(Ordering::Relaxed)), 0.0);
    }
}

/// The one derivation both capture paths use to decide what the growing
/// waveform can draw, and for which track.
///
/// It earns its own tests because getting it wrong is silent: the take still
/// records correctly to disk, and only the picture on the timeline is missing
/// or shows the wrong channel.
#[cfg(test)]
mod preview_track_tests {
    use super::preview_tracks_for;
    use crate::types::JsRecordingTrackConfig;

    fn track(id: &str, channels: &[u32]) -> JsRecordingTrackConfig {
        JsRecordingTrackConfig {
            track_id: id.to_string(),
            input_channels: channels.to_vec(),
            name: id.to_string(),
        }
    }

    #[test]
    fn each_track_previews_its_own_channels_not_the_monitor_pair() {
        let tracks = [
            track("vox", &[0, 1]),
            track("gtr", &[2, 3]),
            track("kick", &[4]),
        ];
        assert_eq!(
            preview_tracks_for(&tracks, 8),
            vec![
                ("vox".to_string(), 0, 1),
                ("gtr".to_string(), 2, 3),
                // A mono track draws its one channel on both sides rather than
                // pulling in whatever happens to sit next to it.
                ("kick".to_string(), 4, 4),
            ]
        );
    }

    /// Skipped, not clamped: a track pointed past the end of the device is not
    /// recording anything the preview could draw, and clamping would put
    /// somebody else's signal under its name.
    #[test]
    fn a_track_outside_the_devices_channel_count_is_skipped() {
        let tracks = [track("vox", &[0, 1]), track("ghost", &[8, 9])];
        assert_eq!(
            preview_tracks_for(&tracks, 2),
            vec![("vox".to_string(), 0, 1)]
        );
    }

    /// Half-outside is a mono preview of the channel that does exist, which is
    /// exactly what that track is recording.
    #[test]
    fn a_right_channel_outside_the_device_falls_back_to_the_left() {
        let tracks = [track("wide", &[1, 7])];
        assert_eq!(preview_tracks_for(&tracks, 2), vec![("wide".into(), 1, 1)]);
    }

    #[test]
    fn a_track_with_no_input_channel_has_nothing_to_preview() {
        assert!(preview_tracks_for(&[track("silent", &[])], 8).is_empty());
    }

    /// The slot table the rings live in is fixed, so the derivation has to stop
    /// at its length rather than hand out an index that would panic.
    #[test]
    fn the_slot_table_bounds_how_many_tracks_preview() {
        let tracks: Vec<_> = (0..crate::input_ring::MAX_RECORDING_PREVIEW_TRACKS + 4)
            .map(|i| track(&format!("t{i}"), &[0, 1]))
            .collect();
        assert_eq!(
            preview_tracks_for(&tracks, 2).len(),
            crate::input_ring::MAX_RECORDING_PREVIEW_TRACKS
        );
    }
}
