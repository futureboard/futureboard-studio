//! Audio file decoder for the native playback engine.
//!
//! **WAV/WAVE** — decoded by an inline RIFF/WAVE parser (fast, zero extra deps).
//! **Everything else** — decoded via `symphonia` (MP3, FLAC, OGG Vorbis, M4A, AIFF).
//!
//! The result is always interleaved `f32` samples normalised to `−1.0 … 1.0`.
//! Decoding happens on the control thread; the audio callback only reads the
//! finished `AudioFileBuffer` through an `Arc` — no allocation at runtime.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::SphereAudioError;
use sphere_encoder::rauf::{RaufReader, RaufSampleFormat};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

// ── Public API ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AudioFileBuffer {
    pub sample_rate: u32,
    pub channels: usize,
    pub frames: usize,
    /// Interleaved PCM samples, normalised to `−1.0 … 1.0`.
    pub samples: Vec<f32>,
}

/// Declick ramp lengths for a browser audition, in seconds.
///
/// Every Browser selection starts a voice from an arbitrary sample value and
/// every new selection cuts the previous one mid-waveform. Without these ramps
/// each step through a folder produces a click, which is what the preview
/// mostly sounds like when walking a directory with the arrow keys.
const AUDITION_ATTACK_SECONDS: f32 = 0.002;
const AUDITION_RELEASE_SECONDS: f32 = 0.008;

/// How much of a file a Browser preview plays before it fades out on its own.
///
/// Browsing a folder is a scan, not a listen: a five second head is enough to
/// identify a sample and keeps a long stem from occupying the preview voice
/// (and the user's ears) until the next selection. Files shorter than this play
/// to their real end — the limit only ever truncates.
pub const AUDITION_PREVIEW_SECONDS: f64 = 5.0;

/// A pre-decoded, one-shot browser audition voice. It owns immutable PCM data
/// prepared off the audio thread; rendering only advances a cursor and mixes
/// samples, so no callback allocation, locks, or I/O are required.
#[derive(Debug)]
pub struct AudioFileAudition {
    source: Box<AudioFileBuffer>,
    source_frame: f64,
    /// Source frames consumed per output frame (source rate / device rate).
    step: f64,
    /// Declick envelope: current gain plus its per-output-frame increment.
    gain: f32,
    gain_step: f32,
    /// Source frame at which the end-of-file fade starts, and the reciprocal of
    /// that fade's length — precomputed so the mix loop stays division-free.
    tail_start: f64,
    inv_tail_len: f64,
    /// Last source frame this voice will play (exclusive) — the file end, or the
    /// [`AUDITION_PREVIEW_SECONDS`] head of it, whichever comes first.
    end_frame: f64,
    /// Source seconds per source frame — publishing the playhead must not divide
    /// on the audio thread.
    seconds_per_frame: f64,
}

impl AudioFileAudition {
    pub fn new(source: Box<AudioFileBuffer>, output_rate: u32) -> Self {
        let rate = output_rate.max(1);
        let source_rate = source.sample_rate.max(1);
        let step = source_rate as f64 / rate as f64;
        // Preview head: whole file when it is shorter than the limit.
        let end_frame = (AUDITION_PREVIEW_SECONDS * source_rate as f64).min(source.frames as f64);
        // The tail fade is measured in source frames, so it stays the same
        // audible length whatever the resample ratio is. It hangs off the
        // preview end, so a truncated file fades out instead of being cut.
        let tail_len = (AUDITION_RELEASE_SECONDS as f64 * source_rate as f64)
            .min(end_frame)
            .max(1.0);
        Self {
            source_frame: 0.0,
            step,
            gain: 0.0,
            gain_step: 1.0 / (AUDITION_ATTACK_SECONDS * rate as f32).max(1.0),
            tail_start: end_frame - tail_len,
            inv_tail_len: 1.0 / tail_len,
            end_frame,
            seconds_per_frame: 1.0 / source_rate as f64,
            source,
        }
    }

    /// Playhead of this voice in seconds of the source file. Read by the
    /// callback right after mixing so the Browser preview pane can draw it.
    #[inline]
    pub fn position_seconds(&self) -> f64 {
        self.source_frame * self.seconds_per_frame
    }

    /// Start fading this voice out; it retires once the ramp reaches silence.
    pub fn begin_release(&mut self, output_rate: u32) {
        let frames = (AUDITION_RELEASE_SECONDS * output_rate.max(1) as f32).max(1.0);
        self.gain_step = -1.0 / frames;
    }

    pub fn into_source(self) -> Box<AudioFileBuffer> {
        self.source
    }

    /// Mix this source into an interleaved output block. Returns `true` once
    /// the voice is finished (EOF, or a release ramp that reached silence).
    /// Mono sources are duplicated; source channels beyond stereo are downmixed
    /// by taking their first stereo pair.
    #[inline]
    pub fn mix_into(&mut self, output: &mut [f32], output_channels: usize) -> bool {
        if output_channels == 0 || self.source.channels == 0 || self.source.frames == 0 {
            return true;
        }
        for frame in output.chunks_mut(output_channels) {
            let source_index = self.source_frame as usize;
            if self.source_frame >= self.end_frame || source_index >= self.source.frames {
                return true;
            }
            self.gain = (self.gain + self.gain_step).clamp(0.0, 1.0);
            if self.gain <= 0.0 && self.gain_step < 0.0 {
                return true; // release ramp finished
            }
            let next_index = (source_index + 1).min(self.source.frames - 1);
            let fraction = (self.source_frame - source_index as f64) as f32;
            let sample_at = |frame_index: usize, channel: usize| {
                self.source.samples[frame_index * self.source.channels + channel]
            };
            let tail = if self.source_frame > self.tail_start {
                (((self.end_frame - self.source_frame) * self.inv_tail_len) as f32).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let env = self.gain * tail;
            let left = sample_at(source_index, 0)
                + (sample_at(next_index, 0) - sample_at(source_index, 0)) * fraction;
            let right_channel = if self.source.channels > 1 { 1 } else { 0 };
            let right = sample_at(source_index, right_channel)
                + (sample_at(next_index, right_channel) - sample_at(source_index, right_channel))
                    * fraction;
            frame[0] += left * env;
            if output_channels > 1 {
                frame[1] += right * env;
            }
            self.source_frame += self.step;
        }
        self.source_frame >= self.end_frame
    }
}

/// The realtime side of the Browser sample preview: the voice currently
/// auditioning plus at most one voice fading out because it was replaced or
/// stopped. Owned by the render callback, so both slots are plain `Option`s and
/// finished sources leave through the graveyard rather than being freed here.
#[derive(Debug, Default)]
pub struct AuditionPlayer {
    current: Option<AudioFileAudition>,
    releasing: Option<AudioFileAudition>,
}

impl AuditionPlayer {
    /// Audition `source`, fading out whatever was playing instead of cutting it.
    pub fn start(&mut self, source: Box<AudioFileBuffer>, output_rate: u32) {
        let voice = AudioFileAudition::new(source, output_rate);
        if let Some(previous) = self.current.replace(voice) {
            self.release(previous, output_rate);
        }
    }

    /// Stop the current audition through the same fade-out.
    pub fn stop(&mut self, output_rate: u32) {
        if let Some(previous) = self.current.take() {
            self.release(previous, output_rate);
        }
    }

    /// `true` when nothing is playing or fading — the render callback uses this
    /// to decide whether the graph still has to be woken while stopped.
    pub fn is_idle(&self) -> bool {
        self.current.is_none() && self.releasing.is_none()
    }

    /// Playhead of the voice the user is actually auditioning, in seconds of the
    /// source file. `None` while nothing is auditioning — a voice that is only
    /// fading out has already been replaced or stopped, so it no longer owns the
    /// Browser's playhead.
    #[inline]
    pub fn position_seconds(&self) -> Option<f64> {
        self.current
            .as_ref()
            .map(AudioFileAudition::position_seconds)
    }

