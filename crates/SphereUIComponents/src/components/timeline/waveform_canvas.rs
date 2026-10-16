use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use super::timeline_state::{
    clip_output_local_to_source_sample, AudioImportState, ClipState, TimelineState,
    TimelineViewport,
};
use super::waveform_cache::{self, PeakSpan, WaveformBars, WaveformDisplayStatus, WaveformPeak};
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
        tempo: Arc<DirectAudio::TempoMap>,
        /// The arrangement viewport with its scroll zeroed — see
        /// [`TimeMapSpec::for_clip`].
        viewport: TimelineViewport,
        origin: f64,
        clip_w: f64,
        start_seconds: f64,
        played_seconds: f64,
    },
}

impl ClipTimeMap {
    /// Position of clip-local pixel `x` inside the clip's audio, 0..1.
    fn fraction_at(&self, x: f32) -> f64 {
        match self {
            Self::Uniform { clip_w } => ((x as f64) / clip_w).clamp(0.0, 1.0),
            Self::Tempo {
                tempo,
                viewport,
                origin,
                clip_w,
                start_seconds,
                played_seconds,
            } => {
                let content_x = origin + (x as f64).clamp(0.0, *clip_w);
                let beat = super::timeline_state::x_to_beat(content_x as f32, viewport);
                let seconds = tempo.seconds_at_beat(beat) - start_seconds;
                (seconds / played_seconds).clamp(0.0, 1.0)
            }
        }
    }
}

/// What a [`ClipTimeMap`] is built from, resolved without the tempo map so it
/// is cheap enough to hash on every frame. The map itself is only fetched when
/// bars actually have to be built.
enum TimeMapSpec {
    Uniform,
    Tempo {
        viewport: TimelineViewport,
        origin: f32,
        played_seconds: f64,
    },
}

impl TimeMapSpec {
    /// Columns resolve in *content* space — the viewport's scroll zeroed, the
    /// clip's left edge at its unscrolled lane x — so neither the mapping nor
    /// the bars cached from it move with the scroll.
    ///
    /// The lane draws a clip at `round(content − scroll)`. At a whole-pixel
    /// scroll that is `round(content) − scroll`, so every column lands on the
    /// same beat the scrolled lane-space mapping gave it. At a fractional
    /// scroll the lane-space mapping crept by up to half a pixel from frame to
    /// frame; this one holds the clip's first column on its first sample, as
    /// the flat-tempo mapping always has.
    fn for_clip(clip: &ClipState, state: &TimelineState) -> Self {
        if !state.tempo_map.has_automation() {
            return Self::Uniform;
        }
        let project_bpm = state.bpm.max(1.0) as f64;
        let Some(played_seconds) = clip.stretch.played_seconds_for_project_bpm(project_bpm) else {
            return Self::Uniform;
        };
        if played_seconds <= 0.0 {
            return Self::Uniform;
        }
        let mut viewport = state.viewport.clone();
        viewport.scroll_x = 0.0;
        let origin = super::timeline_state::beat_to_x(clip.start_beat as f64, &viewport);
        Self::Tempo {
            viewport,
            origin,
            played_seconds,
        }
    }

    /// Everything the mapping depends on that the geometry cache key does not
    /// already carry, so a tempo or zoom edit invalidates the cached bars.
    fn hash_into(&self, clip: &ClipState, state: &TimelineState, hasher: &mut impl Hasher) {
        match self {
            Self::Uniform => 0u8.hash(hasher),
            Self::Tempo {
                viewport,
                origin,
                played_seconds,
            } => {
                1u8.hash(hasher);
                state.tempo_cache_key().hash(hasher);
                viewport.time_warp.key().hash(hasher);
                viewport.pixels_per_second.to_bits().hash(hasher);
                viewport.pixels_per_beat.to_bits().hash(hasher);
                clip.start_beat.to_bits().hash(hasher);
                played_seconds.to_bits().hash(hasher);
                origin.to_bits().hash(hasher);
            }
        }
    }

    fn build(self, clip: &ClipState, state: &TimelineState, clip_w: f64) -> ClipTimeMap {
        match self {
            Self::Uniform => ClipTimeMap::Uniform { clip_w },
            Self::Tempo {
                viewport,
                origin,
                played_seconds,
            } => {
                // The project's resolved map: one `Arc` clone while the tempo
                // is unchanged, instead of a rebuild per clip per frame.
                let tempo = state.resolved_tempo_map();
                let start_seconds = tempo.seconds_at_beat(clip.start_beat.max(0.0) as f64);
                ClipTimeMap::Tempo {
                    tempo,
                    viewport,
                    origin: origin as f64,
                    clip_w,
                    start_seconds,
                    played_seconds,
                }
            }
        }
    }
}

/// Columns per cached bar tile.
///
/// A cache keyed on the exact visible window misses on every frame of a
/// scroll for any clip crossing the viewport edge — the one gesture whose cost
/// the user feels. Bars are instead built and cached per fixed run of
/// clip-local columns, so a scroll reuses every tile it does not newly reveal
/// and builds only the one it does. Same granularity as the MIDI note preview
/// (`clip_geometry::PREVIEW_TILE_PX`).
const WAVEFORM_TILE_COLS: usize = 256;

