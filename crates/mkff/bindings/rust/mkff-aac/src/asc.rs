//! `AudioSpecificConfig` (14496-3 1.6.2.1), `GASpecificConfig` (4.4.1) and
//! `program_config_element` (4.4.1.1) parsing and writing.

use crate::tables::{nearest_sampling_index, sampling_index, SAMPLE_RATES};
use mkff_audio_core::bits::{BitReader, BitWriter};
use mkff_audio_core::{Error, Result};

/// Audio object types this crate names.
pub mod object_type {
    /// AAC Main.
    pub const AAC_MAIN: u8 = 1;
    /// AAC Low Complexity.
    pub const AAC_LC: u8 = 2;
    /// AAC Scalable Sample Rate.
    pub const AAC_SSR: u8 = 3;
    /// AAC Long Term Prediction.
    pub const AAC_LTP: u8 = 4;
    /// Spectral Band Replication (HE-AAC).
    pub const SBR: u8 = 5;
    /// AAC scalable.
    pub const AAC_SCALABLE: u8 = 6;
    /// ER AAC LC.
    pub const ER_AAC_LC: u8 = 17;
    /// ER AAC LD.
    pub const ER_AAC_LD: u8 = 23;
    /// Parametric Stereo (HE-AAC v2).
    pub const PS: u8 = 29;
}

/// How SBR / PS presence was signalled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SbrSignalling {
    /// No SBR signalling in the configuration: SBR may still be present implicitly (an SBR
    /// extension payload in the access units of a stream with a core rate of 24 kHz or less).
    #[default]
    None,
    /// Explicit hierarchical signalling: the object type is 5 (SBR) or 29 (PS).
    Hierarchical,
    /// Explicit backward-compatible signalling: the `0x2b7` sync extension after the core
    /// configuration.
    BackwardCompatible,
}

/// A `program_config_element`: an explicit channel layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramConfig {
    /// `element_instance_tag`.
    pub element_instance_tag: u8,
    /// Object type minus one (2 bits, as stored).
    pub object_type: u8,
    /// `sampling_frequency_index`.
    pub sampling_index: u8,
    /// Front channel elements: `(is_cpe, element_instance_tag)`.
    pub front: Vec<(bool, u8)>,
    /// Side channel elements: `(is_cpe, element_instance_tag)`.
    pub side: Vec<(bool, u8)>,
    /// Back channel elements: `(is_cpe, element_instance_tag)`.
    pub back: Vec<(bool, u8)>,
    /// LFE element tags.
    pub lfe: Vec<u8>,
    /// Associated data element tags.
    pub assoc_data: Vec<u8>,
    /// Coupling channel elements: `(is_ind_sw, element_instance_tag)`.
    pub cc: Vec<(bool, u8)>,
    /// Element number of the mono mixdown, when present.
    pub mono_mixdown: Option<u8>,
    /// Element number of the stereo mixdown, when present.
    pub stereo_mixdown: Option<u8>,
    /// `(matrix_mixdown_idx, pseudo_surround_enable)`.
    pub matrix_mixdown: Option<(u8, bool)>,
    /// Comment field bytes.
    pub comment: Vec<u8>,
}

impl ProgramConfig {
    /// Parse a `program_config_element` (after its element id when it comes from a raw data
    /// block). `align_base` is the bit position byte alignment is measured from.
    pub fn parse(br: &mut BitReader, align_base: usize) -> Result<Self> {
        let mut p = ProgramConfig {
            element_instance_tag: br.read(4)? as u8,
            ..Default::default()
        };
        p.object_type = br.read(2)? as u8;
        p.sampling_index = br.read(4)? as u8;
        let nf = br.read(4)?;
        let ns = br.read(4)?;
        let nb = br.read(4)?;
        let nl = br.read(2)?;
        let na = br.read(3)?;
        let nc = br.read(4)?;
        if br.read_bit()? {
            p.mono_mixdown = Some(br.read(4)? as u8);
        }
        if br.read_bit()? {
            p.stereo_mixdown = Some(br.read(4)? as u8);
        }
        if br.read_bit()? {
            let idx = br.read(2)? as u8;
            p.matrix_mixdown = Some((idx, br.read_bit()?));
        }
        let mut elems = |n: u32| -> Result<Vec<(bool, u8)>> {
            (0..n)
                .map(|_| Ok((br.read_bit()?, br.read(4)? as u8)))
                .collect()
        };
        p.front = elems(nf)?;
        p.side = elems(ns)?;
        p.back = elems(nb)?;
        p.lfe = (0..nl)
            .map(|_| br.read(4).map(|v| v as u8))
            .collect::<Result<_>>()?;
        p.assoc_data = (0..na)
            .map(|_| br.read(4).map(|v| v as u8))
            .collect::<Result<_>>()?;
        p.cc = (0..nc)
            .map(|_| Ok((br.read_bit()?, br.read(4)? as u8)))
            .collect::<Result<_>>()?;
        let rel = br.position().saturating_sub(align_base);
        br.skip((8 - rel % 8) % 8)?;
        let n = br.read(8)?;
        p.comment = (0..n)
            .map(|_| br.read(8).map(|v| v as u8))
            .collect::<Result<_>>()?;
        Ok(p)
    }

