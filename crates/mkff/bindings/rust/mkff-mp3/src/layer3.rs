//! Layer III decoding (ISO/IEC 11172-3 2.4.3.4 and ISO/IEC 13818-3 2.4.3.4 for the low
//! sampling frequencies).

use crate::header::{sfreq_index, ChannelMode, FrameHeader};
use crate::huffman::{tables, TABLE_INFO};
use crate::synth::Synth;
use crate::tables::{alias_coeffs, pow43, windows, SfBands, NR_OF_SFB, PRETAB, SF_BANDS, SLEN};
use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};

/// Side information of one granule of one channel.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GranuleInfo {
    pub part2_3_length: u32,
    pub big_values: u32,
    pub global_gain: u32,
    pub scalefac_compress: u32,
    /// 0 normal, 1 start, 2 short, 3 stop.
    pub block_type: u8,
    pub mixed: bool,
    pub table_select: [u8; 3],
    pub subblock_gain: [u8; 3],
    pub region0_count: u32,
    pub region1_count: u32,
    pub preflag: bool,
    pub scalefac_scale: bool,
    pub count1_table_b: bool,
}

/// Layer III side information.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SideInfo {
    pub main_data_begin: usize,
    pub private_bits: u32,
    pub scfsi: [[bool; 4]; 2],
    pub gr: [[GranuleInfo; 2]; 2],
}

/// Parse the side information (`data` starts right after the header and optional CRC).
pub(crate) fn parse_side_info(h: &FrameHeader, data: &[u8]) -> Result<SideInfo> {
    let mut r = BitReader::new(data);
    let lsf = h.version.is_lsf();
    let nch = h.channels;
    let mut si = SideInfo::default();
    if lsf {
        si.main_data_begin = r.read(8)? as usize;
        si.private_bits = r.read(if nch == 1 { 1 } else { 2 })?;
    } else {
        si.main_data_begin = r.read(9)? as usize;
        si.private_bits = r.read(if nch == 1 { 5 } else { 3 })?;
        for ch in 0..nch {
            for b in 0..4 {
                si.scfsi[ch][b] = r.read_bit()?;
            }
        }
    }
    let ngr = if lsf { 1 } else { 2 };
    for gr in 0..ngr {
        for ch in 0..nch {
            let g = &mut si.gr[gr][ch];
            g.part2_3_length = r.read(12)?;
            g.big_values = r.read(9)?;
            if g.big_values > 288 {
                return Err(Error::invalid("big_values > 288"));
            }
            g.global_gain = r.read(8)?;
            g.scalefac_compress = r.read(if lsf { 9 } else { 4 })?;
            if r.read_bit()? {
                g.block_type = r.read(2)? as u8;
                if g.block_type == 0 {
                    return Err(Error::invalid("window switching with block type 0"));
                }
                g.mixed = r.read_bit()?;
                for t in 0..2 {
                    g.table_select[t] = r.read(5)? as u8;
                }
                for w in 0..3 {
                    g.subblock_gain[w] = r.read(3)? as u8;
                }
                g.region0_count = if g.block_type == 2 && !g.mixed { 8 } else { 7 };
                g.region1_count = 36;
            } else {
                for t in 0..3 {
                    g.table_select[t] = r.read(5)? as u8;
                }
                g.region0_count = r.read(4)?;
                g.region1_count = r.read(3)?;
            }
            if !lsf {
                g.preflag = r.read_bit()?;
            }
            g.scalefac_scale = r.read_bit()?;
            g.count1_table_b = r.read_bit()?;
        }
    }
    Ok(si)
}

/// Start lines of big-values regions 1 and 2 (before clipping to `big_values`).
///
/// With window switching, region 0 ends after `region0_count + 1` = 8 long bands, or after 9
/// short-band windows (3 short bands) for pure short blocks; region 2 is empty.
pub(crate) fn region_bounds(g: &GranuleInfo, bands: &SfBands) -> (usize, usize) {
    if g.block_type != 0 {
        let r1 = if g.block_type == 2 && !g.mixed {
            3 * bands.short[3] as usize
        } else {
            bands.long[8] as usize
        };
        (r1, 576)
    } else {
        let a = (g.region0_count as usize + 1).min(22);
        let b = (g.region0_count as usize + g.region1_count as usize + 2).min(22);
        (bands.long[a] as usize, bands.long[b] as usize)
    }
}

