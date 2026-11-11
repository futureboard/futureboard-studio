//! Clip reading: source positions, fades, envelopes, stretch and warp.

use super::*;

/// Map an in-clip output offset `rel` to a source position in **seconds**,
/// honoring the clip's resample `speed_ratio` and reverse flag.
///
/// Map a clip-relative output sample to a source frame.
///
/// Warp segments, when present, replace the global read rate so marker
/// positions stay monotonic and allocation-free on the audio thread.
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn resolved_clip_source_frame(
    rel: u64,
    duration_samples: u64,
    offset_seconds: f64,
    output_sample_rate: u32,
    source_sample_rate: f64,
    speed_ratio: f32,
    reverse: bool,
    warp_segments: &[crate::runtime::RuntimeWarpSegment],
) -> f64 {
    if let Some(frame) =
        crate::runtime::map_warp_source_frame(rel, duration_samples, reverse, warp_segments)
    {
        return frame;
    }
    clip_source_pos_seconds(
        offset_seconds,
        rel,
        duration_samples,
        output_sample_rate,
        speed_ratio,
        reverse,
    ) * source_sample_rate
}

/// Forward playback reads from `offset_seconds` and advances at `speed_ratio`
/// source-seconds per output-second. Reverse reads the same source window from
/// its end backward, so output sample 0 maps to the last source frame and the
/// final output sample maps back to `offset_seconds`. Allocation-free; called
/// from the audio callback.
#[inline]
pub(crate) fn clip_source_pos_seconds(
    offset_seconds: f64,
    rel: u64,
    duration_samples: u64,
    output_sample_rate: u32,
    speed_ratio: f32,
    reverse: bool,
) -> f64 {
    let sr = output_sample_rate.max(1) as f64;
    let advance = if reverse {
        duration_samples.saturating_sub(1).saturating_sub(rel)
    } else {
        rel
    } as f64;
    offset_seconds + (advance / sr) * speed_ratio as f64
}

#[inline]
pub(crate) fn sample_clip_processor_stereo(
    source: &ClipAudioSource,
    source_pos: f64,
    resample_pos: f64,
    effective_time_ratio: f32,
    processor: ClipDspProcessor,
) -> (f32, f32) {
    if !matches!(processor, ClipDspProcessor::PhaseVocoderBasic) {
        return sample_source_stereo(source, resample_pos);
    }
    phase_vocoder_basic_sample(source, source_pos, effective_time_ratio)
}

#[inline]
pub(super) fn phase_vocoder_basic_sample(
    source: &ClipAudioSource,
    source_pos: f64,
    effective_time_ratio: f32,
) -> (f32, f32) {
    let ratio = effective_time_ratio.clamp(0.05, 20.0) as f64;
    if (ratio - 1.0).abs() < 1e-6 {
        return sample_source_stereo(source, source_pos);
    }

    // Basic streaming OLA/granular stretcher. It is allocation-free and reads
    // from the existing clip source; a higher-quality phase vocoder can replace
    // this processor without changing snapshot/runtime routing.
    let grain = 1024.0_f64;
    let hop_out = grain * 0.5;
    let hop_in = hop_out / ratio;
    let phase = (source_pos / hop_in).fract().clamp(0.0, 1.0);
    let window = 0.5 - 0.5 * (std::f64::consts::TAU * phase).cos();
    let (al, ar) = sample_source_stereo(source, source_pos);
    let (bl, br) = sample_source_stereo(source, source_pos + hop_in);
    let w = window as f32;
    (al * (1.0 - w) + bl * w, ar * (1.0 - w) + br * w)
}

/// Clip-fade gain for a sample at offset `rel` from the clip start.
///
/// `1.0` outside both fade regions; ramps `0→1` across the fade-in and `1→0`
/// across the fade-out, each in its own [`FadeCurve`]. Allocation-free and safe
/// for the realtime render path.
///
/// The curves used to be ignored here: the project and the engine snapshot both
/// carried an `in_curve`/`out_curve` tag, and this function rendered equal power
/// whatever they said — so choosing a curve changed the file and nothing else.
#[inline]
pub(crate) fn clip_fade_gain_with_curves(
    rel: u64,
    duration: u64,
    fade_in: u64,
    fade_out: u64,
    in_curve: crate::runtime::FadeCurve,
    out_curve: crate::runtime::FadeCurve,
) -> f32 {
    let mut gain = 1.0f32;
    if fade_in > 0 && rel < fade_in {
        gain *= in_curve.rising_gain(rel as f32 / fade_in as f32);
    }
    if fade_out > 0 {
        let fade_out_start = duration.saturating_sub(fade_out);
        if rel >= fade_out_start {
            gain *= out_curve
                .falling_gain((rel - fade_out_start) as f32 / fade_out as f32)
                .max(0.0);
        }
    }
    gain
}

