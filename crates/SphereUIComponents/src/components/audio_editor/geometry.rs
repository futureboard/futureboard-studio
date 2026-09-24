//! The Audio Editor's one coordinate model.
//!
//! The editor's horizontal axis is the clip's own timeline position, in beats
//! from the clip's start — the same musical axis the arrangement uses, so the
//! ruler, grid, playhead and snapping line up with everything else. What the
//! editor *edits*, though, is the source file. [`ClipMap`] is the only bridge
//! between the two: every waveform column, selection, hit-test and edit range
//! goes through it, in both directions, so what is drawn under the pointer is
//! exactly the audio an edit touches and exactly what playback plays there.
//!
//! The mapping follows the clip's playback:
//!
//! * **Tempo Sync / Warp** clips are defined in beats, so time is linear in
//!   beats;
//! * other clips play at a fixed rate in *seconds*, so a tempo change inside
//!   the clip bends the beat → source mapping through the tempo map;
//! * **Warp markers** pin source frames to timeline beats piecewise;
//! * **Reverse** runs the source window backwards.
//!
//! The forward map is sampled once into a monotonic, piecewise-linear table
//! — refined wherever the mapping bends (a tempo change, a warp marker) until
//! it is within a fraction of a frame — and the inverse searches the same
//! table, so the two can never disagree.

use crate::components::timeline::timeline_state::{
    warp_timeline_beat_to_source_sample, ClipState, StretchMode,
};

/// Uniform segments the forward table starts from. Linear clips (the common
/// case) are exact at any count; bends are refined below.
const TABLE_SEGMENTS: usize = 512;
/// Largest error, in source frames, a refined table segment may have.
const TABLE_TOLERANCE: f64 = 0.25;
/// Halvings allowed when refining one segment.
const MAX_REFINE_DEPTH: u32 = 24;

#[derive(Debug, Clone, PartialEq)]
pub struct ClipMap {
    /// Absolute timeline beat the clip starts on.
    pub clip_start: f64,
    /// Clip length in beats.
    pub duration: f64,
    /// Source window `[start, end)` the clip plays, in source frames.
    pub window: (u64, u64),
    pub reverse: bool,
    /// `(clip-relative beat, source frame)` knots, increasing in beat and
    /// monotonic in frame; linear between knots.
    knots: Vec<(f64, f64)>,
}

impl ClipMap {
    /// Build the map for `clip`, whose source has `total_frames` frames.
    ///
    /// `seconds_at` is the project tempo map (absolute beat → seconds) and
    /// `project_bpm` the base tempo; both come from the timeline state so the
    /// editor bends time exactly where the arrangement does.
    pub fn build(
        clip: &ClipState,
        total_frames: u64,
        project_bpm: f64,
        seconds_at: impl Fn(f64) -> f64,
    ) -> Option<Self> {
        let stretch = &clip.stretch;
        let (s0, s1) = stretch.resolved_source_trim_range(total_frames);
        let duration = clip.duration_beats.max(0.0) as f64;
        if s1 <= s0 || duration <= 0.0 {
            return None;
        }
        let clip_start = clip.start_beat.max(0.0) as f64;
        let len = (s1 - s0) as f64;
        let markers = &stretch.warp_markers;
        let warped = stretch.mode == StretchMode::Warp && !markers.is_empty();

        // Fraction of the window played by `rel` beats into the clip.
        let played_seconds = stretch.played_seconds_for_project_bpm(project_bpm);
        let start_seconds = seconds_at(clip_start);
        let fraction = |rel: f64| -> f64 {
            if stretch.follows_project_tempo() {
                return rel / duration;
            }
            match played_seconds {
                Some(total) if total > 0.0 => {
                    (seconds_at(clip_start + rel) - start_seconds) / total
                }
                _ => rel / duration,
            }
        };

        let source = |rel: f64| -> f64 {
            if warped {
                return warp_timeline_beat_to_source_sample(
                    clip_start + rel,
                    s0,
                    s1,
                    stretch.effective_time_ratio(project_bpm),
                    markers,
                );
            }
            let advance = fraction(rel).clamp(0.0, 1.0) * len;
            if stretch.reverse {
                s1 as f64 - advance
            } else {
                s0 as f64 + advance
            }
        };

        let mut knots = Vec::with_capacity(TABLE_SEGMENTS + 1);
        knots.push((0.0, source(0.0)));
        for i in 1..=TABLE_SEGMENTS {
            let a = knots.last().copied().unwrap_or((0.0, source(0.0)));
            let rel = duration * i as f64 / TABLE_SEGMENTS as f64;
            refine(&source, a, (rel, source(rel)), 0, &mut knots);
        }

        Some(Self {
            clip_start,
            duration,
            window: (s0, s1),
            reverse: stretch.reverse && !warped,
            knots,
        })
    }

