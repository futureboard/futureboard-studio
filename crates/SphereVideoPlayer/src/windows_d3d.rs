//! D3D11 side of the Windows video backend: the shared video device handed to
//! the Source Reader, the GPU colour-convert/downscale stage, and the CPU
//! converter used when a sample arrives in system memory anyway.
//!
//! [`windows_mf`](crate::windows_mf) owns decoding; everything that touches a
//! D3D11 device, a video processor, or raw YUV bytes lives here.
//!
//! ## Why a video processor and not a shader
//!
//! The decoder emits NV12 (or P010 for 10-bit) `ID3D11Texture2D` surfaces. The
//! preview wants packed BGRA at preview size. `ID3D11VideoProcessor` does the
//! YUV->RGB conversion, the chroma upsample and the downscale in one blit, on
//! the fixed-function video hardware, with the driver honouring the stream's
//! declared matrix and range. Doing it with a pixel shader would mean shipping
//! compiled bytecode and reimplementing colour handling by hand.
//!
//! ## Readback
//!
//! Only the *preview-size* BGRA result is copied back to system memory. The
//! full-size decoded surface never crosses the bus, which is what keeps a 4K
//! source affordable: a 3840x2160 readback is 33 MB per frame, a 1280x720 one
//! is 3.7 MB.
//!
//! ## Threading
//!
//! [`shared_video_device`] hands out one process-wide device. It is created
//! with `ID3D11Multithread::SetMultithreadProtected(true)` before it is
//! published, so several preview workers can submit to it and D3D serializes
//! them. [`FrameConverter`] is not itself synchronized and stays owned by the
//! single decoder thread that built it.

use std::sync::{Arc, OnceLock};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::core::Interface;

use crate::VideoError;

/// Pixel format of a hardware decoder surface.
///
/// Both are semi-planar: a full-resolution luma plane followed by an
/// interleaved Cb/Cr plane at half width and half height. P010 stores each
/// sample as a 16-bit little-endian word with the 10 significant bits in the
/// high end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HwPixelFormat {
    Nv12,
    P010,
}

impl HwPixelFormat {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12",
            Self::P010 => "P010",
        }
    }

    /// Bytes per luma sample in system memory.
    pub(crate) fn bytes_per_sample(self) -> usize {
        match self {
            Self::Nv12 => 1,
            Self::P010 => 2,
        }
    }
}

/// How to read the YUV samples of a stream back as RGB.
///
/// Defaults follow the conventional reading of the frame height, and
/// `windows_mf` overwrites either field when the media type declares one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColorInfo {
    /// BT.709 coefficients when true, BT.601 when false.
    pub(crate) bt709: bool,
    /// Studio swing (Y 16-235, C 16-240) when true, full 0-255 when false.
    pub(crate) studio_range: bool,
}

impl ColorInfo {
    /// SD material is BT.601, HD and above is BT.709. Both are studio range
    /// unless the stream says otherwise — full-range video exists but is the
    /// exception, and guessing full range on studio content crushes blacks.
    pub(crate) fn assume_from_height(height: u32) -> Self {
        Self {
            bt709: height > 576,
            studio_range: true,
        }
    }

    /// `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` is a bitfield in the generated
    /// bindings, so pack it by hand:
    ///
    /// ```text
    /// bit  0     Usage          0 = playback, 1 = video processing
    /// bit  1     RGB_Range      0 = full (0-255), 1 = studio (16-235)
    /// bit  2     YCbCr_Matrix   0 = BT.601, 1 = BT.709
    /// bit  3     YCbCr_xvYCC    0 = conventional
    /// bits 4-5   Nominal_Range  1 = 16-235, 2 = 0-255
    /// ```
    fn to_input_color_space(self) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        let matrix = u32::from(self.bt709) << 2;
        let nominal = if self.studio_range { 1 } else { 2 } << 4;
        D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: matrix | nominal,
        }
    }

    /// The preview surface is always full-range BGRA.
    fn output_color_space() -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 2 << 4 }
    }
}

