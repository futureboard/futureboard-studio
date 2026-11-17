//! Syncframe headers, sync search and the ISO BMFF `dac3` / `dec3` configuration boxes.

use mkff_audio_core::bits::BitReader;
use mkff_audio_core::crc::crc16;

use crate::layout::{base_layout, chanmap_channels, Channel};
use crate::tables::{ac3_frame_words, BIT_RATES, SAMPLE_RATES};

/// The AC-3 / E-AC-3 syncword.
pub const SYNCWORD: u16 = 0x0B77;

/// Kind of syncframe (`strmtyp` for E-AC-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamType {
    /// An AC-3 syncframe (`bsid` 0..=10).
    Ac3,
    /// E-AC-3 independent substream (`strmtyp` 0).
    Independent,
    /// E-AC-3 dependent substream (`strmtyp` 1): channels that extend or replace those of the
    /// preceding independent substream.
    Dependent,
    /// E-AC-3 independent substream converted from AC-3 (`strmtyp` 2).
    Converted,
}

/// Information from the `syncinfo` and `bsi` of one syncframe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    /// Bit stream identification: 0..=8 AC-3, 9 / 10 half / quarter rate AC-3, 11..=16 E-AC-3.
    pub bsid: u8,
    /// Output sample rate in Hz (after the half / quarter rate of `bsid` 9 / 10 and E-AC-3
    /// `fscod2`).
    pub sample_rate: u32,
    /// Syncframe size in bytes.
    pub frame_size: usize,
    /// Nominal bit rate in bit/s (computed from the frame size for E-AC-3).
    pub bit_rate: u32,
    /// Audio coding mode (`acmod`, Table 4.3): 0 = 1+1, 1 = 1/0, 2 = 2/0, 3 = 3/0, 4 = 2/1,
    /// 5 = 3/1, 6 = 2/2, 7 = 3/2.
    pub acmod: u8,
    /// Low frequency effects channel present.
    pub lfe: bool,
    /// Coded channels in this syncframe (full bandwidth channels plus the LFE).
    pub channels: usize,
    /// Audio blocks (of 256 samples) per syncframe: 6 for AC-3, 1, 2, 3 or 6 for E-AC-3.
    pub blocks_per_frame: usize,
    /// Stream / substream type.
    pub stream_type: StreamType,
    /// E-AC-3 substream ID (0 for AC-3).
    pub substream_id: u8,
    /// Bit stream mode (`bsmod`, Table 4.2; 0 when an E-AC-3 frame does not carry it).
    pub bsmod: u8,
    /// Dialogue normalization: the average dialogue level is `-dialnorm` dBFS (1..=31; the
    /// reserved value 0 is reported as 31).
    pub dialnorm: u8,
    /// AC-3 `cmixlev` (when three front channels exist).
    pub cmixlev: Option<u8>,
    /// AC-3 `surmixlev` (when surround channels exist).
    pub surmixlev: Option<u8>,
    /// E-AC-3 / Annex D Lo/Ro centre mix level code (`lorocmixlev`) when present.
    pub lorocmixlev: Option<u8>,
    /// E-AC-3 / Annex D Lo/Ro surround mix level code (`lorosurmixlev`) when present.
    pub lorosurmixlev: Option<u8>,
    /// Custom channel map of an E-AC-3 dependent substream (`chanmap`, Table E.1.4).
    pub chanmap: Option<u16>,
}

impl FrameInfo {
    /// PCM samples per channel this syncframe decodes to.
    pub fn samples(&self) -> usize {
        self.blocks_per_frame * 256
    }

    /// True for E-AC-3 syncframes (`bsid` 11..=16).
    pub fn is_eac3(&self) -> bool {
        self.stream_type != StreamType::Ac3
    }

    /// The channels this syncframe codes, in coded order mapped to locations (for a dependent
    /// substream with a `chanmap`, the locations it names).
    pub fn coded_channels(&self) -> Vec<Channel> {
        match self.chanmap {
            Some(map) => chanmap_channels(map),
            None => base_layout(self.acmod, self.lfe),
        }
    }
}

