//! MPEG audio Layer III (MP3) encoder: constant bit rate, MPEG-1 (32, 44.1, 48 kHz),
//! MPEG-2 LSF (16, 22.05, 24 kHz) and MPEG-2.5 (8, 11.025, 12 kHz), mono, stereo and joint
//! (M/S) stereo.
//!
//! Pipeline per granule (576 samples): polyphase analysis filterbank → attack detection on the
//! subband samples (block switching with one granule of look-ahead plus one for the start
//! window) → MDCT with long / start / short / stop windows and alias reduction → masking
//! model ([`psy`]) → M/S decision per frame → rate / noise control loops ([`quant`]) with the
//! bit reservoir → Huffman coding ([`bitstream`]).

mod bitstream;
mod psy;
mod quant;

use crate::header::{
    bitrate_table, frame_length, sfreq_index, ChannelMode, FrameHeader, MpegVersion,
};
use crate::synth::Analysis;
use crate::tables::{alias_coeffs, windows, SfBands, SF_BANDS};
use crate::xing::{crc16_lame, xing_frame, XingFrameInfo, DECODER_DELAY};
use bitstream::Reservoir;
use mkff_audio_core::bits::BitWriter;
use mkff_audio_core::{Encoder, Error, Packet, Result};
use psy::{Masking, Psy};
use quant::{Layout, Quantized, Quantizer};
use std::collections::VecDeque;

/// Channel coding of the encoded stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StereoMode {
    /// Independent left / right channels.
    Stereo,
    /// Left / right or mid / side, chosen per frame.
    JointStereo,
    /// One channel (the input must be mono).
    Mono,
}

/// Encoder settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mp3EncoderConfig {
    /// 32000, 44100 or 48000 (MPEG-1); 16000, 22050 or 24000 (MPEG-2); 8000, 11025 or 12000
    /// (MPEG-2.5).
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: usize,
    /// A Layer III bit rate of the version: 32..=320 kbit/s for MPEG-1, 8..=160 for MPEG-2 /
    /// 2.5 (one of the standard values).
    pub bitrate_kbps: u32,
    pub mode: StereoMode,
    /// 0 (best, slowest) ..= 9 (fastest): iterations of the noise allocation loop and the
    /// thoroughness of the Huffman table search.
    pub quality: u8,
}

impl Default for Mp3EncoderConfig {
    /// 44.1 kHz stereo, 128 kbit/s joint stereo, quality 3.
    fn default() -> Self {
        Self {
            sample_rate: 44100,
            channels: 2,
            bitrate_kbps: 128,
            mode: StereoMode::JointStereo,
            quality: 3,
        }
    }
}

impl Mp3EncoderConfig {
    /// Settings for `channels` channels at `sample_rate` and `bitrate_kbps` (joint stereo for
    /// two channels, mono for one; quality 3).
    pub fn new(sample_rate: u32, channels: usize, bitrate_kbps: u32) -> Self {
        Self {
            sample_rate,
            channels,
            bitrate_kbps,
            mode: if channels == 1 {
                StereoMode::Mono
            } else {
                StereoMode::JointStereo
            },
            quality: 3,
        }
    }
}

/// Totals of the stream written so far, for the Xing / Info header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Audio frames output.
    pub frames: u64,
    /// Bytes of the audio frames output.
    pub bytes: u64,
    /// Input samples per channel consumed.
    pub input_samples: u64,
    /// CRC-16/ARC of all output frames (the LAME tag's music CRC).
    pub music_crc: u16,
    /// Frames coded as mid / side.
    pub ms_frames: u64,
    /// Granule-channels coded with short blocks.
    pub short_blocks: u64,
}

/// Total encoder + decoder delay of this encoder: one granule of MDCT look-back plus the
/// polyphase filterbank pair (481 samples).
const TOTAL_DELAY: u32 = 576 + 481;

/// A subband granule: 18 time slots × 32 subbands.
type SbGranule = [[f32; 32]; 18];

