use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::timeline_state::{
    clip_output_local_to_source_sample, AudioImportState, ClipState, TimelineState,
};
use super::waveform_cache::{self, WaveformBars, WaveformDisplayStatus, WaveformPeak};
use crate::theme::Colors;
use gpui::{
    canvas, div, fill, point, px, size, Bounds, IntoElement, ParentElement, Pixels, Styled,
};

pub fn resolve_waveform_source_range(
    clip: &ClipState,
    total_source_frames: u64,
    sample_rate: u32,
    project_bpm: f64,
) -> (u64, u64, f64) {
    let effective_time_ratio = clip.stretch.effective_time_ratio(project_bpm).max(1e-6);
    let total = total_source_frames.max(clip.stretch.original_duration_samples);
    let (source_start, source_end) = clip.stretch.resolved_source_trim_range(total);
    if source_end > source_start {
        return (source_start, source_end, effective_time_ratio);
    }

    let source_rate = sample_rate.max(1) as f64;
    let source_duration_seconds =
        clip.duration_beats.max(0.0) as f64 * (60.0 / project_bpm.max(1.0)) / effective_time_ratio;
    let fallback_end = (clip.stretch.source_start_samples as f64
        + source_duration_seconds * source_rate)
        .round()
        .max(clip.stretch.source_start_samples as f64) as u64;
    (
        clip.stretch.source_start_samples,
        fallback_end.max(clip.stretch.source_start_samples),
        effective_time_ratio,
    )
}

/// Maps a clip-local pixel column to its position *inside the clip's audio*,
/// as a 0..1 fraction of what the clip plays.
///
/// Flat-tempo projects use `Uniform`, which is the column's share of the clip
/// width — the mapping the waveform has always used, at exactly its old cost.
/// With tempo automation the clip's width covers a bent beat axis, so each
/// column is resolved to a beat, the beat to a real elapsed second, and the
/// second to its share of the clip's playing time. That is the mapping the
/// engine reads samples by, which is what keeps the drawn waveform on the audio
/// through a tempo change instead of sliding against it.
enum ClipTimeMap {
    Uniform {
        clip_w: f64,
    },
    Tempo {
        tempo: DirectAudio::TempoMap,
        viewport: super::timeline_state::TimelineViewport,
        clip_left: f64,
        clip_w: f64,
        start_seconds: f64,
        played_seconds: f64,
        revision: u64,
    },
}

impl ClipTimeMap {
    fn for_clip(clip: &ClipState, state: &TimelineState, clip_left: f32, clip_w: f64) -> Self {
        let uniform = Self::Uniform { clip_w };
        if !state.tempo_map.has_automation() {
            return uniform;
        }
        let project_bpm = state.bpm.max(1.0) as f64;
        let Some(played_seconds) = clip.stretch.played_seconds_for_project_bpm(project_bpm) else {
            return uniform;
        };
        if played_seconds <= 0.0 {
            return uniform;
        }
        let tempo = state.tempo_map.to_engine_map(project_bpm);
        let start_seconds = tempo.seconds_at_beat(clip.start_beat.max(0.0) as f64);
        Self::Tempo {
            tempo,
            viewport: state.viewport.clone(),
            clip_left: clip_left as f64,
            clip_w,
            start_seconds,
            played_seconds,
            revision: state.tempo_map.revision(),
        }
    }

    /// Position of clip-local pixel `x` inside the clip's audio, 0..1.
    fn fraction_at(&self, x: f32) -> f64 {
        match self {
            Self::Uniform { clip_w } => ((x as f64) / clip_w).clamp(0.0, 1.0),
            Self::Tempo {
                tempo,
                viewport,
                clip_left,
                clip_w,
                start_seconds,
                played_seconds,
                ..
            } => {
                let lane_x = clip_left + (x as f64).clamp(0.0, *clip_w);
                let beat = super::timeline_state::x_to_beat(lane_x as f32, viewport);
                let seconds = tempo.seconds_at_beat(beat) - start_seconds;
                (seconds / played_seconds).clamp(0.0, 1.0)
            }
        }
    }

    /// Everything the mapping depends on that the geometry cache key does not
    /// already carry, so a tempo edit invalidates the cached bars.
    fn hash_into(&self, hasher: &mut impl Hasher) {
        match self {
            Self::Uniform { .. } => 0u8.hash(hasher),
            Self::Tempo {
                revision,
                start_seconds,
                played_seconds,
                clip_left,
                ..
            } => {
                1u8.hash(hasher);
                revision.hash(hasher);
                start_seconds.to_bits().hash(hasher);
                played_seconds.to_bits().hash(hasher);
                clip_left.to_bits().hash(hasher);
            }
        }
    }
}

