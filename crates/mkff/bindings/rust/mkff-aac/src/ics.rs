//! `individual_channel_stream` parsing (14496-3 4.4.2.7) and the spectral tools of the decoder:
//! noiseless decoding, inverse quantisation, PNS, M/S, intensity stereo and TNS.

use crate::huffman::books;
use crate::tables::*;
use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};

/// `ics_info()`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IcsInfo {
    pub seq: u8,
    pub shape: u8,
    pub max_sfb: usize,
    pub num_windows: usize,
    pub groups: usize,
    pub group_len: [u8; 8],
    pub swb: &'static [u16],
    /// Scalefactor bands of the window (`num_swb`).
    pub num_swb: usize,
}

impl Default for IcsInfo {
    fn default() -> Self {
        Self {
            seq: ONLY_LONG,
            shape: 0,
            max_sfb: 0,
            num_windows: 1,
            groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb: swb_offset_long(4),
            num_swb: 49,
        }
    }
}

impl IcsInfo {
    pub fn is_short(&self) -> bool {
        self.seq == EIGHT_SHORT
    }

    /// Parse `ics_info()`.
    pub fn parse(br: &mut BitReader, sf_index: u8) -> Result<Self> {
        if br.read_bit()? {
            return Err(Error::invalid("ics_reserved_bit set"));
        }
        let seq = br.read(2)? as u8;
        let shape = br.read(1)? as u8;
        let mut info = IcsInfo {
            seq,
            shape,
            ..Default::default()
        };
        if seq == EIGHT_SHORT {
            info.max_sfb = br.read(4)? as usize;
            let grouping = br.read(7)?;
            info.num_windows = 8;
            info.swb = swb_offset_short(sf_index);
            info.num_swb = info.swb.len() - 1;
            info.groups = 1;
            info.group_len = [1, 0, 0, 0, 0, 0, 0, 0];
            for i in 0..7 {
                if grouping & (1 << (6 - i)) != 0 {
                    info.group_len[info.groups - 1] += 1;
                } else {
                    info.groups += 1;
                    info.group_len[info.groups - 1] = 1;
                }
            }
        } else {
            info.max_sfb = br.read(6)? as usize;
            info.swb = swb_offset_long(sf_index);
            info.num_swb = info.swb.len() - 1;
            if br.read_bit()? {
                return Err(Error::unsupported(
                    "prediction / LTP data (AAC Main / LTP object types)",
                ));
            }
        }
        if info.max_sfb > info.num_swb {
            return Err(Error::invalid(format!(
                "max_sfb {} > {}",
                info.max_sfb, info.num_swb
            )));
        }
        Ok(info)
    }
}

/// One TNS filter.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TnsFilter {
    pub length: u8,
    pub order: u8,
    pub direction: bool,
    /// LPC coefficients `a[1..=order]`.
    pub lpc: [f32; TNS_MAX_ORDER_LONG],
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tns {
    pub present: bool,
    pub n_filt: [u8; 8],
    pub filt: [[TnsFilter; 3]; 8],
}

/// Decoded data of one channel of an element.
pub(crate) struct ChannelData {
    pub ics: IcsInfo,
    pub global_gain: i32,
    /// `[group * 16 + sfb]` (long windows: `[sfb]`).
    pub band_type: [u8; 128],
    /// Scalefactor, intensity position or noise energy of each band.
    pub sf: [i32; 128],
    pub quant: Box<[i32; 1024]>,
    pub coef: Box<[f32; 1024]>,
    pub tns: Tns,
    pub pulse_present: bool,
}

impl Default for ChannelData {
    fn default() -> Self {
        Self {
            ics: IcsInfo::default(),
            global_gain: 0,
            band_type: [0; 128],
            sf: [0; 128],
            quant: Box::new([0; 1024]),
            coef: Box::new([0.0; 1024]),
            tns: Tns::default(),
            pulse_present: false,
        }
    }
}

#[inline]
pub(crate) fn band_idx(g: usize, sfb: usize) -> usize {
    g * 16 + sfb
}

impl ChannelData {
    /// First window of each group.
    fn group_starts(&self) -> [usize; 9] {
        let mut s = [0usize; 9];
        for g in 0..self.ics.groups {
            s[g + 1] = s[g] + usize::from(self.ics.group_len[g]);
        }
        s
    }