    /// Mix both voices into an interleaved output block and retire whichever
    /// finished during it.
    #[inline]
    pub fn mix_into(&mut self, output: &mut [f32], output_channels: usize) {
        if let Some(voice) = self.current.as_mut() {
            if voice.mix_into(output, output_channels) {
                Self::retire(self.current.take());
            }
        }
        if let Some(voice) = self.releasing.as_mut() {
            if voice.mix_into(output, output_channels) {
                Self::retire(self.releasing.take());
            }
        }
    }

    fn release(&mut self, mut voice: AudioFileAudition, output_rate: u32) {
        voice.begin_release(output_rate);
        // Only one fade-out slot: a third selection inside 8 ms drops the
        // oldest, which is quieter than letting the queue grow.
        Self::retire(self.releasing.replace(voice));
    }

    fn retire(voice: Option<AudioFileAudition>) {
        if let Some(voice) = voice {
            crate::graveyard::retire_audio_file(voice.into_source());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFileFormat {
    Wav,
    Rauf,
    Mp3,
    Flac,
    Ogg,
    M4a,
    /// ISO base-media video container (`mp4`/`m4v`/`mov`) read for its audio
    /// track only — the Video track's sound. Same demuxer as [`Self::M4a`];
    /// kept separate so diagnostics say which kind of file was opened.
    Mp4,
    Aiff,
    Unknown,
}

impl AudioFileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Rauf => "rauf",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::M4a => "m4a",
            Self::Mp4 => "mp4",
            Self::Aiff => "aiff",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioFileInfo {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub total_frames: u64,
    pub duration_seconds: f64,
    pub format: AudioFileFormat,
}

// ── Multi-LOD peak generator ──────────────────────────────────────────────────

/// One min/max pair summarising a contiguous mono span of samples.
#[derive(Debug, Clone, Copy)]
pub struct AudioPeak {
    pub min: f32,
    pub max: f32,
}

/// One mip level: every entry summarises `samples_per_peak` consecutive
/// mono samples. Channels are averaged into mono at decode time so the
/// LOD ladder is independent of channel count.
#[derive(Debug, Clone)]
pub struct AudioPeakLod {
    pub samples_per_peak: u32,
    pub peaks: Vec<AudioPeak>,
}

/// Full peak summary for one decoded source file. Mirrors the shape the
/// Native UI's `waveform_cache::WaveformPreview` consumed before this
/// peak system was centralised here; Electron's `generate_wav_peaks`
/// stays as a single-LOD Int16 surface for back-compat.
#[derive(Debug, Clone)]
pub struct AudioPeakFile {
    pub source_path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub total_frames: u64,
    pub duration_seconds: f64,
    pub format: AudioFileFormat,
    /// Sorted ascending by `samples_per_peak`. UI picks the coarsest LOD
    /// whose `samples_per_peak` is still ≤ the zoom's samples-per-pixel.
    pub lods: Vec<AudioPeakLod>,
}

/// LOD ladder required by `tasks/native/006-NativeStudio.txt` PART 5.
/// Power-of-two from 256 to 65536 — keeps zoom transitions one bilinear
/// step apart at every meaningful zoom level.
pub const PEAK_LOD_LEVELS: &[u32] = &[256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

/// WAV files at or above this size refuse full in-memory decode.
pub const STREAMING_WAV_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// Non-WAV formats refuse in-memory decode above this size.
pub const MAX_IN_MEMORY_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// A WAV `fmt ` chunk is 16 / 18 / 40 bytes in practice (PCM / WAVEFORMATEX /
/// WAVEFORMATEXTENSIBLE). The chunk length is an untrusted 32-bit field, so a
/// crafted file can claim up to 4 GiB here; reject anything past this generous
/// ceiling before allocating the read buffer in `read_wav_header`.
const MAX_WAV_FMT_CHUNK_BYTES: u64 = 4096;

/// Generate a multi-LOD peak summary for any audio format supported by
/// [`load_audio_file`] (WAV via inline RIFF parser, MP3 / FLAC / OGG / M4A /
/// AIFF via symphonia). WAV files are scanned from disk in chunks without
/// loading the full PCM buffer. Other formats decode in memory when small
/// enough; larger files return an error.
pub fn generate_audio_peaks(path: impl AsRef<Path>) -> Result<AudioPeakFile, SphereAudioError> {
    let path = path.as_ref();
    let info = probe_audio_file(path)?;
    match info.format {
        AudioFileFormat::Wav => generate_wav_peaks_streaming(path, &info),
        AudioFileFormat::Rauf => generate_rauf_peaks_streaming(path, &info),
        _ => generate_peaks_in_memory(path, &info),
    }
}

fn generate_peaks_in_memory(
    path: &Path,
    info: &AudioFileInfo,
) -> Result<AudioPeakFile, SphereAudioError> {
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if file_size > MAX_IN_MEMORY_DECODE_BYTES {
        return Err(SphereAudioError::NativeError(format!(
            "file too large ({} bytes) for in-memory peak generation — convert to WAV",
            file_size
        )));
    }

    let path_str = path.to_string_lossy().to_string();
    let buffer = load_audio_file(&path_str)
        .map_err(|error| SphereAudioError::NativeError(format!("decode failed: {error}")))?;

    if buffer.frames == 0 || buffer.channels == 0 {
        return Err(SphereAudioError::NativeError(format!(
            "peak generation: empty buffer for '{}'",
            path.display()
        )));
    }

    let lods =
        peaks_from_interleaved_buffer(&buffer.samples, buffer.channels, buffer.frames as u64);
    Ok(AudioPeakFile {
        source_path: info.path.clone(),
        sample_rate: info.sample_rate,
        channels: info.channels,
        total_frames: info.total_frames.max(buffer.frames as u64),
        duration_seconds: info.duration_seconds,
        format: info.format,
        lods,
    })
}

fn peaks_from_interleaved_buffer(
    samples: &[f32],
    channels: usize,
    total_frames: u64,
) -> Vec<AudioPeakLod> {
    let channels = channels.max(1);
    let mut builders: Vec<PeakLodBuilder> = PEAK_LOD_LEVELS
        .iter()
        .map(|&spp| PeakLodBuilder::with_capacity(spp, total_frames))
        .collect();

    let mut sample_cursor = 0usize;
    while sample_cursor + channels <= samples.len() {
        let mut sum = 0.0f32;
        for c in 0..channels {
            sum += samples[sample_cursor + c];
        }
        let mono = (sum / channels as f32).clamp(-1.0, 1.0);
        for b in &mut builders {
            b.push(mono);
        }
        sample_cursor += channels;
    }

    builders.into_iter().map(PeakLodBuilder::finalize).collect()
}

fn generate_wav_peaks_streaming(
    path: &Path,
    info: &AudioFileInfo,
) -> Result<AudioPeakFile, SphereAudioError> {
    let mut file = File::open(path).map_err(|e| {
        SphereAudioError::NativeError(format!("Cannot open '{}': {e}", path.display()))
    })?;
    let (fmt, data_start, data_len) = read_wav_header(&mut file)
        .map_err(|e| SphereAudioError::NativeError(format!("WAV header read failed: {e}")))?;

    // The `data` chunk size is an untrusted 32-bit header field; clamp it to the
    // bytes actually on disk so a crafted size can't pre-size the LOD builders to
    // gigabytes (`Vec::with_capacity`) before the read loop reaches EOF.
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let data_len = data_len.min(file_size.saturating_sub(data_start));

    let bytes_per_sample = match fmt.bits_per_sample {
        8 => 1usize,
        16 => 2,
        24 => 3,
        32 => 4,
        bits => {
            return Err(SphereAudioError::NativeError(format!(
                "unsupported WAV bit depth for peak scan: {bits}"
            )))
        }
    };
    let bytes_per_frame = fmt.channels * bytes_per_sample;
    if bytes_per_frame == 0 || data_len < bytes_per_frame as u64 {
        return Err(SphereAudioError::NativeError("empty WAV data".to_string()));
    }

    let frames = data_len / bytes_per_frame as u64;
    let mut builders: Vec<PeakLodBuilder> = PEAK_LOD_LEVELS
        .iter()
        .map(|&spp| PeakLodBuilder::with_capacity(spp, frames))
        .collect();

    file.seek(SeekFrom::Start(data_start))
        .map_err(|e| SphereAudioError::NativeError(format!("seek failed: {e}")))?;

    let mut buffer = vec![0u8; 1024 * 1024];
    let mut remaining = data_len;
    let channels = fmt.channels.max(1);

    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        let aligned = if remaining as usize <= buffer.len() {
            wanted
        } else {
            (wanted / bytes_per_frame).max(1) * bytes_per_frame
        };
        let read = file
            .read(&mut buffer[..aligned])
            .map_err(|e| SphereAudioError::NativeError(format!("read failed: {e}")))?;
        if read == 0 {
            break;
        }

        let frame_count = read / bytes_per_frame;
        for frame in 0..frame_count {
            let frame_byte = frame * bytes_per_frame;
            let mut sum = 0.0f32;
            for ch in 0..channels {
                let sample_byte = frame_byte + ch * bytes_per_sample;
                let value = decode_wav_sample(&buffer, sample_byte, &fmt).map_err(|e| {
                    SphereAudioError::NativeError(format!("sample decode failed: {e}"))
                })?;
                sum += value;
            }
            let mono = (sum / channels as f32).clamp(-1.0, 1.0);
            for b in &mut builders {
                b.push(mono);
            }
        }

        remaining = remaining.saturating_sub((frame_count * bytes_per_frame) as u64);
    }

    for b in &mut builders {
        b.flush_partial();
    }

    let lods: Vec<AudioPeakLod> = builders.into_iter().map(PeakLodBuilder::finalize).collect();

    Ok(AudioPeakFile {
        source_path: info.path.clone(),
        sample_rate: info.sample_rate,
        channels: info.channels,
        total_frames: info.total_frames.max(frames),
        duration_seconds: info.duration_seconds,
        format: info.format,
        lods,
    })
}

/// Stream `.rauf` peaks straight from disk in 1 MiB chunks instead of decoding
/// the whole recording into memory. RAUF v1 is raw interleaved S32/F32 LE PCM,
/// so we decode each frame in place and average channels to mono — RAM stays
/// bounded by the read buffer + the (small) peak ladder regardless of take
/// length.
fn generate_rauf_peaks_streaming(
    path: &Path,
    info: &AudioFileInfo,
) -> Result<AudioPeakFile, SphereAudioError> {
    let reader = RaufReader::open(path)
        .map_err(|e| SphereAudioError::NativeError(format!("RAUF open failed: {e}")))?;
    let header = reader.header().clone();
    if !header.interleaved {
        return Err(SphereAudioError::NativeError(
            "RAUF peak scan requires interleaved PCM".to_string(),
        ));
    }
    let frames = if header.flags & sphere_encoder::rauf::RAUF_FLAG_FINALIZED != 0 {
        header.frames_written
    } else {
        reader
            .recover_frames_from_size()
            .map_err(|e| SphereAudioError::NativeError(format!("RAUF recovery failed: {e}")))?
    };

    let channels = (header.channels as usize).max(1);
    let bytes_per_sample = 4usize; // S32 / F32 little-endian
    let bytes_per_frame = channels * bytes_per_sample;
    // `frames_written` is an untrusted header field; clamp to the frames the file
    // can actually hold so a crafted value can't blow up the LOD builders.
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let max_frames = file_size.saturating_sub(header.data_offset) / bytes_per_frame as u64;
    let frames = frames.min(max_frames);
    let data_len = frames.saturating_mul(bytes_per_frame as u64);

    let mut builders: Vec<PeakLodBuilder> = PEAK_LOD_LEVELS
        .iter()
        .map(|&spp| PeakLodBuilder::with_capacity(spp, frames))
        .collect();

    let mut file = File::open(path)
        .map_err(|e| SphereAudioError::NativeError(format!("RAUF open failed: {e}")))?;
    file.seek(SeekFrom::Start(header.data_offset))
        .map_err(|e| SphereAudioError::NativeError(format!("RAUF seek failed: {e}")))?;

    let mut buffer = vec![0u8; 1024 * 1024];
    let mut remaining = data_len;
    let format = header.sample_format;

    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        let aligned = if remaining as usize <= buffer.len() {
            wanted
        } else {
            (wanted / bytes_per_frame).max(1) * bytes_per_frame
        };
        let read = file
            .read(&mut buffer[..aligned])
            .map_err(|e| SphereAudioError::NativeError(format!("RAUF read failed: {e}")))?;
        if read == 0 {
            break;
        }

        let frame_count = read / bytes_per_frame;
        for frame in 0..frame_count {
            let frame_byte = frame * bytes_per_frame;
            let mut sum = 0.0f32;
            for ch in 0..channels {
                let sb = frame_byte + ch * bytes_per_sample;
                let raw = [buffer[sb], buffer[sb + 1], buffer[sb + 2], buffer[sb + 3]];
                let value = match format {
                    RaufSampleFormat::S32 => i32::from_le_bytes(raw) as f32 / 2_147_483_648.0,
                    RaufSampleFormat::F32 => f32::from_le_bytes(raw),
                };
                sum += value;
            }
            let mono = (sum / channels as f32).clamp(-1.0, 1.0);
            for b in &mut builders {
                b.push(mono);
            }
        }

        remaining = remaining.saturating_sub((frame_count * bytes_per_frame) as u64);
    }

    for b in &mut builders {
        b.flush_partial();
    }

    let lods: Vec<AudioPeakLod> = builders.into_iter().map(PeakLodBuilder::finalize).collect();

    Ok(AudioPeakFile {
        source_path: info.path.clone(),
        sample_rate: info.sample_rate,
        channels: info.channels,
        total_frames: info.total_frames.max(frames),
        duration_seconds: info.duration_seconds,
        format: info.format,
        lods,
    })
}

struct PeakLodBuilder {
    samples_per_peak: u32,
    min: f32,
    max: f32,
    count: u32,
    peaks: Vec<AudioPeak>,
}

impl PeakLodBuilder {
    fn with_capacity(samples_per_peak: u32, total_samples_hint: u64) -> Self {
        let spp = samples_per_peak.max(1);
        let cap = (total_samples_hint as usize / spp as usize).saturating_add(1);
        Self {
            samples_per_peak: spp,
            min: 0.0,
            max: 0.0,
            count: 0,
            peaks: Vec::with_capacity(cap),
        }
    }

    #[inline]
    fn push(&mut self, v: f32) {
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
        self.count += 1;
        if self.count >= self.samples_per_peak {
            self.peaks.push(AudioPeak {
                min: self.min,
                max: self.max,
            });
            self.min = 0.0;
            self.max = 0.0;
            self.count = 0;
        }
    }

    fn finalize(mut self) -> AudioPeakLod {
        self.flush_partial();
        AudioPeakLod {
            samples_per_peak: self.samples_per_peak,
            peaks: self.peaks,
        }
    }

    fn flush_partial(&mut self) {
        if self.count > 0 {
            self.peaks.push(AudioPeak {
                min: self.min,
                max: self.max,
            });
            self.min = 0.0;
            self.max = 0.0;
            self.count = 0;
        }
    }
}

pub fn probe_audio_file(path: impl AsRef<Path>) -> Result<AudioFileInfo, SphereAudioError> {
    let path = path.as_ref();
    let format = audio_file_format(path);
    match format {
        AudioFileFormat::Wav => probe_wav_file(path, format),
        AudioFileFormat::Rauf => probe_rauf_file(path),
        AudioFileFormat::Mp3
        | AudioFileFormat::Flac
        | AudioFileFormat::Ogg
        | AudioFileFormat::M4a
        | AudioFileFormat::Mp4
        | AudioFileFormat::Aiff => probe_via_symphonia(path, format),
        AudioFileFormat::Unknown => Err(SphereAudioError::NativeError(format!(
            "unsupported audio format for '{}'",
            path.display()
        ))),
    }
}

/// Load an audio file from `path` into a decoded `AudioFileBuffer`.
///
/// Supported extensions: `rauf`, `wav`, `wave`, `mp3`, `flac`, `ogg`, `oga`,
/// `m4a`, `aiff`, `aif`, and the ISO base-media video containers `mp4`, `m4v`,
/// `mov` (audio track only — the Video track's sound).
///
/// Returns an error string on failure; the caller logs it and skips the clip.
/// Decode only the first `max_seconds` of `path`, for the Browser preview.
///
/// The preview voice never plays more than [`AUDITION_PREVIEW_SECONDS`], so
/// decoding the whole file to throw almost all of it away is pure latency: a
/// 100 MB stem cost a 100 MB read plus a ~200 MB `Vec` before a single sample
/// was audible. Worse, `load_wav` *refuses* files at or above
/// [`STREAMING_WAV_THRESHOLD_BYTES`] (64 MB), so previewing an ordinary long
/// stem failed outright and the Browser simply never played anything.
///
/// Reading a bounded head sidesteps both: the size guard is about how much is
/// held in memory, and this holds seconds rather than files.
pub fn load_audio_file_head(path: &str, max_seconds: f64) -> Result<AudioFileBuffer, String> {
    let p = Path::new(path);
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "wav" | "wave" => load_wav_head(p, max_seconds),
        // Symphonia and RAUF still decode in full. They are bounded by
        // `MAX_IN_MEMORY_DECODE_BYTES` (256 MB) rather than the 64 MB WAV
        // threshold, so they do not hit the failure above; trimming them is a
        // latency win, not a correctness fix, and is left for a separate change.
        _ => load_audio_file(path),
    }
}

/// Read at most `max_seconds` of PCM from the head of a WAV's data chunk.
///
/// Deliberately does not go through [`load_wav`]: that reads the entire file
/// into memory first, which is the cost this exists to avoid.
fn load_wav_head(path: &Path, max_seconds: f64) -> Result<AudioFileBuffer, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = File::open(path).map_err(|e| format!("open failed: {e}"))?;
    let (fmt, data_start, data_len) = read_wav_header(&mut file)?;
    if fmt.channels == 0 || fmt.sample_rate == 0 {
        return Err("invalid channel count or sample rate".to_string());
    }