    /// Source frame (fractional) heard `rel` beats into the clip.
    pub fn source_at(&self, rel: f64) -> f64 {
        let rel = rel.clamp(0.0, self.duration);
        // First knot at or past `rel`.
        let i = self
            .knots
            .partition_point(|&(beat, _)| beat < rel)
            .clamp(1, self.knots.len() - 1);
        let (b0, f0) = self.knots[i - 1];
        let (b1, f1) = self.knots[i];
        let span = b1 - b0;
        if span <= 0.0 {
            return f1;
        }
        f0 + (f1 - f0) * ((rel - b0) / span)
    }

    /// Clip-relative beat at which source frame `frame` plays. Inverse of
    /// [`Self::source_at`]; frames outside the window clamp to its ends.
    pub fn beat_at(&self, frame: f64) -> f64 {
        let first = self.knots[0].1;
        let last = self.knots[self.knots.len() - 1].1;
        // Walk the frames as an increasing sequence whatever the direction.
        let sign = if last >= first { 1.0 } else { -1.0 };
        let target = frame * sign;
        if target <= first * sign {
            return 0.0;
        }
        if target >= last * sign {
            return self.duration;
        }
        let i = self
            .knots
            .partition_point(|&(_, f)| f * sign <= target)
            .clamp(1, self.knots.len() - 1);
        let (b0, f0) = self.knots[i - 1];
        let (b1, f1) = self.knots[i];
        let span = (f1 - f0) * sign;
        if span <= 0.0 {
            return b0;
        }
        b0 + (b1 - b0) * ((target - f0 * sign) / span)
    }

    /// Source frames covered by the clip-relative beat range `[a, b]`, as a
    /// sorted `[start, end)` pair clamped to the window.
    pub fn source_range(&self, a: f64, b: f64) -> (u64, u64) {
        let x = self.source_at(a.min(b));
        let y = self.source_at(a.max(b));
        let (lo, hi) = if x <= y { (x, y) } else { (y, x) };
        let lo = (lo.round().max(self.window.0 as f64) as u64).min(self.window.1);
        let hi = (hi.round().max(lo as f64) as u64).min(self.window.1);
        (lo, hi)
    }

    /// Source frames per beat averaged over the clip.
    pub fn mean_frames_per_beat(&self) -> f64 {
        ((self.window.1 - self.window.0) as f64 / self.duration.max(1.0e-9)).max(1.0e-9)
    }

    /// Source frames per beat around `rel`, for choosing a drawing level.
    pub fn frames_per_beat(&self, rel: f64) -> f64 {
        let step = self.duration / TABLE_SEGMENTS as f64;
        let a = self.source_at((rel - step).max(0.0));
        let b = self.source_at((rel + step).min(self.duration));
        ((b - a).abs() / (2.0 * step)).max(1.0e-9)
    }
}

/// Append knots covering `(a, b]`, halving the segment while its midpoint
/// strays from the straight line by more than [`TABLE_TOLERANCE`].
fn refine(
    source: &impl Fn(f64) -> f64,
    a: (f64, f64),
    b: (f64, f64),
    depth: u32,
    knots: &mut Vec<(f64, f64)>,
) {
    let mid = (a.0 + b.0) * 0.5;
    let at_mid = source(mid);
    if depth < MAX_REFINE_DEPTH && (at_mid - (a.1 + b.1) * 0.5).abs() > TABLE_TOLERANCE {
        refine(source, a, (mid, at_mid), depth + 1, knots);
        refine(source, (mid, at_mid), b, depth + 1, knots);
    } else {
        knots.push(b);
    }
}

/// Horizontal view: which clip-relative beats are on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Clip-relative beat at the left edge.
    pub scroll: f64,
    pub pixels_per_beat: f64,
    /// Visible width in pixels.
    pub width: f64,
}

impl Viewport {
    pub const MIN_PIXELS_PER_BEAT: f64 = 0.5;

    pub fn x_at(&self, rel: f64) -> f64 {
        (rel - self.scroll) * self.pixels_per_beat
    }

    pub fn beat_at(&self, x: f64) -> f64 {
        self.scroll + x / self.pixels_per_beat
    }

    pub fn visible_beats(&self) -> f64 {
        self.width / self.pixels_per_beat
    }

    /// Show the whole clip with a little air at each side.
    pub fn fit(&mut self, duration: f64) {
        let pad = 0.02;
        let beats = duration.max(1.0e-6) * (1.0 + 2.0 * pad);
        self.pixels_per_beat = (self.width / beats).max(Self::MIN_PIXELS_PER_BEAT);
        self.scroll = -duration * pad;
    }

    /// Zoom by `factor`, keeping the beat under `anchor_x` where it is. The
    /// deepest zoom shows `max_pixels_per_beat` (a few pixels per sample).
    pub fn zoom(&mut self, factor: f64, anchor_x: f64, max_pixels_per_beat: f64) {
        let anchor = self.beat_at(anchor_x);
        self.pixels_per_beat = (self.pixels_per_beat * factor)
            .clamp(Self::MIN_PIXELS_PER_BEAT, max_pixels_per_beat.max(1.0));
        self.scroll = anchor - anchor_x / self.pixels_per_beat;
    }