/// [`clip_fade_gain_with_curves`] at the default curve on both edges.
#[inline]
pub(crate) fn clip_fade_gain(rel: u64, duration: u64, fade_in: u64, fade_out: u64) -> f32 {
    clip_fade_gain_with_curves(
        rel,
        duration,
        fade_in,
        fade_out,
        crate::runtime::FadeCurve::EqualPower,
        crate::runtime::FadeCurve::EqualPower,
    )
}

/// Source-stream span consumed to render the output segment
/// `[rel_start, rel_start + frames)` at `time_ratio`. Successive segments tile
/// the source contiguously — block N's `in_start + input_frames` equals block
/// N+1's `in_start` — so the source is read exactly once with no gap or overlap,
/// and the total consumed over the clip is `floor(duration / time_ratio)`
/// (= the source length), never more. This is what keeps the streaming stretcher
/// from over-reading the source or growing an internal backlog.
pub(crate) fn signalsmith_input_span(
    rel_start: u64,
    frames: usize,
    time_ratio: f64,
) -> (i64, usize) {
    let ratio = time_ratio.clamp(0.05, 20.0);
    let in_start = (rel_start as f64 / ratio).floor() as i64;
    let in_end = ((rel_start + frames as u64) as f64 / ratio).floor() as i64;
    (in_start, (in_end - in_start).max(1) as usize)
}

/// Source frame for output frame `rel` of a warped clip, like
/// [`crate::runtime::map_warp_source_frame`] but continuing past the last
/// segment at its slope instead of stopping at its end.
///
/// The stretcher's feed runs a latency ahead of its output, so near the end of
/// a clip it asks for positions past the last marker. Held at the end, the
/// feed would stall there while output kept coming, and the stretcher would
/// read the stall as a sudden, extreme slow-down of the clip's last beat.
pub(super) fn warp_stream_position(
    rel: u64,
    duration_samples: u64,
    segments: &[crate::runtime::RuntimeWarpSegment],
) -> f64 {
    match segments.last() {
        Some(last) if rel > last.out_end => {
            let span = last.out_end.saturating_sub(last.out_start).max(1) as f64;
            let slope = (last.src_end - last.src_start) / span;
            last.src_end + slope * (rel - last.out_end) as f64
        }
        _ => crate::runtime::map_warp_source_frame(rel, duration_samples, false, segments)
            .unwrap_or(0.0),
    }
}

