use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::format::{
    AudioEncodeSpec, AudioEncodeSummary, AudioEncoder, AudioFileFormat, AudioSampleFormat,
    EncodeError, WavEncodeOptions, check_interleaved_len, f32_to_i16, f32_to_i24, f32_to_i32,
    i32_to_f32,
};
use crate::rauf::{RAUF_FLAG_FINALIZED, RaufError, RaufReader, RaufSampleFormat};

pub type Result<T> = std::result::Result<T, WavError>;

#[derive(Debug)]
pub enum WavError {
    Io(std::io::Error),
    Rauf(RaufError),
    UnsupportedFormat(String),
    TooLarge(String),
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Rauf(error) => write!(f, "{error}"),
            Self::UnsupportedFormat(message) => write!(f, "unsupported WAV conversion: {message}"),
            Self::TooLarge(message) => write!(f, "WAV file too large: {message}"),
        }
    }
}

impl std::error::Error for WavError {}

impl From<std::io::Error> for WavError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<RaufError> for WavError {
    fn from(value: RaufError) -> Self {
        Self::Rauf(value)
    }
}

#[derive(Debug, Clone)]
pub struct WavPcmConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

#[derive(Debug, Clone)]
pub struct WavReport {
    pub frames_written: u64,
    pub data_bytes: u64,
}

pub fn write_s32le_wav(
    path: impl AsRef<Path>,
    config: WavPcmConfig,
    samples: &[i32],
) -> Result<WavReport> {
    validate_s32_config(&config)?;
    if !samples.len().is_multiple_of(config.channels as usize) {
        return Err(WavError::UnsupportedFormat(format!(
            "sample count {} is not divisible by channels {}",
            samples.len(),
            config.channels
        )));
    }
    let data_bytes = (samples.len() as u64)
        .checked_mul(4)
        .ok_or_else(|| WavError::TooLarge("sample byte count overflow".to_string()))?;
    let mut file = File::create(path)?;
    write_pcm_header(&mut file, &config, data_bytes)?;
    for sample in samples {
        file.write_all(&sample.to_le_bytes())?;
    }
    file.flush()?;
    Ok(WavReport {
        frames_written: samples.len() as u64 / config.channels as u64,
        data_bytes,
    })
}

pub fn convert_rauf_to_wav(
    rauf_path: impl AsRef<Path>,
    wav_path: impl AsRef<Path>,
) -> Result<WavReport> {
    let rauf_path = rauf_path.as_ref();
    let reader = RaufReader::open(rauf_path)?;
    let header = reader.header().clone();
    if header.sample_format != RaufSampleFormat::S32 {
        return Err(WavError::UnsupportedFormat(
            "only RAUF s32le can be exported to WAV in this path".to_string(),
        ));
    }
    if !header.interleaved {
        return Err(WavError::UnsupportedFormat(
            "non-interleaved RAUF is not supported".to_string(),
        ));
    }

    let frames = if header.flags & RAUF_FLAG_FINALIZED != 0 {
        header.frames_written
    } else {
        reader.recover_frames_from_size()?
    };
    let data_bytes = frames
        .checked_mul(header.channels as u64)
        .and_then(|samples| samples.checked_mul(4))
        .ok_or_else(|| WavError::TooLarge("RAUF data byte count overflow".to_string()))?;

    let config = WavPcmConfig {
        sample_rate: header.sample_rate,
        channels: header.channels,
        bits_per_sample: 32,
    };
    validate_s32_config(&config)?;

    let mut input = File::open(rauf_path)?;
    input.seek(SeekFrom::Start(header.data_offset))?;
    let mut output = File::create(wav_path)?;
    write_pcm_header(&mut output, &config, data_bytes)?;
    copy_exact_bytes(&mut input, &mut output, data_bytes)?;
    output.flush()?;
    output.sync_all()?;

    Ok(WavReport {
        frames_written: frames,
        data_bytes,
    })
}

fn validate_s32_config(config: &WavPcmConfig) -> Result<()> {
    if config.sample_rate == 0 {
        return Err(WavError::UnsupportedFormat(
            "sample_rate must be non-zero".to_string(),
        ));
    }
    if config.channels == 0 {
        return Err(WavError::UnsupportedFormat(
            "channels must be non-zero".to_string(),
        ));
    }
    if config.bits_per_sample != 32 {
        return Err(WavError::UnsupportedFormat(format!(
            "expected 32-bit PCM, got {}",
            config.bits_per_sample
        )));
    }
    Ok(())
}

