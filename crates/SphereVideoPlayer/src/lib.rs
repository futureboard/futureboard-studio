//! Video preview/reference decoding for the Futureboard Studio Video Track.
//!
//! Scope is deliberately narrow: this crate decodes **single still frames at a
//! requested presentation time** so the arrangement can show a reference video
//! next to the audio. It is not a playback engine — it produces no audio, owns
//! no clock, and never runs on a realtime thread.
//!
//! Decoding is MKFF's (`crates/mkff`), with no FFmpeg / libav and no OS media
//! framework:
//!
//! | Path     | Streams                                   | How                                          |
//! |----------|-------------------------------------------|----------------------------------------------|
//! | GPU      | H.264 / HEVC in MP4 / MOV                 | MKFF MP4 demux → `mkff-mediakit` accelerant (D3D11 / DXVA on Windows) |
//! | Software | everything `mkff-codecs` opens            | pure-Rust demux + decode, every platform     |
//!
//! [`VideoDecoder::open`] tries the GPU path first and falls back to software
//! when the file, codec or machine has no accelerant for it; a GPU decoder that
//! fails mid-stream is replaced by the software path for the rest of the
//! session. [`VideoInfo::decoder`] says which one is running.

use std::path::{Path, PathBuf};
use std::sync::Arc;

mod hardware;
mod preview;
mod software;

pub use preview::{PreviewStatus, VideoPreview, VideoPreviewFrame};

/// Container extensions the decoder is expected to open (MKFF's video
/// containers). Used for file dialog filters and drag-and-drop acceptance; the
/// decoder is still the authority and may reject a file whose codec MKFF does
/// not decode.
pub const SUPPORTED_VIDEO_EXTENSIONS: &[&str] = mkff_codecs::filmcraft::media::VIDEO_EXTENSIONS;

/// `true` when `path`'s extension is one this crate advertises support for.
pub fn is_supported_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| SUPPORTED_VIDEO_EXTENSIONS.contains(&e.as_str()))
}

/// Human-readable name of the decode framework, for diagnostics and the player
/// window's status line. Which path a file uses is [`VideoInfo::decoder`].
pub fn backend_name() -> &'static str {
    "mkff"
}

/// `true` when this build has a real decode backend. MKFF's software path
/// runs on every platform.
pub fn backend_available() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoError {
    /// This platform has no decode backend compiled in.
    UnsupportedPlatform,
    /// The file does not exist or could not be opened.
    NotFound(PathBuf),
    /// The container opened but carries no decodable video stream.
    NoVideoStream,
    /// The requested position is past the end of the media.
    EndOfStream,
    /// Decoder failure (unsupported codec, corrupt data, GPU error, …).
    Backend(String),
}

impl std::fmt::Display for VideoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "video decoding is not supported on this platform")
            }
            Self::NotFound(path) => write!(f, "video file not found: {}", path.display()),
            Self::NoVideoStream => write!(f, "file contains no decodable video stream"),
            Self::EndOfStream => write!(f, "requested position is past the end of the video"),
            Self::Backend(message) => write!(f, "video decoder error: {message}"),
        }
    }
}

impl std::error::Error for VideoError {}

/// Static properties of an opened video stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// Total presentation duration. `0.0` when the container does not report one.
    pub duration_seconds: f64,
    /// Nominal frame rate. `0.0` when unknown.
    pub frame_rate: f64,
    /// Decode path in use: `"GPU"` or `"software"`.
    pub decoder: &'static str,
}

impl VideoInfo {
    /// Display aspect ratio, or `16:9` when the stream reported no usable size.
    pub fn aspect_ratio(&self) -> f32 {
        if self.width == 0 || self.height == 0 {
            16.0 / 9.0
        } else {
            self.width as f32 / self.height as f32
        }
    }
}

/// One decoded frame in **BGRA8**, top-down, tightly packed (`width * 4` bytes
/// per row). BGRA is what GPUI's `RenderImage` uploads without conversion.
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub timestamp_seconds: f64,
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("timestamp_seconds", &self.timestamp_seconds)
            .field("bytes", &self.bgra.len())
            .finish()
    }
}

enum Backend {
    Hardware(hardware::HardwareDecoder),
    Software(software::SoftwareDecoder),
}