/// Per-channel scale factors of the current granule.
#[derive(Clone, Copy, Default)]
struct ScaleFactors {
    l: [u8; 22],
    s: [[u8; 3]; 13],
    /// LSF intensity stereo: the illegal (maximum) position per band.
    is_max_l: [u8; 22],
    is_max_s: [[u8; 3]; 13],
    /// LSF: `preflag` implied by `scalefac_compress`.
    preflag: bool,
}

/// Layer III decoder state.
pub(crate) struct Layer3 {
    reservoir: Vec<u8>,
    sf: [ScaleFactors; 2],
    ix: [[i32; 576]; 2],
    xr: [[f32; 576]; 2],
    /// Number of lines up to the last non-zero one, per channel.
    nz: [usize; 2],
    overlap: [[[f32; 18]; 32]; 2],
    /// Time samples of one granule, `[slot][subband]`.
    tmp: [[f32; 32]; 18],
    imdct_long: Box<[[f32; 18 * 36]; 4]>,
    imdct_short: [f32; 6 * 12],
    gain: Vec<f32>,
}

const GAIN_OFFSET: i32 = 420;

impl Layer3 {
    pub(crate) fn new() -> Self {
        let pi = std::f64::consts::PI;
        let w = windows();
        let mut imdct_long = Box::new([[0f32; 18 * 36]; 4]);
        for (bt, m) in imdct_long.iter_mut().enumerate() {
            for i in 0..36 {
                for k in 0..18 {
                    let c = (pi / 72.0 * (2 * i + 1 + 18) as f64 * (2 * k + 1) as f64).cos();
                    m[i * 18 + k] = c as f32 * w.long[bt][i];
                }
            }
        }
        let mut imdct_short = [0f32; 6 * 12];
        for i in 0..12 {
            for k in 0..6 {
                let c = (pi / 24.0 * (2 * i + 1 + 6) as f64 * (2 * k + 1) as f64).cos();
                imdct_short[i * 6 + k] = c as f32 * w.short[i];
            }
        }
        let gain = (0..GAIN_OFFSET + 128)
            .map(|q| 2f64.powf(f64::from(q - GAIN_OFFSET) / 4.0) as f32)
            .collect();
        Self {
            reservoir: Vec::with_capacity(4096),
            sf: [ScaleFactors::default(); 2],
            ix: [[0; 576]; 2],
            xr: [[0.0; 576]; 2],
            nz: [0; 2],
            overlap: [[[0.0; 18]; 32]; 2],
            tmp: [[0.0; 32]; 18],
            imdct_long,
            imdct_short,
            gain,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.reservoir.clear();
        self.overlap = [[[0.0; 18]; 32]; 2];
        self.sf = [ScaleFactors::default(); 2];
    }

    /// Decode one complete frame into `pcm` (`samples_per_frame` per channel). Returns `false`
    /// when the frame's main data was not available (after a seek), in which case the frame
    /// decodes to the overlap tail only.
    pub(crate) fn decode_frame(
        &mut self,
        h: &FrameHeader,
        frame: &[u8],
        crc_check: bool,
        synth: &mut [Synth; 2],
        pcm: &mut [Vec<f32>; 2],
    ) -> Result<bool> {
        let mut pos = 4;
        if h.protection {
            pos += 2;
        }
        let side_len = h.side_info_len();
        let side = frame
            .get(pos..pos + side_len)
            .ok_or_else(|| Error::invalid("frame shorter than its side information"))?;
        if crc_check && h.protection {
            let mut data = Vec::with_capacity(2 + side_len);
            data.extend_from_slice(&frame[2..4]);
            data.extend_from_slice(side);
            let crc = mkff_audio_core::crc::crc16(0xFFFF, &data);
            if crc != u16::from_be_bytes([frame[4], frame[5]]) {
                return Err(Error::invalid("layer III CRC mismatch"));
            }
        }
        let si = parse_side_info(h, side)?;
        let main = frame.get(pos + side_len..).unwrap_or(&[]);

        let have = si.main_data_begin <= self.reservoir.len();
        let start = self.reservoir.len().wrapping_sub(si.main_data_begin);
        self.reservoir.extend_from_slice(main);

        let lsf = h.version.is_lsf();
        let ngr = if lsf { 1 } else { 2 };
        let nch = h.channels;
        let sfreq = sfreq_index(h.sample_rate);
        let ms = h.channel_mode == ChannelMode::JointStereo && h.mode_extension & 2 != 0;
        let is = h.channel_mode == ChannelMode::JointStereo && h.mode_extension & 1 != 0;

        // the reader borrows the reservoir while the other fields are mutated
        let reservoir = std::mem::take(&mut self.reservoir);
        let data = if have { &reservoir[start..] } else { &[][..] };
        let mut r = BitReader::new(data);
        for gr in 0..ngr {
            for ch in 0..nch {
                let g = si.gr[gr][ch];
                if have {
                    let part_start = r.position();
                    let end = part_start + g.part2_3_length as usize;
                    let ok = if lsf {
                        self.read_scalefactors_lsf(&mut r, &g, ch, is)
                    } else {
                        self.read_scalefactors_v1(&mut r, &g, ch, gr, &si.scfsi[ch])
                    };
                    let ok = ok.is_ok() && r.position() <= end;
                    if ok {
                        self.read_huffman(&mut r, &g, ch, sfreq, end.min(data.len() * 8));
                    } else {
                        self.ix[ch] = [0; 576];
                        self.nz[ch] = 0;
                    }
                    r.seek(end);
                    self.requantize(&g, ch, sfreq);
                } else {
                    self.xr[ch] = [0.0; 576];
                    self.ix[ch] = [0; 576];
                    self.nz[ch] = 0;
                }
            }
            if nch == 2 && (ms || is) {
                self.stereo(&si.gr[gr], sfreq, ms, is, lsf);
            }
            for ch in 0..nch {
                let g = si.gr[gr][ch];
                self.hybrid(&g, ch, sfreq);
                let out = &mut pcm[ch][gr * 576..gr * 576 + 576];
                for (slot, chunk) in self.tmp.iter().zip(out.as_chunks_mut::<32>().0) {
                    synth[ch].process(slot, chunk);
                }
            }
        }
        self.reservoir = reservoir;
        // keep what the next frames can reference (main_data_begin is at most 511 bytes)
        let keep = 1024;
        if self.reservoir.len() > keep {
            let cut = self.reservoir.len() - keep;
            self.reservoir.drain(..cut);
        }
        Ok(have)
    }

    fn read_scalefactors_v1(
        &mut self,
        r: &mut BitReader,
        g: &GranuleInfo,
        ch: usize,
        gr: usize,
        scfsi: &[bool; 4],
    ) -> Result<()> {
        let (slen1, slen2) = SLEN[g.scalefac_compress as usize & 15];
        let (slen1, slen2) = (u32::from(slen1), u32::from(slen2));
        let sf = &mut self.sf[ch];
        if g.block_type == 2 {
            if g.mixed {
                for sfb in 0..8 {
                    sf.l[sfb] = r.read(slen1)? as u8;
                }
                for sfb in 3..12 {
                    let n = if sfb < 6 { slen1 } else { slen2 };
                    for w in 0..3 {
                        sf.s[sfb][w] = r.read(n)? as u8;
                    }
                }
            } else {
                for sfb in 0..12 {
                    let n = if sfb < 6 { slen1 } else { slen2 };
                    for w in 0..3 {
                        sf.s[sfb][w] = r.read(n)? as u8;
                    }
                }
            }
            sf.s[12] = [0; 3];
        } else {
            const GROUPS: [(usize, usize); 4] = [(0, 6), (6, 11), (11, 16), (16, 21)];
            for (k, &(a, b)) in GROUPS.iter().enumerate() {
                if gr == 1 && scfsi[k] {
                    continue;
                }
                let n = if k < 2 { slen1 } else { slen2 };
                for sfb in a..b {
                    sf.l[sfb] = r.read(n)? as u8;
                }
            }
            sf.l[21] = 0;
        }
        Ok(())
    }

    fn read_scalefactors_lsf(
        &mut self,
        r: &mut BitReader,
        g: &GranuleInfo,
        ch: usize,
        is: bool,
    ) -> Result<()> {
        let sfc = g.scalefac_compress;
        let slen;
        let mut preflag = false;
        let table;
        if is && ch == 1 {
            let isc = sfc >> 1;
            if isc < 180 {
                slen = [isc / 36, (isc % 36) / 6, (isc % 36) % 6, 0];
                table = 3;
            } else if isc < 244 {
                let v = isc - 180;
                slen = [(v % 64) >> 4, (v % 16) >> 2, v % 4, 0];
                table = 4;
            } else {
                let v = isc - 244;
                slen = [v / 3, v % 3, 0, 0];
                table = 5;
            }
        } else if sfc < 400 {
            slen = [(sfc >> 4) / 5, (sfc >> 4) % 5, (sfc % 16) >> 2, sfc % 4];
            table = 0;
        } else if sfc < 500 {
            let v = sfc - 400;
            slen = [(v >> 2) / 5, (v >> 2) % 5, v % 4, 0];
            table = 1;
        } else {
            let v = sfc - 500;
            slen = [v / 3, v % 3, 0, 0];
            preflag = true;
            table = 2;
        }
        let kind = if g.block_type == 2 {
            if g.mixed {
                2
            } else {
                1
            }
        } else {
            0
        };
        let nr = NR_OF_SFB[table][kind];
        let sf = &mut self.sf[ch];
        *sf = ScaleFactors::default();
        let mut idx = 0usize;
        for k in 0..4 {
            let n = slen[k];
            let max = ((1u32 << n) - 1) as u8;
            for _ in 0..nr[k] {
                let v = r.read(n)? as u8;
                match kind {
                    0 => {
                        if idx < 21 {
                            sf.l[idx] = v;
                            sf.is_max_l[idx] = max;
                        }
                    }
                    1 => {
                        if idx < 36 {
                            sf.s[idx / 3][idx % 3] = v;
                            sf.is_max_s[idx / 3][idx % 3] = max;
                        }
                    }
                    _ => {
                        if idx < 6 {
                            sf.l[idx] = v;
                            sf.is_max_l[idx] = max;
                        } else if idx - 6 < 27 {
                            let j = idx - 6;
                            sf.s[3 + j / 3][j % 3] = v;
                            sf.is_max_s[3 + j / 3][j % 3] = max;
                        }
                    }
                }
                idx += 1;
            }
        }
        // the last band carries no scale factor; intensity stereo reuses the previous band's
        sf.is_max_l[21] = sf.is_max_l[20];
        sf.is_max_s[12] = sf.is_max_s[11];
        sf.preflag = preflag;
        Ok(())
    }

    fn read_huffman(
        &mut self,
        r: &mut BitReader,
        g: &GranuleInfo,
        ch: usize,
        sfreq: usize,
        end: usize,
    ) {
        let t = tables();
        let bands = &SF_BANDS[sfreq];
        let ix = &mut self.ix[ch];
        let bv_end = (g.big_values as usize * 2).min(576);
        let (r1, r2) = region_bounds(g, bands);
        let r1 = r1.min(bv_end);
        let r2 = r2.min(bv_end).max(r1);
        let mut i = 0usize;
        let mut nz = 0usize;
        let mut ok = true;
        'regions: for (region_end, &ts) in [r1, r2, bv_end].iter().zip(&g.table_select) {
            let region_end = *region_end;
            if i >= region_end {
                continue;
            }
            let (tidx, linbits) = TABLE_INFO[ts as usize & 31];
            if tidx == 0 {
                if ts != 0 {
                    // tables 4 and 14 do not exist
                    ok = false;
                    break 'regions;
                }
                ix[i..region_end].fill(0);
                i = region_end;
                continue;
            }
            let dec = &t.dec[tidx as usize - 1];
            let linbits = u32::from(linbits);
            while i < region_end {
                let Some(v) = dec.decode(r) else {
                    ok = false;
                    break 'regions;
                };
                let mut x = i32::from(v >> 4);
                let mut y = i32::from(v & 15);
                if x != 0 {
                    if linbits > 0 && x == 15 {
                        x += r.read(linbits).unwrap_or(0) as i32;
                    }
                    if r.read_bit().unwrap_or(false) {
                        x = -x;
                    }
                }
                if y != 0 {
                    if linbits > 0 && y == 15 {
                        y += r.read(linbits).unwrap_or(0) as i32;
                    }
                    if r.read_bit().unwrap_or(false) {
                        y = -y;
                    }
                }
                ix[i] = x;
                ix[i + 1] = y;
                if x != 0 || y != 0 {
                    nz = i + 2;
                }
                i += 2;
                if r.position() > end {
                    ok = false;
                    break 'regions;
                }
            }
        }
        if ok {
            let quad = if g.count1_table_b {
                &t.quad_b
            } else {
                &t.quad_a
            };
            while i + 4 <= 576 && r.position() < end {
                let Some(v) = quad.decode(r) else { break };
                let mut q = [
                    ((v >> 3) & 1) as i32,
                    ((v >> 2) & 1) as i32,
                    ((v >> 1) & 1) as i32,
                    (v & 1) as i32,
                ];
                for x in &mut q {
                    if *x != 0 && r.read_bit().unwrap_or(false) {
                        *x = -1;
                    }
                }
                if r.position() > end {
                    // the last quadruple overran part2_3_length: discard it
                    break;
                }
                ix[i..i + 4].copy_from_slice(&q);
                if q != [0; 4] {
                    nz = i + 4;
                }
                i += 4;
            }
        } else {
            // corrupt data: keep what decoded before the error
            i = i.min(576);
        }
        ix[i.min(576)..].fill(0);
        self.nz[ch] = nz.min(576);
    }