    let bytes_per_sample = match fmt.bits_per_sample {
        8 => 1usize,
        16 => 2,
        24 => 3,
        32 => 4,
        bits => return Err(format!("unsupported WAV bit depth: {bits}")),
    };
    let bytes_per_frame = fmt.channels * bytes_per_sample;
    if bytes_per_frame == 0 || (data_len as usize) < bytes_per_frame {
        return Err("empty WAV data".to_string());
    }

    let available_frames = data_len as usize / bytes_per_frame;
    let wanted_frames = (max_seconds.max(0.0) * fmt.sample_rate as f64).ceil() as usize;
    let frames = available_frames.min(wanted_frames.max(1));
    let read_len = frames * bytes_per_frame;

    file.seek(SeekFrom::Start(data_start))
        .map_err(|e| format!("seek failed: {e}"))?;
    let mut bytes = vec![0u8; read_len];
    file.read_exact(&mut bytes)
        .map_err(|e| format!("read failed: {e}"))?;

    // `decode_wav_sample` indexes the buffer it is given, so offsets here are
    // relative to the head we just read, not to the start of the file.
    let sample_count = frames * fmt.channels;
    let mut samples = Vec::with_capacity(sample_count);
    let mut offset = 0usize;
    for _ in 0..sample_count {
        samples.push(decode_wav_sample(&bytes, offset, &fmt)?);
        offset += bytes_per_sample;
    }