    /// Keep some of the clip on screen whatever the scroll.
    pub fn clamp(&mut self, duration: f64) {
        let visible = self.visible_beats();
        let slack = visible * 0.5;
        let min = -slack;
        let max = (duration - visible + slack).max(min);
        self.scroll = self.scroll.clamp(min, max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        AudioClipStretchState, AudioImportState, ClipType, WarpMarker,
    };

    fn clip(start: f32, beats: f32, window: (u64, u64)) -> ClipState {
        ClipState {
            id: "c".into(),
            name: "c".into(),
            start_beat: start,
            duration_beats: beats,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "a.wav".into(),
                source_path: Some("a.wav".into()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState {
                original_sample_rate: 48_000,
                source_start_samples: window.0,
                source_end_samples: window.1,
                ..AudioClipStretchState::default()
            },
        }
    }

    /// 120 BPM, no tempo changes.
    fn steady(beat: f64) -> f64 {
        beat * 0.5
    }

    #[test]
    fn a_plain_clip_maps_linearly_and_inverts() {
        // 4 beats at 120 BPM = 2 s = 96 000 frames.
        let c = clip(8.0, 4.0, (1_000, 97_000));
        let map = ClipMap::build(&c, 200_000, 120.0, steady).unwrap();
        assert!((map.source_at(0.0) - 1_000.0).abs() < 1e-6);
        assert!((map.source_at(2.0) - 49_000.0).abs() < 1e-6);
        assert!((map.source_at(4.0) - 97_000.0).abs() < 1e-6);
        assert!((map.beat_at(49_000.0) - 2.0).abs() < 1e-9);
        assert_eq!(map.source_range(1.0, 3.0), (25_000, 73_000));
    }

    #[test]
    fn a_reversed_clip_runs_the_window_backwards() {
        let mut c = clip(0.0, 4.0, (0, 96_000));
        c.stretch.reverse = true;
        let map = ClipMap::build(&c, 96_000, 120.0, steady).unwrap();
        assert!((map.source_at(0.0) - 96_000.0).abs() < 1e-6);
        assert!((map.source_at(1.0) - 72_000.0).abs() < 1e-6);
        assert!((map.beat_at(72_000.0) - 1.0).abs() < 1e-9);
        // Ranges come back sorted whatever the direction.
        assert_eq!(map.source_range(0.0, 1.0), (72_000, 96_000));
    }

    #[test]
    fn a_tempo_change_bends_a_seconds_clip() {
        // 120 BPM for the first 2 beats (1 s), then 60 BPM (1 s per beat).
        let tempo = |beat: f64| {
            if beat <= 2.0 {
                beat * 0.5
            } else {
                1.0 + (beat - 2.0)
            }
        };
        // 3 beats = 1 s + 1 s = 2 s = 96 000 frames.
        let c = clip(0.0, 3.0, (0, 96_000));
        let map = ClipMap::build(&c, 96_000, 120.0, tempo).unwrap();
        // Half way through in *time* is beat 2, not beat 1.5.
        assert!((map.source_at(2.0) - 48_000.0).abs() < 1.0);
        assert!((map.beat_at(48_000.0) - 2.0).abs() < 1e-3);
    }

    #[test]
    fn warp_markers_pin_frames_to_beats() {
        let mut c = clip(0.0, 4.0, (0, 96_000));
        c.stretch.mode = StretchMode::Warp;
        c.stretch.warp_markers = vec![
            WarpMarker {
                id: 1,
                source_sample: 0,
                timeline_beat: 0.0,
                locked: false,
            },
            WarpMarker {
                id: 2,
                source_sample: 24_000,
                timeline_beat: 2.0,
                locked: false,
            },
            WarpMarker {
                id: 3,
                source_sample: 96_000,
                timeline_beat: 4.0,
                locked: false,
            },
        ];
        let map = ClipMap::build(&c, 96_000, 120.0, steady).unwrap();
        assert!((map.source_at(2.0) - 24_000.0).abs() < 1.0);
        assert!((map.source_at(3.0) - 60_000.0).abs() < 1.0);
        assert!((map.beat_at(60_000.0) - 3.0).abs() < 1e-3);
    }

    #[test]
    fn zoom_keeps_the_anchor_beat_in_place() {
        let mut view = Viewport {
            scroll: 1.0,
            pixels_per_beat: 100.0,
            width: 800.0,
        };
        let before = view.beat_at(300.0);
        view.zoom(2.0, 300.0, 1.0e6);
        assert!((view.beat_at(300.0) - before).abs() < 1e-9);
        assert_eq!(view.pixels_per_beat, 200.0);
        view.fit(8.0);
        assert!(view.x_at(0.0) > 0.0 && view.x_at(8.0) < 800.0);
    }
}