    /// Write the element (without an element id); `align_base` as in [`ProgramConfig::parse`].
    pub fn write(&self, w: &mut BitWriter, align_base: usize) {
        w.write(u32::from(self.element_instance_tag), 4);
        w.write(u32::from(self.object_type), 2);
        w.write(u32::from(self.sampling_index), 4);
        w.write(self.front.len() as u32, 4);
        w.write(self.side.len() as u32, 4);
        w.write(self.back.len() as u32, 4);
        w.write(self.lfe.len() as u32, 2);
        w.write(self.assoc_data.len() as u32, 3);
        w.write(self.cc.len() as u32, 4);
        for m in [self.mono_mixdown, self.stereo_mixdown] {
            w.write_bit(m.is_some());
            if let Some(v) = m {
                w.write(u32::from(v), 4);
            }
        }
        w.write_bit(self.matrix_mixdown.is_some());
        if let Some((i, s)) = self.matrix_mixdown {
            w.write(u32::from(i), 2);
            w.write_bit(s);
        }
        for list in [&self.front, &self.side, &self.back] {
            for &(cpe, tag) in list {
                w.write_bit(cpe);
                w.write(u32::from(tag), 4);
            }
        }
        for &t in self.lfe.iter().chain(&self.assoc_data) {
            w.write(u32::from(t), 4);
        }
        for &(ind, tag) in &self.cc {
            w.write_bit(ind);
            w.write(u32::from(tag), 4);
        }
        let rel = w.bit_len().saturating_sub(align_base);
        w.write(0, ((8 - rel % 8) % 8) as u32);
        w.write(self.comment.len().min(255) as u32, 8);
        for &b in self.comment.iter().take(255) {
            w.write(u32::from(b), 8);
        }
    }

    /// Number of output channels the program carries (front, side, back and LFE).
    pub fn channels(&self) -> usize {
        let n = |l: &[(bool, u8)]| l.iter().map(|&(c, _)| if c { 2 } else { 1 }).sum::<usize>();
        n(&self.front) + n(&self.side) + n(&self.back) + self.lfe.len()
    }
}

/// A parsed `AudioSpecificConfig`.
///
/// For HE-AAC streams `object_type` is the core object type (2, AAC-LC) and `sbr_present` /
/// `extension_sample_rate` describe the SBR layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// Audio object type of the core coder (2 = AAC-LC).
    pub object_type: u8,
    /// `sampling_frequency_index` (15 = explicit rate in `sample_rate`).
    pub sampling_index: u8,
    /// Core sampling rate in Hz.
    pub sample_rate: u32,
    /// `channel_configuration` (0 = defined by a program config element).
    pub channel_config: u8,
    /// SBR explicitly signalled present (`sbr_present`) — see also [`SbrSignalling`].
    pub sbr_present: bool,
    /// Parametric stereo explicitly signalled.
    pub ps_present: bool,
    /// How SBR presence was signalled.
    pub sbr_signalling: SbrSignalling,
    /// `extensionSamplingFrequencyIndex` (15 = explicit), when SBR is signalled.
    pub extension_sampling_index: Option<u8>,
    /// SBR output rate in Hz (0 when SBR is not signalled).
    pub extension_sample_rate: u32,
    /// `frameLengthFlag`: 960-sample frames when set.
    pub frame_length_flag: bool,
    /// `dependsOnCoreCoder` (scalable configurations; not decodable here).
    pub depends_on_core_coder: bool,
    /// `coreCoderDelay`.
    pub core_coder_delay: u16,
    /// `extensionFlag` of `GASpecificConfig`.
    pub extension_flag: bool,
    /// Program config element (when `channel_config` is 0).
    pub pce: Option<ProgramConfig>,
}

