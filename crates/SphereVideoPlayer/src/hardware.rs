//! GPU decode: MP4 / MOV samples through an `mkff-mediakit` accelerant.
//!
//! MKFF's MP4 demuxer (`mkff_codecs::filmcraft::isobmff`) gives the video
//! track's sample table; samples are fed in decode order to the accelerant's
//! decoder (D3D11 / DXVA through a Media Foundation decoder MFT on Windows),
//! which hands back NV12 / P010 planes in presentation order. Only the frame
//! that is actually shown is converted to BGRA.
//!
//! H.264 and HEVC only. Anything else — another codec, another container, a
//! machine with no accelerant for the stream — fails [`HardwareDecoder::open`]
//! and the caller uses the software path.

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use mkff_codecs::filmcraft::isobmff::{self, CodecConfig, ColorInfo, Mp4File, TrackKind};
use mkff_mediakit::addon::{VideoDecoderPlugin, VideoFrame as GpuFrame};
use mkff_mediakit::{AccelPolicy, CodecFamily, MediaFormat, MediaRoster, PixelFormat};
use rayon::prelude::*;

use crate::{VideoError, VideoFrame, VideoInfo};

/// Samples fed past the target without the decoder producing a picture for
/// it before the request is treated as a decoder failure.
const MAX_OVERRUN_SAMPLES: usize = 64;

fn backend(what: &str, e: impl std::fmt::Display) -> VideoError {
    VideoError::Backend(format!("{what}: {e}"))
}

pub(crate) struct HardwareDecoder {
    file: File,
    mp4: Mp4File,
    track: usize,
    decoder: Box<dyn VideoDecoderPlugin>,
    info: VideoInfo,
    matrix: YuvMatrix,
    /// Decode-order index of the next sample to feed.
    next_decode: usize,
    /// Every sample has been fed and the decoder flushed.
    at_end: bool,
    /// Decoded pictures not shown yet, ascending pts (µs). At most one of them
    /// is before the most recent target.
    pending: VecDeque<GpuFrame>,
    /// Pictures at or before this pts (µs) were dropped since the last reset.
    floor_pts: Option<i64>,
    current: Option<(i64, Arc<VideoFrame>)>,
}

impl HardwareDecoder {
    pub(crate) fn open(path: &Path) -> Result<Self, VideoError> {
        let file = File::open(path).map_err(|_| VideoError::NotFound(path.to_path_buf()))?;
        let mp4 = isobmff::open(&file).map_err(|e| backend("not an MP4 / MOV file", e))?;
        let track = mp4
            .track_of_kind(TrackKind::Video)
            .ok_or(VideoError::NoVideoStream)?;
        let t = &mp4.tracks[track];
        if t.samples.is_empty() || t.timescale == 0 {
            return Err(VideoError::NoVideoStream);
        }
        let (codec, config) = match t.codec() {
            Some(CodecConfig::Avc(c)) => (CodecFamily::H264, c.to_bytes()),
            Some(CodecConfig::Hevc(c)) => (CodecFamily::Hevc, c.to_bytes()),
            Some(other) => {
                return Err(VideoError::Backend(format!(
                    "{} has no GPU decode path",
                    other.name()
                )));
            }
            None => return Err(VideoError::NoVideoStream),
        };
        let params = t.video().cloned().unwrap_or_default();
        let (width, height) = match (u32::from(params.width), u32::from(params.height)) {
            (0, _) | (_, 0) => (t.width, t.height),
            size => size,
        };

        let format = MediaFormat::encoded(codec, config, width, height);
        let (_flavor, decoder) = MediaRoster::roster()
            .video_decoder(&format, AccelPolicy::HardwareOnly)
            .map_err(|e| backend("no GPU decoder", e))?;

        let duration_seconds = t.duration as f64 / f64::from(t.timescale);
        let frame_rate = if t.duration > 0 {
            t.samples.len() as f64 * f64::from(t.timescale) / t.duration as f64
        } else {
            0.0
        };
        let matrix = YuvMatrix::for_stream(params.color.as_ref(), height);
        Ok(Self {
            file,
            mp4,
            track,
            decoder,
            info: VideoInfo {
                width,
                height,
                duration_seconds,
                frame_rate,
                decoder: "GPU",
            },
            matrix,
            next_decode: 0,
            at_end: false,
            pending: VecDeque::new(),
            floor_pts: None,
            current: None,
        })
    }

