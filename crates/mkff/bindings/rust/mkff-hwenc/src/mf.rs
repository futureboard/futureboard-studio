//! Media Foundation hardware H.264 encoders (NVENC / QSV / AMF MFTs).
//!
//! Hardware encoder MFTs are asynchronous: after `NOTIFY_START_OF_STREAM` they post
//! `METransformNeedInput` / `METransformHaveOutput` events, and input may only be submitted
//! against a `NeedInput`. Input is a system-memory NV12 sample (the MFT uploads it); output is
//! an Annex-B access unit converted to AVCC here. A synchronous MFT is driven the classic way.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};

use crate::bitstream;
use crate::{EncoderInfo, Error, HwEncoderConfig, Packet, Profile, RateControl, Result, Vendor};

const MF_VERSION: u32 = 0x0002_0070;
/// How long an encoder may take to ask for input or finish draining before it is called hung.
const TIMEOUT: Duration = Duration::from_secs(3);

fn err(what: &str, e: windows::core::Error) -> Error {
    Error::Encode(format!("{what}: {e}"))
}

struct Com;

impl Com {
    fn init() -> Com {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        }
        Com
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

fn vendor_rank(v: Vendor) -> u8 {
    match v {
        Vendor::Nvidia => 0,
        Vendor::Amd => 1,
        Vendor::Intel => 2,
        Vendor::Other => 3,
    }
}

unsafe fn string_attr(a: &IMFActivate, key: &GUID) -> String {
    let mut p = windows::core::PWSTR::null();
    let mut len = 0u32;
    if a.GetAllocatedString(key, &mut p, &mut len).is_ok() && !p.is_null() {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    } else {
        String::new()
    }
}

/// Hardware NV12 → H.264 encoders with their activation objects.
pub(crate) fn list() -> Vec<(EncoderInfo, IMFActivate)> {
    let _com = Com::init();
    unsafe { enumerate() }.unwrap_or_default()
}

/// Every GPU's encoders: `MFTEnum2` per DXGI adapter (finds encoders on GPUs without a display,
/// e.g. the Intel iGPU next to a discrete card), then `MFTEnumEx` for anything else.
unsafe fn enumerate() -> windows::core::Result<Vec<(EncoderInfo, IMFActivate)>> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut out: Vec<(EncoderInfo, IMFActivate)> = Vec::new();
    let mut push = |list: *mut Option<IMFActivate>, count: u32, adapter: Option<(Vendor, String)>| {
        if list.is_null() {
            return;
        }
        for i in 0..count as usize {
            if let Some(a) = (*list.add(i)).take() {
                let name = string_attr(&a, &MFT_FRIENDLY_NAME_Attribute);
                let ven = string_attr(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute);
                let mut vendor = Vendor::from_pci(&ven);
                if vendor == Vendor::Other {
                    vendor = adapter.as_ref().map(|(v, _)| *v).unwrap_or(Vendor::Other);
                }
                if vendor == Vendor::Other {
                    let n = name.to_ascii_lowercase();
                    vendor = if n.contains("nvidia") {
                        Vendor::Nvidia
                    } else if n.contains("intel") || n.contains("quick sync") {
                        Vendor::Intel
                    } else if n.contains("amd") || n.contains("radeon") {
                        Vendor::Amd
                    } else {
                        Vendor::Other
                    };
                }
                if !out.iter().any(|(e, _)| e.vendor == vendor && e.name == name) {
                    out.push((EncoderInfo { vendor, name }, a));
                }
            }
        }
        CoTaskMemFree(Some(list as *const _));
    };

    if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else { continue };
            // Skip the software rasteriser (DXGI_ADAPTER_FLAG_SOFTWARE).
            if desc.Flags & 2 != 0 {
                continue;
            }
            let vendor = Vendor::from_pci(&format!("{:04X}", desc.VendorId));
            let luid = ((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64;
            let mut attrs: Option<IMFAttributes> = None;
            if MFCreateAttributes(&mut attrs, 1).is_err() {
                continue;
            }
            let Some(attrs) = attrs else { continue };
            if attrs.SetUINT64(&MFT_ENUM_ADAPTER_LUID, luid).is_err() {
                continue;
            }
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            if MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &attrs, &mut list, &mut count).is_ok() {
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
                push(list, count, Some((vendor, name)));
            }
        }
    }
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut list, &mut count).is_ok() {
        push(list, count, None);
    }
    out.sort_by_key(|(i, _)| vendor_rank(i.vendor));
    Ok(out)
}

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt: VT_UI4, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { ulVal: v } }),
        },
    }
}

fn variant_bool(b: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: VARIANT_BOOL(if b { -1 } else { 0 }) },
            }),
        },
    }
}