pub(super) fn render_signalsmith_clip_segment(
    runtime: &mut RuntimeProject,
    clip_index: usize,
    track_index: usize,
    project_start_sample: u64,
    rel_start: u64,
    frame_idx_start: usize,
    frames: usize,
) -> bool {
    let (
        source,
        offset_seconds,
        duration_samples,
        output_sample_rate,
        reverse,
        fade_in_samples,
        fade_out_samples,
        fade_in_curve,
        fade_out_curve,
        time_ratio,
    ) = {
        let clip = &runtime.clips[clip_index];
        (
            Arc::clone(&clip.source),
            clip.offset_seconds,
            clip.duration_samples,
            runtime.sample_rate,
            clip.reverse,
            clip.fade_in_samples,
            clip.fade_out_samples,
            clip.fade_in_curve,
            clip.fade_out_curve,
            clip.effective_time_ratio.clamp(0.05, 20.0) as f64,
        )
    };

    // Map this output segment [rel_start, rel_start + frames) onto a *contiguous*
    // span of the source stream so successive blocks tile the source with no gap
    // or overlap, and the source is never over-read. The stretcher consumes
    // exactly these `input_frames` samples to produce `frames` output (time ratio
    // = frames / input_frames), so it never has to buffer/grow across calls.
    let output_sr = output_sample_rate.max(1) as f64;
    let source_sr = source.sample_rate() as f64;
    // A warped clip's stream is the source itself, resampled to the output
    // rate: stream index `k` is source frame `k * source_sr / output_sr`, and
    // the output segment consumes exactly the stream span its warp map covers.
    // The map is monotonic, so `floor` of it tiles the stream contiguously the
    // same way `signalsmith_input_span` does for a constant ratio. The local
    // time ratio (`frames / input_frames`) follows each warp segment's slope,
    // so pitch stays put while the speed changes between markers.
    //
    // The stretcher outputs a moment only after it has read `input_latency`
    // past it, and then takes `output_latency` more to deliver it. So while
    // producing output frame `o` it is fed the stream up to
    // `S(o + output_latency) + input_latency`, where `S` maps output frames to
    // stream positions — and `output_seek` at a (re)start primes it with the
    // stream from `S(o)` up to exactly there. Feeding from `S(o)` itself, with
    // the pre-roll taken from *before* the play position, is what put every
    // beat of a stretched clip a whole stretcher latency behind the metronome.
    let (input_latency, output_latency) = runtime.clips[clip_index]
        .stretch_processor
        .as_ref()
        .map(|processor| processor.io_latency())
        .unwrap_or((0, 0));
    let warped = !runtime.clips[clip_index].warp_segments.is_empty();
    let stream_at = |rel: u64| -> f64 {
        if warped {
            warp_stream_position(
                rel,
                duration_samples,
                &runtime.clips[clip_index].warp_segments,
            ) * output_sr
                / source_sr.max(1.0)
        } else {
            rel as f64 / time_ratio
        }
    };
    let feed_at = |rel: u64| -> i64 {
        (stream_at(rel + output_latency as u64) + input_latency as f64).floor() as i64
    };
    let (in_start, input_frames) = if warped {
        let in_start = feed_at(rel_start);
        (
            in_start,
            (feed_at(rel_start + frames as u64) - in_start).max(1) as usize,
        )
    } else {
        // The same feed for a constant ratio, tiled by the shared span helper.
        let (start, input_frames) =
            signalsmith_input_span(rel_start + output_latency as u64, frames, time_ratio);
        (start + input_latency as i64, input_frames)
    };
    let stream_at_start = stream_at(rel_start).floor() as i64;
    let total_input = (duration_samples as f64 / time_ratio).floor() as i64;

    // Source-stream index → source sample position (reverse-aware). Reading the
    // source at the output sample rate lets the seconds map handle the
    // source↔output rate conversion, matching the per-sample resample path.
    // Shared by the pre-roll priming and the per-block feed so both read one
    // contiguous stream.
    let source_pos_at = |stream_index: i64| -> f64 {
        if warped {
            return stream_index as f64 * source_sr / output_sr;
        }
        let effective = if reverse {
            (total_input - 1 - stream_index).max(0)
        } else {
            stream_index
        };
        (offset_seconds + effective as f64 / output_sr) * source_sr
    };

    let clip = &mut runtime.clips[clip_index];
    let Some(processor) = clip.stretch_processor.as_mut() else {
        return false;
    };

    // All scratch storage is sized while the runtime graph is built. A device
    // callback must never grow a Vec; if a future host presents a block larger
    // than the prepared contract, fail closed and use the allocation-free
    // fallback path for this segment.
    if input_frames > clip.stretch_input_l.len()
        || input_frames > clip.stretch_input_r.len()
        || frames > clip.stretch_output_l.len()
        || frames > clip.stretch_output_r.len()
    {
        clip.stretch_next_project_sample = None;
        return false;
    }

    // On a (re)start/discontinuity, latency-align the stretcher to this playback
    // position: prime it with the stream from the play position up to where
    // this block's feed begins, so the *next* `process` output starts exactly
    // at the play position and a high-latency preserve-pitch backend
    // (Signalsmith ≈120 ms) plays on the grid with the rest of the mix.
    // Zero-latency backends have nothing to prime and just reset.
    if clip.stretch_next_project_sample != Some(project_start_sample) {
        let prime_start = stream_at_start;
        let seek_len = (in_start - prime_start).max(0) as usize;
        if input_latency + output_latency > 0 && seek_len > 0 {
            if seek_len > clip.stretch_prime_l.len() || seek_len > clip.stretch_prime_r.len() {
                clip.stretch_next_project_sample = None;
                return false;
            }
            for j in 0..seek_len {
                let stream_index = prime_start + j as i64;
                let (l, r) = sample_source_stereo(&source, source_pos_at(stream_index));
                clip.stretch_prime_l[j] = l;
                clip.stretch_prime_r[j] = r;
            }
            processor.output_seek(
                &clip.stretch_prime_l[..seek_len],
                &clip.stretch_prime_r[..seek_len],
            );
        } else {
            processor.reset();
        }
    }

    for k in 0..input_frames {
        let (l, r) = sample_source_stereo(&source, source_pos_at(in_start + k as i64));
        clip.stretch_input_l[k] = l;
        clip.stretch_input_r[k] = r;
    }

    if processor
        .process_stereo(
            &clip.stretch_input_l[..input_frames],
            &clip.stretch_input_r[..input_frames],
            &mut clip.stretch_output_l[..frames],
            &mut clip.stretch_output_r[..frames],
        )
        .is_err()
    {
        clip.stretch_next_project_sample = None;
        return false;
    }
    clip.stretch_next_project_sample = Some(project_start_sample + frames as u64);

    let track = &mut runtime.tracks[track_index];
    for i in 0..frames {
        let rel = rel_start + i as u64;
        let fade = clip_fade_gain_with_curves(
            rel,
            duration_samples,
            fade_in_samples,
            fade_out_samples,
            fade_in_curve,
            fade_out_curve,
        );
        let (l, r) = process_clip_frame(
            clip,
            rel,
            fade,
            clip.stretch_output_l[i],
            clip.stretch_output_r[i],
        );
        let frame_idx = frame_idx_start + i;
        track.block_l[frame_idx] += l;
        track.block_r[frame_idx] += r;
    }

    true
}

