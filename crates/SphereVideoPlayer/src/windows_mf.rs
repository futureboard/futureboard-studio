//! Windows video frame reader built on the Media Foundation Source Reader.
//!
//! Media Foundation is part of the operating system, so this backend adds no
//! FFmpeg/libav dependency and no redistributable codec binaries — it decodes
//! whatever the machine's installed codecs already handle (H.264/AAC MP4 and
//! MOV out of the box, HEVC/VP9 when the corresponding OS codec is present).
//!
//! ## Two paths, chosen at open time
//!
//! ```text
//! hardware: file -> Source Reader (D3D manager) -> DXVA decoder
//!                -> NV12/P010 ID3D11Texture2D   -> D3D11 Video Processor
//!                -> BGRA (preview size)         -> VideoFrame
//!
//! software: file -> Source Reader (video processing MFT)
//!                -> RGB32 system memory         -> VideoFrame
//! ```
//!
//! The hardware path deliberately never asks the decoder for `MFVideoFormat_RGB32`.
//! No hardware decoder emits RGB, so with a DXGI device manager installed the
//! reader has to build an RGB32 chain out of transforms that cannot consume D3D
//! surfaces, and negotiation fails with
//! `MF_E_TOPO_CODEC_NOT_FOUND (0xC00D5212)`. NV12 (or P010 for 10-bit) is the
//! decoder's own output; colour conversion and downscale are done afterwards on
//! the GPU by [`crate::windows_d3d::FrameConverter`].
//!
//! RGB32 survives only in the software configuration, where there is no D3D
//! manager and the OS Video Processor MFT is the intended converter. The
//! hardware path never silently falls back to it.
//!
//! ## Threading
//!
//! `IMFSourceReader` is apartment-bound and this type is neither `Send` nor
//! `Sync`. Construct and use it from exactly one worker thread —
//! `crate::preview::VideoPreview` owns that thread. `ReadSample` blocks, so it
//! must never run on the GPUI render thread.
//!
//! ## Frame budget
//!
//! This is a picture monitor, not a playback engine: it decodes the single
//! frame under the playhead on demand and caches exactly one. There is no
//! decode-ahead queue to bound and no way for the decoder to run ahead of
//! presentation — the transport asks, the worker answers, and the previous
//! answer is dropped.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFDXGIBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_PD_DURATION,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_DISABLE_DXVA,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MF2DBuffer_LockFlags_Read, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFNominalRange_0_255,
    MFSTARTUP_NOSOCKET, MFStartup, MFVideoFormat_ARGB32, MFVideoFormat_NV12, MFVideoFormat_P010,
    MFVideoFormat_RGB32, MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HSTRING, Interface};

use crate::windows_d3d::{
    ColorInfo, FrameConverter, HwPixelFormat, VideoDevice, YuvPlanes, cpu_convert_to_bgra,
    shared_video_device,
};
use crate::{VideoError, VideoFrame, VideoInfo};

/// Media Foundation timestamps and durations are in 100-nanosecond units.
const HNS_PER_SECOND: f64 = 10_000_000.0;

/// Reading forward is much cheaper than a seek (a seek lands on the preceding
/// key frame and re-decodes the GOP). When the requested position is ahead of
/// the reader by no more than this, step forward instead of seeking.
const FORWARD_SCAN_WINDOW_SECONDS: f64 = 2.0;

/// Upper bound on samples consumed by one `frame_at` call, so a pathological
/// stream cannot pin the worker thread indefinitely. On overrun the most
/// recently decoded frame is returned rather than an error.
const MAX_SAMPLES_PER_REQUEST: u32 = 480;

/// Longest edge the preview presents at. A picture monitor is watched at a few
/// hundred points across, so presenting a 4K source at full size buys nothing
/// and costs 33 MB per frame in readback, copy, and texture upload. The GPU
/// video processor does the scale, so the decoder still runs at native size and
/// nothing full-resolution is ever produced on the CPU.
const PREVIEW_MAX_EDGE: u32 = 1280;

/// How often the per-frame timing summary is emitted. One line per decoded
/// frame would be noise in a release build.
const PERF_LOG_INTERVAL_SECONDS: f64 = 1.0;

fn hresult_error(context: &str, err: windows::core::Error) -> VideoError {
    VideoError::Backend(format!("{context}: {err}"))
}

/// `MFStartup` is process-global and reference-counted by the OS. Calling it
/// once for the process lifetime is intentional: the preview worker can be
/// stopped and restarted while the app runs, and matching every start with a
/// shutdown would tear down Media Foundation under a still-open reader.
fn ensure_media_foundation_started() -> Result<(), VideoError> {
    use std::sync::OnceLock;
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();

    STARTED
        .get_or_init(|| unsafe {
            // The reader is used from a plain worker thread, so it needs COM in
            // the multithreaded apartment. `RPC_E_CHANGED_MODE` means the thread
            // already joined an apartment — harmless here.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|err| err.to_string())
        })
        .clone()
        .map_err(VideoError::Backend)
}

/// The negotiated output of the Source Reader, and how to turn it into BGRA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    /// Hardware decoder output. Samples are expected to carry an
    /// `IMFDXGIBuffer`; when one does not, the CPU converter handles it.
    Hardware(HwPixelFormat),
    /// Software configuration: the OS Video Processor MFT already produced
    /// packed 32-bit BGRA in system memory.
    SoftwareRgb32,
}

impl OutputKind {
    fn subtype_name(self) -> &'static str {
        match self {
            Self::Hardware(format) => format.name(),
            Self::SoftwareRgb32 => "RGB32",
        }
    }
}

/// Frame geometry and colour as the reader currently reports it. Re-read
/// whenever the source reader signals a mid-stream media type change.
#[derive(Debug, Clone, Copy)]
struct OutputFormat {
    kind: OutputKind,
    width: u32,
    height: u32,
    /// Row pitch in bytes for the RGB32 path. Negative means bottom-up.
    stride: i32,
    color: ColorInfo,
}