    /// Parse `individual_channel_stream()`. With `common_window` the caller has already set
    /// `self.ics`. `scale_flag` is never set for AAC-LC.
    pub fn parse(&mut self, br: &mut BitReader, common_window: bool, sf_index: u8) -> Result<()> {
        self.global_gain = br.read(8)? as i32;
        if !common_window {
            self.ics = IcsInfo::parse(br, sf_index)?;
        }
        self.parse_section_data(br)?;
        self.parse_scalefactors(br)?;
        // pulse data
        // (pulse_start_sfb, (offset, amplitude) pairs, count)
        type Pulses = (usize, [(u8, u8); 4], usize);
        let mut pulses: Option<Pulses> = None;
        if br.read_bit()? {
            if self.ics.is_short() {
                return Err(Error::invalid("pulse data in a short-window frame"));
            }
            let n = br.read(2)? as usize + 1;
            let start = br.read(6)? as usize;
            if start >= self.ics.num_swb {
                return Err(Error::invalid("pulse_start_sfb out of range"));
            }
            let mut p = [(0u8, 0u8); 4];
            for e in p.iter_mut().take(n) {
                *e = (br.read(5)? as u8, br.read(4)? as u8);
            }
            pulses = Some((start, p, n));
        }
        self.pulse_present = pulses.is_some();
        // TNS
        self.tns.present = br.read_bit()?;
        if self.tns.present {
            self.parse_tns(br)?;
        }
        if br.read_bit()? {
            return Err(Error::unsupported("gain control data (AAC SSR)"));
        }
        self.parse_spectral(br)?;
        if let Some((start, p, n)) = pulses {
            let mut k = usize::from(self.ics.swb[start]);
            for &(off, amp) in p.iter().take(n) {
                k += usize::from(off);
                if k >= 1024 {
                    return Err(Error::invalid("pulse position out of range"));
                }
                let q = &mut self.quant[k];
                if *q > 0 {
                    *q += i32::from(amp);
                } else {
                    *q -= i32::from(amp);
                }
            }
        }
        self.dequantize();
        Ok(())
    }

    fn parse_section_data(&mut self, br: &mut BitReader) -> Result<()> {
        let short = self.ics.is_short();
        let (bits, esc) = if short { (3, 7) } else { (5, 31) };
        self.band_type = [ZERO_HCB; 128];
        for g in 0..self.ics.groups {
            let mut k = 0;
            while k < self.ics.max_sfb {
                let cb = br.read(4)? as u8;
                if cb == 12 {
                    return Err(Error::invalid("reserved codebook 12"));
                }
                let mut len = 0usize;
                loop {
                    let inc = br.read(bits)? as usize;
                    len += inc;
                    if inc != esc {
                        break;
                    }
                }
                if len == 0 || k + len > self.ics.max_sfb {
                    return Err(Error::invalid("bad section length"));
                }
                for sfb in k..k + len {
                    self.band_type[band_idx(g, sfb)] = cb;
                }
                k += len;
            }
        }
        Ok(())
    }

    fn parse_scalefactors(&mut self, br: &mut BitReader) -> Result<()> {
        let sfb_book = &books().sf;
        let mut off_sf = self.global_gain;
        let mut off_is = 0i32;
        let mut off_noise = self.global_gain - 90;
        let mut noise_first = true;
        for g in 0..self.ics.groups {
            for sfb in 0..self.ics.max_sfb {
                let i = band_idx(g, sfb);
                self.sf[i] = match self.band_type[i] {
                    ZERO_HCB => 0,
                    INTENSITY_HCB | INTENSITY_HCB2 => {
                        off_is += sfb_book.decode(br)? as i32 - 60;
                        off_is
                    }
                    NOISE_HCB => {
                        if noise_first {
                            noise_first = false;
                            off_noise += br.read(9)? as i32 - 256;
                        } else {
                            off_noise += sfb_book.decode(br)? as i32 - 60;
                        }
                        off_noise
                    }
                    _ => {
                        off_sf += sfb_book.decode(br)? as i32 - 60;
                        if !(0..=255).contains(&off_sf) {
                            return Err(Error::invalid("scalefactor out of range"));
                        }
                        off_sf
                    }
                };
            }
        }
        Ok(())
    }