/// A single-stream, single-threaded video frame reader.
///
/// Not `Sync`. Own it from one worker thread (see [`VideoPreview`]) — never
/// from the GPUI render thread, because seek + decode can block for tens of
/// milliseconds on a long GOP.
pub struct VideoDecoder {
    path: PathBuf,
    backend: Backend,
}

impl VideoDecoder {
    /// Opens `path`'s first video stream: on the GPU when an accelerant takes
    /// it, otherwise in software.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, VideoError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(VideoError::NotFound(path));
        }
        let backend = match hardware::HardwareDecoder::open(&path) {
            Ok(decoder) => Backend::Hardware(decoder),
            Err(gpu_err) => {
                // Logged whether or not software takes the file: "why is this
                // clip not on the GPU" is the first question when it is slow.
                eprintln!(
                    "[video-mkff] GPU path declined {}: {gpu_err}",
                    path.display()
                );
                // The software path covers every container, so its error is
                // the one that explains the file.
                Backend::Software(software::SoftwareDecoder::open(&path)?)
            }
        };
        Ok(Self { path, backend })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn info(&self) -> VideoInfo {
        match &self.backend {
            Backend::Hardware(d) => d.info(),
            Backend::Software(d) => d.info(),
        }
    }

    /// Decodes the frame presented at (or immediately before) `seconds`.
    ///
    /// Seeks only when the target is behind the reader or past the next
    /// keyframe, so playback-rate stepping reads forward instead of re-seeking
    /// every frame.
    ///
    /// Returns the frame behind an `Arc`: a decoded frame is a full
    /// uncompressed image, and the same one is handed out repeatedly while the
    /// playhead stays inside it.
    pub fn frame_at(&mut self, seconds: f64) -> Result<Arc<VideoFrame>, VideoError> {
        let seconds = seconds.max(0.0);
        let result = match &mut self.backend {
            Backend::Software(d) => return d.frame_at(seconds),
            Backend::Hardware(d) => d.frame_at(seconds),
        };
        match result {
            Err(VideoError::Backend(message)) => {
                // The GPU decoder broke (device lost, driver rejects the
                // stream); continue in software rather than going dark.
                eprintln!(
                    "[video-mkff] GPU decode failed for {}: {message}; switching to software",
                    self.path.display()
                );
                let mut software = software::SoftwareDecoder::open(&self.path)?;
                let frame = software.frame_at(seconds);
                self.backend = Backend::Software(software);
                frame
            }
            other => other,
        }
    }
}

/// Probes a file for its dimensions/duration without keeping a decoder open.
pub fn probe(path: impl AsRef<Path>) -> Result<VideoInfo, VideoError> {
    Ok(VideoDecoder::open(path)?.info())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_filter_is_case_insensitive() {
        assert!(is_supported_video_path(Path::new("C:/clips/Take 1.MP4")));
        assert!(is_supported_video_path(Path::new("/tmp/ref.mov")));
        assert!(is_supported_video_path(Path::new("/tmp/ref.mkv")));
        assert!(!is_supported_video_path(Path::new("/tmp/mix.wav")));
        assert!(!is_supported_video_path(Path::new("/tmp/noextension")));
    }

    /// A path under a real directory, so the existence check is a local stat
    /// rather than a lookup on an absent drive letter (which can block for
    /// seconds on Windows).
    pub(crate) fn missing_video_path() -> PathBuf {
        std::env::temp_dir().join("futureboard-nonexistent-reference.mp4")
    }

    #[test]
    fn missing_file_reports_not_found_before_touching_a_backend() {
        let result = VideoDecoder::open(missing_video_path());
        assert!(matches!(result.err(), Some(VideoError::NotFound(_))));
    }

    #[test]
    fn aspect_ratio_falls_back_when_size_is_unknown() {
        let unknown = VideoInfo {
            width: 0,
            height: 0,
            duration_seconds: 0.0,
            frame_rate: 0.0,
            decoder: "software",
        };
        assert!((unknown.aspect_ratio() - 16.0 / 9.0).abs() < 1.0e-6);

        let hd = VideoInfo {
            width: 1920,
            height: 1080,
            duration_seconds: 12.0,
            frame_rate: 30.0,
            decoder: "GPU",
        };
        assert!((hd.aspect_ratio() - 16.0 / 9.0).abs() < 1.0e-6);
    }

    #[test]
    fn backend_name_matches_availability() {
        assert_eq!(backend_available(), backend_name() != "unavailable");
    }
}
