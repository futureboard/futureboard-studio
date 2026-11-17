//! AAC-LC encoder: MDCT analysis with block switching, a psychoacoustic model giving the allowed
//! distortion of every scalefactor band, a rate loop over a global threshold offset (bit
//! reservoir, 6144 bits per channel limit), per-band scalefactors, Huffman codebook selection
//! with section merging (dynamic programming), per-band M/S, TNS and PNS.

use crate::adts::adts_header;
use crate::asc::{AudioSpecificConfig, ProgramConfig};
use crate::huffman::{book_index, spectral_codes, BOOK_PARAMS};
use crate::huffman_tables::{SF_BITS, SF_CODES};
use crate::ics::{parcor_to_lpc, tns_ma_filter};
use crate::tables::*;
use mkff_audio_core::bits::BitWriter;
use mkff_audio_core::mdct::Mdct;
use mkff_audio_core::{Encoder, Error, Packet, Result};

/// Encoder settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AacEncoderConfig {
    /// Sampling rate in Hz: one of the 13 AAC table rates (8000 … 96000).
    pub sample_rate: u32,
    /// 1 to 8 channels in WAV order: 1–6 use channel configurations 1–6 (mono, stereo, 3.0,
    /// 4.0 with back centre, 5.0, 5.1); 7 channels (6.1: FL FR FC LFE BC SL SR) use a program
    /// config element; 8 channels (7.1: FL FR FC LFE BL BR SL SR) use channel configuration 7
    /// with its pairs as front, side and back (as FFmpeg does).
    pub channels: usize,
    /// Average bitrate in bits per second for all channels.
    pub bitrate_bps: u32,
    /// Temporal noise shaping.
    pub tns: bool,
    /// Perceptual noise substitution of noise-like bands above 4 kHz (long blocks).
    pub pns: bool,
    /// Per-band M/S stereo in channel pairs.
    pub ms_stereo: bool,
    /// Audio bandwidth in Hz (0 = chosen from the bitrate per channel).
    pub bandwidth_hz: u32,
}

impl AacEncoderConfig {
    /// Defaults: TNS and M/S on, PNS off, automatic bandwidth.
    pub fn new(sample_rate: u32, channels: usize, bitrate_bps: u32) -> Self {
        Self {
            sample_rate,
            channels,
            bitrate_bps,
            tns: true,
            pns: false,
            ms_stereo: true,
            bandwidth_hz: 0,
        }
    }
}

impl Default for AacEncoderConfig {
    fn default() -> Self {
        Self::new(44100, 2, 128_000)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sce,
    Cpe,
    Lfe,
}

#[derive(Clone, Copy, Debug)]
struct Elem {
    kind: Kind,
    ch: [usize; 2],
    tag: u8,
}

fn elem(kind: Kind, a: usize, b: usize, tag: u8) -> Elem {
    Elem {
        kind,
        ch: [a, b],
        tag,
    }
}

/// Channel configuration, elements (input channels in WAV order) and PCE for a channel count.
fn layout(channels: usize, sf_index: u8) -> Option<(u8, Vec<Elem>, Option<ProgramConfig>)> {
    use Kind::*;
    Some(match channels {
        1 => (1, vec![elem(Sce, 0, 0, 0)], None),
        2 => (2, vec![elem(Cpe, 0, 1, 0)], None),
        3 => (3, vec![elem(Sce, 2, 0, 0), elem(Cpe, 0, 1, 0)], None),
        4 => (
            4,
            vec![elem(Sce, 2, 0, 0), elem(Cpe, 0, 1, 0), elem(Sce, 3, 0, 1)],
            None,
        ),
        5 => (
            5,
            vec![elem(Sce, 2, 0, 0), elem(Cpe, 0, 1, 0), elem(Cpe, 3, 4, 1)],
            None,
        ),
        6 => (
            6,
            vec![
                elem(Sce, 2, 0, 0),
                elem(Cpe, 0, 1, 0),
                elem(Cpe, 4, 5, 1),
                elem(Lfe, 3, 0, 0),
            ],
            None,
        ),
        7 => {
            let pce = ProgramConfig {
                object_type: 1,
                sampling_index: sf_index,
                front: vec![(false, 0), (true, 0)],
                side: vec![(true, 1)],
                back: vec![(false, 1)],
                lfe: vec![0],
                ..Default::default()
            };
            (
                0,
                vec![
                    elem(Sce, 2, 0, 0),
                    elem(Cpe, 0, 1, 0),
                    elem(Cpe, 5, 6, 1),
                    elem(Sce, 4, 0, 1),
                    elem(Lfe, 3, 0, 0),
                ],
                Some(pce),
            )
        }
        8 => (
            7,
            vec![
                elem(Sce, 2, 0, 0),
                elem(Cpe, 0, 1, 0),
                elem(Cpe, 6, 7, 1),
                elem(Cpe, 4, 5, 2),
                elem(Lfe, 3, 0, 0),
            ],
            None,
        ),
        _ => return None,
    })
}

/// Destination of bits: a writer or a counter (so counting and writing cannot disagree).
trait Sink {
    fn put(&mut self, v: u32, n: u32);
    fn len(&self) -> usize;
}

impl Sink for BitWriter {
    #[inline]
    fn put(&mut self, v: u32, n: u32) {
        self.write(v, n);
    }
    fn len(&self) -> usize {
        self.bit_len()
    }
}

#[derive(Default)]
struct Counter(usize);

impl Sink for Counter {
    #[inline]
    fn put(&mut self, _v: u32, n: u32) {
        self.0 += n as usize;
    }
    fn len(&self) -> usize {
        self.0
    }
}

const NB: usize = 128;
const INF: u32 = u32::MAX / 4;

/// TNS filter of one window (one filter per window, coef_res 4 bits, no compression).
#[derive(Clone, Copy, Default)]
struct TnsEnc {
    present: bool,
    order: [u8; 8],
    length: [u8; 8],
    coef: [[i8; TNS_MAX_ORDER_LONG]; 8],
}

/// One channel of a frame: analysis results and the current quantisation plan.
struct Chan {
    seq: u8,
    groups: usize,
    group_len: [u8; 8],
    num_windows: usize,
    swb: &'static [u16],
    num_swb: usize,
    max_sfb: usize,
    x: Box<[f32; 1024]>,
    xa: Box<[f32; 1024]>,
    energy: [f32; NB],
    thr: [f32; NB],
    tonal: [f32; NB],
    sqsum: [f32; NB],
    maxabs: [f32; NB],
    pns: [bool; NB],
    tns: TnsEnc,
    // plan
    sf: [i32; NB],
    book: [u8; NB],
    q: Box<[i32; 1024]>,
    global_gain: i32,
    /// max_sfb written (trailing zero bands trimmed unless TNS is active).
    eff_max_sfb: usize,
}

impl Chan {
    fn new() -> Self {
        Self {
            seq: ONLY_LONG,
            groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            num_windows: 1,
            swb: swb_offset_long(4),
            num_swb: 49,
            max_sfb: 0,
            x: Box::new([0.0; 1024]),
            xa: Box::new([0.0; 1024]),
            energy: [0.0; NB],
            thr: [0.0; NB],
            tonal: [0.0; NB],
            sqsum: [0.0; NB],
            maxabs: [0.0; NB],
            pns: [false; NB],
            tns: TnsEnc::default(),
            sf: [0; NB],
            book: [0; NB],
            q: Box::new([0; 1024]),
            global_gain: 100,
            eff_max_sfb: 0,
        }
    }