    fn parse_tns(&mut self, br: &mut BitReader) -> Result<()> {
        let short = self.ics.is_short();
        let (nf_bits, len_bits, ord_bits, max_order) = if short {
            (1, 4, 3, TNS_MAX_ORDER_SHORT)
        } else {
            (2, 6, 5, TNS_MAX_ORDER_LONG)
        };
        for w in 0..self.ics.num_windows {
            let n = br.read(nf_bits)? as usize;
            self.tns.n_filt[w] = n as u8;
            if n == 0 {
                continue;
            }
            let coef_res = br.read(1)?;
            for f in 0..n {
                let filt = &mut self.tns.filt[w][f];
                filt.length = br.read(len_bits)? as u8;
                let order = br.read(ord_bits)? as usize;
                if order > max_order {
                    return Err(Error::invalid("TNS order too high"));
                }
                filt.order = order as u8;
                if order == 0 {
                    continue;
                }
                filt.direction = br.read_bit()?;
                let compress = br.read(1)?;
                let coef_bits = 3 + coef_res - compress;
                let iqfac = ((1 << (coef_res + 2)) as f64 - 0.5) / std::f64::consts::FRAC_PI_2;
                let iqfac_m = ((1 << (coef_res + 2)) as f64 + 0.5) / std::f64::consts::FRAC_PI_2;
                let mut parcor = [0f64; TNS_MAX_ORDER_LONG];
                for p in parcor.iter_mut().take(order) {
                    let c = f64::from(br.read_signed(coef_bits)?);
                    *p = (c / if c >= 0.0 { iqfac } else { iqfac_m }).sin();
                }
                let a = parcor_to_lpc(&parcor[..order]);
                for i in 0..order {
                    filt.lpc[i] = a[i + 1] as f32;
                }
            }
        }
        Ok(())
    }

