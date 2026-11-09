#![allow(dead_code)]

use crate::loop_mode::LoopMode;
use crate::synthesizer_settings::SynthesizerSettings;

// In this class, fixed-point numbers are used for speed-up.
// A fixed-point number is expressed by Int64, whose lower 24 bits represent the fraction part,
// and the rest represent the integer part.
// For clarity, fixed-point number variables have a suffix "_fp".

#[derive(Debug)]
#[non_exhaustive]
pub(crate) struct Oscillator {
    synthesizer_sample_rate: i32,

    loop_mode: LoopMode,
    sample_sample_rate: i32,
    start: i32,
    end: i32,
    start_loop: i32,
    end_loop: i32,
    root_key: i32,

    tune: f32,
    pitch_change_scale: f32,
    sample_rate_ratio: f32,

    looping: bool,

    position_fp: i64,
}

impl Oscillator {
    const FRAC_BITS: i32 = 24;
    const FRAC_UNIT: i64 = 1_i64 << Oscillator::FRAC_BITS;
    const FP_TO_SAMPLE: f32 = 1_f32 / (32768 * Oscillator::FRAC_UNIT) as f32;

    pub(crate) fn new(settings: &SynthesizerSettings) -> Self {
        Self {
            synthesizer_sample_rate: settings.sample_rate,
            loop_mode: LoopMode::NoLoop,
            sample_sample_rate: 0,
            start: 0,
            end: 0,
            start_loop: 0,
            end_loop: 0,
            root_key: 0,
            tune: 0_f32,
            pitch_change_scale: 0_f32,
            sample_rate_ratio: 0_f32,
            looping: false,
            position_fp: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        &mut self,
        loop_mode: LoopMode,
        sample_rate: i32,
        start: i32,
        end: i32,
        start_loop: i32,
        end_loop: i32,
        root_key: i32,
        coarse_tune: i32,
        fine_tune: i32,
        scale_tuning: i32,
    ) {
        self.loop_mode = loop_mode;
        self.sample_sample_rate = sample_rate;
        self.start = start;
        self.end = end;
        self.start_loop = start_loop;
        self.end_loop = end_loop;
        self.root_key = root_key;

        self.tune = coarse_tune as f32 + 0.01_f32 * fine_tune as f32;
        self.pitch_change_scale = 0.01_f32 * scale_tuning as f32;
        self.sample_rate_ratio = sample_rate as f32 / self.synthesizer_sample_rate as f32;
        self.looping = self.loop_mode != LoopMode::NoLoop;
        self.position_fp = (start as i64) << Oscillator::FRAC_BITS;
    }

    pub(crate) fn release(&mut self) {
        if self.loop_mode == LoopMode::LoopUntilNoteOff {
            self.looping = false;
        }
    }

    pub(crate) fn process(&mut self, data: &[i16], block: &mut [f32], pitch: f32) -> bool {
        let pitch_change = self.pitch_change_scale * (pitch - self.root_key as f32) + self.tune;
        let mut pitch_ratio = self.sample_rate_ratio * 2_f32.powf(pitch_change / 12_f32);
        if !pitch_ratio.is_finite() {
            pitch_ratio = 0.0;
        }
        self.fill_block(data, block, pitch_ratio as f64)
    }

    fn fill_block(&mut self, data: &[i16], block: &mut [f32], pitch_ratio: f64) -> bool {
        let pitch_ratio_fp = (Oscillator::FRAC_UNIT as f64 * pitch_ratio) as i64;

        if self.looping {
            self.fill_block_continuous(data, block, pitch_ratio_fp)
        } else {
            self.fill_block_no_loop(data, block, pitch_ratio_fp)
        }
    }

    fn fill_block_no_loop(&mut self, data: &[i16], block: &mut [f32], pitch_ratio_fp: i64) -> bool {
        for t in 0..block.len() {
            // A negative rate, or a step that wrapped, puts the cursor before
            // the sample. Casting that to usize would index off the end.
            let index_i = self.position_fp >> Oscillator::FRAC_BITS;
            if index_i < 0 || index_i >= self.end as i64 || index_i as usize + 1 >= data.len() {
                if t > 0 {
                    let len = block.len();
                    block[t..len].fill(0_f32);
                    return true;
                } else {
                    return false;
                }
            }

            let index = index_i as usize;
            let x1 = data[index] as i64;
            let x2 = data[index + 1] as i64;
            let a_fp = self.position_fp & (Oscillator::FRAC_UNIT - 1);
            block[t] = Oscillator::FP_TO_SAMPLE
                * ((x1 << Oscillator::FRAC_BITS) + a_fp * (x2 - x1)) as f32;

            // Saturating: a huge rate must not overflow the cursor (a debug
            // panic) or wrap it into a plausible index.
            self.position_fp = self.position_fp.saturating_add(pitch_ratio_fp);
        }

        true
    }

    fn fill_block_continuous(
        &mut self,
        data: &[i16],
        block: &mut [f32],
        pitch_ratio_fp: i64,
    ) -> bool {
        let loop_length = self.end_loop as i64 - self.start_loop as i64;
        let Some(loop_length_fp) = loop_length.checked_shl(Oscillator::FRAC_BITS as u32) else {
            return false;
        };
        if loop_length_fp <= 0 {
            return false;
        }
        let start_fp = (self.start_loop as i64) << Oscillator::FRAC_BITS;
        let end_loop_fp = (self.end_loop as i64) << Oscillator::FRAC_BITS;
        let end_loop = self.end_loop.max(0) as usize;
        let loop_length_us = loop_length as usize;

        for sample in block.iter_mut() {
            // The attack before the loop point plays once. Only a cursor that
            // has passed the loop end — by more than one loop, when the step
            // is longer than the loop — or run back before the sample start
            // is folded into the loop. A single subtraction used to leave that
            // cursor past the sample, and the next read panicked.
            let index_i = self.position_fp >> Oscillator::FRAC_BITS;
            let before_sample = index_i < 0 || index_i < self.start as i64;
            if self.position_fp >= end_loop_fp || before_sample {
                let rel = self.position_fp.wrapping_sub(start_fp);
                self.position_fp =
                    if rel >= loop_length_fp && rel < loop_length_fp.saturating_mul(2) {
                        start_fp + (rel - loop_length_fp)
                    } else {
                        start_fp + rel.rem_euclid(loop_length_fp)
                    };
            }

            let index_i = self.position_fp >> Oscillator::FRAC_BITS;
            if index_i < 0 || index_i as usize >= data.len() {
                *sample = 0.0;
                self.position_fp = self.position_fp.wrapping_add(pitch_ratio_fp);
                continue;
            }
            let index1 = index_i as usize;
            let mut index2 = index1 + 1;
            if index2 >= end_loop {
                index2 = index2.saturating_sub(loop_length_us);
            }
            if index2 >= data.len() {
                *sample = 0.0;
                self.position_fp = self.position_fp.wrapping_add(pitch_ratio_fp);
                continue;
            }

            let x1 = data[index1] as i64;
            let x2 = data[index2] as i64;
            let a_fp = self.position_fp & (Oscillator::FRAC_UNIT - 1);
            *sample = Oscillator::FP_TO_SAMPLE
                * ((x1 << Oscillator::FRAC_BITS) + a_fp * (x2 - x1)) as f32;

            self.position_fp = self.position_fp.wrapping_add(pitch_ratio_fp);
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::Oscillator;
    use crate::loop_mode::LoopMode;
    use crate::synthesizer_settings::SynthesizerSettings;

    fn render(mode: LoopMode, sample_rate: i32, pitch: f32) -> Vec<f32> {
        let settings = SynthesizerSettings::new(44_100);
        let mut osc = Oscillator::new(&settings);
        // Loop is four frames long, inside a 32-frame sample.
        osc.start(mode, sample_rate, 0, 32, 8, 12, 60, 0, 0, 100);
        let data = vec![1_000i16; 32];
        let mut block = vec![0.0f32; 128];
        let _ = osc.process(&data, &mut block, pitch);
        block
    }

    #[test]
    fn a_step_longer_than_the_loop_stays_inside_the_sample() {
        for &(rate, pitch) in &[(8_000_000, 60.0), (44_100, 127.0), (192_000, 120.0)] {
            let block = render(LoopMode::Continuous, rate, pitch);
            assert!(
                block.iter().all(|sample| sample.is_finite()),
                "rate {rate} pitch {pitch} left the sample"
            );
        }
    }

    #[test]
    fn a_negative_position_does_not_read_past_the_sample() {
        let looping = render(LoopMode::Continuous, -48_000, 60.0);
        let once = render(LoopMode::NoLoop, -48_000, 60.0);
        assert!(looping.iter().all(|sample| sample.is_finite()));
        assert!(once.iter().all(|sample| sample.is_finite()));
    }
}
