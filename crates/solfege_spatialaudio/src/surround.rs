//! One channel rendered onto a speaker layout.

use crate::bus::SpatialBus;
use crate::dsp::Biquad;
use crate::layout::{MAX_CHANNELS, SpeakerLayout};
use crate::panner::SurroundPanner;
use crate::position::SourceParams;

/// The LFE's band limit.
const LFE_CUTOFF_HZ: f32 = 120.0;

/// Renders a channel's stereo signal onto a speaker layout.
///
/// A stereo channel is placed as two sources, its left and right sides turned
/// either way of the position by the channel's width, so a stereo pad keeps
/// its image wherever it is put; at zero width the sides fold to one point.
/// Gains ramp across each block from the last block's to this block's, so a
/// source being dragged or automated never steps.
#[derive(Debug, Clone)]
pub struct SurroundSource {
    layout: SpeakerLayout,
    panner: SurroundPanner,
    lfe_channel: Option<usize>,
    lfe_filter: Biquad,
    /// Gains applied at the end of the last block, per side.
    last_l: [f32; MAX_CHANNELS],
    last_r: [f32; MAX_CHANNELS],
    last_lfe: f32,
    primed: bool,
    /// Mono scratch for the LFE send.
    lfe_scratch: Vec<f32>,
}

impl SurroundSource {
    pub fn new(layout: SpeakerLayout, sample_rate: u32, max_block: usize) -> Self {
        Self {
            layout,
            panner: SurroundPanner::new(layout),
            lfe_channel: layout.lfe_channel(),
            lfe_filter: Biquad::lowpass(LFE_CUTOFF_HZ, sample_rate.max(1) as f32),
            last_l: [0.0; MAX_CHANNELS],
            last_r: [0.0; MAX_CHANNELS],
            last_lfe: 0.0,
            primed: false,
            lfe_scratch: vec![0.0; max_block.max(1)],
        }
    }

    pub fn layout(&self) -> SpeakerLayout {
        self.layout
    }

    pub fn reset(&mut self) {
        self.lfe_filter.reset();
        self.primed = false;
    }

    /// The target gains for `params`: one set per side of the source.
    fn targets(&self, params: &SourceParams) -> ([f32; MAX_CHANNELS], [f32; MAX_CHANNELS]) {
        let half = params.half_width_radians();
        let mut left = [0.0f32; MAX_CHANNELS];
        let mut right = [0.0f32; MAX_CHANNELS];
        if half <= 1.0e-4 {
            self.panner.gains(params, &mut left);
            right = left;
        } else {
            let mut side = *params;
            side.position = params.position.rotated(-half);
            self.panner.gains(&side, &mut left);
            side.position = params.position.rotated(half);
            self.panner.gains(&side, &mut right);
        }
        (left, right)
    }

