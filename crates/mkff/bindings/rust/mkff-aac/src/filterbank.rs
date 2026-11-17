//! Synthesis filterbank (14496-3 4.6.11): IMDCT, windowing for the four window sequences with
//! sine / KBD shapes, and overlap-add.

use crate::tables::{shared, EIGHT_SHORT, LONG_START, LONG_STOP};
use mkff_audio_core::mdct::Mdct;

/// Output scale: the specification's `2/N` IMDCT normalisation and 16-bit full scale → ±1.
const LONG_SCALE: f32 = 1.0 / (1024.0 * 32768.0);
const SHORT_SCALE: f32 = 1.0 / (128.0 * 32768.0);

/// Per-channel synthesis state.
#[derive(Clone, Debug)]
pub(crate) struct ChannelState {
    pub overlap: Vec<f32>,
    pub prev_shape: u8,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            overlap: vec![0.0; 1024],
            prev_shape: 0,
        }
    }
}

/// The transforms and scratch buffers shared by all channels.
pub(crate) struct Synthesis {
    long: Mdct,
    short: Mdct,
    buf: Vec<f32>,
    z: Vec<f32>,
}

impl Synthesis {
    pub fn new() -> Self {
        Self {
            long: Mdct::new(1024),
            short: Mdct::new(128),
            buf: vec![0.0; 2048],
            z: vec![0.0; 2048],
        }
    }

    /// Inverse transform `coef` (1024 lines, or 8 × 128 for short windows) and overlap-add
    /// into `out` (1024 samples).
    pub fn run(
        &mut self,
        st: &mut ChannelState,
        coef: &[f32],
        seq: u8,
        shape: u8,
        out: &mut [f32],
    ) {
        let sh = shared();
        let lp = &sh.long_win[usize::from(st.prev_shape)];
        let lc = &sh.long_win[usize::from(shape)];
        let sp = &sh.short_win[usize::from(st.prev_shape)];
        let sc = &sh.short_win[usize::from(shape)];
        let out = &mut out[..1024];
        if seq == EIGHT_SHORT {
            let z = &mut self.z;
            z.fill(0.0);
            for w in 0..8 {
                self.short
                    .inverse(&coef[w * 128..w * 128 + 128], &mut self.buf[..256]);
                let left = if w == 0 { sp } else { sc };
                let o = 448 + 128 * w;
                for n in 0..128 {
                    z[o + n] += self.buf[n] * left[n] * SHORT_SCALE;
                    z[o + 128 + n] += self.buf[128 + n] * sc[127 - n] * SHORT_SCALE;
                }
            }
            for n in 0..1024 {
                out[n] = st.overlap[n] + z[n];
            }
            st.overlap.copy_from_slice(&z[1024..2048]);
        } else {
            self.long.inverse(&coef[..1024], &mut self.buf);
            let y = &self.buf;
            if seq == LONG_STOP {
                out[..448].copy_from_slice(&st.overlap[..448]);
                for n in 448..576 {
                    out[n] = st.overlap[n] + y[n] * sp[n - 448] * LONG_SCALE;
                }
                for n in 576..1024 {
                    out[n] = st.overlap[n] + y[n] * LONG_SCALE;
                }
            } else {
                for n in 0..1024 {
                    out[n] = st.overlap[n] + y[n] * lp[n] * LONG_SCALE;
                }
            }
            if seq == LONG_START {
                for n in 0..448 {
                    st.overlap[n] = y[1024 + n] * LONG_SCALE;
                }
                for n in 448..576 {
                    st.overlap[n] = y[1024 + n] * sc[127 - (n - 448)] * LONG_SCALE;
                }
                st.overlap[576..].fill(0.0);
            } else {
                for n in 0..1024 {
                    st.overlap[n] = y[1024 + n] * lc[1023 - n] * LONG_SCALE;
                }
            }
        }
        st.prev_shape = shape;
    }
}
