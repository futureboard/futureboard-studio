//! The 32-band polyphase filterbank of ISO/IEC 11172-3 (synthesis: 2.4.3.2 / Annex A.2;
//! analysis: Annex C.1.3), shared by all three layers and the encoder.

/// First half (taps 0..=256) of the prototype low-pass filter in units of 2^-16; the window is
/// symmetric (`h[512 - i] = h[i]`). This is Table B.3's synthesis window `D[i]` without the
/// alternating sign of every second group of 64 taps.
#[rustfmt::skip]
const PROTO: [i32; 257] = [
    0, -1, -1, -1, -1, -1, -1, -2, -2, -2, -2, -3, -3, -4, -4, -5,
    -5, -6, -7, -7, -8, -9, -10, -11, -13, -14, -16, -17, -19, -21, -24, -26,
    -29, -31, -35, -38, -41, -45, -49, -53, -58, -63, -68, -73, -79, -85, -91, -97,
    -104, -111, -117, -125, -132, -139, -147, -154, -161, -169, -176, -183, -190, -196, -202, -208,
    -213, -218, -222, -225, -227, -228, -228, -227, -224, -221, -215, -208, -200, -189, -177, -163,
    -146, -127, -106, -83, -57, -29, 2, 36, 72, 111, 153, 197, 244, 294, 347, 401,
    459, 519, 581, 645, 711, 779, 848, 919, 991, 1064, 1137, 1210, 1283, 1356, 1428, 1498,
    1567, 1634, 1698, 1759, 1817, 1870, 1919, 1962, 2001, 2032, 2057, 2075, 2085, 2087, 2080, 2063,
    2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535, 1414, 1280, 1131, 970, 794, 605, 402, 185,
    -45, -288, -545, -814, -1095, -1388, -1692, -2006, -2330, -2663, -3004, -3351, -3705, -4063, -4425, -4788,
    -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597, -7910, -8209, -8491, -8755, -8998, -9219, -9416, -9585,
    -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750, -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134,
    -6574, -5959, -5288, -4561, -3776, -2935, -2037, -1082, -70, 998, 2122, 3300, 4533, 5818, 7154, 8540,
    9975, 11455, 12980, 14548, 16155, 17799, 19478, 21189, 22929, 24694, 26482, 28289, 30112, 31947, 33791, 35640,
    37489, 39336, 41176, 43006, 44821, 46617, 48390, 50137, 51853, 53534, 55178, 56778, 58333, 59838, 61289, 62684,
    64019, 65290, 66494, 67629, 68692, 69679, 70590, 71420, 72169, 72835, 73415, 73908, 74313, 74630, 74856, 74992,
    75038,
];

/// The synthesis window `D[i]` of Table B.3 (512 taps).
pub(crate) fn synthesis_window() -> &'static [f32; 512] {
    static W: std::sync::OnceLock<[f32; 512]> = std::sync::OnceLock::new();
    W.get_or_init(|| {
        let mut d = [0f32; 512];
        for (i, v) in d.iter_mut().enumerate() {
            let p = if i <= 256 { PROTO[i] } else { PROTO[512 - i] };
            let sign = if (i / 64) % 2 == 1 { -1.0 } else { 1.0 };
            *v = sign * p as f32 / 65536.0;
        }
        d
    })
}

/// The 32-point DCT-II used by both directions: `cos(π m (2k + 1) / 64)`, `[m][k]`.
fn dct_matrix() -> &'static [[f32; 32]; 32] {
    static M: std::sync::OnceLock<[[f32; 32]; 32]> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let mut m = [[0f32; 32]; 32];
        for (i, row) in m.iter_mut().enumerate() {
            for (k, v) in row.iter_mut().enumerate() {
                *v = (std::f64::consts::PI * i as f64 * (2 * k + 1) as f64 / 64.0).cos() as f32;
            }
        }
        m
    })
}

/// Synthesis filterbank state for one channel.
#[derive(Clone)]
pub(crate) struct Synth {
    /// 16 vectors of 64 `V` values (a ring, newest at `pos`).
    v: [[f32; 64]; 16],
    pos: usize,
}

impl Default for Synth {
    fn default() -> Self {
        Self {
            v: [[0.0; 64]; 16],
            pos: 0,
        }
    }
}

