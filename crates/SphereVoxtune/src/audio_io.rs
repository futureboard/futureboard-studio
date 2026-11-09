//! Real audio file decoding & resampling for CANTO.
//! Borrowed from `Futureboard-release/crates/SphereDirectAudioEngine/src/audio_file.rs`.
//!
//! Supports WAV/WAVE (8/16/24/32-bit PCM & 32-bit IEEE float, including WAVEFORMATEXTENSIBLE)
//! via fast inline RIFF parsing, and MP3, FLAC, OGG/Vorbis, M4A/AAC, AIFF, ALAC via `symphonia`.

use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Rates below this are treated as a broken header. Resampling 1 Hz audio up
/// to a project rate allocates tens of thousands of samples per input sample.
pub const MIN_SAMPLE_RATE: u32 = 1_000;
/// Upper bound for a file or a project rate we will resample.
pub const MAX_SAMPLE_RATE: u32 = 384_000;
/// 8 kHz up to 384 kHz. Wider ratios are refused.
pub const MAX_UPSAMPLE: u32 = 48;
/// The longest take, at any sample rate. A take needs memory in proportion
/// to its rate; the large buffers on the way are reserved fallibly, so a
/// machine without the memory gets an error instead of an abort.
pub const MAX_SECONDS: u32 = 60 * 60;

/// Frames in [`MAX_SECONDS`] at `rate`.
pub fn max_frames(rate: u32) -> usize {
    MAX_SECONDS as usize * rate as usize
}

/// Why audio cannot be loaded or converted. `Display` is the message a user
/// sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadLimit {
    /// The audio's own sample rate is outside the supported range.
    UnsupportedRate(u32),
    /// The rate it would be converted to (the project's) is outside it.
    UnsupportedProjectRate(u32),
    /// The two rates are too far apart to convert between.
    RatesTooFarApart { from: u32, to: u32 },
    /// Longer than [`MAX_SECONDS`].
    TooLong,
}

impl std::fmt::Display for LoadLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let range = format!(
            "{} kHz to {} kHz",
            MIN_SAMPLE_RATE / 1_000,
            MAX_SAMPLE_RATE / 1_000
        );
        match self {
            LoadLimit::UnsupportedRate(rate) => write!(
                f,
                "Sample rate {rate} Hz is outside the supported range ({range})"
            ),
            LoadLimit::UnsupportedProjectRate(rate) => write!(
                f,
                "Project sample rate {rate} Hz is not supported ({range})"
            ),
            LoadLimit::RatesTooFarApart { from, to } => {
                write!(f, "Audio at {from} Hz cannot be converted to {to} Hz")
            }
            LoadLimit::TooLong => write!(
                f,
                "Audio is too long to load (the limit is {} minutes)",
                MAX_SECONDS / 60
            ),
        }
    }
}

/// Output length of converting `input_len` frames from `src_rate` to
/// `dst_rate`, or why that is refused: a rate outside [`MIN_SAMPLE_RATE`] ..
/// [`MAX_SAMPLE_RATE`], a ratio beyond [`MAX_UPSAMPLE`], or audio longer
/// than [`MAX_SECONDS`] (the same duration at every rate).
pub fn check_load(input_len: usize, src_rate: u32, dst_rate: u32) -> Result<usize, LoadLimit> {
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&src_rate) {
        return Err(LoadLimit::UnsupportedRate(src_rate));
    }
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&dst_rate) {
        return Err(LoadLimit::UnsupportedProjectRate(dst_rate));
    }
    if input_len > max_frames(src_rate) {
        return Err(LoadLimit::TooLong);
    }
    if input_len == 0 || src_rate == dst_rate {
        return Ok(input_len);
    }
    if dst_rate > src_rate && dst_rate.div_ceil(src_rate) > MAX_UPSAMPLE {
        return Err(LoadLimit::RatesTooFarApart {
            from: src_rate,
            to: dst_rate,
        });
    }
    let out = (input_len as u128 * dst_rate as u128).div_ceil(src_rate as u128);
    if out > max_frames(dst_rate) as u128 {
        return Err(LoadLimit::TooLong);
    }
    Ok(out as usize)
}