/// Layer III encoder. Implements [`Encoder`]: planar `f32` in, one packet per frame out.
///
/// The output frames carry no Xing / Info header; after [`Encoder::flush`], use
/// [`Mp3Encoder::xing_frame`] to build one (with the LAME delay / padding fields) and put it
/// before the first frame for gapless playback and fast seeking.
pub struct Mp3Encoder {
    cfg: Mp3EncoderConfig,
    version: MpegVersion,
    nch: usize,
    ngr: usize,
    joint: bool,
    bands: &'static SfBands,
    analysis: Vec<Analysis>,
    /// Input not yet run through the filterbank, per channel.
    inbuf: Vec<Vec<f32>>,
    /// Subband granules, per channel; front = granule `sb_base`.
    sb: Vec<VecDeque<Box<SbGranule>>>,
    sb_base: i64,
    /// Attack flags per channel per subband granule (front = `sb_base`).
    attack: Vec<VecDeque<bool>>,
    /// Energy history of the attack detector (last 6 segments) per channel.
    seg_hist: Vec<[[f32; 6]; 4]>,
    /// Short-block decision of the previous block, per channel.
    prev_short: Vec<bool>,
    /// Next block (granule) to encode.
    next_block: i64,
    granules_analyzed: i64,
    psy: Psy,
    quant: Quantizer,
    reservoir: Reservoir,
    /// Long-block MDCT matrices (window and 1/9 folded in) for block types 0, 1, 3: `[k][n]`.
    mdct_long: Box<[[f32; 18 * 36]; 4]>,
    mdct_short: [f32; 6 * 12],
    lowpass_long: usize,
    /// Quantizer band layouts: long, short.
    layouts: [Layout; 2],
    lowpass_short: usize,
    pad_acc: u64,
    stats: StreamStats,
    out: Vec<Packet>,
    /// Running average of the perceptual entropy (bit allocation).
    pe_avg: f32,
    flushed: bool,
}

/// Lowpass frequency for a bit rate per channel (Hz).
fn lowpass_for(kbps_per_ch: f64, rate: u32) -> f64 {
    const PTS: [(f64, f64); 8] = [
        (8.0, 2800.0),
        (16.0, 5500.0),
        (24.0, 7500.0),
        (32.0, 11000.0),
        (48.0, 14500.0),
        (64.0, 16500.0),
        (96.0, 18500.0),
        (128.0, 20000.0),
    ];
    let mut f = PTS[PTS.len() - 1].1;
    if kbps_per_ch <= PTS[0].0 {
        f = PTS[0].1;
    } else {
        for w in PTS.windows(2) {
            if kbps_per_ch <= w[1].0 {
                let t = (kbps_per_ch - w[0].0) / (w[1].0 - w[0].0);
                f = w[0].1 + t * (w[1].1 - w[0].1);
                break;
            }
        }
    }
    f.min(f64::from(rate) * 0.5 * 0.96)
}

