//! Arrangement exporter: render offline → encode with `sphere_encoder` → write
//! atomically. Streaming, cancellable, progress-reporting.

use std::path::{Path, PathBuf};

use sphere_encoder::{
    create_encoder, AudioEncodeOptions, AudioEncodeSpec, AudioFileFormat, AudioSampleFormat,
};

use crate::plugin_bridge::PluginBridgeSinkMap;
use crate::types::EngineProjectSnapshot;

use super::level_meter::{LevelMeter, LevelReport};
use super::offline_renderer::{
    render_offline_mix_and_tracks_with_bridges, render_offline_with_bridges,
};
use super::render_progress::{ExportCancelToken, ExportProgress, ExportStage};
use super::render_request::{ExportNormalizeMode, ExportTailMode, OfflineRenderRequest};
use super::ExportError;

/// A full arrangement export request: where to write, in what container, and
/// how to render.
#[derive(Debug, Clone)]
pub struct ArrangementExportRequest {
    pub output_path: PathBuf,
    pub format: AudioFileFormat,
    pub sample_format: AudioSampleFormat,
    pub render: OfflineRenderRequest,
    /// Per-format encoder options (WAV/FLAC/MP3 + metadata).
    pub encode_options: AudioEncodeOptions,
}

#[derive(Debug, Clone)]
pub struct TrackExportTarget {
    pub track_id: String,
    pub request: ArrangementExportRequest,
}

/// What one rendered file came to.
#[derive(Debug, Clone)]
pub struct ArrangementExportSummary {
    pub output_path: PathBuf,
    pub format: AudioFileFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames_written: u64,
    pub duration_seconds: f64,
    /// Sample peak, dBFS (the same value as `levels.sample_peak_db`).
    pub peak_db: Option<f32>,
    /// Peaks, loudness and overs, measured on the samples given to the
    /// encoder.
    pub levels: LevelReport,
    /// The file's sample format cannot hold samples above full scale (every
    /// integer format, MP3), so the overs in `levels` were clipped flat in the
    /// file itself. `false` for 32-bit float WAV, which keeps them.
    pub overs_clamped: bool,
}

/// Temp path an export writes to before atomically replacing the final file.
pub fn partial_path_for(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_os_string();
    s.push(".partial");
    PathBuf::from(s)
}

/// Whether a file of this format keeps samples above full scale.
fn keeps_overs(format: AudioFileFormat, sample_format: AudioSampleFormat) -> bool {
    matches!(format, AudioFileFormat::Wav) && matches!(sample_format, AudioSampleFormat::F32)
}

/// One render: the stereo mixdown, any number of mixer channels, or both,
/// from a single pass of the graph — plug-ins and routing are processed once
/// however many files come out. Every file shares the render geometry (rate,
/// channels, range, tail); only the mixdown can be normalized, since stems
/// normalized one by one would no longer sum to the mix.
#[derive(Debug, Clone, Default)]
pub struct RenderJob {
    pub mixdown: Option<ArrangementExportRequest>,
    pub tracks: Vec<TrackExportTarget>,
}

impl RenderJob {
    fn render(&self) -> Option<&OfflineRenderRequest> {
        self.mixdown
            .as_ref()
            .map(|mixdown| &mixdown.render)
            .or_else(|| self.tracks.first().map(|track| &track.request.render))
    }

    /// Output paths in report order: the mixdown, then the tracks.
    fn requests(&self) -> impl Iterator<Item = &ArrangementExportRequest> {
        self.mixdown
            .iter()
            .chain(self.tracks.iter().map(|track| &track.request))
    }
}

/// Export the arrangement to `request.output_path`.
///
/// Flow: Preparing → (AnalyzingPeak if normalizing) → Rendering/Encoding →
/// Finalizing → Complete. Writes to a `.partial` temp file and only replaces the
/// final output once the encoder finalizes successfully. On cancel or error the
/// partial file is removed and an existing final file is left untouched.
pub fn export_arrangement(
    snapshot: &EngineProjectSnapshot,
    request: &ArrangementExportRequest,
    cancel: &ExportCancelToken,
    on_progress: impl FnMut(ExportProgress),
) -> Result<ArrangementExportSummary, ExportError> {
    export_arrangement_with_bridges(snapshot, request, cancel, None, on_progress)
}

pub fn export_arrangement_with_bridges(
    snapshot: &EngineProjectSnapshot,
    request: &ArrangementExportRequest,
    cancel: &ExportCancelToken,
    bridge_sinks: Option<&PluginBridgeSinkMap>,
    on_progress: impl FnMut(ExportProgress),
) -> Result<ArrangementExportSummary, ExportError> {
    let job = RenderJob {
        mixdown: Some(request.clone()),
        tracks: Vec::new(),
    };
    export_render_job_with_bridges(snapshot, &job, cancel, bridge_sinks, on_progress)?
        .pop()
        .ok_or_else(|| ExportError::Build("the mixdown produced no file".to_string()))
}

/// Export mixer-channel taps in one offline graph/timeline pass. Each target is
/// encoded independently, but plug-ins and routing are processed only once.
pub fn export_tracks_single_pass(
    snapshot: &EngineProjectSnapshot,
    targets: &[TrackExportTarget],
    cancel: &ExportCancelToken,
    on_progress: impl FnMut(ExportProgress),
) -> Result<Vec<ArrangementExportSummary>, ExportError> {
    export_tracks_single_pass_with_bridges(snapshot, targets, cancel, None, on_progress)
}

