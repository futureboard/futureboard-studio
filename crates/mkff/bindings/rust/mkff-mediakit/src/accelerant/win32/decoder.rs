//! Hardware video decode: a Media Foundation decoder MFT given the accelerant's D3D11 device
//! (`MFT_MESSAGE_SET_D3D_MANAGER`), so it decodes with DXVA on that GPU and outputs D3D11
//! textures.
//!
//! Two output modes:
//! - CPU (default): each frame's planes are copied out (NV12 / P010) and the decoder's surface
//!   is returned to it at once.
//! - GPU (`RawVideoFormat::gpu` requested): frames carry the texture as a [`GpuSurface`] and no
//!   CPU copy. The decoder owns a fixed pool of surfaces (at least [`GPU_POOL`] are requested);
//!   while every one of them is held by frames in flight the decoder cannot output and
//!   `decode` / `flush` block, so consumers must drop frames (or their `GpuSurface`) promptly —
//!   the same contract as BeOS overlay buffers.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use ::windows::core::Interface;
use ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use ::windows::Win32::Media::MediaFoundation::*;

use super::mf::{self, Runtime};
use super::Device;
use crate::addon::{Acceleration, VideoDecoderPlugin, VideoFrame};
use crate::buffer::GpuSurface;
use crate::format::{CodecFamily, MediaFormat, PixelFormat, RawVideoFormat};
use crate::{Error, Result};

/// Output surfaces requested from the decoder in GPU mode, beyond what it needs for reference
/// frames.
pub const GPU_POOL: u32 = 16;

fn os(what: &str, e: ::windows::core::Error) -> Error {
    Error::Codec(format!("{what}: {e}"))
}

/// Keeps a decoded sample's texture alive while a [`GpuSurface`] refers to it.
struct SampleKeep {
    _sample: IMFSample,
    texture: ID3D11Texture2D,
}
// SAFETY: D3D11 textures and MF samples are free-threaded COM objects.
unsafe impl Send for SampleKeep {}
unsafe impl Sync for SampleKeep {}

/// The D3D11 texture and array slice behind a surface decoded by an accelerant.
pub fn texture_of(s: &GpuSurface) -> Option<(ID3D11Texture2D, u32)> {
    let k = s.keep_alive.as_ref()?.downcast_ref::<SampleKeep>()?;
    Some((k.texture.clone(), s.array_index))
}

/// How packets must be rewritten for the MFT (which wants Annex-B for H.264 / HEVC).
enum Framing {
    /// Pass through (Annex-B, VPx, AV1, MPEG-2); `prefix` goes in front of the first packet.
    Raw,
    /// MP4 / Matroska length-prefixed NAL units → Annex-B.
    LengthPrefixed(usize),
}

pub(crate) struct MfVideoDecoder {
    mft: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    _dev: Arc<Device>,
    luid: u64,
    accel: Acceleration,
    codec: CodecFamily,
    gpu_output: bool,
    framing: Framing,
    /// Parameter sets / sequence header put in front of the first packet after open / reset.
    prefix: Vec<u8>,
    primed: bool,
    provides_samples: bool,
    out_size: u32,
    out: RawVideoFormat,
    /// Coded (allocated) height: where the chroma plane starts in a locked buffer.
    coded_height: u32,
    _rt: Runtime,
}

// SAFETY: the MFT and device manager are used from one thread at a time (the plugin is `Send`,
// not `Sync`), in the multithreaded apartment.
unsafe impl Send for MfVideoDecoder {}

