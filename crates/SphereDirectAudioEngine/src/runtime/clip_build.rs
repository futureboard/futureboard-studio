//! Building clip runtimes and assigning streaming lanes.

use super::*;

/// Cache key of the `lane`th extra stream of `path`; lane 0 is the shared
/// `path` entry itself. NUL never appears in a path, so these keys cannot
/// collide with one and are easy to tell apart when pruning.
pub(super) fn stream_lane_key(path: &str, lane: usize) -> String {
    format!("{path}\u{0}stream-lane:{lane}")
}

/// Tell every disk stream where the clips reading it sit on the timeline, so
/// a transport seek can cue its target before the audio thread reads there
/// (#380, [`crate::streaming_source::StreamingSource::prefetch_timeline`]).
///
/// Control thread only: allocates and takes each stream's span lock.
pub(super) fn publish_stream_clip_spans(clips: &[RuntimeClip], output_sample_rate: u32) {
    use crate::streaming_source::{StreamClipSpan, StreamingSource};
    let mut by_stream: Vec<(&Arc<StreamingSource>, Vec<StreamClipSpan>)> = Vec::new();
    for clip in clips {
        let ClipAudioSource::Streaming(stream) = clip.source.as_ref() else {
            continue;
        };
        let span = StreamClipSpan::from_clip(clip, output_sample_rate);
        match by_stream
            .iter_mut()
            .find(|(known, _)| Arc::ptr_eq(known, stream))
        {
            Some((_, spans)) => spans.push(span),
            None => by_stream.push((stream, vec![span])),
        }
    }
    for (stream, spans) in by_stream {
        stream.set_clip_spans(spans);
    }
}

/// Give clips of one disk-streamed file that overlap in time their own stream.
///
/// A [`crate::streaming_source::StreamingSource`] follows a single read cursor.
/// Every clip of a file used to get the one cached source, so two clips playing
/// different parts of the file at once (a double-tracked section, a crossfade
/// within one take) pulled that cursor back and forth and both dropped out.
///
/// Clips are packed into lanes by timeline position, greedily: a clip joins the
/// first lane whose last clip has ended, so the number of streams is the most
/// clips of that file that ever sound together — a comp of many back-to-back
/// slices still shares one. Lane 0 keeps the `path` entry; lane `n` is cached
/// under [`stream_lane_key`] so rebuilds (edits, undo) reuse the open stream,
/// and lanes no clip uses any more are dropped from the cache here.
///
/// Control thread only: opens files and allocates. Returns how many streams it
/// opened.
pub(super) fn assign_stream_lanes(
    clips: &mut [RuntimeClip],
    clip_paths: &[&str],
    cache: &mut HashMap<String, Arc<ClipAudioSource>>,
    open: &mut dyn FnMut(&str) -> Result<ClipAudioSource, String>,
) -> u32 {
    let mut by_path: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, clip) in clips.iter().enumerate() {
        if clip.source.is_streaming() {
            if let Some(&path) = clip_paths.get(index) {
                by_path.entry(path).or_default().push(index);
            }
        }
    }

    let mut used_lanes = std::collections::HashSet::new();
    let mut opened = 0u32;
    for (path, mut indices) in by_path {
        if indices.len() < 2 {
            continue;
        }
        indices.sort_by_key(|&index| (clips[index].start_sample, index));
        let mut lane_ends: Vec<u64> = Vec::new();
        for index in indices {
            let start = clips[index].start_sample;
            let end = start.saturating_add(clips[index].duration_samples);
            let lane = match lane_ends.iter().position(|&lane_end| lane_end <= start) {
                Some(lane) => lane,
                None => {
                    lane_ends.push(0);
                    lane_ends.len() - 1
                }
            };
            lane_ends[lane] = end;
            if lane == 0 {
                continue;
            }
            let key = stream_lane_key(path, lane);
            let source = match cache.get(&key) {
                Some(existing) => Arc::clone(existing),
                None => match open(path) {
                    Ok(source) => {
                        opened += 1;
                        let source = Arc::new(source);
                        cache.insert(key.clone(), Arc::clone(&source));
                        source
                    }
                    Err(e) => {
                        // Keep the shared stream: worse playback, not none.
                        eprintln!("[SphereAudio] extra stream for '{path}' failed: {e}");
                        continue;
                    }
                },
            };
            used_lanes.insert(key);
            clips[index].source = source;
        }
    }
    cache.retain(|key, _| !key.contains('\u{0}') || used_lanes.contains(key));
    opened
}