/// One bar per visible CSS pixel column. No hard cap on number of bars —
/// the visible-range clamp naturally bounds it by viewport width.
pub fn waveform_canvas(
    clip: &ClipState,
    color: gpui::Rgba,
    state: &TimelineState,
    clip_left: f32,
    clip_width: f32,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("WaveformCanvas");
    // Live recording take: draw streamed preview peaks (Part 1). Checked first
    // so the temporary preview clip never falls through to the file cache or the
    // synthetic placeholder.
    if let Some(preview) = waveform_cache::recording_preview(&clip.id) {
        // Live take: peaks change every poll, so never cache (identity = None).
        return draw_preview_waveform(
            preview.as_ref(),
            color,
            state,
            clip_left,
            clip_width,
            None,
            clip.gain,
        );
    }
    match clip.audio_asset_key() {
        Some(asset_key) => waveform_cache::with_file_entry(asset_key, |entry| {
            let Some(entry) = entry else {
                waveform_cache::record_timeline_render(1, 0, false);
                return import_status_canvas(&clip.audio_import, false, None);
            };
            match waveform_cache::display_status_from_entry(entry) {
                WaveformDisplayStatus::Ready { meta }
                | WaveformDisplayStatus::Partial { meta, .. } => {
                    let pixels_per_second = state.viewport.pixels_per_second;
                    draw_chunk_waveform_locked(
                        asset_key,
                        entry,
                        meta.as_ref(),
                        color,
                        clip,
                        state,
                        clip_left,
                        clip_width,
                        pixels_per_second,
                    )
                }
                WaveformDisplayStatus::Pending => {
                    waveform_cache::record_timeline_render(1, 0, false);
                    import_status_canvas(&clip.audio_import, false, None)
                }
                WaveformDisplayStatus::Error(message) => {
                    waveform_cache::record_timeline_render(1, 0, false);
                    import_status_canvas(&AudioImportState::Failed { message }, true, None)
                }
            }
        }),
        _ => {
            let preview = waveform_cache::get_or_generate_waveform(
                &clip.id,
                &clip.name,
                clip.duration_beats,
                state.bpm,
            );
            // Synthetic placeholder is cached in `clip_cache`, so its `Arc` ptr
            // is a stable identity for the geometry cache.
            let identity = Arc::as_ptr(&preview) as usize as u64;
            draw_preview_waveform(
                preview.as_ref(),
                color,
                state,
                clip_left,
                clip_width,
                Some(identity),
                clip.gain,
            )
        }
    }
}

fn import_status_canvas(
    import: &AudioImportState,
    is_error: bool,
    _progress: Option<f32>,
) -> gpui::Div {
    let (label, show_progress) = match import {
        AudioImportState::Pending => ("Queued".to_string(), false),
        AudioImportState::Probing => ("Probing…".to_string(), true),
        AudioImportState::Decoding { .. } => ("Decoding…".to_string(), true),
        AudioImportState::GeneratingPeaks { progress } => {
            let pct = ((*progress * 100.0) as u32).min(100);
            (format!("Building waveform… {pct}%"), true)
        }
        AudioImportState::Ready => ("Ready".to_string(), false),
        AudioImportState::Failed { message } => (message.clone(), false),
    };

    let stripe = show_progress.then(|| {
        div()
            .absolute()
            .left_0()
            .right_0()
            .top(px(0.0))
            .h(px(2.0))
            .bg(Colors::with_alpha(Colors::accent_primary(), 0.55))
    });

    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .bg(Colors::with_alpha(Colors::surface_base(), 0.35))
        .children(stripe)
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .rounded(px(crate::theme::radius::CONTROL))
                        .border(px(1.0))
                        .border_color(if is_error {
                            Colors::status_error()
                        } else {
                            Colors::border_subtle()
                        })
                        .bg(Colors::with_alpha(Colors::surface_base(), 0.72))
                        .px(px(6.0))
                        .py(px(2.0))
                        .text_size(px(9.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if is_error {
                            Colors::status_error()
                        } else {
                            Colors::text_muted()
                        })
                        .child(label),
                ),
        )
}

