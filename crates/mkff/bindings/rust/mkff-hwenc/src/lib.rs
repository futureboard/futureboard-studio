//! Hardware H.264 encode for MKFF.
//!
//! The GPU vendors' encoders — NVIDIA **NVENC**, Intel **Quick Sync Video (QSV)** and AMD
//! **AMF / VCE** — are driven through the Media Foundation encoder transforms their drivers
//! install, so one code path covers all three and nothing is linked at build time. Output is the
//! same [`Packet`] type as `mkff_codecs::encode` (4-byte length-prefixed NAL units, AVCC layout)
//! plus the `avcC` record, so it goes straight into `mkff_codecs::encode::MediaWriter`
//! ([`MediaWriter::add_h264_track`](mkff_codecs::encode::MediaWriter::add_h264_track)) or an
//! FLV / RTMP muxer.
//!
//! ```no_run
//! use mkff_hwenc::{HwEncoderConfig, HwVideoEncoder, Vendor};
//!
//! for e in mkff_hwenc::h264_encoders() {
//!     println!("{} — {}", e.vendor.api_name(), e.name);
//! }
//! let mut enc = HwVideoEncoder::new(None, HwEncoderConfig::cbr(1920, 1080, 60, 1, 6000))?;
//! let rgba = vec![0u8; 1920 * 1080 * 4];
//! for i in 0..60 {
//!     for p in enc.encode_rgba(&rgba, i)? { /* p.data, p.pts, p.keyframe */ }
//! }
//! let avcc = enc.avcc(); // known after the first keyframe at the latest
//! # Ok::<(), mkff_codecs::Error>(())
//! ```
//!
//! A `HwVideoEncoder` holds COM objects: create and use it on one thread.
//!
//! Other platforms: [`h264_encoders`] is empty and [`HwVideoEncoder::new`] returns
//! `Error::Unsupported`.

pub mod addon;
mod bitstream;
mod nv12;
#[cfg(windows)]
mod mf;

pub use mkff_codecs::encode::Packet;
pub use mkff_codecs::{Error, Result};
pub use nv12::rgba_to_nv12;

/// GPU vendor of a hardware encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Vendor {
    Nvidia,
    Intel,
    Amd,
    Other,
}

impl Vendor {
    /// The vendor's name for its encoder.
    pub fn api_name(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA NVENC",
            Vendor::Intel => "Intel Quick Sync (QSV)",
            Vendor::Amd => "AMD AMF",
            Vendor::Other => "Hardware",
        }
    }

    /// From a PCI vendor id string such as `VEN_10DE`.
    pub fn from_pci(id: &str) -> Vendor {
        let id = id.to_ascii_uppercase();
        if id.contains("10DE") {
            Vendor::Nvidia
        } else if id.contains("8086") {
            Vendor::Intel
        } else if id.contains("1002") || id.contains("1022") {
            Vendor::Amd
        } else {
            Vendor::Other
        }
    }
}

/// A hardware H.264 encoder found on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderInfo {
    pub vendor: Vendor,
    /// The driver's name for it ("NVIDIA H.264 Encoder MFT", …).
    pub name: String,
}