/// The process-wide D3D11 device the Source Reader decodes onto.
pub(crate) struct VideoDevice {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    manager: IMFDXGIDeviceManager,
}

// SAFETY: the device is created multithread-protected via `ID3D11Multithread`
// before this wrapper is built, which is exactly the condition under which
// D3D11 and the DXGI device manager document concurrent use from several
// threads. Nothing here hands out interior mutability of its own; every method
// forwards to a COM object that serializes internally.
unsafe impl Send for VideoDevice {}
unsafe impl Sync for VideoDevice {}

impl VideoDevice {
    /// The manager the Source Reader needs in `MF_SOURCE_READER_D3D_MANAGER`.
    pub(crate) fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }
}

/// The shared video device, or `None` on a machine with no usable D3D11 video
/// device (no GPU, a remote session, a driver that refuses `VIDEO_SUPPORT`).
///
/// Callers fall back to the software Source Reader configuration rather than
/// failing to open the file: a soft picture monitor beats no picture monitor.
pub(crate) fn shared_video_device() -> Option<Arc<VideoDevice>> {
    static DEVICE: OnceLock<Option<Arc<VideoDevice>>> = OnceLock::new();

    DEVICE
        .get_or_init(|| {
            let device = create_video_device();
            if device.is_none() {
                eprintln!("[video-mf] no D3D11 video device; decoding reference video in software");
            }
            device.map(Arc::new)
        })
        .clone()
}

fn create_video_device() -> Option<VideoDevice> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // `VIDEO_SUPPORT` is what makes the device usable by a decoder MFT and
        // by the video processor; `BGRA_SUPPORT` lets the processor write the
        // BGRA preview surface directly.
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .ok()?;
        let device = device?;
        let context = context?;

        // Media Foundation drives this device from its own decoder threads, so
        // it must be multithread-protected before the manager publishes it.
        let multithread: ID3D11Multithread = device.cast().ok()?;
        // Returns the *previous* setting, not a status — nothing to check.
        let _ = multithread.SetMultithreadProtected(true);

        let video_device: ID3D11VideoDevice = device.cast().ok()?;
        let video_context: ID3D11VideoContext = context.cast().ok()?;

        let mut reset_token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager).ok()?;
        let manager = manager?;
        manager.ResetDevice(&device, reset_token).ok()?;

        Some(VideoDevice {
            device,
            context,
            video_device,
            video_context,
            manager,
        })
    }
}

/// GPU stage that turns one decoder surface into preview-size BGRA bytes.
///
/// Built for an exact (format, source size, output size) combination and
/// rebuilt by the caller when any of them changes — see [`FrameConverter::matches`].
pub(crate) struct FrameConverter {
    device: Arc<VideoDevice>,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Render target the processor writes BGRA into.
    output: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    /// CPU-readable copy of `output`. Kept alive across frames so the readback
    /// allocates nothing per frame.
    staging: ID3D11Texture2D,
    format: HwPixelFormat,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    /// Reused BGRA scratch, sized `output_width * output_height * 4`.
    pixels: Vec<u8>,
}

impl FrameConverter {
    pub(crate) fn new(
        device: Arc<VideoDevice>,
        format: HwPixelFormat,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
        color: ColorInfo,
    ) -> Result<Self, VideoError> {
        if source_width == 0 || source_height == 0 || output_width == 0 || output_height == 0 {
            return Err(VideoError::Backend(format!(
                "gpu conversion: degenerate size {source_width}x{source_height} -> \
                 {output_width}x{output_height}"
            )));
        }

        unsafe {
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: source_width,
                InputHeight: source_height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: output_width,
                OutputHeight: output_height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = device
                .video_device
                .CreateVideoProcessorEnumerator(&content)
                .map_err(|e| backend_error("CreateVideoProcessorEnumerator", e))?;
            let processor = device
                .video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| backend_error("CreateVideoProcessor", e))?;

            // Declare what the decoder is handing over and what the preview
            // expects, and let the driver do the conversion. Guessing here is
            // what produces washed-out or crushed picture.
            device.video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &color.to_input_color_space(),
            );
            device
                .video_context
                .VideoProcessorSetOutputColorSpace(&processor, &ColorInfo::output_color_space());