/// One min/max bar per visible CSS pixel column. Bars are computed once and
/// drawn from a single GPUI `canvas` element via `paint_quad` — no per-pixel
/// child elements, no `MAX_VISIBLE_WIDTH` cap.
///
/// The per-column aggregation is the expensive part and is cached: a signature
/// over `(asset, peak revision, LOD, visible window, clip size, stretch)` keys
/// an immutable `Arc<WaveformBars>` so repeated repaints with unchanged geometry
/// (playback playhead, meters, hover, selection, track height) reuse the bars
/// instead of re-aggregating on the UI thread. Vertical scale comes from the
/// canvas bounds at paint time so track-height resize does not invalidate peaks.
#[allow(clippy::too_many_arguments)]
fn draw_chunk_waveform_locked(
    asset_key: &str,
    entry: &waveform_cache::FileEntry,
    meta: &waveform_cache::WaveformFileMeta,
    color: gpui::Rgba,
    clip: &ClipState,
    state: &TimelineState,
    clip_left: f32,
    clip_width: f32,
    pixels_per_second: f32,
) -> gpui::Div {
    // Visible portion of the clip in clip-local pixels.
    let viewport_w = state.viewport.viewport_width.max(1.0);
    let visible_start = (-clip_left).max(0.0);
    let visible_end = clip_width
        .min((viewport_w - clip_left).max(visible_start))
        .max(visible_start);
    let visible_w = (visible_end - visible_start).max(0.0);

    if visible_w < 1.0 {
        waveform_cache::record_timeline_render(1, 0, true);
        return empty_canvas();
    }

    let num_cols = (visible_w.ceil() as usize).max(1);
    waveform_cache::record_timeline_render(1, num_cols, true);

    let (source_start, source_end, effective_time_ratio) =
        resolve_waveform_source_range(clip, meta.total_frames, meta.sample_rate, state.bpm as f64);

    // Zoomed past the shipped peak ladder, the drawing runs out of data before
    // it runs out of pixels and the waveform turns into 256-frame blocks. Ask
    // for finer peaks over the source window that is actually on screen; the
    // request is a channel push, and this frame still draws with whatever is
    // already cached.
    let desired_spp = match super::waveform_detail::detail_level_for_zoom(
        pixels_per_second * effective_time_ratio.max(1.0e-6) as f32,
        meta.sample_rate,
    ) {
        Some(detail_spp) => {
            if let super::timeline_state::ClipType::Audio {
                source_path: Some(source_path),
                ..
            } = &clip.clip_type
            {
                super::waveform_detail::note_needed(
                    asset_key,
                    source_path,
                    detail_spp,
                    source_start,
                    source_end,
                    meta.total_frames,
                );
            }
            detail_spp
        }
        None => waveform_cache::pick_best_samples_per_peak(pixels_per_second, meta.sample_rate),
    };
    let spp = waveform_cache::best_available_samples_per_peak_in_entry(entry, desired_spp);
    // The level to draw from when `spp` has nothing for a column. The shipped
    // ladder ships with the preview and is always resident, so this is what
    // keeps a waveform on screen while the finer chunks behind a deep zoom are
    // still being decoded — blocky for a frame or two, rather than gone.
    let fallback_spp =
        waveform_cache::resident_ladder_samples_per_peak(entry).filter(|ladder| *ladder != spp);
    // Commit to the fine level only when the whole visible span is there. Half
    // a span at one resolution and half at another shows as a step, because a
    // min/max envelope is taller the coarser it is.
    let spp = match fallback_spp {
        Some(ladder)
            if !waveform_cache::chunks_cover_peak_range(
                entry,
                spp,
                sample_to_peak_index(source_start as f64, spp),
                sample_to_peak_index(source_end.saturating_sub(1) as f64, spp) + 1,
            ) =>
        {
            ladder
        }
        _ => spp,
    };
    let fallback_spp = fallback_spp.filter(|ladder| *ladder != spp);
    let output_len = SphereAudioProcessor::stretched_duration_samples(
        source_end.saturating_sub(source_start),
        &clip.stretch.to_sphere_stretch_params(state.bpm as f64),
        Some(state.bpm),
    )
    .max(1) as f64;

    let clip_w = clip_width.max(1.0) as f64;

    // Where each drawn column sits *inside the audio*, 0..1.
    //
    // With a flat tempo this is the column's share of the clip width, which is
    // what it always was. With tempo automation the clip's pixels are not
    // uniform seconds — the beat axis they are drawn on bends — so a linear
    // share would slide the picture away from the samples the engine actually
    // plays there. Resolving each column back through the tempo map to a real
    // elapsed second is what holds the waveform still across a tempo change.
    let clip_time_map = ClipTimeMap::for_clip(clip, state, clip_left, clip_w);

    // Geometry signature: horizontal mapping only — peak values are normalized
    // and scaled vertically from the canvas bounds at paint time.
    let key = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        asset_key.hash(&mut hasher);
        waveform_cache::entry_revision(entry).hash(&mut hasher);
        (spp as u64).hash(&mut hasher);
        (num_cols as u64).hash(&mut hasher);
        visible_start.to_bits().hash(&mut hasher);
        clip_width.to_bits().hash(&mut hasher);
        source_start.hash(&mut hasher);
        source_end.hash(&mut hasher);
        effective_time_ratio.to_bits().hash(&mut hasher);
        if matches!(
            clip.stretch.mode,
            super::timeline_state::StretchMode::TempoSync
        ) {
            state.bpm.to_bits().hash(&mut hasher);
        }
        clip.stretch.algorithm.to_tag().hash(&mut hasher);
        (clip.stretch.preserve_pitch as u8).hash(&mut hasher);
        clip.stretch
            .pitch_shift_semitones
            .to_bits()
            .hash(&mut hasher);
        (clip.stretch.reverse as u8).hash(&mut hasher);
        clip_time_map.hash_into(&mut hasher);
        hasher.finish()
    };

    let bars = if let Some(cached) = waveform_cache::geometry_cache_get(key) {
        crate::perf::count("waveform_geo_cache_hit", 1);
        cached
    } else {
        crate::perf::count("waveform_geo_cache_miss", 1);
        crate::perf::count("peak_points_drawn", num_cols as u64);
        let mut bars: WaveformBars = Vec::with_capacity(num_cols);
        for col in 0..num_cols {
            let x0 = visible_start + col as f32;
            let x1 = x0 + 1.0;
            let out0 = clip_time_map.fraction_at(x0) * output_len;
            let out1 = clip_time_map.fraction_at(x1) * output_len;
            let s0 = clip_output_local_to_source_sample(
                out0,
                source_start,
                source_end,
                effective_time_ratio,
                clip.stretch.reverse,
            );
            let s1 = clip_output_local_to_source_sample(
                out1,
                source_start,
                source_end,
                effective_time_ratio,
                clip.stretch.reverse,
            );
            let (lo, hi) = (s0.min(s1), s0.max(s1));
            let p0 = sample_to_peak_index(lo, spp);
            let p1 = sample_to_peak_index(hi, spp).max(p0);
            let peak = waveform_cache::aggregate_peak_range_in_entry_opt(entry, spp, p0, p1 + 1)
                .or_else(|| {
                    // Nothing at this level for this column. Re-index the same
                    // source range against the coarse ladder rather than
                    // dropping the bar: the picture is what the user is looking
                    // at, and a blockier one beats none.
                    let ladder = fallback_spp?;
                    let f0 = sample_to_peak_index(lo, ladder);
                    let f1 = sample_to_peak_index(hi, ladder).max(f0);
                    waveform_cache::aggregate_peak_range_in_entry_opt(entry, ladder, f0, f1 + 1)
                });
            // Genuine silence still draws nothing; only a column with no data
            // at any level is skipped.
            let Some(WaveformPeak { min, max }) = peak else {
                continue;
            };
            if min == 0.0 && max == 0.0 {
                continue;
            }
            let mn = min.max(-1.0);
            let mx = max.min(1.0);
            bars.push((x0, mn, mx));
        }
        let bars = Arc::new(bars);
        waveform_cache::geometry_cache_put(key, Arc::clone(&bars));
        bars
    };

    let waveform_color = Colors::timeline_audio_clip_waveform(color);
    let clip_gain = clip.gain;
    let zoom = state.waveform_zoom;

    let element = canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            paint_waveform_bars(bounds, &bars, waveform_color, clip_gain, zoom, window);
        },
    )
    .absolute()
    .inset_0();

    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .child(element)
}