    fn group_starts(&self) -> [usize; 9] {
        let mut s = [0usize; 9];
        for g in 0..self.groups {
            s[g + 1] = s[g] + usize::from(self.group_len[g]);
        }
        s
    }

    fn short(&self) -> bool {
        self.seq == EIGHT_SHORT
    }

    /// Recompute the per-band statistics of the (possibly M/S-transformed) spectrum.
    fn band_stats(&mut self) {
        for (a, &v) in self.xa.iter_mut().zip(self.x.iter()) {
            *a = v.abs().powf(0.75);
        }
        let starts = self.group_starts();
        for g in 0..self.groups {
            for b in 0..self.num_swb {
                let (lo, hi) = (usize::from(self.swb[b]), usize::from(self.swb[b + 1]));
                let (mut e, mut s, mut m) = (0.0f32, 0.0f32, 0.0f32);
                for w in starts[g]..starts[g + 1] {
                    for k in w * 128 + lo..w * 128 + hi {
                        let v = self.x[k];
                        e += v * v;
                        s += v.abs().sqrt();
                        m = m.max(v.abs());
                    }
                }
                let i = g * 16 + b;
                self.energy[i] = e;
                self.sqsum[i] = s;
                self.maxabs[i] = m;
            }
        }
    }
}

/// Per-band constants of the psychoacoustic model for one block length.
struct PsyBands {
    /// Absolute threshold (energy) per band.
    ath: Vec<f32>,
    /// Spreading attenuation from band b-1 to b (upwards) and from b+1 to b (downwards).
    up: Vec<f32>,
    down: Vec<f32>,
    /// First band at or above 4 kHz (PNS) and 1.4 kHz (TNS start).
    pns_start: usize,
    tns_start: usize,
    /// Short blocks: higher minimum SNR (little temporal masking around transients).
    short: bool,
}

fn bark(f: f64) -> f64 {
    13.0 * (0.00076 * f).atan() + 3.5 * ((f / 7500.0).powi(2)).atan()
}

/// Absolute threshold of hearing in dB SPL (Terhardt).
fn ath_db(f: f64) -> f64 {
    let k = (f.max(20.0)) / 1000.0;
    3.64 * k.powf(-0.8) - 6.5 * (-0.6 * (k - 3.3).powi(2)).exp() + 1e-3 * k.powi(4)
}

impl PsyBands {
    fn new(swb: &[u16], lines: usize, rate: u32, full_scale_energy: f64) -> Self {
        let nb = swb.len() - 1;
        let hz = |line: f64| line * f64::from(rate) / (2.0 * lines as f64);
        let centre: Vec<f64> = (0..nb)
            .map(|b| bark(hz((f64::from(swb[b]) + f64::from(swb[b + 1])) / 2.0)))
            .collect();
        let ath = (0..nb)
            .map(|b| {
                let mut m = f64::MAX;
                for k in usize::from(swb[b])..usize::from(swb[b + 1]) {
                    m = m.min(ath_db(hz(k as f64 + 0.5)));
                }
                // full scale sine ≙ 96 dB SPL; keep the threshold within sane limits
                (full_scale_energy * 10f64.powf((m.min(80.0) - 96.0) / 10.0)) as f32
            })
            .collect();
        let up = (0..nb)
            .map(|b| {
                if b == 0 {
                    0.0
                } else {
                    10f64.powf(-1.5 * (centre[b] - centre[b - 1])) as f32
                }
            })
            .collect();
        let down = (0..nb)
            .map(|b| {
                if b + 1 >= nb {
                    0.0
                } else {
                    10f64.powf(-2.7 * (centre[b + 1] - centre[b])) as f32
                }
            })
            .collect();
        let first_at = |f: f64| (0..nb).find(|&b| hz(f64::from(swb[b])) >= f).unwrap_or(nb);
        Self {
            ath,
            up,
            down,
            pns_start: first_at(4000.0),
            tns_start: first_at(1400.0),
            short: lines == 128,
        }
    }