    Ok(AudioFileBuffer {
        sample_rate: fmt.sample_rate,
        channels: fmt.channels,
        frames,
        samples,
    })
}

pub fn load_audio_file(path: &str) -> Result<AudioFileBuffer, String> {
    let p = Path::new(path);
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        // Fast path — hand-written RIFF/WAVE parser (no symphonia overhead).
        "wav" | "wave" => load_wav(p),
        "rauf" => load_rauf(p),

        // Symphonia handles everything else.
        "mp3" | "flac" | "ogg" | "oga" | "m4a" | "aiff" | "aif" | "mp4" | "m4v" | "mov" => {
            load_via_symphonia(p)
        }

        other => Err(format!("unsupported native audio format '{other}'")),
    }
}

fn audio_file_format(path: &Path) -> AudioFileFormat {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "wav" | "wave" => AudioFileFormat::Wav,
        "rauf" => AudioFileFormat::Rauf,
        "mp3" => AudioFileFormat::Mp3,
        "flac" => AudioFileFormat::Flac,
        "ogg" | "oga" => AudioFileFormat::Ogg,
        "m4a" => AudioFileFormat::M4a,
        "mp4" | "m4v" | "mov" => AudioFileFormat::Mp4,
        "aiff" | "aif" => AudioFileFormat::Aiff,
        _ => AudioFileFormat::Unknown,
    }
}

fn probe_wav_file(path: &Path, format: AudioFileFormat) -> Result<AudioFileInfo, SphereAudioError> {
    let mut file = File::open(path).map_err(|e| {
        SphereAudioError::NativeError(format!("Cannot open '{}': {e}", path.display()))
    })?;
    let (fmt, _data_start, data_len) = read_wav_header(&mut file).map_err(|e| {
        SphereAudioError::NativeError(format!(
            "WAV metadata read failed for '{}': {e}",
            path.display()
        ))
    })?;
    let bytes_per_sample = (fmt.bits_per_sample / 8) as u64;
    let bytes_per_frame = fmt.channels as u64 * bytes_per_sample;
    if bytes_per_frame == 0 || fmt.sample_rate == 0 {
        return Err(SphereAudioError::NativeError(format!(
            "invalid WAV metadata for '{}'",
            path.display()
        )));
    }
    let total_frames = data_len / bytes_per_frame;
    Ok(AudioFileInfo {
        path: path.to_path_buf(),
        sample_rate: fmt.sample_rate,
        channels: fmt.channels as u16,
        total_frames,
        duration_seconds: total_frames as f64 / fmt.sample_rate as f64,
        format,
    })
}

fn probe_rauf_file(path: &Path) -> Result<AudioFileInfo, SphereAudioError> {
    let reader = RaufReader::open(path).map_err(|e| {
        SphereAudioError::NativeError(format!(
            "RAUF metadata read failed for '{}': {e}",
            path.display()
        ))
    })?;
    let header = reader.header();
    let total_frames = if header.flags & sphere_encoder::rauf::RAUF_FLAG_FINALIZED != 0 {
        header.frames_written
    } else {
        reader.recover_frames_from_size().map_err(|e| {
            SphereAudioError::NativeError(format!(
                "RAUF recovery read failed for '{}': {e}",
                path.display()
            ))
        })?
    };
    Ok(AudioFileInfo {
        path: path.to_path_buf(),
        sample_rate: header.sample_rate,
        channels: header.channels,
        total_frames,
        duration_seconds: total_frames as f64 / header.sample_rate as f64,
        format: AudioFileFormat::Rauf,
    })
}