    fn requantize(&mut self, g: &GranuleInfo, ch: usize, sfreq: usize) {
        let p43 = pow43();
        let bands = &SF_BANDS[sfreq];
        let sf = &self.sf[ch];
        let ix = &self.ix[ch];
        let xr = &mut self.xr[ch];
        let nz = self.nz[ch];
        let base = g.global_gain as i32 - 210 + GAIN_OFFSET;
        let shift = 1 + u32::from(g.scalefac_scale);
        let preflag = g.preflag || sf.preflag;
        let gain = |q: i32| -> f32 { self.gain.get(q.max(0) as usize).copied().unwrap_or(0.0) };
        let deq = |v: i32, m: f32| -> f32 {
            let a = p43[(v.unsigned_abs() as usize).min(8206)] * m;
            if v < 0 {
                -a
            } else {
                a
            }
        };
        xr[nz..].fill(0.0);
        let long_end = if g.block_type == 2 {
            if g.mixed {
                // 8 long bands (MPEG-1) / 6 (LSF), i.e. the first 36 lines
                3 * bands.short[3] as usize
            } else {
                0
            }
        } else {
            576
        };
        let long_end = long_end.min(nz);
        let mut sfb = 0;
        while sfb < 22 && (bands.long[sfb] as usize) < long_end {
            let a = bands.long[sfb] as usize;
            let b = (bands.long[sfb + 1] as usize).min(long_end);
            let s = if sfb < 21 { i32::from(sf.l[sfb]) } else { 0 };
            let p = if preflag { i32::from(PRETAB[sfb]) } else { 0 };
            let m = gain(base - ((s + p) << shift));
            for i in a..b {
                xr[i] = deq(ix[i], m);
            }
            sfb += 1;
        }
        if g.block_type == 2 {
            let first = if g.mixed { 3 } else { 0 };
            for sfb in first..13 {
                let a = bands.short[sfb] as usize;
                let width = bands.short[sfb + 1] as usize - a;
                for w in 0..3 {
                    let start = 3 * a + w * width;
                    if start >= nz {
                        break;
                    }
                    let s = i32::from(sf.s[sfb][w]);
                    let m = gain(base - 8 * i32::from(g.subblock_gain[w]) - (s << shift));
                    for i in start..(start + width).min(nz) {
                        xr[i] = deq(ix[i], m);
                    }
                }
            }
        }
    }