    /// Masking thresholds of one window: energies `e`, tonality `t` (0 = noise, 1 = tone).
    fn thresholds(&self, e: &[f32], t: &[f32], thr: &mut [f32]) {
        let nb = e.len();
        let mut s = [0f32; 64];
        for b in 0..nb {
            s[b] = e[b];
            if b > 0 {
                s[b] = s[b].max(s[b - 1] * self.up[b]);
            }
        }
        for b in (0..nb.saturating_sub(1)).rev() {
            s[b] = s[b].max(s[b + 1] * self.down[b]);
        }
        for b in 0..nb {
            // required SNR: 6 dB below noise maskers, 24 dB below tonal ones (12 … 22 dB in
            // short blocks)
            let snr_db = if self.short {
                12.0 + 10.0 * t[b]
            } else {
                6.0 + 18.0 * t[b]
            };
            thr[b] = (s[b] * 10f32.powf(-snr_db / 10.0)).max(self.ath[b]);
        }
    }
}

/// Tonality of a band from its spectral flatness: 0 (flat, noise) … 1 (peaky, tonal).
fn tonality(x: &[f32]) -> f32 {
    let n = x.len() as f32;
    let (mut lg, mut ar) = (0.0f32, 0.0f32);
    for &v in x {
        let p = v * v + 1e-9;
        lg += p.ln();
        ar += p;
    }
    if ar <= 1e-6 * n {
        return 0.0;
    }
    let sfm_db = 10.0 * std::f32::consts::LOG10_E * (lg / n - (ar / n).ln());
    ((-sfm_db - 6.0) / 14.0).clamp(0.0, 1.0)
}

/// Per input channel encoder state.
struct ChanState {
    /// Input samples from the start of the current analysis window.
    buf: Vec<f32>,
    prev_seq: u8,
    prev_thr: [f32; 64],
    prev_long: bool,
    /// Short-block decision (and attack window) of the next frame.
    next: Option<u8>,
}

/// AAC-LC encoder (`mkff_audio_core::Encoder`).
///
/// Packets are raw access units (`raw_data_block`); [`AacEncoder::adts_frame`] wraps one in an
/// ADTS header. [`Encoder::config`] returns the `AudioSpecificConfig`. The encoder delay
/// ([`Encoder::priming`]) is 1024 samples: decoded output sample `n + 1024` is input sample
/// `n`. Packet `k` has `pts = 1024·k` and lasts 1024 samples.
pub struct AacEncoder {
    cfg: AacEncoderConfig,
    asc: AudioSpecificConfig,
    sf_index: u8,
    elems: Vec<Elem>,
    pce: Option<ProgramConfig>,
    states: Vec<ChanState>,
    chans: Vec<Chan>,
    long_mdct: Mdct,
    short_mdct: Mdct,
    psy_long: PsyBands,
    psy_short: PsyBands,
    sine_long: Vec<f32>,
    sine_short: Vec<f32>,
    tmp: Vec<f32>,
    /// max_sfb from the bandwidth (long, short) and for the LFE.
    bw_sfb: (usize, usize),
    lfe_sfb: usize,
    mean_bits: f64,
    /// Threshold offset chosen for the previous frame (rate loop start).
    last_offset: f32,
    reservoir: f64,
    max_reservoir: f64,
    /// Input samples received / packets emitted.
    inputs: u64,
    packets: u64,
    total_bits: u64,
}

impl AacEncoder {
    /// Create an encoder. Errors with [`Error::InvalidArgument`] for an unsupported rate, a
    /// channel count outside 1..=8 or a bitrate outside 8 kbps … 320 kbps per channel.
    pub fn new(cfg: AacEncoderConfig) -> Result<Self> {
        let sf_index = sampling_index(cfg.sample_rate).ok_or_else(|| {
            Error::InvalidArgument(format!("unsupported sample rate {}", cfg.sample_rate))
        })?;
        let (chan_config, elems, pce) = layout(cfg.channels, sf_index).ok_or_else(|| {
            Error::InvalidArgument(format!("unsupported channel count {}", cfg.channels))
        })?;
        let per_ch = f64::from(cfg.bitrate_bps) / cfg.channels as f64;
        let max_per_ch = 6144.0 * f64::from(cfg.sample_rate) / 1024.0;
        if !(8000.0..=320_000f64.min(max_per_ch)).contains(&per_ch) {
            return Err(Error::InvalidArgument(format!(
                "bitrate {} for {} channels",
                cfg.bitrate_bps, cfg.channels
            )));
        }
        let mut asc = AudioSpecificConfig::lc(cfg.sample_rate, chan_config);
        asc.pce = pce.clone();
        let swb_l = swb_offset_long(sf_index);
        let swb_s = swb_offset_short(sf_index);
        // energy of a full-scale sine in the coefficient domain
        let mut long_mdct = Mdct::new(1024);
        let mut short_mdct = Mdct::new(128);
        let fs_energy = |m: &mut Mdct, n: usize| {
            let win = mkff_audio_core::mdct::sine_window(2 * n);
            let f = (n / 4) as f64 + 0.5;
            let x: Vec<f32> = (0..2 * n)
                .map(|i| (std::f64::consts::PI * f * i as f64 / n as f64).sin() as f32 * win[i])
                .collect();
            let mut y = vec![0.0; n];
            m.forward(&x, &mut y);
            y.iter()
                .map(|v| f64::from(*v * 65536.0).powi(2))
                .sum::<f64>()
        };
        let e_long = fs_energy(&mut long_mdct, 1024);
        let e_short = fs_energy(&mut short_mdct, 128);
        let psy_long = PsyBands::new(swb_l, 1024, cfg.sample_rate, e_long);
        let psy_short = PsyBands::new(swb_s, 128, cfg.sample_rate, e_short);
        let bw = if cfg.bandwidth_hz > 0 {
            f64::from(cfg.bandwidth_hz)
        } else {
            // bandwidth by bitrate per channel
            let pts = [
                (8000.0, 3500.0),
                (16000.0, 6000.0),
                (32000.0, 11000.0),
                (48000.0, 14500.0),
                (64000.0, 17000.0),
                (96000.0, 20000.0),
            ];
            let mut bw = 20000.0;
            for w in pts.windows(2) {
                if per_ch <= w[1].0 {
                    let t = ((per_ch - w[0].0) / (w[1].0 - w[0].0)).clamp(0.0, 1.0);
                    bw = w[0].1 + t * (w[1].1 - w[0].1);
                    break;
                }
            }
            if per_ch <= pts[0].0 {
                bw = pts[0].1;
            }
            bw
        }
        .min(0.5 * f64::from(cfg.sample_rate));
        let sfb_for = |swb: &[u16], lines: f64, f: f64| {
            let line = f * 2.0 * lines / f64::from(cfg.sample_rate);
            let n = swb.len() - 1;
            (0..n)
                .find(|&b| f64::from(swb[b + 1]) > line)
                .map_or(n, |b| b + 1)
                .min(n)
        };
        let bw_sfb = (sfb_for(swb_l, 1024.0, bw), sfb_for(swb_s, 128.0, bw));
        let lfe_sfb = sfb_for(swb_l, 1024.0, 240.0).max(1);
        let mean_bits = f64::from(cfg.bitrate_bps) * 1024.0 / f64::from(cfg.sample_rate);
        let max_frame = 6144.0 * cfg.channels as f64;
        let max_reservoir = (max_frame - mean_bits).max(0.0);
        let n = cfg.channels;
        Ok(Self {
            states: (0..n)
                .map(|_| ChanState {
                    buf: vec![0.0; 1024],
                    prev_seq: ONLY_LONG,
                    prev_thr: [0.0; 64],
                    prev_long: false,
                    next: None,
                })
                .collect(),
            chans: (0..n).map(|_| Chan::new()).collect(),
            long_mdct,
            short_mdct,
            psy_long,
            psy_short,
            sine_long: mkff_audio_core::mdct::sine_window(2048),
            sine_short: mkff_audio_core::mdct::sine_window(256),
            tmp: vec![0.0; 2048],
            bw_sfb,
            lfe_sfb,
            mean_bits,
            last_offset: 0.0,
            reservoir: 0.5 * max_reservoir,
            max_reservoir,
            inputs: 0,
            packets: 0,
            total_bits: 0,
            cfg,
            asc,
            sf_index,
            elems,
            pce,
        })
    }

    /// The `AudioSpecificConfig` of the stream.
    pub fn audio_specific_config(&self) -> &AudioSpecificConfig {
        &self.asc
    }

    /// Wrap a raw access unit from this encoder in an ADTS header.
    pub fn adts_frame(&self, raw: &[u8]) -> Vec<u8> {
        let mut v = adts_header(&self.asc, raw.len()).to_vec();
        v.extend_from_slice(raw);
        v
    }

    /// Bits written so far (payloads only).
    pub fn total_bits(&self) -> u64 {
        self.total_bits
    }

    /// Short-block decision for the frame whose short windows cover `buf[start..start+1024]`.
    /// Returns a mask of the short windows (bit w) holding an attack.
    fn detect_attack(buf: &[f32], start: usize) -> u8 {
        // high-passed energies of 128-sample segments; the 8 segments before `start` are the
        // reference
        let seg_energy = |s: usize| -> f32 {
            let mut e = 0.0;
            for i in s.max(1)..s + 128 {
                let d = buf[i] - buf[i - 1];
                e += d * d;
            }
            e
        };
        let mut hist = [0f32; 17];
        for (j, h) in hist.iter_mut().enumerate() {
            // segment j starts at start - 1024 + 128 j
            let s = start + j * 128;
            *h = if s >= 1024 { seg_energy(s - 1024) } else { 0.0 };
        }
        // hist[0..8] = 8 segments before `start`, hist[8..16] = the frame's segments
        // segments 8..16 are the frame's short windows; segment 16 (the first short window of
        // the next frame) also lies under this frame's window tail, where a long-start slope
        // would smear an attack, so it counts as an attack in the last window
        let mut mask = 0u8;
        for j in 8..17 {
            let avg = hist[j - 8..j].iter().sum::<f32>() / 8.0;
            let peak_prev = hist[j - 3..j].iter().cloned().fold(0.0, f32::max);
            if hist[j] > 1e-5 && hist[j] > 10.0 * avg && hist[j] > 4.0 * peak_prev {
                mask |= 1 << (j - 8).min(7);
            }
        }
        mask
    }

