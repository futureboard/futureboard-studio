//! Shared beat/pixel viewport math for drawing and hit-testing.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioEditorViewport {
    /// Horizontal content offset in logical pixels.
    pub scroll_x: f32,
    /// Logical pixels per timeline beat.
    pub pixels_per_beat: f32,
    /// Amplitude scale applied by the renderer.
    pub vertical_zoom: f32,
}

impl Default for AudioEditorViewport {
    fn default() -> Self {
        Self {
            scroll_x: 0.0,
            pixels_per_beat: 48.0,
            vertical_zoom: 1.0,
        }
    }
}

impl AudioEditorViewport {
    pub const MIN_PIXELS_PER_BEAT: f32 = 8.0;
    pub const MAX_PIXELS_PER_BEAT: f32 = 512.0;

    pub fn beat_to_x(self, beat: f32) -> f32 {
        beat.max(0.0) * self.pixels_per_beat.max(Self::MIN_PIXELS_PER_BEAT) - self.scroll_x.max(0.0)
    }

    pub fn x_to_beat(self, x: f32) -> f32 {
        ((x.max(0.0) + self.scroll_x.max(0.0))
            / self.pixels_per_beat.max(Self::MIN_PIXELS_PER_BEAT))
        .max(0.0)
    }

    /// Zoom around the content point beneath `anchor_x`, preserving that beat.
    pub fn zoom_around_x(&mut self, factor: f32, anchor_x: f32) {
        let old_ppb = self.pixels_per_beat.max(Self::MIN_PIXELS_PER_BEAT);
        let beat = (anchor_x.max(0.0) + self.scroll_x.max(0.0)) / old_ppb;
        self.pixels_per_beat = (old_ppb * factor.max(0.001))
            .clamp(Self::MIN_PIXELS_PER_BEAT, Self::MAX_PIXELS_PER_BEAT);
        self.scroll_x = (beat * self.pixels_per_beat - anchor_x).max(0.0);
    }

    pub fn fit_clip(&mut self, duration_beats: f32, viewport_width: f32) {
        self.scroll_x = 0.0;
        if duration_beats > 0.0 && viewport_width > 32.0 {
            self.pixels_per_beat =
                (viewport_width * 0.92 / duration_beats).clamp(Self::MIN_PIXELS_PER_BEAT, 256.0);
        }
    }

    pub fn clamp_scroll(&mut self, duration_beats: f32, viewport_width: f32) {
        let content_width = duration_beats.max(0.0) * self.pixels_per_beat.max(1.0);
        self.scroll_x = self
            .scroll_x
            .max(0.0)
            .min((content_width - viewport_width.max(0.0)).max(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_pixel_round_trip() {
        let viewport = AudioEditorViewport {
            scroll_x: 120.0,
            pixels_per_beat: 64.0,
            vertical_zoom: 1.0,
        };
        let beat = 5.5;
        let x = viewport.beat_to_x(beat);
        assert!((viewport.x_to_beat(x) - beat).abs() < 1e-5);
    }

    #[test]
    fn zoom_preserves_anchor() {
        let mut viewport = AudioEditorViewport {
            scroll_x: 100.0,
            ..Default::default()
        };
        let anchor = 240.0;
        let beat_before = viewport.x_to_beat(anchor);
        viewport.zoom_around_x(2.0, anchor);
        assert!((viewport.x_to_beat(anchor) - beat_before).abs() < 1e-5);
    }
}
