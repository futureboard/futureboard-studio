//! Accelerants: the GPU driver layer, after the BeOS accelerant API.
//!
//! On BeOS the `app_server` loaded one *accelerant* add-on per graphics card and asked it for
//! function pointers by feature code (`get_accelerant_hook(B_FILL_RECTANGLE, …)`), serialising
//! access to the card's engine with `acquire_engine` / `release_engine` and ordering work with
//! sync tokens. MKFF keeps that model for media acceleration on Windows:
//!
//! | BeOS | MKFF on Windows |
//! |------|-----------------|
//! | one accelerant per card | one [`Accelerant`] per DXGI adapter (discrete GPUs, iGPU) |
//! | `B_GET_ACCELERANT_DEVICE_INFO` | [`Accelerant::device_info`] (adapter description, PCI ids, VRAM, LUID) |
//! | `get_accelerant_hook(feature)` | [`Accelerant::hook`]: is a [`Feature`] there, and how fast (`HookInfo`) |
//! | `B_ACQUIRE_ENGINE` / `B_RELEASE_ENGINE` | [`Accelerant::acquire_engine`] → [`EngineGuard`] (the D3D11 immediate context, multithread-protected) |
//! | `B_GET_SYNC_TOKEN` / `B_SYNC_TO_TOKEN` | [`SyncToken`]: a D3D11 event query per release; [`Accelerant::sync_to_token`] waits on it |
//! | `B_WAIT_ENGINE_IDLE` | [`Accelerant::wait_engine_idle`] (flush + wait on a fresh event query) |
//! | overlay hooks | [`Feature::Overlay`]: DXGI multiplane overlay support for NV12 / YUY2 |
//! | — (codec kit add-ons) | [`Feature::VideoDecode`] / [`Feature::VideoEncode`]: DXVA decoder profiles, Media Foundation hardware encoders (NVENC / QSV / AMF / VCN) |
//!
//! The accelerant add-on ([`AccelerantAddOn`], registered in [`crate::MediaRoster::roster`])
//! turns every decode hook into a roster flavor, so `roster.video_decoder(h264, Auto)` returns
//! the GPU decoder — and falls back to software flavors where no accelerant takes the stream.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::addon::{Acceleration, FlavorInfo, GpuVendor, Instance, Kinds, MediaAddOn};
use crate::format::{CodecFamily, MediaFormat, PixelFormat};
use crate::roster::MediaRoster;
use crate::{Error, Result};

#[cfg(windows)]
pub mod win32;

/// Accelerated features (the hook codes of `get_accelerant_hook`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    /// Fixed-function decode of a codec (`bit_depth` 8 / 10 / 12).
    VideoDecode { codec: CodecFamily, bit_depth: u8 },
    /// Fixed-function encode of a codec.
    VideoEncode { codec: CodecFamily },
    /// Colour conversion and scaling between pixel formats (D3D11 video processor).
    VideoProcess,
    /// Hardware overlay planes for Y'CbCr surfaces (BeOS overlay hooks).
    Overlay,
    /// Copy between GPU surfaces (`B_SCREEN_TO_SCREEN_BLIT`).
    Blit,
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Feature::VideoDecode { codec, bit_depth } => {
                write!(f, "decode {codec} {bit_depth}-bit")
            }
            Feature::VideoEncode { codec } => write!(f, "encode {codec}"),
            Feature::VideoProcess => write!(f, "video processor"),
            Feature::Overlay => write!(f, "overlay"),
            Feature::Blit => write!(f, "blit"),
        }
    }
}

/// What the driver says about a hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookInfo {
    pub feature: Feature,
    /// Driver path ("D3D11 Video (DXVA)", "Media Foundation: NVIDIA H.264 Encoder MFT", …).
    pub api: String,
    /// Largest picture, when the driver reports it.
    pub max_size: Option<(u32, u32)>,
    /// Pixel formats it reads / writes.
    pub formats: Vec<PixelFormat>,
}

/// `accelerant_device_info`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Adapter description ("NVIDIA GeForce RTX 4070").
    pub name: String,
    /// "VEN_10DE&DEV_2786" style chipset id.
    pub chipset: String,
    pub vendor: GpuVendor,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Adapter LUID (the "serial number" that identifies it to other APIs).
    pub luid: u64,
    /// Dedicated video memory in bytes.
    pub memory: u64,
    /// System memory the GPU may use, in bytes.
    pub shared_memory: u64,
    /// User-mode driver version, when known.
    pub driver_version: Option<String>,
    /// An integrated GPU (shares system memory).
    pub integrated: bool,
}

/// Proof of exclusive use of an accelerant's engine (`engine_token`). Releasing it (drop, or
/// [`EngineGuard::release`]) yields a [`SyncToken`] for the work submitted under it.
pub trait EngineGuard {
    fn engine_id(&self) -> u32;
    /// Release the engine, returning a token that completes when the submitted work has run
    /// (`release_engine(et, &st)`).
    fn release(self: Box<Self>) -> SyncToken;
}

/// Orders GPU work (`sync_token`): completes when the engine has executed everything submitted
/// before it was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SyncToken {
    pub engine_id: u32,
    pub counter: u64,
}

/// A GPU's media accelerant.
pub trait Accelerant: Send + Sync {
    fn device_info(&self) -> DeviceInfo;

    /// Every hook the driver provides.
    fn hooks(&self) -> Vec<HookInfo>;