    /// Analysis of channel `c` for the current frame with the given window sequence.
    fn analyse(&mut self, c: usize, seq: u8, groups: &[u8]) {
        let ch = &mut self.chans[c];
        let st = &self.states[c];
        ch.seq = seq;
        let buf = &st.buf[..2048];
        if seq == EIGHT_SHORT {
            ch.num_windows = 8;
            ch.swb = swb_offset_short(self.sf_index);
            ch.groups = groups.len();
            ch.group_len = [0; 8];
            ch.group_len[..groups.len()].copy_from_slice(groups);
            for w in 0..8 {
                let o = 448 + 128 * w;
                for i in 0..256 {
                    self.tmp[i] = buf[o + i] * self.sine_short[i] * 65536.0;
                }
                self.short_mdct
                    .forward(&self.tmp[..256], &mut ch.x[w * 128..w * 128 + 128]);
            }
        } else {
            ch.num_windows = 1;
            ch.swb = swb_offset_long(self.sf_index);
            ch.groups = 1;
            ch.group_len = [1, 0, 0, 0, 0, 0, 0, 0];
            let wl = &self.sine_long;
            let ws = &self.sine_short;
            for i in 0..2048 {
                let w = match seq {
                    LONG_START => {
                        if i < 1024 {
                            wl[i]
                        } else if i < 1472 {
                            1.0
                        } else if i < 1600 {
                            ws[128 + i - 1472]
                        } else {
                            0.0
                        }
                    }
                    LONG_STOP => {
                        if i < 448 {
                            0.0
                        } else if i < 576 {
                            ws[i - 448]
                        } else if i < 1024 {
                            1.0
                        } else {
                            wl[i]
                        }
                    }
                    _ => wl[i],
                };
                self.tmp[i] = buf[i] * w * 65536.0;
            }
            self.long_mdct.forward(&self.tmp, &mut ch.x[..]);
        }
        ch.num_swb = ch.swb.len() - 1;
    }

    /// Psychoacoustic thresholds, tonality and PNS / TNS decisions of channel `c`.
    fn psy(&mut self, c: usize, lfe: bool) {
        let ch = &mut self.chans[c];
        let st = &mut self.states[c];
        let short = ch.short();
        let pb = if short {
            &self.psy_short
        } else {
            &self.psy_long
        };
        let nb = ch.num_swb;
        ch.max_sfb = if lfe {
            self.lfe_sfb
        } else if short {
            self.bw_sfb.1
        } else {
            self.bw_sfb.0
        };
        // per window thresholds, combined per group
        let mut e = [0f32; 64];
        let mut t = [0f32; 64];
        let mut thr = [0f32; 64];
        let starts = ch.group_starts();
        ch.thr = [0.0; NB];
        ch.tonal = [0.0; NB];
        for g in 0..ch.groups {
            for b in 0..nb {
                ch.thr[g * 16 + b] = f32::MAX;
            }
            for w in starts[g]..starts[g + 1] {
                let x = &ch.x[w * 128..];
                let lines = if short { 128 } else { 1024 };
                let span: usize = if short { 8 } else { 16 };
                for b in 0..nb {
                    let (lo, hi) = (usize::from(ch.swb[b]), usize::from(ch.swb[b + 1]));
                    e[b] = x[lo..hi].iter().map(|v| v * v).sum();
                    // flatness over at least `span` lines around the band
                    let extra = span.saturating_sub(hi - lo) / 2;
                    let (tl, th) = (lo.saturating_sub(extra), (hi + extra).min(lines));
                    t[b] = tonality(&x[tl..th]);
                }
                pb.thresholds(&e[..nb], &t[..nb], &mut thr[..nb]);
                for b in 0..nb {
                    let i = g * 16 + b;
                    ch.thr[i] = ch.thr[i].min(thr[b]);
                    ch.tonal[i] += t[b] / f32::from(ch.group_len[g]);
                }
            }
            for b in 0..nb {
                ch.thr[g * 16 + b] *= f32::from(ch.group_len[g]);
            }
        }
        if short {
            st.prev_long = false;
        } else {
            // pre-echo control: at most +3 dB over the previous frame's threshold
            for b in 0..nb {
                let raw = ch.thr[b];
                if st.prev_long {
                    ch.thr[b] = raw.min(2.0 * st.prev_thr[b]).max(pb.ath[b]);
                }
                st.prev_thr[b] = raw;
            }
            st.prev_long = true;
        }
        // TNS (on the L/R spectrum, before quantisation)
        ch.tns = TnsEnc::default();
        if self.cfg.tns && !lfe {
            let max_order = if short {
                TNS_MAX_ORDER_SHORT
            } else {
                TNS_MAX_ORDER_LONG
            };
            let lim = tns_max_bands(self.sf_index, short).min(ch.max_sfb);
            let start_b = pb.tns_start.min(lim);
            for w in 0..ch.num_windows {
                let lo = usize::from(ch.swb[start_b]);
                let hi = usize::from(ch.swb[lim]);
                if hi < lo + 4 * max_order {
                    continue;
                }
                // spectrum weighted by the inverse band amplitude, so that the analysis sees
                // the temporal envelope rather than the spectral envelope
                let mut wx = [0f32; 1024];
                for b in start_b..lim {
                    let (bl, bh) = (usize::from(ch.swb[b]), usize::from(ch.swb[b + 1]));
                    let band = &ch.x[w * 128 + bl..w * 128 + bh];
                    let e = band.iter().map(|v| v * v).sum::<f32>() / (bh - bl) as f32;
                    let g = 1.0 / (e + 1e-3).sqrt();
                    for k in bl..bh {
                        wx[k - lo] = ch.x[w * 128 + k] * g;
                    }
                }
                let x = &mut ch.x[w * 128 + lo..w * 128 + hi];
                if let Some((order, coefs, lpc)) = tns_analyse(&wx[..hi - lo], max_order, short) {
                    tns_ma_filter(x, &lpc[..order], false);
                    ch.tns.present = true;
                    ch.tns.order[w] = order as u8;
                    ch.tns.length[w] = (nb - start_b) as u8;
                    ch.tns.coef[w] = coefs;
                }
            }
        }
        ch.band_stats();
        for b in 0..NB {
            ch.tonal[b] = ch.tonal[b].clamp(0.0, 1.0);
        }
        ch.pns = [false; NB];
        if self.cfg.pns && !short && !lfe {
            for b in pb.pns_start..ch.max_sfb {
                if ch.tonal[b] < 0.15
                    && ch.energy[b] > 2.0 * ch.thr[b]
                    && ch.swb[b + 1] - ch.swb[b] >= 8
                {
                    ch.pns[b] = true;
                }
            }
        }
    }