impl OutputFormat {
    fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }

    fn abs_stride(&self) -> usize {
        self.stride.unsigned_abs() as usize
    }

    /// Size the preview presents at: the source size, capped on the long edge.
    fn preview_size(&self) -> (u32, u32) {
        preview_frame_size(self.width, self.height).unwrap_or((self.width, self.height))
    }
}

/// Rate-limited timing summary. Every field is filled by the decode path and
/// drained once a second, so a release build never logs per frame.
#[derive(Default)]
struct PerfCounters {
    decoded_frames: u32,
    decode_seconds: f64,
    convert_seconds: f64,
    last_report: Option<Instant>,
}

impl PerfCounters {
    fn record(&mut self, decode: f64, convert: f64) {
        self.decoded_frames += 1;
        self.decode_seconds += decode;
        self.convert_seconds += convert;
    }

    fn maybe_report(&mut self, source_fps: f64, queued: usize) {
        let now = Instant::now();
        let elapsed = match self.last_report {
            None => {
                self.last_report = Some(now);
                return;
            }
            Some(previous) => now.duration_since(previous).as_secs_f64(),
        };
        if elapsed < PERF_LOG_INTERVAL_SECONDS {
            return;
        }
        if self.decoded_frames > 0 {
            let frames = self.decoded_frames as f64;
            eprintln!(
                "[video-perf] source_fps={source_fps:.0} decode_fps={:.1} queue={queued} \
                 decode_ms={:.2} convert_ms={:.2}",
                frames / elapsed,
                self.decode_seconds * 1000.0 / frames,
                self.convert_seconds * 1000.0 / frames,
            );
        }
        self.decoded_frames = 0;
        self.decode_seconds = 0.0;
        self.convert_seconds = 0.0;
        self.last_report = Some(now);
    }
}

pub(crate) struct MediaFoundationDecoder {
    reader: IMFSourceReader,
    format: OutputFormat,
    info: VideoInfo,
    /// The shared D3D11 device, when the reader was configured for hardware
    /// decode. `None` in the software configuration.
    device: Option<Arc<VideoDevice>>,
    /// GPU colour-convert/scale stage. Built lazily on the first hardware
    /// sample (the decoder's real surface size is only known then) and rebuilt
    /// if the stream changes geometry mid-play.
    converter: Option<FrameConverter>,
    /// `true` once a sample has been seen without an `IMFDXGIBuffer` and the CPU
    /// fallback took over, so the choice is logged exactly once.
    logged_cpu_fallback: bool,
    /// `true` once the GPU conversion stage failed to build on this driver. Set
    /// once so the failed creation is not retried for every subsequent frame.
    gpu_conversion_unavailable: bool,
    /// Most recently decoded frame. Behind an `Arc` because it is handed out on
    /// every cache hit and every end-of-stream hold — cloning the `VideoFrame`
    /// itself would memcpy the whole uncompressed image several times per
    /// transport tick. Exactly one frame is retained: this is the whole decoded
    /// "queue", so it cannot grow with playback or with repeated seeking.
    last_frame: Option<Arc<VideoFrame>>,
    /// `true` once the reader has produced end-of-stream and has not been
    /// seeked since; further forward reads would return nothing.
    at_end: bool,
    perf: PerfCounters,
}

impl MediaFoundationDecoder {
    pub(crate) fn open(path: &Path) -> Result<Self, VideoError> {
        ensure_media_foundation_started()?;

        unsafe {
            let device = shared_video_device();

            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 4)
                .map_err(|e| hresult_error("source open: MFCreateAttributes", e))?;
            let attributes = attributes
                .ok_or_else(|| VideoError::Backend("source open: null reader attributes".into()))?;

            // Hardware decode. Without a D3D manager the Source Reader only ever
            // instantiates *software* decoder MFTs, which is what made a 4K
            // long-GOP reference video crawl: every seek re-decoded a GOP on the
            // CPU.
            //
            // `ENABLE_VIDEO_PROCESSING` is deliberately *not* set here. It
            // inserts the OS Video Processor MFT, which cannot consume D3D
            // surfaces, and combining it with a D3D manager is what made RGB32
            // negotiation fail. The GPU conversion this backend does itself
            // replaces it.
            if let Some(device) = &device {
                attributes
                    .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                    .map_err(|e| hresult_error("source open: enable hardware transforms", e))?;
                attributes
                    .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, device.manager())
                    .map_err(|e| hresult_error("source open: set D3D manager", e))?;
                attributes
                    .SetUINT32(&MF_SOURCE_READER_DISABLE_DXVA, 0)
                    .map_err(|e| hresult_error("source open: enable DXVA", e))?;
            } else {
                // Software configuration: let the Source Reader insert the OS
                // Video Processor MFT, which converts the decoder's native
                // NV12/YUV output to RGB32 in system memory. This is the only
                // place RGB32 is legitimate.
                attributes
                    .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                    .map_err(|e| hresult_error("source open: enable video processing", e))?;
            }

            let url = HSTRING::from(path.as_os_str());
            let reader = MFCreateSourceReaderFromURL(&url, &attributes)
                .map_err(|e| hresult_error("source open: MFCreateSourceReaderFromURL", e))?;

            let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .map_err(|e| hresult_error("source open: deselect streams", e))?;
            // A container with no video stream fails here rather than later with
            // an opaque read error.
            reader
                .SetStreamSelection(video_stream, true)
                .map_err(|_| VideoError::NoVideoStream)?;

            let native = reader.GetNativeMediaType(video_stream, 0).ok();
            let native_size = native.as_ref().and_then(frame_size);
            let native_subtype = native
                .as_ref()
                .and_then(|t| t.GetGUID(&MF_MT_SUBTYPE).ok())
                .map(|guid| subtype_name(&guid))
                .unwrap_or_else(|| "unknown".to_string());

