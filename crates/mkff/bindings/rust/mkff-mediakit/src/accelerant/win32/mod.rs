#![allow(non_upper_case_globals)] // D3D11_DECODER_PROFILE_MPEG2and1_VLD in a match pattern

//! The Windows accelerant: one per DXGI adapter, driving the GPU's media engines through
//! D3D11 Video (DXVA) and Media Foundation.

mod decoder;
mod mf;

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ::windows::core::{Interface, GUID};
use ::windows::Win32::Foundation::HMODULE;
use ::windows::Win32::Graphics::Direct3D::*;
use ::windows::Win32::Graphics::Direct3D11::*;
use ::windows::Win32::Graphics::Dxgi::Common::*;
use ::windows::Win32::Graphics::Dxgi::*;

use super::{Accelerant, DeviceInfo, EngineGuard, Feature, HookInfo, SyncToken};
use crate::addon::{GpuVendor, VideoDecoderPlugin};
use crate::format::{CodecFamily, MediaFormat, PixelFormat};
use crate::{Error, Result};

pub use decoder::{texture_of, GPU_POOL};

/// The D3D11 device of an accelerant. D3D11 devices are free-threaded and the immediate
/// context is multithread-protected (`ID3D11Multithread`), so the wrapper may cross threads.
pub struct Device {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub video: Option<ID3D11VideoDevice>,
    multithread: ID3D11Multithread,
}

// SAFETY: see the type docs; context use is serialised by the multithread lock / engine mutex.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

struct EngineState {
    held: bool,
    counter: u64,
    /// Event queries issued at each release, oldest first.
    pending: VecDeque<(u64, ID3D11Query)>,
    /// Every token up to this one is known complete.
    completed: u64,
}

// SAFETY: the queries are only touched under the engine mutex.
unsafe impl Send for EngineState {}

/// A GPU, as an accelerant.
pub struct D3D11Accelerant {
    /// PCI vendor / device / subsystem / revision and VRAM, to spot duplicate listings.
    key: (u32, u32, u32, u32, u64),
    info: DeviceInfo,
    adapter: AdapterHandle,
    device: OnceLock<std::result::Result<Arc<Device>, String>>,
    hooks: OnceLock<Vec<HookInfo>>,
    engine: Mutex<EngineState>,
    engine_free: Condvar,
}

struct AdapterHandle(IDXGIAdapter1);
// SAFETY: DXGI adapters are free-threaded.
unsafe impl Send for AdapterHandle {}
unsafe impl Sync for AdapterHandle {}

/// Every hardware adapter, discrete first (more dedicated memory first).
pub fn enumerate() -> Vec<Arc<dyn Accelerant>> {
    let mut out: Vec<D3D11Accelerant> = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return Vec::new();
        };
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .trim()
                .to_owned();
            let luid = ((desc.AdapterLuid.HighPart as u32 as u64) << 32)
                | u64::from(desc.AdapterLuid.LowPart);
            let driver_version = adapter
                .CheckInterfaceSupport(&IDXGIDevice::IID)
                .ok()
                .map(|v| {
                    let v = v as u64;
                    format!(
                        "{}.{}.{}.{}",
                        v >> 48,
                        (v >> 32) & 0xFFFF,
                        (v >> 16) & 0xFFFF,
                        v & 0xFFFF
                    )
                });
            let vendor = GpuVendor::from_pci(desc.VendorId);
            let memory = desc.DedicatedVideoMemory as u64;
            let info = DeviceInfo {
                name,
                chipset: format!("VEN_{:04X}&DEV_{:04X}", desc.VendorId, desc.DeviceId),
                vendor,
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                luid,
                memory,
                shared_memory: desc.SharedSystemMemory as u64,
                driver_version,
                integrated: memory <= 512 << 20,
            };
            // DXGI can list one GPU more than once (e.g. under an indirect display driver), each
            // time with its own LUID: keep the first of identical adapters
            let key = (
                desc.VendorId,
                desc.DeviceId,
                desc.SubSysId,
                desc.Revision,
                memory,
            );
            if out.iter().any(|a| a.info.luid == luid || a.key == key) {
                continue;
            }
            out.push(D3D11Accelerant {
                key,
                info,
                adapter: AdapterHandle(adapter),
                device: OnceLock::new(),
                hooks: OnceLock::new(),
                engine: Mutex::new(EngineState {
                    held: false,
                    counter: 0,
                    pending: VecDeque::new(),
                    completed: 0,
                }),
                engine_free: Condvar::new(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.info
            .integrated
            .cmp(&b.info.integrated)
            .then(b.info.memory.cmp(&a.info.memory))
    });
    out.into_iter()
        .map(|a| Arc::new(a) as Arc<dyn Accelerant>)
        .collect()
}

