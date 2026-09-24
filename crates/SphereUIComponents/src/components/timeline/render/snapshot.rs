//! Immutable render snapshots built on the UI thread from [`TimelineState`].
//!
//! Renderers must treat these as read-only: no audio decode, no peak generation.

use std::sync::Arc;

use super::viewport::TimelineViewport;
use crate::components::timeline::timeline_state::{
    clip_output_local_to_source_sample, ClipState, ClipType, GridLineLevel, TimelineState,
    TrackState, DEFAULT_TRACK_HEIGHT,
};
use crate::components::timeline::waveform_cache::{
    self, WaveformDisplayStatus, CHUNK_PEAKS, PEAK_FINE_SPP,
};
use crate::components::timeline::waveform_canvas::resolve_waveform_source_range;
use gpui::Rgba;

/// Track rows included in this snapshot (after vertical virtualization).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleTrackRange {
    pub start_index: usize,
    pub end_index: usize,
}

/// Beat interval visible in the lane viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibleBeatRange {
    pub start_beat: f32,
    pub end_beat: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderClipKind {
    Audio,
    Midi,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformReadyKind {
    Pending,
    Partial,
    Ready,
    Error,
}

/// Opaque handle to precomputed peak chunks — WGPU path binds GPU buffers from cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveformChunkHandle {
    /// Stable waveform-cache key (clip `file_id` / asset id), not the on-disk
    /// path — so the GPU binding survives a `source_path` rewrite.
    pub asset_key: String,
    pub samples_per_peak: u32,
    pub chunk_index_start: u32,
    pub chunk_index_end: u32,
    pub peak_index_start: usize,
    pub peak_index_end: usize,
    pub ready: WaveformReadyKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderClipSnapshot {
    pub id: String,
    pub track_id: String,
    pub track_index: usize,
    pub name: String,
    pub kind: RenderClipKind,
    pub color: [f32; 4],
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub selected: bool,
    pub muted: bool,
    pub waveform: Option<WaveformChunkHandle>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderLaneSnapshot {
    pub track_index: usize,
    pub track_id: String,
    pub y: f32,
    pub height: f32,
    pub even_row: bool,
    pub selected: bool,
    pub color: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridLineSnapshot {
    pub x: f32,
    pub beat: f32,
    pub level: GridLineLevel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarShadeSnapshot {
    pub x: f32,
    pub width: f32,
    pub bar: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayheadSnapshot {
    pub beat: f32,
    pub x: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectionSnapshot {
    pub selected_track_id: Option<String>,
    pub selected_clip_ids: Vec<String>,
}

/// Immutable description of one arrangement paint pass.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineRenderSnapshot {
    pub viewport: TimelineViewport,
    pub bpm: f32,
    pub beats_per_bar: f32,
    pub time_signature_revision: u64,
    pub visible_tracks: VisibleTrackRange,
    pub visible_beats: VisibleBeatRange,
    pub lanes: Vec<RenderLaneSnapshot>,
    pub clips: Vec<RenderClipSnapshot>,
    pub grid_lines: Vec<GridLineSnapshot>,
    pub bar_shades: Vec<BarShadeSnapshot>,
    pub playhead: PlayheadSnapshot,
    pub selection: SelectionSnapshot,
    pub track_insert_y: Option<f32>,
}

pub struct SnapshotBuildOptions {
    pub scale_factor: f32,
    pub track_overscan: usize,
}

impl Default for SnapshotBuildOptions {
    fn default() -> Self {
        Self {
            scale_factor: 1.0,
            track_overscan: 2,
        }
    }
}

impl TimelineRenderSnapshot {
    /// Build a snapshot, deriving the arrangement row geometry from `state`.
    /// Prefer [`Self::from_row_layout`] on the render path, where the caller
    /// already holds this frame's layout.
    pub fn from_state(state: &TimelineState, options: SnapshotBuildOptions) -> Self {
        let row_layout = state.track_row_layout();
        Self::from_row_layout(state, &row_layout, options)
    }

    /// Build a snapshot against an already-built row layout.
    ///
    /// Row layout is O(track_count) and owns cloned track ids. The timeline
    /// repaint builds it once and shares it between the scroll geometry, the
    /// GPUI track list, and this snapshot.
    pub fn from_row_layout(
        state: &TimelineState,
        row_layout: &crate::components::timeline::timeline_state::TrackRowLayout,
        options: SnapshotBuildOptions,
    ) -> Self {
        let grid_width = state.viewport.viewport_width.max(1.0);
        let grid_height = state.viewport.viewport_height.max(DEFAULT_TRACK_HEIGHT);
        let seconds_per_beat = state.seconds_per_beat();
        let pixels_per_beat = state.viewport.pixels_per_second * seconds_per_beat;

        let viewport = TimelineViewport::new(
            grid_width,
            grid_height,
            options.scale_factor,
            state.viewport.scroll_x,
            state.viewport.scroll_y,
            pixels_per_beat,
            state.viewport.pixels_per_second,
            seconds_per_beat,
        );

        let visible_tracks = visible_track_range(state, row_layout, options.track_overscan);
        // Positions come from the state's transform, which follows the tempo
        // map (real-time layout); the render viewport only carries zoom/size.
        let (visible_start, visible_end) = state.visible_beat_range(grid_width);
        let visible_beats = VisibleBeatRange {
            start_beat: visible_start,
            end_beat: visible_end.max(visible_start),
        };

        let lanes = build_lanes(state, row_layout, &visible_tracks);
        let clips = build_clips(state, row_layout, &visible_tracks, &viewport);
        let grid_lines = state
            .arrangement_grid_lines(grid_width)
            .iter()
            .map(|line| GridLineSnapshot {
                x: line.x,
                beat: line.beat,
                level: line.level,
            })
            .collect();
        let bar_shades = build_bar_shades(state, &viewport);

        let playhead = PlayheadSnapshot {
            beat: state.transport.playhead_beats,
            x: state.beats_to_x(state.transport.playhead_beats),
        };

        let selection = SelectionSnapshot {
            selected_track_id: state.selection.selected_track_id.clone(),
            selected_clip_ids: state.selection.selected_clip_ids.clone(),
        };

        let track_insert_y = state.drag_target_index.and_then(|index| {
            row_layout.row_for_index(index).map(|row| {
                (row.y - state.viewport.scroll_y).clamp(0.0, grid_height.max(DEFAULT_TRACK_HEIGHT))
            })
        });

        Self {
            viewport,
            bpm: state.bpm,
            beats_per_bar: state.beats_per_bar(),
            time_signature_revision: state.time_signature_map.revision(),
            visible_tracks,
            visible_beats,
            lanes,
            clips,
            grid_lines,
            bar_shades,
            playhead,
            selection,
            track_insert_y,
        }
    }
}

fn visible_track_range(
    state: &TimelineState,
    row_layout: &crate::components::timeline::timeline_state::TrackRowLayout,
    overscan: usize,
) -> VisibleTrackRange {
    let track_count = row_layout.rows.len();
    if track_count == 0 {
        return VisibleTrackRange {
            start_index: 0,
            end_index: 0,
        };
    }
    let scroll_y = state.viewport.scroll_y;
    let viewport_height = state.viewport.viewport_height;
    let (visible_start, visible_end, _, _) =
        crate::components::timeline::track_resize::visible_track_row_range(
            row_layout,
            scroll_y,
            viewport_height,
            overscan,
        );
    VisibleTrackRange {
        start_index: visible_start,
        end_index: visible_end,
    }
}

fn build_lanes(
    state: &TimelineState,
    row_layout: &crate::components::timeline::timeline_state::TrackRowLayout,
    range: &VisibleTrackRange,
) -> Vec<RenderLaneSnapshot> {
    state.tracks[range.start_index..range.end_index]
        .iter()
        .enumerate()
        .filter_map(|(rel, track)| {
            // `row_layout.rows` is 1:1 with `state.tracks`, so this is an index
            // lookup, not an id scan.
            let index = range.start_index + rel;
            let row = row_layout.row_for_index(index)?;
            // Mixer-only channels (Bus/Return + VSTi multi-out children) and
            // collapsed group children are excluded from the arrangement canvas
            // the same way the GPUI track list excludes them: the layout already
            // collapsed them to zero height.
            if row.height <= 0.0 {
                return None;
            }
            let y = row.y - state.viewport.scroll_y;
            Some(RenderLaneSnapshot {
                track_index: index,
                track_id: track.id.clone(),
                y,
                height: row.height,
                even_row: index % 2 == 0,
                selected: state.selection.selected_track_id.as_deref() == Some(track.id.as_str()),
                color: rgba_to_array(track.color),
            })
        })
        .collect()
}

fn build_clips(
    state: &TimelineState,
    row_layout: &crate::components::timeline::timeline_state::TrackRowLayout,
    range: &VisibleTrackRange,
    viewport: &TimelineViewport,
) -> Vec<RenderClipSnapshot> {
    let mut clips = Vec::new();
    let pad = 7.0_f32;
    for (rel, track) in state.tracks[range.start_index..range.end_index]
        .iter()
        .enumerate()
    {
        let track_index = range.start_index + rel;
        // Index lookup: `row_layout.rows` is 1:1 with `state.tracks`.
        let Some(row) = row_layout.row_for_index(track_index) else {
            continue;
        };
        // Mixer-only channels never carry arrangement clips, and collapsed group
        // children have no arrangement row — both are zero height in the layout.
        if row.height <= 0.0 {
            continue;
        }
        let clip_h = row.height - pad * 2.0;
        for clip in &track.clips {
            let clip_left = state.beats_to_x(clip.start_beat);
            let clip_width = state
                .beat_span_px(clip.start_beat, clip.duration_beats)
                .max(10.0);
            if clip_left + clip_width < 0.0 || clip_left > viewport.width {
                continue;
            }
            let clip_y = row.y - state.viewport.scroll_y + pad;
            clips.push(build_clip_snapshot(
                clip,
                track,
                track_index,
                clip_left,
                clip_y,
                clip_width,
                clip_h,
                state,
                viewport,
            ));
        }
    }
    clips
}

fn build_clip_snapshot(
    clip: &ClipState,
    track: &TrackState,
    track_index: usize,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    state: &TimelineState,
    viewport: &TimelineViewport,
) -> RenderClipSnapshot {
    let kind = match clip.clip_type {
        ClipType::Audio { .. } => RenderClipKind::Audio,
        ClipType::Midi { .. } => RenderClipKind::Midi,
        ClipType::Video { .. } => RenderClipKind::Video,
    };
    let waveform = clip
        .audio_asset_key()
        .map(|asset_key| waveform_handle_for_clip(asset_key, clip, state, viewport));

    RenderClipSnapshot {
        id: clip.id.clone(),
        track_id: track.id.clone(),
        track_index,
        name: clip.name.clone(),
        kind,
        color: rgba_to_array(track.color),
        x,
        y,
        width,
        height,
        selected: state.selection.selected_clip_ids.contains(&clip.id),
        muted: clip.muted,
        waveform,
    }
}

fn waveform_handle_for_clip(
    asset_key: &str,
    clip: &ClipState,
    state: &TimelineState,
    viewport: &TimelineViewport,
) -> WaveformChunkHandle {
    let status = waveform_cache::get_file_status(asset_key);
    let ready = match &status {
        WaveformDisplayStatus::Ready { .. } => WaveformReadyKind::Ready,
        WaveformDisplayStatus::Partial { .. } => WaveformReadyKind::Partial,
        WaveformDisplayStatus::Pending => WaveformReadyKind::Pending,
        WaveformDisplayStatus::Error(_) => WaveformReadyKind::Error,
    };

    let (samples_per_peak, peak_count) = match status {
        WaveformDisplayStatus::Ready { meta } | WaveformDisplayStatus::Partial { meta, .. } => {
            let spp = waveform_cache::pick_best_samples_per_peak(
                viewport.pixels_per_second,
                meta.sample_rate,
            ) as u32;
            (spp, meta.peak_count)
        }
        _ => (PEAK_FINE_SPP as u32, 0),
    };

    let sample_rate = waveform_cache::get_file_status(asset_key)
        .ready_meta()
        .map(|m| m.sample_rate)
        .unwrap_or(48_000);
    let total_frames = waveform_cache::get_file_status(asset_key)
        .ready_meta()
        .map(|m| m.total_frames)
        .unwrap_or(clip.stretch.original_duration_samples);
    let (source_start, source_end, effective_time_ratio) =
        resolve_waveform_source_range(clip, total_frames, sample_rate, state.bpm as f64);
    let output_len =
        (source_end.saturating_sub(source_start) as f64 * effective_time_ratio).max(1.0);
    let s0 = clip_output_local_to_source_sample(
        0.0,
        source_start,
        source_end,
        effective_time_ratio,
        clip.stretch.reverse,
    );
    let s1 = clip_output_local_to_source_sample(
        output_len,
        source_start,
        source_end,
        effective_time_ratio,
        clip.stretch.reverse,
    );
    let p0 = sample_to_peak_index(s0.min(s1), samples_per_peak as usize);
    let p1 = sample_to_peak_index(s0.max(s1), samples_per_peak as usize)
        .max(p0)
        .min(peak_count.saturating_sub(1));

    WaveformChunkHandle {
        asset_key: asset_key.to_string(),
        samples_per_peak,
        chunk_index_start: (p0 / CHUNK_PEAKS) as u32,
        chunk_index_end: (p1 / CHUNK_PEAKS) as u32,
        peak_index_start: p0,
        peak_index_end: p1,
        ready,
    }
}

fn sample_to_peak_index(sample: f64, samples_per_peak: usize) -> usize {
    sample.max(0.0) as usize / samples_per_peak.max(1)
}

fn build_bar_shades(state: &TimelineState, viewport: &TimelineViewport) -> Vec<BarShadeSnapshot> {
    let (visible_start, visible_end) = state.visible_beat_range(viewport.width);
    let rects = state
        .time_signature_map
        .visible_bar_rects(visible_start as f64, visible_end as f64);
    let mut shades = Vec::with_capacity(rects.len());
    for rect in rects {
        // Alternate by global bar number: even bars get the subtle region fill.
        if rect.bar % 2 != 0 {
            continue;
        }
        let x0 = state.beats_to_x(rect.start_beat as f32);
        let x1 = state.beats_to_x(rect.end_beat as f32);
        // The bar straddling the left edge starts *before* the viewport, so its
        // unclamped x is negative — and the shade is a translucent wash, so a
        // negative x paints it straight over the track-header column to the
        // left. Clamp the leading edge to the viewport and take the width off
        // the same edge, which is also what the bar visually occupies.
        let x0 = x0.max(0.0);
        let width = x1 - x0;
        if width < 2.0 {
            continue;
        }
        shades.push(BarShadeSnapshot {
            x: x0.round(),
            width: width.round(),
            bar: rect.bar,
        });
    }
    shades
}

fn rgba_to_array(c: Rgba) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

trait WaveformStatusExt {
    fn ready_meta(&self) -> Option<&Arc<waveform_cache::WaveformFileMeta>>;
}

impl WaveformStatusExt for WaveformDisplayStatus {
    fn ready_meta(&self) -> Option<&Arc<waveform_cache::WaveformFileMeta>> {
        match self {
            WaveformDisplayStatus::Ready { meta } | WaveformDisplayStatus::Partial { meta, .. } => {
                Some(meta)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod stress_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, TrackType,
    };

    fn stress_state(track_count: usize) -> TimelineState {
        let mut state = TimelineState::default();
        state.viewport.viewport_width = 1_200.0;
        state.viewport.viewport_height = 500.0;
        for index in 0..track_count {
            state.create_track(CreateTrackOptions {
                track_type: if index % 2 == 0 {
                    TrackType::Audio
                } else {
                    TrackType::Midi
                },
                name: format!("Track {index}"),
                color: crate::theme::Colors::track_color_for_index(index),
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
        }
        state
    }

    #[test]
    fn snapshot_virtualizes_one_thousand_tracks() {
        let state = stress_state(1_000);

        let snapshot = TimelineRenderSnapshot::from_state(&state, SnapshotBuildOptions::default());
        assert_eq!(state.tracks.len(), 1_000);
        assert!(snapshot.lanes.len() <= 12, "only viewport rows are drawn");
        assert!(
            snapshot.visible_tracks.end_index - snapshot.visible_tracks.start_index <= 12,
            "vertical overscan must stay bounded independently of track count"
        );
    }

    #[test]
    fn snapshot_virtualizes_two_thousand_tracks_when_scrolled_deep() {
        let mut state = stress_state(2_000);
        // Park the viewport in the middle of the arrangement — the worst case
        // for any lookup that scans from the top of the track list.
        state.viewport.scroll_y = state.total_track_rows_height() * 0.5;

        let snapshot = TimelineRenderSnapshot::from_state(&state, SnapshotBuildOptions::default());
        assert_eq!(state.tracks.len(), 2_000);
        assert!(snapshot.lanes.len() <= 12, "only viewport rows are drawn");
        assert!(
            snapshot.clips.len() <= 12 * 4,
            "clip build must stay inside the visible row window"
        );
        let first_lane = snapshot
            .lanes
            .first()
            .expect("a scrolled viewport has rows");
        assert!(
            first_lane.track_index > 100,
            "deep scroll must resolve to deep rows, got {}",
            first_lane.track_index
        );
    }

    /// The render path builds the row layout once and hands it to the snapshot.
    /// That shared-layout build must produce exactly what the self-contained one
    /// does, or the arrangement canvas would drift from the GPUI track list.
    #[test]
    fn shared_row_layout_snapshot_matches_the_self_contained_one() {
        let mut state = stress_state(64);
        state.viewport.scroll_y = 300.0;

        let owned = TimelineRenderSnapshot::from_state(&state, SnapshotBuildOptions::default());
        let row_layout = state.track_row_layout();
        let shared = TimelineRenderSnapshot::from_row_layout(
            &state,
            &row_layout,
            SnapshotBuildOptions::default(),
        );
        assert_eq!(owned, shared);
    }
}

/// The arrangement's bar shading is a translucent wash, and the track-header
/// column sits immediately to its left. Anything the grid emits at a negative x
/// therefore paints *over the headers* — which is what made a selected track
/// header look like it had no background of its own.
#[cfg(test)]
mod bar_shade_bounds_tests {
    use super::*;

    fn scrolled_state(scroll_x: f32) -> TimelineState {
        let mut state = TimelineState::default();
        state.viewport.viewport_width = 1_200.0;
        state.viewport.viewport_height = 500.0;
        state.viewport.scroll_x = scroll_x;
        state
    }

    /// Build through the real snapshot path so the test exercises the same
    /// viewport the renderer sees, not a hand-rolled one.
    fn shades(scroll_x: f32) -> Vec<BarShadeSnapshot> {
        let state = scrolled_state(scroll_x);
        TimelineRenderSnapshot::from_state(&state, SnapshotBuildOptions::default()).bar_shades
    }

    /// Scrolled to the very start there is nothing before the viewport, so this
    /// is the baseline the scrolled cases are compared against.
    #[test]
    fn shades_start_inside_the_lane_at_the_song_start() {
        for shade in shades(0.0) {
            assert!(shade.x >= 0.0, "shade at x={} before the lane", shade.x);
        }
    }

    /// Guards the premise of the clamp tests: this scroll offset has to land
    /// mid-bar, or "the leading bar starts before the viewport" is not the case
    /// being exercised at all.
    #[test]
    fn the_test_scroll_really_does_land_mid_bar() {
        let state = scrolled_state(9_137.0);
        let (visible_start, _) = state.visible_beat_range(state.viewport.viewport_width);
        let beats_per_bar = state.beats_per_bar_at_beat(visible_start as f64) as f32;
        let into_bar = visible_start % beats_per_bar;
        assert!(
            into_bar > 0.01,
            "scroll lands on a bar line ({visible_start} beats), so nothing straddles the edge"
        );
    }

    /// The regression: mid-song the bar under the left edge starts off-screen.
    #[test]
    fn no_scroll_position_puts_a_shade_left_of_the_lane() {
        // Only even bars are shaded, so any single scroll offset has an even
        // chance of putting an unshaded bar under the left edge and proving
        // nothing. Sweep a bar's worth of offsets in small steps so the
        // straddling bar is a shaded one somewhere in the range.
        let mut checked = 0;
        for step in 0..240 {
            let scroll = 4_000.0 + step as f32 * 7.0;
            let shades = shades(scroll);
            assert!(!shades.is_empty(), "a scrolled viewport still shades bars");
            for shade in &shades {
                assert!(
                    shade.x >= 0.0,
                    "scroll {scroll}: shade at x={} paints over the track headers",
                    shade.x
                );
                assert!(shade.width > 0.0, "a clamped shade keeps positive width");
                checked += 1;
            }
        }
        assert!(checked > 0, "the sweep must actually examine shades");
    }

    /// Clamping must not silently drop the bar it clamps — the leading bar
    /// still has to be shaded, just from the viewport edge inward.
    #[test]
    fn clamping_trims_the_shade_rather_than_removing_it() {
        let mut widest_at_start = 0.0f32;
        let mut found_leading = false;
        for scroll in [4_001.0, 6_337.0, 9_137.0, 12_599.0] {
            for shade in shades(scroll) {
                if shade.x == 0.0 {
                    found_leading = true;
                    widest_at_start = widest_at_start.max(shade.width);
                }
            }
        }
        assert!(
            found_leading,
            "some scroll position must put a shaded bar under the left edge"
        );
        assert!(
            widest_at_start > 0.0,
            "the clamped leading bar keeps a visible width"
        );
    }

    /// Shades stay inside the viewport on the right too, so the clamp did not
    /// trade a left-side leak for a right-side one.
    #[test]
    fn shades_stay_within_the_visible_width() {
        let width = scrolled_state(9_137.0).viewport.viewport_width;
        for shade in shades(9_137.0) {
            assert!(
                shade.x < width + 1.0,
                "shade at x={} starts past the lane width {width}",
                shade.x
            );
        }
    }
}