/// Alias kept for callers that think of the parsed header as the sync information.
pub type SyncInfo = FrameInfo;

/// Parse the header of the syncframe at the start of `data` (which must begin with the
/// syncword). Returns `None` when the header is invalid or truncated; the rest of the frame
/// does not need to be present.
pub fn parse_frame(data: &[u8]) -> Option<FrameInfo> {
    if data.len() < 8 || u16::from_be_bytes([data[0], data[1]]) != SYNCWORD {
        return None;
    }
    // bsid sits at the same position in both syntaxes: bits 40..45
    let bsid = data[5] >> 3;
    match bsid {
        0..=10 => parse_ac3(data, bsid),
        11..=16 => parse_eac3(data, bsid),
        _ => None,
    }
}

fn parse_ac3(data: &[u8], bsid: u8) -> Option<FrameInfo> {
    let mut r = BitReader::new(data);
    r.skip(32).ok()?; // syncword, crc1
    let fscod = r.read(2).ok()? as u8;
    let frmsizecod = r.read(6).ok()? as u8;
    if fscod == 3 || frmsizecod > 37 {
        return None;
    }
    let words = ac3_frame_words(fscod, frmsizecod)?;
    let _bsid = r.read(5).ok()?;
    let bsmod = r.read(3).ok()? as u8;
    let acmod = r.read(3).ok()? as u8;
    let mut cmixlev = None;
    let mut surmixlev = None;
    if acmod & 1 != 0 && acmod != 1 {
        cmixlev = Some(r.read(2).ok()? as u8);
    }
    if acmod & 4 != 0 {
        surmixlev = Some(r.read(2).ok()? as u8);
    }
    if acmod == 2 {
        r.skip(2).ok()?; // dsurmod
    }
    let lfe = r.read_bit().ok()?;
    let dialnorm = r.read(5).ok()? as u8;
    // Annex D (bsid 6) lo/ro mix levels sit further into the bsi; they are parsed by the
    // decoder, the header only reports the AC-3 levels.
    let shift = bsid.saturating_sub(8);
    let sample_rate = SAMPLE_RATES[usize::from(fscod)] >> shift;
    let nfchans = nfchans(acmod);
    Some(FrameInfo {
        bsid,
        sample_rate,
        frame_size: words * 2,
        bit_rate: (BIT_RATES[usize::from(frmsizecod >> 1)] * 1000) >> shift,
        acmod,
        lfe,
        channels: nfchans + usize::from(lfe),
        blocks_per_frame: 6,
        stream_type: StreamType::Ac3,
        substream_id: 0,
        bsmod,
        dialnorm: if dialnorm == 0 { 31 } else { dialnorm },
        cmixlev,
        surmixlev,
        lorocmixlev: None,
        lorosurmixlev: None,
        chanmap: None,
    })
}