impl Synth {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Turn 32 subband samples into 32 PCM samples.
    pub(crate) fn process(&mut self, s: &[f32; 32], out: &mut [f32]) {
        let m = dct_matrix();
        // X[m] = Σ s[k] cos(π m (2k+1) / 64)
        let mut x = [0f32; 33];
        for (xm, row) in x.iter_mut().zip(m.iter()) {
            let mut acc = 0f32;
            for k in 0..32 {
                acc += row[k] * s[k];
            }
            *xm = acc;
        }
        // V[i] = Σ s[k] cos((16 + i)(2k + 1) π / 64) for i in 0..64, from the 32 values of X:
        // V[i] = X[16 + i] (i < 16), 0 (i = 16), -X[48 - i] (16 < i <= 48), -X[i - 48] (i > 48)
        self.pos = (self.pos + 15) & 15;
        let v = &mut self.v[self.pos];
        v[..16].copy_from_slice(&x[16..32]);
        v[16] = 0.0;
        for i in 17..48 {
            v[i] = -x[48 - i];
        }
        for i in 48..64 {
            v[i] = -x[i - 48];
        }
        let d = synthesis_window();
        let mut acc = [0f32; 32];
        for i in 0..8 {
            let va = &self.v[(self.pos + 2 * i) & 15];
            let vb = &self.v[(self.pos + 2 * i + 1) & 15];
            let da = &d[64 * i..64 * i + 32];
            let db = &d[64 * i + 32..64 * i + 64];
            for j in 0..32 {
                acc[j] += va[j] * da[j] + vb[32 + j] * db[j];
            }
        }
        out[..32].copy_from_slice(&acc);
    }
}

/// Analysis filterbank state for one channel (encoder).
#[derive(Clone)]
pub(crate) struct Analysis {
    /// The last 512 input samples, newest at `x[pos]` going backwards (ring of 1024 so a window
    /// read never wraps).
    x: Vec<f32>,
    pos: usize,
    c: [f32; 512],
    /// `cos((2k + 1)(i - 16) π / 64)` `[k][i]`.
    m: Vec<[f32; 64]>,
}

impl Default for Analysis {
    fn default() -> Self {
        let d = synthesis_window();
        let mut c = [0f32; 512];
        for i in 0..512 {
            c[i] = d[i] / 32.0;
        }
        let m = (0..32)
            .map(|k| {
                let mut row = [0f32; 64];
                for (i, v) in row.iter_mut().enumerate() {
                    *v = ((2 * k + 1) as f64 * (i as f64 - 16.0) * std::f64::consts::PI / 64.0)
                        .cos() as f32;
                }
                row
            })
            .collect();
        Self {
            x: vec![0.0; 1024],
            pos: 0,
            c,
            m,
        }
    }
}

impl Analysis {
    /// Consume 32 input samples (in time order) and produce 32 subband samples.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut [f32; 32]) {
        // ring of 512 stored twice so that x[pos..pos + 512] is always contiguous, newest first
        for &s in input.iter().take(32) {
            self.pos = (self.pos + 511) & 511;
            self.x[self.pos] = s;
            self.x[self.pos + 512] = s;
        }
        let x = &self.x[self.pos..self.pos + 512];
        let mut y = [0f32; 64];
        for j in 0..8 {
            let xs = &x[64 * j..64 * j + 64];
            let cs = &self.c[64 * j..64 * j + 64];
            for i in 0..64 {
                y[i] += cs[i] * xs[i];
            }
        }
        for (k, o) in out.iter_mut().enumerate() {
            let row = &self.m[k];
            let mut acc = 0f32;
            for i in 0..64 {
                acc += row[i] * y[i];
            }
            *o = acc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_smooth_and_normalised() {
        let d = synthesis_window();
        assert!((d[256] - 1.144_989).abs() < 1e-6);
        assert!((d[1] + 0.000015259).abs() < 1e-8);
        assert!((d[511] - 0.000015259).abs() < 1e-8);
        // the unsigned prototype is smooth: small second differences
        let p: Vec<f64> = (0..512)
            .map(|i| f64::from(if i <= 256 { PROTO[i] } else { PROTO[512 - i] }))
            .collect();
        for i in 1..511 {
            let dd = p[i - 1] - 2.0 * p[i] + p[i + 1];
            assert!(dd.abs() < 260.0, "second difference at {i}: {dd}");
        }
    }

    /// Analysis followed by synthesis reconstructs the input (delayed by 481 samples).
    #[test]
    fn near_perfect_reconstruction() {
        let n = 32 * 400;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32;
                0.3 * (t * 0.0123).sin()
                    + 0.2 * (t * 0.731).sin()
                    + 0.1 * ((i * 7919 % 101) as f32 / 50.0 - 1.0)
            })
            .collect();
        let mut a = Analysis::default();
        let mut s = Synth::default();
        let mut out = vec![0f32; n];
        let mut sb = [0f32; 32];
        for b in 0..n / 32 {
            a.process(&input[b * 32..], &mut sb);
            s.process(&sb, &mut out[b * 32..]);
        }
        let delay = 481;
        let (mut sig, mut err) = (0f64, 0f64);
        for i in 2000..n - 1000 {
            let e = f64::from(out[i] - input[i - delay]);
            sig += f64::from(input[i - delay]).powi(2);
            err += e * e;
        }
        let snr = 10.0 * (sig / err).log10();
        assert!(snr > 85.0, "reconstruction SNR {snr} dB");
    }
}
