use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, RaufError>;

pub const RAUF_MAGIC: &[u8; 4] = b"RAUF";
pub const RAUF_VERSION: u16 = 1;
pub const RAUF_HEADER_SIZE: u16 = 256;
pub const RAUF_DATA_OFFSET: u64 = 256;

pub const RAUF_FLAG_FINALIZED: u16 = 1 << 0;
pub const RAUF_FLAG_HAS_SIDECAR: u16 = 1 << 1;
pub const RAUF_FLAG_HAS_PEAK: u16 = 1 << 2;
pub const RAUF_FLAG_RECOVERED: u16 = 1 << 3;

#[derive(Debug)]
pub enum RaufError {
    Io(std::io::Error),
    InvalidMagic,
    UnsupportedVersion(u16),
    InvalidHeader(String),
    UnsupportedFormat(String),
}

impl std::fmt::Display for RaufError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::InvalidMagic => write!(f, "not a RAUF file"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported RAUF version {version}"),
            Self::InvalidHeader(message) => write!(f, "invalid RAUF header: {message}"),
            Self::UnsupportedFormat(message) => write!(f, "unsupported RAUF format: {message}"),
        }
    }
}

impl std::error::Error for RaufError {}

impl From<std::io::Error> for RaufError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RaufSampleFormat {
    S32 = 3,
    F32 = 4,
}