    /// M/S and intensity stereo (before reordering, in bitstream line order).
    fn stereo(&mut self, g: &[GranuleInfo; 2], sfreq: usize, ms: bool, is: bool, lsf: bool) {
        let bands = &SF_BANDS[sfreq];
        let gr = &g[1];
        const INV_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;
        // per line: true when intensity coded
        let mut is_lines = [false; 576];
        let mut is_any = false;
        if is {
            let ix_r = &self.ix[1];
            let sf_r = self.sf[1];
            let intensity_scale = g[1].scalefac_compress & 1;
            let apply = |lines: std::ops::Range<usize>,
                         pos: u8,
                         max: u8,
                         xr: &mut [[f32; 576]; 2],
                         is_lines: &mut [bool; 576]| {
                let lines = lines.start.min(576)..lines.end.min(576);
                let (kl, kr) = if lsf {
                    if pos == max {
                        return;
                    }
                    let io: f32 = if intensity_scale == 0 {
                        0.840_896_4
                    } else {
                        INV_SQRT2
                    };
                    if pos == 0 {
                        (1.0, 1.0)
                    } else if pos & 1 == 1 {
                        (io.powi(i32::from(pos).div_euclid(2) + 1), 1.0)
                    } else {
                        (1.0, io.powi(i32::from(pos) / 2))
                    }
                } else {
                    if pos >= 7 {
                        return;
                    }
                    let ratio = (f64::from(pos) * std::f64::consts::PI / 12.0).tan();
                    if pos == 6 {
                        (1.0, 0.0)
                    } else {
                        ((ratio / (1.0 + ratio)) as f32, (1.0 / (1.0 + ratio)) as f32)
                    }
                };
                for i in lines {
                    let v = xr[0][i];
                    xr[0][i] = v * kl;
                    xr[1][i] = v * kr;
                    is_lines[i] = true;
                }
            };
            if gr.block_type == 2 {
                let first = if gr.mixed { 3 } else { 0 };
                let mut any_short_nz = false;
                for w in 0..3 {
                    // highest band of this window with non-zero right-channel values
                    let mut start_sfb = first;
                    for sfb in (first..13).rev() {
                        let a = bands.short[sfb] as usize;
                        let width = bands.short[sfb + 1] as usize - a;
                        let s = 3 * a + w * width;
                        if ix_r[s..s + width].iter().any(|&v| v != 0) {
                            start_sfb = sfb + 1;
                            any_short_nz = true;
                            break;
                        }
                    }
                    for sfb in start_sfb..13 {
                        let a = bands.short[sfb] as usize;
                        let width = bands.short[sfb + 1] as usize - a;
                        let s = 3 * a + w * width;
                        let src = if sfb == 12 { 11 } else { sfb };
                        let (pos, max) = if lsf {
                            (sf_r.s[src][w], sf_r.is_max_s[src][w])
                        } else {
                            (sf_r.s[src][w], 7)
                        };
                        apply(s..s + width, pos, max, &mut self.xr, &mut is_lines);
                        is_any = true;
                    }
                }
                if gr.mixed && !any_short_nz {
                    let long_end = 3 * bands.short[3] as usize;
                    let last = ix_r[..long_end].iter().rposition(|&v| v != 0);
                    let mut sfb = 0;
                    if let Some(l) = last {
                        while (bands.long[sfb + 1] as usize) <= l {
                            sfb += 1;
                        }
                        sfb += 1;
                    }
                    while (bands.long[sfb] as usize) < long_end {
                        let a = bands.long[sfb] as usize;
                        let b = (bands.long[sfb + 1] as usize).min(long_end);
                        let (pos, max) = (sf_r.l[sfb], if lsf { sf_r.is_max_l[sfb] } else { 7 });
                        apply(a..b, pos, max, &mut self.xr, &mut is_lines);
                        sfb += 1;
                    }
                }
            } else {
                let last = ix_r.iter().rposition(|&v| v != 0);
                let mut sfb = 0;
                if let Some(l) = last {
                    while (bands.long[sfb + 1] as usize) <= l {
                        sfb += 1;
                    }
                    sfb += 1;
                }
                while sfb < 22 {
                    let a = bands.long[sfb] as usize;
                    let b = bands.long[sfb + 1] as usize;
                    let src = if sfb == 21 { 20 } else { sfb };
                    let (pos, max) = if lsf {
                        (sf_r.l[src], sf_r.is_max_l[src])
                    } else {
                        (sf_r.l[src], 7)
                    };
                    apply(a..b, pos, max, &mut self.xr, &mut is_lines);
                    is_any = true;
                    sfb += 1;
                }
            }
        }
        let n = self.nz[0].max(self.nz[1]);
        let [l, r] = &mut self.xr;
        if ms {
            for i in 0..n {
                if is_any && is_lines[i] {
                    continue;
                }
                let (m, s) = (l[i], r[i]);
                l[i] = (m + s) * INV_SQRT2;
                r[i] = (m - s) * INV_SQRT2;
            }
        }
        // intensity coding fills the right channel up to the left channel's last line
        self.nz = [n; 2];
    }

