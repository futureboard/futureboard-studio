//! The Voxtune editor's one coordinate transform.
//!
//! Ruler, grid, waveform, notes, pitch curves, playhead and hit-testing all
//! go through [`VoxViewport`], so what is drawn is exactly what a click
//! lands on. Time is in seconds of the edited take; pitch is fractional MIDI.

/// Lowest and highest pitch the view may scroll to.
pub const PITCH_FLOOR: f32 = 24.0;
pub const PITCH_CEIL: f32 = 108.0;
/// Row height limits, px per semitone.
pub const ROW_MIN: f32 = 6.0;
pub const ROW_MAX: f32 = 48.0;
/// Horizontal zoom limits, px per second.
pub const PPS_MIN: f32 = 4.0;
pub const PPS_MAX: f32 = 2_400.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoxViewport {
    /// Seconds at the left edge of the note area.
    pub t0: f32,
    /// Pixels per second.
    pub pps: f32,
    /// Pitch (fractional MIDI) at the top edge of the note area.
    pub top_pitch: f32,
    /// Pixels per semitone.
    pub row_h: f32,
}

impl Default for VoxViewport {
    fn default() -> Self {
        Self {
            t0: 0.0,
            pps: 120.0,
            top_pitch: 84.0,
            row_h: 16.0,
        }
    }
}

impl VoxViewport {
    #[inline]
    pub fn time_to_x(&self, sec: f32) -> f32 {
        (sec - self.t0) * self.pps
    }

    #[inline]
    pub fn x_to_time(&self, x: f32) -> f32 {
        self.t0 + x / self.pps.max(1e-3)
    }

    /// Y of the centre line of `pitch` (a whole semitone sits mid-row).
    #[inline]
    pub fn pitch_to_y(&self, pitch: f32) -> f32 {
        (self.top_pitch - pitch) * self.row_h
    }

    #[inline]
    pub fn y_to_pitch(&self, y: f32) -> f32 {
        self.top_pitch - y / self.row_h.max(1e-3)
    }

    /// Top edge of the row of semitone `pitch`.
    #[inline]
    pub fn row_top(&self, pitch: i32) -> f32 {
        self.pitch_to_y(pitch as f32 + 0.5)
    }

    /// Whole semitones with at least part of their row inside `height`.
    pub fn visible_pitches(&self, height: f32) -> (i32, i32) {
        let hi = (self.top_pitch + 0.5).ceil() as i32;
        let lo = (self.y_to_pitch(height) - 0.5).floor() as i32;
        (lo.max(PITCH_FLOOR as i32), hi.min(PITCH_CEIL as i32))
    }

    /// Visible span `[t0, t1]` for a note area `width` wide.
    pub fn visible_time(&self, width: f32) -> (f32, f32) {
        (self.t0, self.x_to_time(width))
    }

    /// Keeps the view inside the take and the pitch range.
    pub fn clamp(&mut self, duration: f32, width: f32, height: f32) {
        self.pps = self.pps.clamp(PPS_MIN, PPS_MAX);
        self.row_h = self.row_h.clamp(ROW_MIN, ROW_MAX);
        let span = width / self.pps;
        let max_t0 = (duration - span * 0.5).max(0.0);
        self.t0 = self.t0.clamp(-span * 0.02, max_t0.max(0.0));
        let rows = height / self.row_h;
        let max_top = PITCH_CEIL;
        let min_top = (PITCH_FLOOR + rows).min(max_top);
        self.top_pitch = self.top_pitch.clamp(min_top, max_top);
    }

    /// Zooms time by `factor` keeping the second under `anchor_x` in place.
    pub fn zoom_time(&mut self, factor: f32, anchor_x: f32) {
        let at = self.x_to_time(anchor_x);
        self.pps = (self.pps * factor).clamp(PPS_MIN, PPS_MAX);
        self.t0 = at - anchor_x / self.pps;
    }

    /// Zooms pitch by `factor` keeping the pitch under `anchor_y` in place.
    pub fn zoom_pitch(&mut self, factor: f32, anchor_y: f32) {
        let at = self.y_to_pitch(anchor_y);
        self.row_h = (self.row_h * factor).clamp(ROW_MIN, ROW_MAX);
        self.top_pitch = at + anchor_y / self.row_h;
    }