/// The visible pixel columns of a clip, on the clip-local grid its bars are
/// built on.
///
/// Column `col` sits at clip-local `x = phase + col`. The arrangement lays
/// clips out on whole pixels, so `phase` is 0 and the grid is the clip's own
/// pixel grid; a fractional visible start keeps its fraction there, so every
/// column lands exactly where the old per-window build put it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ColumnWindow {
    phase: f32,
    /// First visible column and one past the last.
    first: usize,
    end: usize,
    /// One past the clip's last column. Bounds the last tile by the clip,
    /// never by the viewport.
    clip_cols: usize,
}

impl ColumnWindow {
    /// `None` when less than one column of the clip is on screen.
    fn visible(clip_left: f32, clip_width: f32, viewport_width: f32) -> Option<Self> {
        let viewport_w = viewport_width.max(1.0);
        let visible_start = (-clip_left).max(0.0);
        let visible_end = clip_width
            .min((viewport_w - clip_left).max(visible_start))
            .max(visible_start);
        let visible_w = (visible_end - visible_start).max(0.0);
        if visible_w < 1.0 {
            return None;
        }
        let num_cols = (visible_w.ceil() as usize).max(1);
        let base = visible_start.floor();
        let phase = visible_start - base;
        let first = base as usize;
        let end = first + num_cols;
        let clip_cols = ((clip_width - phase).ceil().max(0.0) as usize).max(end);
        Some(Self {
            phase,
            first,
            end,
            clip_cols,
        })
    }

    fn len(&self) -> usize {
        self.end - self.first
    }

    /// Clip-local x of column `col`.
    fn x(&self, col: usize) -> f32 {
        self.phase + col as f32
    }

    /// Column ranges of the tiles covering the visible columns. Each depends
    /// only on its tile index and the clip's width, never on the scroll.
    fn tiles(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        let first_tile = self.first / WAVEFORM_TILE_COLS;
        let last_tile = (self.end - 1) / WAVEFORM_TILE_COLS;
        (first_tile..=last_tile).map(move |tile| {
            let start = tile * WAVEFORM_TILE_COLS;
            start..(start + WAVEFORM_TILE_COLS).min(self.clip_cols)
        })
    }

    /// Indices of `bars` that fall on visible columns. Bars are in column
    /// order but silent columns are skipped, so this searches by x.
    fn visible_slice(&self, bars: &WaveformBars) -> Range<usize> {
        let (lo, hi) = (self.x(self.first), self.x(self.end));
        bars.partition_point(|bar| bar.0 < lo)..bars.partition_point(|bar| bar.0 < hi)
    }
}

/// A clip's on-screen bars: the tiles covering the visible columns, each with
/// the range of its bars that is on screen.
type VisibleBars = Vec<(Arc<WaveformBars>, Range<usize>)>;

fn tile_key(base: &DefaultHasher, phase: f32, cols: &Range<usize>) -> u64 {
    let mut hasher = base.clone();
    phase.to_bits().hash(&mut hasher);
    (cols.start as u64).hash(&mut hasher);
    (cols.end as u64).hash(&mut hasher);
    hasher.finish()
}