pub(super) fn build_clip_runtime(
    clip: &EngineClipSnapshot,
    source: Arc<ClipAudioSource>,
    beats_per_second: f64,
    output_sample_rate: u32,
    tempo_map: Option<&RuntimeTempoMapSnapshot>,
) -> Option<RuntimeClip> {
    if beats_per_second <= 0.0 || output_sample_rate == 0 {
        return None;
    }

    let start_seconds = clip.start_beat / beats_per_second;
    let duration_seconds = clip.duration_beats / beats_per_second;
    if duration_seconds <= 0.0 {
        return None;
    }

    let project_bpm = Some((beats_per_second * 60.0) as f32);
    let stretch = resolved_clip_stretch_params(clip);
    let speed_ratio = source_read_rate_for_repitch(&stretch, project_bpm).clamp(0.01, 16.0);
    let source_read_rate = speed_ratio;
    let effective_time_ratio = effective_time_ratio(&stretch, project_bpm).clamp(0.01, 20.0);
    let pitch_ratio = effective_pitch_ratio(&stretch).clamp(0.01, 16.0);
    let mut stretch_backend = resolve_backend(&stretch);
    if stretch_backend == StretchBackend::Signalsmith
        && !SphereAudioProcessor::signalsmith_stretch_available()
    {
        static WARNED_SIGNALS_MISSING: AtomicBool = AtomicBool::new(false);
        if !WARNED_SIGNALS_MISSING.swap(true, Ordering::Relaxed) {
            eprintln!(
                "Signalsmith Stretch unavailable; falling back to InternalRePitch for clip {}",
                clip.id
            );
        }
        stretch_backend = StretchBackend::InternalRePitch;
    }
    let processor = match stretch_backend {
        StretchBackend::InternalRePitch => {
            if stretch.mode == StretchMode::Off || stretch.algorithm == StretchAlgorithm::Off {
                ClipDspProcessor::NoStretch
            } else {
                ClipDspProcessor::Resample
            }
        }
        StretchBackend::Signalsmith => ClipDspProcessor::PhaseVocoderBasic,
    };
    let reverse = clip
        .audio_process
        .as_ref()
        .map(|p| p.reverse)
        .unwrap_or(false);
    let denoise_amount = clip
        .audio_process
        .as_ref()
        .map(|p| p.denoise_amount)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let source_start_samples = clip
        .audio_process
        .as_ref()
        .map(|p| p.source_start_samples)
        .unwrap_or(0);
    let source_end_samples = clip
        .audio_process
        .as_ref()
        .map(|p| p.source_end_samples)
        .unwrap_or(0);
    let mut warp_markers: Vec<RuntimeWarpMarker> = clip
        .audio_process
        .as_ref()
        .map(|p| {
            p.warp_markers
                .iter()
                .map(|m| RuntimeWarpMarker {
                    id: m.id,
                    source_sample: m.source_sample,
                    timeline_beat: m.timeline_beat,
                    locked: m.locked,
                })
                .collect()
        })
        .unwrap_or_default();
    warp_markers.sort_by(|a, b| a.timeline_beat.total_cmp(&b.timeline_beat));

    // One-time, control-thread-only diagnostic of the resolved clip DSP path
    // (never logs from the audio callback). Gated behind a debug flag.
    if clip_dsp_debug_enabled() {
        if let Some(p) = clip.audio_process.as_ref() {
            eprintln!(
                "[clip-dsp] {}",
                describe_clip_dsp_state(clip, p, beats_per_second * 60.0)
            );
        }
    }

    let base_duration_samples = seconds_to_samples(duration_seconds, output_sample_rate).max(1);
    let stretch_is_authoritative =
        clip.audio_process.is_some() || clip.stretch != StretchParams::default();
    let duration_samples = if stretch_is_authoritative {
        let trim_start = source_start_samples.min(source.frames() as u64);
        let trim_end = if source_end_samples > trim_start {
            source_end_samples.min(source.frames() as u64)
        } else {
            source.frames() as u64
        };
        let trimmed_source_frames = trim_end.saturating_sub(trim_start).max(1);
        let source_frames_at_output_rate = ((trimmed_source_frames as f64
            / source.sample_rate().max(1) as f64)
            * output_sample_rate.max(1) as f64)
            .round()
            .max(1.0) as u64;
        stretched_duration_samples(source_frames_at_output_rate, &stretch, project_bpm).max(1)
    } else {
        base_duration_samples
    };

    let start_sample = match tempo_map {
        Some(map) => map.samples_at_beat(clip.start_beat.max(0.0), output_sample_rate as f64),
        None => seconds_to_samples(start_seconds.max(0.0), output_sample_rate),
    };
    let warp_source_start = source_start_samples.min(source.frames() as u64);
    let warp_source_end = if source_end_samples > source_start_samples {
        source_end_samples.min(source.frames() as u64)
    } else {
        source.frames() as u64
    };
    // A clip locked to the tempo (Tempo Sync, Warp) is defined in beats: under
    // a changing tempo it has to speed up and slow down with it, which one
    // global ratio cannot do. It plays through segments built from the map.
    let tempo_locked = tempo_map
        .filter(|_| matches!(stretch.mode, StretchMode::TempoSync | StretchMode::Warp))
        .and_then(|map| {
            let anchors = tempo_locked_anchors(
                &stretch,
                &warp_markers,
                clip.start_beat.max(0.0),
                clip.duration_beats.max(0.0),
                warp_source_start,
                warp_source_end,
                source.sample_rate(),
            )?;
            tempo_mapped_warp_segments(
                map,
                clip.start_beat.max(0.0),
                clip.duration_beats.max(0.0),
                start_sample,
                output_sample_rate,
                &anchors,
            )
        });
    let duration_samples = tempo_locked
        .as_ref()
        .map_or(duration_samples, |(_, duration)| *duration);

    // Resolve fade durations (seconds) → output samples. Clamp so the two
    // fades never overlap or exceed the clip length.
    let (fade_in_samples, fade_out_samples) = clip
        .fades
        .as_ref()
        .map(|f| {
            let fi = seconds_to_samples(f.in_duration.max(0.0), output_sample_rate);
            let fo = seconds_to_samples(f.out_duration.max(0.0), output_sample_rate);
            (fi, fo)
        })
        .unwrap_or((0, 0));
    let fade_in_samples = fade_in_samples.min(duration_samples);
    let fade_out_samples = fade_out_samples.min(duration_samples.saturating_sub(fade_in_samples));

    let stretch_processor =
        create_runtime_stretch_processor(stretch_backend, source.sample_rate(), &stretch);

    let warp_segments = match tempo_locked {
        Some((segments, _)) => segments,
        None => build_warp_segments(
            &warp_markers,
            clip.start_beat.max(0.0),
            clip.duration_beats.max(0.0),
            duration_samples,
            warp_source_start,
            warp_source_end,
        ),
    };
    // The stretcher's scratch is sized for the fastest rate it will be fed.
    // For a warped clip that is its steepest segment, not the global ratio.
    let capacity_time_ratio = warp_min_time_ratio(
        &warp_segments,
        source.sample_rate() as f64,
        output_sample_rate as f64,
    )
    .map_or(effective_time_ratio, |warp| warp.min(effective_time_ratio));

    // Preallocate the latency-priming pre-roll buffer on the control thread so
    // the audio thread never grows it on first use. Sized for this clip's
    // fastest playback rate (`1 / time_ratio` input-per-output); `0` for
    // zero-latency backends / no processor.
    let stretch_prime_len = stretch_processor
        .as_ref()
        // +2: the render floors both ends of the pre-roll span separately.
        .map(|p| p.seek_input_len(1.0 / capacity_time_ratio.max(0.01)) + 2)
        .unwrap_or(0);

    Some(RuntimeClip {
        id: clip.id.clone(),
        track_id: clip.track_id.clone(),
        track_index: None, // resolved by RuntimeProject::resolve_indices
        start_beat: clip.start_beat.max(0.0),
        duration_beats: clip.duration_beats.max(0.0),
        start_sample,
        duration_samples,
        offset_seconds: clip.offset_seconds.max(0.0),
        gain: clip.gain.clamp(0.0, 4.0),
        gain_target: clip.gain.clamp(0.0, 4.0),
        gain_step: clip_gain_step(output_sample_rate),
        ara_rendered: clip.ara_rendered,
        stretch,
        speed_ratio,
        source_read_rate,
        effective_time_ratio,
        pitch_ratio,
        stretch_backend,
        source_start_samples,
        source_end_samples,
        warp_markers,
        warp_segments,
        processor,
        reverse,
        denoise: DenoiseProcessor::new(output_sample_rate, denoise_amount),
        id_hash: crate::analysis_tap::clip_id_hash(&clip.id),
        channel_transform: SphereAudioProcessor::ChannelTransform::from_tag(
            clip.audio_process
                .as_ref()
                .map(|p| p.channel_transform)
                .unwrap_or(0),
        ),
        dc_remove: clip
            .audio_process
            .as_ref()
            .map(|p| p.dc_remove)
            .unwrap_or(false),
        dc_left: clip
            .audio_process
            .as_ref()
            .map(|p| p.dc_left)
            .unwrap_or(0.0),
        dc_right: clip
            .audio_process
            .as_ref()
            .map(|p| p.dc_right)
            .unwrap_or(0.0),
        extra_gain: clip
            .audio_process
            .as_ref()
            .map(|p| {
                if p.extra_gain.is_finite() && p.extra_gain > 0.0 {
                    p.extra_gain
                } else {
                    1.0
                }
            })
            .unwrap_or(1.0),
        dehum: {
            let process = clip.audio_process.as_ref();
            SphereAudioProcessor::DehumProcessor::new(
                output_sample_rate,
                SphereAudioProcessor::DehumParams {
                    base_hz: process.map(|p| p.dehum_hz).unwrap_or(0.0),
                    harmonics: process.map(|p| p.dehum_harmonics).unwrap_or(0),
                    reduction_db: process.map(|p| p.dehum_reduction_db).unwrap_or(0.0),
                },
            )
        },
        envelope_points: clip
            .audio_process
            .as_ref()
            .map(|p| p.envelope_points.clone())
            .unwrap_or_default(),
        preview_bypass: clip
            .audio_process
            .as_ref()
            .map(|p| p.preview_bypass)
            .unwrap_or(false),
        muted: clip.muted,
        fade_in_samples,
        fade_out_samples,
        fade_in_curve: clip
            .fades
            .as_ref()
            .map(|f| FadeCurve::from_tag(&f.in_curve))
            .unwrap_or_default(),
        fade_out_curve: clip
            .fades
            .as_ref()
            .map(|f| FadeCurve::from_tag(&f.out_curve))
            .unwrap_or_default(),
        source,
        stretch_processor,
        stretch_input_l: vec![0.0; stretch_input_capacity(capacity_time_ratio)],
        stretch_input_r: vec![0.0; stretch_input_capacity(capacity_time_ratio)],
        stretch_output_l: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
        stretch_output_r: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
        stretch_prime_l: vec![0.0; stretch_prime_len],
        stretch_prime_r: vec![0.0; stretch_prime_len],
        stretch_next_project_sample: None,
    })
}