pub fn export_tracks_single_pass_with_bridges(
    snapshot: &EngineProjectSnapshot,
    targets: &[TrackExportTarget],
    cancel: &ExportCancelToken,
    bridge_sinks: Option<&PluginBridgeSinkMap>,
    on_progress: impl FnMut(ExportProgress),
) -> Result<Vec<ArrangementExportSummary>, ExportError> {
    let job = RenderJob {
        mixdown: None,
        tracks: targets.to_vec(),
    };
    export_render_job_with_bridges(snapshot, &job, cancel, bridge_sinks, on_progress)
}

pub fn export_render_job(
    snapshot: &EngineProjectSnapshot,
    job: &RenderJob,
    cancel: &ExportCancelToken,
    on_progress: impl FnMut(ExportProgress),
) -> Result<Vec<ArrangementExportSummary>, ExportError> {
    export_render_job_with_bridges(snapshot, job, cancel, None, on_progress)
}

/// Render `job` and write its files: the mixdown first, then the tracks in
/// order, in the returned list too. Each file is written to a `.partial` and
/// moved into place only once every encoder finalized; on cancel or error no
/// partial is left behind and existing files are untouched.
pub fn export_render_job_with_bridges(
    snapshot: &EngineProjectSnapshot,
    job: &RenderJob,
    cancel: &ExportCancelToken,
    bridge_sinks: Option<&PluginBridgeSinkMap>,
    mut on_progress: impl FnMut(ExportProgress),
) -> Result<Vec<ArrangementExportSummary>, ExportError> {
    let Some(render) = job.render().cloned() else {
        return Ok(Vec::new());
    };
    render.validate().map_err(ExportError::Settings)?;
    for request in job.requests() {
        let other = &request.render;
        if other.sample_rate != render.sample_rate
            || other.channels != render.channels
            || other.start_sample != render.start_sample
            || other.end_sample != render.end_sample
        {
            return Err(ExportError::Settings(
                "every file of one render must share its rate, channels and range".to_string(),
            ));
        }
    }
    if job
        .tracks
        .iter()
        .any(|target| !matches!(target.request.render.normalize, ExportNormalizeMode::None))
    {
        return Err(ExportError::Settings(
            "normalization is unavailable for single-pass stem export".to_string(),
        ));
    }
    if let Some(mixdown) = &job.mixdown {
        if let Some(parent) = mixdown.output_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(ExportError::Settings(format!(
                    "output directory does not exist: {}",
                    parent.display()
                )));
            }
        }
    }
    let track_indices: Vec<usize> = job
        .tracks
        .iter()
        .map(|target| {
            snapshot
                .tracks
                .iter()
                .position(|track| track.id == target.track_id)
                .ok_or_else(|| {
                    ExportError::Settings(format!("export track not found: {}", target.track_id))
                })
        })
        .collect::<Result<_, _>>()?;

    let total = render
        .content_frames()
        .saturating_add(render.max_tail_frames());
    on_progress(ExportProgress::stage_only(ExportStage::Preparing, total));

    // ── Pass 1 (optional): peak analysis for the mixdown's normalization ────
    let gain = match job.mixdown.as_ref().map(|mixdown| mixdown.render.normalize) {
        Some(ExportNormalizeMode::PeakDb(target_db)) => {
            on_progress(ExportProgress::stage_only(
                ExportStage::AnalyzingPeak,
                total,
            ));
            let analysis = render_offline_with_bridges(
                snapshot,
                &render,
                cancel,
                1.0,
                bridge_sinks,
                |_block| Ok(()),
                |_p| {},
            )?;
            if analysis.peak <= f32::EPSILON {
                1.0
            } else {
                let target_lin = 10f32.powf(target_db / 20.0);
                (target_lin / analysis.peak).clamp(0.0, 64.0)
            }
        }
        _ => 1.0,
    };

    // ── Encoders: one per file, each into its own .partial ─────────────────
    let requests: Vec<&ArrangementExportRequest> = job.requests().collect();
    let (partials, mut encoders) = open_file_encoders(&requests)?;
    let mut meters: Vec<LevelMeter> = requests
        .iter()
        .map(|request| LevelMeter::new(request.render.channels, request.render.sample_rate))
        .collect();

    // ── One pass: the master into the mixdown, the taps into the tracks ─────
    on_progress(ExportProgress::new(ExportStage::Encoding, 0, total));
    let has_mixdown = job.mixdown.is_some();
    let track_offset = usize::from(has_mixdown);
    let channels = render.channels as usize;
    let mut mono_scratch = vec![Vec::<f32>::new(); job.tracks.len()];
    let (mix_encoders, track_encoders) = encoders.split_at_mut(track_offset);
    let (mix_meters, track_meters) = meters.split_at_mut(track_offset);
    let render_result = render_offline_mix_and_tracks_with_bridges(
        snapshot,
        &render,
        cancel,
        gain,
        bridge_sinks,
        !job.tracks.is_empty(),
        &mut |block| {
            if let (Some(encoder), Some(meter)) = (mix_encoders.first_mut(), mix_meters.first_mut())
            {
                meter.feed(block);
                encoder.write_interleaved_f32(block)?;
            }
            Ok(())
        },
        &mut |taps, frames| {
            for (target_index, &track_index) in track_indices.iter().enumerate() {
                let tap = taps
                    .get(track_index)
                    .ok_or_else(|| ExportError::Build("missing offline track tap".to_string()))?;
                let samples: &[f32] = if channels == 1 {
                    let mono = &mut mono_scratch[target_index];
                    mono.resize(frames, 0.0);
                    for frame in 0..frames {
                        mono[frame] = (tap[frame * 2] + tap[frame * 2 + 1]) * 0.5;
                    }
                    mono
                } else {
                    &tap[..frames * 2]
                };
                track_meters[target_index].feed(samples);
                track_encoders[target_index].write_interleaved_f32(samples)?;
            }
            Ok(())
        },
        &mut |progress| {
            // Render and encode are one streaming pass.
            on_progress(ExportProgress::new(
                ExportStage::Encoding,
                progress.rendered_frames,
                progress.total_frames,
            ));
        },
    );
    if let Err(error) = render_result {
        discard_files(&partials, encoders);
        return Err(error);
    }

    // ── Finalize every file, then move each into place ─────────────────────
    on_progress(ExportProgress::new(ExportStage::Finalizing, total, total));
    let summaries = finalize_files(&requests, &partials, encoders, meters)?;
    on_progress(ExportProgress::stage_only(ExportStage::Complete, total));
    Ok(summaries)
}