/// [`check_load`] without the reason.
pub fn bounded_output_len(input_len: usize, src_rate: u32, dst_rate: u32) -> Option<usize> {
    if input_len == 0 || src_rate == 0 || dst_rate == 0 {
        return Some(0);
    }
    check_load(input_len, src_rate, dst_rate).ok()
}

#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// Mono samples normalized to `-1.0..=1.0`, resampled to `sample_rate`.
    pub samples: Vec<f32>,
    /// Sample rate of `samples` (matches the engine sample rate).
    pub sample_rate: u32,
    /// Original file sample rate before resampling.
    pub source_sample_rate: u32,
    /// Original channel count of the file.
    pub source_channels: usize,
    /// Total duration in seconds.
    pub duration_sec: f64,
    /// Downsampled peak envelope (`0.0..=1.0`) for UI waveform overview.
    pub waveform_peaks: Vec<f32>,
}

/// Decodes an audio file from disk into mono `f32` samples at `target_sample_rate`.
pub fn decode_audio_file_to_mono(
    path: &Path,
    target_sample_rate: u32,
) -> Result<DecodedAudio, String> {
    finalize_decoded(read_audio_file(path)?, target_sample_rate)
}

/// Stereo audio decoded at the engine rate: a backing track, which keeps its
/// stereo image (vocals are analysed mono).
#[derive(Debug, Clone)]
pub struct DecodedStereo {
    /// Interleaved L/R frames at `sample_rate`.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub duration_sec: f64,
    /// Downsampled peak envelope (`0.0..=1.0`) of both channels.
    pub waveform_peaks: Vec<f32>,
}

/// Decodes an audio file from disk into stereo at `target_sample_rate`. Mono
/// files play on both sides; files with more channels keep their first two.
pub fn decode_audio_file_to_stereo(
    path: &Path,
    target_sample_rate: u32,
) -> Result<DecodedStereo, String> {
    let raw = read_audio_file(path)?;
    if raw.frames == 0 || raw.channels == 0 || raw.sample_rate == 0 {
        return Err("Decoded audio buffer is empty".to_string());
    }
    let side = |channel: usize| -> Vec<f32> {
        let channel = channel.min(raw.channels - 1);
        raw.samples
            .chunks_exact(raw.channels)
            .map(|frame| frame[channel])
            .collect()
    };
    let target_sr = target_sample_rate.max(8_000);
    check_load(raw.frames, raw.sample_rate, target_sr).map_err(|limit| limit.to_string())?;
    let (left, right) = (
        resample_mono_cubic(&side(0), raw.sample_rate, target_sr),
        resample_mono_cubic(&side(1), raw.sample_rate, target_sr),
    );
    if left.is_empty() || right.is_empty() {
        return Err(OUT_OF_MEMORY.to_string());
    }
    let mut samples = Vec::new();
    let mut louder = Vec::new();
    if samples.try_reserve_exact(left.len() * 2).is_err()
        || louder.try_reserve_exact(left.len()).is_err()
    {
        return Err(OUT_OF_MEMORY.to_string());
    }
    for (&l, &r) in left.iter().zip(&right) {
        samples.extend_from_slice(&[l, r]);
        louder.push(if l.abs() >= r.abs() { l } else { r });
    }
    Ok(DecodedStereo {
        duration_sec: left.len() as f64 / target_sr as f64,
        waveform_peaks: compute_waveform_peaks(&louder, 240),
        samples,
        sample_rate: target_sr,
    })
}

fn read_audio_file(path: &Path) -> Result<RawInterleavedBuffer, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if matches!(ext.as_str(), "wav" | "wave") {
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|e| format!("Failed to open '{}': {e}", path.display()))?
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
        match decode_wav_bytes(&bytes) {
            Ok(buf) => Ok(buf),
            Err(e) if is_refusal(&e) => Err(e),
            Err(_) => decode_via_symphonia_reader(Box::new(Cursor::new(bytes)), Some(ext.as_str())),
        }
    } else {
        let file =
            File::open(path).map_err(|e| format!("Failed to open '{}': {e}", path.display()))?;
        decode_via_symphonia_reader(Box::new(file), Some(ext.as_str()))
    }
}