fn probe_via_symphonia(
    path: &Path,
    format_kind: AudioFileFormat,
) -> Result<AudioFileInfo, SphereAudioError> {
    let src = File::open(path).map_err(|e| {
        SphereAudioError::NativeError(format!("Cannot open '{}': {e}", path.display()))
    })?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
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
        .map_err(|e| SphereAudioError::NativeError(format!("Format probe failed: {e}")))?;

    let mut format = probed.format;
    // A video container also exposes its picture track, so require a sample
    // rate — that is what distinguishes an audio stream from a video one.
    let track = format
        .tracks()
        .iter()
        .find(|t| {
            t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL
                && t.codec_params.sample_rate.is_some()
        })
        .ok_or_else(|| SphereAudioError::NativeError("No decodable audio track found".to_string()))?
        .clone();

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| SphereAudioError::NativeError("Track has no sample rate".to_string()))?;
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count() as u16)
        .unwrap_or(1)
        .max(1);

    let total_frames = match track.codec_params.n_frames {
        Some(frames) if frames > 0 => frames,
        _ => decode_frame_count(&mut format, &track, channels)?,
    };

    if total_frames == 0 {
        return Err(SphereAudioError::NativeError(format!(
            "no audio frames decoded for '{}'",
            path.display()
        )));
    }

    Ok(AudioFileInfo {
        path: path.to_path_buf(),
        sample_rate,
        channels,
        total_frames,
        duration_seconds: total_frames as f64 / sample_rate as f64,
        format: format_kind,
    })
}

fn decode_frame_count(
    format: &mut Box<dyn symphonia::core::formats::FormatReader>,
    track: &symphonia::core::formats::Track,
    channels: u16,
) -> Result<u64, SphereAudioError> {
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| {
            SphereAudioError::NativeError(format!("Failed to create codec decoder: {e}"))
        })?;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut frames_decoded = 0u64;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                return Err(SphereAudioError::NativeError(format!(
                    "Packet read error: {e}"
                )))
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf_ref) => {
                if sample_buf.is_none() {
                    sample_buf = Some(SampleBuffer::<f32>::new(
                        audio_buf_ref.capacity() as u64,
                        *audio_buf_ref.spec(),
                    ));
                }
                if let Some(buf) = &mut sample_buf {
                    buf.copy_interleaved_ref(audio_buf_ref);
                    frames_decoded += (buf.samples().len() / channels as usize) as u64;
                }
            }
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(SphereAudioError::NativeError(format!("Decode error: {e}"))),
        }
    }

    Ok(frames_decoded)
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "napi"), allow(dead_code))]
pub struct WavPeakResult {
    pub sample_rate: u32,
    pub channel_count: u32,
    pub duration: f64,
    pub samples_per_peak: u32,
    pub peak_count: u32,
    pub peaks: Vec<i32>,
}

#[cfg_attr(not(feature = "napi"), allow(dead_code))]
pub fn generate_wav_peaks_from_path(
    path: &str,
    samples_per_peak: u32,
) -> Result<WavPeakResult, String> {
    let p = Path::new(path);
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "wav" && ext != "wave" {
        return Err("Rust peak generation currently supports PCM WAV only".to_string());
    }

    let mut file = File::open(p).map_err(|e| format!("Cannot open '{}': {e}", p.display()))?;
    let (fmt, data_start, data_len) = read_wav_header(&mut file)?;
    if fmt.audio_format != 1 || !matches!(fmt.bits_per_sample, 16 | 24 | 32) {
        return Err(format!(
            "unsupported WAV format for peak scan: format={} bits={}",
            fmt.audio_format, fmt.bits_per_sample
        ));
    }

    let bytes_per_sample = (fmt.bits_per_sample / 8) as usize;
    let bytes_per_frame = fmt.channels * bytes_per_sample;
    if bytes_per_frame == 0 || data_len < bytes_per_frame as u64 {
        return Err("empty WAV data".to_string());
    }

    let frames = (data_len / bytes_per_frame as u64) as usize;
    let spp = samples_per_peak.max(1) as usize;
    let peak_count = frames.div_ceil(spp);
    let mut peaks = vec![0i32; peak_count * fmt.channels * 2];
    let mut min = vec![1.0f32; fmt.channels];
    let mut max = vec![-1.0f32; fmt.channels];

    file.seek(SeekFrom::Start(data_start))
        .map_err(|e| format!("seek failed: {e}"))?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut remaining = data_len;
    let mut frame_index = 0usize;
    let mut current_peak = 0usize;

    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        let aligned = if remaining as usize <= buffer.len() {
            wanted
        } else {
            (wanted / bytes_per_frame).max(1) * bytes_per_frame
        };
        let read = file
            .read(&mut buffer[..aligned])
            .map_err(|e| format!("read failed: {e}"))?;
        if read == 0 {
            break;
        }

        let frame_count = read / bytes_per_frame;
        for frame in 0..frame_count {
            let frame_byte = frame * bytes_per_frame;
            for ch in 0..fmt.channels {
                let sample_byte = frame_byte + ch * bytes_per_sample;
                let value = read_wav_pcm_sample(&buffer, sample_byte, fmt.bits_per_sample)?;
                if value < min[ch] {
                    min[ch] = value;
                }
                if value > max[ch] {
                    max[ch] = value;
                }
            }

            frame_index += 1;
            if frame_index.is_multiple_of(spp) {
                write_i16_peak_i32(&mut peaks, current_peak, fmt.channels, &min, &max);
                current_peak += 1;
                reset_peak_min_max(&mut min, &mut max);
            }
        }

        remaining = remaining.saturating_sub((frame_count * bytes_per_frame) as u64);
    }

    if current_peak < peak_count {
        write_i16_peak_i32(&mut peaks, current_peak, fmt.channels, &min, &max);
    }

    Ok(WavPeakResult {
        sample_rate: fmt.sample_rate,
        channel_count: fmt.channels as u32,
        duration: frames as f64 / fmt.sample_rate as f64,
        samples_per_peak: spp as u32,
        peak_count: peak_count as u32,
        peaks,
    })
}

// ── Symphonia decoder ──────────────────────────────────────────────────────────

fn load_via_symphonia(path: &Path) -> Result<AudioFileBuffer, String> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_IN_MEMORY_DECODE_BYTES {
        return Err(format!(
            "file too large ({size} bytes) for in-memory decode — convert to WAV for streaming import"
        ));
    }

    let src = File::open(path).map_err(|e| format!("Cannot open '{}': {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
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
        .map_err(|e| format!("Format probe failed: {e}"))?;

    let mut format = probed.format;

    // Pick the first decodable audio track. A video container also exposes its
    // picture track here, so a sample rate is required as well — that is what
    // distinguishes an audio stream from a video one.
    let track = format
        .tracks()
        .iter()
        .find(|t| {
            t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL
                && t.codec_params.sample_rate.is_some()
        })
        .ok_or_else(|| "No decodable audio track found".to_string())?
        .clone();

    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| "Track has no sample rate".to_string())?;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Failed to create codec decoder: {e}"))?;

    let mut all_samples: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // Clean EOF.
            Err(SymphoniaError::IoError(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                break;
            }
            // The codec / format needs a reset (e.g. after a seek or stream error).
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(format!("Packet read error: {e}")),
        };

        // Skip packets that belong to other tracks (e.g. album art).
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf_ref) => {
                // Initialise the sample buffer on first decoded block.
                if sample_buf.is_none() {
                    let spec = *audio_buf_ref.spec();
                    sample_buf = Some(SampleBuffer::<f32>::new(
                        audio_buf_ref.capacity() as u64,
                        spec,
                    ));
                }
                if let Some(buf) = &mut sample_buf {
                    buf.copy_interleaved_ref(audio_buf_ref);
                    all_samples.extend_from_slice(buf.samples());
                }
            }
            // Benign decode errors — skip the packet and keep going.
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Decode error: {e}")),
        }
    }

    let frames = all_samples.len().checked_div(channels).unwrap_or(0);
    Ok(AudioFileBuffer {
        sample_rate,
        channels,
        frames,
        samples: all_samples,
    })
}