    /// Per-band M/S decision for a channel pair (common window) and the transform of the
    /// chosen bands. Returns the `ms_used` mask.
    fn ms_decide(&mut self, a: usize, b: usize) -> [bool; NB] {
        let mut ms = [false; NB];
        if !self.cfg.ms_stereo {
            return ms;
        }
        let (l, r) = if a < b {
            let (x, y) = self.chans.split_at_mut(b);
            (&mut x[a], &mut y[0])
        } else {
            let (x, y) = self.chans.split_at_mut(a);
            (&mut y[0], &mut x[b])
        };
        let starts = l.group_starts();
        let pe = |e: f32, t: f32, w: f32| w * 0.5 * (1.0 + e / t.max(1e-9)).log2();
        let mut any = false;
        for g in 0..l.groups {
            for sfb in 0..l.max_sfb {
                let i = g * 16 + sfb;
                if l.pns[i] || r.pns[i] {
                    continue;
                }
                let (lo, hi) = (usize::from(l.swb[sfb]), usize::from(l.swb[sfb + 1]));
                let (mut em, mut es) = (0.0f32, 0.0f32);
                for w in starts[g]..starts[g + 1] {
                    for k in w * 128 + lo..w * 128 + hi {
                        let (m, s) = (0.5 * (l.x[k] + r.x[k]), 0.5 * (l.x[k] - r.x[k]));
                        em += m * m;
                        es += s * s;
                    }
                }
                let tm = l.thr[i].min(r.thr[i]);
                let width = ((hi - lo) * (starts[g + 1] - starts[g])) as f32;
                let lr = pe(l.energy[i], l.thr[i], width) + pe(r.energy[i], r.thr[i], width);
                let msv = pe(em, tm, width) + pe(es, tm, width);
                if msv < lr * 0.95 {
                    ms[i] = true;
                    any = true;
                    for w in starts[g]..starts[g + 1] {
                        for k in w * 128 + lo..w * 128 + hi {
                            let (m, s) = (0.5 * (l.x[k] + r.x[k]), 0.5 * (l.x[k] - r.x[k]));
                            l.x[k] = m;
                            r.x[k] = s;
                        }
                    }
                    l.thr[i] = tm;
                    r.thr[i] = tm;
                }
            }
        }
        if any {
            l.band_stats();
            r.band_stats();
        }
        ms
    }

    /// Encode one frame from the buffered input (the analysis window starts at `buf[0]`).
    fn encode_frame(&mut self) -> Vec<u8> {
        // block switching decisions (one frame of look-ahead)
        let nel = self.elems.len();
        let mut plans: Vec<(u8, Vec<u8>)> = Vec::with_capacity(nel);
        for e in 0..nel {
            let el = self.elems[e];
            let chs: &[usize] = if el.kind == Kind::Cpe {
                &el.ch
            } else {
                &el.ch[..1]
            };
            let mut attacks = 0u8;
            let mut next_short = false;
            for &c in chs {
                let st = &mut self.states[c];
                let cur = st.next.unwrap_or_else(|| Self::detect_attack(&st.buf, 512));
                let nxt = Self::detect_attack(&st.buf, 1536);
                st.next = Some(nxt);
                attacks |= cur;
                next_short |= nxt != 0;
            }
            let cur_short = attacks != 0;
            let prev = self.states[chs[0]].prev_seq;
            let seq = if el.kind == Kind::Lfe {
                ONLY_LONG
            } else if cur_short || (prev == EIGHT_SHORT && next_short) || prev == LONG_START {
                EIGHT_SHORT
            } else if next_short {
                LONG_START
            } else if prev == EIGHT_SHORT {
                LONG_STOP
            } else {
                ONLY_LONG
            };
            // short windows: every attack window alone, the windows between grouped
            let groups: Vec<u8> = if seq == EIGHT_SHORT {
                let mut g: Vec<u8> = Vec::new();
                for w in 0..8 {
                    let split = w == 0 || attacks & (1 << w) != 0 || attacks & (1 << (w - 1)) != 0;
                    if split {
                        g.push(1);
                    } else if let Some(l) = g.last_mut() {
                        *l += 1;
                    }
                }
                g
            } else {
                vec![1]
            };
            for &c in chs {
                self.states[c].prev_seq = seq;
            }
            plans.push((seq, groups));
        }
        // analysis and psychoacoustics
        let mut ms_masks: Vec<[bool; NB]> = vec![[false; NB]; nel];
        for e in 0..nel {
            let el = self.elems[e];
            let (seq, ref groups) = plans[e];
            let groups = groups.clone();
            let chs: Vec<usize> = if el.kind == Kind::Cpe {
                el.ch.to_vec()
            } else {
                vec![el.ch[0]]
            };
            for &c in &chs {
                self.analyse(c, seq, &groups);
                self.psy(c, el.kind == Kind::Lfe);
            }
            if el.kind == Kind::Cpe {
                ms_masks[e] = self.ms_decide(el.ch[0], el.ch[1]);
            }
        }
        // bit budget
        let r_star = 0.5 * self.max_reservoir;
        let any_short = plans.iter().any(|p| p.0 == EIGHT_SHORT);
        let max_bits = (self.mean_bits + self.reservoir).min(6144.0 * self.cfg.channels as f64);
        let target = if any_short {
            self.mean_bits + 0.7 * self.reservoir
        } else {
            self.mean_bits + 0.25 * (self.reservoir - r_star)
        }
        .clamp(0.5 * self.mean_bits, max_bits);
        // rate loop: smallest threshold offset whose frame fits the target
        // (bracketed around the previous frame's offset, then bisected)
        const O_MIN: f32 = -24.0;
        const O_MAX: f32 = 60.0;
        let fits = |enc: &mut Self, o: f32| enc.count_frame(o, &ms_masks) as f64 <= target;
        let (mut lo, mut hi) = (
            (self.last_offset - 2.0).max(O_MIN),
            (self.last_offset + 2.0).min(O_MAX),
        );
        let mut best;
        while !fits(self, hi) && hi < O_MAX {
            lo = hi;
            hi = (hi + 6.0).min(O_MAX);
        }
        best = hi;
        if fits(self, lo) {
            while lo > O_MIN && fits(self, lo) {
                hi = lo;
                lo = (lo - 6.0).max(O_MIN);
            }
            best = hi;
            if lo == O_MIN && fits(self, lo) {
                best = lo;
                hi = lo;
            }
        }
        for _ in 0..7 {
            if hi - lo < 0.05 {
                break;
            }
            let mid = 0.5 * (lo + hi);
            if fits(self, mid) {
                hi = mid;
                best = mid;
            } else {
                lo = mid;
            }
        }
        self.last_offset = best;
        let mut bits = self.count_frame(best, &ms_masks);
        let mut o = best;
        while bits as f64 > max_bits && o < 200.0 {
            o += 4.0;
            bits = self.count_frame(o, &ms_masks);
        }
        let mut w = BitWriter::with_capacity(bits / 8 + 16);
        self.write_frame(&mut w, &ms_masks);
        // fill to keep the reservoir from overflowing (constant rate)
        let used = w.bit_len() + 3;
        let spare = self.reservoir + self.mean_bits - used as f64 - self.max_reservoir;
        if spare > 8.0 {
            write_fill(&mut w, (spare as usize).saturating_sub(7) / 8);
        }
        w.write(7, 3); // ID_END
        w.byte_align();
        let out = w.finish();
        let used = out.len() * 8;
        self.reservoir =
            (self.reservoir + self.mean_bits - used as f64).clamp(0.0, self.max_reservoir);
        self.total_bits += used as u64;
        out
    }

    fn count_frame(&mut self, o: f32, ms: &[[bool; NB]]) -> usize {
        for e in 0..self.elems.len() {
            let el = self.elems[e];
            let n = if el.kind == Kind::Cpe { 2 } else { 1 };
            for j in 0..n {
                plan_channel(&mut self.chans[el.ch[j]], o);
            }
            if el.kind == Kind::Cpe {
                let m = self.chans[el.ch[0]]
                    .eff_max_sfb
                    .max(self.chans[el.ch[1]].eff_max_sfb);
                self.chans[el.ch[0]].eff_max_sfb = m;
                self.chans[el.ch[1]].eff_max_sfb = m;
                for j in 0..2 {
                    choose_books(&mut self.chans[el.ch[j]]);
                }
            }
        }
        let mut c = Counter::default();
        self.write_elements(&mut c, ms);
        c.len() + 3 + 7
    }