    /// Reorder, alias reduction, IMDCT with overlap-add and frequency inversion into `tmp`.
    fn hybrid(&mut self, g: &GranuleInfo, ch: usize, sfreq: usize) {
        let bands = &SF_BANDS[sfreq];
        let xr = &mut self.xr[ch];
        let nz = self.nz[ch];
        // reorder short-block lines into [subband][window][6]
        if g.block_type == 2 {
            let mut buf = [0f32; 576];
            let first = if g.mixed { 3 } else { 0 };
            let base = 3 * bands.short[first] as usize;
            buf[..base].copy_from_slice(&xr[..base]);
            for sfb in first..13 {
                let a = bands.short[sfb] as usize;
                let width = bands.short[sfb + 1] as usize - a;
                for w in 0..3 {
                    let s = 3 * a + w * width;
                    for k in 0..width {
                        let f = a + k;
                        buf[(f / 6) * 18 + w * 6 + f % 6] = xr[s + k];
                    }
                }
            }
            xr.copy_from_slice(&buf);
        }
        // subbands that hold non-zero data (after reordering short blocks fill whole subbands)
        let nsb = if g.block_type == 2 {
            32
        } else {
            nz.div_ceil(18).min(32)
        };
        // alias reduction
        let (cs, ca) = alias_coeffs();
        let alias_sb = if g.block_type == 2 {
            if g.mixed {
                1
            } else {
                0
            }
        } else {
            (nsb + 1).min(32) - 1
        };
        for sb in 1..=alias_sb {
            for i in 0..8 {
                let u = sb * 18 - 1 - i;
                let d = sb * 18 + i;
                let (bu, bd) = (xr[u], xr[d]);
                xr[u] = bu * cs[i] - bd * ca[i];
                xr[d] = bd * cs[i] + bu * ca[i];
            }
        }
        let nsb = if g.block_type == 2 {
            32
        } else {
            (alias_sb + 1).max(nsb).min(32)
        };
        let overlap = &mut self.overlap[ch];
        let mut out36 = [0f32; 36];
        for sb in 0..32 {
            let x = &xr[sb * 18..sb * 18 + 18];
            let prev = &mut overlap[sb];
            let zero = sb >= nsb || x.iter().all(|&v| v == 0.0);
            if zero {
                for i in 0..18 {
                    self.tmp[i][sb] = prev[i];
                }
                *prev = [0.0; 18];
            } else {
                let long = g.block_type != 2 || (g.mixed && sb < 2);
                if long {
                    let bt = if g.block_type == 2 {
                        0
                    } else {
                        g.block_type as usize
                    };
                    let m = &self.imdct_long[bt];
                    for (i, o) in out36.iter_mut().enumerate() {
                        let row = &m[i * 18..i * 18 + 18];
                        let mut acc = 0f32;
                        for k in 0..18 {
                            acc += row[k] * x[k];
                        }
                        *o = acc;
                    }
                } else {
                    out36 = [0.0; 36];
                    for w in 0..3 {
                        let xw = &x[w * 6..w * 6 + 6];
                        for i in 0..12 {
                            let row = &self.imdct_short[i * 6..i * 6 + 6];
                            let mut acc = 0f32;
                            for k in 0..6 {
                                acc += row[k] * xw[k];
                            }
                            out36[6 + 6 * w + i] += acc;
                        }
                    }
                }
                for i in 0..18 {
                    self.tmp[i][sb] = out36[i] + prev[i];
                    prev[i] = out36[18 + i];
                }
            }
            if sb & 1 == 1 {
                for i in (1..18).step_by(2) {
                    self.tmp[i][sb] = -self.tmp[i][sb];
                }
            }
        }
    }
}