impl RaufSampleFormat {
    fn from_u16(value: u16) -> Result<Self> {
        match value {
            3 => Ok(Self::S32),
            4 => Ok(Self::F32),
            other => Err(RaufError::UnsupportedFormat(format!(
                "sample_format={other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RaufConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: RaufSampleFormat,
    pub interleaved: bool,
    pub project_start_sample: u64,
    pub take_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaufHeader {
    pub version: u16,
    pub header_size: u16,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: RaufSampleFormat,
    pub endianness: u16,
    pub interleaved: bool,
    pub flags: u16,
    pub frames_written: u64,
    pub project_start_sample: u64,
    pub take_id: [u8; 16],
    pub data_offset: u64,
}

#[derive(Debug, Clone)]
pub struct RaufReport {
    pub path: PathBuf,
    pub header: RaufHeader,
    pub frames_written: u64,
    pub data_bytes: u64,
}

/// How much PCM the writer holds before it goes to the file: about 0.7 s of
/// stereo s32 at 48 kHz. A take used to be written one sample per `write_all`
/// straight to the file, close to a hundred thousand system calls a second
/// per stereo track, which is how a recording's writer fell behind the input.
/// A crash loses at most this much of the take; recovery reads the length
/// back from the file size.
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

pub struct RaufWriter {
    path: PathBuf,
    file: BufWriter<File>,
    header: RaufHeader,
    /// One block's samples as little-endian bytes, reused for every block.
    scratch: Vec<u8>,
}

impl RaufWriter {
    pub fn create(path: impl AsRef<Path>, config: RaufConfig) -> Result<Self> {
        if config.sample_rate == 0 {
            return Err(RaufError::InvalidHeader(
                "sample_rate must be non-zero".to_string(),
            ));
        }
        if config.channels == 0 {
            return Err(RaufError::InvalidHeader(
                "channels must be non-zero".to_string(),
            ));
        }
        if !config.interleaved {
            return Err(RaufError::UnsupportedFormat(
                "RAUF v1 requires interleaved PCM".to_string(),
            ));
        }

        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(&path)?;

        let header = RaufHeader {
            version: RAUF_VERSION,
            header_size: RAUF_HEADER_SIZE,
            sample_rate: config.sample_rate,
            channels: config.channels,
            sample_format: config.sample_format,
            endianness: 1,
            interleaved: config.interleaved,
            flags: 0,
            frames_written: 0,
            project_start_sample: config.project_start_sample,
            take_id: config.take_id,
            data_offset: RAUF_DATA_OFFSET,
        };
        write_header(&mut file, &header)?;
        file.seek(SeekFrom::Start(header.data_offset))?;
        Ok(Self {
            path,
            file: BufWriter::with_capacity(WRITE_BUFFER_BYTES, file),
            header,
            scratch: Vec::new(),
        })
    }

    pub fn write_s32le_interleaved(&mut self, samples: &[i32]) -> Result<()> {
        if self.header.sample_format != RaufSampleFormat::S32 {
            return Err(RaufError::UnsupportedFormat(
                "writer was not configured for s32le".to_string(),
            ));
        }
        let channels = self.header.channels as usize;
        if channels == 0 || !samples.len().is_multiple_of(channels) {
            return Err(RaufError::InvalidHeader(format!(
                "sample count {} is not divisible by channels {}",
                samples.len(),
                channels
            )));
        }
        self.scratch.clear();
        for sample in samples {
            self.scratch.extend_from_slice(&sample.to_le_bytes());
        }
        self.file.write_all(&self.scratch)?;
        self.header.frames_written += (samples.len() / channels) as u64;
        Ok(())
    }

    pub fn write_f32le_interleaved(&mut self, samples: &[f32]) -> Result<()> {
        if self.header.sample_format != RaufSampleFormat::F32 {
            return Err(RaufError::UnsupportedFormat(
                "writer was not configured for f32le".to_string(),
            ));
        }
        let channels = self.header.channels as usize;
        if channels == 0 || !samples.len().is_multiple_of(channels) {
            return Err(RaufError::InvalidHeader(format!(
                "sample count {} is not divisible by channels {}",
                samples.len(),
                channels
            )));
        }
        self.scratch.clear();
        for sample in samples {
            self.scratch.extend_from_slice(&sample.to_le_bytes());
        }
        self.file.write_all(&self.scratch)?;
        self.header.frames_written += (samples.len() / channels) as u64;
        Ok(())
    }

    pub fn frames_written(&self) -> u64 {
        self.header.frames_written
    }

    pub fn set_flags(&mut self, flags: u16) {
        self.header.flags |= flags;
    }

    pub fn finalize(mut self) -> Result<RaufReport> {
        self.file.flush()?;
        self.header.flags |= RAUF_FLAG_FINALIZED;
        let file = self.file.get_mut();
        write_header(file, &self.header)?;
        file.flush()?;
        file.sync_all()?;
        let data_bytes = self.header.frames_written
            * self.header.channels as u64
            * bytes_per_sample(self.header.sample_format);
        Ok(RaufReport {
            path: self.path,
            header: self.header.clone(),
            frames_written: self.header.frames_written,
            data_bytes,
        })
    }
}

pub struct RaufReader {
    path: PathBuf,
    header: RaufHeader,
}

impl RaufReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let header = read_header(&mut file)?;
        Ok(Self { path, header })
    }

    pub fn header(&self) -> &RaufHeader {
        &self.header
    }

    pub fn recover_frames_from_size(&self) -> Result<u64> {
        let size = std::fs::metadata(&self.path)?.len();
        recover_frames_from_size(&self.header, size)
    }
}

// ---------------------------------------------------------------------------
// RAUF as a generic AudioEncoder target (internal/debug export format).
// ---------------------------------------------------------------------------

use crate::format::{
    AudioEncodeSpec, AudioEncodeSummary, AudioEncoder, AudioFileFormat, AudioSampleFormat,
    EncodeError, check_interleaved_len, f32_to_i32, i32_to_f32,
};

impl From<RaufError> for EncodeError {
    fn from(value: RaufError) -> Self {
        match value {
            RaufError::Io(error) => EncodeError::Io(error),
            other => EncodeError::FinalizeFailed(other.to_string()),
        }
    }
}

/// Wraps [`RaufWriter`] behind the generic [`AudioEncoder`] trait. F32 specs map
/// to RAUF F32; integer specs are stored as full-scale RAUF S32.
pub struct RaufEncoder {
    writer: Option<RaufWriter>,
    spec: AudioEncodeSpec,
}

impl RaufEncoder {
    pub fn create(
        path: impl AsRef<Path>,
        spec: AudioEncodeSpec,
    ) -> std::result::Result<Self, EncodeError> {
        spec.validate()?;
        let sample_format = if spec.sample_format.is_float() {
            RaufSampleFormat::F32
        } else {
            RaufSampleFormat::S32
        };
        let writer = RaufWriter::create(
            path,
            RaufConfig {
                sample_rate: spec.sample_rate,
                channels: spec.channels,
                sample_format,
                interleaved: true,
                project_start_sample: 0,
                take_id: [0u8; 16],
            },
        )?;
        Ok(Self {
            writer: Some(writer),
            spec,
        })
    }

    fn writer_mut(&mut self) -> std::result::Result<&mut RaufWriter, EncodeError> {
        self.writer
            .as_mut()
            .ok_or_else(|| EncodeError::InvalidInput("write after finalize".to_string()))
    }
}

impl AudioEncoder for RaufEncoder {
    fn format(&self) -> AudioFileFormat {
        AudioFileFormat::Rauf
    }

    fn spec(&self) -> AudioEncodeSpec {
        self.spec.clone()
    }

    fn write_interleaved_f32(&mut self, frames: &[f32]) -> std::result::Result<(), EncodeError> {
        check_interleaved_len(frames.len(), self.spec.channels)?;
        let float = self.spec.sample_format.is_float();
        let writer = self.writer_mut()?;
        if float {
            writer.write_f32le_interleaved(frames)?;
        } else {
            let ints: Vec<i32> = frames.iter().map(|&x| f32_to_i32(x)).collect();
            writer.write_s32le_interleaved(&ints)?;
        }
        Ok(())
    }

    fn write_interleaved_i32(&mut self, frames: &[i32]) -> std::result::Result<(), EncodeError> {
        check_interleaved_len(frames.len(), self.spec.channels)?;
        let float = self.spec.sample_format.is_float();
        let writer = self.writer_mut()?;
        if float {
            let floats: Vec<f32> = frames.iter().map(|&x| i32_to_f32(x)).collect();
            writer.write_f32le_interleaved(&floats)?;
        } else {
            writer.write_s32le_interleaved(frames)?;
        }
        Ok(())
    }

    fn finalize(&mut self) -> std::result::Result<AudioEncodeSummary, EncodeError> {
        let writer = self
            .writer
            .take()
            .ok_or_else(|| EncodeError::FinalizeFailed("encoder already finalized".to_string()))?;
        let report = writer.finalize()?;
        let sample_format = if self.spec.sample_format.is_float() {
            AudioSampleFormat::F32
        } else {
            AudioSampleFormat::I32
        };
        Ok(AudioEncodeSummary {
            format: AudioFileFormat::Rauf,
            sample_rate: report.header.sample_rate,
            channels: report.header.channels,
            sample_format,
            frames_written: report.frames_written,
            bytes_written: report.data_bytes,
        })
    }
}

fn recover_frames_from_size(header: &RaufHeader, size: u64) -> Result<u64> {
    if size < header.data_offset {
        return Ok(0);
    }
    let bytes_per_frame = header.channels as u64 * bytes_per_sample(header.sample_format);
    if bytes_per_frame == 0 {
        return Err(RaufError::InvalidHeader(
            "bytes per frame is zero".to_string(),
        ));
    }
    Ok((size - header.data_offset) / bytes_per_frame)
}

fn bytes_per_sample(format: RaufSampleFormat) -> u64 {
    match format {
        RaufSampleFormat::S32 | RaufSampleFormat::F32 => 4,
    }
}

fn write_header(file: &mut File, header: &RaufHeader) -> Result<()> {
    let mut bytes = [0u8; RAUF_HEADER_SIZE as usize];
    bytes[0..4].copy_from_slice(RAUF_MAGIC);
    put_u16(&mut bytes, 4, header.version);
    put_u16(&mut bytes, 6, header.header_size);
    put_u32(&mut bytes, 8, header.sample_rate);
    put_u16(&mut bytes, 12, header.channels);
    put_u16(&mut bytes, 14, header.sample_format as u16);
    put_u16(&mut bytes, 16, header.endianness);
    put_u16(&mut bytes, 18, u16::from(header.interleaved));
    put_u16(&mut bytes, 20, header.flags);
    put_u64(&mut bytes, 24, header.frames_written);
    put_u64(&mut bytes, 32, header.project_start_sample);
    bytes[40..56].copy_from_slice(&header.take_id);
    put_u64(&mut bytes, 56, header.data_offset);
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    Ok(())
}

fn read_header(file: &mut File) -> Result<RaufHeader> {
    let mut bytes = [0u8; RAUF_HEADER_SIZE as usize];
    file.read_exact(&mut bytes)?;
    if &bytes[0..4] != RAUF_MAGIC {
        return Err(RaufError::InvalidMagic);
    }
    let version = get_u16(&bytes, 4);
    if version != RAUF_VERSION {
        return Err(RaufError::UnsupportedVersion(version));
    }
    let header_size = get_u16(&bytes, 6);
    if header_size != RAUF_HEADER_SIZE {
        return Err(RaufError::InvalidHeader(format!(
            "header_size={header_size}"
        )));
    }
    let sample_rate = get_u32(&bytes, 8);
    let channels = get_u16(&bytes, 12);
    let sample_format = RaufSampleFormat::from_u16(get_u16(&bytes, 14))?;
    let endianness = get_u16(&bytes, 16);
    let interleaved = get_u16(&bytes, 18) != 0;
    let flags = get_u16(&bytes, 20);
    let frames_written = get_u64(&bytes, 24);
    let project_start_sample = get_u64(&bytes, 32);
    let mut take_id = [0u8; 16];
    take_id.copy_from_slice(&bytes[40..56]);
    let data_offset = get_u64(&bytes, 56);

    if sample_rate == 0 || channels == 0 {
        return Err(RaufError::InvalidHeader(
            "sample_rate and channels must be non-zero".to_string(),
        ));
    }
    if endianness != 1 {
        return Err(RaufError::UnsupportedFormat(
            "RAUF v1 supports little endian only".to_string(),
        ));
    }
    if !interleaved {
        return Err(RaufError::UnsupportedFormat(
            "RAUF v1 supports interleaved PCM only".to_string(),
        ));
    }
    if data_offset < RAUF_HEADER_SIZE as u64 {
        return Err(RaufError::InvalidHeader(format!(
            "data_offset={data_offset}"
        )));
    }

    Ok(RaufHeader {
        version,
        header_size,
        sample_rate,
        channels,
        sample_format,
        endianness,
        interleaved,
        flags,
        frames_written,
        project_start_sample,
        take_id,
        data_offset,
    })
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(channels: u16) -> RaufConfig {
        RaufConfig {
            sample_rate: 48_000,
            channels,
            sample_format: RaufSampleFormat::S32,
            interleaved: true,
            project_start_sample: 960_000,
            take_id: [7; 16],
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "futureboard-{name}-{}-{}.rauf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    #[test]
    fn header_write_read_roundtrips() {
        let path = temp_path("header");
        let writer = RaufWriter::create(&path, config(1)).unwrap();
        assert_eq!(writer.frames_written(), 0);
        drop(writer.finalize().unwrap());

        let reader = RaufReader::open(&path).unwrap();
        let header = reader.header();
        assert_eq!(header.version, 1);
        assert_eq!(header.header_size, 256);
        assert_eq!(header.sample_rate, 48_000);
        assert_eq!(header.channels, 1);
        assert_eq!(header.sample_format, RaufSampleFormat::S32);
        assert!(header.interleaved);
        assert_eq!(header.project_start_sample, 960_000);
        assert_eq!(header.data_offset, 256);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn frames_written_and_finalize_flag_are_correct() {
        let path = temp_path("frames");
        let mut writer = RaufWriter::create(&path, config(1)).unwrap();
        writer.write_s32le_interleaved(&[1, 2, 3, 4]).unwrap();
        assert_eq!(writer.frames_written(), 4);
        let report = writer.finalize().unwrap();
        assert_eq!(report.frames_written, 4);

        let reader = RaufReader::open(&path).unwrap();
        assert_eq!(reader.header().frames_written, 4);
        assert_ne!(reader.header().flags & RAUF_FLAG_FINALIZED, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recovery_computes_frames_from_file_size() {
        let path = temp_path("recover");
        let mut writer = RaufWriter::create(&path, config(1)).unwrap();
        writer.write_s32le_interleaved(&[1, 2, 3]).unwrap();
        drop(writer);

        let reader = RaufReader::open(&path).unwrap();
        assert_eq!(reader.header().flags & RAUF_FLAG_FINALIZED, 0);
        assert_eq!(reader.recover_frames_from_size().unwrap(), 3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_magic_is_rejected() {
        let path = temp_path("bad-magic");
        std::fs::write(&path, [0u8; RAUF_HEADER_SIZE as usize]).unwrap();
        assert!(matches!(
            RaufReader::open(&path),
            Err(RaufError::InvalidMagic)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn blocks_reach_the_file_in_order_through_the_write_buffer() {
        let path = temp_path("blocks");
        let mut writer = RaufWriter::create(&path, config(2)).unwrap();
        writer.write_s32le_interleaved(&[1, -2]).unwrap();
        writer
            .write_s32le_interleaved(&[3, 4, i32::MIN, i32::MAX])
            .unwrap();
        let report = writer.finalize().unwrap();
        assert_eq!(report.frames_written, 3);
        assert_eq!(report.data_bytes, 24);

        let bytes = std::fs::read(&path).unwrap();
        let samples: Vec<i32> = bytes[RAUF_DATA_OFFSET as usize..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert_eq!(samples, [1, -2, 3, 4, i32::MIN, i32::MAX]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_writer_dropped_without_finalize_still_leaves_its_samples() {
        // What a crash mid-take relies on: the buffer reaches the file, and
        // recovery reads the length back from its size.
        let path = temp_path("dropped");
        let mut writer = RaufWriter::create(&path, config(1)).unwrap();
        for block in 0..10 {
            writer.write_s32le_interleaved(&[block; 64]).unwrap();
        }
        drop(writer);
        let reader = RaufReader::open(&path).unwrap();
        assert_eq!(reader.recover_frames_from_size().unwrap(), 640);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stereo_interleaved_frame_counting() {
        let path = temp_path("stereo");
        let mut writer = RaufWriter::create(&path, config(2)).unwrap();
        writer.write_s32le_interleaved(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(writer.frames_written(), 3);
        writer.finalize().unwrap();

        let reader = RaufReader::open(&path).unwrap();
        assert_eq!(reader.header().frames_written, 3);
        assert_eq!(reader.recover_frames_from_size().unwrap(), 3);
        let _ = std::fs::remove_file(path);
    }
}