type FileEncoder = Box<dyn sphere_encoder::AudioEncoder>;

/// One encoder per request, each writing to its own `.partial`. On failure the
/// handles already open are closed and every partial so far is removed.
fn open_file_encoders(
    requests: &[&ArrangementExportRequest],
) -> Result<(Vec<PathBuf>, Vec<FileEncoder>), ExportError> {
    let mut partials: Vec<PathBuf> = Vec::with_capacity(requests.len());
    let mut encoders: Vec<FileEncoder> = Vec::with_capacity(requests.len());
    for request in requests {
        let setup = (|| -> Result<_, ExportError> {
            if let Some(parent) = request.output_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let partial = partial_path_for(&request.output_path);
            let _ = std::fs::remove_file(&partial);
            let spec = AudioEncodeSpec {
                sample_rate: request.render.sample_rate,
                channels: request.render.channels,
                sample_format: request.sample_format,
            };
            let encoder = create_encoder(&partial, spec, request.encode_options.clone())?;
            Ok((partial, encoder))
        })();
        match setup {
            Ok((partial, encoder)) => {
                encoders.push(encoder);
                partials.push(partial);
            }
            Err(error) => {
                // Close the handles already open (Windows cannot delete an
                // open file), then sweep every partial so far, this one too.
                drop(encoders);
                for partial in &partials {
                    let _ = std::fs::remove_file(partial);
                }
                let _ = std::fs::remove_file(partial_path_for(&request.output_path));
                return Err(error);
            }
        }
    }
    Ok((partials, encoders))
}

/// Remove every partial after a render that did not finish.
fn discard_files(partials: &[PathBuf], encoders: Vec<FileEncoder>) {
    drop(encoders);
    for partial in partials {
        let _ = std::fs::remove_file(partial);
    }
}

/// Finalize each encoder and move its partial into place, in request order.
fn finalize_files(
    requests: &[&ArrangementExportRequest],
    partials: &[PathBuf],
    encoders: Vec<FileEncoder>,
    meters: Vec<LevelMeter>,
) -> Result<Vec<ArrangementExportSummary>, ExportError> {
    let mut summaries = Vec::with_capacity(requests.len());
    let mut encoders = encoders.into_iter();
    for (index, ((request, partial), meter)) in
        requests.iter().zip(partials.iter()).zip(meters).enumerate()
    {
        let mut encoder = encoders
            .next()
            .ok_or_else(|| ExportError::Build("missing encoder for export target".to_string()))?;
        let finalized = (|| -> Result<_, ExportError> {
            let encoded = encoder.finalize()?;
            if request.output_path.exists() {
                std::fs::remove_file(&request.output_path)?;
            }
            std::fs::rename(partial, &request.output_path)?;
            Ok(encoded)
        })();
        let encoded = match finalized {
            Ok(encoded) => encoded,
            Err(error) => {
                // Close every remaining handle first (Windows cannot delete
                // open files), then sweep this and the partials still to go.
                // Files already moved into place are complete and stay.
                drop(encoder);
                drop(encoders);
                for partial in &partials[index..] {
                    let _ = std::fs::remove_file(partial);
                }
                return Err(error);
            }
        };
        let levels = meter.finish();
        summaries.push(ArrangementExportSummary {
            output_path: request.output_path.clone(),
            format: request.format,
            sample_rate: encoded.sample_rate,
            channels: encoded.channels,
            frames_written: encoded.frames_written,
            duration_seconds: encoded.frames_written as f64 / encoded.sample_rate.max(1) as f64,
            peak_db: levels.sample_peak_db,
            overs_clamped: levels.clips() && !keeps_overs(request.format, request.sample_format),
            levels,
        });
    }
    Ok(summaries)
}