    pub(crate) fn info(&self) -> VideoInfo {
        self.info
    }

    fn pts_us(&self, media_pts: i64) -> i64 {
        let ts = i128::from(self.mp4.tracks[self.track].timescale.max(1));
        (i128::from(media_pts) * 1_000_000 / ts) as i64
    }

    fn reset_to(&mut self, sync_index: usize) {
        self.decoder.reset();
        self.pending.clear();
        self.next_decode = sync_index;
        self.at_end = false;
        self.floor_pts = None;
    }

    /// Feeds the next sample (or flushes at the end) and queues its pictures.
    fn feed_one(&mut self) -> Result<(), VideoError> {
        let t = &self.mp4.tracks[self.track];
        let pictures = if self.next_decode >= t.samples.len() {
            self.at_end = true;
            self.decoder.flush().map_err(|e| backend("GPU decode", e))?
        } else {
            let index = self.next_decode;
            let pts = self.pts_us(t.presentation_pts(index).unwrap_or(0));
            let data = self
                .mp4
                .read_sample(&self.file, self.track, index)
                .map_err(|e| backend("read sample", e))?;
            self.next_decode += 1;
            self.decoder
                .decode(&data, pts)
                .map_err(|e| backend("GPU decode", e))?
        };
        for picture in pictures {
            if self.floor_pts.is_some_and(|floor| picture.pts <= floor) {
                continue;
            }
            let at = self.pending.partition_point(|p| p.pts <= picture.pts);
            self.pending.insert(at, picture);
        }
        Ok(())
    }

    pub(crate) fn frame_at(&mut self, seconds: f64) -> Result<Arc<VideoFrame>, VideoError> {
        if self.info.duration_seconds > 0.0 && seconds >= self.info.duration_seconds {
            return Err(VideoError::EndOfStream);
        }
        let t = &self.mp4.tracks[self.track];
        let media_time = (seconds * f64::from(t.timescale)).floor() as i64;
        let index = t
            .sample_at_presentation_time(media_time)
            .ok_or(VideoError::NoVideoStream)?;
        let sync = t.sync_sample_before(index);
        let target = self.pts_us(t.presentation_pts(index).unwrap_or(0));

        if let Some((pts, frame)) = &self.current {
            if *pts == target {
                return Ok(Arc::clone(frame));
            }
        }

        // Read forward when the target is ahead and no keyframe lies between
        // the reader and the target; otherwise restart at the keyframe.
        let ahead = self.floor_pts.is_none_or(|floor| target > floor);
        if !ahead || sync > self.next_decode {
            self.reset_to(sync);
        }

        let mut overrun = 0usize;
        loop {
            // Keep only the newest picture before the target.
            while self.pending.len() >= 2 && self.pending[1].pts <= target {
                self.pending.pop_front();
            }
            let reached = self.pending.iter().any(|p| p.pts >= target);
            if reached || (self.at_end && !self.pending.is_empty()) {
                break;
            }
            if self.at_end {
                return Err(VideoError::EndOfStream);
            }
            if self.next_decode > index {
                overrun += 1;
                if overrun > MAX_OVERRUN_SAMPLES {
                    return Err(VideoError::Backend(format!(
                        "GPU decoder produced no picture for {seconds:.3}s"
                    )));
                }
            }
            self.feed_one()?;
        }

        // The last picture at or before the target, else the first after it.
        let chosen = match self.pending.iter().rposition(|p| p.pts <= target) {
            Some(i) => i,
            None => 0,
        };
        let picture = self
            .pending
            .drain(..=chosen)
            .last()
            .ok_or(VideoError::EndOfStream)?;
        self.floor_pts = Some(picture.pts);

        let frame = Arc::new(self.to_bgra(&picture)?);
        self.current = Some((target, Arc::clone(&frame)));
        Ok(frame)
    }