            let output = create_texture(
                &device.device,
                output_width,
                output_height,
                D3D11_USAGE_DEFAULT,
                D3D11_BIND_RENDER_TARGET.0 as u32,
                0,
                "video processor output",
            )?;
            let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            device
                .video_device
                .CreateVideoProcessorOutputView(
                    &output,
                    &enumerator,
                    &output_view_desc,
                    Some(&mut output_view),
                )
                .map_err(|e| backend_error("CreateVideoProcessorOutputView", e))?;
            let output_view = output_view.ok_or_else(|| {
                VideoError::Backend("gpu conversion: null video processor output view".into())
            })?;

            let staging = create_texture(
                &device.device,
                output_width,
                output_height,
                D3D11_USAGE_STAGING,
                0,
                D3D11_CPU_ACCESS_READ.0 as u32,
                "video readback staging",
            )?;

            Ok(Self {
                device,
                enumerator,
                processor,
                output,
                output_view,
                staging,
                format,
                source_width,
                source_height,
                output_width,
                output_height,
                pixels: vec![0; output_width as usize * output_height as usize * 4],
            })
        }
    }

    /// True when this converter was built for exactly this conversion, so the
    /// caller can keep it instead of rebuilding the processor per frame.
    pub(crate) fn matches(
        &self,
        format: HwPixelFormat,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> bool {
        self.format == format
            && self.source_width == source_width
            && self.source_height == source_height
            && self.output_width == output_width
            && self.output_height == output_height
    }

    pub(crate) fn output_size(&self) -> (u32, u32) {
        (self.output_width, self.output_height)
    }

    /// Blits `texture` through the video processor and reads the preview-size
    /// BGRA result back.
    ///
    /// `subresource` is the array slice inside the decoder's texture array;
    /// DXVA hands out one surface of a shared array per frame.
    pub(crate) fn convert(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
    ) -> Result<Vec<u8>, VideoError> {
        unsafe {
            // The input view is bound to one array slice, and the slice changes
            // from frame to frame, so this one cannot be cached with the rest.
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: subresource,
                    },
                },
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            self.device
                .video_device
                .CreateVideoProcessorInputView(
                    texture,
                    &self.enumerator,
                    &input_desc,
                    Some(&mut input_view),
                )
                .map_err(|e| backend_error("CreateVideoProcessorInputView", e))?;
            let input_view = input_view.ok_or_else(|| {
                VideoError::Backend("gpu conversion: null video processor input view".into())
            })?;

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                ..Default::default()
            };
            let blt = self.device.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            );
            // `pInputSurface` is `ManuallyDrop`, so the view it holds has to be
            // released explicitly whether or not the blit succeeded.
            let mut stream = stream;
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            blt.map_err(|e| backend_error("VideoProcessorBlt", e))?;

            // Only the preview-size result crosses the bus.
            self.device
                .context
                .CopyResource(&self.staging, &self.output);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.device
                .context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| backend_error("Map readback staging texture", e))?;

            let row_bytes = self.output_width as usize * 4;
            let pitch = mapped.RowPitch as usize;
            if mapped.pData.is_null() || pitch < row_bytes {
                self.device.context.Unmap(&self.staging, 0);
                return Err(VideoError::Backend(format!(
                    "gpu conversion: unexpected readback pitch {pitch} for {row_bytes}-byte rows"
                )));
            }
            // D3D11 guarantees `RowPitch` bytes between rows but only
            // `row_bytes` in the last one; a length of `pitch * height` would
            // run past a mapping whose last row is not padded.
            let height = self.output_height as usize;
            let mapped_len = match height {
                0 => 0,
                h => pitch * (h - 1) + row_bytes,
            };
            // SAFETY: the mapping holds `pitch` bytes per row for every row
            // but the last, which holds at least `row_bytes`; it stays mapped
            // until `Unmap` below, and nothing else writes it meanwhile.
            let source = std::slice::from_raw_parts(mapped.pData as *const u8, mapped_len);
            for row in 0..self.output_height as usize {
                let from = row * pitch;
                let to = row * row_bytes;
                self.pixels[to..to + row_bytes].copy_from_slice(&source[from..from + row_bytes]);
            }
            self.device.context.Unmap(&self.staging, 0);

            Ok(self.pixels.clone())
        }
    }
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    bind_flags: u32,
    cpu_access: u32,
    label: &str,
) -> Result<ID3D11Texture2D, VideoError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind_flags,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| backend_error(&format!("CreateTexture2D ({label})"), e))?;
    }
    texture.ok_or_else(|| VideoError::Backend(format!("gpu conversion: null {label} texture")))
}