fn parse_eac3(data: &[u8], bsid: u8) -> Option<FrameInfo> {
    let mut r = BitReader::new(data);
    r.skip(16).ok()?;
    let strmtyp = r.read(2).ok()?;
    let substream_id = r.read(3).ok()? as u8;
    let frmsiz = r.read(11).ok()? as usize;
    let fscod = r.read(2).ok()? as usize;
    let (sample_rate, blocks) = if fscod == 3 {
        // reduced sample rates (A/52B fscod2); always six blocks
        let fscod2 = r.read(2).ok()? as usize;
        if fscod2 == 3 {
            return None;
        }
        (SAMPLE_RATES[fscod2] / 2, 6)
    } else {
        let numblkscod = r.read(2).ok()? as usize;
        (SAMPLE_RATES[fscod], [1, 2, 3, 6][numblkscod])
    };
    let acmod = r.read(3).ok()? as u8;
    let lfe = r.read_bit().ok()?;
    let _bsid = r.read(5).ok()?;
    let dialnorm = r.read(5).ok()? as u8;
    let stream_type = match strmtyp {
        0 => StreamType::Independent,
        1 => StreamType::Dependent,
        2 => StreamType::Converted,
        _ => return None,
    };
    let frame_size = (frmsiz + 1) * 2;
    if frame_size < 8 {
        return None;
    }
    if r.read_bit().ok()? {
        r.skip(8).ok()?; // compr
    }
    if acmod == 0 {
        r.skip(5).ok()?;
        if r.read_bit().ok()? {
            r.skip(8).ok()?;
        }
    }
    let mut chanmap = None;
    if stream_type == StreamType::Dependent && r.read_bit().ok()? {
        chanmap = Some(r.read(16).ok()? as u16);
    }
    let mut lorocmixlev = None;
    let mut lorosurmixlev = None;
    if r.read_bit().ok()? {
        // mixmdate: the fields that matter for downmixing come first
        if acmod > 2 {
            r.skip(2).ok()?; // dmixmod
        }
        if acmod & 1 != 0 && acmod > 2 {
            r.skip(3).ok()?;
            lorocmixlev = Some(r.read(3).ok()? as u8);
        }
        if acmod & 4 != 0 {
            r.skip(3).ok()?;
            lorosurmixlev = Some(r.read(3).ok()? as u8);
        }
    }
    let samples = (blocks * 256) as u64;
    let bit_rate = (frame_size as u64 * 8 * u64::from(sample_rate) / samples) as u32;
    Some(FrameInfo {
        bsid,
        sample_rate,
        frame_size,
        bit_rate,
        acmod,
        lfe,
        channels: nfchans(acmod) + usize::from(lfe),
        blocks_per_frame: blocks,
        stream_type,
        substream_id,
        bsmod: 0,
        dialnorm: if dialnorm == 0 { 31 } else { dialnorm },
        cmixlev: None,
        surmixlev: None,
        lorocmixlev,
        lorosurmixlev,
        chanmap,
    })
}

/// Full bandwidth channels for an audio coding mode.
pub(crate) fn nfchans(acmod: u8) -> usize {
    [2, 1, 2, 3, 3, 4, 4, 5][usize::from(acmod & 7)]
}

/// Check the CRC words of a complete syncframe (`frame.len()` must be the frame size).
///
/// For AC-3 both `crc1` (first 5/8 of the frame) and `crc2` are verified; E-AC-3 only has
/// `crc2`, covering the whole frame.
pub fn check_crc(frame: &[u8], info: &FrameInfo) -> bool {
    if frame.len() != info.frame_size || frame.len() < 4 {
        return false;
    }
    if info.stream_type == StreamType::Ac3 {
        let words = info.frame_size / 2;
        let five_eighths = ((words >> 1) + (words >> 3)) * 2;
        if crc16(0, &frame[2..five_eighths]) != 0 {
            return false;
        }
        crc16(0, &frame[five_eighths..]) == 0
    } else {
        crc16(0, &frame[2..]) == 0
    }
}

