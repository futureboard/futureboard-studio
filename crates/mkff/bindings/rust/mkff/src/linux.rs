use std::os::raw::c_char;

#[derive(Debug, Clone)]
pub struct DrmDeviceInfo {
    pub path: String,
    pub driver_name: String,
    pub vendor_id: u32,
    pub device_id: u32,
}

impl DrmDeviceInfo {
    pub(crate) fn from_raw(raw: &mkff_sys::MKFF_DrmDeviceInfo) -> Self {
        DrmDeviceInfo {
            path: c_char_array_to_string(&raw.path),
            driver_name: c_char_array_to_string(&raw.driver_name),
            vendor_id: raw.vendor_id,
            device_id: raw.device_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VaInfo {
    pub vendor_string: String,
    pub major_version: i32,
    pub minor_version: i32,
}

impl VaInfo {
    pub(crate) fn from_raw(raw: &mkff_sys::MKFF_VaInfo) -> Self {
        VaInfo {
            vendor_string: c_char_array_to_string(&raw.vendor_string),
            major_version: raw.major_version,
            minor_version: raw.minor_version,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VaProfileInfo {
    pub codec: mkff_sys::MKFF_VideoCodec,
    pub profile: mkff_sys::MKFF_VideoProfile,
    pub entrypoint: mkff_sys::MKFF_VideoEntrypoint,
}

impl VaProfileInfo {
    pub(crate) fn from_raw(raw: &mkff_sys::MKFF_VaProfileInfo) -> Self {
        VaProfileInfo {
            codec: raw.codec,
            profile: raw.profile,
            entrypoint: raw.entrypoint,
        }
    }
}

fn c_char_array_to_string(chars: &[c_char]) -> String {
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(chars.as_ptr().cast::<u8>(), chars.len()) };
    let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..nul]).into_owned()
}