// ── Hand-written RIFF/WAVE parser ─────────────────────────────────────────────
//
// Supports PCM 8 / 16 / 24 / 32-bit integer and IEEE float 32-bit.
// Used instead of symphonia for WAV to avoid the extra dependency overhead on
// the most common format.

fn load_wav(path: &Path) -> Result<AudioFileBuffer, String> {
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if file_size >= STREAMING_WAV_THRESHOLD_BYTES {
        return Err(format!(
            "WAV file too large ({file_size} bytes) for in-memory decode — use streaming source"
        ));
    }

    let bytes = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
    let (fmt, data_start, data_len) = wav_data_layout(&bytes)?;
    if fmt.channels == 0 || fmt.sample_rate == 0 {
        return Err("invalid channel count or sample rate".to_string());
    }

    let bytes_per_sample = match fmt.bits_per_sample {
        8 => 1usize,
        16 => 2,
        24 => 3,
        32 => 4,
        bits => return Err(format!("unsupported WAV bit depth: {bits}")),
    };
    let bytes_per_frame = fmt.channels * bytes_per_sample;
    if bytes_per_frame == 0 || data_len < bytes_per_frame {
        return Err("empty WAV data".to_string());
    }

    let frames = data_len / bytes_per_frame;
    let sample_count = frames * fmt.channels;
    let mut samples = Vec::with_capacity(sample_count);

    let mut offset = data_start;
    for _ in 0..sample_count {
        let value = decode_wav_sample(&bytes, offset, &fmt)?;
        samples.push(value);
        offset += bytes_per_sample;
    }

    Ok(AudioFileBuffer {
        sample_rate: fmt.sample_rate,
        channels: fmt.channels,
        frames,
        samples,
    })
}

fn load_rauf(path: &Path) -> Result<AudioFileBuffer, String> {
    let reader = RaufReader::open(path).map_err(|e| format!("RAUF open failed: {e}"))?;
    let header = reader.header().clone();
    if header.sample_format != RaufSampleFormat::S32 {
        return Err("RAUF playback currently supports s32le only".to_string());
    }
    if !header.interleaved {
        return Err("RAUF playback requires interleaved PCM".to_string());
    }

    let frames = if header.flags & sphere_encoder::rauf::RAUF_FLAG_FINALIZED != 0 {
        header.frames_written
    } else {
        reader
            .recover_frames_from_size()
            .map_err(|e| format!("RAUF recovery failed: {e}"))?
    };
    let channels = header.channels as usize;
    // `frames` (from `frames_written` or size recovery) is untrusted — clamp to
    // what the file can actually hold so a crafted header can't pre-allocate
    // gigabytes via the `vec![0u8; byte_len]` below.
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let bytes_per_frame = (channels as u64).saturating_mul(4);
    let frames = file_size
        .saturating_sub(header.data_offset)
        .checked_div(bytes_per_frame)
        .map_or(0, |max_frames| frames.min(max_frames));
    let sample_count = (frames as usize).saturating_mul(channels);
    let mut file = File::open(path).map_err(|e| format!("open failed: {e}"))?;
    file.seek(SeekFrom::Start(header.data_offset))
        .map_err(|e| format!("seek failed: {e}"))?;
    let byte_len = sample_count
        .checked_mul(4)
        .ok_or_else(|| "RAUF sample byte length overflow".to_string())?;
    let mut bytes = vec![0u8; byte_len];
    file.read_exact(&mut bytes)
        .map_err(|e| format!("read failed: {e}"))?;

    let mut samples = Vec::with_capacity(sample_count);
    for chunk in bytes.chunks_exact(4) {
        let sample = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        samples.push((sample as f32 / 2_147_483_648.0).clamp(-1.0, 1.0));
    }

    Ok(AudioFileBuffer {
        sample_rate: header.sample_rate,
        channels,
        frames: frames as usize,
        samples,
    })
}

fn read_wav_header(file: &mut File) -> Result<(WavFmt, u64, u64), String> {
    let mut header = [0u8; 12];
    file.read_exact(&mut header)
        .map_err(|e| format!("read WAV header failed: {e}"))?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }

    let mut fmt: Option<WavFmt> = None;
    let mut data_range: Option<(u64, u64)> = None;
    let mut cursor = 12u64;

    loop {
        let mut chunk_header = [0u8; 8];
        match file.read_exact(&mut chunk_header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("read WAV chunk header failed: {e}")),
        }
        let id = &chunk_header[0..4];
        let len = u32::from_le_bytes([
            chunk_header[4],
            chunk_header[5],
            chunk_header[6],
            chunk_header[7],
        ]) as u64;
        let body = cursor + 8;

        match id {
            b"fmt " => {
                // `len` is an untrusted 32-bit header field. Validate it against a
                // sane ceiling *before* allocating so a crafted file can't drive a
                // multi-gigabyte `vec![0u8; len]` (OOM/abort on import/probe).
                if !(16..=MAX_WAV_FMT_CHUNK_BYTES).contains(&len) {
                    return Err(format!("invalid fmt chunk length: {len}"));
                }
                let mut buf = vec![0u8; len as usize];
                file.read_exact(&mut buf)
                    .map_err(|e| format!("read fmt chunk failed: {e}"))?;
                if buf.len() < 16 {
                    return Err("invalid fmt chunk".to_string());
                }
                fmt = Some(WavFmt {
                    audio_format: u16::from_le_bytes([buf[0], buf[1]]),
                    channels: u16::from_le_bytes([buf[2], buf[3]]) as usize,
                    sample_rate: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
                    bits_per_sample: u16::from_le_bytes([buf[14], buf[15]]),
                });
            }
            b"data" => {
                data_range = Some((body, len));
                break;
            }
            _ => {
                file.seek(SeekFrom::Current(len as i64))
                    .map_err(|e| format!("skip WAV chunk failed: {e}"))?;
            }
        }

        if len % 2 == 1 {
            file.seek(SeekFrom::Current(1))
                .map_err(|e| format!("skip WAV padding failed: {e}"))?;
        }
        cursor = body + len + (len % 2);
    }

    let fmt = fmt.ok_or_else(|| "missing fmt chunk".to_string())?;
    let (data_start, data_len) = data_range.ok_or_else(|| "missing data chunk".to_string())?;
    if fmt.channels == 0 || fmt.sample_rate == 0 {
        return Err("invalid channel count or sample rate".to_string());
    }
    Ok((fmt, data_start, data_len))
}

// ── Byte-level helpers ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) struct WavFmt {
    pub audio_format: u16,
    pub channels: usize,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

/// Parse RIFF/WAVE layout from bytes without decoding PCM.
pub(crate) fn wav_data_layout(bytes: &[u8]) -> Result<(WavFmt, usize, usize), String> {
    if bytes.len() < 44 {
        return Err("file too small for WAV".to_string());
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }

    let mut cursor = 12usize;
    let mut fmt: Option<WavFmt> = None;
    let mut data_range: Option<(usize, usize)> = None;

    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let len = read_u32_le(bytes, cursor + 4)? as usize;
        let body = cursor + 8;
        let end = body.saturating_add(len);
        if end > bytes.len() {
            return Err("truncated WAV chunk".to_string());
        }

        match id {
            b"fmt " => {
                if len < 16 {
                    return Err("invalid fmt chunk".to_string());
                }
                fmt = Some(WavFmt {
                    audio_format: read_u16_le(bytes, body)?,
                    channels: read_u16_le(bytes, body + 2)? as usize,
                    sample_rate: read_u32_le(bytes, body + 4)?,
                    bits_per_sample: read_u16_le(bytes, body + 14)?,
                });
            }
            b"data" => {
                data_range = Some((body, len));
            }
            _ => {}
        }

        cursor = end + (len & 1);
    }

    let fmt = fmt.ok_or_else(|| "missing fmt chunk".to_string())?;
    let (data_start, data_len) = data_range.ok_or_else(|| "missing data chunk".to_string())?;
    if fmt.channels == 0 || fmt.sample_rate == 0 {
        return Err("invalid channel count or sample rate".to_string());
    }
    Ok((fmt, data_start, data_len))
}