impl D3D11Accelerant {
    /// The accelerant's D3D11 device (created on first use, with video support).
    pub fn device(&self) -> Result<Arc<Device>> {
        self.device
            .get_or_init(|| {
                unsafe { create_device(&self.adapter.0) }
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(Error::Os)
    }

    fn probe_hooks(&self) -> Vec<HookInfo> {
        let mut hooks = Vec::new();
        let Ok(dev) = self.device() else { return hooks };
        let _mf = mf::Runtime::start();
        // decode: DXVA profiles the driver exposes, for codecs a Media Foundation decoder takes
        if let Some(video) = &dev.video {
            let mut seen: Vec<(CodecFamily, u8)> = Vec::new();
            let count = unsafe { video.GetVideoDecoderProfileCount() };
            for i in 0..count {
                let Ok(guid) = (unsafe { video.GetVideoDecoderProfile(i) }) else {
                    continue;
                };
                let Some((codec, depth)) = profile_codec(&guid) else {
                    continue;
                };
                if seen.contains(&(codec, depth)) {
                    continue;
                }
                let fmt = if depth > 8 {
                    DXGI_FORMAT_P010
                } else {
                    DXGI_FORMAT_NV12
                };
                let ok = unsafe { video.CheckVideoDecoderFormat(&guid, fmt) }
                    .map(|b| b.as_bool())
                    .unwrap_or(false);
                if !ok || !mf::has_decoder(codec) {
                    continue;
                }
                seen.push((codec, depth));
                hooks.push(HookInfo {
                    feature: Feature::VideoDecode {
                        codec,
                        bit_depth: depth,
                    },
                    api: "D3D11 Video (DXVA) via Media Foundation".into(),
                    max_size: None,
                    formats: vec![if depth > 8 {
                        PixelFormat::P010
                    } else {
                        PixelFormat::Nv12
                    }],
                });
            }
            // the video processor (colour conversion / scaling)
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputWidth: 1920,
                InputHeight: 1080,
                OutputWidth: 1920,
                OutputHeight: 1080,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                ..Default::default()
            };
            if unsafe { video.CreateVideoProcessorEnumerator(&desc) }.is_ok() {
                hooks.push(HookInfo {
                    feature: Feature::VideoProcess,
                    api: "D3D11 Video Processor".into(),
                    max_size: None,
                    formats: vec![
                        PixelFormat::Nv12,
                        PixelFormat::P010,
                        PixelFormat::Bgra8,
                        PixelFormat::Rgba8,
                    ],
                });
            }
        }
        // encode: hardware encoder MFTs bound to this adapter (NVENC / QSV / AMF / …)
        for (codec, name) in mf::hardware_encoders(self.info.luid, self.info.vendor_id) {
            if hooks
                .iter()
                .any(|h: &HookInfo| h.feature == Feature::VideoEncode { codec })
            {
                continue;
            }
            hooks.push(HookInfo {
                feature: Feature::VideoEncode { codec },
                api: format!("Media Foundation: {name}"),
                max_size: None,
                formats: vec![PixelFormat::Nv12],
            });
        }
        // overlay planes on any output of this adapter
        unsafe {
            let mut i = 0;
            let mut overlay = Vec::new();
            while let Ok(output) = self.adapter.0.EnumOutputs(i) {
                i += 1;
                let Ok(o3) = output.cast::<IDXGIOutput3>() else {
                    continue;
                };
                for (f, pf) in [
                    (DXGI_FORMAT_NV12, PixelFormat::Nv12),
                    (DXGI_FORMAT_P010, PixelFormat::P010),
                ] {
                    if o3
                        .CheckOverlaySupport(f, &dev.device)
                        .map(|flags| flags != 0)
                        .unwrap_or(false)
                        && !overlay.contains(&pf)
                    {
                        overlay.push(pf);
                    }
                }
            }
            if !overlay.is_empty() {
                hooks.push(HookInfo {
                    feature: Feature::Overlay,
                    api: "DXGI multiplane overlay".into(),
                    max_size: None,
                    formats: overlay,
                });
            }
        }
        hooks.push(HookInfo {
            feature: Feature::Blit,
            api: "D3D11 CopySubresourceRegion".into(),
            max_size: None,
            formats: Vec::new(),
        });
        hooks
    }

    fn release_engine(&self) -> SyncToken {
        let mut st = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        st.counter += 1;
        let counter = st.counter;
        if let Ok(dev) = self.device() {
            unsafe {
                let desc = D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                };
                let mut q = None;
                if dev.device.CreateQuery(&desc, Some(&mut q)).is_ok() {
                    if let Some(q) = q {
                        dev.context.End(&q);
                        dev.context.Flush();
                        st.pending.push_back((counter, q));
                    }
                }
                dev.multithread.Leave();
            }
        }
        if st.pending.is_empty() {
            st.completed = counter;
        }
        st.held = false;
        self.engine_free.notify_one();
        SyncToken {
            engine_id: 0,
            counter,
        }
    }

    /// Poll pending queries; returns whether `counter` has completed.
    fn poll(&self, st: &mut EngineState, counter: u64) -> bool {
        let Ok(dev) = self.device() else { return true };
        while let Some((c, q)) = st.pending.front() {
            let mut done = ::windows::core::BOOL(0);
            let r = unsafe {
                dev.context.GetData(
                    q,
                    Some(&mut done as *mut _ as *mut _),
                    std::mem::size_of::<::windows::core::BOOL>() as u32,
                    0,
                )
            };
            if r.is_err() || done.as_bool() {
                st.completed = *c;
                st.pending.pop_front();
            } else {
                break;
            }
        }
        st.completed >= counter
    }
}