    fn parse_spectral(&mut self, br: &mut BitReader) -> Result<()> {
        let bk = books();
        self.quant.fill(0);
        let starts = self.group_starts();
        let swb = self.ics.swb;
        for g in 0..self.ics.groups {
            for sfb in 0..self.ics.max_sfb {
                let bt = self.band_type[band_idx(g, sfb)];
                if bt == ZERO_HCB || bt >= NOISE_HCB {
                    continue;
                }
                let book = &bk.spec[usize::from(bt) - 1];
                let lo = usize::from(swb[sfb]);
                let hi = usize::from(swb[sfb + 1]);
                for w in starts[g]..starts[g + 1] {
                    let base = w * 128;
                    let out = &mut self.quant[base + lo..base + hi];
                    if book.quad {
                        for q in out.as_chunks_mut::<4>().0 {
                            let v = book.values[book.book.decode(br)?];
                            for j in 0..4 {
                                let mut x = i32::from(v[j]);
                                if !book.signed && x != 0 && br.read_bit()? {
                                    x = -x;
                                }
                                q[j] = x;
                            }
                        }
                    } else {
                        for q in out.as_chunks_mut::<2>().0 {
                            let v = book.values[book.book.decode(br)?];
                            let mut x = [i32::from(v[0]), i32::from(v[1])];
                            let mut neg = [false; 2];
                            if !book.signed {
                                for j in 0..2 {
                                    if x[j] != 0 {
                                        neg[j] = br.read_bit()?;
                                    }
                                }
                            }
                            if bt == ESC_HCB {
                                for xj in &mut x {
                                    if *xj == 16 {
                                        let n = br.read_unary_ones()?;
                                        if n > 8 {
                                            return Err(Error::invalid("escape sequence too long"));
                                        }
                                        *xj = (1 << (n + 4)) + br.read(n + 4)? as i32;
                                    }
                                }
                            }
                            q[0] = if neg[0] { -x[0] } else { x[0] };
                            q[1] = if neg[1] { -x[1] } else { x[1] };
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Inverse quantisation of the coded bands (noise and intensity bands are filled later).
    fn dequantize(&mut self) {
        let pow43 = &crate::tables::shared().pow43;
        self.coef.fill(0.0);
        let starts = self.group_starts();
        let swb = self.ics.swb;
        for g in 0..self.ics.groups {
            for sfb in 0..self.ics.max_sfb {
                let i = band_idx(g, sfb);
                let bt = self.band_type[i];
                if bt == ZERO_HCB || bt >= NOISE_HCB {
                    continue;
                }
                let gain = (0.25 * (self.sf[i] - 100) as f32).exp2();
                let lo = usize::from(swb[sfb]);
                let hi = usize::from(swb[sfb + 1]);
                for w in starts[g]..starts[g + 1] {
                    let r = w * 128 + lo..w * 128 + hi;
                    for (c, &q) in self.coef[r.clone()].iter_mut().zip(&self.quant[r]) {
                        let a = q.unsigned_abs() as usize;
                        let v = if a < pow43.len() {
                            pow43[a]
                        } else {
                            (a as f32).powf(4.0 / 3.0)
                        };
                        *c = if q < 0 { -v * gain } else { v * gain };
                    }
                }
            }
        }
    }

    /// Perceptual noise substitution for this channel's noise bands. `shared` gives per-band
    /// noise from the other channel of a pair (correlated noise for M/S noise bands).
    pub fn apply_pns(&mut self, rng: &mut u32, shared_from: Option<(&ChannelData, &[bool; 128])>) {
        let starts = self.group_starts();
        let swb = self.ics.swb;
        for g in 0..self.ics.groups {
            for sfb in 0..self.ics.max_sfb {
                let i = band_idx(g, sfb);
                if self.band_type[i] != NOISE_HCB {
                    continue;
                }
                let lo = usize::from(swb[sfb]);
                let hi = usize::from(swb[sfb + 1]);
                let gain = (0.25 * self.sf[i] as f32).exp2();
                for w in starts[g]..starts[g + 1] {
                    let r = w * 128 + lo..w * 128 + hi;
                    match shared_from {
                        Some((other, ms)) if ms[i] && other.band_type[i] == NOISE_HCB => {
                            // copy the other channel's (already scaled) noise, rescaled
                            let src = &other.coef[r.clone()];
                            let e: f32 = src.iter().map(|v| v * v).sum();
                            let s = if e > 0.0 { gain / e.sqrt() } else { 0.0 };
                            for (d, &v) in self.coef[r].iter_mut().zip(src) {
                                *d = v * s;
                            }
                        }
                        _ => {
                            let mut e = 0.0f32;
                            for c in self.coef[r.clone()].iter_mut() {
                                *rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                                let v = (*rng as i32) as f32;
                                *c = v;
                                e += v * v;
                            }
                            let s = if e > 0.0 { gain / e.sqrt() } else { 0.0 };
                            for c in self.coef[r].iter_mut() {
                                *c *= s;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Apply the TNS synthesis filters.
    pub fn apply_tns(&mut self, sf_index: u8) {
        if !self.tns.present {
            return;
        }
        let short = self.ics.is_short();
        let max_bands = tns_max_bands(sf_index, short).min(self.ics.max_sfb);
        let swb = self.ics.swb;
        for w in 0..self.ics.num_windows {
            let mut top = self.ics.num_swb;
            for f in 0..usize::from(self.tns.n_filt[w]) {
                let filt = self.tns.filt[w][f];
                let bottom = top.saturating_sub(usize::from(filt.length));
                let order = usize::from(filt.order);
                let start = usize::from(swb[bottom.min(max_bands)]);
                let end = usize::from(swb[top.min(max_bands)]);
                top = bottom;
                if order == 0 || end <= start {
                    continue;
                }
                let spec = &mut self.coef[w * 128..w * 128 + if short { 128 } else { 1024 }];
                tns_ar_filter(&mut spec[start..end], &filt.lpc[..order], filt.direction);
            }
        }
    }
}

/// Convert reflection (PARCOR) coefficients to direct-form LPC `a[0..=order]`, `a[0] = 1`.
pub(crate) fn parcor_to_lpc(k: &[f64]) -> [f64; TNS_MAX_ORDER_LONG + 1] {
    let mut a = [0f64; TNS_MAX_ORDER_LONG + 1];
    let mut b = [0f64; TNS_MAX_ORDER_LONG + 1];
    a[0] = 1.0;
    for m in 1..=k.len() {
        for i in 1..m {
            b[i] = a[i] + k[m - 1] * a[m - i];
        }
        a[1..m].copy_from_slice(&b[1..m]);
        a[m] = k[m - 1];
    }
    a
}

/// All-pole TNS filter `y[n] = x[n] - Σ a[i] y[n-i]` over `x`, upwards or (with `reverse`)
/// downwards in frequency.
pub(crate) fn tns_ar_filter(x: &mut [f32], a: &[f32], reverse: bool) {
    let order = a.len();
    if order == 0 {
        return;
    }
    let mut state = [0f32; TNS_MAX_ORDER_LONG];
    let n = x.len();
    for j in 0..n {
        let idx = if reverse { n - 1 - j } else { j };
        let mut y = x[idx];
        for i in 0..order {
            y -= a[i] * state[i];
        }
        state.copy_within(0..order - 1, 1);
        state[0] = y;
        x[idx] = y;
    }
}

/// The TNS analysis (all-zero) filter, the inverse of [`tns_ar_filter`].
pub(crate) fn tns_ma_filter(x: &mut [f32], a: &[f32], reverse: bool) {
    let order = a.len();
    if order == 0 {
        return;
    }
    let mut state = [0f32; TNS_MAX_ORDER_LONG];
    let n = x.len();
    for j in 0..n {
        let idx = if reverse { n - 1 - j } else { j };
        let v = x[idx];
        let mut y = v;
        for i in 0..order {
            y += a[i] * state[i];
        }
        state.copy_within(0..order - 1, 1);
        state[0] = v;
        x[idx] = y;
    }
}

/// M/S and intensity stereo of a channel pair. `ms` is the per-band `ms_used` mask (all false
/// when `ms_mask_present` is 0), `ms_mask_present` the raw field.
pub(crate) fn apply_stereo(
    l: &mut ChannelData,
    r: &mut ChannelData,
    ms: &[bool; 128],
    ms_mask_present: u32,
) {
    let mut starts = [0usize; 9];
    for g in 0..l.ics.groups {
        starts[g + 1] = starts[g] + usize::from(l.ics.group_len[g]);
    }
    let swb = l.ics.swb;
    for g in 0..l.ics.groups {
        for sfb in 0..l.ics.max_sfb {
            let i = band_idx(g, sfb);
            let lo = usize::from(swb[sfb]);
            let hi = usize::from(swb[sfb + 1]);
            let rbt = r.band_type[i];
            if rbt == INTENSITY_HCB || rbt == INTENSITY_HCB2 {
                let mut scale = (-0.25 * r.sf[i] as f32).exp2();
                if rbt == INTENSITY_HCB2 {
                    scale = -scale;
                }
                if ms_mask_present == 1 && ms[i] {
                    scale = -scale;
                }
                for w in starts[g]..starts[g + 1] {
                    let rg = w * 128 + lo..w * 128 + hi;
                    for (d, &s) in r.coef[rg.clone()].iter_mut().zip(&l.coef[rg]) {
                        *d = s * scale;
                    }
                }
            } else if ms[i] && l.band_type[i] < NOISE_HCB && rbt < NOISE_HCB {
                for w in starts[g]..starts[g + 1] {
                    let rg = w * 128 + lo..w * 128 + hi;
                    for (a, b) in l.coef[rg.clone()].iter_mut().zip(r.coef[rg].iter_mut()) {
                        let (m, s) = (*a, *b);
                        *a = m + s;
                        *b = m - s;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tns_filters_invert() {
        let k = [0.5, -0.3, 0.2, 0.1];
        let a64 = parcor_to_lpc(&k);
        let a: Vec<f32> = (1..=4).map(|i| a64[i] as f32).collect();
        let orig: Vec<f32> = (0..64)
            .map(|i| ((i * 31 % 17) as f32 - 8.0) / 3.0)
            .collect();
        for rev in [false, true] {
            let mut x = orig.clone();
            tns_ma_filter(&mut x, &a, rev);
            tns_ar_filter(&mut x, &a, rev);
            for (p, q) in x.iter().zip(&orig) {
                assert!((p - q).abs() < 1e-4);
            }
        }
    }
}