/// Decodes audio from an in-memory byte slice into mono `f32` samples at `target_sample_rate`.
pub fn decode_audio_bytes_to_mono(
    bytes: &[u8],
    ext_hint: Option<&str>,
    target_sample_rate: u32,
) -> Result<DecodedAudio, String> {
    if bytes.is_empty() {
        return Err("Empty audio byte buffer".to_string());
    }
    let is_riff_wav = bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE";
    let raw = if is_riff_wav {
        match decode_wav_bytes(bytes) {
            Ok(buf) => buf,
            Err(e) if is_refusal(&e) => {
                return Err(e);
            }
            Err(_) => decode_via_symphonia_reader(
                Box::new(Cursor::new(bytes.to_vec())),
                ext_hint.or(Some("wav")),
            )?,
        }
    } else {
        decode_via_symphonia_reader(Box::new(Cursor::new(bytes.to_vec())), ext_hint)?
    };

    finalize_decoded(raw, target_sample_rate)
}

struct RawInterleavedBuffer {
    sample_rate: u32,
    channels: usize,
    frames: usize,
    samples: Vec<f32>,
}

fn finalize_decoded(
    raw: RawInterleavedBuffer,
    target_sample_rate: u32,
) -> Result<DecodedAudio, String> {
    if raw.frames == 0 || raw.channels == 0 || raw.sample_rate == 0 {
        return Err("Decoded audio buffer is empty".to_string());
    }

    let target_sr = target_sample_rate.max(8_000);
    check_load(raw.frames, raw.sample_rate, target_sr).map_err(|limit| limit.to_string())?;
    let mono_src = downmix_to_mono(&raw.samples, raw.channels, raw.frames)
        .ok_or_else(|| OUT_OF_MEMORY.to_string())?;
    let mono = if raw.sample_rate == target_sr {
        mono_src
    } else {
        resample_mono_cubic(&mono_src, raw.sample_rate, target_sr)
    };
    if mono.is_empty() {
        return Err(OUT_OF_MEMORY.to_string());
    }

    let duration_sec = mono.len() as f64 / target_sr as f64;
    let waveform_peaks = compute_waveform_peaks(&mono, 240);

    Ok(DecodedAudio {
        samples: mono,
        sample_rate: target_sr,
        source_sample_rate: raw.sample_rate,
        source_channels: raw.channels,
        duration_sec,
        waveform_peaks,
    })
}

/// What a large buffer that could not be reserved is reported as.
const OUT_OF_MEMORY: &str = "Not enough memory to load this audio";

/// A WAV the inline parser read but refused (rate, length, memory): not a
/// format problem the general decoder could get past.
fn is_refusal(error: &str) -> bool {
    error.contains("too long")
        || error.contains("outside the supported range")
        || error == OUT_OF_MEMORY
}

/// `None` when the buffer cannot be reserved.
fn downmix_to_mono(interleaved: &[f32], channels: usize, frames: usize) -> Option<Vec<f32>> {
    let mut mono = Vec::new();
    mono.try_reserve_exact(frames).ok()?;
    if channels <= 1 {
        mono.extend_from_slice(interleaved);
        return Some(mono);
    }
    let inv = 1.0 / channels as f32;
    for chunk in interleaved.chunks_exact(channels) {
        let sum: f32 = chunk.iter().copied().sum();
        mono.push((sum * inv).clamp(-1.0, 1.0));
    }
    Some(mono)
}