    fn to_bgra(&self, picture: &GpuFrame) -> Result<VideoFrame, VideoError> {
        // Decoders allocate in macroblock units (1088 lines for 1080p); show
        // only the stream's own size.
        let width = clamp_dim(picture.width, self.info.width);
        let height = clamp_dim(picture.height, self.info.height);
        let bytes = match picture.format {
            PixelFormat::Nv12 => 1,
            PixelFormat::P010 => 2,
            other => {
                return Err(VideoError::Backend(format!(
                    "GPU decoder returned unsupported {other:?} pictures"
                )));
            }
        };
        if picture.planes.len() < 2 || width == 0 || height == 0 {
            return Err(VideoError::Backend("GPU decoder returned no planes".into()));
        }
        let y_stride = stride(picture, 0, bytes);
        let uv_stride = stride(picture, 1, bytes);
        let planes = SemiPlanar {
            y: &picture.planes[0],
            uv: &picture.planes[1],
            y_stride,
            uv_stride,
            bytes,
        };
        if !planes.covers(width, height) {
            return Err(VideoError::Backend(format!(
                "GPU picture planes too small for {width}x{height}"
            )));
        }
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        semi_planar_to_bgra(&planes, width as usize, self.matrix, &mut bgra);
        Ok(VideoFrame {
            width,
            height,
            timestamp_seconds: picture.pts as f64 / 1_000_000.0,
            bgra,
        })
    }
}

fn clamp_dim(decoded: u32, stream: u32) -> u32 {
    if stream > 0 {
        decoded.min(stream)
    } else {
        decoded
    }
}

fn stride(picture: &GpuFrame, plane: usize, bytes: usize) -> usize {
    match picture.strides.get(plane) {
        Some(&s) if s > 0 => s,
        _ => picture.width as usize * bytes,
    }
}

/// NV12 (`bytes == 1`) or P010 (`bytes == 2`, little-endian, MSB-aligned).
struct SemiPlanar<'a> {
    y: &'a [u8],
    uv: &'a [u8],
    y_stride: usize,
    uv_stride: usize,
    bytes: usize,
}

impl SemiPlanar<'_> {
    fn covers(&self, width: u32, height: u32) -> bool {
        let (w, h) = (width as usize, height as usize);
        let chroma_rows = h.div_ceil(2);
        let chroma_row = w.div_ceil(2) * 2 * self.bytes;
        self.y_stride >= w * self.bytes
            && self.uv_stride >= chroma_row
            && self.y.len() >= (h - 1) * self.y_stride + w * self.bytes
            && self.uv.len() >= (chroma_rows - 1) * self.uv_stride + chroma_row
    }

    /// 8-bit sample at byte offset `at` (P010: the high byte of the word).
    #[inline]
    fn sample(plane: &[u8], at: usize, bytes: usize) -> i32 {
        if bytes == 2 {
            i32::from(plane[at + 1])
        } else {
            i32::from(plane[at])
        }
    }
}

/// Y'CbCr → R'G'B' coefficients, 16.16 fixed point, with the range offsets.
#[derive(Clone, Copy, Debug, PartialEq)]
struct YuvMatrix {
    y_offset: i32,
    y_scale: i32,
    cr_r: i32,
    cb_g: i32,
    cr_g: i32,
    cb_b: i32,
}