unsafe fn create_device(adapter: &IDXGIAdapter1) -> ::windows::core::Result<Device> {
    let mut device = None;
    let mut context = None;
    let levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    D3D11CreateDevice(
        adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )?;
    let device: ID3D11Device = device.ok_or_else(::windows::core::Error::empty)?;
    let context: ID3D11DeviceContext = context.ok_or_else(::windows::core::Error::empty)?;
    let multithread: ID3D11Multithread = context.cast()?;
    let _ = multithread.SetMultithreadProtected(true);
    let video = device.cast::<ID3D11VideoDevice>().ok();
    Ok(Device {
        device,
        context,
        video,
        multithread,
    })
}

/// DXVA decoder profile → codec and bit depth.
fn profile_codec(g: &GUID) -> Option<(CodecFamily, u8)> {
    Some(match *g {
        D3D11_DECODER_PROFILE_H264_VLD_NOFGT => (CodecFamily::H264, 8),
        D3D11_DECODER_PROFILE_HEVC_VLD_MAIN => (CodecFamily::Hevc, 8),
        D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10 => (CodecFamily::Hevc, 10),
        D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0 => (CodecFamily::Vp9, 8),
        D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2 => (CodecFamily::Vp9, 10),
        D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0 => (CodecFamily::Av1, 10),
        D3D11_DECODER_PROFILE_VP8_VLD => (CodecFamily::Vp8, 8),
        D3D11_DECODER_PROFILE_MPEG2and1_VLD | D3D11_DECODER_PROFILE_MPEG2_VLD => {
            (CodecFamily::Mpeg2, 8)
        }
        _ => return None,
    })
}

struct Guard<'a> {
    accel: &'a D3D11Accelerant,
    released: bool,
}

impl EngineGuard for Guard<'_> {
    fn engine_id(&self) -> u32 {
        0
    }

    fn release(mut self: Box<Self>) -> SyncToken {
        self.released = true;
        self.accel.release_engine()
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.accel.release_engine();
        }
    }
}

impl Accelerant for D3D11Accelerant {
    fn device_info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn hooks(&self) -> Vec<HookInfo> {
        self.hooks.get_or_init(|| self.probe_hooks()).clone()
    }

    fn acquire_engine(
        &self,
        max_wait: Option<Duration>,
        after: Option<SyncToken>,
    ) -> Result<Box<dyn EngineGuard + '_>> {
        if let Some(t) = after {
            self.sync_to_token(t)?;
        }
        let dev = self.device()?;
        let deadline = max_wait.map(|d| Instant::now() + d);
        let mut st = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        while st.held {
            st = match deadline {
                None => self.engine_free.wait(st).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(Error::TimedOut);
                    }
                    self.engine_free
                        .wait_timeout(st, d - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
        st.held = true;
        drop(st);
        unsafe { dev.multithread.Enter() };
        Ok(Box::new(Guard {
            accel: self,
            released: false,
        }))
    }

    fn sync_to_token(&self, token: SyncToken) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let mut st = self.engine.lock().unwrap_or_else(|e| e.into_inner());
                if self.poll(&mut st, token.counter) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::TimedOut);
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    fn wait_engine_idle(&self) -> Result<()> {
        let t = self.acquire_engine(None, None)?.release();
        self.sync_to_token(t)
    }

    fn video_decoder(
        &self,
        format: &MediaFormat,
        gpu_output: bool,
    ) -> Result<Box<dyn VideoDecoderPlugin>> {
        let codec = format
            .codec()
            .filter(|c| c.is_video())
            .ok_or_else(|| Error::BadFormat(format!("{format} is not encoded video")))?;
        if !self
            .hooks()
            .iter()
            .any(|h| matches!(h.feature, Feature::VideoDecode { codec: c, .. } if c == codec))
        {
            return Err(Error::Unsupported(format!(
                "{} has no {codec} decoder",
                self.info.name
            )));
        }
        let dev = self.device()?;
        let accel = self.acceleration("D3D11 Video (DXVA) via Media Foundation");
        Ok(Box::new(decoder::MfVideoDecoder::open(
            dev,
            self.info.luid,
            accel,
            format,
            gpu_output,
        )?))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_and_engine_tokens() {
        let accels = enumerate();
        eprintln!("{}", super::super::describe(&accels));
        for a in &accels {
            let t1 = a
                .acquire_engine(Some(Duration::from_secs(5)), None)
                .unwrap()
                .release();
            let t2 = a
                .acquire_engine(Some(Duration::from_secs(5)), Some(t1))
                .unwrap()
                .release();
            assert!(t2.counter > t1.counter);
            a.sync_to_token(t2).unwrap();
            a.wait_engine_idle().unwrap();
        }
    }
}