/// Find the first syncframe in `data`: the offset of a syncword whose header parses and whose
/// frame either has valid CRCs or is followed by another valid header. A candidate whose frame
/// runs past the end of `data` is accepted on its header alone.
pub fn find_sync(data: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0x0B && data[i + 1] == 0x77 {
            if let Some(info) = parse_frame(&data[i..]) {
                let end = i + info.frame_size;
                if end > data.len() {
                    return Some(i);
                }
                if check_crc(&data[i..end], &info) {
                    return Some(i);
                }
                if end + 8 <= data.len() && parse_frame(&data[end..]).is_some() {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Contents of an ISO BMFF `dac3` box (AC3SpecificBox, clause F.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dac3 {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Bit stream identification.
    pub bsid: u8,
    /// Bit stream mode (service type).
    pub bsmod: u8,
    /// Audio coding mode (see [`FrameInfo::acmod`]).
    pub acmod: u8,
    /// LFE channel present.
    pub lfe: bool,
    /// Nominal bit rate in bit/s.
    pub bit_rate: u32,
    /// Channel count (full bandwidth channels plus LFE).
    pub channels: usize,
    /// Channel locations in the decoder's output order.
    pub layout: Vec<Channel>,
}

/// Parse the payload of a `dac3` box (the 3 bytes after the box header).
pub fn parse_dac3(payload: &[u8]) -> Option<Dac3> {
    let mut r = BitReader::new(payload);
    let fscod = r.read(2).ok()? as usize;
    let bsid = r.read(5).ok()? as u8;
    let bsmod = r.read(3).ok()? as u8;
    let acmod = r.read(3).ok()? as u8;
    let lfe = r.read_bit().ok()?;
    let bit_rate_code = r.read(5).ok()? as usize;
    if fscod == 3 {
        return None;
    }
    let bit_rate = *BIT_RATES.get(bit_rate_code)? * 1000;
    let layout = crate::layout::output_order(&base_layout(acmod, lfe));
    Some(Dac3 {
        sample_rate: SAMPLE_RATES[fscod],
        bsid,
        bsmod,
        acmod,
        lfe,
        bit_rate,
        channels: layout.len(),
        layout,
    })
}

/// One independent substream described by a `dec3` box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dec3Substream {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Bit stream identification.
    pub bsid: u8,
    /// Associated service (to be mixed with a main service).
    pub asvc: bool,
    /// Bit stream mode (service type).
    pub bsmod: u8,
    /// Audio coding mode of the independent substream.
    pub acmod: u8,
    /// LFE channel present in the independent substream.
    pub lfe: bool,
    /// Number of dependent substreams.
    pub num_dep_sub: u8,
    /// Extra channel locations carried by the dependent substreams (Table F.6.1).
    pub chan_loc: u16,
    /// All channels of the programme (independent plus dependent substreams), in the decoder's
    /// output order.
    pub layout: Vec<Channel>,
}

/// Contents of an ISO BMFF `dec3` box (EC3SpecificBox, clause F.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dec3 {
    /// Data rate of the whole bit stream in kbit/s.
    pub data_rate: u32,
    /// The independent substreams (programmes).
    pub substreams: Vec<Dec3Substream>,
}

impl Dec3 {
    /// Channel count of the first programme (what [`crate::Ac3Decoder`] outputs by default).
    pub fn channels(&self) -> usize {
        self.substreams.first().map_or(0, |s| s.layout.len())
    }

    /// Sample rate of the first programme.
    pub fn sample_rate(&self) -> u32 {
        self.substreams.first().map_or(0, |s| s.sample_rate)
    }
}

/// Parse the payload of a `dec3` box (the bytes after the box header).
pub fn parse_dec3(payload: &[u8]) -> Option<Dec3> {
    let mut r = BitReader::new(payload);
    let data_rate = r.read(13).ok()?;
    let num_ind_sub = r.read(3).ok()? as usize + 1;
    let mut substreams = Vec::with_capacity(num_ind_sub);
    for _ in 0..num_ind_sub {
        let fscod = r.read(2).ok()? as usize;
        let bsid = r.read(5).ok()? as u8;
        r.skip(1).ok()?;
        let asvc = r.read_bit().ok()?;
        let bsmod = r.read(3).ok()? as u8;
        let acmod = r.read(3).ok()? as u8;
        let lfe = r.read_bit().ok()?;
        r.skip(3).ok()?;
        let num_dep_sub = r.read(4).ok()? as u8;
        let chan_loc = if num_dep_sub > 0 {
            r.read(9).ok()? as u16
        } else {
            r.skip(1).ok()?;
            0
        };
        if fscod == 3 {
            return None;
        }
        let mut chans = base_layout(acmod, lfe);
        // chan_loc bits 0..=7 are chanmap bits 5..=12, bit 8 is LFE2 (chanmap bit 14)
        let mut map = 0u16;
        for bit in 0..8 {
            if chan_loc & (1 << bit) != 0 {
                map |= 1 << (15 - (5 + bit));
            }
        }
        if chan_loc & 0x100 != 0 {
            map |= 1 << 1;
        }
        for c in chanmap_channels(map) {
            if !chans.contains(&c) {
                chans.push(c);
            }
        }
        substreams.push(Dec3Substream {
            sample_rate: SAMPLE_RATES[fscod],
            bsid,
            asvc,
            bsmod,
            acmod,
            lfe,
            num_dep_sub,
            chan_loc,
            layout: crate::layout::output_order(&chans),
        });
    }
    Some(Dec3 {
        data_rate,
        substreams,
    })
}