            let kind = negotiate_output(
                &reader,
                video_stream,
                device.is_some(),
                &native_subtype,
                native_size,
            )?;

            let format = read_output_format(&reader, kind)?;
            let info = VideoInfo {
                // `info` describes the *source*, so it keeps native dimensions.
                width: native_size.map(|(w, _)| w).unwrap_or(format.width),
                height: native_size.map(|(_, h)| h).unwrap_or(format.height),
                duration_seconds: read_duration_seconds(&reader),
                frame_rate: read_frame_rate(&reader),
            };

            let (preview_width, preview_height) = format.preview_size();
            eprintln!("[video-mf] hardware-transform  = {}", device.is_some());
            eprintln!(
                "[video-mf] requested-output    = {}",
                if device.is_some() {
                    "NV12/P010"
                } else {
                    "RGB32"
                }
            );
            eprintln!(
                "[video-mf] negotiated-output   = {} {}x{} @ {:.3} fps ({})",
                kind.subtype_name(),
                format.width,
                format.height,
                info.frame_rate,
                interlace_mode_name(&reader),
            );
            eprintln!(
                "[video-mf] render-bridge       = {}",
                if device.is_some() {
                    "d3d11-video-processor -> bgra readback -> gpui atlas"
                } else {
                    "mf video processor -> rgb32 -> gpui atlas"
                }
            );
            eprintln!("[video-mf] present-size        = {preview_width}x{preview_height}");
            eprintln!("[video-mf] frame-queue-limit   = 1 (on-demand picture monitor)");