/// How long a realtime render waits for playback to deliver its first block,
/// or its next one, before giving up.
const REALTIME_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Record `job` from a realtime capture while the project plays: the master
/// into the mixdown, lane `k + 1` into `job.tracks[k]`. Runs on the writer
/// thread; the caller starts playback at `start_sample` (the render's own
/// start) and stops it once this returns.
///
/// `latency_frames` of output are skipped from `start_sample`, as an offline
/// render discards its warm-up, so the files line up with the timeline. A
/// render the capture could not keep up with, or whose playback moved (a
/// seek, a stop, a loop), fails rather than writing a file with a hole in it.
pub fn record_render_job(
    capture: &crate::render_capture::RenderCapture,
    job: &RenderJob,
    latency_frames: u64,
    cancel: &ExportCancelToken,
    mut on_progress: impl FnMut(ExportProgress),
) -> Result<Vec<ArrangementExportSummary>, ExportError> {
    let Some(render) = job.render().cloned() else {
        return Ok(Vec::new());
    };
    render.validate().map_err(ExportError::Settings)?;
    if render.sample_rate != capture.sample_rate() {
        return Err(ExportError::Settings(format!(
            "a realtime render is recorded at the device rate ({} Hz), not {} Hz",
            capture.sample_rate(),
            render.sample_rate
        )));
    }
    if job
        .requests()
        .any(|request| !matches!(request.render.normalize, ExportNormalizeMode::None))
    {
        return Err(ExportError::Settings(
            "a realtime render cannot be normalized: it is recorded in one pass".to_string(),
        ));
    }
    if job.tracks.len() + 1 > capture.lanes() {
        return Err(ExportError::Settings(
            "the capture carries fewer tracks than the render asks for".to_string(),
        ));
    }

    let content = render.content_frames();
    let tail_cap = render.max_tail_frames();
    let total = content.saturating_add(tail_cap);
    let write_from = render.start_sample.saturating_add(latency_frames);
    let content_end = write_from.saturating_add(content);
    let write_to = content_end.saturating_add(tail_cap);
    let until_silence = match render.tail {
        ExportTailMode::UntilSilence { threshold_db, .. } => {
            Some(10f32.powf(threshold_db / 20.0).clamp(0.0, 1.0))
        }
        _ => None,
    };
    on_progress(ExportProgress::stage_only(ExportStage::Preparing, total));

    let requests: Vec<&ArrangementExportRequest> = job.requests().collect();
    let (partials, mut encoders) = open_file_encoders(&requests)?;
    let mut meters: Vec<LevelMeter> = requests
        .iter()
        .map(|request| LevelMeter::new(request.render.channels, request.render.sample_rate))
        .collect();
    // File index → capture lane.
    let lanes: Vec<usize> = job
        .mixdown
        .iter()
        .map(|_| 0)
        .chain((0..job.tracks.len()).map(|k| k + 1))
        .collect();
    let channels = usize::from(render.channels.max(1));
    let mut lane_buffers = vec![Vec::<f32>::new(); capture.lanes()];
    let mut out = Vec::<f32>::new();
    let chunk = (capture.sample_rate() as usize / 20).max(256);
    let mut next_sample: Option<u64> = None; // transport sample of the next frame read
    let mut last_data = std::time::Instant::now();

    let result = (|| -> Result<(), ExportError> {
        loop {
            if cancel.is_cancelled() {
                return Err(ExportError::Cancelled);
            }
            if capture.dropped_frames() > 0 {
                return Err(ExportError::Build(format!(
                    "the realtime render fell behind and lost {} frames; try a larger \
                     buffer size or an offline render",
                    capture.dropped_frames()
                )));
            }
            if capture.discontinuities() > 0 {
                return Err(ExportError::Build(
                    "playback moved during the realtime render (a seek, stop or loop)".to_string(),
                ));
            }
            let frames = capture.read(&mut lane_buffers, chunk);
            if frames == 0 {
                if last_data.elapsed() > REALTIME_STALL_TIMEOUT {
                    return Err(ExportError::Build(
                        "playback did not deliver audio for the realtime render".to_string(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            last_data = std::time::Instant::now();
            let chunk_start = match next_sample {
                Some(sample) => sample,
                None => {
                    let first = capture.first_sample().unwrap_or(render.start_sample);
                    // Started past the render's start: the frames it missed
                    // are silence, not a shorter file.
                    let missing = first.saturating_sub(write_from);
                    if missing > 0 {
                        let silence = vec![0.0f32; missing as usize * channels];
                        for (file, encoder) in encoders.iter_mut().enumerate() {
                            meters[file].feed(&silence);
                            encoder.write_interleaved_f32(&silence)?;
                        }
                    }
                    first
                }
            };
            next_sample = Some(chunk_start + frames as u64);
            // The part of this chunk inside [write_from, write_to).
            let from = write_from.saturating_sub(chunk_start).min(frames as u64) as usize;
            let to = write_to.saturating_sub(chunk_start).min(frames as u64) as usize;
            if to > from {
                for (file, &lane) in lanes.iter().enumerate() {
                    let stereo = &lane_buffers[lane][from * 2..to * 2];
                    out.clear();
                    if channels == 1 {
                        out.extend(stereo.chunks_exact(2).map(|pair| (pair[0] + pair[1]) * 0.5));
                    } else {
                        for pair in stereo.chunks_exact(2) {
                            out.extend_from_slice(pair);
                            out.extend(std::iter::repeat(0.0).take(channels - 2));
                        }
                    }
                    meters[file].feed(&out);
                    encoders[file].write_interleaved_f32(&out)?;
                }
            }
            let reached = chunk_start + frames as u64;
            let written = reached.saturating_sub(write_from).min(total);
            on_progress(ExportProgress::new(
                ExportStage::Rendering,
                written,
                total.max(1),
            ));
            if reached >= write_to {
                return Ok(());
            }
            // UntilSilence: past the content, stop on the first quiet chunk.
            if let Some(threshold) = until_silence {
                if chunk_start >= content_end {
                    let master_peak = lane_buffers[0][..frames * 2]
                        .iter()
                        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                    if master_peak < threshold {
                        return Ok(());
                    }
                }
            }
        }
    })();
    if let Err(error) = result {
        discard_files(&partials, encoders);
        return Err(error);
    }
    on_progress(ExportProgress::new(ExportStage::Finalizing, total, total));
    let summaries = finalize_files(&requests, &partials, encoders, meters)?;
    on_progress(ExportProgress::stage_only(ExportStage::Complete, total));
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::offline_renderer::{make_track_snapshot, silence_snapshot};
    use crate::export::render_request::{ExportNormalizeMode, ExportTailMode};
    use sphere_encoder::AudioEncodeOptions;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "futureboard-export-{name}-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    fn wav_request(output: PathBuf, end: u64) -> ArrangementExportRequest {
        let encode_options = AudioEncodeOptions {
            format: AudioFileFormat::Wav,
            ..Default::default()
        };
        ArrangementExportRequest {
            output_path: output,
            format: AudioFileFormat::Wav,
            sample_format: AudioSampleFormat::F32,
            render: OfflineRenderRequest {
                sample_rate: 48_000,
                channels: 2,
                start_sample: 0,
                end_sample: end,
                master_volume: 1.0,
                block_size: 256,
                tail: ExportTailMode::None,
                normalize: ExportNormalizeMode::None,
            },
            encode_options,
        }
    }

    #[test]
    fn exports_silence_to_wav_atomically() {
        let out = temp_path("ok");
        let req = wav_request(out.clone(), 1000);
        let snapshot = silence_snapshot(48_000);
        let cancel = ExportCancelToken::new();
        let summary = export_arrangement(&snapshot, &req, &cancel, |_p| {}).unwrap();
        assert_eq!(summary.frames_written, 1000);
        assert!(out.exists());
        assert!(
            !partial_path_for(&out).exists(),
            "partial should be renamed away"
        );
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn single_pass_track_export_writes_all_targets() {
        let first = temp_path("single-pass-a");
        let second = temp_path("single-pass-b");
        let targets = vec![
            TrackExportTarget {
                track_id: "track-1".to_string(),
                request: wav_request(first.clone(), 1_000),
            },
            TrackExportTarget {
                track_id: "track-1".to_string(),
                request: wav_request(second.clone(), 1_000),
            },
        ];
        let snapshot = silence_snapshot(48_000);
        let summaries = export_tracks_single_pass(
            &snapshot,
            &targets,
            &ExportCancelToken::new(),
            |_progress| {},
        )
        .unwrap();

        assert_eq!(summaries.len(), 2);
        assert!(summaries
            .iter()
            .all(|summary| summary.frames_written == 1_000));
        for output in [first, second] {
            assert_eq!(&std::fs::read(&output).unwrap()[..4], b"RIFF");
            assert!(!partial_path_for(&output).exists());
            let _ = std::fs::remove_file(output);
        }
    }

    /// Deterministic stand-in for the shared-memory plugin host: a 4-channel
    /// multi-out "instrument" that emits constant per-channel values with the
    /// real one-block freshness contract (each produced block is handed out at
    /// most once; a read before the next `request_block` returns 0).
    #[derive(Debug, Default)]
    struct ConstantMultiOutSink {
        fresh: std::sync::atomic::AtomicBool,
    }

    impl ConstantMultiOutSink {
        const CHANNELS: usize = 4;

        /// Channel `c` (0-based) carries the constant `0.1 * (c + 1)`.
        fn channel_value(channel: usize) -> f32 {
            0.1 * (channel + 1) as f32
        }
    }

    impl crate::plugin_bridge::PluginBridgeSink for ConstantMultiOutSink {
        fn dsp_ready(&self) -> bool {
            true
        }
        fn plugin_output_channels(&self) -> u32 {
            Self::CHANNELS as u32
        }
        fn read_output(&self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) -> usize {
            if !self.fresh.swap(false, std::sync::atomic::Ordering::AcqRel) {
                return 0;
            }
            let n = frames.min(out_l.len()).min(out_r.len());
            out_l[..n].fill(Self::channel_value(0));
            out_r[..n].fill(Self::channel_value(1));
            n
        }
        fn read_output_multichannel(&self, out: &mut [f32], frames: usize) -> (usize, usize) {
            if !self.fresh.swap(false, std::sync::atomic::Ordering::AcqRel) {
                return (0, 0);
            }
            let n = frames.min(out.len() / Self::CHANNELS);
            for frame in 0..n {
                for channel in 0..Self::CHANNELS {
                    out[frame * Self::CHANNELS + channel] = Self::channel_value(channel);
                }
            }
            (n, Self::CHANNELS)
        }
        fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}
        fn write_input(&self, _: &[f32], _: &[f32], _: usize) {}
        fn request_block(&self, _frames: u32) {
            self.fresh.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// End-to-end single-pass stem export through a bridged multi-out
    /// instrument: parent VSTi track plus two `vsti-out:` child bus strips, all
    /// captured from ONE offline graph pass. Verifies the offline bridge
    /// handoff produces non-silent files (the Addictive Drums silence bug) and
    /// that each child gets exactly its own channel pair.
    #[test]
    fn single_pass_bridged_multiout_stems_are_not_silent() {
        use crate::types::{EngineInsertSnapshot, EngineTrackSnapshot};
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut snapshot = silence_snapshot(48_000);
        let child_a_id = "vsti-out:insert-1:bus:0".to_string();
        let child_b_id = "vsti-out:insert-1:bus:1".to_string();

        let mut params: HashMap<String, serde_json::Value> = HashMap::new();
        params.insert("role".to_string(), serde_json::json!("instrument"));
        params.insert(
            "vstiOutputChildren".to_string(),
            serde_json::json!([
                { "trackId": child_a_id, "channelL": 1, "channelR": 2, "busIndex": 0 },
                { "trackId": child_b_id, "channelL": 3, "channelR": 4, "busIndex": 1 },
            ]),
        );
        snapshot.tracks[0].track_type = "instrument".to_string();
        snapshot.tracks[0].inserts.push(EngineInsertSnapshot {
            id: "insert-1".to_string(),
            kind: "external-bridge-plugin".to_string(),
            enabled: true,
            params,
            state: None,
        });
        for child_id in [&child_a_id, &child_b_id] {
            snapshot.tracks.push(EngineTrackSnapshot {
                track_type: "bus".to_string(),
                ..make_track_snapshot(child_id)
            });
        }

        let mut bridge_sinks = crate::plugin_bridge::PluginBridgeSinkMap::new();
        bridge_sinks.insert(
            "insert-1".to_string(),
            Arc::new(ConstantMultiOutSink::default())
                as crate::plugin_bridge::SharedPluginBridgeSink,
        );

        let parent_out = temp_path("multiout-parent");
        let child_a_out = temp_path("multiout-child-a");
        let child_b_out = temp_path("multiout-child-b");
        let targets = vec![
            TrackExportTarget {
                track_id: "track-1".to_string(),
                request: wav_request(parent_out.clone(), 1_000),
            },
            TrackExportTarget {
                track_id: child_a_id,
                request: wav_request(child_a_out.clone(), 1_000),
            },
            TrackExportTarget {
                track_id: child_b_id,
                request: wav_request(child_b_out.clone(), 1_000),
            },
        ];

        let summaries = export_tracks_single_pass_with_bridges(
            &snapshot,
            &targets,
            &ExportCancelToken::new(),
            Some(&bridge_sinks),
            |_progress| {},
        )
        .unwrap();

        assert_eq!(summaries.len(), 3);
        assert!(summaries
            .iter()
            .all(|summary| summary.frames_written == 1_000));
        for output in [&parent_out, &child_a_out, &child_b_out] {
            assert_eq!(&std::fs::read(output).unwrap()[..4], b"RIFF");
            assert!(!partial_path_for(output).exists());
        }

        // With explicit child routes the parent instrument strip receives no
        // fallback downmix — its stem is silent by contract.
        assert_eq!(
            summaries[0].peak_db, None,
            "parent with explicit multi-out children must not receive a downmix"
        );
        // Each child stem carries exactly its own channel pair: A = plugin
        // channels 1/2 (0.1/0.2), B = channels 3/4 (0.3/0.4). Unity fader,
        // centered pan → the tap peak is the pair's larger constant.
        let peak_lin = |db: Option<f32>| db.map(|d| 10f32.powf(d / 20.0)).unwrap_or(0.0);
        let child_a_peak = peak_lin(summaries[1].peak_db);
        let child_b_peak = peak_lin(summaries[2].peak_db);
        assert!(
            (child_a_peak - 0.2).abs() < 1e-3,
            "child A should carry plugin channels 1/2, got peak {child_a_peak}"
        );
        assert!(
            (child_b_peak - 0.4).abs() < 1e-3,
            "child B should carry plugin channels 3/4, got peak {child_b_peak}"
        );

        for output in [parent_out, child_a_out, child_b_out] {
            let _ = std::fs::remove_file(output);
        }
    }

    /// Bridged multi-out instrument (parent + two `vsti-out:` child bus strips)
    /// with the solo flags under test. `ConstantMultiOutSink` writes a constant
    /// per plugin channel: ch1=0.1, ch2=0.2, ch3=0.3, ch4=0.4, so child A peaks
    /// at 0.2 and child B at 0.4 when audible, and 0 when solo silences them.
    fn multiout_solo_snapshot(
        parent_solo: bool,
        child_solos: [bool; 2],
    ) -> (
        EngineProjectSnapshot,
        crate::plugin_bridge::PluginBridgeSinkMap,
        [String; 2],
    ) {
        use crate::types::{EngineInsertSnapshot, EngineTrackSnapshot};
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut snapshot = silence_snapshot(48_000);
        let child_ids = [
            "vsti-out:insert-1:bus:0".to_string(),
            "vsti-out:insert-1:bus:1".to_string(),
        ];

        let mut params: HashMap<String, serde_json::Value> = HashMap::new();
        params.insert("role".to_string(), serde_json::json!("instrument"));
        params.insert(
            "vstiOutputChildren".to_string(),
            serde_json::json!([
                { "trackId": child_ids[0], "channelL": 1, "channelR": 2, "busIndex": 0 },
                { "trackId": child_ids[1], "channelL": 3, "channelR": 4, "busIndex": 1 },
            ]),
        );
        snapshot.tracks[0].track_type = "instrument".to_string();
        snapshot.tracks[0].solo = parent_solo;
        snapshot.tracks[0].inserts.push(EngineInsertSnapshot {
            id: "insert-1".to_string(),
            kind: "external-bridge-plugin".to_string(),
            enabled: true,
            params,
            state: None,
        });
        for (child_id, solo) in child_ids.iter().zip(child_solos) {
            snapshot.tracks.push(EngineTrackSnapshot {
                track_type: "bus".to_string(),
                solo,
                ..make_track_snapshot(child_id)
            });
        }

        let mut bridge_sinks = crate::plugin_bridge::PluginBridgeSinkMap::new();
        bridge_sinks.insert(
            "insert-1".to_string(),
            Arc::new(ConstantMultiOutSink::default())
                as crate::plugin_bridge::SharedPluginBridgeSink,
        );
        (snapshot, bridge_sinks, child_ids)
    }

    /// Peak of each `vsti-out:` child stem, in linear amplitude, from one
    /// single-pass export of the snapshot.
    fn multiout_child_peaks(
        snapshot: &EngineProjectSnapshot,
        bridge_sinks: &crate::plugin_bridge::PluginBridgeSinkMap,
        child_ids: &[String; 2],
    ) -> [f32; 2] {
        let outputs = [temp_path("solo-child-a"), temp_path("solo-child-b")];
        let targets: Vec<TrackExportTarget> = child_ids
            .iter()
            .zip(&outputs)
            .map(|(track_id, output)| TrackExportTarget {
                track_id: track_id.clone(),
                request: wav_request(output.clone(), 1_000),
            })
            .collect();

        let summaries = export_tracks_single_pass_with_bridges(
            snapshot,
            &targets,
            &ExportCancelToken::new(),
            Some(bridge_sinks),
            |_progress| {},
        )
        .unwrap();

        for output in outputs {
            let _ = std::fs::remove_file(output);
        }
        let peak_lin = |db: Option<f32>| db.map(|d| 10f32.powf(d / 20.0)).unwrap_or(0.0);
        [
            peak_lin(summaries[0].peak_db),
            peak_lin(summaries[1].peak_db),
        ]
    }

    /// Solo on the main VSTi track is a solo of the whole instrument: every one
    /// of its separate-output channels stays audible. The child strips carry no
    /// solo flag of their own here — before the parent link existed they were
    /// all silenced the moment anything was soloed, so soloing a multi-out
    /// instrument from its main track produced silence.
    #[test]
    fn soloing_parent_vsti_track_sounds_every_multi_out_channel() {
        let (snapshot, bridge_sinks, child_ids) = multiout_solo_snapshot(true, [false, false]);
        let [child_a_peak, child_b_peak] =
            multiout_child_peaks(&snapshot, &bridge_sinks, &child_ids);
        assert!(
            (child_a_peak - 0.2).abs() < 1e-3,
            "parent solo must keep channel pair 1/2 audible, got peak {child_a_peak}"
        );
        assert!(
            (child_b_peak - 0.4).abs() < 1e-3,
            "parent solo must keep channel pair 3/4 audible, got peak {child_b_peak}"
        );
    }

    /// The other half of the contract: a channel soloed on its own is heard by
    /// itself, so a single drum pad / bus can still be auditioned even though
    /// its parent instrument is not soloed.
    #[test]
    fn soloing_one_multi_out_channel_isolates_that_channel() {
        let (snapshot, bridge_sinks, child_ids) = multiout_solo_snapshot(false, [false, true]);
        let [child_a_peak, child_b_peak] =
            multiout_child_peaks(&snapshot, &bridge_sinks, &child_ids);
        assert_eq!(
            child_a_peak, 0.0,
            "an unsoloed sibling channel must be silent"
        );
        assert!(
            (child_b_peak - 0.4).abs() < 1e-3,
            "the soloed channel must still be audible, got peak {child_b_peak}"
        );
    }

    /// One job, one pass: the mixdown and two stems come out together, and
    /// each file's report is its own — the mixdown pushed over full scale into
    /// a 24-bit file says it clipped and was clamped; the stems do not.
    #[test]
    fn one_render_job_writes_the_mixdown_and_stems_and_reports_overs() {
        let (snapshot, bridge_sinks, child_ids) = multiout_solo_snapshot(false, [false, false]);
        let mix_out = temp_path("job-mix");
        let stem_outs = [temp_path("job-stem-a"), temp_path("job-stem-b")];
        let mut mixdown = wav_request(mix_out.clone(), 1_000);
        mixdown.sample_format = AudioSampleFormat::I24;
        // The stems peak at 0.4 on the master; +6 dBFS drives it to ~2.0.
        mixdown.render.normalize = ExportNormalizeMode::PeakDb(6.0);
        let job = RenderJob {
            mixdown: Some(mixdown),
            tracks: child_ids
                .iter()
                .zip(&stem_outs)
                .map(|(track_id, output)| TrackExportTarget {
                    track_id: track_id.clone(),
                    request: wav_request(output.clone(), 1_000),
                })
                .collect(),
        };

        let summaries = export_render_job_with_bridges(
            &snapshot,
            &job,
            &ExportCancelToken::new(),
            Some(&bridge_sinks),
            |_progress| {},
        )
        .unwrap();

        assert_eq!(summaries.len(), 3, "the mixdown, then the two stems");
        assert_eq!(summaries[0].output_path, mix_out);
        let mix = &summaries[0];
        assert!(mix.levels.clips(), "the mixdown went over full scale");
        assert!(mix.overs_clamped, "a 24-bit file cannot keep them");
        assert!(mix.levels.first_clip_seconds.is_some());
        for stem in &summaries[1..] {
            assert!(!stem.levels.clips(), "stems are not normalized");
            assert!(!stem.overs_clamped);
        }
        for output in std::iter::once(&mix_out).chain(stem_outs.iter()) {
            assert_eq!(&std::fs::read(output).unwrap()[..4], b"RIFF");
            assert!(!partial_path_for(output).exists());
            let _ = std::fs::remove_file(output);
        }
    }

    /// 32-bit float keeps what went over: reported, but not clamped.
    #[test]
    fn a_float_file_keeps_its_overs() {
        let (snapshot, bridge_sinks, _) = multiout_solo_snapshot(false, [false, false]);
        let out = temp_path("float-overs");
        let mut request = wav_request(out.clone(), 1_000);
        request.render.normalize = ExportNormalizeMode::PeakDb(6.0);
        let summary = export_arrangement_with_bridges(
            &snapshot,
            &request,
            &ExportCancelToken::new(),
            Some(&bridge_sinks),
            |_progress| {},
        )
        .unwrap();
        assert!(summary.levels.clips());
        assert!(!summary.overs_clamped);
        let _ = std::fs::remove_file(out);
    }

    /// Plays `blocks` blocks of 256 frames into `capture` from transport
    /// sample `start`, as the audio callback does: master 0.5, the track at
    /// index 7 at 0.25. `jump_at` skips the transport ahead once.
    fn play_into_capture(
        capture: std::sync::Arc<crate::render_capture::RenderCapture>,
        start: u64,
        blocks: usize,
        jump_at: Option<usize>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            capture.set_recording(true);
            let master = vec![0.5f32; 256 * 2];
            let track = vec![0.25f32; 256];
            let mut base = start;
            for block in 0..blocks {
                if jump_at == Some(block) {
                    base += 10_000;
                }
                if capture.begin_block(base, 256) {
                    capture.stage_track(7, &track, &track, 256);
                    capture.stage_master_interleaved(&master, 2, 256);
                    capture.commit_block();
                }
                base += 256;
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        })
    }

    fn realtime_job(mix: PathBuf, stem: PathBuf) -> RenderJob {
        RenderJob {
            mixdown: Some(wav_request(mix, 1_000)),
            tracks: vec![TrackExportTarget {
                track_id: "track".to_string(),
                request: wav_request(stem, 1_000),
            }],
        }
    }

    /// A realtime render records the master and a track from the live
    /// callback, skips the graph latency at the start, and stops at the end
    /// of the range.
    #[test]
    fn a_realtime_render_records_the_range_from_playback() {
        let capture =
            std::sync::Arc::new(crate::render_capture::RenderCapture::new(1, 48_000, 48_000));
        capture.map_tracks(&[Some(7)]);
        let (mix, stem) = (temp_path("rt-mix"), temp_path("rt-stem"));
        let player = play_into_capture(capture.clone(), 0, 40, None);
        let summaries = record_render_job(
            &capture,
            &realtime_job(mix.clone(), stem.clone()),
            300,
            &ExportCancelToken::new(),
            |_progress| {},
        )
        .unwrap();
        player.join().unwrap();

        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().all(|s| s.frames_written == 1_000));
        let db = |s: &ArrangementExportSummary| s.levels.sample_peak_db.unwrap();
        assert!((db(&summaries[0]) - (-6.02)).abs() < 0.05, "master");
        assert!((db(&summaries[1]) - (-12.04)).abs() < 0.05, "track lane");
        for output in [mix, stem] {
            assert!(!partial_path_for(&output).exists());
            let _ = std::fs::remove_file(output);
        }
    }

    /// Playback that jumps mid-render fails the render instead of writing a
    /// file with a hole in it, and leaves nothing behind.
    #[test]
    fn a_realtime_render_whose_playback_moves_fails_cleanly() {
        let capture =
            std::sync::Arc::new(crate::render_capture::RenderCapture::new(1, 48_000, 48_000));
        capture.map_tracks(&[Some(7)]);
        let (mix, stem) = (temp_path("rt-jump-mix"), temp_path("rt-jump-stem"));
        let player = play_into_capture(capture.clone(), 0, 40, Some(2));
        let result = record_render_job(
            &capture,
            &realtime_job(mix.clone(), stem.clone()),
            0,
            &ExportCancelToken::new(),
            |_progress| {},
        );
        player.join().unwrap();
        assert!(matches!(result, Err(ExportError::Build(ref m)) if m.contains("playback moved")));
        for output in [mix, stem] {
            assert!(!output.exists());
            assert!(!partial_path_for(&output).exists());
        }
    }

    #[test]
    fn cancelled_export_removes_partial_and_leaves_existing_output() {
        let out = temp_path("cancel");
        std::fs::write(&out, b"ORIGINAL").unwrap();
        let req = wav_request(out.clone(), 500_000);
        let snapshot = silence_snapshot(48_000);
        let cancel = ExportCancelToken::new();
        cancel.cancel();
        let result = export_arrangement(&snapshot, &req, &cancel, |_p| {});
        assert!(matches!(result, Err(ExportError::Cancelled)));
        // Existing file untouched, no leftover partial.
        assert_eq!(std::fs::read(&out).unwrap(), b"ORIGINAL");
        assert!(!partial_path_for(&out).exists());
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn rejects_missing_output_directory() {
        let mut out = std::env::temp_dir();
        out.push("futureboard-nonexistent-dir-xyz");
        out.push("file.wav");
        let req = wav_request(out, 1000);
        let snapshot = silence_snapshot(48_000);
        let cancel = ExportCancelToken::new();
        let result = export_arrangement(&snapshot, &req, &cancel, |_p| {});
        assert!(matches!(result, Err(ExportError::Settings(_))));
    }

    #[test]
    fn success_replaces_existing_output_file() {
        let out = temp_path("replace");
        std::fs::write(&out, b"OLD-SMALL").unwrap();
        let req = wav_request(out.clone(), 2000);
        let snapshot = silence_snapshot(48_000);
        let cancel = ExportCancelToken::new();
        let summary = export_arrangement(&snapshot, &req, &cancel, |_p| {}).unwrap();
        assert_eq!(summary.frames_written, 2000);
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert!(bytes.len() > 9, "new export should be larger than the stub");
        let _ = std::fs::remove_file(out);
    }
}
