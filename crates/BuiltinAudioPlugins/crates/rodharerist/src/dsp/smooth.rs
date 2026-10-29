//! Control-value smoothing and 2× oversampling helpers.
//!
//! [`Smoothed`] removes zipper noise now that editor knobs drive the DSP live:
//! `configure()` sets targets on the control path, `tick()` glides one sample
//! toward them on the audio path (one multiply-add, no allocation).
//!
//! [`Oversampler2x`] runs a nonlinear stereo op at twice the sample rate
//! through fixed-coefficient polyphase half-band allpass filters, halving the
//! aliasing the waveshapers fold back at high gain. All state is a few floats;
//! nothing allocates after construction.

use builtin_dsp_core::time_constant;

/// One-pole parameter smoother. `tick` glides `current` toward `target`; the
/// audio thread only ever calls `tick`, the control thread only `set_target` /
/// `snap` (same single-thread-per-side contract as the rest of the DSP).
#[derive(Debug, Clone)]
pub(super) struct Smoothed {
    current: f32,
    target: f32,
    coeff: f32,
}

impl Smoothed {
    pub(super) fn new(sample_rate: f32, seconds: f32, value: f32) -> Self {
        Self {
            current: value,
            target: value,
            coeff: time_constant(sample_rate.max(1.0), seconds),
        }
    }

    /// Re-time the glide after a sample-rate change (keeps current/target).
    pub(super) fn set_time(&mut self, sample_rate: f32, seconds: f32) {
        self.coeff = time_constant(sample_rate.max(1.0), seconds);
    }

    pub(super) fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Jump to the target immediately (reset / first configure).
    pub(super) fn snap(&mut self) {
        self.current = self.target;
    }

    /// Where the glide is now, without advancing it.
    #[inline]
    pub(super) fn value(&self) -> f32 {
        self.current
    }

    #[inline]
    pub(super) fn tick(&mut self) -> f32 {
        self.current = self.target + (self.current - self.target) * self.coeff;
        self.current
    }
}

/// First-order allpass section `A(z) = (a + z⁻¹) / (1 + a·z⁻¹)` — the building
/// block of the polyphase half-band branches below. Unity gain at all
/// frequencies; only phase differs between branches.
#[derive(Debug, Clone, Default)]
struct Allpass1 {
    a: f32,
    x1: f32,
    y1: f32,
}

impl Allpass1 {
    fn new(a: f32) -> Self {
        Self {
            a,
            x1: 0.0,
            y1: 0.0,
        }
    }

    fn clear(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.x1 + self.a * (x - self.y1);
        self.x1 = x;
        // The allpass feeds `y1` back, so a decaying tail parks it in the
        // subnormal range and the whole oversampler slows down with it.
        self.y1 = super::flush_denormal(y);
        y
    }
}

/// Polyphase half-band allpass coefficients (two cascaded sections per
/// branch, even indices → branch A, odd → branch B). Classic fixed design in
/// the hiir/Valenzuela–Constantinides style: ~69 dB stop-band attenuation,
/// comfortably below the waveshapers' harmonic floor.
const HALFBAND_COEFFS: [f32; 4] = [0.041_893_99, 0.168_903_48, 0.390_560_77, 0.743_895_75];

/// One half-band filter: two allpass branches whose outputs interleave (up)
/// or average (down). Separate instances are required for the up and down
/// directions — they carry independent state.
#[derive(Debug, Clone)]
struct Halfband {
    a: [Allpass1; 2],
    b: [Allpass1; 2],
}

impl Halfband {
    fn new() -> Self {
        Self {
            a: [
                Allpass1::new(HALFBAND_COEFFS[0]),
                Allpass1::new(HALFBAND_COEFFS[2]),
            ],
            b: [
                Allpass1::new(HALFBAND_COEFFS[1]),
                Allpass1::new(HALFBAND_COEFFS[3]),
            ],
        }
    }

    fn clear(&mut self) {
        for s in self.a.iter_mut().chain(self.b.iter_mut()) {
            s.clear();
        }
    }

    #[inline]
    fn branch_a(&mut self, x: f32) -> f32 {
        let y = self.a[0].run(x);
        self.a[1].run(y)
    }

    #[inline]
    fn branch_b(&mut self, x: f32) -> f32 {
        let y = self.b[0].run(x);
        self.b[1].run(y)
    }

    /// Interpolate one input sample into two output samples at 2× rate.
    #[inline]
    fn up(&mut self, x: f32) -> (f32, f32) {
        (self.branch_a(x), self.branch_b(x))
    }

    /// Decimate two 2×-rate samples into one output sample.
    #[inline]
    fn down(&mut self, x0: f32, x1: f32) -> f32 {
        0.5 * (self.branch_a(x0) + self.branch_b(x1))
    }
}

/// Stereo 2× oversampling wrapper for a nonlinear per-sample op.
#[derive(Debug, Clone)]
pub(super) struct Oversampler2x {
    up_l: Halfband,
    up_r: Halfband,
    down_l: Halfband,
    down_r: Halfband,
}

impl Oversampler2x {
    pub(super) fn new() -> Self {
        Self {
            up_l: Halfband::new(),
            up_r: Halfband::new(),
            down_l: Halfband::new(),
            down_r: Halfband::new(),
        }
    }

    pub(super) fn reset(&mut self) {
        self.up_l.clear();
        self.up_r.clear();
        self.down_l.clear();
        self.down_r.clear();
    }

