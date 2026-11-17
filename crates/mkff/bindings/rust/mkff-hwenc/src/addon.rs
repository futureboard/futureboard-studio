//! The hardware encoders as a Media Kit add-on: one H.264 encoder flavor per GPU encoder, so
//! `MediaRoster::video_encoder` picks NVENC / QSV / AMF before any software encoder.
//!
//! ```no_run
//! use mkff_mediakit::{AccelPolicy, CodecFamily, MediaFormat, MediaRoster, PixelFormat};
//!
//! let roster = MediaRoster::roster();
//! mkff_hwenc::addon::register(roster);
//! let input = MediaFormat::raw_video(1920, 1080, PixelFormat::Nv12);
//! let mut output = MediaFormat::encoded(CodecFamily::H264, Vec::new(), 1920, 1080);
//! if let MediaFormat::EncodedVideo(e) = &mut output {
//!     e.bit_rate = 8_000_000;
//! }
//! let (flavor, enc) = roster.video_encoder(&input, &output, AccelPolicy::Auto)?;
//! # Ok::<(), mkff_mediakit::Error>(())
//! ```

use std::sync::Arc;

use mkff_mediakit::{
    Acceleration, CodecFamily, EncodedChunk, FlavorInfo, GpuVendor, Instance, Kinds, MediaAddOn, MediaFormat, MediaRoster,
    PixelFormat, VideoEncoderPlugin, VideoFrame,
};

use crate::{h264_encoders, EncoderInfo, HwEncoderConfig, HwVideoEncoder, Packet, Vendor};

/// Register the add-on with `roster` (does nothing on machines without hardware encoders).
pub fn register(roster: &MediaRoster) {
    let encoders = h264_encoders();
    if !encoders.is_empty() {
        roster.register_addon(Arc::new(HwEncoderAddOn { encoders }));
    }
}

/// Media Kit add-on over [`h264_encoders`].
pub struct HwEncoderAddOn {
    encoders: Vec<EncoderInfo>,
}

fn gpu_vendor(v: Vendor) -> GpuVendor {
    match v {
        Vendor::Nvidia => GpuVendor::Nvidia,
        Vendor::Intel => GpuVendor::Intel,
        Vendor::Amd => GpuVendor::Amd,
        Vendor::Other => GpuVendor::Other(0),
    }
}

fn accel(v: Vendor) -> Acceleration {
    Acceleration::Gpu { vendor: gpu_vendor(v), adapter_luid: 0, api: v.api_name().into() }
}

fn mk(e: mkff_codecs::Error) -> mkff_mediakit::Error {
    mkff_mediakit::Error::Codec(e.to_string())
}

impl MediaAddOn for HwEncoderAddOn {
    fn name(&self) -> &str {
        "hardware H.264 encoders (Media Foundation)"
    }

    fn flavors(&self) -> Vec<FlavorInfo> {
        self.encoders
            .iter()
            .enumerate()
            .map(|(i, e)| FlavorInfo {
                id: i as u32,
                name: e.name.clone(),
                info: format!("{} through its Media Foundation encoder", e.vendor.api_name()),
                kinds: Kinds::ENCODER,
                accel: accel(e.vendor),
                in_formats: vec![MediaFormat::raw_video(0, 0, PixelFormat::Nv12), MediaFormat::raw_video(0, 0, PixelFormat::Rgba8)],
                out_formats: vec![MediaFormat::encoded(CodecFamily::H264, Vec::new(), 0, 0)],
                // discrete GPUs (listed first) before the iGPU
                rank: 300 - i as i32,
            })
            .collect()
    }

    fn instantiate(&self, flavor_id: u32, input: &MediaFormat, output: &MediaFormat) -> mkff_mediakit::Result<Instance> {
        let e = self.encoders.get(flavor_id as usize).ok_or_else(|| mkff_mediakit::Error::BadNode(format!("flavor {flavor_id}")))?;
        let MediaFormat::RawVideo(v) = input else {
            return Err(mkff_mediakit::Error::BadFormat(format!("H.264 encoder input {input}")));
        };
        let kbps = match output {
            MediaFormat::EncodedVideo(o) if o.bit_rate > 0 => o.bit_rate / 1000,
            _ => 8_000,
        };
        let (num, den) = if v.fps.0 > 0 && v.fps.1 > 0 { v.fps } else { (30, 1) };
        let enc = HwVideoEncoder::new(Some(e.vendor), HwEncoderConfig::cbr(v.width, v.height, num, den, kbps)).map_err(mk)?;
        Ok(Instance::VideoEncoder(Box::new(HwEncoderPlugin { enc, index: 0, nv12: Vec::new(), accel: accel(e.vendor) })))
    }
}

struct HwEncoderPlugin {
    enc: HwVideoEncoder,
    index: u64,
    nv12: Vec<u8>,
    accel: Acceleration,
}

// SAFETY: the encoder's COM objects live in the multithreaded apartment (COINIT_MULTITHREADED),
// and the plugin is used by one thread at a time (`Send`, not `Sync`).
unsafe impl Send for HwEncoderPlugin {}

impl HwEncoderPlugin {
    fn chunks(&self, ps: Vec<Packet>) -> Vec<EncodedChunk> {
        let ts = i64::from(self.enc.timescale().max(1));
        let us = |t: i64| t.saturating_mul(1_000_000) / ts;
        ps.into_iter()
            .map(|p| EncodedChunk { pts: us(p.pts), dts: us(p.dts), duration: us(i64::from(p.duration)), keyframe: p.keyframe, data: p.data })
            .collect()
    }
}

impl VideoEncoderPlugin for HwEncoderPlugin {
    fn encode(&mut self, frame: &VideoFrame) -> mkff_mediakit::Result<Vec<EncodedChunk>> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let i = self.index;
        self.index += 1;
        let ps = match frame.format {
            PixelFormat::Rgba8 => {
                let p = frame.planes.first().ok_or_else(|| mkff_mediakit::Error::BadFormat("RGBA frame without a plane".into()))?;
                self.enc.encode_rgba(p, i).map_err(mk)?
            }
            PixelFormat::Nv12 if frame.planes.len() >= 2 => {
                // pack the two planes tightly
                let mut buf = std::mem::take(&mut self.nv12);
                buf.clear();
                let ys = frame.strides.first().copied().unwrap_or(w);
                let cs = frame.strides.get(1).copied().unwrap_or(w);
                for r in 0..h {
                    buf.extend_from_slice(frame.planes[0].get(r * ys..r * ys + w).unwrap_or(&[]));
                }
                for r in 0..h / 2 {
                    buf.extend_from_slice(frame.planes[1].get(r * cs..r * cs + w).unwrap_or(&[]));
                }
                let r = self.enc.encode_nv12(&buf, i).map_err(mk);
                self.nv12 = buf;
                r?
            }
            other => return Err(mkff_mediakit::Error::BadFormat(format!("hardware H.264 takes NV12 or RGBA, not {other:?}"))),
        };
        Ok(self.chunks(ps))
    }

    fn flush(&mut self) -> mkff_mediakit::Result<Vec<EncodedChunk>> {
        let ps = self.enc.flush().map_err(mk)?;
        Ok(self.chunks(ps))
    }

    fn output_format(&self) -> MediaFormat {
        let c = self.enc.config();
        MediaFormat::encoded(CodecFamily::H264, self.enc.avcc().map(<[u8]>::to_vec).unwrap_or_default(), c.width, c.height)
    }

    fn acceleration(&self) -> Acceleration {
        self.accel.clone()
    }
}