    fn write_frame(&mut self, w: &mut BitWriter, ms: &[[bool; NB]]) {
        self.write_elements(w, ms);
    }

    fn write_elements<S: Sink>(&self, s: &mut S, ms: &[[bool; NB]]) {
        if let Some(p) = &self.pce {
            s.put(5, 3);
            // the PCE's byte alignment is relative to the raw data block start
            let mut w = BitWriter::new();
            let base = s.len();
            w.write(0, (base % 8) as u32);
            p.write(&mut w, 0);
            let bits = w.bit_len() - base % 8;
            let bytes = w.finish();
            let mut r = mkff_audio_core::bits::BitReader::new(&bytes);
            let _ = r.skip(base % 8);
            let mut left = bits;
            while left > 0 {
                let n = left.min(16);
                s.put(r.read(n as u32).unwrap_or(0), n as u32);
                left -= n;
            }
        }
        for (e, el) in self.elems.iter().enumerate() {
            match el.kind {
                Kind::Sce | Kind::Lfe => {
                    s.put(if el.kind == Kind::Sce { 0 } else { 3 }, 3);
                    s.put(u32::from(el.tag), 4);
                    write_ics(s, &self.chans[el.ch[0]], false);
                }
                Kind::Cpe => {
                    s.put(1, 3);
                    s.put(u32::from(el.tag), 4);
                    s.put(1, 1); // common_window
                    let l = &self.chans[el.ch[0]];
                    let r = &self.chans[el.ch[1]];
                    write_ics_info(s, l);
                    let m = &ms[e];
                    let mut n_used = 0;
                    let mut total = 0;
                    for g in 0..l.groups {
                        for b in 0..l.eff_max_sfb {
                            total += 1;
                            n_used += usize::from(m[g * 16 + b]);
                        }
                    }
                    if n_used == 0 {
                        s.put(0, 2);
                    } else if n_used == total {
                        s.put(2, 2);
                    } else {
                        s.put(1, 2);
                        for g in 0..l.groups {
                            for b in 0..l.eff_max_sfb {
                                s.put(u32::from(m[g * 16 + b]), 1);
                            }
                        }
                    }
                    write_ics(s, l, true);
                    write_ics(s, r, true);
                }
            }
        }
    }
}

/// TNS analysis of a spectral region: Levinson-Durbin on its autocorrelation; returns the
/// order, the quantised coefficient indices (4-bit resolution) and the matching LPC filter
/// when the prediction gain is worth it.
fn tns_analyse(
    x: &[f32],
    max_order: usize,
    short: bool,
) -> Option<(usize, [i8; TNS_MAX_ORDER_LONG], [f32; TNS_MAX_ORDER_LONG])> {
    let mut r = [0f64; TNS_MAX_ORDER_LONG + 1];
    for lag in 0..=max_order {
        let mut s = 0.0f64;
        for i in lag..x.len() {
            s += f64::from(x[i]) * f64::from(x[i - lag]);
        }
        r[lag] = s;
    }
    if r[0] <= 1e-9 {
        return None;
    }
    // lag window against ill-conditioning
    for (lag, v) in r.iter_mut().enumerate().skip(1) {
        *v *= (-0.5 * (0.02 * lag as f64).powi(2)).exp();
    }
    let mut a = [0f64; TNS_MAX_ORDER_LONG + 1];
    let mut k = [0f64; TNS_MAX_ORDER_LONG];
    a[0] = 1.0;
    let mut err = r[0];
    for m in 1..=max_order {
        let mut acc = r[m];
        for i in 1..m {
            acc += a[i] * r[m - i];
        }
        let km = -acc / err;
        k[m - 1] = km;
        let prev = a;
        for i in 1..m {
            a[i] = prev[i] + km * prev[m - i];
        }
        a[m] = km;
        err *= 1.0 - km * km;
        if err <= 0.0 {
            return None;
        }
    }
    let gain = r[0] / err;
    if gain < if short { 2.0 } else { 1.4 } {
        return None;
    }
    // quantise the reflection coefficients (coef_res = 1: 4 bits)
    let iqfac = (8.0 - 0.5) / std::f64::consts::FRAC_PI_2;
    let iqfac_m = (8.0 + 0.5) / std::f64::consts::FRAC_PI_2;
    let mut idx = [0i8; TNS_MAX_ORDER_LONG];
    let mut kq = [0f64; TNS_MAX_ORDER_LONG];
    let mut order = 0;
    for i in 0..max_order {
        // the decoder's convention: its filter is y = x - Σ a y, our k follow x + Σ a x
        let kk = k[i].clamp(-0.999, 0.999);
        let asin = kk.asin();
        let q = if asin >= 0.0 {
            (asin * iqfac).round()
        } else {
            (asin * iqfac_m).round()
        };
        let q = q.clamp(-8.0, 7.0) as i8;
        idx[i] = q;
        let qf = f64::from(q);
        kq[i] = (qf / if q >= 0 { iqfac } else { iqfac_m }).sin();
        if q != 0 {
            order = i + 1;
        }
    }
    if order == 0 {
        return None;
    }
    let lpc64 = parcor_to_lpc(&kq[..order]);
    let mut lpc = [0f32; TNS_MAX_ORDER_LONG];
    for i in 0..order {
        lpc[i] = lpc64[i + 1] as f32;
    }
    Some((order, idx, lpc))
}

/// Escape codeword length for |v| ≥ 16.
#[inline]
fn esc_bits(v: i32) -> u32 {
    let n = 31 - (v as u32).leading_zeros(); // floor(log2 v) ≥ 4
    2 * (n - 4) + 5
}

/// Bits of `q` (a band region, length a multiple of 4) in codebook `cb`, or `INF` when a
/// value exceeds the codebook's range.
fn book_bits(cb: usize, q: &[i32]) -> u32 {
    let (_, bits) = spectral_codes(cb);
    let (quad, signed, lav) = BOOK_PARAMS[cb - 1];
    let mut total = 0u32;
    if quad {
        for t in q.as_chunks::<4>().0 {
            if t.iter().any(|v| v.abs() > lav) {
                return INF;
            }
            total += u32::from(bits[book_index(cb, t)]);
            if !signed {
                total += t.iter().filter(|v| **v != 0).count() as u32;
            }
        }
    } else {
        for t in q.as_chunks::<2>().0 {
            if cb != 11 && t.iter().any(|v| v.abs() > lav) {
                return INF;
            }
            total += u32::from(bits[book_index(cb, t)]);
            if !signed {
                total += t.iter().filter(|v| **v != 0).count() as u32;
            }
            if cb == 11 {
                for &v in t {
                    if v.abs() >= 16 {
                        total += esc_bits(v.abs());
                    }
                }
            }
        }
    }
    total
}

/// Smallest codebook pair able to code a band whose largest magnitude is `m`.
fn min_book(m: i32) -> usize {
    match m {
        0 => 0,
        1 => 1,
        2 => 3,
        3..=4 => 5,
        5..=7 => 7,
        8..=12 => 9,
        _ => 11,
    }
}

/// Scalefactors and quantisation of one channel for the threshold offset `o` (log2 units).
fn plan_channel(ch: &mut Chan, o: f32) {
    let mult = o.exp2();
    let starts = ch.group_starts();
    let nb = ch.max_sfb;
    ch.book = [0; NB];
    ch.sf = [0; NB];
    // 1. desired scalefactors
    for g in 0..ch.groups {
        for b in 0..nb {
            let i = g * 16 + b;
            if ch.pns[i] {
                ch.book[i] = NOISE_HCB;
                ch.sf[i] = (2.0 * ch.energy[i].max(1e-9).log2()).round() as i32;
                continue;
            }
            let e = ch.energy[i];
            let t = ch.thr[i] * mult;
            if e <= t || e <= 0.0 || ch.sqsum[i] <= 0.0 {
                continue;
            }
            let sf = (100.0 + 8.0 / 3.0 * (6.75 * t / ch.sqsum[i]).log2()).floor() as i32;
            let sfmin =
                (100.0 + 4.0 * (ch.maxabs[i].log2() - 4.0 / 3.0 * 8190.5f32.log2())).ceil() as i32;
            ch.sf[i] = sf.max(sfmin).clamp(0, 255);
            ch.book[i] = 1; // provisional: coded
        }
    }
    // 2. quantise at the desired scalefactors and drop the bands that vanish; then bring the
    //    scalefactor chain within ±60 (only ever lowering scalefactors, so no band vanishes)
    //    and quantise again
    for pass in 0..2 {
        if pass == 1 {
            fix_sf_chain(ch);
        }
        for g in 0..ch.groups {
            for b in 0..nb {
                let i = g * 16 + b;
                if ch.book[i] == 0 || ch.book[i] == NOISE_HCB {
                    continue;
                }
                let f = (-0.1875 * (ch.sf[i] - 100) as f32).exp2();
                let (lo, hi) = (usize::from(ch.swb[b]), usize::from(ch.swb[b + 1]));
                let mut m = 0;
                for w in starts[g]..starts[g + 1] {
                    for k in w * 128 + lo..w * 128 + hi {
                        let a = ((ch.xa[k] * f + 0.4054) as i32).min(8191);
                        ch.q[k] = if ch.x[k] < 0.0 { -a } else { a };
                        m = m.max(a);
                    }
                }
                ch.book[i] = min_book(m).max(usize::from(pass == 1)) as u8;
            }
        }
    }
    // zero bands must hold zeros
    for g in 0..ch.groups {
        for b in 0..ch.num_swb {
            let i = g * 16 + b;
            if b >= nb || ch.book[i] == 0 || ch.book[i] == NOISE_HCB {
                let (lo, hi) = (usize::from(ch.swb[b]), usize::from(ch.swb[b + 1]));
                for w in starts[g]..starts[g + 1] {
                    ch.q[w * 128 + lo..w * 128 + hi].fill(0);
                }
                if b >= nb {
                    ch.book[i] = 0;
                }
            }
        }
    }
    // global gain and the noise energy chain
    ch.global_gain = 100;
    'outer: for g in 0..ch.groups {
        for b in 0..nb {
            let i = g * 16 + b;
            if ch.book[i] != 0 && ch.book[i] != NOISE_HCB {
                ch.global_gain = ch.sf[i];
                break 'outer;
            }
        }
    }
    let mut prev: Option<i32> = None;
    for g in 0..ch.groups {
        for b in 0..nb {
            let i = g * 16 + b;
            if ch.book[i] == NOISE_HCB {
                let v = match prev {
                    None => ch.sf[i].clamp(ch.global_gain - 90 - 256, ch.global_gain - 90 + 255),
                    Some(p) => ch.sf[i].clamp(p - 60, p + 60),
                };
                ch.sf[i] = v;
                prev = Some(v);
            }
        }
    }
    // trailing zero bands (kept when TNS needs the full range)
    let mut last = 0;
    for g in 0..ch.groups {
        for b in 0..nb {
            if ch.book[g * 16 + b] != 0 {
                last = last.max(b + 1);
            }
        }
    }
    ch.eff_max_sfb = if ch.tns.present { nb } else { last };
    choose_books(ch);
}