impl Mp3Encoder {
    /// Create an encoder; fails on an unsupported rate, bit rate, channel count or mode.
    pub fn new(cfg: Mp3EncoderConfig) -> Result<Self> {
        let version = match cfg.sample_rate {
            32000 | 44100 | 48000 => MpegVersion::Mpeg1,
            16000 | 22050 | 24000 => MpegVersion::Mpeg2,
            8000 | 11025 | 12000 => MpegVersion::Mpeg25,
            r => {
                return Err(Error::InvalidArgument(format!(
                    "unsupported sample rate {r}"
                )))
            }
        };
        if !(1..=2).contains(&cfg.channels) {
            return Err(Error::InvalidArgument(format!(
                "unsupported channel count {}",
                cfg.channels
            )));
        }
        if cfg.channels == 2 && cfg.mode == StereoMode::Mono {
            return Err(Error::InvalidArgument(
                "mono mode needs one input channel".into(),
            ));
        }
        if !bitrate_table(version, 3)[1..]
            .iter()
            .any(|&b| u32::from(b) == cfg.bitrate_kbps)
        {
            return Err(Error::InvalidArgument(format!(
                "unsupported bit rate {} kbit/s for {} Hz",
                cfg.bitrate_kbps, cfg.sample_rate
            )));
        }
        let nch = cfg.channels;
        let lsf = version.is_lsf();
        let bands = &SF_BANDS[sfreq_index(cfg.sample_rate)];
        let quality = cfg.quality.min(9);
        // MDCT matrices
        let pi = std::f64::consts::PI;
        let w = windows();
        let mut mdct_long = Box::new([[0f32; 18 * 36]; 4]);
        for (bt, m) in mdct_long.iter_mut().enumerate() {
            for k in 0..18 {
                for n in 0..36 {
                    let c = (pi / 72.0 * (2 * n + 19) as f64 * (2 * k + 1) as f64).cos();
                    m[k * 36 + n] = (c / 9.0) as f32 * w.long[bt][n];
                }
            }
        }
        let mut mdct_short = [0f32; 6 * 12];
        for k in 0..6 {
            for n in 0..12 {
                let c = (pi / 24.0 * (2 * n + 7) as f64 * (2 * k + 1) as f64).cos();
                mdct_short[k * 12 + n] = (c / 3.0) as f32 * w.short[n];
            }
        }
        let kbps_ch = f64::from(cfg.bitrate_kbps) / nch as f64
            * if nch == 2 && cfg.mode == StereoMode::JointStereo {
                1.25
            } else {
                1.0
            };
        let lowpass = lowpass_for(kbps_ch, cfg.sample_rate);
        let nyq = f64::from(cfg.sample_rate) / 2.0;
        let lowpass_long = ((lowpass / nyq) * 576.0).ceil().min(576.0) as usize;
        let lowpass_short = ((lowpass / nyq) * 192.0).ceil().min(192.0) as usize;
        let mut enc = Self {
            cfg: Mp3EncoderConfig { quality, ..cfg },
            version,
            nch,
            ngr: if lsf { 1 } else { 2 },
            joint: nch == 2 && cfg.mode == StereoMode::JointStereo,
            bands,
            analysis: (0..nch).map(|_| Analysis::default()).collect(),
            inbuf: vec![Vec::with_capacity(4096); nch],
            sb: vec![VecDeque::new(); nch],
            sb_base: -1,
            attack: vec![VecDeque::new(); nch],
            seg_hist: vec![[[0.0; 6]; 4]; nch],
            prev_short: vec![false; nch],
            next_block: 0,
            granules_analyzed: 0,
            psy: Psy::new(cfg.sample_rate, nch, 1.0, lowpass),
            quant: Quantizer::new(lsf, quality),
            reservoir: Reservoir::default(),
            mdct_long,
            mdct_short,
            lowpass_long,
            layouts: [Layout::new(bands, false), Layout::new(bands, true)],
            lowpass_short,
            pad_acc: 0,
            stats: StreamStats::default(),
            out: Vec::new(),
            pe_avg: 0.0,
            flushed: false,
        };
        // granule -1 is silence
        for c in 0..nch {
            enc.sb[c].push_back(Box::new([[0.0; 32]; 18]));
            enc.attack[c].push_back(false);
        }
        let full_scale = enc.calibrate_full_scale();
        enc.psy = Psy::new(cfg.sample_rate, nch, full_scale, lowpass);
        Ok(enc)
    }

    /// MDCT energy of a full-scale 1 kHz sine (calibrates the absolute hearing threshold).
    fn calibrate_full_scale(&mut self) -> f64 {
        let mut a = Analysis::default();
        let rate = f64::from(self.cfg.sample_rate);
        let mut prev;
        let mut cur = [[0f32; 32]; 18];
        let mut energy = 0f64;
        for g in 0..4 {
            prev = cur;
            for (s, slot) in cur.iter_mut().enumerate() {
                let mut buf = [0f32; 32];
                for (i, b) in buf.iter_mut().enumerate() {
                    let t = (g * 576 + s * 32 + i) as f64 / rate;
                    *b = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32;
                }
                a.process(&buf, slot);
            }
            if g == 3 {
                let mut xr = [0f32; 576];
                self.mdct_long_block(&prev, &cur, 0, &mut xr);
                energy = xr.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
            }
        }
        energy
    }

