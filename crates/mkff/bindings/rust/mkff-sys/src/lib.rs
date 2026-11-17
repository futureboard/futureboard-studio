//! Raw `repr(C)` FFI declarations for libmkff. Hand-written to mirror
//! `include/mkff/*.h` field-for-field and function-for-function; no
//! bindgen, no build-time codegen. This crate is unsafe by construction
//! — see the `mkff` crate for a safe RAII wrapper.
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::os::raw::{c_char, c_void};

pub const MKFF_ABI_VERSION: u32 = 1;
pub const MKFF_PLATFORM_ABI_VERSION: u32 = 1;

pub const MKFF_TIMESTAMP_NONE: i64 = i64::MIN;

pub const MKFF_LINUX_DMABUF_MAX_OBJECTS: usize = 4;
pub const MKFF_LINUX_DMABUF_MAX_PLANES: usize = 4;

// ---------------------------------------------------------------------
// Opaque handles
// ---------------------------------------------------------------------

#[repr(C)]
pub struct MKFF_Context {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct MKFF_VideoDecoder {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct MKFF_VideoFrame {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct MKFF_Mp4Demux {
    _opaque: [u8; 0],
}

// ---------------------------------------------------------------------
// Enums (mkff/types.h, mkff/error.h, mkff/log.h)
// ---------------------------------------------------------------------

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MKFF_Result {
    MKFF_RESULT_OK = 0,
    MKFF_RESULT_NOT_READY = 1,
    MKFF_RESULT_END_OF_STREAM = 2,
    MKFF_RESULT_ERROR_INVALID_ARGUMENT = -1,
    MKFF_RESULT_ERROR_OUT_OF_MEMORY = -2,
    MKFF_RESULT_ERROR_ABI_MISMATCH = -3,
    MKFF_RESULT_ERROR_PLATFORM_LOAD = -4,
    MKFF_RESULT_ERROR_NOT_SUPPORTED = -5,
    MKFF_RESULT_ERROR_DEVICE = -6,
    MKFF_RESULT_ERROR_BITSTREAM = -7,
    MKFF_RESULT_ERROR_DECODE = -8,
    MKFF_RESULT_ERROR_POOL_EXHAUSTED = -9,
    MKFF_RESULT_ERROR_INTERNAL = -10,
    MKFF_RESULT_ERROR_CODEC_UNAVAILABLE = -11,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MKFF_LogLevel {
    MKFF_LOG_LEVEL_TRACE = 0,
    MKFF_LOG_LEVEL_DEBUG = 1,
    MKFF_LOG_LEVEL_INFO = 2,
    MKFF_LOG_LEVEL_WARN = 3,
    MKFF_LOG_LEVEL_ERROR = 4,
    MKFF_LOG_LEVEL_NONE = 5,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MKFF_PixelFormat {
    MKFF_PIXEL_FORMAT_UNKNOWN = 0,
    MKFF_PIXEL_FORMAT_NV12 = 1,
    MKFF_PIXEL_FORMAT_P010 = 2,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MKFF_VideoBackend {
    MKFF_VIDEO_BACKEND_AUTO = 0,
    MKFF_VIDEO_BACKEND_HARDWARE_ONLY = 1,
    MKFF_VIDEO_BACKEND_SOFTWARE_ONLY = 2,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MKFF_VideoCodec {
    MKFF_VIDEO_CODEC_UNKNOWN = 0,
    MKFF_VIDEO_CODEC_H264 = 1,
    MKFF_VIDEO_CODEC_HEVC = 2,
    MKFF_VIDEO_CODEC_VP9 = 3,
    MKFF_VIDEO_CODEC_AV1 = 4,
    MKFF_VIDEO_CODEC_VP8 = 5,
    MKFF_VIDEO_CODEC_MPEG1 = 6,
    MKFF_VIDEO_CODEC_MPEG2 = 7,
    MKFF_VIDEO_CODEC_PRORES = 8,
    MKFF_VIDEO_CODEC_DNXHD = 9,
    MKFF_VIDEO_CODEC_APV = 10,
    MKFF_VIDEO_CODEC_MJPEG = 11,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MKFF_VideoProfile {
    MKFF_VIDEO_PROFILE_UNKNOWN = 0,
    MKFF_VIDEO_PROFILE_H264_BASELINE = 100,
    MKFF_VIDEO_PROFILE_H264_MAIN = 101,
    MKFF_VIDEO_PROFILE_H264_HIGH = 102,
    MKFF_VIDEO_PROFILE_H264_CONSTRAINED_BASELINE = 103,
    MKFF_VIDEO_PROFILE_HEVC_MAIN = 200,
    MKFF_VIDEO_PROFILE_HEVC_MAIN10 = 201,
    MKFF_VIDEO_PROFILE_VP9_PROFILE0 = 300,
    MKFF_VIDEO_PROFILE_VP9_PROFILE2 = 301,
    MKFF_VIDEO_PROFILE_AV1_MAIN = 400,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MKFF_VideoEntrypoint {
    MKFF_VIDEO_ENTRYPOINT_UNKNOWN = 0,
    MKFF_VIDEO_ENTRYPOINT_VLD = 1,
}

pub type MKFF_LogCallback = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        level: MKFF_LogLevel,
        component: *const c_char,
        message: *const c_char,
    ),
>;

// ---------------------------------------------------------------------
// Structs (extensible: struct_size/abi_version/reserved header first)
// ---------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_ContextDesc {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub log_callback: MKFF_LogCallback,
    pub log_user_data: *mut c_void,
    pub log_min_level: MKFF_LogLevel,
    pub platform_module_path: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_VideoFrameInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub width: u32,
    pub height: u32,
    pub format: MKFF_PixelFormat,
    pub pts: i64,
    pub dts: i64,
    pub is_key_frame: u32,
    pub pad0: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_VideoDecoderDesc {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub codec: MKFF_VideoCodec,
    pub max_surfaces: u32,
    pub width_hint: u32,
    pub height_hint: u32,
    pub backend: MKFF_VideoBackend,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_VideoDecoderInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub width: u32,
    pub height: u32,
    pub profile: MKFF_VideoProfile,
    pub entrypoint: MKFF_VideoEntrypoint,
    pub output_format: MKFF_PixelFormat,
    pub surface_pool_size: u32,
    pub surface_pool_capacity: u32,
    pub backend: MKFF_VideoBackend,
    pub bit_depth: u32,
    pub chroma_format_idc: u32,
    pub hardware: u32,
}

pub const MKFF_CPU_PLANES_MAX: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_Mp4VideoTrackInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub codec: MKFF_VideoCodec,
    pub width: u32,
    pub height: u32,
    pub timescale: u32,
    pub duration: u64,
    pub sample_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_Mp4AccessUnit {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub data: *const u8,
    pub size: usize,
    pub pts: i64,
    pub dts: i64,
    pub sync: u32,
    pub sample_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_CpuPlaneDesc {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub format: MKFF_PixelFormat,
    pub width: u32,
    pub height: u32,
    pub plane_count: u32,
    pub data: [*const u8; MKFF_CPU_PLANES_MAX],
    pub stride: [u32; MKFF_CPU_PLANES_MAX],
    pub height_lines: [u32; MKFF_CPU_PLANES_MAX],
}

// Linux VA-API / dma-buf extensions — only exported from libmkff on Linux.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_DrmDeviceInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub path: [c_char; 128],
    pub driver_name: [c_char; 64],
    pub vendor_id: u32,
    pub device_id: u32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_VaInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub vendor_string: [c_char; 256],
    pub major_version: i32,
    pub minor_version: i32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_VaProfileInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub codec: MKFF_VideoCodec,
    pub profile: MKFF_VideoProfile,
    pub entrypoint: MKFF_VideoEntrypoint,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_LinuxDmaBufObject {
    pub fd: i32,
    pub size: u32,
    pub modifier: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_LinuxDmaBufPlane {
    pub object_index: u32,
    pub offset: u32,
    pub pitch: u32,
    pub pad0: u32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MKFF_LinuxDmaBufDesc {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub drm_fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [MKFF_LinuxDmaBufObject; MKFF_LINUX_DMABUF_MAX_OBJECTS],
    pub num_planes: u32,
    pub planes: [MKFF_LinuxDmaBufPlane; MKFF_LINUX_DMABUF_MAX_PLANES],
}

// ---------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------

extern "C" {
    pub fn mkff_result_to_string(result: MKFF_Result) -> *const c_char;

    pub fn mkff_context_create(desc: *const MKFF_ContextDesc, out_context: *mut *mut MKFF_Context) -> MKFF_Result;
    pub fn mkff_context_destroy(context: *mut MKFF_Context);
    pub fn mkff_context_get_last_error(context: *mut MKFF_Context) -> *const c_char;

    pub fn mkff_video_decoder_create(
        context: *mut MKFF_Context,
        desc: *const MKFF_VideoDecoderDesc,
        out_decoder: *mut *mut MKFF_VideoDecoder,
    ) -> MKFF_Result;
    pub fn mkff_video_decoder_destroy(decoder: *mut MKFF_VideoDecoder);
    pub fn mkff_video_decoder_submit(
        decoder: *mut MKFF_VideoDecoder,
        annex_b_data: *const u8,
        annex_b_size: usize,
        pts: i64,
        dts: i64,
    ) -> MKFF_Result;
    pub fn mkff_video_decoder_receive(decoder: *mut MKFF_VideoDecoder, out_frame: *mut *mut MKFF_VideoFrame) -> MKFF_Result;
    pub fn mkff_video_decoder_flush(decoder: *mut MKFF_VideoDecoder) -> MKFF_Result;
    pub fn mkff_video_decoder_get_info(decoder: *const MKFF_VideoDecoder, out_info: *mut MKFF_VideoDecoderInfo) -> MKFF_Result;

    pub fn mkff_video_frame_retain(frame: *mut MKFF_VideoFrame) -> *mut MKFF_VideoFrame;
    pub fn mkff_video_frame_release(frame: *mut MKFF_VideoFrame);
    pub fn mkff_video_frame_get_info(frame: *const MKFF_VideoFrame, out_info: *mut MKFF_VideoFrameInfo) -> MKFF_Result;
    pub fn mkff_video_frame_map_cpu_planes(
        frame: *const MKFF_VideoFrame,
        out_planes: *mut MKFF_CpuPlaneDesc,
    ) -> MKFF_Result;
    pub fn mkff_video_frame_unmap_cpu_planes(frame: *const MKFF_VideoFrame, planes: *mut MKFF_CpuPlaneDesc);

    pub fn mkff_mp4_demux_open_path(path: *const c_char, out_demux: *mut *mut MKFF_Mp4Demux) -> MKFF_Result;
    pub fn mkff_mp4_demux_open_memory(
        data: *const u8,
        size: usize,
        out_demux: *mut *mut MKFF_Mp4Demux,
    ) -> MKFF_Result;
    pub fn mkff_mp4_demux_destroy(demux: *mut MKFF_Mp4Demux);
    pub fn mkff_mp4_demux_get_video_track(
        demux: *const MKFF_Mp4Demux,
        out_info: *mut MKFF_Mp4VideoTrackInfo,
    ) -> MKFF_Result;
    pub fn mkff_mp4_demux_read_access_unit(
        demux: *mut MKFF_Mp4Demux,
        out_au: *mut MKFF_Mp4AccessUnit,
    ) -> MKFF_Result;
    pub fn mkff_mp4_demux_seek_sample(demux: *mut MKFF_Mp4Demux, sample_index: u32) -> MKFF_Result;
}

#[cfg(target_os = "linux")]
extern "C" {
    pub fn mkff_linux_enumerate_drm_devices(
        context: *mut MKFF_Context,
        out_array: *mut MKFF_DrmDeviceInfo,
        array_capacity: u32,
        out_count: *mut u32,
    ) -> MKFF_Result;

    pub fn mkff_linux_query_va_info(context: *mut MKFF_Context, drm_device_path: *const c_char, out_info: *mut MKFF_VaInfo) -> MKFF_Result;
    pub fn mkff_linux_query_va_profiles(
        context: *mut MKFF_Context,
        drm_device_path: *const c_char,
        out_array: *mut MKFF_VaProfileInfo,
        array_capacity: u32,
        out_count: *mut u32,
    ) -> MKFF_Result;

    pub fn mkff_linux_video_frame_export_dmabuf(frame: *const MKFF_VideoFrame, out_desc: *mut MKFF_LinuxDmaBufDesc) -> MKFF_Result;
    pub fn mkff_linux_dmabuf_desc_close(desc: *mut MKFF_LinuxDmaBufDesc);
}
