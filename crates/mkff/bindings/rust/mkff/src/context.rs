use std::ptr;

use crate::decoder::VideoDecoder;
use crate::error::{check, Result};
#[cfg(target_os = "linux")]
use crate::linux::{DrmDeviceInfo, VaInfo, VaProfileInfo};

/// Owns one `MKFF_Context`. Dropping it destroys the underlying C
/// context (and, transitively, unloads the platform module once every
/// decoder/frame it produced has also been dropped).
///
/// Not `Send`/`Sync`: the C context serializes only its own
/// bookkeeping (last-error string, platform module lifetime), not
/// decoder/frame operations, so nothing here guarantees safety across
/// threads without external synchronization the C API doesn't promise.
pub struct Context {
    ptr: *mut mkff_sys::MKFF_Context,
}

impl Context {
    pub fn new() -> Result<Self> {
        let mut ptr = ptr::null_mut();
        let result = unsafe { mkff_sys::mkff_context_create(ptr::null(), &mut ptr) };
        check(result)?;
        Ok(Context { ptr })
    }

    pub(crate) fn as_raw(&self) -> *mut mkff_sys::MKFF_Context {
        self.ptr
    }

    pub fn last_error(&self) -> String {
        unsafe {
            let p = mkff_sys::mkff_context_get_last_error(self.ptr);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    pub fn video_decoder_h264(&self, max_surfaces: u32) -> Result<VideoDecoder<'_>> {
        self.video_decoder(
            mkff_sys::MKFF_VideoCodec::MKFF_VIDEO_CODEC_H264,
            max_surfaces,
            mkff_sys::MKFF_VideoBackend::MKFF_VIDEO_BACKEND_AUTO,
        )
    }

    pub fn video_decoder_hevc(&self, max_surfaces: u32) -> Result<VideoDecoder<'_>> {
        self.video_decoder(
            mkff_sys::MKFF_VideoCodec::MKFF_VIDEO_CODEC_HEVC,
            max_surfaces,
            mkff_sys::MKFF_VideoBackend::MKFF_VIDEO_BACKEND_AUTO,
        )
    }

    pub fn video_decoder_hevc_with_backend(
        &self,
        max_surfaces: u32,
        backend: mkff_sys::MKFF_VideoBackend,
    ) -> Result<VideoDecoder<'_>> {
        self.video_decoder(
            mkff_sys::MKFF_VideoCodec::MKFF_VIDEO_CODEC_HEVC,
            max_surfaces,
            backend,
        )
    }

    /// Creates a decoder for `codec` with the given backend selection.
    pub fn video_decoder(
        &self,
        codec: mkff_sys::MKFF_VideoCodec,
        max_surfaces: u32,
        backend: mkff_sys::MKFF_VideoBackend,
    ) -> Result<VideoDecoder<'_>> {
        VideoDecoder::create(self, codec, max_surfaces, backend)
    }

    #[cfg(target_os = "linux")]
    pub fn linux_enumerate_drm_devices(&self) -> Result<Vec<DrmDeviceInfo>> {
        let mut count: u32 = 0;
        check(unsafe { mkff_sys::mkff_linux_enumerate_drm_devices(self.ptr, ptr::null_mut(), 0, &mut count) })?;
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut buf = vec![zeroed_drm_device_info(); count as usize];
        let mut actual: u32 = 0;
        check(unsafe { mkff_sys::mkff_linux_enumerate_drm_devices(self.ptr, buf.as_mut_ptr(), count, &mut actual) })?;
        buf.truncate(actual as usize);
        Ok(buf.iter().map(DrmDeviceInfo::from_raw).collect())
    }

    #[cfg(target_os = "linux")]
    pub fn linux_query_va_info(&self, drm_device_path: Option<&str>) -> Result<VaInfo> {
        let c_path = drm_device_path.map(|p| std::ffi::CString::new(p).expect("path must not contain NUL"));
        let path_ptr = c_path.as_ref().map_or(ptr::null(), |c| c.as_ptr());

        let mut raw = zeroed_va_info();
        check(unsafe { mkff_sys::mkff_linux_query_va_info(self.ptr, path_ptr, &mut raw) })?;
        Ok(VaInfo::from_raw(&raw))
    }

    #[cfg(target_os = "linux")]
    pub fn linux_query_va_profiles(&self, drm_device_path: Option<&str>) -> Result<Vec<VaProfileInfo>> {
        let c_path = drm_device_path.map(|p| std::ffi::CString::new(p).expect("path must not contain NUL"));
        let path_ptr = c_path.as_ref().map_or(ptr::null(), |c| c.as_ptr());

        let mut count: u32 = 0;
        check(unsafe { mkff_sys::mkff_linux_query_va_profiles(self.ptr, path_ptr, ptr::null_mut(), 0, &mut count) })?;
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut buf = vec![zeroed_va_profile_info(); count as usize];
        let mut actual: u32 = 0;
        check(unsafe { mkff_sys::mkff_linux_query_va_profiles(self.ptr, path_ptr, buf.as_mut_ptr(), count, &mut actual) })?;
        buf.truncate(actual as usize);
        Ok(buf.iter().map(VaProfileInfo::from_raw).collect())
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { mkff_sys::mkff_context_destroy(self.ptr) };
    }
}

#[cfg(target_os = "linux")]
fn zeroed_drm_device_info() -> mkff_sys::MKFF_DrmDeviceInfo {
    let mut v: mkff_sys::MKFF_DrmDeviceInfo = unsafe { std::mem::zeroed() };
    v.struct_size = std::mem::size_of::<mkff_sys::MKFF_DrmDeviceInfo>() as u32;
    v.abi_version = mkff_sys::MKFF_ABI_VERSION;
    v
}

#[cfg(target_os = "linux")]
fn zeroed_va_info() -> mkff_sys::MKFF_VaInfo {
    let mut v: mkff_sys::MKFF_VaInfo = unsafe { std::mem::zeroed() };
    v.struct_size = std::mem::size_of::<mkff_sys::MKFF_VaInfo>() as u32;
    v.abi_version = mkff_sys::MKFF_ABI_VERSION;
    v
}

#[cfg(target_os = "linux")]
fn zeroed_va_profile_info() -> mkff_sys::MKFF_VaProfileInfo {
    let mut v: mkff_sys::MKFF_VaProfileInfo = unsafe { std::mem::zeroed() };
    v.struct_size = std::mem::size_of::<mkff_sys::MKFF_VaProfileInfo>() as u32;
    v.abi_version = mkff_sys::MKFF_ABI_VERSION;
    v
}
