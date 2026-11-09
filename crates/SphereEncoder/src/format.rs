//! Format-agnostic encoding surface for SphereEncoder.
//!
//! SphereEncoder is responsible only for turning interleaved PCM frames into a
//! valid container on disk. It never renders audio and never spawns external
//! processes — in particular there is no FFmpeg/libav anywhere in this crate.
//! Arrangement rendering lives in the audio engine; building an export request
//! from project state lives in the UI/layout layer.

use std::path::Path;

use crate::metadata::AudioMetadata;

/// Container/codec a render is written to.
///
/// Stored by its lower-case id (`"wav"`, `"flac"`, `"opus"`, `"rauf"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFileFormat {
    #[default]
    Wav,
    Flac,
    /// Ogg Opus (RFC 7845), encoded with libopus.
    ///
    /// Replaced MP3 export, whose LAME encoder is LGPL (#284): a stored
    /// `"mp3"` reads back as Opus, so nothing that saved MP3 fails to load.
    #[serde(alias = "mp3", alias = "Mp3", alias = "MP3")]
    Opus,
    /// Internal Futureboard recording/cache format. Useful as a debug export
    /// target; not expected to be readable by general-purpose tools.
    Rauf,
}

impl AudioFileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Opus => "opus",
            Self::Rauf => "rauf",
        }
    }

    /// File extension (without the dot) conventionally used for this format.
    pub fn extension(self) -> &'static str {
        self.as_str()
    }

    /// The format an id from [`as_str`](Self::as_str) names, ignoring case.
    /// The retired `"mp3"` id maps to [`Opus`](Self::Opus), as in serde.
    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "wav" => Some(Self::Wav),
            "flac" => Some(Self::Flac),
            "opus" | "mp3" => Some(Self::Opus),
            "rauf" => Some(Self::Rauf),
            _ => None,
        }
    }
}

impl std::fmt::Display for AudioFileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Target sample representation inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSampleFormat {
    F32,
    I16,
    I24,
    I32,
}

impl AudioSampleFormat {
    pub fn bits(self) -> u16 {
        match self {
            Self::I16 => 16,
            Self::I24 => 24,
            Self::I32 | Self::F32 => 32,
        }
    }

    /// Bytes one sample occupies on disk for PCM containers.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 => 4,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32)
    }
}

/// Stream geometry shared by every encoder.
#[derive(Debug, Clone)]
pub struct AudioEncodeSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: AudioSampleFormat,
}

