//! Helpers for uploading MKFF-decoded NV12 frames into wgpu textures.
//!
//! Hardware zero-copy into wgpu (dma-buf / D3D11 shared handle / IOSurface) is
//! not stable across wgpu backends yet. This crate uses the portable path:
//! [`mkff::VideoFrame::map_cpu_planes`] → R8 + RG8 textures → YUV→RGB shader.
//!
//! Input: raw Annex-B (`.hevc` / `.h264`) or progressive MP4/MOV (`.mp4` /
//! `.mov` / `ftyp`) via [`mkff::Mp4Demux`] and libmkff's hardware decoders.
//! Everything else — Matroska / WebM, AVI, MXF, MPEG-TS / PS, Ogg, Y4M, and
//! MP4s with VP9 / AV1 / ProRes / … — plus any clip libmkff can't decode on
//! this machine, goes through [`mkff_codecs`] (pure-Rust software decode).

use mkff::{
    Context, CpuPlanes, Mp4Demux, PixelFormat, ReadAuOutcome, ReceiveOutcome, VideoBackend,
    VideoCodec, VideoDecoder,
};

#[derive(Debug)]
pub enum UploadError {
    UnsupportedFormat(PixelFormat),
    MissingPlane,
    ZeroSize,
    InvalidStride { plane: usize, stride: u32, min: u32 },
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::UnsupportedFormat(fmt) => write!(f, "unsupported pixel format: {fmt:?}"),
            UploadError::MissingPlane => write!(f, "NV12 frame missing Y or UV plane"),
            UploadError::ZeroSize => write!(f, "frame has zero width or height"),
            UploadError::InvalidStride { plane, stride, min } => {
                write!(f, "plane {plane} stride {stride} < min row bytes {min}")
            }
        }
    }
}

impl std::error::Error for UploadError {}

/// Packed NV12 plane bytes copied out of a mapped [`CpuPlanes`] view.
///
/// Planes are tightly packed (`y_stride == width`, `uv_stride == width`) after
/// [`Nv12Host::from_cpu_planes`] copies out of possibly padded source strides.
pub struct Nv12Host {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: u32,
    pub uv: Vec<u8>,
    pub uv_stride: u32,
}

impl Nv12Host {
    pub fn from_cpu_planes(planes: &CpuPlanes<'_>) -> Result<Self, UploadError> {
        if planes.format() != PixelFormat::MKFF_PIXEL_FORMAT_NV12 {
            return Err(UploadError::UnsupportedFormat(planes.format()));
        }
        let width = planes.width();
        let height = planes.height();
        if width == 0 || height == 0 {
            return Err(UploadError::ZeroSize);
        }

        let (y_ptr, y_stride, y_lines) = planes.plane(0).ok_or(UploadError::MissingPlane)?;
        let (uv_ptr, uv_stride, uv_lines) = planes.plane(1).ok_or(UploadError::MissingPlane)?;

        // NV12: Y row = width bytes; UV row = width bytes (interleaved UV pairs).
        let row_bytes = width;
        if y_stride < row_bytes {
            return Err(UploadError::InvalidStride {
                plane: 0,
                stride: y_stride,
                min: row_bytes,
            });
        }
        if uv_stride < row_bytes {
            return Err(UploadError::InvalidStride {
                plane: 1,
                stride: uv_stride,
                min: row_bytes,
            });
        }

        let y = copy_plane(y_ptr, y_stride, y_lines, row_bytes);
        let uv = copy_plane(uv_ptr, uv_stride, uv_lines, row_bytes);

        Ok(Nv12Host {
            width,
            height,
            y,
            y_stride: row_bytes,
            uv,
            uv_stride: row_bytes,
        })
    }

    /// Converts a software-decoded [`mkff_codecs::Frame`] (any layout) to NV12.
    pub fn from_sw_frame(frame: &mkff_codecs::Frame) -> Self {
        let (width, height) = (frame.width(), frame.height());
        let mut y = frame.to_nv12();
        let uv = y.split_off((width * height) as usize);
        Nv12Host {
            width,
            height,
            y,
            y_stride: width,
            uv,
            uv_stride: width.div_ceil(2) * 2,
        }
    }

    /// BT.709 limited-range YUV → packed RGBA8 (for egui / CPU display).
    pub fn to_rgba8(&self) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let y_stride = self.y_stride as usize;
        let uv_stride = self.uv_stride as usize;
        let mut rgba = vec![0u8; w * h * 4];
        for row in 0..h {
            let y_row = row * y_stride;
            let uv_row = (row / 2) * uv_stride;
            for col in 0..w {
                // Limited-range BT.709 (studio swing).
                let y = (self.y[y_row + col] as f32 - 16.0) * (255.0 / 219.0);
                let uv = uv_row + (col & !1);
                let u = (self.uv[uv] as f32 - 128.0) * (255.0 / 224.0);
                let v = (self.uv[uv + 1] as f32 - 128.0) * (255.0 / 224.0);
                let r = y + 1.5748 * v;
                let g = y - 0.1873 * u - 0.4681 * v;
                let b = y + 1.8556 * u;
                let i = (row * w + col) * 4;
                rgba[i] = r.round().clamp(0.0, 255.0) as u8;
                rgba[i + 1] = g.round().clamp(0.0, 255.0) as u8;
                rgba[i + 2] = b.round().clamp(0.0, 255.0) as u8;
                rgba[i + 3] = 255;
            }
        }
        rgba
    }
}

