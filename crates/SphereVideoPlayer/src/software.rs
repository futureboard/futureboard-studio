//! Software decode: any container / codec `mkff-codecs` opens, in pure Rust.

use std::path::Path;
use std::sync::Arc;

use mkff_codecs::MediaFile;

use crate::{VideoError, VideoFrame, VideoInfo};

pub(crate) struct SoftwareDecoder {
    media: MediaFile,
    info: VideoInfo,
    frame_count: u64,
    /// The last frame handed out, by index: positions inside the same frame
    /// return the same `Arc` so the preview does not republish it.
    current: Option<(u64, Arc<VideoFrame>)>,
}

impl SoftwareDecoder {
    pub(crate) fn open(path: &Path) -> Result<Self, VideoError> {
        let media = MediaFile::open(path).map_err(|e| VideoError::Backend(e.to_string()))?;
        let video = media
            .info()
            .video
            .clone()
            .ok_or(VideoError::NoVideoStream)?;
        let (num, den) = video.frame_rate;
        let frame_rate = if num > 0 && den > 0 {
            num as f64 / den as f64
        } else {
            0.0
        };
        let frame_count = media.frame_count();
        if frame_rate <= 0.0 || frame_count == 0 {
            return Err(VideoError::NoVideoStream);
        }
        let info = VideoInfo {
            width: video.width,
            height: video.height,
            duration_seconds: media.info().duration_seconds.max(0.0),
            frame_rate,
            decoder: "software",
        };
        Ok(Self {
            media,
            info,
            frame_count,
            current: None,
        })
    }

    pub(crate) fn info(&self) -> VideoInfo {
        self.info
    }

    pub(crate) fn frame_at(&mut self, seconds: f64) -> Result<Arc<VideoFrame>, VideoError> {
        if self.info.duration_seconds > 0.0 && seconds >= self.info.duration_seconds {
            return Err(VideoError::EndOfStream);
        }
        let index = ((seconds * self.info.frame_rate) + 1.0e-6).floor().max(0.0) as u64;
        let index = index.min(self.frame_count - 1);
        if let Some((current, frame)) = &self.current
            && *current == index
        {
            return Ok(Arc::clone(frame));
        }

        let decoded = self
            .media
            .video_frame(index)
            .map_err(|e| VideoError::Backend(e.to_string()))?;
        let (width, height) = (decoded.width(), decoded.height());
        let mut bgra = decoded.to_rgba8();
        let expected = width as usize * height as usize * 4;
        if width == 0 || height == 0 || bgra.len() < expected {
            return Err(VideoError::Backend(format!(
                "decoder returned a {width}x{height} frame with {} bytes",
                bgra.len()
            )));
        }
        bgra.truncate(expected);
        rgba_to_bgra_in_place(&mut bgra);

        let frame = Arc::new(VideoFrame {
            width,
            height,
            timestamp_seconds: index as f64 / self.info.frame_rate,
            bgra,
        });
        self.current = Some((index, Arc::clone(&frame)));
        Ok(frame)
    }
}

/// RGBA8 → BGRA8 (swap the red and blue bytes of every pixel).
fn rgba_to_bgra_in_place(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_only() {
        let mut px = vec![1, 2, 3, 4, 10, 20, 30, 40];
        rgba_to_bgra_in_place(&mut px);
        assert_eq!(px, vec![3, 2, 1, 4, 30, 20, 10, 40]);
    }
}