/// Fetch the bar tiles covering `window` from the geometry cache, building
/// every missing one in a single `build` call. Returns the visible bars and
/// whether every tile was a hit.
///
/// `base` hashes everything the bars depend on except the tile. `None` marks a
/// source that changes too fast to cache (a live take): then exactly the
/// visible columns are built, as one span, and nothing is published.
///
/// `build` gets the column ranges to build and returns their bars, plus
/// whether they may be published — `false` when the peaks changed between the
/// key being taken and the bars being read, so stale-keyed bars never land.
fn tiled_bars(
    window: &ColumnWindow,
    base: Option<&DefaultHasher>,
    build: impl FnOnce(&[Range<usize>]) -> (Vec<WaveformBars>, bool),
) -> (VisibleBars, bool) {
    let Some(base) = base else {
        let (mut built, _) = build(&[window.first..window.end]);
        let bars = Arc::new(built.pop().unwrap_or_default());
        let range = window.visible_slice(&bars);
        return (vec![(bars, range)], false);
    };
    let tiles: Vec<Range<usize>> = window.tiles().collect();
    let keys: Vec<u64> = tiles
        .iter()
        .map(|cols| tile_key(base, window.phase, cols))
        .collect();
    let mut found: Vec<Option<Arc<WaveformBars>>> = keys
        .iter()
        .map(|key| waveform_cache::geometry_cache_get(*key))
        .collect();
    let missing: Vec<usize> = (0..tiles.len()).filter(|i| found[*i].is_none()).collect();
    let all_hit = missing.is_empty();
    if !all_hit {
        let ranges: Vec<Range<usize>> = missing.iter().map(|i| tiles[*i].clone()).collect();
        let (built, publish) = build(&ranges);
        for (i, bars) in missing.into_iter().zip(built) {
            let bars = Arc::new(bars);
            if publish {
                waveform_cache::geometry_cache_put(keys[i], Arc::clone(&bars));
            }
            found[i] = Some(bars);
        }
    }
    let visible = found
        .into_iter()
        .flatten()
        .map(|bars| {
            let range = window.visible_slice(&bars);
            (bars, range)
        })
        .filter(|(_, range)| !range.is_empty())
        .collect();
    (visible, all_hit)
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
        Some(asset_key) => {
            let window =
                ColumnWindow::visible(clip_left, clip_width, state.viewport.viewport_width);
            // Only the level decisions are made under the file-cache lock; the
            // bars are built after it is released (see `chunk_visible_bars`).
            let plan = waveform_cache::with_file_entry(asset_key, |entry| {
                let Some(entry) = entry else {
                    return EntryPlan::Pending;
                };
                match waveform_cache::display_status_from_entry(entry) {
                    WaveformDisplayStatus::Ready { meta }
                    | WaveformDisplayStatus::Partial { meta, .. } => match window {
                        Some(window) => {
                            EntryPlan::Draw(plan_chunk_levels(entry, &meta, clip, state), window)
                        }
                        None => EntryPlan::Hidden,
                    },
                    WaveformDisplayStatus::Pending => EntryPlan::Pending,
                    WaveformDisplayStatus::Error(message) => EntryPlan::Failed(message),
                }
            });
            match plan {
                EntryPlan::Draw(plan, window) => {
                    draw_chunk_waveform(asset_key, &plan, window, color, clip, state, clip_width)
                }
                EntryPlan::Hidden => {
                    waveform_cache::record_timeline_render(1, 0, true);
                    empty_canvas()
                }
                EntryPlan::Pending => {
                    waveform_cache::record_timeline_render(1, 0, false);
                    import_status_canvas(&clip.audio_import, false, None)
                }
                EntryPlan::Failed(message) => {
                    waveform_cache::record_timeline_render(1, 0, false);
                    import_status_canvas(&AudioImportState::Failed { message }, true, None)
                }
            }
        }
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

/// What the file-cache lock hands back to the render path.
enum EntryPlan {
    Draw(ChunkPlan, ColumnWindow),
    /// Peaks are there, but less than a column of the clip is on screen.
    Hidden,
    Pending,
    Failed(String),
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

/// The peak levels a clip draws from this frame, chosen under the file-cache
/// lock. Decisions only — no aggregation happens while the lock is held.
struct ChunkPlan {
    revision: u64,
    total_frames: u64,
    sample_rate: u32,
    source_start: u64,
    source_end: u64,
    effective_time_ratio: f64,
    spp: usize,
    fallback_spp: Option<usize>,
    /// Set when the zoom is past the shipped ladder: the level to request.
    detail_spp: Option<usize>,
}

fn plan_chunk_levels(
    entry: &waveform_cache::FileEntry,
    meta: &waveform_cache::WaveformFileMeta,
    clip: &ClipState,
    state: &TimelineState,
) -> ChunkPlan {
    let pixels_per_second = state.viewport.pixels_per_second;
    let (source_start, source_end, effective_time_ratio) =
        resolve_waveform_source_range(clip, meta.total_frames, meta.sample_rate, state.bpm as f64);

    // Zoomed past the shipped peak ladder, the drawing runs out of data before
    // it runs out of pixels and the waveform turns into 256-frame blocks. Ask
    // for finer peaks over the source window that is actually on screen (the
    // request is made once the lock is released); this frame still draws with
    // whatever is already cached.
    let detail_spp = super::waveform_detail::detail_level_for_zoom(
        pixels_per_second * effective_time_ratio.max(1.0e-6) as f32,
        meta.sample_rate,
    );
    let desired_spp = detail_spp.unwrap_or_else(|| {
        waveform_cache::pick_best_samples_per_peak(pixels_per_second, meta.sample_rate)
    });
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
    ChunkPlan {
        revision: waveform_cache::entry_revision(entry),
        total_frames: meta.total_frames,
        sample_rate: meta.sample_rate,
        source_start,
        source_end,
        effective_time_ratio,
        spp,
        fallback_spp,
        detail_spp,
    }
}

/// One min/max bar per visible CSS pixel column. Bars are computed once and
/// drawn from a single GPUI `canvas` element via `paint_quad` — no per-pixel
/// child elements, no `MAX_VISIBLE_WIDTH` cap.
///
/// The per-column aggregation is the expensive part and is cached: a signature
/// over `(asset, peak revision, LOD, column tile, clip size, stretch)` keys an
/// immutable `Arc<WaveformBars>` per tile of [`WAVEFORM_TILE_COLS`] columns, so
/// repeated repaints with unchanged geometry (playback playhead, meters, hover,
/// selection, track height) and scrolls within the built tiles reuse the bars
/// instead of re-aggregating on the UI thread. Vertical scale comes from the
/// canvas bounds at paint time so track-height resize does not invalidate peaks.
fn draw_chunk_waveform(
    asset_key: &str,
    plan: &ChunkPlan,
    window: ColumnWindow,
    color: gpui::Rgba,
    clip: &ClipState,
    state: &TimelineState,
    clip_width: f32,
) -> gpui::Div {
    waveform_cache::record_timeline_render(1, window.len(), true);
    if let Some(detail_spp) = plan.detail_spp {
        if let super::timeline_state::ClipType::Audio {
            source_path: Some(source_path),
            ..
        } = &clip.clip_type
        {
            super::waveform_detail::note_needed(
                asset_key,
                source_path,
                detail_spp,
                plan.source_start,
                plan.source_end,
                plan.total_frames,
            );
        }
    }

    let (bars, _) = chunk_visible_bars(asset_key, plan, &window, clip, state, clip_width);

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

/// The visible bars of a chunk-backed clip, from the tile cache where it can,
/// and whether every tile was a hit.
///
/// A miss builds in three steps so the global file cache is held only for the
/// middle one: each column's source range is resolved unlocked; the peaks those
/// ranges read are taken out of the entry as `Arc`s (see [`PeakSpan`]); and the
/// columns are aggregated unlocked again.
fn chunk_visible_bars(
    asset_key: &str,
    plan: &ChunkPlan,
    window: &ColumnWindow,
    clip: &ClipState,
    state: &TimelineState,
    clip_width: f32,
) -> (VisibleBars, bool) {
    let output_len = SphereAudioProcessor::stretched_duration_samples(
        plan.source_end.saturating_sub(plan.source_start),
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
    let time_spec = TimeMapSpec::for_clip(clip, state);

    // Geometry signature: horizontal mapping only — peak values are normalized
    // and scaled vertically from the canvas bounds at paint time. The tile is
    // added per tile; nothing here depends on the scroll.
    let base = {
        let mut hasher = DefaultHasher::new();
        asset_key.hash(&mut hasher);
        plan.revision.hash(&mut hasher);
        (plan.spp as u64).hash(&mut hasher);
        plan.fallback_spp.map(|spp| spp as u64).hash(&mut hasher);
        clip_width.to_bits().hash(&mut hasher);
        plan.source_start.hash(&mut hasher);
        plan.source_end.hash(&mut hasher);
        plan.effective_time_ratio.to_bits().hash(&mut hasher);
        output_len.to_bits().hash(&mut hasher);
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
        time_spec.hash_into(clip, state, &mut hasher);
        hasher
    };

    let (bars, all_hit) = tiled_bars(window, Some(&base), |tiles| {
        let time_map = time_spec.build(clip, state, clip_w);
        let (spp, fallback_spp) = (plan.spp, plan.fallback_spp);

        // Unlocked: each column's source sample range, and the peak span every
        // column of every tile reads at each level.
        let tile_columns: Vec<Vec<(f32, f64, f64)>> = tiles
            .iter()
            .map(|cols| {
                cols.clone()
                    .map(|col| {
                        let x0 = window.x(col);
                        let x1 = x0 + 1.0;
                        let out0 = time_map.fraction_at(x0) * output_len;
                        let out1 = time_map.fraction_at(x1) * output_len;
                        let s0 = clip_output_local_to_source_sample(
                            out0,
                            plan.source_start,
                            plan.source_end,
                            plan.effective_time_ratio,
                            clip.stretch.reverse,
                        );
                        let s1 = clip_output_local_to_source_sample(
                            out1,
                            plan.source_start,
                            plan.source_end,
                            plan.effective_time_ratio,
                            clip.stretch.reverse,
                        );
                        (x0, s0.min(s1), s0.max(s1))
                    })
                    .collect()
            })
            .collect();
        let peak_span = |level: usize| -> Range<usize> {
            let mut span = usize::MAX..0;
            for &(_, lo, hi) in tile_columns.iter().flatten() {
                let p0 = sample_to_peak_index(lo, level);
                let p1 = sample_to_peak_index(hi, level).max(p0);
                span.start = span.start.min(p0);
                span.end = span.end.max(p1 + 1);
            }
            span
        };
        let span = peak_span(spp);
        let fallback_span = fallback_spp.map(|ladder| (ladder, peak_span(ladder)));

        // Locked: one `Arc` clone per chunk, and the revision the peaks are at.
        let snapshot = waveform_cache::with_file_entry(asset_key, |entry| {
            let entry = entry?;
            Some((
                waveform_cache::entry_revision(entry),
                waveform_cache::peak_span_in_entry(entry, spp, span.start, span.end),
                fallback_span.as_ref().map(|(ladder, span)| {
                    (
                        *ladder,
                        waveform_cache::peak_span_in_entry(entry, *ladder, span.start, span.end),
                    )
                }),
            ))
        });
        let Some((revision, level, fallback)) = snapshot else {
            // Invalidated since the plan was taken: nothing to draw this frame.
            return (vec![Vec::new(); tiles.len()], false);
        };

        // Unlocked again: the aggregation itself.
        crate::perf::count(
            "peak_points_drawn",
            tile_columns.iter().map(Vec::len).sum::<usize>() as u64,
        );
        let built = tile_columns
            .iter()
            .map(|columns| chunk_tile_bars(columns, spp, &level, fallback.as_ref()))
            .collect();
        // Peaks that moved on since the key was taken are drawn, not cached.
        (built, revision == plan.revision)
    });
    crate::perf::count(
        if all_hit {
            "waveform_geo_cache_hit"
        } else {
            "waveform_geo_cache_miss"
        },
        1,
    );
    (bars, all_hit)
}

/// Bars for one tile's columns, given each column's `(x, lo, hi)` source range.
fn chunk_tile_bars(
    columns: &[(f32, f64, f64)],
    spp: usize,
    level: &PeakSpan,
    fallback: Option<&(usize, PeakSpan)>,
) -> WaveformBars {
    let mut bars: WaveformBars = Vec::with_capacity(columns.len());
    for &(x0, lo, hi) in columns {
        let p0 = sample_to_peak_index(lo, spp);
        let p1 = sample_to_peak_index(hi, spp).max(p0);
        let peak = level.aggregate(p0, p1 + 1).or_else(|| {
            // Nothing at this level for this column. Re-index the same
            // source range against the coarse ladder rather than dropping
            // the bar: the picture is what the user is looking at, and a
            // blockier one beats none.
            let (ladder, span) = fallback?;
            let f0 = sample_to_peak_index(lo, *ladder);
            let f1 = sample_to_peak_index(hi, *ladder).max(f0);
            span.aggregate(f0, f1 + 1)
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
    bars
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
    let Some(window) = ColumnWindow::visible(clip_left, clip_width, state.viewport.viewport_width)
    else {
        return empty_canvas();
    };
    let samples_per_pixel = (preview.total_frames.max(1) as f32 / clip_width.max(1.0)).max(1.0);
    let Some(lod) = waveform_cache::pick_lod(preview, samples_per_pixel) else {
        return empty_canvas();
    };

    let total_peaks = lod.peaks.len().max(1);
    let clip_w = clip_width.max(1.0);

    let base = identity.map(|id| {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        (lod.samples_per_peak as u64).hash(&mut hasher);
        (total_peaks as u64).hash(&mut hasher);
        clip_width.to_bits().hash(&mut hasher);
        hasher
    });

    let (bars, _) = tiled_bars(&window, base.as_ref(), |tiles| {
        let built = tiles
            .iter()
            .map(|cols| {
                cols.clone()
                    .map(|col| {
                        let x0 = window.x(col);
                        let x1 = x0 + 1.0;
                        let frac0 = (x0 / clip_w).max(0.0);
                        let frac1 = (x1 / clip_w).min(1.0);
                        // Every column is inside the clip, so `p0` is too; the
                        // clamp only keeps a rounding edge from slicing past it.
                        let p0 = ((frac0 * total_peaks as f32).floor() as usize)
                            .min(total_peaks - 1);
                        let p1 = (frac1 * total_peaks as f32).ceil() as usize;
                        let end = p1.min(total_peaks).max(p0 + 1);
                        let agg = aggregate_slice(&lod.peaks[p0..end]);
                        let mn = agg.min.max(-1.0);
                        let mx = agg.max.min(1.0);
                        (x0, mn, mx)
                    })
                    .collect()
            })
            .collect();
        (built, true)
    });

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
    bars: &VisibleBars,
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
    // One scene layer for the clip's bars. Outside a layer every quad claims
    // its own draw order from the scene's bounds tree — a tree insert per
    // pixel column. The bars never overlap one another, so they can share the
    // layer's single order; the layer is the canvas's own bounds, so it orders
    // against every other element exactly as the bars did. The quads are
    // unchanged, so the pixels are too.
    window.paint_layer(bounds, |window| {
        for (tile, range) in bars {
            for (x, mn, mx) in &tile[range.clone()] {
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
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        AudioClipStretchState, ClipType, TempoCurve,
    };
    use waveform_cache::{WaveformFileMeta, WaveformLod, WaveformPreview, CHUNK_PEAKS, LOD_LEVELS};

    const SAMPLE_RATE: u32 = 48_000;
    const VIEWPORT_W: f32 = 1600.0;

    /// Deterministic noise peaks at the finest level, with silent stretches so
    /// the skip-silence path is exercised too.
    fn fine_peaks(count: usize, seed: u64) -> Vec<WaveformPeak> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..count)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                if i % 97 < 6 {
                    return WaveformPeak { min: 0.0, max: 0.0 };
                }
                let a = (s & 0xFFFF) as f32 / 65_535.0;
                let b = ((s >> 16) & 0xFFFF) as f32 / 65_535.0;
                WaveformPeak {
                    min: -a * 1.2,
                    max: b * 1.2,
                }
            })
            .collect()
    }

    fn ladder(fine: Vec<WaveformPeak>) -> Vec<WaveformLod> {
        let mut lods = vec![WaveformLod {
            samples_per_peak: LOD_LEVELS[0],
            peaks: fine,
        }];
        for &spp in &LOD_LEVELS[1..] {
            let peaks = lods
                .last()
                .unwrap()
                .peaks
                .chunks(2)
                .map(aggregate_slice)
                .collect();
            lods.push(WaveformLod {
                samples_per_peak: spp,
                peaks,
            });
        }
        lods
    }

    fn frames(seconds: f64) -> u64 {
        (seconds * SAMPLE_RATE as f64) as u64
    }

    fn fine_count(seconds: f64) -> usize {
        (frames(seconds) as usize).div_ceil(LOD_LEVELS[0])
    }

    /// A fully imported asset: preview ladder and its chunks.
    fn install_ready(key: &str, seconds: f64, seed: u64) {
        waveform_cache::install_ready(
            key,
            Arc::new(WaveformPreview {
                sample_rate: SAMPLE_RATE,
                channels: 1,
                duration_seconds: seconds,
                total_frames: frames(seconds),
                lods: ladder(fine_peaks(fine_count(seconds), seed)),
            }),
        );
    }

    /// An import still in progress: only some primary chunks, no ladder.
    fn install_partial(key: &str, seconds: f64, seed: u64) {
        let fine = fine_peaks(fine_count(seconds), seed);
        let chunks_total = fine.len().div_ceil(CHUNK_PEAKS);
        waveform_cache::begin_peak_build(
            key,
            Arc::new(WaveformFileMeta {
                sample_rate: SAMPLE_RATE,
                channels: 1,
                duration_seconds: seconds,
                total_frames: frames(seconds),
                peak_count: fine.len(),
                primary_spp: LOD_LEVELS[0],
            }),
            chunks_total,
        );
        for (chunk, peaks) in fine.chunks(CHUNK_PEAKS).enumerate() {
            if chunk % 3 != 1 {
                waveform_cache::install_chunk(
                    key,
                    LOD_LEVELS[0] as u32,
                    chunk as u32,
                    Arc::new(peaks.to_vec()),
                );
            }
        }
    }

    fn audio_clip(asset_key: &str, start_beat: f32, seconds: f64) -> ClipState {
        let mut clip = ClipState {
            id: format!("clip-{asset_key}"),
            name: asset_key.to_string(),
            start_beat,
            duration_beats: (seconds * 2.0) as f32,
            source_duration_seconds: Some(seconds),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: asset_key.to_string(),
                source_path: Some(format!("{asset_key}.wav")),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = SAMPLE_RATE;
        clip.stretch.project_sample_rate = SAMPLE_RATE;
        clip.stretch.original_duration_samples = frames(seconds);
        clip.stretch.source_start_samples = 0;
        clip.stretch.source_end_samples = frames(seconds);
        clip
    }

    fn timeline(pixels_per_second: f32, tempo_automation: bool) -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.viewport.viewport_width = VIEWPORT_W;
        state.viewport.pixels_per_second = pixels_per_second;
        state.sync_pixels_per_beat();
        if tempo_automation {
            for marker in 0..6 {
                state.tempo_map.add_or_update_point(
                    marker as f64 * 6.0,
                    96.0 + (marker % 4) as f64 * 14.0,
                    if marker % 2 == 0 {
                        TempoCurve::Linear
                    } else {
                        TempoCurve::Hold
                    },
                );
            }
            state.refresh_tempo_cache();
        }
        state
    }

    fn plan_for(asset_key: &str, clip: &ClipState, state: &TimelineState) -> ChunkPlan {
        waveform_cache::with_file_entry(asset_key, |entry| {
            let entry = entry.expect("asset installed");
            match waveform_cache::display_status_from_entry(entry) {
                WaveformDisplayStatus::Ready { meta }
                | WaveformDisplayStatus::Partial { meta, .. } => {
                    plan_chunk_levels(entry, &meta, clip, state)
                }
                _ => panic!("asset has no peaks"),
            }
        })
    }

    fn tiled(asset_key: &str, clip: &ClipState, state: &TimelineState) -> (VisibleBars, bool) {
        let (clip_left, clip_width) = state.clip_lane_x_span(clip);
        let window = ColumnWindow::visible(clip_left, clip_width, state.viewport.viewport_width)
            .expect("clip on screen");
        let plan = plan_for(asset_key, clip, state);
        chunk_visible_bars(asset_key, &plan, &window, clip, state, clip_width)
    }

    fn flatten(bars: &VisibleBars) -> Vec<(f32, f32, f32)> {
        bars.iter()
            .flat_map(|(tile, range)| tile[range.clone()].iter().copied())
            .collect()
    }

    /// The per-visible-window build this replaced, verbatim apart from being
    /// uncached: lane-space tempo mapping, a tempo map built per call, and the
    /// aggregation run against the locked entry.
    fn per_window_bars(
        asset_key: &str,
        clip: &ClipState,
        state: &TimelineState,
    ) -> Vec<(f32, f32, f32)> {
        let (clip_left, clip_width) = state.clip_lane_x_span(clip);
        let plan = plan_for(asset_key, clip, state);
        let viewport_w = state.viewport.viewport_width.max(1.0);
        let visible_start = (-clip_left).max(0.0);
        let visible_end = clip_width
            .min((viewport_w - clip_left).max(visible_start))
            .max(visible_start);
        let num_cols = ((visible_end - visible_start).ceil() as usize).max(1);
        let output_len = SphereAudioProcessor::stretched_duration_samples(
            plan.source_end.saturating_sub(plan.source_start),
            &clip.stretch.to_sphere_stretch_params(state.bpm as f64),
            Some(state.bpm),
        )
        .max(1) as f64;
        let clip_w = clip_width.max(1.0) as f64;
        let project_bpm = state.bpm.max(1.0) as f64;
        let tempo = state
            .tempo_map
            .has_automation()
            .then(|| clip.stretch.played_seconds_for_project_bpm(project_bpm))
            .flatten()
            .filter(|played| *played > 0.0)
            .map(|played| {
                let tempo = state.tempo_map.to_engine_map(project_bpm);
                let start = tempo.seconds_at_beat(clip.start_beat.max(0.0) as f64);
                (tempo, start, played)
            });
        let fraction_at = |x: f32| match &tempo {
            None => ((x as f64) / clip_w).clamp(0.0, 1.0),
            Some((tempo, start_seconds, played_seconds)) => {
                let lane_x = clip_left as f64 + (x as f64).clamp(0.0, clip_w);
                let beat = super::super::timeline_state::x_to_beat(lane_x as f32, &state.viewport);
                let seconds = tempo.seconds_at_beat(beat) - start_seconds;
                (seconds / played_seconds).clamp(0.0, 1.0)
            }
        };
        waveform_cache::with_file_entry(asset_key, |entry| {
            let entry = entry.expect("asset installed");
            let mut bars = Vec::new();
            for col in 0..num_cols {
                let x0 = visible_start + col as f32;
                let x1 = x0 + 1.0;
                let s0 = clip_output_local_to_source_sample(
                    fraction_at(x0) * output_len,
                    plan.source_start,
                    plan.source_end,
                    plan.effective_time_ratio,
                    clip.stretch.reverse,
                );
                let s1 = clip_output_local_to_source_sample(
                    fraction_at(x1) * output_len,
                    plan.source_start,
                    plan.source_end,
                    plan.effective_time_ratio,
                    clip.stretch.reverse,
                );
                let (lo, hi) = (s0.min(s1), s0.max(s1));
                let p0 = sample_to_peak_index(lo, plan.spp);
                let p1 = sample_to_peak_index(hi, plan.spp).max(p0);
                let peak = waveform_cache::aggregate_peak_range_in_entry_opt(
                    entry,
                    plan.spp,
                    p0,
                    p1 + 1,
                )
                .or_else(|| {
                    let ladder = plan.fallback_spp?;
                    let f0 = sample_to_peak_index(lo, ladder);
                    let f1 = sample_to_peak_index(hi, ladder).max(f0);
                    waveform_cache::aggregate_peak_range_in_entry_opt(entry, ladder, f0, f1 + 1)
                });
                let Some(WaveformPeak { min, max }) = peak else {
                    continue;
                };
                if min == 0.0 && max == 0.0 {
                    continue;
                }
                bars.push((x0, min.max(-1.0), max.min(1.0)));
            }
            bars
        })
    }

    /// Scrolls that put the clip across the left edge, the right edge, both,
    /// and neither — on and off tile boundaries.
    const SCROLLS: [f32; 9] = [0.0, 1.0, 37.0, 255.0, 256.0, 257.0, 300.0, 811.0, 2_000.0];

    fn assert_matches_per_window_build(
        asset_key: &str,
        state: &mut TimelineState,
        clip: &ClipState,
    ) {
        let mut compared = 0;
        for scroll in SCROLLS {
            state.viewport.scroll_x = scroll;
            let (clip_left, clip_width) = state.clip_lane_x_span(clip);
            if ColumnWindow::visible(clip_left, clip_width, VIEWPORT_W).is_none() {
                continue;
            }
            let expected = per_window_bars(asset_key, clip, state);
            assert!(!expected.is_empty(), "{asset_key}: scroll {scroll} drew nothing");
            // Twice: once built, once from the cache.
            for pass in 0..2 {
                let (bars, _) = tiled(asset_key, clip, state);
                assert_eq!(
                    flatten(&bars),
                    expected,
                    "{asset_key}: scroll {scroll}, pass {pass} drew different bars"
                );
            }
            compared += 1;
        }
        assert!(compared >= 3, "{asset_key}: only {compared} scrolls had the clip on screen");
    }

    #[test]
    fn tiled_bars_match_the_per_window_build_on_the_ladder() {
        for (pps, key) in [(40.0, "tile-test-ladder-40"), (3.0, "tile-test-ladder-3")] {
            install_ready(key, 600.0, 7);
            let mut state = timeline(pps, false);
            let clip = audio_clip(key, 2.0, 600.0);
            assert_matches_per_window_build(key, &mut state, &clip);
        }
    }

    #[test]
    fn tiled_bars_match_the_per_window_build_on_partial_chunks() {
        // Columns spanning several chunks, some of them not resident yet.
        for (pps, key) in [(40.0, "tile-test-partial-40"), (2.0, "tile-test-partial-2")] {
            install_partial(key, 600.0, 11);
            let mut state = timeline(pps, false);
            let clip = audio_clip(key, 0.0, 600.0);
            assert_matches_per_window_build(key, &mut state, &clip);
        }
    }

    /// Tempo automation resolves columns in content space; at a whole-pixel
    /// scroll that is exactly the lane-space mapping it replaced.
    #[test]
    fn tiled_bars_match_the_per_window_build_under_tempo_automation() {
        let key = "tile-test-tempo";
        install_ready(key, 120.0, 13);
        let mut state = timeline(40.0, true);
        let clip = audio_clip(key, 3.0, 120.0);
        assert!(matches!(
            TimeMapSpec::for_clip(&clip, &state),
            TimeMapSpec::Tempo { .. }
        ));
        assert_matches_per_window_build(key, &mut state, &clip);
    }

    fn assert_one_pixel_scroll_hits(key: &str, state: &mut TimelineState, clip: &ClipState) {
        state.viewport.scroll_x = 300.0;
        let (first, first_hit) = tiled(key, clip, state);
        assert!(!first_hit, "a fresh asset cannot hit");
        for scroll in [301.0, 300.25, 299.0] {
            state.viewport.scroll_x = scroll;
            let (again, hit) = tiled(key, clip, state);
            assert!(hit, "scrolling to {scroll} inside the same tiles missed the cache");
            for ((a, _), (b, _)) in first.iter().zip(&again) {
                assert!(Arc::ptr_eq(a, b), "scroll {scroll} rebuilt a tile");
            }
        }
    }

    #[test]
    fn scrolling_within_a_tile_hits_the_cache() {
        let key = "tile-test-scroll-hit";
        install_ready(key, 120.0, 17);
        let mut state = timeline(40.0, false);
        let clip = audio_clip(key, 0.0, 120.0);
        assert_one_pixel_scroll_hits(key, &mut state, &clip);
    }

    /// The miss the tiles fixed hardest: with tempo automation the old key held
    /// the clip's lane x, so every audio clip missed on any horizontal scroll.
    #[test]
    fn scrolling_within_a_tile_hits_the_cache_under_tempo_automation() {
        let key = "tile-test-scroll-hit-tempo";
        install_ready(key, 120.0, 19);
        let mut state = timeline(40.0, true);
        let clip = audio_clip(key, 0.0, 120.0);
        assert_one_pixel_scroll_hits(key, &mut state, &clip);
    }

    #[test]
    fn crossing_a_tile_builds_only_the_new_tiles() {
        let key = "tile-test-scroll-cross";
        install_ready(key, 120.0, 23);
        let mut state = timeline(40.0, false);
        let clip = audio_clip(key, 0.0, 120.0);
        state.viewport.scroll_x = 300.0;
        let (before, _) = tiled(key, &clip, &state);
        // 300 → 600: columns 300..1900 become 600..2200, so tile 256..512
        // drops off and 2048..2304 is revealed; the rest is reused.
        state.viewport.scroll_x = 600.0;
        let (after, hit) = tiled(key, &clip, &state);
        assert!(!hit);
        let reused = after
            .iter()
            .filter(|(tile, _)| before.iter().any(|(old, _)| Arc::ptr_eq(old, tile)))
            .count();
        assert_eq!(reused, after.len() - 1, "only the newly revealed tile rebuilds");
    }

    #[test]
    fn column_window_tiles_do_not_depend_on_the_scroll() {
        let clip_width = 5_000.0;
        let at = |clip_left: f32| {
            let window = ColumnWindow::visible(clip_left, clip_width, VIEWPORT_W).unwrap();
            (window, window.tiles().collect::<Vec<_>>())
        };
        let (a, tiles_a) = at(-300.0);
        let (b, tiles_b) = at(-301.0);
        assert_eq!((a.first, a.end), (300, 1_900));
        assert_eq!((b.first, b.end), (301, 1_901));
        assert_eq!(tiles_a, tiles_b);
        assert_eq!(tiles_a.first(), Some(&(256..512)));
        assert_eq!(tiles_a.last(), Some(&(1_792..2_048)));
        // The last tile stops at the clip, not at the viewport.
        let (_, tail) = at(-(clip_width - 100.0));
        assert_eq!(tail.last(), Some(&(4_864..5_000)));
        // A clip starting on screen has no phase and starts at column 0.
        let (on_screen, _) = at(40.0);
        assert_eq!((on_screen.phase, on_screen.first), (0.0, 0));
        // Off screen, or under a column visible, there is nothing to tile.
        assert!(ColumnWindow::visible(VIEWPORT_W + 1.0, clip_width, VIEWPORT_W).is_none());
        assert!(ColumnWindow::visible(-clip_width + 0.5, clip_width, VIEWPORT_W).is_none());
    }

    /// A fractional visible start keeps the old column grid: column x is the
    /// visible start plus a whole number, bit for bit.
    #[test]
    fn column_window_keeps_a_fractional_grid() {
        let window = ColumnWindow::visible(-300.25, 2_000.0, VIEWPORT_W).unwrap();
        assert_eq!(window.phase, 0.25);
        for col in 0..window.len() {
            assert_eq!(window.x(window.first + col), 300.25 + col as f32);
        }
    }

    #[test]
    fn visible_slice_selects_only_on_screen_bars() {
        let window = ColumnWindow::visible(-300.0, 2_000.0, VIEWPORT_W).unwrap();
        // Tile 256..512 with a silent gap at 299..=301.
        let bars: WaveformBars = (256..512)
            .filter(|col| !(299..=301).contains(col))
            .map(|col| (col as f32, -0.5, 0.5))
            .collect();
        let range = window.visible_slice(&bars);
        assert_eq!(bars[range.clone()].first().map(|bar| bar.0), Some(302.0));
        assert_eq!(bars[range].last().map(|bar| bar.0), Some(511.0));
    }
}
