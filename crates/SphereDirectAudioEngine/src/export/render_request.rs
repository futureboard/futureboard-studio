//! Plain-data request describing what to render offline.

use crate::runtime::build_project_tempo_map;
use crate::types::EngineProjectSnapshot;

/// What to do after the last timeline content ends, so reverb/instrument tails
/// are not hard-cut.
#[derive(Debug, Clone, Copy, Default)]
pub enum ExportTailMode {
    #[default]
    None,
    FixedSeconds(f64),
    /// Keep rendering until the block peak drops below `threshold_db`, up to
    /// `max_seconds`.
    UntilSilence {
        max_seconds: f64,
        threshold_db: f32,
    },
}

/// Post-render gain stage.
#[derive(Debug, Clone, Copy, Default)]
pub enum ExportNormalizeMode {
    #[default]
    None,
    /// Two-pass: analyze peak, then apply gain so the peak hits `db` dBFS.
    PeakDb(f32),
}

/// Geometry for an offline render. Sample positions are absolute project
/// samples at `sample_rate`. `end_sample` is exclusive.
#[derive(Debug, Clone)]
pub struct OfflineRenderRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub start_sample: u64,
    pub end_sample: u64,
    /// Linear master gain (mirrors the engine's master volume atomic).
    pub master_volume: f32,
    pub block_size: usize,
    pub tail: ExportTailMode,
    pub normalize: ExportNormalizeMode,
}

impl OfflineRenderRequest {
    /// Number of content frames (excluding tail) this request will render.
    pub fn content_frames(&self) -> u64 {
        self.end_sample.saturating_sub(self.start_sample)
    }

    /// Max tail frames implied by the tail mode (UntilSilence reports its cap).
    pub fn max_tail_frames(&self) -> u64 {
        let secs = match self.tail {
            ExportTailMode::None => 0.0,
            ExportTailMode::FixedSeconds(s) => s.max(0.0),
            ExportTailMode::UntilSilence { max_seconds, .. } => max_seconds.max(0.0),
        };
        (secs * self.sample_rate as f64).round() as u64
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.sample_rate == 0 {
            return Err("sample_rate must be non-zero".to_string());
        }
        if self.channels == 0 {
            return Err("channels must be non-zero".to_string());
        }
        if self.end_sample <= self.start_sample {
            return Err("range end must be greater than range start".to_string());
        }
        if self.block_size == 0 {
            return Err("block_size must be non-zero".to_string());
        }
        Ok(())
    }
}

/// Compute the natural `[start, end)` sample bounds of the entire arrangement
/// from a snapshot: sample 0 to the latest content end across audio + MIDI
/// clips, converted through the project tempo map. Returns `(0, 0)` for an
/// empty arrangement.
pub fn arrangement_bounds_samples(
    snapshot: &EngineProjectSnapshot,
    sample_rate: u32,
) -> (u64, u64) {
    let sr = sample_rate.max(1) as f64;
    let mut end_beat = 0.0f64;
    for clip in &snapshot.clips {
        end_beat = end_beat.max(clip.start_beat + clip.duration_beats.max(0.0));
    }
    for clip in &snapshot.midi_clips {
        end_beat = end_beat.max(clip.start_beat + clip.length_beats.max(0.0));
    }
    if end_beat <= 0.0 {
        return (0, 0);
    }
    let tempo_map = build_project_tempo_map(snapshot);
    let end_sample = tempo_map.samples_at_beat(end_beat, sr);
    (0, end_sample)
}

/// Convert a project beat position to an absolute sample position at
/// `sample_rate`, honoring the snapshot's tempo map (not just a static BPM).
pub fn beats_to_samples(snapshot: &EngineProjectSnapshot, beat: f64, sample_rate: u32) -> u64 {
    let sr = sample_rate.max(1) as f64;
    let tempo_map = build_project_tempo_map(snapshot);
    tempo_map.samples_at_beat(beat.max(0.0), sr)
}

/// Scale the live device block duration to the output rate so block-driven
/// automation is evaluated at the same time intervals during offline export.
pub fn render_block_size_for_sample_rate(
    snapshot: &EngineProjectSnapshot,
    sample_rate: u32,
) -> usize {
    let source_rate = snapshot.sample_rate.max(1) as u64;
    let output_rate = sample_rate.max(1) as u64;
    let live_block = snapshot.routing.buffer_size.max(1) as u64;
    let output_block = live_block
        .saturating_mul(output_rate)
        .saturating_add(source_rate / 2)
        / source_rate;
    output_block
        .max(1)
        .min(crate::plugin_bridge::MAX_BRIDGE_BLOCK_FRAMES as u64) as usize
}