impl Default for AudioSpecificConfig {
    fn default() -> Self {
        Self {
            object_type: object_type::AAC_LC,
            sampling_index: 4,
            sample_rate: 44100,
            channel_config: 2,
            sbr_present: false,
            ps_present: false,
            sbr_signalling: SbrSignalling::None,
            extension_sampling_index: None,
            extension_sample_rate: 0,
            frame_length_flag: false,
            depends_on_core_coder: false,
            core_coder_delay: 0,
            extension_flag: false,
            pce: None,
        }
    }
}

fn read_object_type(br: &mut BitReader) -> Result<u8> {
    let t = br.read(5)? as u8;
    Ok(if t == 31 { 32 + br.read(6)? as u8 } else { t })
}

fn read_rate(br: &mut BitReader) -> Result<(u8, u32)> {
    let idx = br.read(4)? as u8;
    if idx == 15 {
        let r = br.read(24)?;
        if r == 0 {
            return Err(Error::invalid("explicit sampling rate of 0"));
        }
        Ok((15, r))
    } else if usize::from(idx) < SAMPLE_RATES.len() {
        Ok((idx, SAMPLE_RATES[usize::from(idx)]))
    } else {
        Err(Error::invalid(format!(
            "reserved sampling frequency index {idx}"
        )))
    }
}

fn write_object_type(w: &mut BitWriter, t: u8) {
    if t >= 31 {
        w.write(31, 5);
        w.write(u32::from(t - 32), 6);
    } else {
        w.write(u32::from(t), 5);
    }
}

fn write_rate(w: &mut BitWriter, idx: u8, rate: u32) {
    match sampling_index(rate) {
        Some(i) if idx != 15 => w.write(u32::from(i), 4),
        _ => {
            w.write(15, 4);
            w.write(rate, 24);
        }
    }
}

const GA_TYPES: [u8; 12] = [1, 2, 3, 4, 6, 7, 17, 19, 20, 21, 22, 23];

impl AudioSpecificConfig {
    /// A plain AAC-LC configuration.
    pub fn lc(sample_rate: u32, channel_config: u8) -> Self {
        Self {
            sampling_index: sampling_index(sample_rate).unwrap_or(15),
            sample_rate,
            channel_config,
            ..Default::default()
        }
    }