    /// The stream totals so far (complete after [`Encoder::flush`]).
    pub fn stats(&self) -> StreamStats {
        self.stats
    }

    /// A header describing the encoded stream (as the first audio frame looks, without padding).
    pub fn stream_header(&self) -> FrameHeader {
        let mut h = FrameHeader {
            version: self.version,
            layer: 3,
            protection: false,
            bitrate_kbps: self.cfg.bitrate_kbps,
            sample_rate: self.cfg.sample_rate,
            padding: false,
            channel_mode: match (self.nch, self.joint) {
                (1, _) => ChannelMode::Mono,
                (_, true) => ChannelMode::JointStereo,
                _ => ChannelMode::Stereo,
            },
            mode_extension: 0,
            frame_len_bytes: 0,
            samples_per_frame: self.frame_size(),
            channels: self.nch,
            copyright: false,
            original: true,
            emphasis: 0,
        };
        h.frame_len_bytes = frame_length(h.version, 3, h.bitrate_kbps, h.sample_rate, false);
        h
    }

    /// What an Info header for the stream written so far contains (LAME delay / padding,
    /// frame and byte counts, music CRC).
    pub fn xing_info(&self) -> XingFrameInfo {
        let fs = self.frame_size() as u64;
        let total = self.stats.frames * fs;
        let enc_delay = u64::from(TOTAL_DELAY - DECODER_DELAY);
        let padding = total.saturating_sub(enc_delay + self.stats.input_samples);
        let nyq = f64::from(self.cfg.sample_rate) / 2.0;
        XingFrameInfo {
            frames: self.stats.frames.min(u64::from(u32::MAX)) as u32,
            audio_bytes: self.stats.bytes,
            encoder_delay: enc_delay as u32,
            padding: padding.min(4095) as u32,
            cbr: true,
            bitrate_kbps: self.cfg.bitrate_kbps,
            lowpass_hz: (self.lowpass_long as f64 / 576.0 * nyq) as u32,
            music_crc: self.stats.music_crc,
            toc: None,
            ..XingFrameInfo::default()
        }
    }

    /// An Info header frame (Xing layout plus the LAME extension) for the stream written so
    /// far; call after [`Encoder::flush`] and store it before the first audio frame.
    pub fn xing_frame(&self) -> Vec<u8> {
        xing_frame(&self.stream_header(), &self.xing_info())
    }

    /// Run complete granules of buffered input through the filterbank.
    fn analyze(&mut self) {
        while self.inbuf[0].len() >= 576 {
            for c in 0..self.nch {
                let mut g: Box<SbGranule> = Box::new([[0.0; 32]; 18]);
                for (s, slot) in g.iter_mut().enumerate() {
                    self.analysis[c].process(&self.inbuf[c][s * 32..s * 32 + 32], slot);
                }
                self.inbuf[c].drain(..576);
                let atk = self.detect_attack(c, &g);
                self.sb[c].push_back(g);
                self.attack[c].push_back(atk);
            }
            self.granules_analyzed += 1;
        }
    }

    /// Transient detection on the subband samples: six segments of three slots; in each of
    /// four subband groups (above the two lowest subbands) the segment energy is compared with
    /// the group's preceding six segments, so steady tones in one region do not hide an onset
    /// in another.
    fn detect_attack(&mut self, c: usize, g: &SbGranule) -> bool {
        const GROUPS: [(usize, usize); 4] = [(2, 6), (6, 12), (12, 20), (20, 32)];
        let mut attack = false;
        for seg in 0..6 {
            for (k, &(a, b)) in GROUPS.iter().enumerate() {
                let mut e = 0f32;
                for slot in &g[seg * 3..seg * 3 + 3] {
                    for &v in &slot[a..b] {
                        e += v * v;
                    }
                }
                let hist = &mut self.seg_hist[c][k];
                let avg = hist.iter().sum::<f32>() / 6.0;
                if e > 8.0 * avg && e > 1e-4 {
                    attack = true;
                }
                hist.rotate_left(1);
                hist[5] = e;
            }
        }
        attack
    }