/// Lower scalefactors so that consecutive coded bands differ by at most 60.
fn fix_sf_chain(ch: &mut Chan) {
    let mut idx: Vec<usize> = Vec::with_capacity(NB);
    for g in 0..ch.groups {
        for b in 0..ch.max_sfb {
            let i = g * 16 + b;
            if ch.book[i] != 0 && ch.book[i] != NOISE_HCB {
                idx.push(i);
            }
        }
    }
    for j in (1..idx.len()).rev() {
        let (a, b) = (idx[j - 1], idx[j]);
        if ch.sf[a] > ch.sf[b] + 60 {
            ch.sf[a] = ch.sf[b] + 60;
        }
    }
    for j in 1..idx.len() {
        let (a, b) = (idx[j - 1], idx[j]);
        if ch.sf[b] > ch.sf[a] + 60 {
            ch.sf[b] = ch.sf[a] + 60;
        }
        if ch.sf[b] < ch.sf[a] - 60 {
            ch.sf[b] = ch.sf[a] - 60;
        }
    }
}

/// Codebook per band with section merging: dynamic programming over (band, codebook) with
/// the section header cost on every codebook change.
#[allow(clippy::needless_range_loop)]
fn choose_books(ch: &mut Chan) {
    let starts = ch.group_starts();
    let hdr = if ch.short() { 4 + 3 } else { 4 + 5 };
    let n = ch.eff_max_sfb;
    let mut back = [[0u8; 16]; 64];
    for g in 0..ch.groups {
        let mut prev = [INF; 16];
        let mut best_prev = (0u32, 0u8);
        for b in 0..n {
            let i = g * 16 + b;
            let mut cost = [INF; 16];
            match ch.book[i] {
                0 => cost[0] = 0,
                NOISE_HCB => cost[usize::from(NOISE_HCB)] = 0,
                mb => {
                    let (lo, hi) = (usize::from(ch.swb[b]), usize::from(ch.swb[b + 1]));
                    let first = usize::from(mb);
                    let last = (first + 3).min(11);
                    for cb in first..=last {
                        let mut t = 0;
                        for w in starts[g]..starts[g + 1] {
                            t += book_bits(cb, &ch.q[w * 128 + lo..w * 128 + hi]);
                            if t >= INF {
                                break;
                            }
                        }
                        cost[cb] = t.min(INF);
                    }
                }
            }
            let mut cur = [INF; 16];
            let mut best = (INF, 0u8);
            for c in 0..16 {
                if cost[c] >= INF {
                    continue;
                }
                let (stay, switch) = if b == 0 {
                    (INF, hdr)
                } else {
                    (prev[c], best_prev.0.saturating_add(hdr))
                };
                let (base, from) = if stay <= switch {
                    (stay, c as u8)
                } else {
                    (switch, best_prev.1)
                };
                cur[c] = base.saturating_add(cost[c]);
                back[b][c] = from;
                if cur[c] < best.0 {
                    best = (cur[c], c as u8);
                }
            }
            prev = cur;
            best_prev = best;
        }
        // backtrack
        let mut c = best_prev.1;
        for b in (0..n).rev() {
            ch.book[g * 16 + b] = c;
            c = back[b][usize::from(c)];
        }
    }
}