            Ok(Self {
                reader,
                format,
                info,
                device,
                converter: None,
                logged_cpu_fallback: false,
                gpu_conversion_unavailable: false,
                last_frame: None,
                at_end: false,
                perf: PerfCounters::default(),
            })
        }
    }

    pub(crate) fn info(&self) -> VideoInfo {
        self.info
    }

    /// Nominal duration of one frame, used to decide whether the cached frame
    /// still covers the requested position.
    fn frame_duration_seconds(&self) -> f64 {
        if self.info.frame_rate > 0.0 {
            1.0 / self.info.frame_rate
        } else {
            1.0 / 30.0
        }
    }

    pub(crate) fn frame_at(&mut self, seconds: f64) -> Result<Arc<VideoFrame>, VideoError> {
        let frame_duration = self.frame_duration_seconds();

        // Already showing the right frame — most scrub/tick requests land here.
        if let Some(frame) = &self.last_frame {
            let start = frame.timestamp_seconds;
            if seconds >= start && seconds < start + frame_duration {
                return Ok(Arc::clone(frame));
            }
        }

        let needs_seek = match &self.last_frame {
            None => true,
            Some(frame) => {
                seconds < frame.timestamp_seconds
                    || seconds > frame.timestamp_seconds + FORWARD_SCAN_WINDOW_SECONDS
            }
        };

        if needs_seek {
            self.seek(seconds)?;
        } else if self.at_end {
            // Forward reads cannot produce anything new past end-of-stream;
            // hold the last frame instead of spinning.
            return self.last_frame.clone().ok_or(VideoError::EndOfStream);
        }

        self.read_forward_to(seconds, frame_duration)
    }

    /// Flushes the reader and drops every frame decoded for the old position,
    /// so a seek can never present a stale picture from where the playhead was.
    fn seek(&mut self, seconds: f64) -> Result<(), VideoError> {
        let hns = (seconds * HNS_PER_SECOND).round().max(0.0) as i64;
        unsafe {
            let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            // Discards samples the decoder already produced for the previous
            // position; without it the first read after a seek can return a
            // frame from before it.
            let _ = self.reader.Flush(video_stream);

            let mut position: windows::Win32::System::Com::StructuredStorage::PROPVARIANT =
                std::mem::zeroed();
            // `PROPVARIANT`'s payload sits behind a `ManuallyDrop` union field,
            // so the writes are explicit derefs — the zeroed value it replaces
            // owns nothing and must not be dropped.
            (*position.Anonymous.Anonymous).vt = VT_I8;
            (*position.Anonymous.Anonymous).Anonymous.hVal = hns;
            self.reader
                .SetCurrentPosition(&GUID::zeroed(), &position)
                .map_err(|e| hresult_error("seek: SetCurrentPosition", e))?;
        }
        self.last_frame = None;
        self.at_end = false;
        Ok(())
    }

    /// Consumes samples until one covers `target_seconds`, then returns it.
    fn read_forward_to(
        &mut self,
        target_seconds: f64,
        frame_duration: f64,
    ) -> Result<Arc<VideoFrame>, VideoError> {
        let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

        for _ in 0..MAX_SAMPLES_PER_REQUEST {
            let mut flags = 0u32;
            let mut timestamp_hns = 0i64;
            let mut sample = None;

            let decode_started = Instant::now();
            unsafe {
                self.reader
                    .ReadSample(
                        video_stream,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp_hns),
                        Some(&mut sample),
                    )
                    .map_err(|e| hresult_error("decode: ReadSample", e))?;
            }
            let decode_seconds = decode_started.elapsed().as_secs_f64();

            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                // Geometry or pixel format changed mid-stream. Re-read it and
                // drop the conversion stage; it is rebuilt against the new
                // format on the next sample.
                self.format = read_output_format(&self.reader, self.format.kind)?;
                self.converter = None;
                eprintln!(
                    "[video-mf] media type changed  = {} {}x{}",
                    self.format.kind.subtype_name(),
                    self.format.width,
                    self.format.height
                );
            }

            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                self.at_end = true;
                // Past the end, the last decoded frame is the honest answer for
                // a reference view; only a stream that produced nothing errors.
                return self.last_frame.clone().ok_or(VideoError::EndOfStream);
            }

            // Gaps and stream ticks carry no sample; keep reading.
            let Some(sample) = sample else {
                continue;
            };

            let timestamp_seconds = timestamp_hns as f64 / HNS_PER_SECOND;
            let convert_started = Instant::now();
            let frame = Arc::new(self.frame_from_sample(&sample, timestamp_seconds)?);
            self.perf
                .record(decode_seconds, convert_started.elapsed().as_secs_f64());
            self.perf.maybe_report(self.info.frame_rate, 1);

            let reached_target = timestamp_seconds + frame_duration > target_seconds;
            // Replacing the cached frame releases the previous one's pixels and,
            // on the hardware path, the previous sample's decoder surface.
            self.last_frame = Some(Arc::clone(&frame));
            if reached_target {
                return Ok(frame);
            }
        }

        // Budget exhausted (very long GOP or a badly timestamped stream): return
        // what was decoded rather than blocking the worker for another pass.
        self.last_frame.clone().ok_or(VideoError::EndOfStream)
    }

    /// Turns one decoded sample into a BGRA frame, on whichever path the
    /// sample's buffer supports.
    fn frame_from_sample(
        &mut self,
        sample: &IMFSample,
        timestamp_seconds: f64,
    ) -> Result<VideoFrame, VideoError> {
        match self.format.kind {
            OutputKind::SoftwareRgb32 => {
                convert_rgb32_sample(sample, self.format, timestamp_seconds)
            }
            OutputKind::Hardware(pixel_format) => {
                // The sample owns the decoder surface; it must outlive the blt,
                // which it does — `sample` is borrowed for this whole call and
                // the converter's readback completes before returning.
                let surface = (!self.gpu_conversion_unavailable)
                    .then(|| dxgi_surface(sample))
                    .flatten();

                // A `None` from the converter means the GPU stage could not be
                // built on this driver; fall through to the CPU converter,
                // which this sample's buffer still supports.
                if let Some((texture, subresource)) = surface
                    && let Some(frame) = self.convert_hardware_sample(
                        &texture,
                        subresource,
                        pixel_format,
                        timestamp_seconds,
                    )?
                {
                    return Ok(frame);
                }

                // Either the sample never carried an `IMFDXGIBuffer` (a software
                // decoder MFT ran despite the D3D manager) or the GPU conversion
                // stage could not be built on this driver. Both keep showing
                // picture through the CPU converter: `IMFMediaBuffer::Lock` on a
                // D3D-backed buffer makes Media Foundation stage it into system
                // memory.
                if !self.logged_cpu_fallback {
                    self.logged_cpu_fallback = true;
                    eprintln!(
                        "[video-mf] dxgi-buffer         = false ({} converted on the CPU)",
                        pixel_format.name()
                    );
                    eprintln!("[video-mf] cpu-readback        = full frame");
                }
                convert_system_memory_yuv(sample, self.format, pixel_format, timestamp_seconds)
            }
        }
    }

    fn convert_hardware_sample(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
        pixel_format: HwPixelFormat,
        timestamp_seconds: f64,
    ) -> Result<Option<VideoFrame>, VideoError> {
        let Some(device) = self.device.clone() else {
            return Err(VideoError::Backend(
                "hardware surface retrieval: a D3D surface arrived with no D3D device".into(),
            ));
        };

        let (width, height) = (self.format.width, self.format.height);
        let (preview_width, preview_height) = self.format.preview_size();

        let needs_build = !self
            .converter
            .as_ref()
            .is_some_and(|c| c.matches(pixel_format, width, height, preview_width, preview_height));
        if needs_build {
            match FrameConverter::new(
                Arc::clone(&device),
                pixel_format,
                width,
                height,
                preview_width,
                preview_height,
                self.format.color,
            ) {
                Ok(converter) => {
                    eprintln!(
                        "[video-mf] dxgi-buffer         = true ({} {width}x{height} -> BGRA \
                         {preview_width}x{preview_height})",
                        pixel_format.name()
                    );
                    eprintln!("[video-mf] cpu-readback        = preview-size only");
                    self.converter = Some(converter);
                }
                Err(err) => {
                    // The GPU stage could not be built on this driver. Say so
                    // once, stop trying, and let the caller keep showing picture
                    // through the CPU converter rather than failing the file.
                    eprintln!(
                        "[video-mf] gpu conversion unavailable ({err}); \
                         falling back to CPU {} conversion",
                        pixel_format.name()
                    );
                    self.gpu_conversion_unavailable = true;
                    return Ok(None);
                }
            }
        }

        let converter = self
            .converter
            .as_mut()
            .expect("converter was just built or already matched");
        let bgra = converter.convert(texture, subresource)?;
        let (width, height) = converter.output_size();
        Ok(Some(VideoFrame {
            width,
            height,
            timestamp_seconds,
            bgra,
        }))
    }
}

/// Negotiates the reader's output format and returns which path was chosen.
///
/// Hardware order is NV12 first (what every mainstream SDR decoder emits), then
/// P010 for 10-bit sources. RGB32 is never requested with a D3D manager
/// installed. The software configuration asks for RGB32, then ARGB32, which the
/// OS Video Processor MFT produces from any decoder output.
fn negotiate_output(
    reader: &IMFSourceReader,
    stream: u32,
    hardware: bool,
    native_subtype: &str,
    native_size: Option<(u32, u32)>,
) -> Result<OutputKind, VideoError> {
    let candidates: &[(GUID, OutputKind)] = if hardware {
        &[
            (
                MFVideoFormat_NV12,
                OutputKind::Hardware(HwPixelFormat::Nv12),
            ),
            (
                MFVideoFormat_P010,
                OutputKind::Hardware(HwPixelFormat::P010),
            ),
        ]
    } else {
        &[
            (MFVideoFormat_RGB32, OutputKind::SoftwareRgb32),
            (MFVideoFormat_ARGB32, OutputKind::SoftwareRgb32),
        ]
    };

    let mut last_error: Option<windows::core::Error> = None;
    let mut attempted = Vec::new();

    for (subtype, kind) in candidates {
        attempted.push(subtype_name(subtype));
        unsafe {
            let requested = MFCreateMediaType()
                .map_err(|e| hresult_error("decoder negotiation: MFCreateMediaType", e))?;
            if let Err(err) = requested.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) {
                last_error = Some(err);
                continue;
            }
            if let Err(err) = requested.SetGUID(&MF_MT_SUBTYPE, subtype) {
                last_error = Some(err);
                continue;
            }
            match reader.SetCurrentMediaType(stream, None, &requested) {
                Ok(()) => return Ok(*kind),
                Err(err) => last_error = Some(err),
            }
        }
    }

    let (width, height) = native_size.unwrap_or((0, 0));
    let hresult = last_error
        .map(|err| err.to_string())
        .unwrap_or_else(|| "no HRESULT reported".to_string());
    Err(VideoError::Backend(format!(
        "decoder negotiation failed: requested [{}], source subtype {native_subtype}, \
         {width}x{height}, d3d-manager={hardware}, {hresult}",
        attempted.join(", "),
    )))
}

