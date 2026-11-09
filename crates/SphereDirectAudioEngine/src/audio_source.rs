//! Lightweight clip audio sources.
//!
//! Small files may be decoded into memory. Large files are streamed by a
//! background worker into bounded realtime-readable rings so the audio callback
//! never decodes, seeks, opens files, or faults file-backed mmap pages.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

use crate::audio_file::{
    decode_wav_sample, load_audio_file, wav_data_layout, AudioFileBuffer, WavFmt,
    MAX_IN_MEMORY_DECODE_BYTES, STREAMING_WAV_THRESHOLD_BYTES,
};
use crate::streaming_source::StreamingSource;

/// Playback source for one media file referenced by runtime clips.
#[derive(Debug, Clone)]
pub enum ClipAudioSource {
    InMemory(Arc<AudioFileBuffer>),
    MappedWav(Arc<MappedWavSource>),
    /// Large compressed file streamed from disk through a background decoder
    /// thread + ring buffer (Phase F). See [`crate::streaming_source`].
    Streaming(Arc<StreamingSource>),
}

impl ClipAudioSource {
    #[inline]
    pub fn sample_rate(&self) -> u32 {
        match self {
            Self::InMemory(buffer) => buffer.sample_rate,
            Self::MappedWav(mapped) => mapped.sample_rate,
            Self::Streaming(source) => source.sample_rate(),
        }
    }

    #[inline]
    pub fn channels(&self) -> usize {
        match self {
            Self::InMemory(buffer) => buffer.channels,
            Self::MappedWav(mapped) => mapped.channels,
            // The streaming ring is always downmixed to stereo.
            Self::Streaming(_) => 2,
        }
    }

    #[inline]
    pub fn frames(&self) -> usize {
        match self {
            Self::InMemory(buffer) => buffer.frames,
            Self::MappedWav(mapped) => mapped.frames,
            Self::Streaming(source) => source.frames(),
        }
    }

    #[inline]
    pub fn is_mapped(&self) -> bool {
        matches!(self, Self::MappedWav(_))
    }

    /// True for the disk-streaming variant (used by diagnostics / logging).
    #[inline]
    pub fn is_streaming(&self) -> bool {
        matches!(self, Self::Streaming(_))
    }
}

/// Memory-mapped PCM WAV — samples decoded on read from the mapped file bytes.
#[derive(Debug)]
pub struct MappedWavSource {
    pub sample_rate: u32,
    pub channels: usize,
    pub frames: usize,
    _file: File,
    mmap: Mmap,
    data_start: usize,
    bytes_per_sample: usize,
    bytes_per_frame: usize,
    fmt: WavFmt,
}

impl MappedWavSource {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("open failed: {e}"))?;
        let mmap = unsafe { Mmap::map(&file).map_err(|e| format!("mmap failed: {e}"))? };
        let (fmt, data_start, data_len) = wav_data_layout(&mmap)?;

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
        Ok(Self {
            sample_rate: fmt.sample_rate,
            channels: fmt.channels,
            frames,
            _file: file,
            mmap,
            data_start,
            bytes_per_sample,
            bytes_per_frame,
            fmt,
        })
    }

    #[inline]
    pub fn read_frame_stereo(&self, frame: usize) -> (f32, f32) {
        if frame >= self.frames || self.channels == 0 {
            return (0.0, 0.0);
        }
        match self.channels {
            1 => {
                let v = self.read_channel(frame, 0);
                (v, v)
            }
            _ => (self.read_channel(frame, 0), self.read_channel(frame, 1)),
        }
    }

    #[inline]
    fn read_channel(&self, frame: usize, channel: usize) -> f32 {
        let offset =
            self.data_start + frame * self.bytes_per_frame + channel * self.bytes_per_sample;
        decode_wav_sample(&self.mmap, offset, &self.fmt).unwrap_or(0.0)
    }
}

#[cfg(test)]
thread_local! {
    static STREAMING_WAV_THRESHOLD_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// WAVs this size or larger stream from disk. Tests on this thread can lower
/// it ([`with_streaming_wav_threshold`]) to stream a small file.
#[inline]
fn streaming_wav_threshold_bytes() -> u64 {
    #[cfg(test)]
    if let Some(bytes) = STREAMING_WAV_THRESHOLD_OVERRIDE.with(|cell| cell.get()) {
        return bytes;
    }
    STREAMING_WAV_THRESHOLD_BYTES
}

/// Run `f` with WAVs of `bytes` or more streamed from disk on this thread.
#[cfg(test)]
pub(crate) fn with_streaming_wav_threshold<R>(bytes: u64, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            STREAMING_WAV_THRESHOLD_OVERRIDE.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(STREAMING_WAV_THRESHOLD_OVERRIDE.with(|cell| cell.replace(Some(bytes))));
    f()
}

/// Open the best playback source for `path`.
///
/// Large WAVs and large compressed files use the disk-streaming source. Small
/// files can still be decoded in memory for low-latency editing and tests.
pub fn open_clip_audio_source(path: &str) -> Result<ClipAudioSource, String> {
    let p = Path::new(path);
    let file_size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if matches!(ext.as_str(), "wav" | "wave") && file_size >= streaming_wav_threshold_bytes() {
        let source = StreamingSource::open(path)?;
        eprintln!(
            "[SphereAudio] stream WAV '{}': {} bytes, {} frames @ {}Hz",
            path,
            file_size,
            source.frames(),
            source.sample_rate()
        );
        return Ok(ClipAudioSource::Streaming(Arc::new(source)));
    }

    if file_size > MAX_IN_MEMORY_DECODE_BYTES && !matches!(ext.as_str(), "wav" | "wave") {
        // Too large to decode into memory and not PCM-mappable: stream it from
        // disk through a background decoder + ring buffer (Phase F).
        let source = StreamingSource::open(path)?;
        return Ok(ClipAudioSource::Streaming(Arc::new(source)));
    }

    let buffer = Arc::new(load_audio_file(path)?);
    Ok(ClipAudioSource::InMemory(buffer))
}

#[inline]
pub fn read_frame_stereo(source: &ClipAudioSource, frame: usize) -> (f32, f32) {
    match source {
        ClipAudioSource::InMemory(buffer) => {
            let base = frame * buffer.channels;
            match buffer.channels {
                0 => (0.0, 0.0),
                1 => {
                    let v = buffer.samples.get(base).copied().unwrap_or(0.0);
                    (v, v)
                }
                _ => (
                    buffer.samples.get(base).copied().unwrap_or(0.0),
                    buffer.samples.get(base + 1).copied().unwrap_or(0.0),
                ),
            }
        }
        ClipAudioSource::MappedWav(mapped) => mapped.read_frame_stereo(frame),
        ClipAudioSource::Streaming(source) => source.read_frame_stereo(frame),
    }
}

#[inline]
pub fn sample_source_stereo(source: &ClipAudioSource, pos: f64) -> (f32, f32) {
    // The streaming ring does its own windowed interpolation + underrun
    // accounting, so route straight to it.
    if let ClipAudioSource::Streaming(streaming) = source {
        return streaming.read_interp(pos);
    }

    if pos < 0.0 || source.frames() == 0 {
        return (0.0, 0.0);
    }

    let frames = source.frames();
    let idx = pos.floor() as usize;
    if idx >= frames {
        return (0.0, 0.0);
    }
    let frac = (pos - idx as f64) as f32;
    let next_idx = (idx + 1).min(frames - 1);

    let (l0, r0) = read_frame_stereo(source, idx);
    let (l1, r1) = read_frame_stereo(source, next_idx);

    (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
}