    /// Parse from bytes (the `esds` / `CodecPrivate` payload).
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut br = BitReader::new(data);
        Self::parse_bits(&mut br, data.len() * 8)
    }

    /// Parse from a bit reader positioned at the start of the configuration. `end_bit` is the
    /// bit position where the configuration ends (used to look for backward-compatible SBR /
    /// PS signalling after the core configuration; pass the reader's length when unknown).
    pub fn parse_bits(br: &mut BitReader, end_bit: usize) -> Result<Self> {
        let start = br.position();
        let mut c = AudioSpecificConfig {
            object_type: read_object_type(br)?,
            ..Default::default()
        };
        let (idx, rate) = read_rate(br)?;
        c.sampling_index = idx;
        c.sample_rate = rate;
        c.channel_config = br.read(4)? as u8;
        let mut ext_type = 0u8;
        if c.object_type == object_type::SBR || c.object_type == object_type::PS {
            ext_type = object_type::SBR;
            c.sbr_present = true;
            c.ps_present = c.object_type == object_type::PS;
            c.sbr_signalling = SbrSignalling::Hierarchical;
            let (ei, er) = read_rate(br)?;
            c.extension_sampling_index = Some(ei);
            c.extension_sample_rate = er;
            c.object_type = read_object_type(br)?;
            if c.object_type == 22 {
                br.skip(4)?; // extensionChannelConfiguration
            }
        }
        if !GA_TYPES.contains(&c.object_type) {
            return Err(Error::unsupported(format!(
                "audio object type {}",
                c.object_type
            )));
        }
        // GASpecificConfig
        c.frame_length_flag = br.read_bit()?;
        c.depends_on_core_coder = br.read_bit()?;
        if c.depends_on_core_coder {
            c.core_coder_delay = br.read(14)? as u16;
        }
        c.extension_flag = br.read_bit()?;
        if c.channel_config == 0 {
            c.pce = Some(ProgramConfig::parse(br, start)?);
        }
        if c.object_type == 6 || c.object_type == 20 {
            br.skip(3)?; // layerNr
        }
        if c.extension_flag {
            if c.object_type == 22 {
                br.skip(16)?;
            }
            if matches!(c.object_type, 17 | 19 | 20 | 23) {
                br.skip(3)?;
            }
            br.skip(1)?; // extensionFlag3
        }
        if matches!(c.object_type, 17 | 19..=27) {
            let ep = br.read(2)?;
            if ep == 2 || ep == 3 {
                return Err(Error::unsupported("error protection configuration"));
            }
        }
        // backward-compatible explicit SBR / PS signalling
        if ext_type != object_type::SBR && end_bit.saturating_sub(br.position()) >= 16 {
            let save = br.position();
            if br.read(11)? == 0x2b7 {
                let et = read_object_type(br)?;
                if et == object_type::SBR {
                    c.sbr_present = br.read_bit()?;
                    c.sbr_signalling = SbrSignalling::BackwardCompatible;
                    if c.sbr_present {
                        let (ei, er) = read_rate(br)?;
                        c.extension_sampling_index = Some(ei);
                        c.extension_sample_rate = er;
                        if end_bit.saturating_sub(br.position()) >= 12 && br.read(11)? == 0x548 {
                            c.ps_present = br.read_bit()?;
                        }
                    }
                } else {
                    br.seek(save);
                }
            } else {
                br.seek(save);
            }
        }
        if c.channel_config > 7 && c.channel_config != 0 {
            // 8..15 are reserved in 14496-3:2005 (later editions define more layouts)
            return Err(Error::unsupported(format!(
                "channel configuration {}",
                c.channel_config
            )));
        }
        Ok(c)
    }

    /// Serialise. SBR is written with hierarchical signalling (object type 5, or 29 with PS).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        self.write(&mut w);
        w.finish()
    }

    /// Write the configuration to a bit writer.
    pub fn write(&self, w: &mut BitWriter) {
        let start = w.bit_len();
        if self.sbr_present && self.extension_sample_rate > 0 {
            write_object_type(
                w,
                if self.ps_present {
                    object_type::PS
                } else {
                    object_type::SBR
                },
            );
            write_rate(w, self.sampling_index, self.sample_rate);
            w.write(u32::from(self.channel_config), 4);
            write_rate(
                w,
                self.extension_sampling_index.unwrap_or(0),
                self.extension_sample_rate,
            );
            write_object_type(w, self.object_type);
        } else {
            write_object_type(w, self.object_type);
            write_rate(w, self.sampling_index, self.sample_rate);
            w.write(u32::from(self.channel_config), 4);
        }
        w.write_bit(self.frame_length_flag);
        w.write_bit(self.depends_on_core_coder);
        if self.depends_on_core_coder {
            w.write(u32::from(self.core_coder_delay), 14);
        }
        w.write_bit(false); // extensionFlag
        if self.channel_config == 0 {
            self.pce.clone().unwrap_or_default().write(w, start);
        }
    }

    /// Output channel count (parametric stereo makes a mono core stereo).
    pub fn channels(&self) -> usize {
        let n = match self.channel_config {
            0 => self.pce.as_ref().map_or(0, ProgramConfig::channels),
            1..=6 => usize::from(self.channel_config),
            7 => 8,
            _ => 0,
        };
        if self.ps_present && n == 1 {
            2
        } else {
            n
        }
    }

    /// Output sampling rate: the SBR rate when SBR is signalled, else the core rate.
    pub fn output_sample_rate(&self) -> u32 {
        if self.sbr_present && self.extension_sample_rate > 0 {
            self.extension_sample_rate
        } else {
            self.sample_rate
        }
    }

    /// Sampling index used to pick the scalefactor band tables (the nearest standard rate for
    /// explicit rates).
    pub fn band_index(&self) -> u8 {
        if self.sampling_index < 13 {
            self.sampling_index
        } else {
            nearest_sampling_index(self.sample_rate)
        }
    }

    /// Samples per channel of one decoded access unit of the core (1024, or 960).
    pub fn frame_length(&self) -> usize {
        if self.frame_length_flag {
            960
        } else {
            1024
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lc_round_trip() {
        let a = AudioSpecificConfig::parse(&[0x12, 0x10]).unwrap();
        assert_eq!(a.object_type, 2);
        assert_eq!(a.sample_rate, 44100);
        assert_eq!(a.channel_config, 2);
        assert!(!a.sbr_present);
        assert_eq!(a.to_bytes(), vec![0x12, 0x10]);
        assert_eq!(
            AudioSpecificConfig::lc(48000, 1).to_bytes(),
            vec![0x11, 0x88]
        );
    }

    #[test]
    fn explicit_rate() {
        let mut a = AudioSpecificConfig::lc(37800, 2);
        assert_eq!(a.sampling_index, 15);
        let b = a.to_bytes();
        let p = AudioSpecificConfig::parse(&b).unwrap();
        assert_eq!(p.sample_rate, 37800);
        assert_eq!(p.sampling_index, 15);
        assert_eq!(p.band_index(), 4);
        a.sample_rate = 37800;
        assert_eq!(p, a);
    }

    #[test]
    fn hierarchical_sbr_and_ps() {
        // AOT 5, 22050 Hz core, stereo, ext 44100, core AOT 2
        let mut a = AudioSpecificConfig::lc(22050, 2);
        a.sbr_present = true;
        a.extension_sample_rate = 44100;
        a.extension_sampling_index = Some(4);
        a.sbr_signalling = SbrSignalling::Hierarchical;
        let b = a.to_bytes();
        assert_eq!(b[0] >> 3, 5);
        let p = AudioSpecificConfig::parse(&b).unwrap();
        assert!(p.sbr_present && !p.ps_present);
        assert_eq!(p.object_type, 2);
        assert_eq!(p.output_sample_rate(), 44100);
        // HE-AAC v2: AOT 29, mono core
        let v2 = AudioSpecificConfig::parse(&[0xEB, 0x09, 0x88, 0x00]).unwrap();
        assert!(v2.sbr_present && v2.ps_present);
        assert_eq!(v2.sample_rate, 24000);
        assert_eq!(v2.output_sample_rate(), 48000);
        assert_eq!(v2.channels(), 2);
    }

    #[test]
    fn backward_compatible_sbr() {
        // LC 24 kHz stereo, then sync 0x2b7, AOT 5, sbr 1, ext idx 3 (48 kHz), sync 0x548, ps 0
        let mut w = BitWriter::new();
        w.write(2, 5);
        w.write(6, 4);
        w.write(2, 4);
        w.write(0, 3);
        w.write(0x2b7, 11);
        w.write(5, 5);
        w.write(1, 1);
        w.write(3, 4);
        w.write(0x548, 11);
        w.write(1, 1);
        let b = w.finish();
        let p = AudioSpecificConfig::parse(&b).unwrap();
        assert_eq!(p.sbr_signalling, SbrSignalling::BackwardCompatible);
        assert!(p.sbr_present && p.ps_present);
        assert_eq!(p.output_sample_rate(), 48000);
    }

    #[test]
    fn pce_round_trip() {
        let pce = ProgramConfig {
            element_instance_tag: 0,
            object_type: 1,
            sampling_index: 3,
            front: vec![(false, 0), (true, 0)],
            side: vec![],
            back: vec![(true, 1)],
            lfe: vec![0],
            comment: b"hi".to_vec(),
            ..Default::default()
        };
        let a = AudioSpecificConfig {
            channel_config: 0,
            pce: Some(pce),
            ..AudioSpecificConfig::lc(48000, 0)
        };
        let p = AudioSpecificConfig::parse(&a.to_bytes()).unwrap();
        assert_eq!(p.channels(), 6);
        assert_eq!(p, a);
    }

    #[test]
    fn unsupported_and_garbage() {
        assert!(AudioSpecificConfig::parse(&[]).is_err());
        assert!(AudioSpecificConfig::parse(&[0x12]).is_err());
        // AOT 8 (CELP) is not a GA type
        assert!(matches!(
            AudioSpecificConfig::parse(&[0x41, 0x10]),
            Err(Error::Unsupported(_))
        ));
        // reserved sampling index 13
        assert!(AudioSpecificConfig::parse(&[0x16, 0x90]).is_err());
    }
}