fn sample_to_peak_index(sample: f64, samples_per_peak: usize) -> usize {
    sample.max(0.0) as usize / samples_per_peak.max(1)
}

/// `identity` is a stable id for the underlying preview (e.g. its `Arc` ptr) or
/// `None` for fast-changing sources (a live recording take) that should never
/// be cached. When `Some`, the bar geometry is reused via the shared cache.
#[allow(clippy::too_many_arguments)]
fn draw_preview_waveform(
    preview: &waveform_cache::WaveformPreview,
    color: gpui::Rgba,
    state: &TimelineState,
    clip_left: f32,
    clip_width: f32,
    identity: Option<u64>,
    clip_gain: f32,
) -> gpui::Div {
    let viewport_w = state.viewport.viewport_width.max(1.0);
    let visible_start = (-clip_left).max(0.0);
    let visible_end = clip_width
        .min((viewport_w - clip_left).max(visible_start))
        .max(visible_start);
    let visible_w = (visible_end - visible_start).max(0.0);
    if visible_w < 1.0 {
        return empty_canvas();
    }
    let samples_per_pixel = (preview.total_frames.max(1) as f32 / clip_width.max(1.0)).max(1.0);
    let Some(lod) = waveform_cache::pick_lod(preview, samples_per_pixel) else {
        return empty_canvas();
    };

    let num_cols = (visible_w.ceil() as usize).max(1);
    let total_peaks = lod.peaks.len().max(1);
    let clip_w = clip_width.max(1.0);

    let key = identity.map(|id| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        (lod.samples_per_peak as u64).hash(&mut hasher);
        (total_peaks as u64).hash(&mut hasher);
        (num_cols as u64).hash(&mut hasher);
        visible_start.to_bits().hash(&mut hasher);
        clip_width.to_bits().hash(&mut hasher);
        hasher.finish()
    });

    let bars = match key.and_then(waveform_cache::geometry_cache_get) {
        Some(cached) => cached,
        None => {
            let mut bars: WaveformBars = Vec::with_capacity(num_cols);
            for col in 0..num_cols {
                let x0 = visible_start + col as f32;
                let x1 = x0 + 1.0;
                let frac0 = (x0 / clip_w).max(0.0);
                let frac1 = (x1 / clip_w).min(1.0);
                let p0 = (frac0 * total_peaks as f32).floor() as usize;
                let p1 = (frac1 * total_peaks as f32).ceil() as usize;
                let end = p1.min(total_peaks).max(p0 + 1);
                let agg = aggregate_slice(&lod.peaks[p0..end]);
                let mn = agg.min.max(-1.0);
                let mx = agg.max.min(1.0);
                bars.push((x0, mn, mx));
            }
            let bars = Arc::new(bars);
            if let Some(key) = key {
                waveform_cache::geometry_cache_put(key, Arc::clone(&bars));
            }
            bars
        }
    };

    let waveform_color = Colors::timeline_audio_clip_waveform(color);
    let zoom = state.waveform_zoom;

    let element = canvas(
        |_b, _w, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            paint_waveform_bars(bounds, &bars, waveform_color, clip_gain, zoom, window);
        },
    )
    .absolute()
    .inset_0();

    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .child(element)
}