/// `MF_MT_FRAME_SIZE` packs width in the high 32 bits and height in the low.
/// Only the read direction is needed now that output size is never requested;
/// the writer stays for the round-trip test that pins the layout.
#[cfg(test)]
fn pack_frame_size(width: u32, height: u32) -> u64 {
    ((width as u64) << 32) | height as u64
}

fn unpack_frame_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
}

fn frame_size(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    unsafe {
        let (width, height) = unpack_frame_size(media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?);
        (width > 0 && height > 0).then_some((width, height))
    }
}

/// Human-readable name for the well-known subtypes this backend deals in. MF
/// subtype GUIDs embed a FourCC in `data1`, so anything else prints as its tag.
fn subtype_name(subtype: &GUID) -> String {
    if *subtype == MFVideoFormat_NV12 {
        return "NV12".into();
    }
    if *subtype == MFVideoFormat_P010 {
        return "P010".into();
    }
    if *subtype == MFVideoFormat_RGB32 {
        return "RGB32".into();
    }
    if *subtype == MFVideoFormat_ARGB32 {
        return "ARGB32".into();
    }
    let fourcc = subtype.data1.to_le_bytes();
    if fourcc.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&fourcc).into_owned()
    } else {
        format!("{subtype:?}")
    }
}

/// The size to present `width`x`height` at, or `None` when it already fits.
///
/// Never upscales, and keeps the aspect ratio. Both edges are rounded to even
/// numbers because chroma-subsampled sources scale to odd sizes badly.
fn preview_frame_size(width: u32, height: u32) -> Option<(u32, u32)> {
    let longest = width.max(height);
    if longest <= PREVIEW_MAX_EDGE || width == 0 || height == 0 {
        return None;
    }
    let scale = PREVIEW_MAX_EDGE as f64 / longest as f64;
    let even = |value: u32| ((value as f64 * scale).round() as u32).max(2) & !1;
    Some((even(width), even(height)))
}

/// Reads geometry, stride and colour from the reader's *current* output type.
/// Called after negotiation and again on every media-type change.
fn read_output_format(
    reader: &IMFSourceReader,
    kind: OutputKind,
) -> Result<OutputFormat, VideoError> {
    unsafe {
        let media_type = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|e| hresult_error("decoder negotiation: GetCurrentMediaType", e))?;

        let (width, height) = frame_size(&media_type).ok_or(VideoError::NoVideoStream)?;

        // Absent on some transforms. The fallback is one packed row in the
        // negotiated format — 4 bytes per pixel for RGB32, one luma byte per
        // pixel for NV12, two for P010 — and positive, because a transform that
        // reports no stride emits top-down.
        let default_stride = match kind {
            OutputKind::SoftwareRgb32 => width * 4,
            OutputKind::Hardware(HwPixelFormat::Nv12) => width,
            OutputKind::Hardware(HwPixelFormat::P010) => width * 2,
        };
        let stride = media_type
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|value| value as i32)
            .unwrap_or(default_stride as i32);

        // Trust the stream when it declares a matrix or range, and fall back to
        // the conventional reading of the frame height when it does not.
        let mut color = ColorInfo::assume_from_height(height);
        if let Ok(matrix) = media_type.GetUINT32(&MF_MT_YUV_MATRIX) {
            color.bt709 = matrix == MFVideoTransferMatrix_BT709.0 as u32;
        }
        if let Ok(range) = media_type.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) {
            color.studio_range = range != MFNominalRange_0_255.0 as u32;
        }

        Ok(OutputFormat {
            kind,
            width,
            height,
            stride,
            color,
        })
    }
}

fn interlace_mode_name(reader: &IMFSourceReader) -> &'static str {
    unsafe {
        let Ok(media_type) =
            reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
        else {
            return "unknown";
        };
        match media_type.GetUINT32(&MF_MT_INTERLACE_MODE) {
            Ok(mode) if mode == MFVideoInterlace_Progressive.0 as u32 => "progressive",
            Ok(_) => "interlaced",
            Err(_) => "unknown",
        }
    }
}

fn read_frame_rate(reader: &IMFSourceReader) -> f64 {
    unsafe {
        let Ok(media_type) =
            reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
        else {
            return 0.0;
        };
        let Ok(packed) = media_type.GetUINT64(&MF_MT_FRAME_RATE) else {
            return 0.0;
        };
        let numerator = (packed >> 32) as u32;
        let denominator = (packed & 0xFFFF_FFFF) as u32;
        if denominator == 0 {
            0.0
        } else {
            numerator as f64 / denominator as f64
        }
    }
}