impl MfVideoDecoder {
    pub(crate) fn open(
        dev: Arc<Device>,
        luid: u64,
        accel: Acceleration,
        format: &MediaFormat,
        gpu_output: bool,
    ) -> Result<Self> {
        let MediaFormat::EncodedVideo(enc) = format else {
            return Err(Error::BadFormat(format!("{format} is not encoded video")));
        };
        let rt = Runtime::start();
        let codec = enc.codec;
        let sub = mf::subtype(codec).ok_or_else(|| {
            Error::Unsupported(format!("no Media Foundation subtype for {codec}"))
        })?;
        let (framing, prefix) = framing(codec, &enc.config)?;
        let mut last = Error::Unsupported(format!("no Media Foundation {codec} decoder"));
        for act in mf::decoders(codec) {
            let name = unsafe { mf::friendly_name(&act) };
            match unsafe { Self::configure(&act, &dev, sub, enc.a, enc.b) } {
                Ok((mft, manager)) => {
                    log::debug!("hardware {codec} decode: {name} on adapter {luid:#x}");
                    let mut d = MfVideoDecoder {
                        mft,
                        _manager: manager,
                        _dev: dev,
                        luid,
                        accel,
                        codec,
                        gpu_output,
                        framing,
                        prefix,
                        primed: false,
                        provides_samples: true,
                        out_size: 0,
                        out: RawVideoFormat {
                            width: enc.a,
                            height: enc.b,
                            format: PixelFormat::Nv12,
                            fps: (0, 0),
                            gpu: true,
                        },
                        coded_height: enc.b,
                        _rt: rt,
                    };
                    if gpu_output {
                        unsafe {
                            if let Ok(a) = d.mft.GetOutputStreamAttributes(0) {
                                let _ = a.SetUINT32(&MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT, GPU_POOL);
                            }
                        }
                    }
                    // decoders that know no output type before the first SPS report it later
                    let _ = d.negotiate_output();
                    unsafe {
                        d.mft
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                            .map_err(|e| os("begin streaming", e))?;
                        d.mft
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                            .map_err(|e| os("start of stream", e))?;
                    }
                    return Ok(d);
                }
                Err(e) => {
                    log::debug!("{name}: {e}");
                    last = e;
                }
            }
        }
        Err(last)
    }

