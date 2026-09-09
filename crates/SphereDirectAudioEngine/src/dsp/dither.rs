//! Output word-length reduction for integer device formats.
//!
//! The engine mixes in `f32`. When the device (an ASIO driver's Int32LSB /
//! Int16LSB buffers, WASAPI Exclusive integer formats) wants integers, the last
//! step used to be cpal's `from_sample`, which *truncates* toward zero: every
//! quiet passage and every fade tail then carried a bias and correlated
//! quantization distortion — the "dirty" floor on an otherwise clean mix.
//!
//! [`OutputDither`] replaces that with the textbook path: scale to the target
//! word length, add one TPDF dither sample of ±1 LSB, round to nearest, and
//! hard-clip. Dither decorrelates the rounding error into a flat, signal-
//! independent noise floor at the word length's own level; rounding removes the
//! bias. 32-bit integer targets are quantized at **24 valid bits** in the high
//! bytes, which is what ASIO `Int32LSB` interfaces and their converters
//! actually resolve — dithering a 32-bit word at 2^-31 would just add noise
//! the DAC never sees.
//!
//! Realtime contract: one xorshift state per stream, no allocation, no
//! branches beyond the format match the compiler resolves at monomorphization.

/// Per-stream dither generator. `Copy`-free on purpose: a stream owns one and
/// advances it sample by sample, so successive frames never share noise.
#[derive(Debug, Clone)]
pub struct OutputDither {
    state: u32,
}

impl Default for OutputDither {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputDither {
    pub fn new() -> Self {
        Self {
            // Any non-zero seed; a fixed one keeps offline bounces reproducible.
            state: 0x9E37_79B9,
        }
    }

    /// Uniform noise in `[0, 1)` from a 32-bit xorshift. 24 mantissa bits of
    /// entropy per draw, which is all `f32` can carry anyway.
    #[inline]
    fn uniform(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        (x >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// One triangular-PDF dither sample in `(-1, 1)` LSB.
    #[inline]
    pub fn tpdf(&mut self) -> f32 {
        self.uniform() - self.uniform()
    }

    /// Quantize a nominal `±1.0` sample to a signed integer with `bits` valid
    /// bits (`bits ≤ 31`): scale, dither, round to nearest, clip to the
    /// representable range. Returned as `i32` holding the `bits`-wide value.
    #[inline]
    pub fn quantize(&mut self, sample: f32, bits: u32) -> i32 {
        let full_scale = (1u32 << (bits - 1)) as f32;
        let max = full_scale - 1.0;
        let scaled = sample * full_scale + self.tpdf();
        // NaN (a broken plugin) must become silence, not `i32::MIN`.
        if scaled.is_nan() {
            return 0;
        }
        scaled.round().clamp(-full_scale, max) as i32
    }
}

/// Conversion of one engine `f32` sample into a device sample type.
///
/// Integer types quantize with dither and clip to the representable range —
/// the one and only place the engine bounds a sample, because an integer word
/// cannot hold anything past full scale. Float types pass through untouched:
/// their resolution exceeds the mix's, and a float device buffer carries an
/// over-full-scale sample exactly as the graph produced it.
pub trait DitheredOutput: Sized {
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self;
}

/// Valid bits carried by a 32-bit integer device word — see the module doc.
pub const I32_VALID_BITS: u32 = 24;

impl DitheredOutput for f32 {
    #[inline]
    fn dithered_from_f32(sample: f32, _: &mut OutputDither) -> Self {
        sample
    }
}

impl DitheredOutput for f64 {
    #[inline]
    fn dithered_from_f32(sample: f32, _: &mut OutputDither) -> Self {
        f64::from(sample)
    }
}

impl DitheredOutput for i8 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        dither.quantize(sample, 8) as i8
    }
}

impl DitheredOutput for i16 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        dither.quantize(sample, 16) as i16
    }
}

impl DitheredOutput for i32 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        dither.quantize(sample, I32_VALID_BITS) << (32 - I32_VALID_BITS)
    }
}

impl DitheredOutput for i64 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        i64::from(i32::dithered_from_f32(sample, dither)) << 32
    }
}