fn read_duration_seconds(reader: &IMFSourceReader) -> f64 {
    unsafe {
        let Ok(value) =
            reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        else {
            return 0.0;
        };
        // MF_PD_DURATION is a VT_UI8 count of 100ns units; no heap payload to
        // release, so the PROPVARIANT can be read and dropped as plain data.
        let hns = value.Anonymous.Anonymous.Anonymous.uhVal;
        hns as f64 / HNS_PER_SECOND
    }
}

/// The `ID3D11Texture2D` and array slice behind a hardware-decoded sample, or
/// `None` when the sample lives in system memory.
///
/// The returned texture is a strong COM reference: the caller may use it after
/// the buffer goes out of scope, though the sample must stay alive for the
/// surface's *contents* to remain the decoded frame.
fn dxgi_surface(sample: &IMFSample) -> Option<(ID3D11Texture2D, u32)> {
    unsafe {
        let buffer = sample.GetBufferByIndex(0).ok()?;
        let dxgi = buffer.cast::<IMFDXGIBuffer>().ok()?;

        let mut resource: Option<ID3D11Texture2D> = None;
        dxgi.GetResource(
            &ID3D11Texture2D::IID,
            &mut resource as *mut _ as *mut *mut core::ffi::c_void,
        )
        .ok()?;
        let resource = resource?;
        let subresource = dxgi.GetSubresourceIndex().ok()?;
        Some((resource, subresource))
    }
}

/// Copies one RGB32 sample into a tightly packed, top-down BGRA buffer.
///
/// Software path only. Every row read is bounded by what the lock reported:
/// `IMF2DBuffer2::Lock2DSize` gives the real pitch and the buffer's extent,
/// and a buffer without it is read through the plain `Lock`, whose length is
/// the bound. (`IMF2DBuffer::Lock2D` reports no length, so a sample larger
/// than the cached format — a size change without a media-type change, or a
/// cropped surface — would have been read past its end.)
fn convert_rgb32_sample(
    sample: &IMFSample,
    format: OutputFormat,
    timestamp_seconds: f64,
) -> Result<VideoFrame, VideoError> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| hresult_error("presentation: ConvertToContiguousBuffer", e))?;

        let row_bytes = format.row_bytes();
        let height = format.height as usize;
        let mut bgra = vec![0u8; row_bytes * height];

        // The image's first scanline, the signed step to the next one, and
        // the locked bytes before and after that scanline. A bottom-up
        // surface is walked by stepping backwards from its top row.
        let two_d = buffer.cast::<IMF2DBuffer2>().ok();
        let (base, step, extent) = if let Some(two_d) = &two_d {
            let mut scanline0: *mut u8 = std::ptr::null_mut();
            let mut signed_pitch = 0i32;
            let mut buffer_start: *mut u8 = std::ptr::null_mut();
            let mut buffer_len = 0u32;
            two_d
                .Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scanline0,
                    &mut signed_pitch,
                    &mut buffer_start,
                    &mut buffer_len,
                )
                .map_err(|e| hresult_error("presentation: IMF2DBuffer2::Lock2DSize", e))?;
            let len = buffer_len as usize;
            let after = bytes_after(buffer_start, len, scanline0);
            let before = if after == 0 { 0 } else { len - after };
            (scanline0, signed_pitch as isize, (before, after))
        } else {
            let mut start: *mut u8 = std::ptr::null_mut();
            let mut locked_len = 0u32;
            buffer
                .Lock(&mut start, None, Some(&mut locked_len))
                .map_err(|e| hresult_error("presentation: IMFMediaBuffer::Lock", e))?;
            let len = locked_len as usize;
            let stride = format.abs_stride();
            if format.stride < 0 {
                // Bottom-up: buffer row 0 is the image's last row. Only an
                // address computation; the bounds below decide whether any
                // of it is read.
                let top = (stride * height.saturating_sub(1)).min(len);
                (
                    start.wrapping_add(top),
                    -(stride as isize),
                    (top, len - top),
                )
            } else {
                (start, stride as isize, (0, len))
            }
        };

        let result = if base.is_null() || !rows_fit(step, row_bytes, height, extent) {
            Err(VideoError::Backend(format!(
                "presentation: unexpected frame layout: {}x{} pitch={step} bytes={}+{}",
                format.width, format.height, extent.0, extent.1
            )))
        } else {
            for row in 0..height {
                // SAFETY: `rows_fit` checked that every row from `base`,
                // `row_bytes` long and `step` apart, lies inside the locked
                // bytes.
                let source = base.offset(step * row as isize);
                let destination = bgra.as_mut_ptr().add(row * row_bytes);
                std::ptr::copy_nonoverlapping(source, destination, row_bytes);
            }
            // RGB32's fourth channel is undefined padding; force it opaque so
            // the frame composites correctly in the UI.
            for pixel in bgra.as_chunks_mut::<4>().0 {
                pixel[3] = 0xFF;
            }
            Ok(())
        };

        if let Some(two_d) = &two_d {
            let _ = two_d.Unlock2D();
        } else {
            let _ = buffer.Unlock();
        }
        result?;

        Ok(VideoFrame {
            width: format.width,
            height: format.height,
            timestamp_seconds,
            bgra,
        })
    }
}