    /// Add one block of `input_l`/`input_r` at `params` into `bus`.
    pub fn process(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        params: &SourceParams,
        bus: &mut SpatialBus,
    ) {
        let frames = input_l
            .len()
            .min(input_r.len())
            .min(bus.capacity())
            .min(self.lfe_scratch.len());
        if frames == 0 {
            return;
        }
        let params = params.sanitized();
        let (target_l, target_r) = self.targets(&params);
        let target_lfe = params.lfe;
        if !self.primed {
            self.last_l = target_l;
            self.last_r = target_r;
            self.last_lfe = target_lfe;
            self.primed = true;
        }
        let channels = self.layout.channel_count().min(bus.channels());
        let inv = 1.0 / frames as f32;
        for channel in 0..channels {
            if Some(channel) == self.lfe_channel {
                continue;
            }
            let (from_l, to_l) = (self.last_l[channel], target_l[channel]);
            let (from_r, to_r) = (self.last_r[channel], target_r[channel]);
            if from_l.max(to_l).max(from_r).max(to_r) <= 1.0e-7 {
                continue;
            }
            let step_l = (to_l - from_l) * inv;
            let step_r = (to_r - from_r) * inv;
            let out = bus.channel_mut(channel, frames);
            for n in 0..frames {
                let g_l = from_l + step_l * n as f32;
                let g_r = from_r + step_r * n as f32;
                out[n] += input_l[n] * g_l + input_r[n] * g_r;
            }
        }
        if let Some(lfe) = self.lfe_channel.filter(|&c| c < channels) {
            let (from, to) = (self.last_lfe, target_lfe);
            if from.max(to) > 1.0e-7 {
                let step = (to - from) * inv;
                for n in 0..frames {
                    let g = from + step * n as f32;
                    let mono = 0.5 * (input_l[n] + input_r[n]);
                    self.lfe_scratch[n] = self.lfe_filter.tick(mono * g);
                }
                bus.add(lfe, &self.lfe_scratch[..frames], 1.0);
            } else {
                // Keep the filter's state moving toward silence.
                for _ in 0..frames {
                    self.lfe_filter.tick(0.0);
                }
            }
        }
        self.last_l = target_l;
        self.last_r = target_r;
        self.last_lfe = target_lfe;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::RoomPosition;

    fn render(layout: SpeakerLayout, params: SourceParams, l: f32, r: f32) -> Vec<f32> {
        let frames = 256;
        let mut source = SurroundSource::new(layout, 48_000, frames);
        let mut bus = SpatialBus::new(layout.channel_count(), frames);
        let in_l = vec![l; frames];
        let in_r = vec![r; frames];
        // Two blocks: the second is at steady gains.
        source.process(&in_l, &in_r, &params, &mut bus);
        bus.clear(frames);
        source.process(&in_l, &in_r, &params, &mut bus);
        (0..layout.channel_count())
            .map(|c| bus.channel(c, frames)[frames - 1])
            .collect()
    }

    #[test]
    fn a_stereo_source_on_the_front_wall_is_the_front_pair() {
        // Width 1 puts the sides at ±30°: L and R speakers exactly.
        let params = SourceParams {
            position: RoomPosition::FRONT,
            width: 1.0,
            ..SourceParams::default()
        };
        let out = render(SpeakerLayout::Surround51, params, 1.0, 0.5);
        assert!((out[0] - 1.0).abs() < 1.0e-3, "{out:?}");
        assert!((out[1] - 0.5).abs() < 1.0e-3, "{out:?}");
        assert!(out[2].abs() < 1.0e-3 && out[4].abs() < 1.0e-3 && out[5].abs() < 1.0e-3);
    }

    #[test]
    fn the_lfe_send_reaches_only_the_lfe() {
        let params = SourceParams {
            lfe: 1.0,
            ..SourceParams::default()
        };
        let frames = 4_800;
        let mut source = SurroundSource::new(SpeakerLayout::Surround51, 48_000, frames);
        let mut bus = SpatialBus::new(6, frames);
        let dc = vec![1.0; frames];
        source.process(&dc, &dc, &params, &mut bus);
        // DC passes the 120 Hz low-pass once it has settled.
        assert!((bus.channel(3, frames)[frames - 1] - 1.0).abs() < 1.0e-2);
    }

    #[test]
    fn moving_a_source_ramps_instead_of_stepping() {
        let frames = 512;
        let mut source = SurroundSource::new(SpeakerLayout::Quad, 48_000, frames);
        let mut bus = SpatialBus::new(4, frames);
        let ones = vec![1.0; frames];
        let left = SourceParams {
            position: RoomPosition::new(-1.0, 1.0, 0.0),
            width: 0.0,
            ..SourceParams::default()
        };
        let right = SourceParams {
            position: RoomPosition::new(1.0, 1.0, 0.0),
            ..left
        };
        source.process(&ones, &ones, &left, &mut bus);
        bus.clear(frames);
        source.process(&ones, &ones, &right, &mut bus);
        let front_left = bus.channel(0, frames);
        let biggest_step = front_left
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(biggest_step < 0.01, "a jump of {biggest_step}");
        assert!(front_left[0] > 1.9 && front_left[frames - 1] < 0.01);
    }
}
