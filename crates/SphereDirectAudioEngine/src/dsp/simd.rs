//! Vector kernels for the render pass, dispatched at runtime by instruction set.
//!
//! Every kernel has one body, written as a plain loop the compiler vectorises.
//! On x86 it is compiled twice: once for AVX2 (256-bit) and once for the SSE2
//! baseline every x86-64 CPU has. [`active_level`] picks between them. AVX2 is
//! the default wherever the CPU reports it; SSE is the fallback, both for CPUs
//! without AVX2 and for anyone who selects it in Settings. Other architectures
//! build the body once, for their own baseline vector unit.
//!
//! Both paths run the same arithmetic in the same order — no fused multiply-add,
//! no reassociation — so switching instruction sets never changes a sample.
//!
//! Realtime-safe: the level is one relaxed atomic load per call, and the
//! kernels only read and write the slices they are given.

use std::sync::atomic::{AtomicU8, Ordering};

/// Instruction set the render kernels run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SimdLevel {
    /// SSE2: present on every x86-64 CPU, so it is always available. Also the
    /// name for the portable baseline on other architectures.
    Sse = 0,
    /// AVX2: twice the vector width. The default where the CPU has it.
    Avx2 = 1,
}

impl SimdLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sse => "SSE",
            Self::Avx2 => "AVX2",
        }
    }
}

const LEVEL_UNRESOLVED: u8 = u8::MAX;

/// What Settings asked for. AVX2 unless the user picked SSE.
static REQUESTED: AtomicU8 = AtomicU8::new(SimdLevel::Avx2 as u8);
/// What the kernels actually run: the request, capped at what the CPU has.
/// Resolved lazily so a build that never sets a preference still gets AVX2.
static ACTIVE: AtomicU8 = AtomicU8::new(LEVEL_UNRESOLVED);

/// Whether this CPU can run the AVX2 kernels.
pub fn cpu_supports_avx2() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        false
    }
}

fn resolve(requested: SimdLevel) -> SimdLevel {
    match requested {
        SimdLevel::Avx2 if cpu_supports_avx2() => SimdLevel::Avx2,
        _ => SimdLevel::Sse,
    }
}

/// Ask for an instruction set. A request the CPU cannot honour falls back to
/// SSE; returns the level that is now active.
pub fn set_simd_level(requested: SimdLevel) -> SimdLevel {
    REQUESTED.store(requested as u8, Ordering::Relaxed);
    let active = resolve(requested);
    ACTIVE.store(active as u8, Ordering::Relaxed);
    active
}

/// The instruction set the kernels are running on right now.
#[inline]
pub fn active_level() -> SimdLevel {
    match ACTIVE.load(Ordering::Relaxed) {
        0 => SimdLevel::Sse,
        1 => SimdLevel::Avx2,
        _ => {
            // First use before any preference was set: resolve the default.
            // `is_x86_feature_detected!` caches its answer, so this is a CPUID
            // at most once, and allocation-free.
            let requested = if REQUESTED.load(Ordering::Relaxed) == SimdLevel::Sse as u8 {
                SimdLevel::Sse
            } else {
                SimdLevel::Avx2
            };
            let active = resolve(requested);
            ACTIVE.store(active as u8, Ordering::Relaxed);
            active
        }
    }
}