/// CPU conversion of an NV12/P010 sample that arrived in system memory.
///
/// Reached when the reader negotiated a hardware output format but the sample
/// carries no `IMFDXGIBuffer` — a software decoder MFT ran, or the driver
/// declined to allocate D3D surfaces.
fn convert_system_memory_yuv(
    sample: &IMFSample,
    format: OutputFormat,
    pixel_format: HwPixelFormat,
    timestamp_seconds: f64,
) -> Result<VideoFrame, VideoError> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| hresult_error("presentation: ConvertToContiguousBuffer", e))?;

        // Every read below is bounded by what the lock actually handed back.
        // `IMF2DBuffer2::Lock2DSize` reports the buffer's real extent next to
        // the pitch; a buffer without it is read through the plain contiguous
        // `Lock`, whose current length is the bound. (`IMF2DBuffer::Lock2D`
        // reports no length at all, so it is not used here.)
        let two_d = buffer.cast::<IMF2DBuffer2>().ok();
        let (base, pitch, available) = if let Some(two_d) = &two_d {
            let mut scanline0: *mut u8 = std::ptr::null_mut();
            let mut signed_pitch = 0i32;
            let mut buffer_start: *mut u8 = std::ptr::null_mut();
            let mut buffer_len = 0u32;
            two_d
                .Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scanline0,
                    &mut signed_pitch,
                    &mut buffer_start,
                    &mut buffer_len,
                )
                .map_err(|e| hresult_error("presentation: IMF2DBuffer2::Lock2DSize", e))?;
            (
                scanline0,
                signed_pitch as isize,
                bytes_after(buffer_start, buffer_len as usize, scanline0),
            )
        } else {
            let mut base: *mut u8 = std::ptr::null_mut();
            let mut locked_len = 0u32;
            buffer
                .Lock(&mut base, None, Some(&mut locked_len))
                .map_err(|e| hresult_error("presentation: IMFMediaBuffer::Lock", e))?;
            let pitch = format
                .abs_stride()
                .max(format.width as usize * pixel_format.bytes_per_sample());
            (base, pitch as isize, locked_len as usize)
        };

        let layout =
            semi_planar_layout(pixel_format, format.width, format.height, pitch, available);
        let result = match layout {
            Some(layout) if !base.is_null() => {
                // SAFETY: `semi_planar_layout` only succeeds when both planes
                // fit in the `available` bytes the lock reported from `base`.
                let planes = std::slice::from_raw_parts(base as *const u8, layout.total_len);
                let (luma, chroma) = planes.split_at(layout.luma_len);
                let (preview_width, preview_height) = format.preview_size();
                cpu_convert_to_bgra(
                    YuvPlanes {
                        luma,
                        luma_pitch: layout.pitch,
                        chroma,
                        chroma_pitch: layout.pitch,
                        format: pixel_format,
                        width: format.width,
                        height: format.height,
                    },
                    preview_width,
                    preview_height,
                    format.color,
                )
            }
            _ => Err(VideoError::Backend(format!(
                "presentation: unexpected {} layout {}x{} pitch={pitch} bytes={available}",
                pixel_format.name(),
                format.width,
                format.height
            ))),
        };

        if let Some(two_d) = &two_d {
            let _ = two_d.Unlock2D();
        } else {
            let _ = buffer.Unlock();
        }

        let bgra = result?;
        let (preview_width, preview_height) = format.preview_size();
        Ok(VideoFrame {
            width: preview_width,
            height: preview_height,
            timestamp_seconds,
            bgra,
        })
    }
}

/// Where the planes of a locked NV12/P010 frame sit, from its first scanline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemiPlanarLayout {
    /// Row pitch of both planes, in bytes.
    pitch: usize,
    /// Bytes of the full-height luma plane; the chroma plane starts here.
    luma_len: usize,
    /// Bytes of both planes together.
    total_len: usize,
}

/// Lays out an NV12/P010 frame at `pitch` and checks it against the
/// `available` bytes the lock reported from the first scanline.
///
/// NV12/P010 store the full-height luma plane followed by an interleaved chroma
/// plane of half the height at the same pitch. `None` when the frame does not
/// fit, when a row is narrower than the image, or when the pitch is not a
/// positive top-down pitch (walking forward from a bottom-up surface's first
/// scanline would leave the buffer).
fn semi_planar_layout(
    pixel_format: HwPixelFormat,
    width: u32,
    height: u32,
    pitch: isize,
    available: usize,
) -> Option<SemiPlanarLayout> {
    let pitch = usize::try_from(pitch).ok().filter(|pitch| *pitch > 0)?;
    let row_bytes = (width as usize).checked_mul(pixel_format.bytes_per_sample())?;
    if pitch < row_bytes {
        return None;
    }
    let height = height as usize;
    let luma_len = pitch.checked_mul(height)?;
    let chroma_len = pitch.checked_mul(height.div_ceil(2))?;
    let total_len = luma_len.checked_add(chroma_len)?;
    (total_len <= available).then_some(SemiPlanarLayout {
        pitch,
        luma_len,
        total_len,
    })
}

/// Whether `height` rows of `row_bytes`, starting at a scanline with `before`
/// readable bytes ahead of it and `after` from it on, and `step` bytes apart
/// (negative for a bottom-up surface), all lie inside the locked buffer.
fn rows_fit(step: isize, row_bytes: usize, height: usize, (before, after): (usize, usize)) -> bool {
    let pitch = step.unsigned_abs();
    if height == 0 {
        return true;
    }
    if pitch < row_bytes {
        return false;
    }
    let Some(span) = pitch.checked_mul(height - 1) else {
        return false;
    };
    if step >= 0 {
        span.checked_add(row_bytes)
            .is_some_and(|need| need <= after)
    } else {
        span <= before && row_bytes <= after
    }
}