/// Decode one interleaved sample from WAV bytes at `offset`.
pub(crate) fn decode_wav_sample(bytes: &[u8], offset: usize, fmt: &WavFmt) -> Result<f32, String> {
    let value = match (fmt.audio_format, fmt.bits_per_sample) {
        (1, 8) => {
            (bytes
                .get(offset)
                .copied()
                .ok_or_else(|| "unexpected EOF".to_string())? as f32
                - 128.0)
                / 128.0
        }
        (1, 16) => read_i16_le(bytes, offset)? as f32 / 32_768.0,
        (1, 24) => read_i24_le(bytes, offset)? as f32 / 8_388_608.0,
        (1, 32) => read_i32_le(bytes, offset)? as f32 / 2_147_483_648.0,
        (3, 32) => {
            let b = bytes
                .get(offset..offset + 4)
                .ok_or_else(|| "unexpected EOF".to_string())?;
            f32::from_le_bytes([b[0], b[1], b[2], b[3]])
        }
        (format, _) => return Err(format!("unsupported WAV format code: {format}")),
    };
    Ok(value.clamp(-1.0, 1.0))
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let b = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "unexpected EOF reading u16".to_string())?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let b = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "unexpected EOF reading u32".to_string())?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_i16_le(bytes: &[u8], offset: usize) -> Result<i16, String> {
    let b = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "unexpected EOF reading i16".to_string())?;
    Ok(i16::from_le_bytes([b[0], b[1]]))
}

fn read_i24_le(bytes: &[u8], offset: usize) -> Result<i32, String> {
    let b = bytes
        .get(offset..offset + 3)
        .ok_or_else(|| "unexpected EOF reading i24".to_string())?;
    let raw = ((b[2] as i32) << 16) | ((b[1] as i32) << 8) | b[0] as i32;
    Ok((raw << 8) >> 8)
}

fn read_i32_le(bytes: &[u8], offset: usize) -> Result<i32, String> {
    let b = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "unexpected EOF reading i32".to_string())?;
    Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg_attr(not(feature = "napi"), allow(dead_code))]
fn read_wav_pcm_sample(bytes: &[u8], offset: usize, bits_per_sample: u16) -> Result<f32, String> {
    let fmt = WavFmt {
        audio_format: 1,
        channels: 1,
        sample_rate: 0,
        bits_per_sample,
    };
    decode_wav_sample(bytes, offset, &fmt)
}

#[cfg_attr(not(feature = "napi"), allow(dead_code))]
fn reset_peak_min_max(min: &mut [f32], max: &mut [f32]) {
    for i in 0..min.len() {
        min[i] = 1.0;
        max[i] = -1.0;
    }
}

#[cfg_attr(not(feature = "napi"), allow(dead_code))]
fn write_i16_peak_i32(
    peaks: &mut [i32],
    peak_index: usize,
    channels: usize,
    min: &[f32],
    max: &[f32],
) {
    for ch in 0..channels {
        let base = (peak_index * channels + ch) * 2;
        peaks[base] = clamp_i16_as_i32(min[ch]);
        peaks[base + 1] = clamp_i16_as_i32(max[ch]);
    }
}

#[cfg_attr(not(feature = "napi"), allow(dead_code))]
fn clamp_i16_as_i32(value: f32) -> i32 {
    (value.clamp(-1.0, 1.0) * 32767.0)
        .round()
        .clamp(-32768.0, 32767.0) as i32
}

#[cfg(test)]
mod audition_tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Constant full-scale stereo source: any envelope shows up directly in the
    /// mixed output, so the declick ramps are readable from the block.
    fn dc_source(seconds: f32) -> Box<AudioFileBuffer> {
        let frames = (RATE as f32 * seconds) as usize;
        Box::new(AudioFileBuffer {
            sample_rate: RATE,
            channels: 2,
            frames,
            samples: vec![1.0; frames * 2],
        })
    }

    fn mix_block(player: &mut AuditionPlayer, frames: usize) -> Vec<f32> {
        let mut block = vec![0.0f32; frames * 2];
        player.mix_into(&mut block, 2);
        block
    }

    #[test]
    fn audition_ramps_in_rather_than_jumping_to_full_scale() {
        let mut player = AuditionPlayer::default();
        player.start(dc_source(1.0), RATE);
        let attack_frames = (AUDITION_ATTACK_SECONDS * RATE as f32) as usize;

        let block = mix_block(&mut player, 8);
        assert!(
            block[0] > 0.0 && block[0] < 0.05,
            "first frame must start on the ramp, got {}",
            block[0]
        );

        let settled = mix_block(&mut player, attack_frames);
        let last = settled[settled.len() - 1];
        assert!(
            (last - 1.0).abs() < 1e-6,
            "ramp must reach unity gain, got {last}"
        );
    }

    #[test]
    fn replacing_an_audition_fades_the_old_voice_instead_of_cutting_it() {
        let mut player = AuditionPlayer::default();
        player.start(dc_source(1.0), RATE);
        let _ = mix_block(&mut player, 4_800); // let the first voice settle

        player.start(dc_source(1.0), RATE);
        let block = mix_block(&mut player, 8);
        // The retiring voice is still audible (near unity) while the new one
        // ramps up, so the sum stays above the new voice on its own.
        assert!(
            block[0] > 0.9,
            "replaced voice must fade, not cut, got {}",
            block[0]
        );
    }

    #[test]
    fn stopped_audition_releases_to_silence_and_goes_idle() {
        let mut player = AuditionPlayer::default();
        player.start(dc_source(1.0), RATE);
        let _ = mix_block(&mut player, 4_800);

        player.stop(RATE);
        assert!(!player.is_idle(), "release voice must keep the mixer awake");

        let release_frames = (AUDITION_RELEASE_SECONDS * RATE as f32) as usize + 2;
        let block = mix_block(&mut player, release_frames);
        assert!(
            block[0] > 0.9,
            "release must start from the current gain, got {}",
            block[0]
        );
        assert!(
            block[block.len() - 1].abs() < 1e-6,
            "release must reach silence, got {}",
            block[block.len() - 1]
        );
        assert!(player.is_idle(), "finished release must retire the voice");
    }

    #[test]
    fn source_end_fades_out_before_the_last_frame() {
        let mut player = AuditionPlayer::default();
        // 20 ms source: long enough to pass the attack, short enough that the
        // tail fade covers the end of a single mixed block.
        player.start(dc_source(0.02), RATE);
        let block = mix_block(&mut player, (RATE as f32 * 0.02) as usize);
        let last = block[block.len() - 2];
        assert!(
            last.abs() < 0.2,
            "end of file must fade out, got {last} on the last frame"
        );
        assert!(player.is_idle(), "finished source must retire the voice");
    }

    #[test]
    fn long_file_previews_only_its_head_and_reports_the_playhead() {
        let mut player = AuditionPlayer::default();
        // Twice the preview limit: playback must stop at the limit, not at EOF.
        player.start(dc_source(AUDITION_PREVIEW_SECONDS as f32 * 2.0), RATE);

        let limit_frames = (AUDITION_PREVIEW_SECONDS * RATE as f64) as usize;
        let _ = mix_block(&mut player, limit_frames / 2);
        let halfway = player
            .position_seconds()
            .expect("an auditioning voice must report a playhead");
        assert!(
            (halfway - AUDITION_PREVIEW_SECONDS / 2.0).abs() < 0.01,
            "playhead must track the source position, got {halfway}"
        );

        let block = mix_block(&mut player, limit_frames / 2 + 2);
        assert!(
            block[block.len() - 2].abs() < 1e-6,
            "preview must be silent past the limit, got {}",
            block[block.len() - 2]
        );
        assert!(player.is_idle(), "preview limit must retire the voice");
        assert!(
            player.position_seconds().is_none(),
            "an idle player must report no playhead"
        );
    }

    #[test]
    fn a_file_shorter_than_the_preview_limit_still_plays_to_its_end() {
        let mut player = AuditionPlayer::default();
        player.start(dc_source(0.5), RATE);
        let _ = mix_block(&mut player, (RATE as f32 * 0.4) as usize);
        assert!(
            !player.is_idle(),
            "a half-second file must not be cut at 0.4 s"
        );
        let _ = mix_block(&mut player, (RATE as f32 * 0.1) as usize + 2);
        assert!(player.is_idle(), "the file must retire at its own end");
    }
}