impl YuvMatrix {
    fn new(kr: f64, kb: f64, full_range: bool) -> Self {
        let kg = 1.0 - kr - kb;
        let (y_scale, c_scale, y_offset) = if full_range {
            (1.0, 1.0, 0)
        } else {
            (255.0 / 219.0, 255.0 / 224.0, 16)
        };
        let fx = |v: f64| (v * 65536.0).round() as i32;
        Self {
            y_offset,
            y_scale: fx(y_scale),
            cr_r: fx(2.0 * (1.0 - kr) * c_scale),
            cb_g: fx(2.0 * kb * (1.0 - kb) / kg * c_scale),
            cr_g: fx(2.0 * kr * (1.0 - kr) / kg * c_scale),
            cb_b: fx(2.0 * (1.0 - kb) * c_scale),
        }
    }

    /// The matrix the stream signals (`colr`), else BT.709 for HD and BT.601
    /// for SD — what players assume for untagged video.
    fn for_stream(color: Option<&ColorInfo>, height: u32) -> Self {
        let (matrix, full_range) = match color {
            Some(ColorInfo::Nclx {
                matrix, full_range, ..
            }) => (Some(*matrix), *full_range),
            Some(ColorInfo::Nclc { matrix, .. }) => (Some(*matrix), false),
            _ => (None, false),
        };
        let (kr, kb) = match matrix {
            Some(1) => (0.2126, 0.0722),
            Some(5 | 6) => (0.299, 0.114),
            Some(9 | 10) => (0.2627, 0.0593),
            _ if height >= 720 => (0.2126, 0.0722),
            _ => (0.299, 0.114),
        };
        Self::new(kr, kb, full_range)
    }

    #[inline]
    fn to_bgra(self, y: i32, cb: i32, cr: i32) -> [u8; 4] {
        let y = (y - self.y_offset) * self.y_scale;
        let (cb, cr) = (cb - 128, cr - 128);
        let clamp = |v: i32| ((v + 32768) >> 16).clamp(0, 255) as u8;
        let r = clamp(y + self.cr_r * cr);
        let g = clamp(y - self.cb_g * cb - self.cr_g * cr);
        let b = clamp(y + self.cb_b * cb);
        [b, g, r, 255]
    }
}