    /// Short-block decision for block `j` (needs attack flags up to `j + 1`).
    fn raw_short(&self, c: usize, j: i64) -> bool {
        let at = |g: i64| -> bool {
            let i = g - self.sb_base;
            if i < 0 {
                return false;
            }
            let any = |ch: usize| self.attack[ch].get(i as usize).copied().unwrap_or(false);
            if self.joint {
                any(0) || any(1)
            } else {
                any(c)
            }
        };
        at(j - 1) || at(j)
    }

    /// Long MDCT of one subband granule pair into `xr` (with alias reduction).
    fn mdct_long_block(&self, prev: &SbGranule, cur: &SbGranule, bt: usize, xr: &mut [f32; 576]) {
        let m = &self.mdct_long[bt];
        let mut s = [0f32; 36];
        for sb in 0..32 {
            for n in 0..18 {
                s[n] = prev[n][sb];
                s[18 + n] = cur[n][sb];
            }
            if sb & 1 == 1 {
                for n in (1..36).step_by(2) {
                    s[n] = -s[n];
                }
            }
            for k in 0..18 {
                let row = &m[k * 36..k * 36 + 36];
                let mut acc = 0f32;
                for n in 0..36 {
                    acc += row[n] * s[n];
                }
                xr[sb * 18 + k] = acc;
            }
        }
        let (cs, ca) = alias_coeffs();
        for sb in 1..32 {
            for i in 0..8 {
                let u = sb * 18 - 1 - i;
                let d = sb * 18 + i;
                let (a, b) = (xr[u], xr[d]);
                xr[u] = a * cs[i] + b * ca[i];
                xr[d] = b * cs[i] - a * ca[i];
            }
        }
    }

    /// Short MDCTs (three windows of 192 lines).
    fn mdct_short_block(&self, prev: &SbGranule, cur: &SbGranule, out: &mut [[f32; 192]; 3]) {
        let mut s = [0f32; 36];
        for sb in 0..32 {
            for n in 0..18 {
                s[n] = prev[n][sb];
                s[18 + n] = cur[n][sb];
            }
            if sb & 1 == 1 {
                for n in (1..36).step_by(2) {
                    s[n] = -s[n];
                }
            }
            for (w, o) in out.iter_mut().enumerate() {
                let z = &s[6 + 6 * w..18 + 6 * w];
                for k in 0..6 {
                    let row = &self.mdct_short[k * 12..k * 12 + 12];
                    let mut acc = 0f32;
                    for n in 0..12 {
                        acc += row[n] * z[n];
                    }
                    o[sb * 6 + k] = acc;
                }
            }
        }
    }

    /// Spectrum of block `j` for channel `c` in bitstream order, plus the masking.
    fn spectrum(
        &mut self,
        c: usize,
        j: i64,
        bt: u8,
        xr: &mut [f32; 576],
        natural_short: &mut [[f32; 192]; 3],
    ) {
        let i = (j - self.sb_base) as usize;
        let (prev, cur) = (&self.sb[c][i - 1], &self.sb[c][i]);
        if bt == 2 {
            self.mdct_short_block(prev, cur, natural_short);
            for w in natural_short.iter_mut() {
                w[self.lowpass_short..].fill(0.0);
            }
            let b = self.bands;
            for sfb in 0..13 {
                let a = b.short[sfb] as usize;
                let width = b.short[sfb + 1] as usize - a;
                for (w, x) in natural_short.iter().enumerate() {
                    let o = 3 * a + w * width;
                    xr[o..o + width].copy_from_slice(&x[a..a + width]);
                }
            }
        } else {
            self.mdct_long_block(prev, cur, bt as usize, xr);
            xr[self.lowpass_long..].fill(0.0);
        }
    }

