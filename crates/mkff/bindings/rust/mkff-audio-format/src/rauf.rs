//! Futureboard RAUF v1 raw recording container reading.

use std::io::SeekFrom;

use mkff_audio_core::{CodecId, Error, Result};

use crate::io::{le16, le32, Source};
use crate::pcm::PcmFormat;
use crate::wav::PcmRegion;
use crate::{ReadSeek, Track};

const RAUF_MAGIC: &[u8; 4] = b"RAUF";
const RAUF_VERSION: u16 = 1;
const RAUF_HEADER_SIZE: u16 = 256;

/// RAUF header flag: the recording was finalized cleanly and `frames_written` is authoritative.
pub const RAUF_FLAG_FINALIZED: u16 = 1 << 0;
/// RAUF header flag: a sidecar JSON file accompanies the recording.
pub const RAUF_FLAG_HAS_SIDECAR: u16 = 1 << 1;
/// RAUF header flag: a peak summary accompanies the recording.
pub const RAUF_FLAG_HAS_PEAK: u16 = 1 << 2;
/// RAUF header flag: the recording was recovered after an unclean stop.
pub const RAUF_FLAG_RECOVERED: u16 = 1 << 3;

/// DAW recording metadata stored in a RAUF header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RaufInfo {
    /// Transport sample position in the project timeline where recording started.
    pub project_start_sample: u64,
    /// Unique 16-byte identifier of the take.
    pub take_id: [u8; 16],
    /// Raw header flags bitmask (`RAUF_FLAG_*`).
    pub flags: u16,
    /// Whether `RAUF_FLAG_FINALIZED` is set.
    pub finalized: bool,
    /// Whether the take was recovered (unfinalized or `RAUF_FLAG_RECOVERED` set).
    pub recovered: bool,
}

#[derive(Clone, Copy, Debug)]
struct ParsedHeader {
    sample_rate: u32,
    channels: u16,
    pcm: PcmFormat,
    flags: u16,
    frames_written: u64,
    project_start_sample: u64,
    take_id: [u8; 16],
    data_offset: u64,
}

fn le64(d: &[u8], at: usize) -> u64 {
    u64::from(le32(d, at)) | u64::from(le32(d, at + 4)) << 32
}

fn parse_header(bytes: &[u8; RAUF_HEADER_SIZE as usize], file_len: u64) -> Result<ParsedHeader> {
    if &bytes[0..4] != RAUF_MAGIC {
        return Err(Error::invalid("not a RAUF file"));
    }
    let version = le16(bytes, 4);
    if version != RAUF_VERSION {
        return Err(Error::unsupported(format!(
            "unsupported RAUF version {version}"
        )));
    }
    let header_size = le16(bytes, 6);
    if header_size != RAUF_HEADER_SIZE {
        return Err(Error::invalid(format!(
            "invalid RAUF header_size={header_size}"
        )));
    }
    let sample_rate = le32(bytes, 8);
    let channels = le16(bytes, 12);
    let sample_format_code = le16(bytes, 14);
    let endianness = le16(bytes, 16);
    let interleaved = le16(bytes, 18) != 0;
    let flags = le16(bytes, 20);
    let frames_written = le64(bytes, 24);
    let project_start_sample = le64(bytes, 32);
    let mut take_id = [0u8; 16];
    take_id.copy_from_slice(&bytes[40..56]);
    let data_offset = le64(bytes, 56);

    let pcm = match sample_format_code {
        3 => PcmFormat::int(32, false),
        4 => PcmFormat::float(32, false),
        other => {
            return Err(Error::unsupported(format!(
                "unsupported RAUF sample_format={other}"
            )))
        }
    };
    if sample_rate == 0 || channels == 0 {
        return Err(Error::invalid(
            "RAUF sample_rate and channels must be non-zero",
        ));
    }
    if endianness != 1 {
        return Err(Error::unsupported(
            "RAUF v1 supports little-endian PCM only",
        ));
    }
    if !interleaved {
        return Err(Error::unsupported("RAUF v1 supports interleaved PCM only"));
    }
    if data_offset < u64::from(RAUF_HEADER_SIZE) || data_offset > file_len {
        return Err(Error::invalid(format!(
            "invalid RAUF data_offset={data_offset} for file length {file_len}"
        )));
    }

    Ok(ParsedHeader {
        sample_rate,
        channels,
        pcm,
        flags,
        frames_written,
        project_start_sample,
        take_id,
        data_offset,
    })
}

/// Read and validate the RAUF header from `reader`, returning its recording metadata.
pub fn read_rauf_info(reader: &mut dyn ReadSeek) -> Result<RaufInfo> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let mut bytes = [0u8; RAUF_HEADER_SIZE as usize];
    let mut done = 0;
    while done < bytes.len() {
        match reader.read(&mut bytes[done..]) {
            Ok(0) => return Err(Error::invalid("truncated RAUF header")),
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let h = parse_header(&bytes, file_len)?;
    let finalized = (h.flags & RAUF_FLAG_FINALIZED) != 0;
    let recovered = !finalized || (h.flags & RAUF_FLAG_RECOVERED) != 0;
    Ok(RaufInfo {
        project_start_sample: h.project_start_sample,
        take_id: h.take_id,
        flags: h.flags,
        finalized,
        recovered,
    })
}

/// RAUF demuxer constructor.
pub(crate) struct RaufDemuxer;

impl RaufDemuxer {
    #[allow(clippy::new_ret_no_self)]
    pub fn open(mut src: Source) -> Result<PcmRegion> {
        let len = src.len();
        if len < u64::from(RAUF_HEADER_SIZE) {
            return Err(Error::invalid("truncated RAUF header"));
        }
        let mut bytes = [0u8; RAUF_HEADER_SIZE as usize];
        if src.read_at(0, &mut bytes)? < bytes.len() {
            return Err(Error::invalid("truncated RAUF header"));
        }
        let h = parse_header(&bytes, len)?;
        let bytes_per_sample = h.pcm.bytes_per_sample() as u64;
        let bytes_per_frame = u64::from(h.channels) * bytes_per_sample;
        if bytes_per_frame == 0 {
            return Err(Error::invalid("RAUF bytes per frame is zero"));
        }
        let avail_frames = (len - h.data_offset) / bytes_per_frame;
        let finalized = (h.flags & RAUF_FLAG_FINALIZED) != 0;
        let frames = if finalized {
            h.frames_written.min(avail_frames)
        } else {
            avail_frames
        };
        let data_len = frames.saturating_mul(bytes_per_frame);

        let mut track = Track::new(
            CodecId::Pcm,
            h.sample_rate,
            u32::from(h.channels),
            format!("PCM {}", h.pcm.label()),
        );
        track.pcm = Some(h.pcm);
        track.bits_per_sample = Some(32);

        PcmRegion::new(src, "RAUF", track, h.data_offset, data_len)
    }
}