/// Bytes from `from` to the end of the `len`-byte buffer at `start`, or zero
/// when `from` is not inside it.
fn bytes_after(start: *const u8, len: usize, from: *const u8) -> usize {
    let (start, from) = (start as usize, from as usize);
    match from.checked_sub(start) {
        Some(offset) if offset <= len => len - offset,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_semi_planar_frame_must_fit_the_locked_buffer() {
        // 4x4 NV12 at pitch 8: 32 luma bytes, then 2 chroma rows of 8.
        let exact = semi_planar_layout(HwPixelFormat::Nv12, 4, 4, 8, 48);
        assert_eq!(
            exact,
            Some(SemiPlanarLayout {
                pitch: 8,
                luma_len: 32,
                total_len: 48
            })
        );
        // One byte short of the chroma plane: the regression read past it.
        assert_eq!(semi_planar_layout(HwPixelFormat::Nv12, 4, 4, 8, 47), None);
        assert_eq!(semi_planar_layout(HwPixelFormat::Nv12, 4, 4, 8, 0), None);
    }

    #[test]
    fn odd_heights_round_the_chroma_plane_up() {
        // 3 luma rows need 2 chroma rows.
        let layout = semi_planar_layout(HwPixelFormat::Nv12, 4, 3, 4, 20).expect("fits");
        assert_eq!((layout.luma_len, layout.total_len), (12, 20));
        assert_eq!(semi_planar_layout(HwPixelFormat::Nv12, 4, 3, 4, 19), None);
    }

    #[test]
    fn p010_rows_need_two_bytes_per_sample() {
        // A pitch that is wide enough for NV12 is half a P010 row.
        assert_eq!(
            semi_planar_layout(HwPixelFormat::P010, 8, 2, 8, 1 << 20),
            None
        );
        let layout = semi_planar_layout(HwPixelFormat::P010, 8, 2, 16, 48).expect("fits");
        assert_eq!((layout.luma_len, layout.total_len), (32, 48));
    }

    #[test]
    fn bottom_up_zero_and_overflowing_pitches_are_rejected() {
        assert_eq!(
            semi_planar_layout(HwPixelFormat::Nv12, 4, 4, -8, 1 << 20),
            None
        );
        assert_eq!(
            semi_planar_layout(HwPixelFormat::Nv12, 4, 4, 0, 1 << 20),
            None
        );
        assert_eq!(
            semi_planar_layout(HwPixelFormat::Nv12, 4, u32::MAX, isize::MAX, usize::MAX),
            None
        );
    }

    #[test]
    fn bytes_after_measures_from_the_first_scanline() {
        let buffer = [0u8; 64];
        let start = buffer.as_ptr();
        assert_eq!(bytes_after(start, 64, start), 64);
        assert_eq!(bytes_after(start, 64, start.wrapping_add(16)), 48);
        assert_eq!(bytes_after(start, 64, start.wrapping_add(64)), 0);
        // Outside the buffer on either side: nothing is readable.
        assert_eq!(bytes_after(start, 64, start.wrapping_add(65)), 0);
        assert_eq!(bytes_after(start.wrapping_add(8), 56, start), 0);
    }

    #[test]
    fn rgb32_rows_must_fit_the_locked_buffer() {
        // 4 rows of 16 bytes at pitch 16: exactly 64 bytes.
        assert!(rows_fit(16, 16, 4, (0, 64)));
        assert!(!rows_fit(16, 16, 4, (0, 63)), "one byte short");
        // A padded pitch needs the padding only between rows, not after the
        // last one.
        assert!(rows_fit(20, 16, 4, (0, 76)));
        assert!(!rows_fit(20, 16, 4, (0, 75)));
        // Bottom-up: three rows lie before the top scanline.
        assert!(rows_fit(-16, 16, 4, (48, 16)));
        assert!(!rows_fit(-16, 16, 4, (47, 16)));
        assert!(!rows_fit(-16, 16, 4, (48, 15)));
        // A pitch narrower than a row, or one that overflows, never fits.
        assert!(!rows_fit(8, 16, 4, (0, usize::MAX)));
        assert!(!rows_fit(isize::MAX, 16, 4, (0, usize::MAX)));
        assert!(rows_fit(16, 16, 0, (0, 0)), "no rows, nothing to read");
    }

    #[test]
    fn frame_size_packing_round_trips() {
        assert_eq!(unpack_frame_size(pack_frame_size(3840, 2160)), (3840, 2160));
    }

    #[test]
    fn a_source_that_already_fits_is_not_rescaled() {
        assert_eq!(preview_frame_size(1280, 720), None);
        assert_eq!(preview_frame_size(640, 480), None);
        // Never upscales a small source.
        assert_eq!(preview_frame_size(320, 240), None);
    }

    #[test]
    fn oversized_sources_scale_down_keeping_aspect() {
        // 4K UHD, 16:9 -> capped long edge, height follows.
        assert_eq!(preview_frame_size(3840, 2160), Some((1280, 720)));
        // Portrait: the cap applies to the long edge, which is the height.
        assert_eq!(preview_frame_size(2160, 3840), Some((720, 1280)));
    }

    #[test]
    fn scaled_sizes_are_even() {
        for (width, height) in [(3840u32, 2158u32), (2000, 1333), (4096, 1717)] {
            let (scaled_width, scaled_height) =
                preview_frame_size(width, height).expect("oversized");
            assert_eq!(scaled_width % 2, 0, "{width}x{height} width");
            assert_eq!(scaled_height % 2, 0, "{width}x{height} height");
            assert!(scaled_width.max(scaled_height) <= PREVIEW_MAX_EDGE);
        }
    }

    #[test]
    fn degenerate_sizes_do_not_scale() {
        assert_eq!(preview_frame_size(0, 0), None);
        assert_eq!(preview_frame_size(3840, 0), None);
    }

    /// The hardware path must never name RGB32 — asking a DXVA decoder for RGB
    /// is what produced `0xC00D5212`.
    #[test]
    fn hardware_output_candidates_never_include_rgb32() {
        assert_eq!(subtype_name(&MFVideoFormat_NV12), "NV12");
        assert_eq!(subtype_name(&MFVideoFormat_P010), "P010");
        assert_ne!(subtype_name(&MFVideoFormat_NV12), "RGB32");
        assert_ne!(subtype_name(&MFVideoFormat_P010), "RGB32");
    }

    #[test]
    fn output_kinds_report_the_subtype_they_negotiated() {
        assert_eq!(
            OutputKind::Hardware(HwPixelFormat::Nv12).subtype_name(),
            "NV12"
        );
        assert_eq!(
            OutputKind::Hardware(HwPixelFormat::P010).subtype_name(),
            "P010"
        );
        assert_eq!(OutputKind::SoftwareRgb32.subtype_name(), "RGB32");
    }
}