    /// Encode the next frame (blocks `next_block ..` + ngr).
    fn encode_frame(&mut self) {
        let ngr = self.ngr;
        let nch = self.nch;
        let j0 = self.next_block;
        // block types
        let mut bt = [[0u8; 2]; 2];
        for gr in 0..ngr {
            let j = j0 + gr as i64;
            for c in 0..nch {
                let s_prev = self.prev_short[c];
                let s_cur = self.raw_short(c, j) || (s_prev && self.raw_short(c, j + 1));
                let s_next = self.raw_short(c, j + 1) || (s_cur && self.raw_short(c, j + 2));
                bt[gr][c] = if s_cur {
                    2
                } else if s_next {
                    1
                } else if s_prev {
                    3
                } else {
                    0
                };
                self.prev_short[c] = s_cur;
            }
        }
        // spectra and masking
        let mut xr = vec![[[0f32; 576]; 2]; ngr];
        let mut mask = vec![[Masking::default(); 2]; ngr];
        let mut nat = [[0f32; 192]; 3];
        for gr in 0..ngr {
            for c in 0..nch {
                let j = j0 + gr as i64;
                self.spectrum(c, j, bt[gr][c], &mut xr[gr][c], &mut nat);
                mask[gr][c] = if bt[gr][c] == 2 {
                    self.psy.short(c, &nat, self.bands)
                } else {
                    let mut x = xr[gr][c];
                    x[self.lowpass_long..].fill(0.0);
                    self.psy.long(c, &x, self.bands)
                };
            }
        }
        // M/S decision for the frame
        let mut ms = false;
        if self.joint {
            let mut pe_lr = 0f32;
            let mut pe_ms = 0f32;
            let mut mid = vec![[[0f32; 576]; 2]; ngr];
            for gr in 0..ngr {
                let (l, r) = (&xr[gr][0], &xr[gr][1]);
                for i in 0..576 {
                    mid[gr][0][i] = (l[i] + r[i]) * std::f32::consts::FRAC_1_SQRT_2;
                    mid[gr][1][i] = (l[i] - r[i]) * std::f32::consts::FRAC_1_SQRT_2;
                }
                let layout = &self.layouts[usize::from(bt[gr][0] == 2)];
                for (b, band) in layout.bands.iter().enumerate() {
                    let t = mask[gr][0].xmin[b].min(mask[gr][1].xmin[b]).max(1e-30);
                    let w = (band.end - band.start) as f32;
                    let pe = |e: f32, t: f32| if e > t { w * 0.5 * (e / t).log2() } else { 0.0 };
                    let em: f32 = mid[gr][0][band.start..band.end].iter().map(|v| v * v).sum();
                    let es: f32 = mid[gr][1][band.start..band.end].iter().map(|v| v * v).sum();
                    pe_ms += pe(em, t) + pe(es, t);
                    pe_lr += pe(mask[gr][0].energy[b], mask[gr][0].xmin[b].max(1e-30))
                        + pe(mask[gr][1].energy[b], mask[gr][1].xmin[b].max(1e-30));
                }
            }
            if pe_ms < pe_lr {
                ms = true;
                for gr in 0..ngr {
                    xr[gr] = mid[gr];
                    for b in 0..39 {
                        let t = mask[gr][0].xmin[b].min(mask[gr][1].xmin[b]);
                        mask[gr][0].xmin[b] = t;
                        mask[gr][1].xmin[b] = t;
                    }
                }
            }
        }
        self.stats.ms_frames += u64::from(ms);
        self.stats.short_blocks += bt
            .iter()
            .take(ngr)
            .flat_map(|g| g.iter().take(nch))
            .filter(|&&b| b == 2)
            .count() as u64;
        // frame size and bit budget
        let lsf = self.version.is_lsf();
        let factor: u64 = if lsf { 72 } else { 144 };
        let num = factor * u64::from(self.cfg.bitrate_kbps) * 1000;
        let rate = u64::from(self.cfg.sample_rate);
        self.pad_acc += num % rate;
        let padding = self.pad_acc >= rate;
        if padding {
            self.pad_acc -= rate;
        }
        let mut header = self.stream_header();
        header.padding = padding;
        header.mode_extension = if ms { 2 } else { 0 };
        let frame_len = frame_length(
            self.version,
            3,
            self.cfg.bitrate_kbps,
            self.cfg.sample_rate,
            padding,
        );
        header.frame_len_bytes = frame_len;
        let side_len = header.side_info_len();
        let main_bytes = frame_len - 4 - side_len;
        let max_res = if lsf { 255 } else { 511 };
        let mut emitted = Vec::new();
        self.reservoir.limit(max_res, &mut emitted);
        let res_bytes = self.reservoir.available();
        let mean_gr = (main_bytes * 8 / ngr) as i64;
        let mut res_bits = (res_bytes * 8) as i64;
        let max_res_bits = (max_res * 8) as i64;

        let mut results: Vec<Vec<Quantized>> = Vec::with_capacity(ngr);
        for gr in 0..ngr {
            // what the granule would like: perceptual entropy relative to its running mean
            let pe: Vec<f32> = (0..nch).map(|c| mask[gr][c].pe.max(1.0)).collect();
            let pe_sum: f32 = pe.iter().sum();
            self.pe_avg = if self.pe_avg == 0.0 {
                pe_sum
            } else {
                0.95 * self.pe_avg + 0.05 * pe_sum
            };
            let ratio = (pe_sum / self.pe_avg.max(1.0)).clamp(0.5, 3.0);
            let short_any = (0..nch).any(|c| bt[gr][c] == 2);
            let want = (mean_gr as f32 * if short_any { ratio.max(1.3) } else { ratio }) as i64;
            let last = gr + 1 == ngr;
            let extra_cap = (res_bits as f32 * if last { 0.85 } else { 0.6 }) as i64;
            let extra = (want - mean_gr).clamp(0, extra_cap.max(0));
            let mut budget = mean_gr + extra;
            // bits that would overflow the reservoir must be spent here
            let must = (res_bits + mean_gr - max_res_bits).max(0);
            budget = budget.max(must).min(res_bits + mean_gr);
            let budget_ch: Vec<i64> = if nch == 2 {
                let share0 = (pe[0] / pe_sum).clamp(0.2, 0.8);
                let b0 = (budget as f32 * share0) as i64;
                vec![b0, budget - b0]
            } else {
                vec![budget]
            };
            let mut gr_res = Vec::with_capacity(nch);
            let mut used = 0i64;
            for c in 0..nch {
                let layout = &self.layouts[usize::from(bt[gr][c] == 2)];
                let maxb = budget_ch[c].clamp(0, 4095) as u32;
                let minb = if must > 0 {
                    ((must as f32) / nch as f32) as u32
                } else {
                    0
                }
                .min(maxb);
                let q = self.quant.quantize_granule(
                    &xr[gr][c],
                    layout,
                    self.bands,
                    bt[gr][c],
                    &mask[gr][c].xmin,
                    maxb,
                    minb,
                );
                used += i64::from(q.info.part2_3_length);
                gr_res.push(q);
            }
            res_bits = res_bits + mean_gr - used;
            results.push(gr_res);
        }
        // scale factor sharing between the granules (MPEG-1, long blocks)
        let mut scfsi = [[false; 4]; 2];
        if !lsf {
            const GROUPS: [(usize, usize); 4] = [(0, 6), (6, 11), (11, 16), (16, 21)];
            for c in 0..nch {
                let (g0, g1) = (&results[0][c], &results[1][c]);
                if g0.info.block_type == 2 || g1.info.block_type == 2 {
                    continue;
                }
                let mut saved = 0u32;
                for (k, &(a, b)) in GROUPS.iter().enumerate() {
                    if g0.sf_l[a..b] == g1.sf_l[a..b] {
                        scfsi[c][k] = true;
                        let slen = if k < 2 { g1.slen[0] } else { g1.slen[1] };
                        saved += (b - a) as u32 * u32::from(slen);
                    }
                }
                let g1 = &mut results[1][c];
                g1.part2_bits -= saved;
                g1.info.part2_3_length -= saved;
            }
        }
        // main data
        let total_bits: u32 = results
            .iter()
            .flatten()
            .map(|q| q.info.part2_3_length)
            .sum();
        let mut w = BitWriter::with_capacity(total_bits as usize / 8 + 8);
        for (gr, g) in results.iter().enumerate() {
            for (c, q) in g.iter().enumerate() {
                let start = w.bit_len();
                bitstream::write_scalefactors(&mut w, q, lsf, gr, &scfsi[c]);
                bitstream::write_huffman(&mut w, q, self.bands);
                debug_assert_eq!(
                    (w.bit_len() - start) as u32,
                    q.info.part2_3_length,
                    "part2_3_length mismatch"
                );
            }
        }
        let main = w.finish();
        let mut head = BitWriter::with_capacity(4 + side_len);
        head.write_bytes(&header.to_bytes());
        bitstream::write_side_info(&mut head, lsf, nch, res_bytes, &scfsi, &results);
        let head = head.finish();
        self.reservoir.push(&head, frame_len, &main, &mut emitted);
        self.queue(emitted);
        self.next_block += ngr as i64;
        // drop subband granules no longer needed (block j needs j - 1)
        while self.sb_base < self.next_block - 1 {
            for c in 0..nch {
                self.sb[c].pop_front();
                self.attack[c].pop_front();
            }
            self.sb_base += 1;
        }
    }