    /// Run `f` twice at the doubled rate around one stereo sample. `f` must be
    /// memoryless per call (waveshaper), or carry its own 2×-rate state.
    #[inline]
    pub(super) fn process_stereo(
        &mut self,
        l: f32,
        r: f32,
        mut f: impl FnMut(f32, f32) -> (f32, f32),
    ) -> (f32, f32) {
        let (l0, l1) = self.up_l.up(l);
        let (r0, r1) = self.up_r.up(r);
        let (yl0, yr0) = f(l0, r0);
        let (yl1, yr1) = f(l1, r1);
        (self.down_l.down(yl0, yl1), self.down_r.down(yr0, yr1))
    }
}

/// 4× oversampling as two nested half-band stages. The inner stage runs at
/// the outer stage's doubled rate; nesting keeps every filter's state exactly
/// where it belongs with zero new filter code.
#[derive(Debug, Clone)]
pub(super) struct Oversampler4x {
    outer: Oversampler2x,
    inner: Oversampler2x,
}

impl Oversampler4x {
    pub(super) fn new() -> Self {
        Self {
            outer: Oversampler2x::new(),
            inner: Oversampler2x::new(),
        }
    }

    pub(super) fn reset(&mut self) {
        self.outer.reset();
        self.inner.reset();
    }

    /// Run `f` four times at 4× rate around one stereo sample.
    #[inline]
    pub(super) fn process_stereo(
        &mut self,
        l: f32,
        r: f32,
        mut f: impl FnMut(f32, f32) -> (f32, f32),
    ) -> (f32, f32) {
        let inner = &mut self.inner;
        self.outer
            .process_stereo(l, r, |a, b| inner.process_stereo(a, b, &mut f))
    }
}

/// 8× oversampling as three nested half-band stages — for the most
/// aggressive clippers, whose high-order harmonics alias hardest.
#[derive(Debug, Clone)]
pub(super) struct Oversampler8x {
    outer: Oversampler2x,
    mid: Oversampler2x,
    inner: Oversampler2x,
}

impl Oversampler8x {
    pub(super) fn new() -> Self {
        Self {
            outer: Oversampler2x::new(),
            mid: Oversampler2x::new(),
            inner: Oversampler2x::new(),
        }
    }

    pub(super) fn reset(&mut self) {
        self.outer.reset();
        self.mid.reset();
        self.inner.reset();
    }

    /// Run `f` eight times at 8× rate around one stereo sample.
    #[inline]
    pub(super) fn process_stereo(
        &mut self,
        l: f32,
        r: f32,
        mut f: impl FnMut(f32, f32) -> (f32, f32),
    ) -> (f32, f32) {
        let mid = &mut self.mid;
        let inner = &mut self.inner;
        self.outer.process_stereo(l, r, |a, b| {
            mid.process_stereo(a, b, |c, d| inner.process_stereo(c, d, &mut f))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothed_glides_monotonically_to_target() {
        let mut s = Smoothed::new(48_000.0, 0.010, 0.0);
        s.set_target(1.0);
        let mut last = 0.0;
        for _ in 0..10_000 {
            let v = s.tick();
            assert!(v >= last && v <= 1.0);
            last = v;
        }
        // ~21 time constants: converged for all practical purposes.
        assert!((last - 1.0).abs() < 1.0e-3, "did not converge: {last}");
        s.set_target(0.25);
        s.snap();
        assert_eq!(s.tick() < 0.2501 && s.tick() > 0.2499, true);
    }

    #[test]
    fn oversampler_passes_a_linear_op_transparently() {
        // Identity op through up/down must return the input band unchanged
        // (allpass branches are unity gain; only group delay is added).
        let mut os = Oversampler2x::new();
        let mut peak_in: f32 = 0.0;
        let mut peak_out: f32 = 0.0;
        for n in 0..4_000 {
            let x = (n as f32 * 0.05).sin() * 0.5; // well inside the passband
            let (l, r) = os.process_stereo(x, x, |a, b| (a, b));
            assert!(l.is_finite() && r.is_finite());
            if n > 200 {
                peak_in = peak_in.max(x.abs());
                peak_out = peak_out.max(l.abs());
            }
        }
        assert!(
            (peak_in - peak_out).abs() < 0.05,
            "passband gain drifted: in={peak_in} out={peak_out}"
        );
    }

    #[test]
    fn nested_oversamplers_are_transparent_and_run_f_the_right_number_of_times() {
        let mut os4 = Oversampler4x::new();
        let mut os8 = Oversampler8x::new();
        let mut calls4 = 0u32;
        let mut calls8 = 0u32;
        let mut peak_in: f32 = 0.0;
        let mut peak4: f32 = 0.0;
        let mut peak8: f32 = 0.0;
        for n in 0..4_000 {
            let x = (n as f32 * 0.05).sin() * 0.5;
            let (a, _) = os4.process_stereo(x, x, |l, r| {
                calls4 += 1;
                (l, r)
            });
            let (b, _) = os8.process_stereo(x, x, |l, r| {
                calls8 += 1;
                (l, r)
            });
            assert!(a.is_finite() && b.is_finite());
            if n > 400 {
                peak_in = peak_in.max(x.abs());
                peak4 = peak4.max(a.abs());
                peak8 = peak8.max(b.abs());
            }
        }
        assert_eq!(calls4, 4_000 * 4);
        assert_eq!(calls8, 4_000 * 8);
        assert!((peak_in - peak4).abs() < 0.05, "4x passband drift: {peak4}");
        assert!((peak_in - peak8).abs() < 0.05, "8x passband drift: {peak8}");
    }

    #[test]
    fn oversampler_output_is_finite_through_a_hard_nonlinearity() {
        let mut os = Oversampler2x::new();
        for n in 0..2_000 {
            let x = (n as f32 * 0.3).sin() * 4.0;
            let (l, r) = os.process_stereo(x, -x, |a, b| (a.tanh(), b.tanh()));
            assert!(l.is_finite() && r.is_finite());
            assert!(l.abs() <= 1.5 && r.abs() <= 1.5);
        }
    }
}