    unsafe fn configure(
        act: &IMFActivate,
        dev: &Device,
        sub: ::windows::core::GUID,
        width: u32,
        height: u32,
    ) -> Result<(IMFTransform, IMFDXGIDeviceManager)> {
        let mft: IMFTransform = act
            .ActivateObject()
            .map_err(|e| os("activate decoder", e))?;
        let attrs = mft
            .GetAttributes()
            .map_err(|e| os("decoder attributes", e))?;
        if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
            let _ = act.ShutdownObject();
            return Err(Error::Unsupported(
                "decoder is not D3D11-aware (software only)".into(),
            ));
        }
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)
            .map_err(|e| os("DXGI device manager", e))?;
        let manager = manager.ok_or_else(|| Error::Os("no DXGI device manager".into()))?;
        manager
            .ResetDevice(&dev.device, token)
            .map_err(|e| os("ResetDevice", e))?;
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(|e| os("decoder refused the D3D11 device", e))?;
        let t = MFCreateMediaType().map_err(|e| os("media type", e))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| os("input type", e))?;
        t.SetGUID(&MF_MT_SUBTYPE, &sub)
            .map_err(|e| os("input type", e))?;
        if width > 0 && height > 0 {
            t.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )
            .map_err(|e| os("input type", e))?;
        }
        mft.SetInputType(0, &t, 0)
            .map_err(|e| os("decoder rejected the stream type", e))?;
        Ok((mft, manager))
    }

    /// Pick NV12 / P010 output (first offered) and read the picture geometry.
    fn negotiate_output(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            loop {
                let t = match self.mft.GetOutputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                    Err(e) => return Err(os("output types", e)),
                };
                i += 1;
                let sub = t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
                let fmt = if sub == MFVideoFormat_NV12 {
                    PixelFormat::Nv12
                } else if sub == MFVideoFormat_P010 {
                    PixelFormat::P010
                } else {
                    continue;
                };
                self.mft
                    .SetOutputType(0, &t, 0)
                    .map_err(|e| os("set output type", e))?;
                let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                let (cw, ch) = ((size >> 32) as u32, size as u32);
                let mut area = [0u8; 16];
                let (dw, dh) = if t
                    .GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, None)
                    .is_ok()
                {
                    let w = i32::from_le_bytes([area[8], area[9], area[10], area[11]]);
                    let h = i32::from_le_bytes([area[12], area[13], area[14], area[15]]);
                    if w > 0 && h > 0 {
                        (w as u32, h as u32)
                    } else {
                        (cw, ch)
                    }
                } else {
                    (cw, ch)
                };
                self.out.width = dw;
                self.out.height = dh;
                self.out.format = fmt;
                self.coded_height = ch.max(dh);
                let info = self
                    .mft
                    .GetOutputStreamInfo(0)
                    .map_err(|e| os("output stream info", e))?;
                self.provides_samples = info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                    != 0;
                self.out_size = info.cbSize;
                return Ok(());
            }
        }
        Err(Error::Unsupported(format!(
            "{} decoder offers neither NV12 nor P010",
            self.codec
        )))
    }

    fn sample(&self, data: &[u8], pts: i64) -> Result<IMFSample> {
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len().max(1) as u32)
                .map_err(|e| os("input buffer", e))?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)
                .map_err(|e| os("lock input", e))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            let _ = buf.Unlock();
            buf.SetCurrentLength(data.len() as u32)
                .map_err(|e| os("input length", e))?;
            let s = MFCreateSample().map_err(|e| os("input sample", e))?;
            s.AddBuffer(&buf).map_err(|e| os("input sample", e))?;
            s.SetSampleTime(pts.saturating_mul(10))
                .map_err(|e| os("sample time", e))?;
            Ok(s)
        }
    }

    /// One decoded frame, or `None` when the MFT needs more input.
    fn pull(&mut self) -> Result<Option<VideoFrame>> {
        for _ in 0..4 {
            let alloc = if self.provides_samples {
                None
            } else {
                Some(self.output_sample()?)
            };
            let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(alloc),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = unsafe { self.mft.ProcessOutput(0, &mut bufs, &mut status) };
            let sample = unsafe { ManuallyDrop::take(&mut bufs[0].pSample) };
            drop(unsafe { ManuallyDrop::take(&mut bufs[0].pEvents) });
            match r {
                Ok(()) => {
                    let Some(sample) = sample else {
                        return Ok(None);
                    };
                    return self.frame(sample).map(Some);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.negotiate_output()?,
                Err(e) => return Err(os("ProcessOutput", e)),
            }
        }
        Err(Error::Codec(
            "decoder keeps changing its output type".into(),
        ))
    }

    fn output_sample(&self) -> Result<IMFSample> {
        unsafe {
            let buf =
                MFCreateMemoryBuffer(self.out_size.max(1)).map_err(|e| os("output buffer", e))?;
            let s = MFCreateSample().map_err(|e| os("output sample", e))?;
            s.AddBuffer(&buf).map_err(|e| os("output sample", e))?;
            Ok(s)
        }
    }

    fn frame(&self, sample: IMFSample) -> Result<VideoFrame> {
        let pts = unsafe { sample.GetSampleTime() }.unwrap_or(0) / 10;
        let buffer = unsafe { sample.GetBufferByIndex(0) }.map_err(|e| os("output buffer", e))?;
        let (w, h, fmt) = (self.out.width, self.out.height, self.out.format);
        let mut frame = VideoFrame {
            width: w,
            height: h,
            format: fmt,
            pts,
            planes: Vec::new(),
            strides: Vec::new(),
            gpu: None,
        };
        let mut texture = None;
        if let Ok(dx) = buffer.cast::<IMFDXGIBuffer>() {
            unsafe {
                let mut raw = std::ptr::null_mut();
                if dx.GetResource(&ID3D11Texture2D::IID, &mut raw).is_ok() && !raw.is_null() {
                    let tex = ID3D11Texture2D::from_raw(raw);
                    let index = dx.GetSubresourceIndex().unwrap_or(0);
                    texture = Some((tex, index));
                }
            }
        }
        if !self.gpu_output {
            // CPU mode: release the decoder's surface as soon as the planes are copied
            texture = texture.filter(|_| false);
        }
        if !self.gpu_output || texture.is_none() {
            let bpp = if fmt == PixelFormat::P010 { 2 } else { 1 };
            let row = w as usize * bpp;
            let crow = (w as usize).div_ceil(2) * 2 * bpp;
            let ch = (h as usize).div_ceil(2);
            let mut y = vec![0u8; row * h as usize];
            let mut uv = vec![0u8; crow * ch];
            unsafe {
                if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                    let mut scan0 = std::ptr::null_mut();
                    let mut pitch = 0i32;
                    b2.Lock2D(&mut scan0, &mut pitch)
                        .map_err(|e| os("Lock2D", e))?;
                    let pitch = pitch.unsigned_abs() as usize;
                    copy_planes(
                        scan0,
                        pitch,
                        self.coded_height as usize,
                        h as usize,
                        row,
                        crow,
                        &mut y,
                        &mut uv,
                    );
                    let _ = b2.Unlock2D();
                } else {
                    let mut p = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer
                        .Lock(&mut p, None, Some(&mut len))
                        .map_err(|e| os("lock output", e))?;
                    let pitch = row.max(1);
                    let need = pitch * (self.coded_height as usize + ch);
                    if (len as usize) >= need.min(pitch * (h as usize + ch)) {
                        copy_planes(
                            p,
                            pitch,
                            self.coded_height as usize,
                            h as usize,
                            row,
                            crow,
                            &mut y,
                            &mut uv,
                        );
                    }
                    let _ = buffer.Unlock();
                }
            }
            frame.planes = vec![y, uv];
            frame.strides = vec![row, crow];
        }
        if let Some((tex, index)) = texture {
            frame.gpu = Some(GpuSurface {
                adapter_luid: self.luid,
                shared_handle: 0,
                array_index: index,
                width: w,
                height: h,
                format: fmt,
                keep_alive: Some(Arc::new(SampleKeep {
                    _sample: sample,
                    texture: tex,
                })),
            });
        }
        Ok(frame)
    }

    fn drain_outputs(&mut self, out: &mut Vec<VideoFrame>) -> Result<()> {
        while let Some(f) = self.pull()? {
            out.push(f);
        }
        Ok(())
    }
}