    fn queue(&mut self, frames: Vec<(u64, Vec<u8>)>) {
        let fs = self.frame_size() as u32;
        for (idx, data) in frames {
            self.stats.frames += 1;
            self.stats.bytes += data.len() as u64;
            self.stats.music_crc = crc16_lame(self.stats.music_crc, &data);
            self.out.push(Packet {
                data,
                pts: idx as i64 * i64::from(fs),
                duration: fs,
            });
        }
    }

    /// Encode every frame whose look-ahead is available.
    fn pump(&mut self) {
        self.analyze();
        // block j needs subband granules up to j + 2 (attack look-ahead)
        while self.granules_analyzed > self.next_block + self.ngr as i64 + 1 {
            self.encode_frame();
        }
    }
}

impl Encoder for Mp3Encoder {
    fn encode(&mut self, input: &[&[f32]]) -> Result<Vec<Packet>> {
        if self.flushed {
            return Err(Error::InvalidArgument("encoder already flushed".into()));
        }
        if input.len() != self.nch {
            return Err(Error::InvalidArgument(format!(
                "expected {} channels, got {}",
                self.nch,
                input.len()
            )));
        }
        let n = input[0].len();
        if input.iter().any(|c| c.len() != n) {
            return Err(Error::InvalidArgument("channel lengths differ".into()));
        }
        // feed in pieces so the input buffer stays small
        let mut pos = 0;
        while pos < n {
            let take = (n - pos).min(576 * 4);
            for (buf, ch) in self.inbuf.iter_mut().zip(input) {
                buf.extend(ch[pos..pos + take].iter().map(|&v| {
                    if v.is_finite() {
                        v.clamp(-4.0, 4.0)
                    } else {
                        0.0
                    }
                }));
            }
            pos += take;
            self.pump();
        }
        self.stats.input_samples += n as u64;
        Ok(std::mem::take(&mut self.out))
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        if self.flushed {
            return Ok(Vec::new());
        }
        self.flushed = true;
        // granules needed so that every input sample reaches the decoded output
        let need = (self.stats.input_samples + u64::from(TOTAL_DELAY)).div_ceil(576) as i64;
        let ngr = self.ngr as i64;
        let need = (need + ngr - 1) / ngr * ngr;
        while self.next_block < need {
            let fill = 576 - self.inbuf[0].len() % 576;
            for buf in &mut self.inbuf {
                buf.resize(buf.len() + fill, 0.0);
            }
            self.pump();
        }
        let mut emitted = Vec::new();
        self.reservoir.flush(&mut emitted);
        self.queue(emitted);
        Ok(std::mem::take(&mut self.out))
    }

    fn sample_rate(&self) -> u32 {
        self.cfg.sample_rate
    }

    fn channels(&self) -> usize {
        self.nch
    }

    fn frame_size(&self) -> usize {
        if self.version.is_lsf() {
            576
        } else {
            1152
        }
    }

    fn priming(&self) -> u32 {
        TOTAL_DELAY
    }

    fn config(&self) -> Vec<u8> {
        Vec::new()
    }
}