/// Define a kernel as `$name(args)`, running on [`active_level`], plus
/// `$name_on(level, args)` for tests that pin a level.
macro_rules! kernel {
    (
        $(#[$meta:meta])*
        pub fn $name:ident / $name_on:ident ($($arg:ident : $ty:ty),* $(,)?) $(-> $ret:ty)? $body:block
    ) => {
        $(#[$meta])*
        #[inline]
        pub fn $name($($arg: $ty),*) $(-> $ret)? {
            $name_on(active_level(), $($arg),*)
        }

        #[inline]
        pub fn $name_on(level: SimdLevel, $($arg: $ty),*) $(-> $ret)? {
            #[inline(always)]
            fn body($($arg: $ty),*) $(-> $ret)? $body

            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                #[target_feature(enable = "avx2")]
                unsafe fn avx2($($arg: $ty),*) $(-> $ret)? {
                    body($($arg),*)
                }
                if level == SimdLevel::Avx2 && cpu_supports_avx2() {
                    // SAFETY: only reached when the CPU reports AVX2.
                    return unsafe { avx2($($arg),*) };
                }
            }
            let _ = level;
            body($($arg),*)
        }
    };
}

kernel! {
    /// `dst[i] += src[i]`.
    pub fn add_into / add_into_on(dst: &mut [f32], src: &[f32]) {
        let n = dst.len().min(src.len());
        let (dst, src) = (&mut dst[..n], &src[..n]);
        for i in 0..n {
            dst[i] += src[i];
        }
    }
}

kernel! {
    /// `dst[i] += src[i] * gain`.
    pub fn add_scaled_into / add_scaled_into_on(dst: &mut [f32], src: &[f32], gain: f32) {
        let n = dst.len().min(src.len());
        let (dst, src) = (&mut dst[..n], &src[..n]);
        for i in 0..n {
            dst[i] += src[i] * gain;
        }
    }
}

kernel! {
    /// `l[i] *= gain_l; r[i] *= gain_r`.
    pub fn gain_stereo / gain_stereo_on(l: &mut [f32], r: &mut [f32], gain_l: f32, gain_r: f32) {
        let n = l.len().min(r.len());
        let (l, r) = (&mut l[..n], &mut r[..n]);
        for i in 0..n {
            l[i] *= gain_l;
            r[i] *= gain_r;
        }
    }
}

kernel! {
    /// A per-sample gain ramp: sample `i` is scaled by `start + inc * i`, on
    /// each side independently. The same expression the scalar fader used, so
    /// a ramp is bit-identical to it.
    pub fn ramp_gain_stereo / ramp_gain_stereo_on(
        l: &mut [f32],
        r: &mut [f32],
        start_l: f32,
        inc_l: f32,
        start_r: f32,
        inc_r: f32,
    ) {
        let n = l.len().min(r.len());
        let (l, r) = (&mut l[..n], &mut r[..n]);
        for i in 0..n {
            let step = i as f32;
            l[i] *= start_l + inc_l * step;
            r[i] *= start_r + inc_r * step;
        }
    }
}

kernel! {
    /// Fold a stereo pair to its mid (`(l + r) / 2`) on both sides.
    pub fn fold_mid / fold_mid_on(l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let (l, r) = (&mut l[..n], &mut r[..n]);
        for i in 0..n {
            let m = (l[i] + r[i]) * 0.5;
            l[i] = m;
            r[i] = m;
        }
    }
}

kernel! {
    /// Fold a stereo pair to its side (`(l - r) / 2`) on both sides.
    pub fn fold_side / fold_side_on(l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let (l, r) = (&mut l[..n], &mut r[..n]);
        for i in 0..n {
            let s = (l[i] - r[i]) * 0.5;
            l[i] = s;
            r[i] = s;
        }
    }
}

kernel! {
    /// Peak magnitude and sum of squares of `samples`, for a meter.
    ///
    /// Eight independent lanes, combined at the end in a fixed order: the same
    /// answer on every instruction set. NaN never wins the peak, as with
    /// `f32::max`.
    pub fn peak_and_sum_sq / peak_and_sum_sq_on(samples: &[f32]) -> (f32, f32) {
        const LANES: usize = 8;
        let mut peak = [0.0f32; LANES];
        let mut sum = [0.0f32; LANES];
        let mut chunks = samples.chunks_exact(LANES);
        for chunk in &mut chunks {
            for lane in 0..LANES {
                let x = chunk[lane];
                peak[lane] = peak[lane].max(x.abs());
                sum[lane] += x * x;
            }
        }
        let mut total_peak = 0.0f32;
        let mut total_sum = 0.0f32;
        for lane in 0..LANES {
            total_peak = total_peak.max(peak[lane]);
            total_sum += sum[lane];
        }
        for &x in chunks.remainder() {
            total_peak = total_peak.max(x.abs());
            total_sum += x * x;
        }
        (total_peak, total_sum)
    }
}

kernel! {
    /// True when every sample is finite. Scans the whole slice rather than
    /// stopping at the first bad sample, which is what lets it vectorise; the
    /// bad case is the rare one.
    pub fn all_finite / all_finite_on(samples: &[f32]) -> bool {
        let mut bad = false;
        for &x in samples {
            bad |= !x.is_finite();
        }
        !bad
    }
}

kernel! {
    /// Sum a planar stereo pair into an interleaved stereo buffer:
    /// `out[2i] += l[i]; out[2i + 1] += r[i]`.
    pub fn add_planar_to_interleaved_stereo / add_planar_to_interleaved_stereo_on(
        out: &mut [f32],
        l: &[f32],
        r: &[f32],
    ) {
        let n = (out.len() / 2).min(l.len()).min(r.len());
        let (out, l, r) = (&mut out[..n * 2], &l[..n], &r[..n]);
        for i in 0..n {
            out[2 * i] += l[i];
            out[2 * i + 1] += r[i];
        }
    }
}

kernel! {
    /// `samples[i] *= gain`.
    pub fn scale / scale_on(samples: &mut [f32], gain: f32) {
        for x in samples.iter_mut() {
            *x *= gain;
        }
    }
}

kernel! {
    /// Ramp an interleaved stereo buffer: frame `i` is scaled by
    /// `start + inc * i` on both channels.
    pub fn ramp_interleaved_stereo / ramp_interleaved_stereo_on(
        out: &mut [f32],
        start: f32,
        inc: f32,
    ) {
        let n = out.len() / 2;
        let out = &mut out[..n * 2];
        for i in 0..n {
            let g = start + inc * i as f32;
            out[2 * i] *= g;
            out[2 * i + 1] *= g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, sign-varying test signal with a few awkward values.
    fn signal(len: usize, seed: u32) -> Vec<f32> {
        let mut state = seed.wrapping_mul(2_654_435_761).max(1);
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    const LEVELS: [SimdLevel; 2] = [SimdLevel::Sse, SimdLevel::Avx2];
    /// Lengths that cover empty, shorter than one vector, and ragged tails.
    const LENGTHS: [usize; 7] = [0, 1, 7, 8, 9, 63, 517];

    #[test]
    fn requesting_avx2_falls_back_to_sse_where_the_cpu_lacks_it() {
        assert_eq!(resolve(SimdLevel::Sse), SimdLevel::Sse);
        let expected = if cpu_supports_avx2() {
            SimdLevel::Avx2
        } else {
            SimdLevel::Sse
        };
        assert_eq!(resolve(SimdLevel::Avx2), expected);
    }

    #[test]
    fn every_level_matches_the_scalar_reference_bit_for_bit() {
        for len in LENGTHS {
            let a = signal(len, 1);
            let b = signal(len, 2);
            for level in LEVELS {
                let mut dst = a.clone();
                add_into_on(level, &mut dst, &b);
                let want: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
                assert_eq!(dst, want, "add_into {level:?} len {len}");

                let mut dst = a.clone();
                add_scaled_into_on(level, &mut dst, &b, 0.37);
                let want: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y * 0.37).collect();
                assert_eq!(dst, want, "add_scaled_into {level:?} len {len}");

                let (mut l, mut r) = (a.clone(), b.clone());
                ramp_gain_stereo_on(level, &mut l, &mut r, 0.2, 0.001, 0.9, -0.0005);
                for i in 0..len {
                    assert_eq!(l[i], a[i] * (0.2 + 0.001 * i as f32));
                    assert_eq!(r[i], b[i] * (0.9 + -0.0005 * i as f32));
                }

                let (mut l, mut r) = (a.clone(), b.clone());
                gain_stereo_on(level, &mut l, &mut r, 0.5, 1.5);
                for i in 0..len {
                    assert_eq!((l[i], r[i]), (a[i] * 0.5, b[i] * 1.5));
                }

                let (mut l, mut r) = (a.clone(), b.clone());
                fold_mid_on(level, &mut l, &mut r);
                for i in 0..len {
                    let m = (a[i] + b[i]) * 0.5;
                    assert_eq!((l[i], r[i]), (m, m));
                }
                let (mut l, mut r) = (a.clone(), b.clone());
                fold_side_on(level, &mut l, &mut r);
                for i in 0..len {
                    let s = (a[i] - b[i]) * 0.5;
                    assert_eq!((l[i], r[i]), (s, s));
                }

                let mut out: Vec<f32> = signal(len * 2, 3);
                let before = out.clone();
                add_planar_to_interleaved_stereo_on(level, &mut out, &a, &b);
                for i in 0..len {
                    assert_eq!(out[2 * i], before[2 * i] + a[i]);
                    assert_eq!(out[2 * i + 1], before[2 * i + 1] + b[i]);
                }

                let mut out = before.clone();
                ramp_interleaved_stereo_on(level, &mut out, 1.0, -0.001);
                for i in 0..len {
                    let g = 1.0 + -0.001 * i as f32;
                    assert_eq!(out[2 * i], before[2 * i] * g);
                    assert_eq!(out[2 * i + 1], before[2 * i + 1] * g);
                }

                let mut out = a.clone();
                scale_on(level, &mut out, 0.25);
                let want: Vec<f32> = a.iter().map(|x| x * 0.25).collect();
                assert_eq!(out, want);
            }
        }
    }

    #[test]
    fn the_meter_reads_the_same_on_every_level() {
        for len in LENGTHS {
            let a = signal(len, 9);
            let (peak, sum) = peak_and_sum_sq_on(SimdLevel::Sse, &a);
            let want_peak = a.iter().fold(0.0f32, |p, x| p.max(x.abs()));
            let want_sum: f64 = a.iter().map(|x| (*x as f64) * (*x as f64)).sum();
            assert_eq!(peak, want_peak, "len {len}");
            assert!((sum as f64 - want_sum).abs() <= 1e-4 * want_sum.max(1.0));
            assert_eq!(
                peak_and_sum_sq_on(SimdLevel::Avx2, &a),
                (peak, sum),
                "levels disagree at len {len}"
            );
        }
        // NaN never becomes the peak.
        let (peak, _) = peak_and_sum_sq(&[0.5, f32::NAN, -0.25]);
        assert_eq!(peak, 0.5);
    }

    #[test]
    fn a_single_bad_sample_anywhere_is_caught() {
        for level in LEVELS {
            for len in [1usize, 8, 9, 100] {
                let mut samples = vec![0.25f32; len];
                assert!(all_finite_on(level, &samples));
                for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                    samples[len - 1] = bad;
                    assert!(!all_finite_on(level, &samples), "{level:?} {len} {bad}");
                    samples[len - 1] = 0.25;
                }
            }
        }
    }
}