fn semi_planar_to_bgra(src: &SemiPlanar<'_>, width: usize, matrix: YuvMatrix, out: &mut [u8]) {
    let bytes = src.bytes;
    out.par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(row, line)| {
            let y_row = row * src.y_stride;
            let uv_row = (row / 2) * src.uv_stride;
            for (x, px) in line.chunks_exact_mut(4).enumerate() {
                let y = SemiPlanar::sample(src.y, y_row + x * bytes, bytes);
                let c = uv_row + (x / 2) * 2 * bytes;
                let cb = SemiPlanar::sample(src.uv, c, bytes);
                let cr = SemiPlanar::sample(src.uv, c + bytes, bytes);
                px.copy_from_slice(&matrix.to_bgra(y, cb, cr));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(y: u8, cb: u8, cr: u8, matrix: YuvMatrix) -> [u8; 4] {
        // 2x2 NV12 picture with one chroma pair.
        let luma = [y; 4];
        let chroma = [cb, cr];
        let planes = SemiPlanar {
            y: &luma,
            uv: &chroma,
            y_stride: 2,
            uv_stride: 2,
            bytes: 1,
        };
        assert!(planes.covers(2, 2));
        let mut out = [0u8; 16];
        semi_planar_to_bgra(&planes, 2, matrix, &mut out);
        [out[0], out[1], out[2], out[3]]
    }

    #[test]
    fn limited_range_black_white_and_grey() {
        let m = YuvMatrix::for_stream(None, 1080);
        assert_eq!(convert(16, 128, 128, m), [0, 0, 0, 255]);
        assert_eq!(convert(235, 128, 128, m), [255, 255, 255, 255]);
        let grey = convert(126, 128, 128, m);
        assert_eq!(grey[0], grey[1]);
        assert_eq!(grey[1], grey[2]);
    }

    #[test]
    fn bt709_red_is_red() {
        // BT.709 limited-range 100% red: Y'=63, Cb=102, Cr=240.
        let px = convert(63, 102, 240, YuvMatrix::for_stream(None, 1080));
        assert!(px[2] >= 250, "red {px:?}");
        assert!(px[1] <= 5 && px[0] <= 5, "green/blue {px:?}");
    }

    #[test]
    fn tagged_matrix_overrides_the_size_heuristic() {
        let bt601 = ColorInfo::Nclx {
            primaries: 6,
            transfer: 6,
            matrix: 6,
            full_range: false,
        };
        assert_eq!(
            YuvMatrix::for_stream(Some(&bt601), 1080),
            YuvMatrix::new(0.299, 0.114, false)
        );
        assert_eq!(
            YuvMatrix::for_stream(None, 480),
            YuvMatrix::new(0.299, 0.114, false)
        );
    }

    #[test]
    fn p010_uses_the_high_byte() {
        // 10-bit white (940 << 6) little-endian.
        let w = (940u16 << 6).to_le_bytes();
        let n = (512u16 << 6).to_le_bytes();
        let luma: Vec<u8> = w.iter().copied().cycle().take(8).collect();
        let chroma = [n[0], n[1], n[0], n[1]];
        let planes = SemiPlanar {
            y: &luma,
            uv: &chroma,
            y_stride: 4,
            uv_stride: 4,
            bytes: 2,
        };
        assert!(planes.covers(2, 2));
        let mut out = [0u8; 16];
        semi_planar_to_bgra(&planes, 2, YuvMatrix::for_stream(None, 2160), &mut out);
        assert!(out[0] >= 250 && out[1] >= 250 && out[2] >= 250, "{out:?}");
    }

    /// When this machine has a GPU decoder for H.264, its picture must match
    /// the software decode of the same frame (same matrix, same crop).
    #[test]
    fn gpu_and_software_agree_on_a_real_clip() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../mkff/testdata/tiny_baseline_64x64.mp4");
        let mut gpu = match HardwareDecoder::open(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: {e}");
                return;
            }
        };
        let mut sw = crate::software::SoftwareDecoder::open(&path).unwrap();
        let info = gpu.info();
        assert_eq!(info.decoder, "GPU");
        let samples = gpu.mp4.tracks[gpu.track].samples.len();
        eprintln!("{info:?}, {samples} samples");
        let last = info.duration_seconds * 0.99;
        for t in [0.0, last, last * 0.5, 0.0] {
            let a = gpu
                .frame_at(t)
                .unwrap_or_else(|e| panic!("GPU frame at {t}s: {e:?}"));
            let b = sw.frame_at(t).unwrap();
            assert_eq!((a.width, a.height), (b.width, b.height));
            let diff: u64 = a
                .bgra
                .iter()
                .zip(&b.bgra)
                .map(|(x, y)| u64::from(x.abs_diff(*y)))
                .sum();
            let mean = diff as f64 / a.bgra.len() as f64;
            assert!(
                mean < 4.0,
                "GPU vs software mean abs diff {mean:.2} at {t}s"
            );
        }
    }

    const W: u32 = 256;
    const H: u32 = 96;
    const FPS: u32 = 30;
    const FRAMES: u64 = 90;
    const BAR_STEP: u64 = 2;

    /// Frame `i`: dark grey with a white 6-pixel bar at x = 2·i (never wraps:
    /// 2·89 + 6 < 256), so every frame's index can be read back from the
    /// picture.
    fn moving_bar(i: u64) -> Vec<u8> {
        let x0 = (i * BAR_STEP) as usize;
        let mut rgba = vec![0u8; (W * H * 4) as usize];
        for (p, px) in rgba.chunks_exact_mut(4).enumerate() {
            let x = p % W as usize;
            let v = if (x0..x0 + 6).contains(&x) { 235 } else { 40 };
            px.copy_from_slice(&[v, v, v, 255]);
        }
        rgba
    }

    /// The bar's left edge in a decoded BGRA picture → the frame index.
    fn frame_index_of(frame: &VideoFrame) -> u64 {
        let w = frame.width as usize;
        let row = &frame.bgra[(frame.height as usize / 2) * w * 4..][..w * 4];
        let x = row
            .chunks_exact(4)
            .position(|p| p[1] > 140)
            .expect("no bar");
        x as u64 / BAR_STEP
    }

    /// 3 s of H.264 High, keyframe every 15 frames, 2 B-frames: reorder, GOPs
    /// and seeks all matter.
    fn encode_clip(dir: &std::path::Path) -> std::path::PathBuf {
        use mkff_codecs::encode::{
            Container, H264Preset, MediaWriter, VideoEncodeCodec, VideoEncoder, VideoEncoderConfig,
        };
        let mut cfg = VideoEncoderConfig::h264(W, H, FPS, 1);
        if let VideoEncodeCodec::H264(s) = &mut cfg.codec {
            s.keyint = Some(15);
            s.bframes = 2;
            s.preset = H264Preset::Balanced;
        }
        let path = dir.join("fb-video-moving-bar.mp4");
        let mut enc = VideoEncoder::new(cfg).unwrap();
        let mut out = MediaWriter::create(&path, Container::Mp4, false).unwrap();
        let track = out.add_video_track(&enc).unwrap();
        for i in 0..FRAMES {
            for p in enc.encode_rgba(&moving_bar(i), i).unwrap() {
                out.write(track, &p).unwrap();
            }
        }
        for p in enc.flush().unwrap() {
            out.write(track, &p).unwrap();
        }
        out.finish().unwrap();
        path
    }

    /// Scrub order a reference player sees: forward steps, a jump across
    /// GOPs, backwards seeks, a repeat, the last frame.
    const SCRUB: &[u64] = &[
        0, 1, 2, 3, 7, 14, 15, 16, 40, 39, 38, 10, 10, 11, 60, 89, 5, 0,
    ];

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fb-video-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn software_lands_on_the_exact_frame_while_scrubbing() {
        let dir = temp_dir("sw");
        let path = encode_clip(&dir);
        let mut sw = crate::software::SoftwareDecoder::open(&path).unwrap();
        for &i in SCRUB {
            let t = (i as f64 + 0.5) / f64::from(FPS);
            let frame = sw.frame_at(t).unwrap();
            assert_eq!(frame_index_of(&frame), i, "software at {t:.3}s");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpu_lands_on_the_exact_frame_while_scrubbing() {
        let dir = temp_dir("gpu");
        let path = encode_clip(&dir);
        let mut gpu = match HardwareDecoder::open(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: {e}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        assert_eq!(gpu.info().frame_rate.round() as u32, FPS);
        let mut sw = crate::software::SoftwareDecoder::open(&path).unwrap();
        for &i in SCRUB {
            let t = (i as f64 + 0.5) / f64::from(FPS);
            let a = gpu
                .frame_at(t)
                .unwrap_or_else(|e| panic!("GPU at {t:.3}s: {e:?}"));
            assert_eq!(frame_index_of(&a), i, "GPU at {t:.3}s");
            let b = sw.frame_at(t).unwrap();
            let diff: u64 = a
                .bgra
                .iter()
                .zip(&b.bgra)
                .map(|(x, y)| u64::from(x.abs_diff(*y)))
                .sum();
            let mean = diff as f64 / a.bgra.len() as f64;
            assert!(
                mean < 4.0,
                "GPU vs software mean abs diff {mean:.2} at frame {i}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_planes_are_rejected() {
        let planes = SemiPlanar {
            y: &[0; 3],
            uv: &[0; 2],
            y_stride: 2,
            uv_stride: 2,
            bytes: 1,
        };
        assert!(!planes.covers(2, 2));
    }
}