fn backend_error(stage: &str, error: windows::core::Error) -> VideoError {
    VideoError::Backend(format!(
        "gpu conversion: {stage} failed: {} (0x{:08X})",
        error.message(),
        error.code().0
    ))
}

/// One decoded frame's planes as they sit in system memory.
///
/// Both planes are borrowed from the locked Media Foundation buffer, so this
/// only lives for the duration of the lock.
pub(crate) struct YuvPlanes<'a> {
    pub(crate) luma: &'a [u8],
    pub(crate) luma_pitch: usize,
    pub(crate) chroma: &'a [u8],
    pub(crate) chroma_pitch: usize,
    pub(crate) format: HwPixelFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// CPU fallback: NV12/P010 in system memory -> packed BGRA at preview size.
///
/// Reached when the reader negotiated a hardware format but the sample arrived
/// without an `IMFDXGIBuffer`, or when the GPU stage could not be built on this
/// driver. Sampling is nearest-neighbour: this is a reference monitor, the
/// alternative is no picture, and a bilinear pass here would cost more than the
/// decode it is standing in for.
pub(crate) fn cpu_convert_to_bgra(
    planes: YuvPlanes<'_>,
    output_width: u32,
    output_height: u32,
    color: ColorInfo,
) -> Result<Vec<u8>, VideoError> {
    if planes.width == 0 || planes.height == 0 || output_width == 0 || output_height == 0 {
        return Err(VideoError::Backend(format!(
            "cpu conversion: degenerate size {}x{} -> {output_width}x{output_height}",
            planes.width, planes.height
        )));
    }

    let sample_bytes = planes.format.bytes_per_sample();
    let source_width = planes.width as usize;
    let source_height = planes.height as usize;
    // Chroma is half resolution in both axes, and each chroma site holds an
    // interleaved Cb/Cr pair.
    let chroma_height = source_height.div_ceil(2);

    if planes.luma_pitch < source_width * sample_bytes
        || planes.luma.len() < planes.luma_pitch * source_height
        || planes.chroma_pitch < source_width * sample_bytes
        || planes.chroma.len() < planes.chroma_pitch * chroma_height
    {
        return Err(VideoError::Backend(format!(
            "cpu conversion: {} plane too small for {}x{} (luma {} bytes at pitch {}, chroma {} \
             bytes at pitch {})",
            planes.format.name(),
            planes.width,
            planes.height,
            planes.luma.len(),
            planes.luma_pitch,
            planes.chroma.len(),
            planes.chroma_pitch,
        )));
    }

    let coefficients = YuvCoefficients::new(color);
    let mut bgra = vec![0u8; output_width as usize * output_height as usize * 4];

    for out_y in 0..output_height as usize {
        let src_y = out_y * source_height / output_height as usize;
        let luma_row = src_y * planes.luma_pitch;
        let chroma_row = (src_y / 2) * planes.chroma_pitch;
        let out_row = out_y * output_width as usize * 4;

        for out_x in 0..output_width as usize {
            let src_x = out_x * source_width / output_width as usize;
            let luma = read_sample(planes.luma, luma_row + src_x * sample_bytes, sample_bytes);
            // Chroma pairs are Cb then Cr, so a chroma site starts at twice the
            // half-resolution column.
            let chroma_index = chroma_row + (src_x / 2) * 2 * sample_bytes;
            let cb = read_sample(planes.chroma, chroma_index, sample_bytes);
            let cr = read_sample(planes.chroma, chroma_index + sample_bytes, sample_bytes);

            let (b, g, r) = coefficients.to_bgr(luma, cb, cr);
            let pixel = out_row + out_x * 4;
            bgra[pixel] = b;
            bgra[pixel + 1] = g;
            bgra[pixel + 2] = r;
            bgra[pixel + 3] = 255;
        }
    }

    Ok(bgra)
}