/// Copy the Y plane (`h` rows) and the interleaved chroma plane (starting `coded_h` rows in).
///
/// # Safety
/// `base` must point at a locked buffer of at least `pitch × (coded_h + ⌈h/2⌉)` bytes.
#[allow(clippy::too_many_arguments)]
unsafe fn copy_planes(
    base: *const u8,
    pitch: usize,
    coded_h: usize,
    h: usize,
    row: usize,
    crow: usize,
    y: &mut [u8],
    uv: &mut [u8],
) {
    for r in 0..h {
        std::ptr::copy_nonoverlapping(
            base.add(r * pitch),
            y.as_mut_ptr().add(r * row),
            row.min(pitch),
        );
    }
    let c = base.add(coded_h * pitch);
    for r in 0..h.div_ceil(2) {
        std::ptr::copy_nonoverlapping(
            c.add(r * pitch),
            uv.as_mut_ptr().add(r * crow),
            crow.min(pitch),
        );
    }
}

impl VideoDecoderPlugin for MfVideoDecoder {
    fn decode(&mut self, packet: &[u8], pts: i64) -> Result<Vec<VideoFrame>> {
        let mut data = Vec::with_capacity(packet.len() + self.prefix.len() + 16);
        if !self.primed {
            data.extend_from_slice(&self.prefix);
        }
        match self.framing {
            Framing::Raw => data.extend_from_slice(packet),
            Framing::LengthPrefixed(n) => to_annexb(packet, n, &mut data)?,
        }
        self.primed = true;
        let sample = self.sample(&data, pts)?;
        let mut frames = Vec::new();
        for _ in 0..64 {
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => {
                    self.drain_outputs(&mut frames)?;
                    return Ok(frames);
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    let before = frames.len();
                    self.drain_outputs(&mut frames)?;
                    if frames.len() == before {
                        return Err(Error::Codec(
                            "decoder neither accepts input nor produces output".into(),
                        ));
                    }
                }
                Err(e) => return Err(os("ProcessInput", e)),
            }
        }
        Err(Error::Codec("decoder stalled".into()))
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>> {
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|e| os("drain", e))?;
        let mut frames = Vec::new();
        self.drain_outputs(&mut frames)?;
        Ok(frames)
    }

    fn reset(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        self.primed = false;
    }

    fn output_format(&self) -> RawVideoFormat {
        self.out
    }

    fn acceleration(&self) -> Acceleration {
        self.accel.clone()
    }
}