fn copy_plane(ptr: *const u8, stride: u32, lines: u32, row_bytes: u32) -> Vec<u8> {
    let mut out = vec![0u8; (row_bytes * lines) as usize];
    for row in 0..lines {
        let src = unsafe { ptr.add((row * stride) as usize) };
        let dst = (row * row_bytes) as usize;
        unsafe {
            std::ptr::copy_nonoverlapping(src, out[dst..].as_mut_ptr(), row_bytes as usize);
        }
    }
    out
}

/// Creates (or recreates) R8 (Y) + RG8 (UV) textures and uploads packed NV12.
pub fn upload_nv12(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    host: &Nv12Host,
    y_tex: &mut Option<wgpu::Texture>,
    uv_tex: &mut Option<wgpu::Texture>,
) {
    let y_size = wgpu::Extent3d {
        width: host.width,
        height: host.height,
        depth_or_array_layers: 1,
    };
    let uv_size = wgpu::Extent3d {
        width: host.width / 2,
        height: host.height / 2,
        depth_or_array_layers: 1,
    };

    let need_new = match (&*y_tex, &*uv_tex) {
        (Some(y), Some(uv)) => y.size() != y_size || uv.size() != uv_size,
        _ => true,
    };

    if need_new {
        *y_tex = Some(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mkff-nv12-y"),
            size: y_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        }));
        *uv_tex = Some(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mkff-nv12-uv"),
            size: uv_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        }));
    }

    let y = y_tex.as_ref().unwrap();
    let uv = uv_tex.as_ref().unwrap();

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: y,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &host.y,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(host.y_stride),
            rows_per_image: Some(host.height),
        },
        y_size,
    );

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: uv,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &host.uv,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(host.uv_stride),
            rows_per_image: Some(host.height / 2),
        },
        uv_size,
    );
}

/// Byte ranges of HEVC access units in an Annex-B bitstream (VCL-boundary split).
pub fn split_hevc_access_units(data: &[u8]) -> Vec<std::ops::Range<usize>> {
    split_annex_b_access_units(data, |nal| {
        if nal.len() < 2 {
            return false;
        }
        let nal_unit_type = (nal[0] >> 1) & 0x3F;
        nal_unit_type <= 9 || (16..=21).contains(&nal_unit_type)
    })
}

/// Byte ranges of H.264 access units in an Annex-B bitstream (VCL-boundary split).
pub fn split_h264_access_units(data: &[u8]) -> Vec<std::ops::Range<usize>> {
    split_annex_b_access_units(data, |nal| {
        if nal.is_empty() {
            return false;
        }
        let nal_unit_type = nal[0] & 0x1F;
        (1..=5).contains(&nal_unit_type)
    })
}

fn split_annex_b_access_units(
    data: &[u8],
    is_vcl: impl Fn(&[u8]) -> bool,
) -> Vec<std::ops::Range<usize>> {
    let Some(first) = find_start_code(data, 0) else {
        return Vec::new();
    };

    let mut au_start = first;
    let mut seen_vcl = false;
    let mut ranges = Vec::new();
    let mut pos = first + 3;

    while pos <= data.len() {
        let next = find_start_code(data, pos);
        let nal_start = pos;
        let nal_end = next.unwrap_or(data.len());

        if nal_end > nal_start && is_vcl(&data[nal_start..nal_end]) {
            if seen_vcl {
                let end = nal_start.saturating_sub(3);
                if end > au_start {
                    ranges.push(au_start..end);
                }
                au_start = end;
            }
            seen_vcl = true;
        }

        match next {
            Some(n) => pos = n + 3,
            None => break,
        }
    }

    if au_start < data.len() {
        ranges.push(au_start..data.len());
    }
    ranges
}

fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    if data.len() < 3 || from >= data.len() {
        return None;
    }
    let end = data.len().saturating_sub(2);
    for i in from..end {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        }
    }
    None
}

fn annex_b_au_is_keyframe(au: &[u8], codec: VideoCodec) -> bool {
    let mut pos = 0;
    while let Some(sc) = find_start_code(au, pos) {
        let nal = sc + 3;
        if nal >= au.len() {
            break;
        }
        match codec {
            VideoCodec::MKFF_VIDEO_CODEC_H264 => {
                let nal_type = au[nal] & 0x1F;
                if nal_type == 5 {
                    return true;
                }
            }
            VideoCodec::MKFF_VIDEO_CODEC_HEVC => {
                if nal + 1 < au.len() {
                    let nal_type = (au[nal] >> 1) & 0x3F;
                    if (16..=21).contains(&nal_type) {
                        return true;
                    }
                }
            }
            _ => {}
        }
        pos = nal;
    }
    false
}