    /// Frames `[t_start, t_end]` × `[p_lo, p_hi]` in a `width` × `height`
    /// area with a little margin.
    pub fn fit(&mut self, t_start: f32, t_end: f32, p_lo: f32, p_hi: f32, width: f32, height: f32) {
        let span = (t_end - t_start).max(0.5);
        self.pps = (width / (span * 1.04)).clamp(PPS_MIN, PPS_MAX);
        self.t0 = t_start - span * 0.02;
        let rows = (p_hi - p_lo + 6.0).max(12.0);
        self.row_h = (height / rows).clamp(ROW_MIN, ROW_MAX);
        let mid = (p_lo + p_hi) * 0.5;
        self.top_pitch = mid + height / self.row_h * 0.5;
    }
}

/// Musical grid for the ruler: bar lines lead, beats support.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridLine {
    pub x: f32,
    pub sec: f32,
    /// 1-based bar number when this is a bar line.
    pub bar: Option<u32>,
}

/// Bar and beat lines inside `width` at `bpm` (4/4), thinned so lines stay
/// at least ~8 px apart and bars at least ~40 px for labels.
pub fn grid_lines(view: &VoxViewport, bpm: f32, width: f32) -> Vec<GridLine> {
    let beat = 60.0 / bpm.max(1.0);
    let beat_px = beat * view.pps;
    let bar_px = beat_px * 4.0;
    // Bar stride: 1, 2, 4, 8 … bars so labels don't collide.
    let mut bar_stride = 1u32;
    while (bar_px * bar_stride as f32) < 40.0 && bar_stride < 1024 {
        bar_stride *= 2;
    }
    let show_beats = beat_px >= 8.0;
    let (t0, t1) = view.visible_time(width);
    let first = (t0.max(0.0) / beat).floor() as i64;
    let last = (t1 / beat).ceil() as i64;
    let mut out = Vec::new();
    for b in first..=last {
        if b < 0 {
            continue;
        }
        let is_bar = b % 4 == 0;
        let bar_no = (b / 4) as u32;
        if is_bar && bar_no % bar_stride != 0 {
            if !show_beats {
                continue;
            }
        } else if !is_bar && !show_beats {
            continue;
        }
        let sec = b as f32 * beat;
        out.push(GridLine {
            x: view.time_to_x(sec),
            sec,
            bar: (is_bar && bar_no % bar_stride == 0).then_some(bar_no + 1),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_and_pitch_round_trip() {
        let v = VoxViewport {
            t0: 2.0,
            pps: 100.0,
            top_pitch: 72.0,
            row_h: 10.0,
        };
        assert_eq!(v.time_to_x(3.5), 150.0);
        assert!((v.x_to_time(150.0) - 3.5).abs() < 1e-5);
        assert_eq!(v.pitch_to_y(70.0), 20.0);
        assert!((v.y_to_pitch(20.0) - 70.0).abs() < 1e-5);
        // A semitone's row is centred on its pitch line.
        assert_eq!(v.row_top(70), 15.0);
    }

    #[test]
    fn zoom_keeps_the_anchor_still() {
        let mut v = VoxViewport::default();
        let at = v.x_to_time(300.0);
        v.zoom_time(2.0, 300.0);
        assert!((v.x_to_time(300.0) - at).abs() < 1e-4);
        let p = v.y_to_pitch(120.0);
        v.zoom_pitch(1.5, 120.0);
        assert!((v.y_to_pitch(120.0) - p).abs() < 1e-4);
    }

    #[test]
    fn fit_frames_the_notes() {
        let mut v = VoxViewport::default();
        v.fit(1.0, 9.0, 60.0, 72.0, 800.0, 400.0);
        assert!(v.time_to_x(1.0) > 0.0 && v.time_to_x(9.0) < 800.0);
        assert!(v.pitch_to_y(72.0) > 0.0 && v.pitch_to_y(60.0) < 400.0);
    }

    #[test]
    fn grid_thins_as_it_zooms_out() {
        let mut v = VoxViewport::default();
        v.pps = 200.0;
        let dense = grid_lines(&v, 120.0, 800.0);
        assert!(
            dense.iter().any(|l| l.bar.is_none()),
            "beats when zoomed in"
        );
        v.pps = 5.0;
        let sparse = grid_lines(&v, 120.0, 800.0);
        assert!(sparse.iter().all(|l| l.bar.is_some()));
        let bars: Vec<u32> = sparse.iter().filter_map(|l| l.bar).collect();
        assert!(bars.windows(2).all(|w| w[1] - w[0] >= 2));
    }
}