impl Drop for MfVideoDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// Decide the packet framing and the in-band prefix from the container's configuration.
fn framing(codec: CodecFamily, config: &[u8]) -> Result<(Framing, Vec<u8>)> {
    let annexb = config.starts_with(&[0, 0, 1]) || config.starts_with(&[0, 0, 0, 1]);
    match codec {
        CodecFamily::H264 if !config.is_empty() && !annexb => {
            let (n, ps) = avcc(config).ok_or_else(|| Error::BadFormat("bad avcC record".into()))?;
            Ok((Framing::LengthPrefixed(n), ps))
        }
        CodecFamily::Hevc if !config.is_empty() && !annexb => {
            let (n, ps) = hvcc(config).ok_or_else(|| Error::BadFormat("bad hvcC record".into()))?;
            Ok((Framing::LengthPrefixed(n), ps))
        }
        // av1C: 4 bytes of fields, then the configuration OBUs (sequence header)
        CodecFamily::Av1 if config.len() >= 4 && config[0] == 0x81 => {
            Ok((Framing::Raw, config[4..].to_vec()))
        }
        CodecFamily::Vp8 | CodecFamily::Vp9 => Ok((Framing::Raw, Vec::new())),
        _ => Ok((Framing::Raw, config.to_vec())),
    }
}

fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// `avcC` → (NAL length size, Annex-B SPS + PPS).
fn avcc(c: &[u8]) -> Option<(usize, Vec<u8>)> {
    if c.len() < 7 || c[0] != 1 {
        return None;
    }
    let n = usize::from(c[4] & 3) + 1;
    let mut ps = Vec::new();
    let mut pos = 6;
    for _ in 0..(c[5] & 0x1F) {
        let len = usize::from(u16::from_be_bytes([*c.get(pos)?, *c.get(pos + 1)?]));
        push_nal(&mut ps, c.get(pos + 2..pos + 2 + len)?);
        pos += 2 + len;
    }
    let count = *c.get(pos)?;
    pos += 1;
    for _ in 0..count {
        let len = usize::from(u16::from_be_bytes([*c.get(pos)?, *c.get(pos + 1)?]));
        push_nal(&mut ps, c.get(pos + 2..pos + 2 + len)?);
        pos += 2 + len;
    }
    Some((n, ps))
}

/// `hvcC` → (NAL length size, Annex-B VPS + SPS + PPS (+ SEI)).
fn hvcc(c: &[u8]) -> Option<(usize, Vec<u8>)> {
    if c.len() < 23 {
        return None;
    }
    let n = usize::from(c[21] & 3) + 1;
    let mut ps = Vec::new();
    let mut pos = 23;
    for _ in 0..c[22] {
        let count = u16::from_be_bytes([*c.get(pos + 1)?, *c.get(pos + 2)?]);
        pos += 3;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes([*c.get(pos)?, *c.get(pos + 1)?]));
            push_nal(&mut ps, c.get(pos + 2..pos + 2 + len)?);
            pos += 2 + len;
        }
    }
    Some((n, ps))
}

/// Length-prefixed NAL units → Annex-B.
fn to_annexb(p: &[u8], n: usize, out: &mut Vec<u8>) -> Result<()> {
    let mut pos = 0;
    while pos + n <= p.len() {
        let mut len = 0usize;
        for b in &p[pos..pos + n] {
            len = (len << 8) | usize::from(*b);
        }
        pos += n;
        let nal = p
            .get(pos..pos + len)
            .ok_or_else(|| Error::Codec("NAL unit runs past the packet".into()))?;
        push_nal(out, nal);
        pos += len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_to_annexb() {
        let avcc_rec = [
            1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9,
        ];
        let (n, ps) = avcc(&avcc_rec).unwrap();
        assert_eq!(n, 4);
        assert_eq!(ps, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 9]);
        let mut out = Vec::new();
        to_annexb(&[0, 0, 0, 2, 0x65, 7, 0, 0, 0, 1, 0x06], 4, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 7, 0, 0, 0, 1, 0x06]);
        assert!(to_annexb(&[0, 0, 0, 9, 1], 4, &mut Vec::new()).is_err());
    }
}