impl AudioEncodeSpec {
    pub fn validate(&self) -> Result<(), EncodeError> {
        if self.sample_rate == 0 {
            return Err(EncodeError::UnsupportedSpec(
                "sample_rate must be non-zero".to_string(),
            ));
        }
        if self.channels == 0 {
            return Err(EncodeError::UnsupportedSpec(
                "channels must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// Per-format encode summary returned by [`AudioEncoder::finalize`].
#[derive(Debug, Clone)]
pub struct AudioEncodeSummary {
    pub format: AudioFileFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: AudioSampleFormat,
    pub frames_written: u64,
    pub bytes_written: u64,
}

// ---------------------------------------------------------------------------
// Per-format options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct WavEncodeOptions {
    /// Reserve room for an RF64 header (EBU Tech 3306) so the file may grow
    /// past the 4 GiB RIFF limit.
    ///
    /// The header carries a 36-byte `JUNK` chunk ahead of `fmt `, which every
    /// RIFF reader skips. While the data fits RIFF the file stays a plain
    /// RIFF/WAVE; once it does not, the header is rewritten in place as
    /// `RF64` with that chunk turned into `ds64`. Off by default: a file
    /// written without it keeps the classic 44-byte header.
    pub rf64: bool,
}

#[derive(Debug, Clone)]
pub struct FlacEncodeOptions {
    /// flacenc compression preset (0..=8). Higher = smaller/slower.
    pub compression_level: u8,
    /// Fixed block size in frames.
    pub block_size: usize,
}

impl Default for FlacEncodeOptions {
    fn default() -> Self {
        Self {
            compression_level: 5,
            block_size: 4096,
        }
    }
}

/// The rate every Opus stream decodes at (and its granule-position unit),
/// whatever the source rate was.
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Target bitrate of an Opus export (VBR), for the whole stream rather than
/// per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpusBitrate {
    Kbps64,
    Kbps96,
    Kbps128,
    #[default]
    Kbps160,
    Kbps192,
    Kbps256,
}

impl OpusBitrate {
    /// Every preset, lowest first.
    pub const ALL: [Self; 6] = [
        Self::Kbps64,
        Self::Kbps96,
        Self::Kbps128,
        Self::Kbps160,
        Self::Kbps192,
        Self::Kbps256,
    ];

    pub fn from_kbps(kbps: u32) -> Result<Self, EncodeError> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.kbps() == kbps)
            .ok_or_else(|| {
                EncodeError::UnsupportedSpec(format!(
                    "unsupported Opus bitrate {kbps} kbps (expected 64/96/128/160/192/256)"
                ))
            })
    }

    pub fn kbps(self) -> u32 {
        match self {
            Self::Kbps64 => 64,
            Self::Kbps96 => 96,
            Self::Kbps128 => 128,
            Self::Kbps160 => 160,
            Self::Kbps192 => 192,
            Self::Kbps256 => 256,
        }
    }

    pub fn bits_per_second(self) -> i32 {
        self.kbps() as i32 * 1000
    }

    /// The Opus preset that stands in for an MP3 CBR bitrate saved before
    /// MP3 export was removed (#284): about the same quality, not the same
    /// number, since Opus needs far fewer bits. MP3's 256 kbps default maps
    /// to Opus's 160 kbps default. Never fails: any other value takes the
    /// nearest step.
    pub fn from_legacy_mp3_kbps(kbps: u32) -> Self {
        match kbps {
            0..=112 => Self::Kbps64,
            113..=160 => Self::Kbps96,
            161..=224 => Self::Kbps128,
            225..=288 => Self::Kbps160,
            _ => Self::Kbps192,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpusEncodeOptions {
    pub bitrate: OpusBitrate,
    /// libopus encoder complexity, 0 (fastest) ..= 10 (best). Export is
    /// offline, so the default spends the CPU.
    pub complexity: u8,
}

impl Default for OpusEncodeOptions {
    fn default() -> Self {
        Self {
            bitrate: OpusBitrate::default(),
            complexity: 10,
        }
    }
}

/// Word-length reduction used when samples are written at 16 or 24 bits.
///
/// Float and 32-bit integer containers are never dithered: their resolution
/// exceeds the mix's, so there is no truncation distortion to decorrelate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioDither {
    /// Round to nearest with no dither. Quiet fades and reverb tails then
    /// carry correlated quantization distortion. The default, so callers such
    /// as LiveStage's recorder keep writing exactly what they are given.
    #[default]
    Off,
    /// One triangular-PDF dither sample of ±1 LSB at the target depth, then
    /// round to nearest — the same treatment Studio's live device output gets.
    /// Studio's arrangement export selects it.
    Tpdf,
}

/// Bundle of every per-format option plus metadata.
#[derive(Debug, Clone, Default)]
pub struct AudioEncodeOptions {
    pub format: AudioFileFormat,
    /// Dither applied when float samples are quantized to a 16- or 24-bit
    /// integer stream (WAV, FLAC). Opus encodes the float samples, so it is
    /// never dithered.
    pub dither: AudioDither,
    pub wav: WavEncodeOptions,
    pub flac: FlacEncodeOptions,
    pub opus: OpusEncodeOptions,
    pub metadata: AudioMetadata,
}

// ---------------------------------------------------------------------------
// Error model
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum EncodeError {
    Io(std::io::Error),
    UnsupportedFormat(AudioFileFormat),
    UnsupportedSpec(String),
    /// The format is recognized but its codec is not compiled into this build
    /// (e.g. Opus without the `opus` feature). Never silently faked.
    CodecUnavailable {
        format: AudioFileFormat,
        reason: String,
    },
    InvalidInput(String),
    FinalizeFailed(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::UnsupportedFormat(format) => write!(f, "unsupported format: {format}"),
            Self::UnsupportedSpec(message) => write!(f, "unsupported encode spec: {message}"),
            Self::CodecUnavailable { format, reason } => {
                write!(f, "{format} codec unavailable: {reason}")
            }
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::FinalizeFailed(message) => write!(f, "finalize failed: {message}"),
        }
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for EncodeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

// ---------------------------------------------------------------------------
// Encoder trait + factory
// ---------------------------------------------------------------------------

/// A streaming PCM encoder. Frames are pushed interleaved; the encoder owns its
/// output file and finalizes the container on [`finalize`](AudioEncoder::finalize).
///
/// Implementations must not panic on bad input or codec errors — every failure
/// surfaces as an [`EncodeError`].
pub trait AudioEncoder: Send {
    fn format(&self) -> AudioFileFormat;
    fn spec(&self) -> AudioEncodeSpec;

    /// Push interleaved `[-1.0, 1.0]` float frames. Length must be a multiple of
    /// `channels`.
    fn write_interleaved_f32(&mut self, frames: &[f32]) -> Result<(), EncodeError>;

    /// Push interleaved full-scale signed 32-bit frames. Length must be a
    /// multiple of `channels`.
    fn write_interleaved_i32(&mut self, frames: &[i32]) -> Result<(), EncodeError>;

    /// Flush, patch headers/trailers, and close the file. Calling twice is a
    /// no-op-safe error rather than a panic.
    fn finalize(&mut self) -> Result<AudioEncodeSummary, EncodeError>;

    /// Make what has been written so far readable on disk, as if the stream
    /// ended here, without ending it: a process that dies before
    /// [`finalize`](AudioEncoder::finalize) then leaves a file that opens up
    /// to the last checkpoint. Blocks on file I/O; never call it from a
    /// realtime thread. Formats that cannot do this cheaply do nothing.
    fn checkpoint(&mut self) -> Result<(), EncodeError> {
        Ok(())
    }
}

/// Whether Opus encoding is compiled into this build (the `opus` feature). The
/// UI uses this to show Opus as available or disabled with a clear
/// explanation — never to silently fall back.
pub fn opus_available() -> bool {
    cfg!(feature = "opus")
}

/// Whether FLAC encoding is compiled into this build (the `flac` feature).
pub fn flac_available() -> bool {
    cfg!(feature = "flac")
}

/// Construct an encoder for `path` from a spec + options.
///
/// Returns [`EncodeError::CodecUnavailable`] (not a panic, not a fake success)
/// when a format's codec is not compiled into this build.
pub fn create_encoder(
    path: impl AsRef<Path>,
    spec: AudioEncodeSpec,
    options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    spec.validate()?;
    let path = path.as_ref();
    match options.format {
        AudioFileFormat::Wav => Ok(Box::new(
            crate::wav::WavEncoder::create(path, spec, options.wav)?.with_dither(options.dither),
        )),
        AudioFileFormat::Rauf => Ok(Box::new(crate::rauf::RaufEncoder::create(path, spec)?)),
        AudioFileFormat::Flac => create_flac_encoder(path, spec, options),
        AudioFileFormat::Opus => create_opus_encoder(path, spec, options),
    }
}

#[cfg(feature = "flac")]
fn create_flac_encoder(
    path: &Path,
    spec: AudioEncodeSpec,
    options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    Ok(Box::new(
        crate::flac::FlacEncoder::create(path, spec, options.flac, options.metadata)?
            .with_dither(options.dither),
    ))
}

#[cfg(not(feature = "flac"))]
fn create_flac_encoder(
    _path: &Path,
    _spec: AudioEncodeSpec,
    _options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    Err(EncodeError::CodecUnavailable {
        format: AudioFileFormat::Flac,
        reason: "this build was compiled without the `flac` feature".to_string(),
    })
}

#[cfg(feature = "opus")]
fn create_opus_encoder(
    path: &Path,
    spec: AudioEncodeSpec,
    options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    // No dither: Opus takes the float samples as they are.
    Ok(Box::new(crate::ogg_opus::OpusEncoder::create(
        path,
        spec,
        options.opus,
        options.metadata,
    )?))
}

#[cfg(not(feature = "opus"))]
fn create_opus_encoder(
    _path: &Path,
    _spec: AudioEncodeSpec,
    _options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    Err(EncodeError::CodecUnavailable {
        format: AudioFileFormat::Opus,
        reason: "Opus export is not available in this build (`opus` feature disabled)".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Shared sample conversion helpers
// ---------------------------------------------------------------------------

/// Validate that an interleaved buffer length divides evenly by `channels`.
pub(crate) fn check_interleaved_len(len: usize, channels: u16) -> Result<(), EncodeError> {
    let channels = channels as usize;
    if channels == 0 {
        return Err(EncodeError::UnsupportedSpec("channels is zero".to_string()));
    }
    if !len.is_multiple_of(channels) {
        return Err(EncodeError::InvalidInput(format!(
            "interleaved sample count {len} is not divisible by channels {channels}"
        )));
    }
    Ok(())
}

/// Full-scale `[-1, 1]` float to signed 32-bit. Clamps; never dithered (see
/// [`AudioDither`]).
#[inline]
pub(crate) fn f32_to_i32(x: f32) -> i32 {
    let v = (x.clamp(-1.0, 1.0) as f64) * (i32::MAX as f64);
    v.round() as i32
}

/// Base of the dither generators' seeds.
const DITHER_SEED: u32 = 0x9E37_79B9;

/// The dither seed for an encoder writing `path`: a hash of the path.
///
/// Every encoder used to start from the same seed, so a stem export gave the
/// mixdown and every stem byte-identical dither noise: summed later, it added
/// up coherently (about 16 dB over independent dither with 40 stems), and
/// silent stems became identical noise files (#362). Seeding from the file
/// path gives each file its own noise, while the same render written to the
/// same file still encodes to the same bytes.
pub(crate) fn dither_seed_for(path: &Path) -> u32 {
    // FNV-1a over the path bytes, folded into the base seed.
    let hash = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0x811C_9DC5u32 ^ DITHER_SEED, |hash, &byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        });
    if hash == 0 { DITHER_SEED } else { hash }
}

/// Float / 32-bit integer to 16- or 24-bit integer conversion, with the
/// encoder's [`AudioDither`]. One per encoder, advanced sample by sample, so
/// successive samples never share noise. No allocation, no locks.
#[derive(Debug, Clone)]
pub(crate) struct Quantizer {
    dither: AudioDither,
    /// Where the noise sequence starts; never zero (xorshift would stick).
    seed: u32,
    state: u32,
}

impl Quantizer {
    pub(crate) fn new(dither: AudioDither) -> Self {
        Self {
            dither,
            seed: DITHER_SEED,
            state: DITHER_SEED,
        }
    }

    /// An undithered quantizer whose dither, once switched on with
    /// [`Self::with_dither`], is seeded for the file at `path`.
    pub(crate) fn for_file(path: &Path) -> Self {
        let seed = dither_seed_for(path);
        Self {
            dither: AudioDither::Off,
            seed,
            state: seed,
        }
    }

    /// This quantizer's seed with `dither`, from the start of its sequence.
    pub(crate) fn with_dither(&self, dither: AudioDither) -> Self {
        Self {
            dither,
            seed: self.seed,
            state: self.seed,
        }
    }

    /// Uniform noise in `[0, 1)` from a 32-bit xorshift.
    #[inline]
    fn uniform(&mut self) -> f64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        f64::from(x >> 8) * (1.0 / 16_777_216.0)
    }

    /// One triangular-PDF dither sample in `(-1, 1)` LSB.
    #[inline]
    fn tpdf(&mut self) -> f64 {
        self.uniform() - self.uniform()
    }

    /// Full-scale `[-1, 1]` float to a signed integer whose positive full
    /// scale is `max`. Clamps; NaN becomes 0.
    #[inline]
    fn from_f32(&mut self, x: f32, max: f64) -> i32 {
        let scaled = f64::from(x.clamp(-1.0, 1.0)) * max;
        match self.dither {
            AudioDither::Off => scaled.round() as i32,
            AudioDither::Tpdf => (scaled + self.tpdf()).round().clamp(-max - 1.0, max) as i32,
        }
    }

    /// Full-scale signed 32-bit to `bits` bits (`bits < 32`).
    #[inline]
    fn from_i32(&mut self, x: i32, bits: u32) -> i32 {
        let shift = 32 - bits;
        match self.dither {
            AudioDither::Off => x >> shift,
            AudioDither::Tpdf => {
                let max = f64::from((1u32 << (bits - 1)) - 1);
                let scaled = f64::from(x) / f64::from(1u32 << shift);
                (scaled + self.tpdf()).round().clamp(-max - 1.0, max) as i32
            }
        }
    }

    #[inline]
    pub(crate) fn f32_to_i16(&mut self, x: f32) -> i16 {
        self.from_f32(x, f64::from(i16::MAX)) as i16
    }

    /// Float to signed 24-bit value stored in the low bytes of an `i32`.
    #[inline]
    pub(crate) fn f32_to_i24(&mut self, x: f32) -> i32 {
        self.from_f32(x, 8_388_607.0)
    }

    #[inline]
    pub(crate) fn i32_to_i16(&mut self, x: i32) -> i16 {
        self.from_i32(x, 16) as i16
    }

    #[inline]
    pub(crate) fn i32_to_i24(&mut self, x: i32) -> i32 {
        self.from_i32(x, 24)
    }
}

#[inline]
pub(crate) fn i32_to_f32(x: i32) -> f32 {
    (x as f64 / (i32::MAX as f64 + 1.0)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undithered_conversion_rounds_and_clamps_as_before() {
        let mut q = Quantizer::new(AudioDither::Off);
        assert_eq!(q.f32_to_i16(1.0), i16::MAX);
        assert_eq!(q.f32_to_i16(-1.0), -i16::MAX);
        assert_eq!(q.f32_to_i16(2.0), i16::MAX);
        assert_eq!(q.f32_to_i16(0.5), 16_384); // 16383.5 rounds away from zero
        assert_eq!(q.f32_to_i16(f32::NAN), 0);
        assert_eq!(q.f32_to_i24(1.0), 8_388_607);
        assert_eq!(q.i32_to_i16(i32::MAX), i16::MAX);
        assert_eq!(q.i32_to_i24(-256), -1);
    }

    #[test]
    fn tpdf_silence_stays_within_one_lsb_and_is_unbiased() {
        let mut q = Quantizer::new(AudioDither::Tpdf);
        let (mut sum, mut moved) = (0i64, 0);
        for _ in 0..100_000 {
            let s = q.f32_to_i16(0.0);
            assert!((-1..=1).contains(&s), "silence quantized to {s}");
            sum += i64::from(s);
            moved += usize::from(s != 0);
        }
        assert!(sum.abs() < 500, "dither carries a DC bias: {sum}");
        assert!(moved > 10_000, "dither should move silence: {moved}");
    }

    /// The point of dither: a signal below half an LSB, which plain rounding
    /// erases, survives on average instead of truncating to zero.
    #[test]
    fn tpdf_preserves_sub_lsb_signal_on_average() {
        let value = 0.3 / 32_767.0;
        let mut off = Quantizer::new(AudioDither::Off);
        assert_eq!(off.f32_to_i16(value), 0);
        let mut q = Quantizer::new(AudioDither::Tpdf);
        let n = 200_000;
        let mean = (0..n).map(|_| f64::from(q.f32_to_i16(value))).sum::<f64>() / n as f64;
        assert!(
            (mean - 0.3).abs() < 0.02,
            "mean {mean} should track 0.3 LSB"
        );

        let value24 = 0.3 / 8_388_607.0;
        let mean24 = (0..n)
            .map(|_| f64::from(q.f32_to_i24(value24)))
            .sum::<f64>()
            / n as f64;
        assert!((mean24 - 0.3).abs() < 0.02, "24-bit mean {mean24}");
        let mean_i32 = (0..n).map(|_| f64::from(q.i32_to_i16(19_661))).sum::<f64>() / n as f64;
        assert!((mean_i32 - 0.3).abs() < 0.02, "i32 input mean {mean_i32}");
    }

    #[test]
    fn tpdf_clips_full_scale_without_wrapping() {
        let mut q = Quantizer::new(AudioDither::Tpdf);
        for _ in 0..10_000 {
            assert!(q.f32_to_i16(1.0) >= i16::MAX - 1);
            assert!(q.f32_to_i16(-1.0) <= -i16::MAX + 1);
            assert!((-8_388_608..=8_388_607).contains(&q.f32_to_i24(1.5)));
            assert!((-8_388_608..=8_388_607).contains(&q.f32_to_i24(-1.5)));
            assert_eq!(q.f32_to_i16(f32::NAN), 0);
            assert!(q.i32_to_i16(i32::MIN) <= i16::MIN + 1);
        }
    }

    #[test]
    fn tpdf_is_seeded_so_encodes_are_reproducible() {
        let mut a = Quantizer::new(AudioDither::Tpdf);
        let mut b = Quantizer::new(AudioDither::Tpdf);
        for n in 0..10_000 {
            let x = (n as f32 * 0.001).sin() * 1e-3;
            assert_eq!(a.f32_to_i16(x), b.f32_to_i16(x));
        }
    }

    /// #362: each file's dither is its own. Stems of one export get distinct
    /// seeds and uncorrelated noise (silence through two of them is not the
    /// same file), while one path always gets the same seed.
    #[test]
    fn every_file_gets_its_own_dither() {
        let dir = std::path::Path::new("C:/Exports/Song");
        let paths: Vec<std::path::PathBuf> = ["Song.wav", "Song - Bass.wav", "Song - Drums.wav"]
            .iter()
            .map(|name| dir.join(format!("{name}.partial")))
            .collect();
        let seeds: Vec<u32> = paths.iter().map(|p| dither_seed_for(p)).collect();
        assert_eq!(seeds[0], dither_seed_for(&paths[0]), "stable per path");
        assert!(seeds.iter().all(|&s| s != 0));
        assert!(seeds[0] != seeds[1] && seeds[1] != seeds[2] && seeds[0] != seeds[2]);

        let noise = |path: &std::path::Path| -> Vec<i16> {
            let mut q = Quantizer::for_file(path).with_dither(AudioDither::Tpdf);
            (0..48_000).map(|_| q.f32_to_i16(0.0)).collect()
        };
        let (a, b) = (noise(&paths[1]), noise(&paths[2]));
        assert_ne!(a, b, "silent stems must not be identical noise");
        assert_eq!(a, noise(&paths[1]), "the same file encodes the same");
        // Coherent noise would correlate fully; independent noise near zero.
        let dot: i64 = a
            .iter()
            .zip(&b)
            .map(|(&x, &y)| i64::from(x) * i64::from(y))
            .sum();
        let power: i64 = a.iter().map(|&x| i64::from(x) * i64::from(x)).sum();
        assert!(
            (dot as f64).abs() < 0.05 * power as f64,
            "stem noise correlates: {dot} of {power}"
        );
    }

    /// Dither is opt-in: the shared default (LiveStage's recorder, bounces,
    /// the stem extractor) does not dither; Studio's export selects TPDF.
    #[test]
    fn the_shared_default_does_not_dither() {
        assert_eq!(AudioEncodeOptions::default().dither, AudioDither::Off);
    }

    /// #284: MP3 export is gone. Whatever stored the MP3 format (by serde or
    /// by id) loads as Opus instead of failing, and the format ids round-trip.
    #[test]
    fn a_stored_mp3_format_loads_as_opus() {
        for legacy in ["\"mp3\"", "\"Mp3\"", "\"MP3\""] {
            let format: AudioFileFormat = serde_json::from_str(legacy).unwrap();
            assert_eq!(format, AudioFileFormat::Opus, "{legacy}");
        }
        assert_eq!(AudioFileFormat::from_id("mp3"), Some(AudioFileFormat::Opus));
        assert_eq!(
            AudioFileFormat::from_id(" MP3 "),
            Some(AudioFileFormat::Opus)
        );
        assert_eq!(AudioFileFormat::from_id("ogg"), None);

        for format in [
            AudioFileFormat::Wav,
            AudioFileFormat::Flac,
            AudioFileFormat::Opus,
            AudioFileFormat::Rauf,
        ] {
            let json = serde_json::to_string(&format).unwrap();
            assert_eq!(json, format!("\"{}\"", format.as_str()));
            assert_eq!(
                serde_json::from_str::<AudioFileFormat>(&json).unwrap(),
                format
            );
            assert_eq!(AudioFileFormat::from_id(format.as_str()), Some(format));
        }
        assert_eq!(AudioFileFormat::Opus.extension(), "opus");
    }

    #[test]
    fn a_legacy_mp3_bitrate_maps_to_a_comparable_opus_preset() {
        assert_eq!(OpusBitrate::from_legacy_mp3_kbps(128), OpusBitrate::Kbps96);
        assert_eq!(OpusBitrate::from_legacy_mp3_kbps(192), OpusBitrate::Kbps128);
        // The old MP3 default lands on the Opus default.
        assert_eq!(
            OpusBitrate::from_legacy_mp3_kbps(256),
            OpusBitrate::default()
        );
        assert_eq!(OpusBitrate::from_legacy_mp3_kbps(320), OpusBitrate::Kbps192);
        assert_eq!(OpusBitrate::from_legacy_mp3_kbps(0), OpusBitrate::Kbps64);
        assert_eq!(
            OpusBitrate::from_legacy_mp3_kbps(9_999),
            OpusBitrate::Kbps192
        );
    }

    #[test]
    fn opus_bitrate_presets_validate() {
        assert_eq!(OpusBitrate::default().kbps(), 160);
        for preset in OpusBitrate::ALL {
            assert_eq!(OpusBitrate::from_kbps(preset.kbps()).unwrap(), preset);
        }
        assert!(OpusBitrate::from_kbps(320).is_err());
        assert!(OpusBitrate::from_kbps(123).is_err());
    }
}
