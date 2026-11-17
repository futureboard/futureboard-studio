//! Media Foundation plumbing: runtime start-up and MFT discovery.

use ::windows::core::GUID;
use ::windows::Win32::Media::MediaFoundation::*;
use ::windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use crate::format::CodecFamily;

const MF_VERSION: u32 = 0x0002_0070;

/// COM (MTA) + Media Foundation for the lifetime of the value. Nests: every start is paired with
/// its own shutdown.
pub(crate) struct Runtime {
    com: bool,
}

impl Runtime {
    pub(crate) fn start() -> Runtime {
        unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
            Runtime { com }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com {
                CoUninitialize();
            }
        }
    }
}

pub(crate) fn subtype(codec: CodecFamily) -> Option<GUID> {
    Some(match codec {
        CodecFamily::H264 => MFVideoFormat_H264,
        CodecFamily::Hevc => MFVideoFormat_HEVC,
        CodecFamily::Vp8 => MFVideoFormat_VP80,
        CodecFamily::Vp9 => MFVideoFormat_VP90,
        CodecFamily::Av1 => MFVideoFormat_AV1,
        CodecFamily::Mpeg2 => MFVideoFormat_MPEG2,
        _ => return None,
    })
}

/// Activation objects for `category` MFTs matching the types, released by the caller.
pub(crate) unsafe fn enum_mfts(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<MFT_REGISTER_TYPE_INFO>,
    output: Option<MFT_REGISTER_TYPE_INFO>,
    adapter_luid: Option<u64>,
) -> Vec<IMFActivate> {
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let inp = input.as_ref().map(|i| i as *const _);
    let outp = output.as_ref().map(|o| o as *const _);
    let ok = match adapter_luid {
        Some(luid) => {
            let mut attrs: Option<IMFAttributes> = None;
            if MFCreateAttributes(&mut attrs, 1).is_err() {
                return Vec::new();
            }
            let Some(attrs) = attrs else {
                return Vec::new();
            };
            if attrs.SetUINT64(&MFT_ENUM_ADAPTER_LUID, luid).is_err() {
                return Vec::new();
            }
            MFTEnum2(category, flags, inp, outp, &attrs, &mut list, &mut count).is_ok()
        }
        None => MFTEnumEx(category, flags, inp, outp, &mut list, &mut count).is_ok(),
    };
    let mut out = Vec::new();
    if ok && !list.is_null() {
        for i in 0..count as usize {
            if let Some(a) = (*list.add(i)).take() {
                out.push(a);
            }
        }
        CoTaskMemFree(Some(list as *const _));
    }
    out
}

pub(crate) unsafe fn friendly_name(a: &IMFActivate) -> String {
    string_attr(a, &MFT_FRIENDLY_NAME_Attribute)
}

pub(crate) unsafe fn string_attr(a: &IMFActivate, key: &GUID) -> String {
    let mut p = ::windows::core::PWSTR::null();
    let mut len = 0u32;
    if a.GetAllocatedString(key, &mut p, &mut len).is_ok() && !p.is_null() {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    } else {
        String::new()
    }
}

/// Decoder MFTs (synchronous, DXVA-capable ones included) for `codec`, best first.
pub(crate) fn decoders(codec: CodecFamily) -> Vec<IMFActivate> {
    let Some(sub) = subtype(codec) else {
        return Vec::new();
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: sub,
    };
    let flags = MFT_ENUM_FLAG(
        MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_LOCALMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
    );
    unsafe { enum_mfts(MFT_CATEGORY_VIDEO_DECODER, flags, Some(input), None, None) }
}

pub(crate) fn has_decoder(codec: CodecFamily) -> bool {
    !decoders(codec).is_empty()
}

/// Hardware encoder MFTs bound to the adapter with `luid`: (codec, friendly name).
/// `MFTEnum2` filters by adapter only loosely (drivers register their encoders for every
/// adapter), so encoders whose hardware vendor id names another vendor are dropped.
pub(crate) fn hardware_encoders(luid: u64, vendor_id: u32) -> Vec<(CodecFamily, String)> {
    let _rt = Runtime::start();
    let mut out = Vec::new();
    for codec in [CodecFamily::H264, CodecFamily::Hevc, CodecFamily::Av1] {
        let Some(sub) = subtype(codec) else { continue };
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: sub,
        };
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
        for a in unsafe {
            enum_mfts(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(input),
                Some(output),
                Some(luid),
            )
        } {
            let ven = unsafe { string_attr(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute) }
                .to_ascii_uppercase();
            if !ven.is_empty() && !ven.contains(&format!("{vendor_id:04X}")) {
                continue;
            }
            out.push((codec, unsafe { friendly_name(&a) }));
        }
    }
    out
}