fn write_pcm_header(file: &mut File, config: &WavPcmConfig, data_bytes: u64) -> Result<()> {
    if data_bytes > u32::MAX as u64 {
        return Err(WavError::TooLarge(format!(
            "data chunk is {data_bytes} bytes; RIFF/WAVE v1 limit is {}",
            u32::MAX
        )));
    }
    let data_size = data_bytes as u32;
    let riff_size = data_size
        .checked_add(36)
        .ok_or_else(|| WavError::TooLarge("RIFF size overflow".to_string()))?;
    let block_align = config.channels * 4;
    let byte_rate = config.sample_rate * block_align as u32;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&config.channels.to_le_bytes())?;
    file.write_all(&config.sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&config.bits_per_sample.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_size.to_le_bytes())?;
    Ok(())
}

fn copy_exact_bytes(input: &mut File, output: &mut File, bytes: u64) -> Result<()> {
    let mut remaining = bytes;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        input.read_exact(&mut buffer[..wanted])?;
        output.write_all(&buffer[..wanted])?;
        remaining -= wanted as u64;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming WAV encoder implementing the generic AudioEncoder trait.
// ---------------------------------------------------------------------------

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// Size of the `JUNK` chunk an RF64-ready header reserves ahead of `fmt `:
/// the 8-byte chunk header plus the 28-byte `ds64` payload it becomes.
const RF64_RESERVE_BYTES: u64 = 8 + 28;
/// Offset of the reserved chunk's id (`JUNK`, later `ds64`).
const RF64_RESERVE_AT: u64 = 12;
/// Data offset of the classic 44-byte header.
const PLAIN_DATA_OFFSET: u64 = 44;

/// Streaming RIFF/WAVE encoder supporting PCM 16/24/32-bit and Float32.
///
/// The header is written up-front with placeholder sizes and patched on
/// [`finalize`](AudioEncoder::finalize) by seeking back, and on every
/// [`checkpoint`](AudioEncoder::checkpoint) so a file whose writer dies still
/// opens. Samples are streamed straight through a `BufWriter`, so memory use
/// stays bounded regardless of render length.
///
/// With [`WavEncodeOptions::rf64`] the file may grow past 4 GiB: the sizes
/// are then written as RF64 (see [`patch_sizes`]).
pub struct WavEncoder {
    writer: BufWriter<File>,
    spec: AudioEncodeSpec,
    data_bytes: u64,
    /// Whether the header reserves room for `ds64`.
    rf64: bool,
    finalized: bool,
    scratch: Vec<u8>,
}

impl WavEncoder {
    pub fn create(
        path: impl AsRef<Path>,
        spec: AudioEncodeSpec,
        options: WavEncodeOptions,
    ) -> std::result::Result<Self, EncodeError> {
        spec.validate()?;
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        write_stream_header_placeholder(&mut writer, &spec, options.rf64)?;
        Ok(Self {
            writer,
            spec,
            data_bytes: 0,
            rf64: options.rf64,
            finalized: false,
            scratch: Vec::new(),
        })
    }

    fn frames_written(&self) -> u64 {
        let bytes_per_frame =
            self.spec.channels as u64 * self.spec.sample_format.bytes_per_sample() as u64;
        self.data_bytes.checked_div(bytes_per_frame).unwrap_or(0)
    }

    /// Flush what is buffered and write the sizes the file has now into its
    /// header, leaving the file positioned at its end for further samples.
    fn flush_and_patch(&mut self) -> std::result::Result<(), EncodeError> {
        self.writer.flush()?;
        let frames = self.frames_written();
        let (data_bytes, rf64) = (self.data_bytes, self.rf64);
        let file = self.writer.get_mut();
        patch_sizes(file, data_bytes, frames, rf64)?;
        file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn push_sample_bytes(
        &mut self,
        encode: impl Fn(&mut Vec<u8>),
    ) -> std::result::Result<(), EncodeError> {
        self.scratch.clear();
        encode(&mut self.scratch);
        self.writer.write_all(&self.scratch)?;
        self.data_bytes += self.scratch.len() as u64;
        Ok(())
    }
}

fn write_stream_header_placeholder(
    writer: &mut BufWriter<File>,
    spec: &AudioEncodeSpec,
    rf64: bool,
) -> std::result::Result<(), EncodeError> {
    let audio_format = if spec.sample_format.is_float() {
        WAVE_FORMAT_IEEE_FLOAT
    } else {
        WAVE_FORMAT_PCM
    };
    let bits = spec.sample_format.bits();
    let block_align = spec.channels * spec.sample_format.bytes_per_sample() as u16;
    let byte_rate = spec.sample_rate * block_align as u32;

    writer.write_all(b"RIFF")?;
    writer.write_all(&0u32.to_le_bytes())?; // patched at finalize
    writer.write_all(b"WAVE")?;
    if rf64 {
        // Becomes `ds64` if the data outgrows RIFF; skipped by readers until then.
        writer.write_all(b"JUNK")?;
        writer.write_all(&28u32.to_le_bytes())?;
        writer.write_all(&[0u8; 28])?;
    }
    writer.write_all(b"fmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    writer.write_all(&audio_format.to_le_bytes())?;
    writer.write_all(&spec.channels.to_le_bytes())?;
    writer.write_all(&spec.sample_rate.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&block_align.to_le_bytes())?;
    writer.write_all(&bits.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&0u32.to_le_bytes())?; // patched at finalize
    Ok(())
}

/// Write the sizes of a stream header made by
/// [`write_stream_header_placeholder`] for `data_bytes` of samples
/// (`frames` frames).
///
/// While the file fits RIFF (a RIFF size of at most `u32::MAX`) it is a plain
/// RIFF/WAVE. Past that, an RF64-ready header becomes RF64: `RF64` with
/// `0xFFFFFFFF` sizes, the true 64-bit sizes in `ds64`. Without the reserve
/// the file cannot grow past RIFF and this fails, leaving it unpatched.
fn patch_sizes(
    file: &mut File,
    data_bytes: u64,
    frames: u64,
    rf64: bool,
) -> std::result::Result<(), EncodeError> {
    let data_offset = if rf64 {
        PLAIN_DATA_OFFSET + RF64_RESERVE_BYTES
    } else {
        PLAIN_DATA_OFFSET
    };
    // Everything after the 8-byte RIFF chunk header.
    let riff_size = (data_offset - 8)
        .checked_add(data_bytes)
        .ok_or_else(|| EncodeError::FinalizeFailed("RIFF size overflow".to_string()))?;
    if riff_size <= u64::from(u32::MAX) {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"RIFF")?;
        file.write_all(&(riff_size as u32).to_le_bytes())?;
        file.seek(SeekFrom::Start(data_offset - 4))?;
        file.write_all(&(data_bytes as u32).to_le_bytes())?;
        return Ok(());
    }
    if !rf64 {
        return Err(EncodeError::FinalizeFailed(format!(
            "data chunk is {data_bytes} bytes; RIFF/WAVE v1 limit is {}",
            u32::MAX
        )));
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"RF64")?;
    file.write_all(&u32::MAX.to_le_bytes())?;
    file.seek(SeekFrom::Start(RF64_RESERVE_AT))?;
    let mut ds64 = [0u8; RF64_RESERVE_BYTES as usize];
    ds64[0..4].copy_from_slice(b"ds64");
    ds64[4..8].copy_from_slice(&28u32.to_le_bytes());
    ds64[8..16].copy_from_slice(&riff_size.to_le_bytes());
    ds64[16..24].copy_from_slice(&data_bytes.to_le_bytes());
    ds64[24..32].copy_from_slice(&frames.to_le_bytes());
    // ds64[32..36]: an empty chunk-size table.
    file.write_all(&ds64)?;
    file.seek(SeekFrom::Start(data_offset - 4))?;
    file.write_all(&u32::MAX.to_le_bytes())?;
    Ok(())
}

impl AudioEncoder for WavEncoder {
    fn format(&self) -> AudioFileFormat {
        AudioFileFormat::Wav
    }

    fn spec(&self) -> AudioEncodeSpec {
        self.spec.clone()
    }

    fn write_interleaved_f32(&mut self, frames: &[f32]) -> std::result::Result<(), EncodeError> {
        if self.finalized {
            return Err(EncodeError::InvalidInput(
                "write after finalize".to_string(),
            ));
        }
        check_interleaved_len(frames.len(), self.spec.channels)?;
        let fmt = self.spec.sample_format;
        for &x in frames {
            self.push_sample_bytes(|buf| encode_one(buf, fmt, SampleIn::F32(x)))?;
        }
        Ok(())
    }

    fn write_interleaved_i32(&mut self, frames: &[i32]) -> std::result::Result<(), EncodeError> {
        if self.finalized {
            return Err(EncodeError::InvalidInput(
                "write after finalize".to_string(),
            ));
        }
        check_interleaved_len(frames.len(), self.spec.channels)?;
        let fmt = self.spec.sample_format;
        for &x in frames {
            self.push_sample_bytes(|buf| encode_one(buf, fmt, SampleIn::I32(x)))?;
        }
        Ok(())
    }

    fn finalize(&mut self) -> std::result::Result<AudioEncodeSummary, EncodeError> {
        if self.finalized {
            return Err(EncodeError::FinalizeFailed(
                "encoder already finalized".to_string(),
            ));
        }
        self.finalized = true;
        self.flush_and_patch()?;
        let file = self.writer.get_mut();
        file.flush()?;
        file.sync_all()?;

        let frames_written = self.frames_written();
        Ok(AudioEncodeSummary {
            format: AudioFileFormat::Wav,
            sample_rate: self.spec.sample_rate,
            channels: self.spec.channels,
            sample_format: self.spec.sample_format,
            frames_written,
            bytes_written: self.data_bytes,
        })
    }

    /// Patch the header to the samples written so far and push them to the
    /// disk, so a writer that dies leaves a file that opens to this point.
    fn checkpoint(&mut self) -> std::result::Result<(), EncodeError> {
        if self.finalized {
            return Ok(());
        }
        self.flush_and_patch()?;
        self.writer.get_mut().sync_data()?;
        Ok(())
    }
}

enum SampleIn {
    F32(f32),
    I32(i32),
}

fn encode_one(buf: &mut Vec<u8>, fmt: AudioSampleFormat, sample: SampleIn) {
    match fmt {
        AudioSampleFormat::F32 => {
            let v = match sample {
                SampleIn::F32(x) => x,
                SampleIn::I32(x) => i32_to_f32(x),
            };
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AudioSampleFormat::I32 => {
            let v = match sample {
                SampleIn::F32(x) => f32_to_i32(x),
                SampleIn::I32(x) => x,
            };
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AudioSampleFormat::I16 => {
            let v = match sample {
                SampleIn::F32(x) => f32_to_i16(x),
                SampleIn::I32(x) => (x >> 16) as i16,
            };
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AudioSampleFormat::I24 => {
            let v = match sample {
                SampleIn::F32(x) => f32_to_i24(x),
                SampleIn::I32(x) => x >> 8,
            };
            let bytes = v.to_le_bytes();
            buf.extend_from_slice(&bytes[0..3]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rauf::{RaufConfig, RaufSampleFormat, RaufWriter};

    fn temp_path(name: &str, ext: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "futureboard-{name}-{}-{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            ext
        ));
        path
    }

    #[test]
    fn writes_s32le_wav_header_and_payload() {
        let path = temp_path("s32-wav", "wav");
        let report = write_s32le_wav(
            &path,
            WavPcmConfig {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 32,
            },
            &[1, -1, i32::MAX, i32::MIN],
        )
        .unwrap();
        assert_eq!(report.frames_written, 2);
        assert_eq!(report.data_bytes, 16);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 1);
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 32);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16);
        assert_eq!(i32::from_le_bytes(bytes[44..48].try_into().unwrap()), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn converts_rauf_to_wav_without_ffmpeg() {
        let rauf_path = temp_path("convert", "rauf");
        let wav_path = temp_path("convert", "wav");
        let mut writer = RaufWriter::create(
            &rauf_path,
            RaufConfig {
                sample_rate: 44_100,
                channels: 2,
                sample_format: RaufSampleFormat::S32,
                interleaved: true,
                project_start_sample: 128,
                take_id: [9; 16],
            },
        )
        .unwrap();
        writer
            .write_s32le_interleaved(&[10, 20, 30, 40, 50, 60])
            .unwrap();
        writer.finalize().unwrap();

        let report = convert_rauf_to_wav(&rauf_path, &wav_path).unwrap();
        assert_eq!(report.frames_written, 3);
        assert_eq!(report.data_bytes, 24);

        let bytes = std::fs::read(&wav_path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            44_100
        );
        assert_eq!(i32::from_le_bytes(bytes[44..48].try_into().unwrap()), 10);
        assert_eq!(i32::from_le_bytes(bytes[64..68].try_into().unwrap()), 60);

        let _ = std::fs::remove_file(rauf_path);
        let _ = std::fs::remove_file(wav_path);
    }
}

#[cfg(test)]
mod encoder_tests {
    use super::*;
    use crate::format::AudioEncodeSpec;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "futureboard-{name}-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    #[test]
    fn float32_wav_streams_header_and_payload() {
        let path = temp_path("f32");
        let mut enc = WavEncoder::create(
            &path,
            AudioEncodeSpec {
                sample_rate: 48_000,
                channels: 2,
                sample_format: AudioSampleFormat::F32,
            },
            WavEncodeOptions::default(),
        )
        .unwrap();
        enc.write_interleaved_f32(&[0.0, 1.0, -1.0, 0.5]).unwrap();
        let summary = enc.finalize().unwrap();
        assert_eq!(summary.frames_written, 2);
        assert_eq!(summary.bytes_written, 16);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // IEEE float format code.
        assert_eq!(
            u16::from_le_bytes([bytes[20], bytes[21]]),
            WAVE_FORMAT_IEEE_FLOAT
        );
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 32);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16);
        assert_eq!(f32::from_le_bytes(bytes[44..48].try_into().unwrap()), 0.0);
        assert_eq!(f32::from_le_bytes(bytes[48..52].try_into().unwrap()), 1.0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pcm16_wav_clamps_and_rounds() {
        let path = temp_path("i16");
        let mut enc = WavEncoder::create(
            &path,
            AudioEncodeSpec {
                sample_rate: 44_100,
                channels: 1,
                sample_format: AudioSampleFormat::I16,
            },
            WavEncodeOptions::default(),
        )
        .unwrap();
        enc.write_interleaved_f32(&[1.0, -1.0, 2.0]).unwrap();
        let summary = enc.finalize().unwrap();
        assert_eq!(summary.frames_written, 3);
        assert_eq!(summary.bytes_written, 6);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), WAVE_FORMAT_PCM);
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 16);
        assert_eq!(i16::from_le_bytes([bytes[44], bytes[45]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), -i16::MAX);
        // 2.0 clamps to full-scale, not wraps.
        assert_eq!(i16::from_le_bytes([bytes[48], bytes[49]]), i16::MAX);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_misaligned_interleaved_input() {
        let path = temp_path("misaligned");
        let mut enc = WavEncoder::create(
            &path,
            AudioEncodeSpec {
                sample_rate: 48_000,
                channels: 2,
                sample_format: AudioSampleFormat::I16,
            },
            WavEncodeOptions::default(),
        )
        .unwrap();
        let err = enc.write_interleaved_f32(&[0.1, 0.2, 0.3]).unwrap_err();
        assert!(matches!(err, EncodeError::InvalidInput(_)));
        let _ = enc.finalize();
        let _ = std::fs::remove_file(path);
    }

    fn spec(channels: u16, sample_format: AudioSampleFormat) -> AudioEncodeSpec {
        AudioEncodeSpec {
            sample_rate: 48_000,
            channels,
            sample_format,
        }
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// A writer that dies after a checkpoint (dropped without `finalize`)
    /// leaves a header that describes every sample up to the checkpoint,
    /// rather than the zero sizes it was created with.
    #[test]
    fn a_checkpoint_leaves_a_readable_header_if_the_writer_dies() {
        let path = temp_path("checkpoint");
        let mut enc = WavEncoder::create(
            &path,
            spec(2, AudioSampleFormat::I16),
            WavEncodeOptions::default(),
        )
        .unwrap();
        enc.write_interleaved_f32(&[0.5; 8]).unwrap(); // 4 frames, 16 bytes
        enc.checkpoint().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 16, "the samples are on disk");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32_at(&bytes, 4), 36 + 16);
        assert_eq!(u32_at(&bytes, 40), 16);

        // More samples after the checkpoint, then the process "dies".
        enc.write_interleaved_f32(&[0.25; 4]).unwrap();
        enc.checkpoint().unwrap();
        enc.write_interleaved_f32(&[0.125; 4]).unwrap();
        drop(enc);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(u32_at(&bytes, 40), 24, "sized to the last checkpoint");
        assert_eq!(u32_at(&bytes, 4), 36 + 24);
        let _ = std::fs::remove_file(path);
    }

    /// An RF64-ready file that stays under 4 GiB is a plain RIFF/WAVE whose
    /// reserve is a `JUNK` chunk every reader skips.
    #[test]
    fn an_rf64_ready_file_under_4_gib_is_plain_riff_with_a_junk_chunk() {
        let path = temp_path("rf64-small");
        let mut enc = WavEncoder::create(
            &path,
            spec(1, AudioSampleFormat::F32),
            WavEncodeOptions { rf64: true },
        )
        .unwrap();
        enc.write_interleaved_f32(&[0.5, -0.5, 0.25]).unwrap();
        let summary = enc.finalize().unwrap();
        assert_eq!(summary.frames_written, 3);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 80 + 12);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32_at(&bytes, 4), bytes.len() as u32 - 8);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"JUNK");
        assert_eq!(u32_at(&bytes, 16), 28);
        assert_eq!(&bytes[48..52], b"fmt ");
        assert_eq!(
            u16::from_le_bytes([bytes[56], bytes[57]]),
            WAVE_FORMAT_IEEE_FLOAT
        );
        assert_eq!(&bytes[72..76], b"data");
        assert_eq!(u32_at(&bytes, 76), 12);
        assert_eq!(f32::from_le_bytes(bytes[80..84].try_into().unwrap()), 0.5);
        let _ = std::fs::remove_file(path);
    }

    /// Past the RIFF limit the same header is rewritten as RF64: `RF64`,
    /// `0xFFFFFFFF` sizes, and the 64-bit sizes in `ds64`. Patched directly
    /// with a size past 4 GiB rather than by writing one.
    #[test]
    fn past_4_gib_the_header_becomes_rf64_with_a_ds64_chunk() {
        let path = temp_path("rf64-large");
        let mut enc = WavEncoder::create(
            &path,
            spec(2, AudioSampleFormat::F32),
            WavEncodeOptions { rf64: true },
        )
        .unwrap();
        enc.write_interleaved_f32(&[0.0; 4]).unwrap();
        enc.writer.flush().unwrap();
        let data_bytes = 5_000_000_000u64;
        let frames = data_bytes / 8;
        patch_sizes(enc.writer.get_mut(), data_bytes, frames, true).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RF64");
        assert_eq!(u32_at(&bytes, 4), u32::MAX);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"ds64");
        assert_eq!(u32_at(&bytes, 16), 28);
        assert_eq!(u64_at(&bytes, 20), 72 + data_bytes, "RIFF size");
        assert_eq!(u64_at(&bytes, 28), data_bytes, "data size");
        assert_eq!(u64_at(&bytes, 36), frames, "sample count");
        assert_eq!(u32_at(&bytes, 44), 0, "no chunk-size table");
        assert_eq!(&bytes[48..52], b"fmt ");
        assert_eq!(&bytes[72..76], b"data");
        assert_eq!(u32_at(&bytes, 76), u32::MAX);
        drop(enc);
        let _ = std::fs::remove_file(path);
    }

    /// Without the reserve a file cannot outgrow RIFF: that is still an
    /// error, never a header with wrapped sizes.
    #[test]
    fn a_plain_header_refuses_sizes_past_the_riff_limit() {
        let path = temp_path("riff-limit");
        let mut enc = WavEncoder::create(
            &path,
            spec(1, AudioSampleFormat::I16),
            WavEncodeOptions::default(),
        )
        .unwrap();
        enc.writer.flush().unwrap();
        assert!(matches!(
            patch_sizes(enc.writer.get_mut(), u64::from(u32::MAX), 1, false),
            Err(EncodeError::FinalizeFailed(_))
        ));
        drop(enc);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn double_finalize_errors_not_panics() {
        let path = temp_path("double");
        let mut enc = WavEncoder::create(
            &path,
            AudioEncodeSpec {
                sample_rate: 48_000,
                channels: 1,
                sample_format: AudioSampleFormat::I32,
            },
            WavEncodeOptions::default(),
        )
        .unwrap();
        enc.finalize().unwrap();
        assert!(matches!(
            enc.finalize(),
            Err(EncodeError::FinalizeFailed(_))
        ));
        let _ = std::fs::remove_file(path);
    }
}