fn write_ics_info<S: Sink>(s: &mut S, ch: &Chan) {
    s.put(0, 1);
    s.put(u32::from(ch.seq), 2);
    s.put(0, 1); // sine window
    if ch.short() {
        s.put(ch.eff_max_sfb as u32, 4);
        let mut grouping = 0u32;
        let mut w = 0;
        for g in 0..ch.groups {
            for j in 0..usize::from(ch.group_len[g]) {
                if w > 0 {
                    grouping = (grouping << 1) | u32::from(j > 0);
                }
                w += 1;
            }
        }
        s.put(grouping, 7);
    } else {
        s.put(ch.eff_max_sfb as u32, 6);
        s.put(0, 1); // predictor_data_present
    }
}

fn write_ics<S: Sink>(s: &mut S, ch: &Chan, common_window: bool) {
    s.put(ch.global_gain as u32 & 0xFF, 8);
    if !common_window {
        write_ics_info(s, ch);
    }
    let n = ch.eff_max_sfb;
    // section data
    let (len_bits, esc) = if ch.short() { (3, 7) } else { (5, 31) };
    for g in 0..ch.groups {
        let mut b = 0;
        while b < n {
            let cb = ch.book[g * 16 + b];
            let mut e = b + 1;
            while e < n && ch.book[g * 16 + e] == cb {
                e += 1;
            }
            s.put(u32::from(cb), 4);
            let mut len = e - b;
            while len >= esc {
                s.put(esc as u32, len_bits);
                len -= esc;
            }
            s.put(len as u32, len_bits);
            b = e;
        }
    }
    // scalefactors
    let mut last_sf = ch.global_gain;
    let mut last_noise: Option<i32> = None;
    for g in 0..ch.groups {
        for b in 0..n {
            let i = g * 16 + b;
            match ch.book[i] {
                0 => {}
                NOISE_HCB => {
                    match last_noise {
                        None => s.put((ch.sf[i] - (ch.global_gain - 90) + 256) as u32, 9),
                        Some(p) => {
                            let d = (ch.sf[i] - p + 60) as usize;
                            s.put(SF_CODES[d], u32::from(SF_BITS[d]));
                        }
                    }
                    last_noise = Some(ch.sf[i]);
                }
                _ => {
                    let d = (ch.sf[i] - last_sf + 60) as usize;
                    s.put(SF_CODES[d], u32::from(SF_BITS[d]));
                    last_sf = ch.sf[i];
                }
            }
        }
    }
    s.put(0, 1); // pulse_data_present
    s.put(u32::from(ch.tns.present), 1);
    if ch.tns.present {
        let short = ch.short();
        for w in 0..ch.num_windows {
            let order = usize::from(ch.tns.order[w]);
            s.put(u32::from(order > 0), if short { 1 } else { 2 });
            if order == 0 {
                continue;
            }
            s.put(1, 1); // coef_res: 4 bits
            s.put(u32::from(ch.tns.length[w]), if short { 4 } else { 6 });
            s.put(order as u32, if short { 3 } else { 5 });
            s.put(0, 1); // direction: upwards
            s.put(0, 1); // coef_compress
            for &c in &ch.tns.coef[w][..order] {
                s.put((c as u32) & 0xF, 4);
            }
        }
    }
    s.put(0, 1); // gain_control_data_present
                 // spectral data
    let starts = ch.group_starts();
    for g in 0..ch.groups {
        for b in 0..n {
            let cb = usize::from(ch.book[g * 16 + b]);
            if cb == 0 || cb > 11 {
                continue;
            }
            let (codes, bits) = spectral_codes(cb);
            let (quad, signed, _) = BOOK_PARAMS[cb - 1];
            let (lo, hi) = (usize::from(ch.swb[b]), usize::from(ch.swb[b + 1]));
            let step = if quad { 4 } else { 2 };
            for w in starts[g]..starts[g + 1] {
                for t in ch.q[w * 128 + lo..w * 128 + hi].chunks_exact(step) {
                    let idx = book_index(cb, t);
                    s.put(u32::from(codes[idx]), u32::from(bits[idx]));
                    if !signed {
                        for &v in t {
                            if v != 0 {
                                s.put(u32::from(v < 0), 1);
                            }
                        }
                    }
                    if cb == 11 {
                        for &v in t {
                            let a = v.abs();
                            if a >= 16 {
                                let n = 31 - (a as u32).leading_zeros(); // ≥ 4
                                for _ in 4..n {
                                    s.put(1, 1);
                                }
                                s.put(0, 1);
                                s.put(a as u32 - (1 << n), n);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Fill elements carrying `bytes` bytes of padding.
fn write_fill(w: &mut BitWriter, mut bytes: usize) {
    while bytes > 0 {
        // a FIL element holds up to 14 + 255 payload bytes
        let n = bytes.min(269);
        w.write(6, 3);
        if n < 15 {
            w.write(n as u32, 4);
        } else {
            w.write(15, 4);
            w.write((n - 14) as u32, 8);
        }
        // extension_type EXT_FILL (0) and fill bits
        for i in 0..n {
            w.write(if i == 0 { 0x00 } else { 0xA5 }, 8);
        }
        bytes -= n;
    }
}

impl AacEncoder {
    /// Frames ready: the analysis window and one frame of look-ahead (+512) are buffered.
    fn ready(&self, flushing: bool) -> bool {
        let have = self.states[0].buf.len();
        if flushing {
            // packets needed: ceil(inputs / 1024) + 1
            let needed = self.inputs.div_ceil(1024) + 1;
            self.packets < needed
        } else {
            have >= 2048 + 1536
        }
    }

    fn emit(&mut self, out: &mut Vec<Packet>, flushing: bool) {
        while self.ready(flushing) {
            if flushing {
                for st in &mut self.states {
                    if st.buf.len() < 2048 + 1536 {
                        st.buf.resize(2048 + 1536, 0.0);
                    }
                }
            }
            let data = self.encode_frame();
            out.push(Packet {
                data,
                pts: self.packets as i64 * 1024,
                duration: 1024,
            });
            self.packets += 1;
            for st in &mut self.states {
                st.buf.drain(..1024);
            }
        }
    }
}

impl Encoder for AacEncoder {
    fn encode(&mut self, input: &[&[f32]]) -> Result<Vec<Packet>> {
        if input.len() != self.cfg.channels {
            return Err(Error::InvalidArgument(format!(
                "{} input channels, expected {}",
                input.len(),
                self.cfg.channels
            )));
        }
        let n = input[0].len();
        if input.iter().any(|c| c.len() != n) {
            return Err(Error::InvalidArgument(
                "channel slices differ in length".into(),
            ));
        }
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < n {
            let room = (2048 + 1536usize)
                .saturating_sub(self.states[0].buf.len())
                .max(1024);
            let take = room.min(n - pos);
            for (st, src) in self.states.iter_mut().zip(input) {
                st.buf.extend(src[pos..pos + take].iter().map(|v| {
                    if v.is_finite() {
                        v.clamp(-2.0, 2.0)
                    } else {
                        0.0
                    }
                }));
            }
            self.inputs += take as u64;
            pos += take;
            self.emit(&mut out, false);
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        self.emit(&mut out, true);
        Ok(out)
    }

    fn sample_rate(&self) -> u32 {
        self.cfg.sample_rate
    }

    fn channels(&self) -> usize {
        self.cfg.channels
    }

    fn frame_size(&self) -> usize {
        1024
    }

    fn priming(&self) -> u32 {
        1024
    }

    fn config(&self) -> Vec<u8> {
        self.asc.to_bytes()
    }
}