#[cfg(test)]
mod peak_tests {
    use super::*;
    use sphere_encoder::rauf::{RaufConfig, RaufSampleFormat, RaufWriter};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "fb_rauf_peak_{label}_{}_{}.rauf",
            std::process::id(),
            nanos
        ))
    }

    /// Full-scale S32 value for a normalized sample in `-1.0..=1.0`.
    fn s32(value: f32) -> i32 {
        (value * 2_147_483_648.0).clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
    }

    fn write_rauf(path: &PathBuf, channels: u16, interleaved_frames: &[i32]) {
        let mut writer = RaufWriter::create(
            path,
            RaufConfig {
                sample_rate: 48_000,
                channels,
                sample_format: RaufSampleFormat::S32,
                interleaved: true,
                project_start_sample: 0,
                take_id: [0u8; 16],
            },
        )
        .unwrap();
        writer.write_s32le_interleaved(interleaved_frames).unwrap();
        writer.finalize().unwrap();
    }

    #[test]
    fn rauf_streaming_peaks_capture_mono_min_max() {
        // 600 mono frames alternating +0.5 / -0.5 → finest-LOD peaks span both.
        let path = temp_path("mono");
        let samples: Vec<i32> = (0..600)
            .map(|i| if i % 2 == 0 { s32(0.5) } else { s32(-0.5) })
            .collect();
        write_rauf(&path, 1, &samples);

        let peaks = generate_audio_peaks(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(peaks.channels, 1);
        assert_eq!(peaks.total_frames, 600);
        assert_eq!(peaks.lods.len(), PEAK_LOD_LEVELS.len());
        let finest = &peaks.lods[0];
        assert_eq!(finest.samples_per_peak, PEAK_LOD_LEVELS[0]);
        let first = finest.peaks.first().expect("at least one peak");
        assert!((first.max - 0.5).abs() < 1e-3, "max was {}", first.max);
        assert!((first.min + 0.5).abs() < 1e-3, "min was {}", first.min);
    }

    #[test]
    fn rauf_streaming_peaks_average_stereo_to_mono() {
        // Both channels at +0.5 → mono average stays +0.5 (max), min 0.0.
        let path = temp_path("stereo");
        let frames: Vec<i32> = (0..512).flat_map(|_| [s32(0.5), s32(0.5)]).collect();
        write_rauf(&path, 2, &frames);

        let peaks = generate_audio_peaks(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(peaks.channels, 2);
        assert_eq!(peaks.total_frames, 512);
        let first = peaks.lods[0].peaks.first().expect("at least one peak");
        assert!((first.max - 0.5).abs() < 1e-3, "max was {}", first.max);
    }

    /// A crafted WAV whose `fmt ` chunk claims ~4 GiB must be rejected by the
    /// header parser instead of attempting a multi-gigabyte allocation.
    #[test]
    fn wav_header_rejects_absurd_fmt_chunk_length() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&0u32.to_le_bytes()); // riff size (unused here)
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes()); // absurd fmt length
        let path = std::env::temp_dir().join(format!(
            "fb_wav_fmt_guard_{}_{}.wav",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&path, &bytes).unwrap();
        let mut file = File::open(&path).unwrap();
        let result = read_wav_header(&mut file);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err(), "absurd fmt chunk length must be rejected");
    }

    /// A finalized RAUF whose `frames_written` claims `u64::MAX` must not drive a
    /// gigantic `Vec::with_capacity`; the scan clamps to the real file size.
    #[test]
    fn rauf_peaks_clamp_corrupt_frames_written_to_file_size() {
        use std::io::Write;

        let path = temp_path("corrupt_frames");
        let samples: Vec<i32> = (0..256).map(|_| s32(0.25)).collect();
        write_rauf(&path, 1, &samples);

        // Patch `frames_written` (header offset 24, u64 LE) to a hostile value.
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(24)).unwrap();
            f.write_all(&u64::MAX.to_le_bytes()).unwrap();
        }

        let peaks = generate_audio_peaks(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        // Bounded by the 256 frames actually present, not u64::MAX.
        let finest = &peaks.lods[0];
        assert!(
            finest.peaks.len() <= 4,
            "finest LOD must stay bounded to real frames, got {}",
            finest.peaks.len()
        );
    }
}

#[cfg(test)]
mod audition_head_tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 16-bit stereo WAV of `frames` frames at 48 kHz.
    fn write_wav(path: &Path, frames: usize) {
        let sample_rate = 48_000u32;
        let channels = 2u16;
        let bits = 16u16;
        let bytes_per_frame = (channels * bits / 8) as usize;
        let data_len = frames * bytes_per_frame;
        let mut out: Vec<u8> = Vec::with_capacity(44 + data_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * bytes_per_frame as u32).to_le_bytes());
        out.extend_from_slice(&(bytes_per_frame as u16).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        for i in 0..frames {
            let v = ((i % 1000) as i16).wrapping_mul(30);
            out.extend_from_slice(&v.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        let mut f = File::create(path).unwrap();
        f.write_all(&out).unwrap();
    }

    fn temp_wav(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "fb_audition_head_{label}_{}_{}.wav",
            std::process::id(),
            nanos
        ))
    }

    /// The head decode stops at the requested duration instead of reading the
    /// whole file.
    #[test]
    fn head_decode_truncates_to_requested_seconds() {
        let path = temp_wav("truncate");
        write_wav(&path, 48_000 * 10); // 10 seconds
        let buf = load_audio_file_head(path.to_str().unwrap(), AUDITION_PREVIEW_SECONDS).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(buf.sample_rate, 48_000);
        assert_eq!(buf.channels, 2);
        assert_eq!(buf.frames, (AUDITION_PREVIEW_SECONDS * 48_000.0) as usize);
        assert_eq!(buf.samples.len(), buf.frames * 2);
    }

    /// A file shorter than the preview window is returned whole — the limit
    /// only ever truncates.
    #[test]
    fn head_decode_keeps_short_files_intact() {
        let path = temp_wav("short");
        write_wav(&path, 4_800); // 0.1 s
        let buf = load_audio_file_head(path.to_str().unwrap(), AUDITION_PREVIEW_SECONDS).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(buf.frames, 4_800);
    }

    /// The regression this exists for: `load_wav` refuses any WAV at or above
    /// `STREAMING_WAV_THRESHOLD_BYTES`, so Browser preview of an ordinary long
    /// stem failed outright and nothing ever played. The head decode never
    /// reads the whole file, so it must succeed where the full decode errors.
    #[test]
    fn head_decode_previews_a_file_the_full_decoder_rejects() {
        // Just past the 64 MB in-memory WAV threshold.
        let frames = (STREAMING_WAV_THRESHOLD_BYTES as usize / 4) + 48_000;
        let path = temp_wav("oversize");
        write_wav(&path, frames);

        let full = load_audio_file(path.to_str().unwrap());
        let head = load_audio_file_head(path.to_str().unwrap(), AUDITION_PREVIEW_SECONDS);
        let _ = std::fs::remove_file(&path);

        assert!(
            full.is_err(),
            "full decode should still refuse an oversize WAV"
        );
        let head = head.expect("preview head must decode a file the full decoder rejects");
        assert_eq!(head.frames, (AUDITION_PREVIEW_SECONDS * 48_000.0) as usize);
    }
}