/// Reads one luma/chroma sample as 0-255, whatever the source depth.
///
/// P010 stores 10 bits left-aligned in a 16-bit little-endian word, so the high
/// byte already *is* the 8-bit value.
fn read_sample(plane: &[u8], index: usize, sample_bytes: usize) -> f32 {
    match sample_bytes {
        1 => plane.get(index).copied().unwrap_or(0) as f32,
        _ => plane.get(index + 1).copied().unwrap_or(0) as f32,
    }
}

/// Matrix and range applied to every pixel, resolved once per frame.
struct YuvCoefficients {
    /// Scale applied to `Y - luma_offset`.
    luma_scale: f32,
    luma_offset: f32,
    /// Scale applied to `C - 128`.
    chroma_scale: f32,
    r_cr: f32,
    g_cb: f32,
    g_cr: f32,
    b_cb: f32,
}

impl YuvCoefficients {
    fn new(color: ColorInfo) -> Self {
        // Studio swing puts luma in 16-235 and chroma in 16-240; full range
        // uses the whole byte and needs no expansion.
        let (luma_scale, luma_offset, chroma_scale) = if color.studio_range {
            (255.0 / 219.0, 16.0, 255.0 / 224.0)
        } else {
            (1.0, 0.0, 1.0)
        };
        let (r_cr, g_cb, g_cr, b_cb) = if color.bt709 {
            (1.5748, -0.1873, -0.4681, 1.8556)
        } else {
            (1.402, -0.344136, -0.714136, 1.772)
        };
        Self {
            luma_scale,
            luma_offset,
            chroma_scale,
            r_cr,
            g_cb,
            g_cr,
            b_cb,
        }
    }

    fn to_bgr(&self, luma: f32, cb: f32, cr: f32) -> (u8, u8, u8) {
        let y = (luma - self.luma_offset) * self.luma_scale;
        let u = (cb - 128.0) * self.chroma_scale;
        let v = (cr - 128.0) * self.chroma_scale;
        let r = y + self.r_cr * v;
        let g = y + self.g_cb * u + self.g_cr * v;
        let b = y + self.b_cb * u;
        (clamp_u8(b), clamp_u8(g), clamp_u8(r))
    }
}