    /// `get_accelerant_hook(feature)`: the hook, if the driver has it.
    fn hook(&self, feature: Feature) -> Option<HookInfo> {
        self.hooks().into_iter().find(|h| h.feature == feature)
    }

    /// `B_ACCELERANT_ENGINE_COUNT`.
    fn engine_count(&self) -> u32 {
        1
    }

    /// `B_ACQUIRE_ENGINE`: wait (up to `max_wait`) for the engine, after `after` completed if
    /// given.
    fn acquire_engine(
        &self,
        max_wait: Option<Duration>,
        after: Option<SyncToken>,
    ) -> Result<Box<dyn EngineGuard + '_>>;

    /// `B_SYNC_TO_TOKEN`: block until the work behind `token` has executed.
    fn sync_to_token(&self, token: SyncToken) -> Result<()>;

    /// `B_WAIT_ENGINE_IDLE`.
    fn wait_engine_idle(&self) -> Result<()>;

    /// A decoder session for `format` (an encoded video format) on this GPU.
    fn video_decoder(
        &self,
        format: &MediaFormat,
        gpu_output: bool,
    ) -> Result<Box<dyn crate::addon::VideoDecoderPlugin>>;

    /// How flavors of this accelerant are labelled.
    fn acceleration(&self, api: &str) -> Acceleration {
        let i = self.device_info();
        Acceleration::Gpu {
            vendor: i.vendor,
            adapter_luid: i.luid,
            api: api.to_owned(),
        }
    }

    /// Platform-specific access (e.g. the D3D11 device), for code that needs it.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Every accelerant on this machine, discrete GPUs first.
pub fn enumerate() -> Vec<Arc<dyn Accelerant>> {
    #[cfg(windows)]
    {
        win32::enumerate()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Register the accelerant add-on (one flavor per decode hook per GPU) with `roster`.
pub fn register_platform_addons(roster: &MediaRoster) {
    let accels = roster.accelerants().to_vec();
    if !accels.is_empty() {
        roster.register_addon(Arc::new(AccelerantAddOn { accels }));
    }
}

/// The media add-on that exposes accelerants' decode hooks as flavors.
pub struct AccelerantAddOn {
    accels: Vec<Arc<dyn Accelerant>>,
}

impl AccelerantAddOn {
    pub fn new(accels: Vec<Arc<dyn Accelerant>>) -> Self {
        Self { accels }
    }
}

/// Flavor ids: accelerant index × 1024 + hook index.
const PER_ACCEL: u32 = 1024;

impl MediaAddOn for AccelerantAddOn {
    fn name(&self) -> &str {
        "GPU accelerants"
    }

    fn flavors(&self) -> Vec<FlavorInfo> {
        let mut out = Vec::new();
        for (ai, a) in self.accels.iter().enumerate() {
            let dev = a.device_info();
            let hooks = a.hooks();
            for (hi, h) in hooks.iter().enumerate() {
                let Feature::VideoDecode { codec, bit_depth } = h.feature else {
                    continue;
                };
                // one flavor per codec: the deepest bit depth (the session handles all of them)
                if hooks.iter().any(|o| matches!(o.feature, Feature::VideoDecode { codec: c, bit_depth: b } if c == codec && b > bit_depth)) {
                    continue;
                }
                let input = MediaFormat::encoded(codec, Vec::new(), 0, 0);
                out.push(FlavorInfo {
                    id: ai as u32 * PER_ACCEL + hi as u32,
                    name: format!("{} {codec} decoder ({bit_depth}-bit)", dev.name),
                    info: format!("{} on {}", h.api, dev.name),
                    kinds: Kinds::DECODER,
                    accel: a.acceleration(&h.api),
                    in_formats: vec![input],
                    out_formats: vec![MediaFormat::raw_video(
                        0,
                        0,
                        if bit_depth > 8 {
                            PixelFormat::P010
                        } else {
                            PixelFormat::Nv12
                        },
                    )],
                    // discrete GPUs first; within a GPU, deeper bit depth hooks cover more streams
                    rank: if dev.integrated { 100 } else { 200 } + i32::from(bit_depth),
                });
            }
        }
        out
    }

    fn instantiate(
        &self,
        flavor_id: u32,
        input: &MediaFormat,
        output: &MediaFormat,
    ) -> Result<Instance> {
        let a = self
            .accels
            .get((flavor_id / PER_ACCEL) as usize)
            .ok_or_else(|| Error::BadNode(format!("no accelerant for flavor {flavor_id}")))?;
        let gpu = matches!(output, MediaFormat::RawVideo(v) if v.gpu);
        Ok(Instance::VideoDecoder(a.video_decoder(input, gpu)?))
    }
}

/// A one-line summary per accelerant and hook, for logs and `mkff-mediakit`'s example.
pub fn describe(accels: &[Arc<dyn Accelerant>]) -> String {
    let mut s = String::new();
    for a in accels {
        let d = a.device_info();
        s += &format!(
            "{} [{}] {} MiB{}{}\n",
            d.name,
            d.chipset,
            d.memory >> 20,
            if d.integrated { ", integrated" } else { "" },
            d.driver_version
                .as_ref()
                .map(|v| format!(", driver {v}"))
                .unwrap_or_default()
        );
        for h in a.hooks() {
            s += &format!(
                "  {:<24} {}{}\n",
                h.feature.to_string(),
                h.api,
                h.max_size
                    .map(|(w, h)| format!(" (up to {w}×{h})"))
                    .unwrap_or_default()
            );
        }
    }
    s
}