/// On-demand video source: demux/index quickly, decode current frame (+ optional
/// next) without retaining a full-clip RGBA/NV12 cache.
pub struct VideoSource {
    kind: SourceKind,
    /// Where the clip came from, for reopening it in software when libmkff fails mid-stream.
    path: Option<std::path::PathBuf>,
    width: u32,
    height: u32,
    frame_count: usize,
    /// Small decoded cache: (frame_index, pixels).
    cache: Vec<(usize, Nv12Host)>,
    cache_cap: usize,
}

enum SourceKind {
    Mp4(Mp4Source),
    AnnexB(AnnexBSource),
    /// Any container / codec, decoded in software by mkff-codecs.
    Software(mkff_codecs::MediaFile),
    /// Raw Annex-B decoded in software by mkff-codecs (no usable libmkff decoder).
    SoftwareAnnexB(SwAnnexBSource),
}

struct SwAnnexBSource {
    decoder: mkff_codecs::VideoDecoder,
    bytes: Vec<u8>,
    aus: Vec<std::ops::Range<usize>>,
    keyframes: Vec<usize>,
    submit_pos: usize,
    pending: Vec<(usize, Nv12Host)>,
}

struct Mp4Source {
    // Drop order is declaration order: decoder must be destroyed before ctx.
    decoder: VideoDecoder<'static>,
    /// Owns context; decoder lifetime is tied via raw pointer (see `make_decoder`).
    ctx: Box<Context>,
    demux: Mp4Demux,
    codec: VideoCodec,
    backend: VideoBackend,
    sync_samples: Vec<u32>,
    /// Next sample index the demuxer will return from `read_access_unit`.
    demux_pos: u32,
    /// True after `flush()` — decoder must be recreated before further submits.
    exhausted: bool,
    /// Frames produced by the decoder waiting to be matched to sample order.
    /// HW decoders may reorder; we key by sample_index stamped as PTS.
    pending: Vec<(u32, Nv12Host)>,
}

struct AnnexBSource {
    // Drop order is declaration order: decoder must be destroyed before ctx.
    decoder: VideoDecoder<'static>,
    ctx: Box<Context>,
    bytes: Vec<u8>,
    aus: Vec<std::ops::Range<usize>>,
    keyframes: Vec<usize>,
    codec: VideoCodec,
    backend: VideoBackend,
    /// Next AU index to submit.
    submit_pos: usize,
    pending: Vec<(usize, Nv12Host)>,
}