/// Paint the cached bars scaled by the clip's current gain and the
/// arrangement's waveform zoom.
///
/// Zoom only magnifies the drawing. The overload color still means the *gain*
/// drives samples past full scale: a zoomed quiet take fills the lane without
/// turning red, because nothing in it clips.
///
/// Gain is applied here, at paint time, rather than baked into `bars`: the bar
/// geometry cache is keyed on the horizontal mapping only, so scrubbing the
/// clip gain redraws at the new amplitude every frame without invalidating or
/// re-aggregating peaks. Samples driven past full scale are drawn in the
/// overload color so the waveform reports real clipping instead of silently
/// flat-topping.
fn paint_waveform_bars(
    bounds: Bounds<Pixels>,
    bars: &WaveformBars,
    color: gpui::Rgba,
    gain: f32,
    zoom: f32,
    window: &mut gpui::Window,
) {
    let h: f32 = bounds.size.height.into();
    if h < 1.0 {
        return;
    }
    let gain = gain.max(0.0);
    let zoom = zoom.max(1.0);
    let center = h / 2.0;
    let overload_color = Colors::meter_high();
    for (x, mn, mx) in bars {
        let scaled_mx = mx * gain;
        let scaled_mn = mn * gain;
        let clipped = scaled_mx > 1.0 || scaled_mn < -1.0;
        let top = center - (scaled_mx * zoom).clamp(-1.0, 1.0) * center;
        let bottom = center - (scaled_mn * zoom).clamp(-1.0, 1.0) * center;
        let bar_h = (bottom - top).max(1.0);
        let r = Bounds::new(
            bounds.origin + point(px(*x), px(top)),
            size(px(1.0), px(bar_h)),
        );
        window.paint_quad(fill(r, if clipped { overload_color } else { color }));
    }
}

fn aggregate_slice(peaks: &[waveform_cache::WaveformPeak]) -> waveform_cache::WaveformPeak {
    if peaks.is_empty() {
        return waveform_cache::WaveformPeak { min: 0.0, max: 0.0 };
    }
    let mut mn = peaks[0].min;
    let mut mx = peaks[0].max;
    for p in &peaks[1..] {
        if p.min < mn {
            mn = p.min;
        }
        if p.max > mx {
            mx = p.max;
        }
    }
    waveform_cache::WaveformPeak { min: mn, max: mx }
}

fn empty_canvas() -> gpui::Div {
    div().relative().size_full().overflow_hidden()
}