impl DitheredOutput for u8 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        (i16::from(i8::dithered_from_f32(sample, dither)) + 128) as u8
    }
}

impl DitheredOutput for u16 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        (i32::from(i16::dithered_from_f32(sample, dither)) + 32_768) as u16
    }
}

impl DitheredOutput for u32 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        (i64::from(i32::dithered_from_f32(sample, dither)) + (1i64 << 31)) as u32
    }
}

impl DitheredOutput for u64 {
    #[inline]
    fn dithered_from_f32(sample: f32, dither: &mut OutputDither) -> Self {
        (i128::from(i64::dithered_from_f32(sample, dither)) + (1i128 << 63)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpdf_stays_within_one_lsb_and_averages_to_zero() {
        let mut dither = OutputDither::new();
        let mut sum = 0.0f64;
        const N: usize = 200_000;
        for _ in 0..N {
            let d = dither.tpdf();
            assert!(d > -1.0 && d < 1.0, "tpdf sample {d} outside (-1, 1)");
            sum += f64::from(d);
        }
        assert!((sum / N as f64).abs() < 0.01, "dither is biased: {sum}");
    }

    #[test]
    fn silence_stays_within_one_lsb_and_is_unbiased() {
        let mut dither = OutputDither::new();
        let mut sum = 0i64;
        for _ in 0..100_000 {
            let s = i16::dithered_from_f32(0.0, &mut dither);
            assert!((-1..=1).contains(&s), "silence quantized to {s}");
            sum += i64::from(s);
        }
        assert!(sum.abs() < 500, "silence carries a DC bias: {sum}");
    }

    #[test]
    fn rounds_to_nearest_rather_than_truncating() {
        // 0.75 of an i16 LSB, repeatedly: truncation would always give 0,
        // rounding with dither must land on 1 most of the time.
        let mut dither = OutputDither::new();
        let value = 0.75 / 32_768.0;
        let ones = (0..10_000)
            .filter(|_| i16::dithered_from_f32(value, &mut dither) == 1)
            .count();
        assert!(ones > 6_000, "rounded up only {ones} of 10000 times");
    }

    #[test]
    fn full_scale_clips_without_wrapping() {
        let mut dither = OutputDither::new();
        for _ in 0..1_000 {
            assert_eq!(i16::dithered_from_f32(1.5, &mut dither), i16::MAX);
            assert_eq!(i16::dithered_from_f32(-1.5, &mut dither), i16::MIN);
            // Exact full scale may land one LSB inside the range: that is the
            // dither doing its job, not a clipping error.
            assert!(i32::dithered_from_f32(1.0, &mut dither) >= (i32::MAX - 255));
            assert!(i32::dithered_from_f32(-1.0, &mut dither) <= (i32::MIN + 256));
            assert_eq!(i32::dithered_from_f32(-1.5, &mut dither), i32::MIN);
            assert_eq!(i16::dithered_from_f32(f32::NAN, &mut dither), 0);
        }
    }

    #[test]
    fn i32_words_carry_24_valid_bits_in_the_high_bytes() {
        let mut dither = OutputDither::new();
        for step in -100..=100 {
            let s = i32::dithered_from_f32(step as f32 / 100.0, &mut dither);
            assert_eq!(s & 0xFF, 0, "low byte must be padding: {s:#x}");
        }
        let half = i32::dithered_from_f32(0.5, &mut dither);
        assert!((half - (1 << 30)).abs() <= 2 << 8, "0.5 → {half}");
    }

    #[test]
    fn unsigned_formats_are_offset_binary() {
        let mut dither = OutputDither::new();
        let mid = u16::dithered_from_f32(0.0, &mut dither);
        assert!((i32::from(mid) - 32_768).abs() <= 1);
        assert_eq!(u8::dithered_from_f32(-1.5, &mut dither), 0);
        assert_eq!(u8::dithered_from_f32(1.5, &mut dither), u8::MAX);
    }

    #[test]
    fn floats_pass_through_untouched() {
        let mut dither = OutputDither::new();
        assert_eq!(f32::dithered_from_f32(0.123_456, &mut dither), 0.123_456);
        assert_eq!(f64::dithered_from_f32(-0.5, &mut dither), -0.5);
    }
}