/// Resamples a mono buffer from `src_rate` to `dst_rate` using 4-point Catmull-Rom cubic interpolation.
pub fn resample_mono_cubic(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if input.is_empty() || src_rate == 0 || dst_rate == 0 {
        return Vec::new();
    }
    if src_rate == dst_rate {
        return input.to_vec();
    }
    let Some(out_len) = bounded_output_len(input.len(), src_rate, dst_rate) else {
        return Vec::new();
    };
    let ratio = src_rate as f64 / dst_rate as f64;
    let mut out = Vec::new();
    if out.try_reserve_exact(out_len).is_err() {
        return Vec::new();
    }
    let max_idx = input.len().saturating_sub(1);

    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;

        let p0 = input[idx.saturating_sub(1).min(max_idx)];
        let p1 = input[idx.min(max_idx)];
        let p2 = input[(idx + 1).min(max_idx)];
        let p3 = input[(idx + 2).min(max_idx)];

        let a0 = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
        let a1 = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
        let a2 = -0.5 * p0 + 0.5 * p2;
        let a3 = p1;

        let sample = ((a0 * frac + a1) * frac + a2) * frac + a3;
        out.push(sample.clamp(-1.0, 1.0));
    }
    out
}

pub fn compute_waveform_peaks(mono: &[f32], num_bins: usize) -> Vec<f32> {
    if mono.is_empty() || num_bins == 0 {
        return Vec::new();
    }
    let bins = num_bins.min(mono.len());
    let mut peaks = Vec::with_capacity(bins);
    for b in 0..bins {
        let start = (b * mono.len()) / bins;
        let end = (((b + 1) * mono.len()) / bins)
            .max(start + 1)
            .min(mono.len());
        let mut max_abs = 0.0f32;
        for &s in &mono[start..end] {
            let a = s.abs();
            if a > max_abs {
                max_abs = a;
            }
        }
        peaks.push(max_abs.clamp(0.0, 1.0));
    }
    peaks
}

fn decode_via_symphonia_reader(
    source: Box<dyn MediaSource>,
    ext_hint: Option<&str>,
) -> Result<RawInterleavedBuffer, String> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext_hint {
        if !ext.is_empty() {
            hint.with_extension(ext);
        }
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Audio format probe failed: {e}"))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some())
        .ok_or_else(|| "No decodable audio track found in file".to_string())?
        .clone();

    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| "Audio track has no sample rate".to_string())?;
    let mut channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(1);
    // Before decoding anything: a rate we cannot use is refused as such.
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
        return Err(LoadLimit::UnsupportedRate(sample_rate).to_string());
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Failed to create audio codec decoder: {e}"))?;

    let mut all_samples: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(_) => break,
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf_ref) => {
                let spec = *audio_buf_ref.spec();
                channels = spec.channels.count().max(1);
                if sample_buf
                    .as_ref()
                    .is_none_or(|b| b.capacity() < audio_buf_ref.capacity())
                {
                    sample_buf = Some(SampleBuffer::<f32>::new(
                        audio_buf_ref.capacity() as u64,
                        spec,
                    ));
                }
                if let Some(buf) = &mut sample_buf {
                    buf.copy_interleaved_ref(audio_buf_ref);
                    let incoming = buf.samples();
                    let cap = max_frames(sample_rate).saturating_mul(channels.max(1));
                    if all_samples.len().saturating_add(incoming.len()) > cap {
                        return Err(LoadLimit::TooLong.to_string());
                    }
                    if all_samples.try_reserve(incoming.len()).is_err() {
                        return Err(OUT_OF_MEMORY.to_string());
                    }
                    all_samples.extend_from_slice(incoming);
                }
            }
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    let frames = all_samples.len().checked_div(channels).unwrap_or(0);
    Ok(RawInterleavedBuffer {
        sample_rate,
        channels,
        frames,
        samples: all_samples,
    })
}

#[derive(Debug, Clone, Copy)]
struct WavFmt {
    audio_format: u16,
    channels: usize,
    sample_rate: u32,
    bits_per_sample: u16,
}