pub(crate) struct MfEncoder {
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    need_input: u32,
    provides_samples: bool,
    out_size: u32,
    avcc: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    fps: (u32, u32),
    /// Declared last: released after the COM objects above.
    _com: Com,
}

impl MfEncoder {
    pub(crate) fn open(vendor: Option<Vendor>, cfg: &HwEncoderConfig) -> Result<(MfEncoder, EncoderInfo)> {
        let com = Com::init();
        let all = unsafe { enumerate() }.map_err(|e| err("enumerate hardware encoders", e))?;
        if all.is_empty() {
            return Err(Error::Unsupported("no hardware H.264 encoder (NVENC / QSV / AMF) found".into()));
        }
        let candidates: Vec<_> = all.into_iter().filter(|(i, _)| vendor.is_none_or(|v| v == i.vendor)).collect();
        if candidates.is_empty() {
            return Err(Error::Unsupported(format!("no {} encoder on this machine", vendor.map_or("hardware", |v| v.api_name()))));
        }
        let mut last = None;
        for (info, act) in candidates {
            match unsafe { Self::setup(&act, cfg) } {
                Ok(enc) => {
                    log::info!("hardware encoder: {} ({})", info.name, info.vendor.api_name());
                    drop(com);
                    return Ok((enc, info));
                }
                Err(e) => {
                    log::warn!("{}: {e}", info.name);
                    unsafe {
                        let _ = act.ShutdownObject();
                    }
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::Unsupported("no usable hardware encoder".into())))
    }

    unsafe fn setup(act: &IMFActivate, cfg: &HwEncoderConfig) -> Result<MfEncoder> {
        let mft: IMFTransform = act.ActivateObject().map_err(|e| err("activate encoder", e))?;
        let attrs = mft.GetAttributes().ok();
        let is_async = attrs.as_ref().and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok()).unwrap_or(0) != 0;
        if let Some(a) = &attrs {
            if is_async {
                a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| err("unlock async encoder", e))?;
            }
            let _ = a.SetUINT32(&MF_LOW_LATENCY, cfg.low_latency as u32);
        }
        let codec: Option<ICodecAPI> = mft.cast().ok();
        let apply_codec = |codec: &ICodecAPI| {
            let set = |g: &GUID, v: VARIANT| {
                let _ = codec.SetValue(g, &v);
            };
            match cfg.rate {
                RateControl::Cbr { kbps } => {
                    set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
                    set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(kbps.max(100) * 1000));
                    // One second of buffer, the usual live VBV.
                    set(&CODECAPI_AVEncCommonBufferSize, variant_u32(kbps.max(100) * 1000));
                }
                RateControl::Vbr { kbps, max_kbps } => {
                    set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32));
                    set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(kbps.max(100) * 1000));
                    set(&CODECAPI_AVEncCommonMaxBitRate, variant_u32(max_kbps.max(kbps).max(100) * 1000));
                }
                RateControl::Quality(q) => {
                    set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_Quality.0 as u32));
                    set(&CODECAPI_AVEncCommonQuality, variant_u32(q.clamp(1, 100) as u32));
                }
            }
            set(&CODECAPI_AVEncMPVGOPSize, variant_u32(cfg.gop()));
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0));
            set(&CODECAPI_AVEncCommonQualityVsSpeed, variant_u32(cfg.quality_vs_speed.min(100) as u32));
            set(&CODECAPI_AVLowLatencyMode, variant_bool(cfg.low_latency));
        };
        if let Some(c) = &codec {
            apply_codec(c);
        }

        // Encoders take the output type first.
        let size = ((cfg.width as u64) << 32) | cfg.height as u64;
        let rate = ((cfg.fps_num as u64) << 32) | cfg.fps_den as u64;
        let out = MFCreateMediaType().map_err(|e| err("media type", e))?;
        let e = |what: &'static str| move |x| err(what, x);
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("output type"))?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(e("output type"))?;
        let bps = match cfg.rate {
            RateControl::Cbr { kbps } | RateControl::Vbr { kbps, .. } => kbps.max(100) * 1000,
            RateControl::Quality(_) => 12_000_000,
        };
        out.SetUINT32(&MF_MT_AVG_BITRATE, bps).map_err(e("output type"))?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(e("output type"))?;
        out.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(e("output type"))?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).map_err(e("output type"))?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(e("output type"))?;
        let profile = match cfg.profile {
            Profile::Baseline => eAVEncH264VProfile_Base.0,
            Profile::Main => eAVEncH264VProfile_Main.0,
            Profile::High => eAVEncH264VProfile_High.0,
        };
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32).map_err(e("output type"))?;
        mft.SetOutputType(0, &out, 0).map_err(e("set H.264 output type"))?;

        // Input: the encoder's own NV12 type with our size / rate.
        let mut input = None;
        let mut i = 0;
        while let Ok(t) = mft.GetInputAvailableType(0, i) {
            i += 1;
            if t.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|g| g == MFVideoFormat_NV12) {
                input = Some(t);
                break;
            }
        }
        let input = match input {
            Some(t) => t,
            None => {
                let t = MFCreateMediaType().map_err(e("media type"))?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("input type"))?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(e("input type"))?;
                t
            }
        };
        input.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(e("input type"))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(e("input type"))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).map_err(e("input type"))?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(e("input type"))?;
        mft.SetInputType(0, &input, 0).map_err(e("set NV12 input type"))?;
        // Some encoders only take rate control once the types are known.
        if let Some(c) = &codec {
            apply_codec(c);
        }

        let info = mft.GetOutputStreamInfo(0).map_err(e("output stream info"))?;
        let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        let out_size = info.cbSize.max(cfg.width * cfg.height * 3 / 2);

        let mut enc = MfEncoder {
            events: if is_async { Some(mft.cast().map_err(e("event generator"))?) } else { None },
            mft,
            need_input: 0,
            provides_samples,
            out_size,
            avcc: None,
            sps: None,
            pps: None,
            fps: (cfg.fps_num, cfg.fps_den),
            // COM / MF start-up is reference counted: this encoder holds its own.
            _com: Com::init(),
        };
        // Parameter sets known up front?
        if let Ok(t) = enc.mft.GetOutputCurrentType(0) {
            enc.read_sequence_header(&t);
        }
        enc.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(e("begin streaming"))?;
        enc.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(e("start of stream"))?;
        Ok(enc)
    }

    unsafe fn read_sequence_header(&mut self, t: &IMFMediaType) {
        let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return };
        let mut blob = vec![0u8; n as usize];
        if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).is_ok() {
            let c = bitstream::convert(&blob);
            self.note_parameter_sets(c.sps, c.pps);
        }
    }

    fn note_parameter_sets(&mut self, sps: Option<Vec<u8>>, pps: Option<Vec<u8>>) {
        if sps.is_some() {
            self.sps = sps;
        }
        if pps.is_some() {
            self.pps = pps;
        }
        if self.avcc.is_none() {
            if let (Some(s), Some(p)) = (&self.sps, &self.pps) {
                self.avcc = Some(bitstream::avcc(s, p));
            }
        }
    }

    pub(crate) fn avcc(&self) -> Option<&[u8]> {
        self.avcc.as_deref()
    }

    fn to_100ns(&self, index: u64) -> (i64, i64) {
        let (n, d) = (self.fps.0 as i128, self.fps.1 as i128);
        let pts = (index as i128 * 10_000_000 * d + n / 2) / n;
        let dur = (10_000_000 * d + n / 2) / n;
        (pts as i64, dur as i64)
    }

    pub(crate) fn encode(&mut self, nv12: &[u8], index: u64, timescale: u32, frame_dur: u32) -> Result<Vec<Packet>> {
        let (pts, dur) = self.to_100ns(index);
        let sample = unsafe { make_sample(nv12, pts, dur) }?;
        let mut out = Vec::new();
        unsafe {
            if self.events.is_some() {
                let start = Instant::now();
                while self.need_input == 0 {
                    if !self.pump(&mut out, timescale, frame_dur)? {
                        if start.elapsed() > TIMEOUT {
                            return Err(Error::Encode("hardware encoder stopped asking for input".into()));
                        }
                        std::thread::sleep(Duration::from_micros(250));
                    }
                }
                self.mft.ProcessInput(0, &sample, 0).map_err(|e| err("ProcessInput", e))?;
                self.need_input -= 1;
                while self.pump(&mut out, timescale, frame_dur)? {}
            } else {
                loop {
                    match self.mft.ProcessInput(0, &sample, 0) {
                        Ok(()) => break,
                        Err(e) if e.code() == MF_E_NOTACCEPTING => {
                            while self.output(&mut out, timescale, frame_dur)? {}
                        }
                        Err(e) => return Err(err("ProcessInput", e)),
                    }
                }
                while self.output(&mut out, timescale, frame_dur)? {}
            }
        }
        Ok(out)
    }

    pub(crate) fn flush(&mut self, timescale: u32, frame_dur: u32) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0).map_err(|e| err("end of stream", e))?;
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).map_err(|e| err("drain", e))?;
            if let Some(ev) = self.events.clone() {
                let start = Instant::now();
                loop {
                    match ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                        Ok(e) => match MF_EVENT_TYPE(e.GetType().unwrap_or(0) as i32) {
                            t if t == METransformHaveOutput => {
                                self.output(&mut out, timescale, frame_dur)?;
                            }
                            t if t == METransformDrainComplete => break,
                            _ => {}
                        },
                        Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                            if start.elapsed() > TIMEOUT {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => return Err(err("encoder event", e)),
                    }
                }
            } else {
                while self.output(&mut out, timescale, frame_dur)? {}
            }
        }
        Ok(out)
    }

    /// Handle one pending event (async). Returns false when none was waiting.
    unsafe fn pump(&mut self, out: &mut Vec<Packet>, timescale: u32, frame_dur: u32) -> Result<bool> {
        let Some(ev) = self.events.clone() else { return Ok(false) };
        match ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
            Ok(e) => {
                let t = MF_EVENT_TYPE(e.GetType().unwrap_or(0) as i32);
                if t == METransformNeedInput {
                    self.need_input += 1;
                } else if t == METransformHaveOutput {
                    self.output(out, timescale, frame_dur)?;
                } else if t == MEError {
                    let status = e.GetStatus().map(|s| s.0).unwrap_or(0);
                    return Err(Error::Encode(format!("hardware encoder error 0x{status:08X}")));
                }
                Ok(true)
            }
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => Ok(false),
            Err(e) => Err(err("encoder event", e)),
        }
    }

    /// Pull one output sample. Returns false when the encoder has nothing (needs more input).
    unsafe fn output(&mut self, out: &mut Vec<Packet>, timescale: u32, frame_dur: u32) -> Result<bool> {
        let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(if self.provides_samples { None } else { Some(alloc_sample(self.out_size)?) }),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = self.mft.ProcessOutput(0, &mut bufs, &mut status);
        // The array never drops its ManuallyDrop fields: take both out to release them.
        let sample = ManuallyDrop::take(&mut bufs[0].pSample);
        drop(ManuallyDrop::take(&mut bufs[0].pEvents));
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // New output type (often carrying the sequence header): accept it and go on.
                if let Ok(t) = self.mft.GetOutputAvailableType(0, 0) {
                    self.read_sequence_header(&t);
                    self.mft.SetOutputType(0, &t, 0).map_err(|e| err("renegotiate output", e))?;
                }
                return Ok(true);
            }
            Err(e) => return Err(err("ProcessOutput", e)),
        }
        let Some(sample) = sample else { return Ok(true) };
        let time = sample.GetSampleTime().unwrap_or(0);
        let dts100 = sample.GetUINT64(&MFSampleExtension_DecodeTimestamp).map(|v| v as i64).unwrap_or(time);
        let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        let buffer = sample.ConvertToContiguousBuffer().map_err(|e| err("output buffer", e))?;
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut p, None, Some(&mut len)).map_err(|e| err("lock output", e))?;
        let data = std::slice::from_raw_parts(p, len as usize).to_vec();
        let _ = buffer.Unlock();
        if self.avcc.is_none() {
            if let Ok(t) = self.mft.GetOutputCurrentType(0) {
                self.read_sequence_header(&t);
            }
        }
        let c = bitstream::convert(&data);
        self.note_parameter_sets(c.sps, c.pps);
        if c.avcc_payload.is_empty() {
            return Ok(true);
        }
        let fd = frame_dur as i64;
        let snap = |t100: i64| -> i64 {
            let ticks = (t100 as i128 * timescale as i128 + 5_000_000) / 10_000_000;
            ((ticks as i64 + fd / 2).div_euclid(fd)) * fd
        };
        let pts = snap(time);
        let dts = snap(dts100).min(pts);
        out.push(Packet { data: c.avcc_payload, pts, dts, duration: frame_dur, keyframe: clean || c.idr });
        Ok(true)
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}

unsafe fn make_sample(data: &[u8], pts: i64, dur: i64) -> Result<IMFSample> {
    let e = |what: &'static str| move |x| err(what, x);
    let buf = MFCreateMemoryBuffer(data.len() as u32).map_err(e("input buffer"))?;
    let mut p: *mut u8 = std::ptr::null_mut();
    buf.Lock(&mut p, None, None).map_err(e("lock input"))?;
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    let _ = buf.Unlock();
    buf.SetCurrentLength(data.len() as u32).map_err(e("input length"))?;
    let s = MFCreateSample().map_err(e("input sample"))?;
    s.AddBuffer(&buf).map_err(e("input sample"))?;
    s.SetSampleTime(pts).map_err(e("sample time"))?;
    s.SetSampleDuration(dur).map_err(e("sample duration"))?;
    Ok(s)
}

unsafe fn alloc_sample(size: u32) -> Result<IMFSample> {
    let buf = MFCreateMemoryBuffer(size).map_err(|e| err("output buffer", e))?;
    let s = MFCreateSample().map_err(|e| err("output sample", e))?;
    s.AddBuffer(&buf).map_err(|e| err("output sample", e))?;
    Ok(s)
}