impl VideoSource {
    /// Opens any clip: libmkff (hardware first) for progressive MP4/MOV and
    /// Annex-B H.264 / HEVC, mkff-codecs (software) for every other container
    /// and codec, and as the fallback when libmkff can't open or decode it.
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let ext = path_ext(path);
        let native_ext = matches!(
            ext.as_str(),
            "mp4" | "m4v" | "mov" | "h264" | "264" | "avc" | "hevc" | "h265" | "265" | ""
        );
        if !native_ext {
            return Self::open_software(path);
        }
        let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
        let mut src = match Self::open_bytes(path, bytes) {
            Ok(src) => src,
            Err(native) => Self::open_software(path)
                .or_else(|sw| {
                    // raw Annex-B is not a container mkff-codecs' MediaFile opens
                    let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
                    Self::open_annex_b_software(bytes, annex_b_codec(&ext))
                        .map_err(|e| format!("{sw}; {e}"))
                })
                .map_err(|sw| format!("{native}; software: {sw}"))?,
        };
        src.path = Some(path.to_path_buf());
        Ok(src)
    }

    fn open_software(path: &std::path::Path) -> Result<Self, String> {
        let media = mkff_codecs::MediaFile::open(path).map_err(|e| e.to_string())?;
        let video = media.info().video.clone().ok_or("no video stream")?;
        let frame_count = media.frame_count() as usize;
        if frame_count == 0 {
            return Err("video stream has no frames".into());
        }
        Ok(VideoSource {
            kind: SourceKind::Software(media),
            path: Some(path.to_path_buf()),
            width: video.width,
            height: video.height,
            frame_count,
            cache: Vec::new(),
            cache_cap: 2,
        })
    }

    /// `codec`: H.264 or HEVC; `None` sniffs the first NAL unit header.
    fn open_annex_b_software(bytes: Vec<u8>, codec: Option<VideoCodec>) -> Result<Self, String> {
        let mkff_codec = codec
            .or_else(|| sniff_annex_b_codec(&bytes))
            .ok_or("not an Annex-B H.264 / HEVC stream")?;
        let (codec, aus) = match mkff_codec {
            VideoCodec::MKFF_VIDEO_CODEC_HEVC => (
                mkff_codecs::VideoCodec::Hevc,
                split_hevc_access_units(&bytes),
            ),
            _ => (
                mkff_codecs::VideoCodec::H264,
                split_h264_access_units(&bytes),
            ),
        };
        if aus.is_empty() {
            return Err("no access units found".into());
        }
        let keyframes: Vec<usize> = aus
            .iter()
            .enumerate()
            .filter_map(|(i, r)| annex_b_au_is_keyframe(&bytes[r.clone()], mkff_codec).then_some(i))
            .collect();
        let mut src = SwAnnexBSource {
            decoder: mkff_codecs::VideoDecoder::new(codec).map_err(|e| e.to_string())?,
            bytes,
            aus,
            keyframes: if keyframes.is_empty() {
                vec![0]
            } else {
                keyframes
            },
            submit_pos: 0,
            pending: Vec::new(),
        };
        let first = decode_sw_annex_b_frame(&mut src, 0)?;
        Ok(VideoSource {
            width: first.width,
            height: first.height,
            frame_count: src.aus.len(),
            kind: SourceKind::SoftwareAnnexB(src),
            path: None,
            cache: vec![(0, first)],
            cache_cap: 2,
        })
    }

    /// Swap a failing libmkff source for a software one (same clip).
    fn fall_back_to_software(&mut self) -> Result<(), String> {
        let sw = match (&self.kind, &self.path) {
            (SourceKind::AnnexB(ab), _) => {
                // a copy: the libmkff source stays intact if software can't take over either
                Self::open_annex_b_software(ab.bytes.clone(), Some(ab.codec))?
            }
            (SourceKind::Mp4(_), Some(path)) => Self::open_software(path)?,
            _ => return Err("no software fallback for this source".into()),
        };
        self.kind = sw.kind;
        self.frame_count = sw.frame_count;
        self.cache.clear();
        Ok(())
    }

    fn open_bytes(path: &std::path::Path, bytes: Vec<u8>) -> Result<Self, String> {
        let ext = path_ext(path);
        if looks_like_isom_container(&bytes) || matches!(ext.as_str(), "mp4" | "m4v" | "mov") {
            return Self::open_mp4(bytes);
        }
        match ext.as_str() {
            "h264" | "264" | "avc" => Self::open_annex_b(
                bytes,
                VideoCodec::MKFF_VIDEO_CODEC_H264,
                VideoBackend::MKFF_VIDEO_BACKEND_AUTO,
                split_h264_access_units,
            ),
            "hevc" | "h265" | "265" => Self::open_annex_b(
                bytes,
                VideoCodec::MKFF_VIDEO_CODEC_HEVC,
                VideoBackend::MKFF_VIDEO_BACKEND_SOFTWARE_ONLY,
                split_hevc_access_units,
            ),
            _ if looks_like_hevc_annex_b(&bytes) => Self::open_annex_b(
                bytes,
                VideoCodec::MKFF_VIDEO_CODEC_HEVC,
                VideoBackend::MKFF_VIDEO_BACKEND_SOFTWARE_ONLY,
                split_hevc_access_units,
            ),
            _ if looks_like_h264_annex_b(&bytes) => Self::open_annex_b(
                bytes,
                VideoCodec::MKFF_VIDEO_CODEC_H264,
                VideoBackend::MKFF_VIDEO_BACKEND_AUTO,
                split_h264_access_units,
            ),
            _ => Err(
                "unrecognized input — use Annex-B (.hevc / .h264) or progressive MP4/MOV (.mp4 / .mov)"
                    .into(),
            ),
        }
    }

    fn open_mp4(bytes: Vec<u8>) -> Result<Self, String> {
        let mut demux = Mp4Demux::open_memory(&bytes).map_err(|e| format!("mp4 demux open: {e}"))?;
        let track = demux.video_track().map_err(|e| format!("mp4 video track: {e}"))?;
        if track.sample_count == 0 {
            return Err("MP4 video track has no samples".into());
        }
        let codec = track.codec;
        if !matches!(
            codec,
            VideoCodec::MKFF_VIDEO_CODEC_H264 | VideoCodec::MKFF_VIDEO_CODEC_HEVC
        ) {
            return Err(format!("unsupported MP4 video codec: {codec:?}"));
        }

        // Index sync samples without keeping AU payloads.
        let mut sync_samples = Vec::new();
        loop {
            match demux.read_access_unit().map_err(|e| format!("index AU: {e}"))? {
                ReadAuOutcome::EndOfStream => break,
                ReadAuOutcome::Au(au) => {
                    if au.sync {
                        sync_samples.push(au.sample_index);
                    }
                }
            }
        }
        if sync_samples.is_empty() {
            sync_samples.push(0);
        }
        demux
            .seek_sample(0)
            .map_err(|e| format!("seek after index: {e}"))?;

        let backend = VideoBackend::MKFF_VIDEO_BACKEND_AUTO;
        let ctx = Box::new(Context::new().map_err(|e| format!("context: {e}"))?);
        let decoder = make_decoder(&ctx, codec, backend)?;

        let width = track.width;
        let height = track.height;
        let frame_count = track.sample_count as usize;

        Ok(VideoSource {
            kind: SourceKind::Mp4(Mp4Source {
                decoder,
                ctx,
                demux,
                codec,
                backend,
                sync_samples,
                demux_pos: 0,
                exhausted: false,
                pending: Vec::new(),
            }),
            path: None,
            width,
            height,
            frame_count,
            cache: Vec::new(),
            cache_cap: 2,
        })
    }

    fn open_annex_b(
        bytes: Vec<u8>,
        codec: VideoCodec,
        backend: VideoBackend,
        split: fn(&[u8]) -> Vec<std::ops::Range<usize>>,
    ) -> Result<Self, String> {
        let aus = split(&bytes);
        if aus.is_empty() {
            return Err("no access units found (need start codes 00 00 01 / 00 00 00 01)".into());
        }
        let keyframes: Vec<usize> = aus
            .iter()
            .enumerate()
            .filter_map(|(i, r)| annex_b_au_is_keyframe(&bytes[r.clone()], codec).then_some(i))
            .collect();
        let keyframes = if keyframes.is_empty() {
            vec![0]
        } else {
            keyframes
        };

        let ctx = Box::new(Context::new().map_err(|e| format!("context: {e}"))?);
        let decoder = make_decoder(&ctx, codec, backend)?;

        // Probe dimensions from the first decodable frame.
        let mut src = AnnexBSource {
            decoder,
            ctx,
            bytes,
            aus,
            keyframes,
            codec,
            backend,
            submit_pos: 0,
            pending: Vec::new(),
        };
        let first = decode_annex_b_frame(&mut src, 0)?;
        let width = first.width;
        let height = first.height;
        let frame_count = src.aus.len();

        Ok(VideoSource {
            kind: SourceKind::AnnexB(src),
            path: None,
            width,
            height,
            frame_count,
            cache: vec![(0, first)],
            cache_cap: 2,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Decode (or return cached) frame `index`. Keeps a small cache of recent frames.
    pub fn frame(&mut self, index: usize) -> Result<&Nv12Host, String> {
        if index >= self.frame_count {
            return Err(format!("frame {index} out of range (0..{})", self.frame_count));
        }
        if let Some(pos) = self.cache.iter().position(|(i, _)| *i == index) {
            // Move hit to end (MRU).
            let item = self.cache.remove(pos);
            self.cache.push(item);
            return Ok(&self.cache.last().unwrap().1);
        }

        let host = match self.decode_frame(index) {
            Ok(h) => h,
            Err(e) if matches!(self.kind, SourceKind::Mp4(_) | SourceKind::AnnexB(_)) => {
                // libmkff can't decode this clip here (no hardware decoder, unsupported
                // profile, …): continue in software.
                self.fall_back_to_software()
                    .map_err(|e2| format!("{e}; software fallback: {e2}"))?;
                if index >= self.frame_count {
                    return Err(format!(
                        "{e}; software decode has {} frames",
                        self.frame_count
                    ));
                }
                self.decode_frame(index)
                    .map_err(|e2| format!("{e}; software fallback: {e2}"))?
            }
            Err(e) => return Err(e),
        };
        if self.width == 0 {
            self.width = host.width;
            self.height = host.height;
        }
        self.cache.push((index, host));
        while self.cache.len() > self.cache_cap {
            self.cache.remove(0);
        }
        Ok(&self.cache.last().unwrap().1)
    }

    fn decode_frame(&mut self, index: usize) -> Result<Nv12Host, String> {
        Ok(match &mut self.kind {
            SourceKind::Software(media) => {
                let f = media
                    .video_frame(index as u64)
                    .map_err(|e| format!("frame {index}: {e}"))?;
                Nv12Host::from_sw_frame(&f)
            }
            SourceKind::SoftwareAnnexB(sw) => decode_sw_annex_b_frame(sw, index)?,
            SourceKind::Mp4(mp4) => match decode_mp4_frame(mp4, index as u32) {
                Ok(h) => h,
                Err(e)
                    if mp4.codec == VideoCodec::MKFF_VIDEO_CODEC_HEVC
                        && mp4.backend != VideoBackend::MKFF_VIDEO_BACKEND_SOFTWARE_ONLY =>
                {
                    // HW HEVC can fail to emit frames on some devices/clips; fall back.
                    mp4.backend = VideoBackend::MKFF_VIDEO_BACKEND_SOFTWARE_ONLY;
                    recreate_mp4_decoder(mp4)?;
                    mp4.demux
                        .seek_sample(0)
                        .map_err(|err| format!("seek after SW fallback: {err}"))?;
                    mp4.demux_pos = 0;
                    decode_mp4_frame(mp4, index as u32)
                        .map_err(|e2| format!("{e}; software fallback: {e2}"))?
                }
                Err(e) => return Err(e),
            },
            SourceKind::AnnexB(ab) => decode_annex_b_frame(ab, index)?,
        })
    }
}

fn decode_sw_annex_b_frame(sw: &mut SwAnnexBSource, index: usize) -> Result<Nv12Host, String> {
    if let Some(pos) = sw.pending.iter().position(|(i, _)| *i == index) {
        return Ok(sw.pending.remove(pos).1);
    }
    let sync = sw
        .keyframes
        .iter()
        .copied()
        .rfind(|&k| k <= index)
        .unwrap_or(0);
    if sw.submit_pos > index || sw.submit_pos < sync {
        sw.decoder.reset();
        sw.submit_pos = sync;
        sw.pending.clear();
    }
    while sw.pending.iter().all(|(i, _)| *i != index) {
        let frames = if sw.submit_pos >= sw.aus.len() {
            let rest = sw.decoder.flush();
            if rest.is_empty() {
                break;
            }
            rest
        } else {
            let i = sw.submit_pos;
            sw.submit_pos += 1;
            sw.decoder
                .decode(&sw.bytes[sw.aus[i].clone()], i as i64)
                .map_err(|e| format!("AU {i}: {e}"))?
        };
        sw.pending.extend(
            frames
                .iter()
                .map(|f| (f.pts() as usize, Nv12Host::from_sw_frame(f))),
        );
        if sw.pending.len() > 8 {
            sw.pending.sort_by_key(|(i, _)| *i);
            let keep_from = sw.pending.len() - 8;
            sw.pending.drain(0..keep_from);
        }
    }
    sw.pending
        .iter()
        .position(|(i, _)| *i == index)
        .map(|pos| sw.pending.remove(pos).1)
        .ok_or_else(|| format!("no frame for AU {index}"))
}

fn make_decoder(
    ctx: &Box<Context>,
    codec: VideoCodec,
    backend: VideoBackend,
) -> Result<VideoDecoder<'static>, String> {
    // Context is heap-pinned for the source lifetime; decoder is dropped first
    // in SourceKind Drop order (field order: demux/bytes, then ctx, then decoder
    // — wait, decoder is after ctx so Drop drops decoder first). Good.
    let ctx_ref: &'static Context = unsafe { &*(ctx.as_ref() as *const Context) };
    ctx_ref
        .video_decoder(codec, 8, backend)
        .map_err(|e| format!("decoder create ({codec:?}): {e}"))
}

fn recreate_mp4_decoder(mp4: &mut Mp4Source) -> Result<(), String> {
    let ctx_ref: &'static Context = unsafe { &*(mp4.ctx.as_ref() as *const Context) };
    mp4.decoder = ctx_ref
        .video_decoder(mp4.codec, 8, mp4.backend)
        .map_err(|e| format!("decoder recreate: {e}"))?;
    mp4.pending.clear();
    mp4.exhausted = false;
    Ok(())
}

fn recreate_annex_b_decoder(ab: &mut AnnexBSource) -> Result<(), String> {
    let ctx_ref: &'static Context = unsafe { &*(ab.ctx.as_ref() as *const Context) };
    ab.decoder = ctx_ref
        .video_decoder(ab.codec, 8, ab.backend)
        .map_err(|e| format!("decoder recreate: {e}"))?;
    ab.pending.clear();
    ab.submit_pos = 0;
    Ok(())
}

fn sync_at_or_before(sync_samples: &[u32], target: u32) -> u32 {
    let mut best = sync_samples[0];
    for &s in sync_samples {
        if s <= target {
            best = s;
        } else {
            break;
        }
    }
    best
}

fn take_pending_mp4(pending: &mut Vec<(u32, Nv12Host)>, index: u32) -> Option<Nv12Host> {
    if let Some(pos) = pending.iter().position(|(i, _)| *i == index) {
        Some(pending.remove(pos).1)
    } else {
        None
    }
}

fn decode_mp4_frame(mp4: &mut Mp4Source, index: u32) -> Result<Nv12Host, String> {
    if let Some(host) = take_pending_mp4(&mut mp4.pending, index) {
        return Ok(host);
    }

    let sync = sync_at_or_before(&mp4.sync_samples, index);
    // Continue only if demux is still inside [sync, index] and decoder is live.
    let can_continue =
        !mp4.exhausted && mp4.demux_pos >= sync && mp4.demux_pos <= index;
    if !can_continue {
        recreate_mp4_decoder(mp4)?;
        mp4.demux
            .seek_sample(sync)
            .map_err(|e| format!("seek_sample({sync}): {e}"))?;
        mp4.demux_pos = sync;
        mp4.pending.clear();
    }

    let mut guard = 0u32;
    while mp4.pending.iter().all(|(i, _)| *i != index) {
        guard += 1;
        if guard > index.saturating_sub(sync).saturating_add(64) {
            return Err(format!(
                "failed to produce sample {index} (demux_pos={})",
                mp4.demux_pos
            ));
        }

        // Keep feeding until we've submitted the target sample.
        if mp4.demux_pos <= index {
            match mp4.demux.read_access_unit().map_err(|e| format!("read AU: {e}"))? {
                ReadAuOutcome::EndOfStream => {
                    mp4.decoder.flush().map_err(|e| format!("flush: {e}"))?;
                    drain_mp4_pending(mp4)?;
                    mp4.exhausted = true;
                    break;
                }
                ReadAuOutcome::Au(au) => {
                    let sample_index = au.sample_index;
                    mp4.decoder
                        .submit(au.data, Some(sample_index as i64), Some(au.dts))
                        .map_err(|e| format!("submit sample {sample_index}: {e}"))?;
                    mp4.demux_pos = sample_index.saturating_add(1);
                    drain_mp4_pending(mp4)?;
                }
            }
            continue;
        }

        // Submitted target but frame not out yet (reorder delay): peek further,
        // then flush once if still missing (marks decoder exhausted).
        match mp4.demux.read_access_unit().map_err(|e| format!("read AU: {e}"))? {
            ReadAuOutcome::EndOfStream => {
                mp4.decoder.flush().map_err(|e| format!("flush: {e}"))?;
                drain_mp4_pending(mp4)?;
                mp4.exhausted = true;
                break;
            }
            ReadAuOutcome::Au(au) => {
                let sample_index = au.sample_index;
                mp4.decoder
                    .submit(au.data, Some(sample_index as i64), Some(au.dts))
                    .map_err(|e| format!("submit sample {sample_index}: {e}"))?;
                mp4.demux_pos = sample_index.saturating_add(1);
                drain_mp4_pending(mp4)?;
                if mp4.pending.iter().all(|(i, _)| *i != index) && sample_index >= index + 16
                {
                    mp4.decoder.flush().map_err(|e| format!("flush: {e}"))?;
                    drain_mp4_pending(mp4)?;
                    mp4.exhausted = true;
                    break;
                }
            }
        }
    }

    take_pending_mp4(&mut mp4.pending, index)
        .ok_or_else(|| format!("no frame for sample {index}"))
}

fn drain_mp4_pending(mp4: &mut Mp4Source) -> Result<(), String> {
    loop {
        match mp4.decoder.receive().map_err(|e| format!("receive: {e}"))? {
            ReceiveOutcome::Frame(frame) => {
                let info = frame.info().map_err(|e| format!("frame info: {e}"))?;
                if info.format != PixelFormat::MKFF_PIXEL_FORMAT_NV12 {
                    return Err(format!("expected NV12, got {:?}", info.format));
                }
                let sample_index = info.pts.unwrap_or(0) as u32;
                let planes = frame
                    .map_cpu_planes()
                    .map_err(|e| format!("map_cpu_planes: {e}"))?;
                let host = Nv12Host::from_cpu_planes(&planes).map_err(|e| e.to_string())?;
                mp4.pending.push((sample_index, host));
            }
            ReceiveOutcome::NotReady | ReceiveOutcome::EndOfStream => break,
        }
    }
    // Bound pending memory (keep a small window ahead of the lowest index).
    if mp4.pending.len() > 8 {
        mp4.pending.sort_by_key(|(i, _)| *i);
        let keep_from = mp4.pending.len().saturating_sub(8);
        mp4.pending.drain(0..keep_from);
    }
    Ok(())
}

fn decode_annex_b_frame(ab: &mut AnnexBSource, index: usize) -> Result<Nv12Host, String> {
    if let Some(pos) = ab.pending.iter().position(|(i, _)| *i == index) {
        return Ok(ab.pending.remove(pos).1);
    }

    let sync = {
        let mut best = ab.keyframes[0];
        for &k in &ab.keyframes {
            if k <= index {
                best = k;
            } else {
                break;
            }
        }
        best
    };

    if ab.submit_pos > index || ab.submit_pos < sync {
        recreate_annex_b_decoder(ab)?;
        ab.submit_pos = sync;
        ab.pending.clear();
    }

    let mut guard = 0usize;
    while ab.pending.iter().all(|(i, _)| *i != index) {
        guard += 1;
        if guard > index + 64 {
            return Err(format!("failed to produce AU {index}"));
        }
        if ab.submit_pos >= ab.aus.len() {
            ab.decoder.flush().map_err(|e| format!("flush: {e}"))?;
            drain_annex_b_pending(ab)?;
            break;
        }
        let i = ab.submit_pos;
        let range = ab.aus[i].clone();
        let au = &ab.bytes[range];
        ab.decoder
            .submit(au, Some(i as i64), Some(i as i64))
            .map_err(|e| format!("submit AU {i}: {e}"))?;
        ab.submit_pos = i + 1;
        drain_annex_b_pending(ab)?;
    }

    if let Some(pos) = ab.pending.iter().position(|(i, _)| *i == index) {
        Ok(ab.pending.remove(pos).1)
    } else {
        Err(format!("no frame for AU {index}"))
    }
}

fn drain_annex_b_pending(ab: &mut AnnexBSource) -> Result<(), String> {
    loop {
        match ab.decoder.receive().map_err(|e| format!("receive: {e}"))? {
            ReceiveOutcome::Frame(frame) => {
                let info = frame.info().map_err(|e| format!("frame info: {e}"))?;
                if info.format != PixelFormat::MKFF_PIXEL_FORMAT_NV12 {
                    return Err(format!("expected NV12, got {:?}", info.format));
                }
                let idx = info.pts.unwrap_or(0) as usize;
                let planes = frame
                    .map_cpu_planes()
                    .map_err(|e| format!("map_cpu_planes: {e}"))?;
                let host = Nv12Host::from_cpu_planes(&planes).map_err(|e| e.to_string())?;
                ab.pending.push((idx, host));
            }
            ReceiveOutcome::NotReady | ReceiveOutcome::EndOfStream => break,
        }
    }
    if ab.pending.len() > 8 {
        ab.pending.sort_by_key(|(i, _)| *i);
        let keep_from = ab.pending.len().saturating_sub(8);
        ab.pending.drain(0..keep_from);
    }
    Ok(())
}

/// Decode a media file: progressive MP4/MOV or Annex-B `.hevc` / `.h264`.
///
/// Prefer [`VideoSource`] for large clips — this helper still predecodes every
/// frame and is intended for tiny fixtures / tests.
pub fn decode_video_file(path: &std::path::Path) -> Result<Vec<Nv12Host>, String> {
    let mut src = VideoSource::open(path)?;
    let n = src.frame_count();
    let mut frames = Vec::with_capacity(n);
    for i in 0..n {
        frames.push(src.frame(i)?.clone_host());
    }
    Ok(frames)
}

/// Legacy name: HEVC Annex-B software decode (still rejects unsupported containers).
pub fn decode_hevc_software_file(path: &std::path::Path) -> Result<Vec<Nv12Host>, String> {
    decode_video_file(path)
}

pub fn decode_hevc_software_bytes(bytes: &[u8]) -> Result<Vec<Nv12Host>, String> {
    let mut src = VideoSource::open_bytes(std::path::Path::new("clip.hevc"), bytes.to_vec())?;
    let n = src.frame_count();
    let mut frames = Vec::with_capacity(n);
    for i in 0..n {
        frames.push(src.frame(i)?.clone_host());
    }
    Ok(frames)
}

impl Nv12Host {
    fn clone_host(&self) -> Self {
        Nv12Host {
            width: self.width,
            height: self.height,
            y: self.y.clone(),
            y_stride: self.y_stride,
            uv: self.uv.clone(),
            uv_stride: self.uv_stride,
        }
    }
}

fn path_ext(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The Annex-B codec an extension names.
fn annex_b_codec(ext: &str) -> Option<VideoCodec> {
    match ext {
        "h264" | "264" | "avc" => Some(VideoCodec::MKFF_VIDEO_CODEC_H264),
        "hevc" | "h265" | "265" => Some(VideoCodec::MKFF_VIDEO_CODEC_HEVC),
        _ => None,
    }
}

/// H.264 or HEVC from the first NAL unit header of an Annex-B stream: HEVC streams open with a
/// VPS (type 32, two-byte header `40 01`), H.264 with an AUD, SPS or SEI.
fn sniff_annex_b_codec(bytes: &[u8]) -> Option<VideoCodec> {
    let start = bytes.windows(3).position(|w| w == [0, 0, 1])? + 3;
    let (b0, b1) = (*bytes.get(start)?, *bytes.get(start + 1)?);
    if b0 & 0x81 == 0 && matches!((b0 >> 1) & 0x3F, 32..=35 | 39) && b1 & 7 != 0 {
        Some(VideoCodec::MKFF_VIDEO_CODEC_HEVC)
    } else if b0 & 0x80 == 0 && matches!(b0 & 0x1F, 1 | 5..=9) {
        Some(VideoCodec::MKFF_VIDEO_CODEC_H264)
    } else {
        None
    }
}

/// ISOBMFF / MP4 files typically begin with a size + `ftyp` box.
fn looks_like_isom_container(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && &bytes[4..8] == b"ftyp"
}

fn looks_like_hevc_annex_b(bytes: &[u8]) -> bool {
    !split_hevc_access_units(bytes).is_empty()
}

fn looks_like_h264_annex_b(bytes: &[u8]) -> bool {
    !split_h264_access_units(bytes).is_empty()
}