/// Everything a clip does to its signal after the source has been read (and
/// stretched): de-noise, channel transform, DC removal and de-hum, then the
/// fade × clip gain × gain envelope × Audio Tools preview gain, and the
/// analysis tap. Every clip render path — the per-sample path, the resample
/// block path and the Signalsmith path, i.e. live stereo playback and export —
/// goes through this one function, so a clip sounds the same everywhere.
/// (Stereo playback and export used to apply only gain × fade, so the gain
/// envelope, de-noise, channel, DC and de-hum settings were silent there.)
///
/// Realtime: per-sample, allocation-free, no locks; the processors are
/// fixed-size state prepared when the runtime graph is built.
#[inline]
pub(crate) fn process_clip_frame(
    clip: &mut crate::runtime::RuntimeClip,
    rel: u64,
    fade: f32,
    mut l: f32,
    mut r: f32,
) -> (f32, f32) {
    if rel == 0 {
        clip.denoise.reset();
    }
    (l, r) = clip.denoise.process_stereo(l, r);
    let extra = if clip.preview_bypass {
        1.0
    } else {
        (l, r) = SphereAudioProcessor::apply_channel_transform(l, r, clip.channel_transform);
        if clip.dc_remove {
            l -= clip.dc_left;
            r -= clip.dc_right;
        }
        (l, r) = clip.dehum.process_stereo(l, r);
        clip.extra_gain
            * envelope_linear_gain(
                &clip.envelope_points,
                rel as f32 / clip.duration_samples.max(1) as f32,
            )
    };
    // A live gain change (`SetClipGain`) ramps over a few milliseconds
    // instead of stepping, which would click on every drag move.
    if clip.gain != clip.gain_target {
        clip.gain += (clip.gain_target - clip.gain) * clip.gain_step;
        if (clip.gain_target - clip.gain).abs() < 1.0e-5 {
            clip.gain = clip.gain_target;
        }
    }
    let g = clip.gain * fade * extra;
    l *= g;
    r *= g;
    crate::analysis_tap::analysis_tap().write_if_target(clip.id_hash, l, r);
    (l, r)
}

#[inline]
pub(super) fn envelope_linear_gain(points: &[(f32, f32)], time: f32) -> f32 {
    if points.is_empty() {
        return 1.0;
    }
    let t = time.clamp(0.0, 1.0);
    if t <= points[0].0 {
        return SphereAudioProcessor::db_to_lin(points[0].1);
    }
    for pair in points.windows(2) {
        if t <= pair[1].0 {
            let span = (pair[1].0 - pair[0].0).max(1.0e-6);
            let x = ((t - pair[0].0) / span).clamp(0.0, 1.0);
            let db = pair[0].1 + (pair[1].1 - pair[0].1) * x;
            return SphereAudioProcessor::db_to_lin(db);
        }
    }
    SphereAudioProcessor::db_to_lin(points.last().map(|p| p.1).unwrap_or(0.0))
}