fn clamp_u8(value: f32) -> u8 {
    value.clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12(width: usize, height: usize, luma: u8, cb: u8, cr: u8) -> (Vec<u8>, Vec<u8>) {
        let luma_plane = vec![luma; width * height];
        let mut chroma_plane = vec![0u8; width * height.div_ceil(2)];
        for pair in chroma_plane.as_chunks_mut::<2>().0 {
            pair[0] = cb;
            pair[1] = cr;
        }
        (luma_plane, chroma_plane)
    }

    fn convert(width: usize, height: usize, luma: u8, cb: u8, cr: u8, color: ColorInfo) -> Vec<u8> {
        let (luma_plane, chroma_plane) = nv12(width, height, luma, cb, cr);
        cpu_convert_to_bgra(
            YuvPlanes {
                luma: &luma_plane,
                luma_pitch: width,
                chroma: &chroma_plane,
                chroma_pitch: width,
                format: HwPixelFormat::Nv12,
                width: width as u32,
                height: height as u32,
            },
            width as u32,
            height as u32,
            color,
        )
        .expect("conversion succeeds")
    }

    #[test]
    fn studio_black_and_white_map_to_full_range() {
        let color = ColorInfo {
            bt709: true,
            studio_range: true,
        };
        let black = convert(4, 4, 16, 128, 128, color);
        assert_eq!(&black[0..4], &[0, 0, 0, 255]);
        let white = convert(4, 4, 235, 128, 128, color);
        assert_eq!(&white[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn full_range_grey_is_not_expanded() {
        let color = ColorInfo {
            bt709: true,
            studio_range: false,
        };
        let grey = convert(2, 2, 128, 128, 128, color);
        assert_eq!(&grey[0..4], &[128, 128, 128, 255]);
    }

    #[test]
    fn out_of_gamut_chroma_is_clamped_not_wrapped() {
        let color = ColorInfo {
            bt709: false,
            studio_range: true,
        };
        // Maximum red: full Cr, minimum Cb.
        let red = convert(2, 2, 235, 16, 240, color);
        assert_eq!(red[2], 255);
        assert_eq!(red[3], 255);
    }

    #[test]
    fn nearest_neighbour_downscale_keeps_frame_size() {
        let color = ColorInfo::assume_from_height(1080);
        let (luma_plane, chroma_plane) = nv12(64, 32, 100, 128, 128);
        let bgra = cpu_convert_to_bgra(
            YuvPlanes {
                luma: &luma_plane,
                luma_pitch: 64,
                chroma: &chroma_plane,
                chroma_pitch: 64,
                format: HwPixelFormat::Nv12,
                width: 64,
                height: 32,
            },
            16,
            8,
            color,
        )
        .expect("conversion succeeds");
        assert_eq!(bgra.len(), 16 * 8 * 4);
    }

    #[test]
    fn a_short_plane_is_reported_not_read_past() {
        let color = ColorInfo::assume_from_height(480);
        let luma_plane = vec![0u8; 8];
        let chroma_plane = vec![0u8; 8];
        let result = cpu_convert_to_bgra(
            YuvPlanes {
                luma: &luma_plane,
                luma_pitch: 64,
                chroma: &chroma_plane,
                chroma_pitch: 64,
                format: HwPixelFormat::Nv12,
                width: 64,
                height: 32,
            },
            16,
            8,
            color,
        );
        assert!(matches!(result, Err(VideoError::Backend(_))));
    }

    #[test]
    fn colour_defaults_follow_frame_height() {
        assert!(!ColorInfo::assume_from_height(480).bt709);
        assert!(ColorInfo::assume_from_height(1080).bt709);
        assert!(ColorInfo::assume_from_height(1080).studio_range);
    }

    #[test]
    fn colour_space_bitfields_match_the_documented_layout() {
        let studio_709 = ColorInfo {
            bt709: true,
            studio_range: true,
        }
        .to_input_color_space();
        // YCbCr_Matrix = 1 (bit 2), Nominal_Range = 1 (bits 4-5).
        assert_eq!(studio_709._bitfield, (1 << 2) | (1 << 4));
        let full_601 = ColorInfo {
            bt709: false,
            studio_range: false,
        }
        .to_input_color_space();
        // YCbCr_Matrix = 0, Nominal_Range = 2.
        assert_eq!(full_601._bitfield, 2 << 4);
    }
}