fn decode_wav_bytes(bytes: &[u8]) -> Result<RawInterleavedBuffer, String> {
    if bytes.len() < 44 {
        return Err("File too small for WAV".to_string());
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("Not a RIFF/WAVE file".to_string());
    }

    let mut cursor = 12usize;
    let mut fmt: Option<WavFmt> = None;
    let mut data_range: Option<(usize, usize)> = None;

    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let len = read_u32_le(bytes, cursor + 4)? as usize;
        let body = cursor + 8;
        let end = body.saturating_add(len).min(bytes.len());

        match id {
            b"fmt " => {
                if len < 16 || body + 16 > bytes.len() {
                    return Err("Invalid WAV fmt chunk".to_string());
                }
                let mut audio_format = read_u16_le(bytes, body)?;
                let channels = read_u16_le(bytes, body + 2)? as usize;
                let sample_rate = read_u32_le(bytes, body + 4)?;
                let bits_per_sample = read_u16_le(bytes, body + 14)?;
                // WAVE_FORMAT_EXTENSIBLE (0xFFFE): subformat code is at offset 24..26
                if audio_format == 0xFFFE && len >= 40 && body + 26 <= bytes.len() {
                    audio_format = read_u16_le(bytes, body + 24)?;
                }
                fmt = Some(WavFmt {
                    audio_format,
                    channels,
                    sample_rate,
                    bits_per_sample,
                });
            }
            b"data" => {
                data_range = Some((body, end - body));
                break;
            }
            _ => {}
        }

        cursor = end + (len & 1);
    }

    let fmt = fmt.ok_or_else(|| "Missing WAV fmt chunk".to_string())?;
    let (data_start, data_len) = data_range.ok_or_else(|| "Missing WAV data chunk".to_string())?;
    if fmt.channels == 0 || fmt.sample_rate == 0 {
        return Err("Invalid WAV channel count or sample rate".to_string());
    }

    let bytes_per_sample = match fmt.bits_per_sample {
        8 => 1usize,
        16 => 2,
        24 => 3,
        32 => 4,
        bits => return Err(format!("Unsupported WAV bit depth: {bits}")),
    };
    let bytes_per_frame = fmt.channels * bytes_per_sample;
    if bytes_per_frame == 0 || data_len < bytes_per_frame {
        return Err("Empty WAV data chunk".to_string());
    }

    let frames = data_len / bytes_per_frame;
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&fmt.sample_rate) {
        return Err(LoadLimit::UnsupportedRate(fmt.sample_rate).to_string());
    }
    if frames > max_frames(fmt.sample_rate) || fmt.channels > 16 {
        return Err(LoadLimit::TooLong.to_string());
    }
    let sample_count = frames * fmt.channels;
    let mut samples = Vec::new();
    if samples.try_reserve_exact(sample_count).is_err() {
        return Err(OUT_OF_MEMORY.to_string());
    }

    let mut offset = data_start;
    for _ in 0..sample_count {
        let s = match (fmt.audio_format, fmt.bits_per_sample) {
            (1, 8) => (bytes[offset] as f32 - 128.0) / 128.0,
            (1, 16) => read_i16_le(bytes, offset)? as f32 / 32_768.0,
            (1, 24) => read_i24_le(bytes, offset)? as f32 / 8_388_608.0,
            (1, 32) => read_i32_le(bytes, offset)? as f32 / 2_147_483_648.0,
            (3, 32) => {
                let b = &bytes[offset..offset + 4];
                let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if v.is_finite() { v } else { 0.0 }
            }
            (code, _) => return Err(format!("Unsupported WAV format code: {code}")),
        };
        samples.push(s.clamp(-1.0, 1.0));
        offset += bytes_per_sample;
    }

    Ok(RawInterleavedBuffer {
        sample_rate: fmt.sample_rate,
        channels: fmt.channels,
        frames,
        samples,
    })
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let b = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "Unexpected EOF reading u16".to_string())?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let b = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "Unexpected EOF reading u32".to_string())?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_i16_le(bytes: &[u8], offset: usize) -> Result<i16, String> {
    let b = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "Unexpected EOF reading i16".to_string())?;
    Ok(i16::from_le_bytes([b[0], b[1]]))
}

fn read_i24_le(bytes: &[u8], offset: usize) -> Result<i32, String> {
    let b = bytes
        .get(offset..offset + 3)
        .ok_or_else(|| "Unexpected EOF reading i24".to_string())?;
    let raw = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
    Ok(if (raw & 0x80_0000) != 0 {
        raw | !0xFF_FFFF
    } else {
        raw
    })
}