/// Every hardware H.264 encoder, discrete GPUs first (NVIDIA, AMD, then Intel).
pub fn h264_encoders() -> Vec<EncoderInfo> {
    #[cfg(windows)]
    {
        mf::list().into_iter().map(|(i, _)| i).collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Profile {
    Baseline,
    #[default]
    Main,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateControl {
    /// Constant bitrate (streaming).
    Cbr { kbps: u32 },
    /// Peak-constrained variable bitrate.
    Vbr { kbps: u32, max_kbps: u32 },
    /// Constant quality, 1–100 (higher is better).
    Quality(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub struct HwEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub profile: Profile,
    pub rate: RateControl,
    /// Frames between keyframes (0: two seconds).
    pub keyint: u32,
    /// 0 = fastest, 100 = best quality.
    pub quality_vs_speed: u8,
    /// No B-frames, minimal look-ahead (always on for live use).
    pub low_latency: bool,
}

impl HwEncoderConfig {
    /// Live streaming defaults: CBR, Main, 2 s GOP, low latency.
    pub fn cbr(width: u32, height: u32, fps_num: u32, fps_den: u32, kbps: u32) -> Self {
        HwEncoderConfig {
            width,
            height,
            fps_num,
            fps_den,
            profile: Profile::Main,
            rate: RateControl::Cbr { kbps },
            keyint: 0,
            quality_vs_speed: 50,
            low_latency: true,
        }
    }

    fn gop(&self) -> u32 {
        if self.keyint > 0 {
            self.keyint
        } else {
            (2 * self.fps_num).div_ceil(self.fps_den.max(1)).max(1)
        }
    }
}

/// A hardware H.264 encoder session.
pub struct HwVideoEncoder {
    #[cfg(windows)]
    inner: mf::MfEncoder,
    info: EncoderInfo,
    cfg: HwEncoderConfig,
    tick_scale: u32,
    nv12: Vec<u8>,
}

impl HwVideoEncoder {
    /// Open the first hardware encoder of `vendor` (or any, discrete GPUs first).
    pub fn new(vendor: Option<Vendor>, cfg: HwEncoderConfig) -> Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || cfg.width % 2 != 0 || cfg.height % 2 != 0 {
            return Err(Error::InvalidArgument(format!("H.264 needs an even frame size, got {}x{}", cfg.width, cfg.height)));
        }
        if cfg.fps_num == 0 || cfg.fps_den == 0 {
            return Err(Error::InvalidArgument(format!("frame rate {}/{}", cfg.fps_num, cfg.fps_den)));
        }
        let tick_scale = 10_000u32.div_ceil(cfg.fps_num).max(1);
        #[cfg(windows)]
        {
            let (inner, info) = mf::MfEncoder::open(vendor, &cfg)?;
            Ok(HwVideoEncoder { inner, info, cfg, tick_scale, nv12: Vec::new() })
        }
        #[cfg(not(windows))]
        {
            let _ = (vendor, tick_scale);
            Err(Error::Unsupported("hardware encoding is only implemented on Windows".into()))
        }
    }

    pub fn info(&self) -> &EncoderInfo {
        &self.info
    }

    pub fn config(&self) -> &HwEncoderConfig {
        &self.cfg
    }

    /// Track timescale: `fps_num ×` a factor ≥ 10 000 ticks / s (same rule as the software
    /// encoder, so either can feed one muxer).
    pub fn timescale(&self) -> u32 {
        self.cfg.fps_num * self.tick_scale
    }

    /// Ticks per frame.
    pub fn frame_duration(&self) -> u32 {
        self.cfg.fps_den * self.tick_scale
    }

    /// The AVCDecoderConfigurationRecord, once the encoder has produced its parameter sets
    /// (some report them at start, the rest with the first keyframe).
    pub fn avcc(&self) -> Option<&[u8]> {
        #[cfg(windows)]
        {
            self.inner.avcc()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Encode straight RGBA8 (`width * height * 4` bytes) as frame `index` (converted to BT.709
    /// limited-range NV12 on the CPU).
    pub fn encode_rgba(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>> {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        if rgba.len() < w * h * 4 {
            return Err(Error::InvalidArgument(format!("RGBA frame of {} bytes for {w}x{h}", rgba.len())));
        }
        let mut nv12 = std::mem::take(&mut self.nv12);
        rgba_to_nv12(rgba, w, h, &mut nv12);
        let r = self.encode_nv12(&nv12, index);
        self.nv12 = nv12;
        r
    }

    /// Encode an NV12 picture (`width * height` luma bytes then interleaved CbCr, no padding).
    pub fn encode_nv12(&mut self, nv12: &[u8], index: u64) -> Result<Vec<Packet>> {
        let need = self.cfg.width as usize * self.cfg.height as usize * 3 / 2;
        if nv12.len() < need {
            return Err(Error::InvalidArgument(format!("NV12 frame of {} bytes, need {need}", nv12.len())));
        }
        #[cfg(windows)]
        {
            let (ts, fd) = (self.timescale(), self.frame_duration());
            self.inner.encode(&nv12[..need], index, ts, fd)
        }
        #[cfg(not(windows))]
        {
            let _ = index;
            Err(Error::Unsupported("hardware encoding".into()))
        }
    }

    /// Drain the frames still in the encoder (call once, at the end).
    pub fn flush(&mut self) -> Result<Vec<Packet>> {
        #[cfg(windows)]
        {
            let (ts, fd) = (self.timescale(), self.frame_duration());
            self.inner.flush(ts, fd)
        }
        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
    }
}

impl std::fmt::Debug for HwVideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HwVideoEncoder").field("info", &self.info).field("cfg", &self.cfg).finish()
    }
}