fn read_i32_le(bytes: &[u8], offset: usize) -> Result<i32, String> {
    let b = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "Unexpected EOF reading i32".to_string())?;
    Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Encodes mono `f32` samples into a standard 16-bit PCM RIFF/WAVE byte buffer.
pub fn encode_mono_wav_i16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_bytes = samples.len() * 2;
    let riff_size = 36 + data_bytes as u32;
    let byte_rate = sample_rate * 2;
    let mut out = Vec::with_capacity(44 + data_bytes);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 channel
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());

    for &s in samples {
        let pcm = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        out.extend_from_slice(&pcm.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_hertz_wav_is_refused() {
        let mut bytes = encode_mono_wav_i16(&[0.2, -0.2, 0.2, -0.2], 8_000);
        bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
        let err = decode_audio_bytes_to_mono(&bytes, Some("wav"), 48_000).unwrap_err();
        assert!(
            err.contains("outside the supported range") || err.contains("too long"),
            "{err}"
        );
    }

    #[test]
    fn an_eight_kilohertz_wav_still_decodes() {
        let bytes = encode_mono_wav_i16(&[0.25, -0.25, 0.25, -0.25], 8_000);
        let audio = decode_audio_bytes_to_mono(&bytes, Some("wav"), 48_000).unwrap();
        assert_eq!(audio.sample_rate, 48_000);
        assert!(audio.samples.len() > 4);
        assert!(audio.samples.len() < 64);
    }

    #[test]
    fn resampling_refuses_a_runaway_ratio() {
        let input = vec![0.1f32; 1_000];
        assert!(resample_mono_cubic(&input, 1, 48_000).is_empty());
        assert!(bounded_output_len(input.len(), 1, 48_000).is_none());
        assert!(bounded_output_len(max_frames(48_000) + 1, 48_000, 96_000).is_none());
    }

    /// #367: an hour at any rate; a 20-minute 192 kHz take is not "too long".
    #[test]
    fn the_length_limit_is_the_same_duration_at_every_rate() {
        for rate in [44_100u32, 48_000, 96_000, 192_000, 384_000] {
            let twenty_minutes = 20 * 60 * rate as usize;
            assert_eq!(check_load(twenty_minutes, rate, rate), Ok(twenty_minutes));
            assert_eq!(
                check_load(max_frames(rate), rate, rate),
                Ok(max_frames(rate))
            );
            assert_eq!(
                check_load(max_frames(rate) + 1, rate, rate),
                Err(LoadLimit::TooLong)
            );
        }
        // An hour at 192 kHz for a 48 kHz project is an hour at 48 kHz.
        assert_eq!(
            check_load(max_frames(192_000), 192_000, 48_000),
            Ok(max_frames(48_000))
        );
    }

    /// #367: a rate we cannot use is named, not reported as too long.
    #[test]
    fn unsupported_rates_are_named_not_called_too_long() {
        let file = check_load(1_000, 768_000, 48_000).unwrap_err();
        assert_eq!(file, LoadLimit::UnsupportedRate(768_000));
        assert!(file.to_string().contains("768000 Hz"));
        assert!(!file.to_string().contains("too long"));
        let project = check_load(1_000, 48_000, 768_000).unwrap_err();
        assert_eq!(project, LoadLimit::UnsupportedProjectRate(768_000));
        assert!(!project.to_string().contains("too long"));
    }

    #[test]
    fn a_short_768_khz_wav_is_refused_for_its_rate() {
        let mut bytes = encode_mono_wav_i16(&[0.2; 64], 48_000);
        bytes[24..28].copy_from_slice(&768_000u32.to_le_bytes());
        let err = decode_audio_bytes_to_mono(&bytes, Some("wav"), 48_000).unwrap_err();
        assert!(
            err.contains("768000 Hz") && err.contains("outside the supported range"),
            "{err}"
        );
    }

    #[test]
    fn a_project_rate_above_the_range_is_named() {
        let bytes = encode_mono_wav_i16(&[0.2; 64], 48_000);
        let err = decode_audio_bytes_to_mono(&bytes, Some("wav"), 768_000).unwrap_err();
        assert!(err.contains("Project sample rate 768000 Hz"), "{err}");
    }
}
