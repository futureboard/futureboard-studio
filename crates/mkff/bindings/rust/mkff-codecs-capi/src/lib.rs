//! C ABI for `mkff-codecs`: the functions declared in `include/mkff/codecs.h`.
//!
//! Every entry point checks its pointers, catches panics (nothing unwinds into C) and reports
//! failures as an `MKFF_Result` plus a thread-local message (`mkff_sw_last_error`).

#![allow(non_camel_case_types, clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use mkff_codecs::{
    AudioBuffer, AudioCodec, AudioDecoder, ChromaFormat, Error, Frame, Matrix, MediaFile,
    PixelLayout, Planes, VideoCodec, VideoDecoder,
};

pub type MKFF_Result = i32;
pub const MKFF_RESULT_OK: MKFF_Result = 0;
pub const MKFF_RESULT_NOT_READY: MKFF_Result = 1;
pub const MKFF_RESULT_END_OF_STREAM: MKFF_Result = 2;
pub const MKFF_RESULT_ERROR_INVALID_ARGUMENT: MKFF_Result = -1;
pub const MKFF_RESULT_ERROR_ABI_MISMATCH: MKFF_Result = -3;
pub const MKFF_RESULT_ERROR_NOT_SUPPORTED: MKFF_Result = -5;
pub const MKFF_RESULT_ERROR_DECODE: MKFF_Result = -8;
pub const MKFF_RESULT_ERROR_INTERNAL: MKFF_Result = -10;
pub const MKFF_RESULT_ERROR_CODEC_UNAVAILABLE: MKFF_Result = -11;

pub const MKFF_ABI_VERSION: u32 = 1;
pub const MKFF_SW_MAX_PLANES: usize = 4;

pub const MKFF_SW_LAYOUT_YUV: i32 = 1;
pub const MKFF_SW_LAYOUT_RGBA8: i32 = 2;
pub const MKFF_SW_LAYOUT_RGBA_F32: i32 = 3;

// ---------------------------------------------------------------------
// Structs (field-for-field with mkff/codecs.h)
// ---------------------------------------------------------------------

#[repr(C)]
pub struct MKFF_SwFrameInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub width: u32,
    pub height: u32,
    pub layout: i32,
    pub chroma_format_idc: u32,
    pub bit_depth: u32,
    pub has_alpha: u32,
    pub full_range: u32,
    pub matrix: u32,
    pub par_num: u32,
    pub par_den: u32,
    pub pts: i64,
    pub plane_count: u32,
    pub data: [*const c_void; MKFF_SW_MAX_PLANES],
    pub stride: [u32; MKFF_SW_MAX_PLANES],
    pub height_lines: [u32; MKFF_SW_MAX_PLANES],
}

#[repr(C)]
pub struct MKFF_SwAudioStreamInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub codec: [c_char; 48],
}

#[repr(C)]
pub struct MKFF_SwMediaInfo {
    pub struct_size: u32,
    pub abi_version: u32,
    pub reserved: [u32; 4],
    pub duration_seconds: f64,
    pub has_video: u32,
    pub width: u32,
    pub height: u32,
    pub has_alpha: u32,
    pub frame_rate_num: i64,
    pub frame_rate_den: i64,
    pub frame_count: u64,
    pub audio_stream_count: u32,
    pub container: [c_char; 32],
    pub video_codec: [c_char; 48],
    pub pixel_format: [c_char; 32],
}

// Opaque handles.
pub struct MKFF_SwMedia(MediaFile);
pub struct MKFF_SwFrame(Frame);
pub struct MKFF_SwAudio {
    sample_rate: u32,
    channels: u32,
    interleaved: Vec<f32>,
}
pub struct MKFF_SwVideoDecoder {
    dec: VideoDecoder,
    queue: VecDeque<Frame>,
    flushed: bool,
}
pub struct MKFF_SwAudioDecoder(AudioDecoder);

// ---------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(msg: impl Into<String>) {
    let msg = msg.into().replace('\0', " ");
    LAST_ERROR.with(|e| *e.borrow_mut() = CString::new(msg).unwrap_or_default());
}

/// An `Error` as a result code (message recorded).
fn fail(e: Error) -> MKFF_Result {
    let code = match &e {
        Error::Unsupported(_) => MKFF_RESULT_ERROR_NOT_SUPPORTED,
        Error::Decode(_) => MKFF_RESULT_ERROR_DECODE,
        Error::Encode(_) => MKFF_RESULT_ERROR_INTERNAL,
        Error::Io(_) | Error::NoStream(_) | Error::InvalidArgument(_) => {
            MKFF_RESULT_ERROR_INVALID_ARGUMENT
        }
    };
    set_error(e.to_string());
    code
}

fn invalid(what: &str) -> MKFF_Result {
    set_error(format!("invalid argument: {what}"));
    MKFF_RESULT_ERROR_INVALID_ARGUMENT
}

/// Run an entry point: clear the error, catch panics.
fn guard(f: impl FnOnce() -> MKFF_Result) -> MKFF_Result {
    LAST_ERROR.with(|e| *e.borrow_mut() = CString::default());
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        set_error(format!("internal error: {msg}"));
        MKFF_RESULT_ERROR_INTERNAL
    })
}

/// Checks an out-struct's caller-declared size (0 = "fill it all"), then writes `value`.
unsafe fn write_struct<T>(out: *mut T, value: impl FnOnce(u32) -> T) -> MKFF_Result {
    if out.is_null() {
        return invalid("null output struct");
    }
    let declared = ptr::read(out as *const u32);
    let size = std::mem::size_of::<T>() as u32;
    if declared != 0 && declared < size {
        set_error(format!(
            "struct_size {declared} smaller than {size}: rebuild against this mkff/codecs.h"
        ));
        return MKFF_RESULT_ERROR_ABI_MISMATCH;
    }
    ptr::write(out, value(size));
    MKFF_RESULT_OK
}

fn c_str_array<const N: usize>(s: &str) -> [c_char; N] {
    let mut out = [0 as c_char; N];
    // truncate on a char boundary, leaving room for the terminator
    let mut end = s.len().min(N - 1);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    for (o, b) in out.iter_mut().zip(&s.as_bytes()[..end]) {
        *o = *b as c_char;
    }
    out
}

unsafe fn bytes<'a>(data: *const u8, size: usize) -> Option<&'a [u8]> {
    if size == 0 {
        Some(&[])
    } else if data.is_null() {
        None
    } else {
        Some(std::slice::from_raw_parts(data, size))
    }
}

fn video_codec(c: i32) -> Option<VideoCodec> {
    Some(match c {
        1 => VideoCodec::H264,
        2 => VideoCodec::Hevc,
        3 => VideoCodec::Vp9,
        4 => VideoCodec::Av1,
        5 => VideoCodec::Vp8,
        6 => VideoCodec::Mpeg1,
        7 => VideoCodec::Mpeg2,
        8 => VideoCodec::ProRes,
        9 => VideoCodec::Dnx,
        10 => VideoCodec::Apv,
        11 => VideoCodec::Mjpeg,
        _ => return None,
    })
}

fn audio_codec(c: i32) -> Option<AudioCodec> {
    Some(match c {
        1 => AudioCodec::Aac,
        2 => AudioCodec::Opus,
        3 => AudioCodec::Ac3,
        4 => AudioCodec::MpegAudio,
        5 => AudioCodec::Flac,
        6 => AudioCodec::Alac,
        7 => AudioCodec::Vorbis,
        _ => return None,
    })
}

fn static_cstr(s: &'static CStr) -> *const c_char {
    s.as_ptr()
}

fn audio_handle(b: AudioBuffer) -> *mut MKFF_SwAudio {
    Box::into_raw(Box::new(MKFF_SwAudio {
        sample_rate: b.sample_rate,
        channels: b.channel_count() as u32,
        interleaved: b.interleaved(),
    }))
}

// ---------------------------------------------------------------------
// Errors and names
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn mkff_sw_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn mkff_sw_video_codec_name(codec: i32) -> *const c_char {
    match video_codec(codec) {
        Some(VideoCodec::H264) => static_cstr(c"h264"),
        Some(VideoCodec::Hevc) => static_cstr(c"hevc"),
        Some(VideoCodec::Vp8) => static_cstr(c"vp8"),
        Some(VideoCodec::Vp9) => static_cstr(c"vp9"),
        Some(VideoCodec::Av1) => static_cstr(c"av1"),
        Some(VideoCodec::Mpeg1) => static_cstr(c"mpeg1"),
        Some(VideoCodec::Mpeg2) => static_cstr(c"mpeg2"),
        Some(VideoCodec::ProRes) => static_cstr(c"prores"),
        Some(VideoCodec::Dnx) => static_cstr(c"dnxhd"),
        Some(VideoCodec::Apv) => static_cstr(c"apv"),
        Some(VideoCodec::Mjpeg) => static_cstr(c"mjpeg"),
        None => static_cstr(c"unknown"),
    }
}

#[no_mangle]
pub extern "C" fn mkff_sw_audio_codec_name(codec: i32) -> *const c_char {
    match audio_codec(codec) {
        Some(AudioCodec::Aac) => static_cstr(c"aac"),
        Some(AudioCodec::Opus) => static_cstr(c"opus"),
        Some(AudioCodec::Ac3) => static_cstr(c"ac3"),
        Some(AudioCodec::MpegAudio) => static_cstr(c"mpeg-audio"),
        Some(AudioCodec::Flac) => static_cstr(c"flac"),
        Some(AudioCodec::Alac) => static_cstr(c"alac"),
        Some(AudioCodec::Vorbis) => static_cstr(c"vorbis"),
        None => static_cstr(c"unknown"),
    }
}

// ---------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_frame_get_info(
    frame: *const MKFF_SwFrame,
    out_info: *mut MKFF_SwFrameInfo,
) -> MKFF_Result {
    guard(|| {
        let Some(MKFF_SwFrame(f)) = frame.as_ref() else {
            return invalid("null frame");
        };
        let (w, h, cw, ch) = (f.width(), f.height(), f.chroma_width(), f.chroma_height());
        let mut data = [ptr::null::<c_void>(); MKFF_SW_MAX_PLANES];
        let mut stride = [0u32; MKFF_SW_MAX_PLANES];
        let mut lines = [0u32; MKFF_SW_MAX_PLANES];
        let mut plane = |i: usize, p: *const c_void, s: u32, l: u32| {
            data[i] = p;
            stride[i] = s;
            lines[i] = l;
        };
        let count = match f.planes() {
            Planes::U8 { y, u, v, alpha } => {
                plane(0, y.as_ptr().cast(), w, h);
                plane(1, u.as_ptr().cast(), cw, ch);
                plane(2, v.as_ptr().cast(), cw, ch);
                alpha.map_or(3, |a| {
                    plane(3, a.as_ptr().cast(), w, h);
                    4
                })
            }
            Planes::U16 { y, u, v, alpha } => {
                plane(0, y.as_ptr().cast(), w * 2, h);
                plane(1, u.as_ptr().cast(), cw * 2, ch);
                plane(2, v.as_ptr().cast(), cw * 2, ch);
                alpha.map_or(3, |a| {
                    plane(3, a.as_ptr().cast(), w * 2, h);
                    4
                })
            }
            Planes::Rgba8(d) => {
                plane(0, d.as_ptr().cast(), w * 4, h);
                1
            }
            Planes::RgbaF32(d) => {
                plane(0, d.as_ptr().cast(), w * 16, h);
                1
            }
        };
        let (layout, chroma_format_idc, bit_depth, has_alpha) = match f.layout() {
            PixelLayout::Yuv {
                chroma,
                bit_depth,
                alpha,
            } => {
                let idc = match chroma {
                    ChromaFormat::Yuv420 => 1,
                    ChromaFormat::Yuv422 => 2,
                    ChromaFormat::Yuv444 => 3,
                };
                (MKFF_SW_LAYOUT_YUV, idc, bit_depth, alpha)
            }
            PixelLayout::Rgba8 => (MKFF_SW_LAYOUT_RGBA8, 0, 8, true),
            PixelLayout::RgbaF32 => (MKFF_SW_LAYOUT_RGBA_F32, 0, 32, true),
        };
        let color = f.color();
        let (par_num, par_den) = f.pixel_aspect();
        write_struct(out_info, |size| MKFF_SwFrameInfo {
            struct_size: size,
            abi_version: MKFF_ABI_VERSION,
            reserved: [0; 4],
            width: w,
            height: h,
            layout,
            chroma_format_idc,
            bit_depth,
            has_alpha: has_alpha as u32,
            full_range: color.full_range as u32,
            matrix: match color.matrix {
                Matrix::Bt709 => 1,
                Matrix::Bt601 => 6,
                Matrix::Bt2020Ncl => 9,
            },
            par_num,
            par_den,
            pts: f.pts(),
            plane_count: count,
            data,
            stride,
            height_lines: lines,
        })
    })
}

unsafe fn copy_out<T: Copy>(
    frame: *const MKFF_SwFrame,
    dst: *mut T,
    len: usize,
    convert: impl FnOnce(&Frame) -> Vec<T>,
) -> MKFF_Result {
    guard(|| {
        let Some(MKFF_SwFrame(f)) = frame.as_ref() else {
            return invalid("null frame");
        };
        if dst.is_null() {
            return invalid("null destination");
        }
        let v = convert(f);
        if len < v.len() {
            return invalid(&format!(
                "destination holds {len} elements, {} needed",
                v.len()
            ));
        }
        ptr::copy_nonoverlapping(v.as_ptr(), dst, v.len());
        MKFF_RESULT_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_frame_copy_nv12(
    frame: *const MKFF_SwFrame,
    dst: *mut u8,
    dst_size: usize,
) -> MKFF_Result {
    copy_out(frame, dst, dst_size, Frame::to_nv12)
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_frame_copy_p010(
    frame: *const MKFF_SwFrame,
    dst: *mut u16,
    dst_count: usize,
) -> MKFF_Result {
    copy_out(frame, dst, dst_count, Frame::to_p010)
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_frame_copy_rgba8(
    frame: *const MKFF_SwFrame,
    dst: *mut u8,
    dst_size: usize,
) -> MKFF_Result {
    copy_out(frame, dst, dst_size, Frame::to_rgba8)
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_frame_destroy(frame: *mut MKFF_SwFrame) {
    if !frame.is_null() {
        drop(Box::from_raw(frame));
    }
}

// ---------------------------------------------------------------------
// Audio buffers
// ---------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_audio_get(
    audio: *const MKFF_SwAudio,
    out_sample_rate: *mut u32,
    out_channels: *mut u32,
    out_frames: *mut usize,
    out_interleaved: *mut *const f32,
) -> MKFF_Result {
    guard(|| {
        let Some(a) = audio.as_ref() else {
            return invalid("null audio");
        };
        if let Some(o) = out_sample_rate.as_mut() {
            *o = a.sample_rate;
        }
        if let Some(o) = out_channels.as_mut() {
            *o = a.channels;
        }
        if let Some(o) = out_frames.as_mut() {
            *o = if a.channels == 0 {
                0
            } else {
                a.interleaved.len() / a.channels as usize
            };
        }
        if let Some(o) = out_interleaved.as_mut() {
            *o = a.interleaved.as_ptr();
        }
        MKFF_RESULT_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_audio_destroy(audio: *mut MKFF_SwAudio) {
    if !audio.is_null() {
        drop(Box::from_raw(audio));
    }
}

// ---------------------------------------------------------------------
// Media files
// ---------------------------------------------------------------------

unsafe fn store<T>(out: *mut *mut T, value: T) -> MKFF_Result {
    *out = Box::into_raw(Box::new(value));
    MKFF_RESULT_OK
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_open_path(
    path: *const c_char,
    out_media: *mut *mut MKFF_SwMedia,
) -> MKFF_Result {
    guard(|| {
        if path.is_null() || out_media.is_null() {
            return invalid("null path or output");
        }
        *out_media = ptr::null_mut();
        let Ok(p) = CStr::from_ptr(path).to_str() else {
            return invalid("path is not UTF-8");
        };
        match MediaFile::open(p) {
            Ok(m) => store(out_media, MKFF_SwMedia(m)),
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_open_memory(
    data: *const u8,
    size: usize,
    name_hint: *const c_char,
    out_media: *mut *mut MKFF_SwMedia,
) -> MKFF_Result {
    guard(|| {
        if out_media.is_null() {
            return invalid("null output");
        }
        *out_media = ptr::null_mut();
        let Some(b) = bytes(data, size).filter(|b| !b.is_empty()) else {
            return invalid("no data");
        };
        let name = if name_hint.is_null() {
            String::new()
        } else {
            CStr::from_ptr(name_hint).to_string_lossy().into_owned()
        };
        match MediaFile::from_bytes(&name, b.to_vec()) {
            Ok(m) => store(out_media, MKFF_SwMedia(m)),
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_destroy(media: *mut MKFF_SwMedia) {
    if !media.is_null() {
        drop(Box::from_raw(media));
    }
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_get_info(
    media: *const MKFF_SwMedia,
    out_info: *mut MKFF_SwMediaInfo,
) -> MKFF_Result {
    guard(|| {
        let Some(MKFF_SwMedia(m)) = media.as_ref() else {
            return invalid("null media");
        };
        let i = m.info();
        let v = i.video.as_ref();
        write_struct(out_info, |size| MKFF_SwMediaInfo {
            struct_size: size,
            abi_version: MKFF_ABI_VERSION,
            reserved: [0; 4],
            duration_seconds: i.duration_seconds,
            has_video: v.is_some() as u32,
            width: v.map_or(0, |v| v.width),
            height: v.map_or(0, |v| v.height),
            has_alpha: v.is_some_and(|v| v.has_alpha) as u32,
            frame_rate_num: v.map_or(0, |v| v.frame_rate.0),
            frame_rate_den: v.map_or(1, |v| v.frame_rate.1),
            frame_count: m.frame_count(),
            audio_stream_count: i.audio.len() as u32,
            container: c_str_array(&i.container),
            video_codec: c_str_array(v.map_or("", |v| &v.codec)),
            pixel_format: c_str_array(v.map_or("", |v| &v.pixel_format)),
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_get_audio_stream_info(
    media: *const MKFF_SwMedia,
    stream: u32,
    out_info: *mut MKFF_SwAudioStreamInfo,
) -> MKFF_Result {
    guard(|| {
        let Some(MKFF_SwMedia(m)) = media.as_ref() else {
            return invalid("null media");
        };
        let Some(a) = m.info().audio.get(stream as usize) else {
            return fail(Error::NoStream("audio"));
        };
        write_struct(out_info, |size| MKFF_SwAudioStreamInfo {
            struct_size: size,
            abi_version: MKFF_ABI_VERSION,
            reserved: [0; 4],
            sample_rate: a.sample_rate,
            channels: a.channels,
            bits_per_sample: a.bits_per_sample.unwrap_or(0),
            codec: c_str_array(&a.codec),
        })
    })
}

unsafe fn read_frame(
    media: *const MKFF_SwMedia,
    out_frame: *mut *mut MKFF_SwFrame,
    read: impl FnOnce(&MediaFile) -> mkff_codecs::Result<Frame>,
) -> MKFF_Result {
    guard(|| {
        if out_frame.is_null() {
            return invalid("null output");
        }
        *out_frame = ptr::null_mut();
        let Some(MKFF_SwMedia(m)) = media.as_ref() else {
            return invalid("null media");
        };
        match read(m) {
            Ok(f) => store(out_frame, MKFF_SwFrame(f)),
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_read_video_frame(
    media: *const MKFF_SwMedia,
    index: u64,
    out_frame: *mut *mut MKFF_SwFrame,
) -> MKFF_Result {
    read_frame(media, out_frame, |m| m.video_frame(index))
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_read_video_frame_at(
    media: *const MKFF_SwMedia,
    seconds: f64,
    out_frame: *mut *mut MKFF_SwFrame,
) -> MKFF_Result {
    read_frame(media, out_frame, |m| m.video_frame_at(seconds))
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_media_read_audio(
    media: *const MKFF_SwMedia,
    stream: u32,
    start: i64,
    frames: u32,
    sample_rate: u32,
    out_audio: *mut *mut MKFF_SwAudio,
) -> MKFF_Result {
    guard(|| {
        if out_audio.is_null() {
            return invalid("null output");
        }
        *out_audio = ptr::null_mut();
        let Some(MKFF_SwMedia(m)) = media.as_ref() else {
            return invalid("null media");
        };
        match m.audio_stream(stream as usize, start, frames as usize, sample_rate) {
            Ok(b) => {
                *out_audio = audio_handle(b);
                MKFF_RESULT_OK
            }
            Err(e) => fail(e),
        }
    })
}

// ---------------------------------------------------------------------
// Elementary stream decoders
// ---------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_video_decoder_create(
    codec: i32,
    config: *const u8,
    config_size: usize,
    out_decoder: *mut *mut MKFF_SwVideoDecoder,
) -> MKFF_Result {
    guard(|| {
        if out_decoder.is_null() {
            return invalid("null output");
        }
        *out_decoder = ptr::null_mut();
        let Some(c) = video_codec(codec) else {
            set_error(format!("no software decoder for video codec {codec}"));
            return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
        };
        let Some(cfg) = bytes(config, config_size) else {
            return invalid("null config with non-zero size");
        };
        match VideoDecoder::with_config(c, cfg) {
            Ok(dec) => store(
                out_decoder,
                MKFF_SwVideoDecoder {
                    dec,
                    queue: VecDeque::new(),
                    flushed: false,
                },
            ),
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_video_decoder_destroy(decoder: *mut MKFF_SwVideoDecoder) {
    if !decoder.is_null() {
        drop(Box::from_raw(decoder));
    }
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_video_decoder_submit(
    decoder: *mut MKFF_SwVideoDecoder,
    data: *const u8,
    size: usize,
    pts: i64,
) -> MKFF_Result {
    guard(|| {
        let Some(d) = decoder.as_mut() else {
            return invalid("null decoder");
        };
        let Some(pkt) = bytes(data, size).filter(|b| !b.is_empty()) else {
            return invalid("empty packet");
        };
        d.flushed = false;
        match d.dec.decode(pkt, pts) {
            Ok(frames) => {
                d.queue.extend(frames);
                MKFF_RESULT_OK
            }
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_video_decoder_receive(
    decoder: *mut MKFF_SwVideoDecoder,
    out_frame: *mut *mut MKFF_SwFrame,
) -> MKFF_Result {
    guard(|| {
        if out_frame.is_null() {
            return invalid("null output");
        }
        *out_frame = ptr::null_mut();
        let Some(d) = decoder.as_mut() else {
            return invalid("null decoder");
        };
        match d.queue.pop_front() {
            Some(f) => store(out_frame, MKFF_SwFrame(f)),
            None if d.flushed => MKFF_RESULT_END_OF_STREAM,
            None => MKFF_RESULT_NOT_READY,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_video_decoder_flush(
    decoder: *mut MKFF_SwVideoDecoder,
) -> MKFF_Result {
    guard(|| {
        let Some(d) = decoder.as_mut() else {
            return invalid("null decoder");
        };
        let rest = d.dec.flush();
        d.queue.extend(rest);
        d.flushed = true;
        MKFF_RESULT_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_video_decoder_reset(
    decoder: *mut MKFF_SwVideoDecoder,
) -> MKFF_Result {
    guard(|| {
        let Some(d) = decoder.as_mut() else {
            return invalid("null decoder");
        };
        d.dec.reset();
        d.queue.clear();
        d.flushed = false;
        MKFF_RESULT_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_audio_decoder_create(
    codec: i32,
    sample_rate: u32,
    config: *const u8,
    config_size: usize,
    out_decoder: *mut *mut MKFF_SwAudioDecoder,
) -> MKFF_Result {
    guard(|| {
        if out_decoder.is_null() {
            return invalid("null output");
        }
        *out_decoder = ptr::null_mut();
        let Some(c) = audio_codec(codec) else {
            set_error(format!("no software decoder for audio codec {codec}"));
            return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
        };
        let Some(cfg) = bytes(config, config_size) else {
            return invalid("null config with non-zero size");
        };
        match AudioDecoder::new(c, sample_rate, cfg) {
            Ok(d) => store(out_decoder, MKFF_SwAudioDecoder(d)),
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_audio_decoder_destroy(decoder: *mut MKFF_SwAudioDecoder) {
    if !decoder.is_null() {
        drop(Box::from_raw(decoder));
    }
}

#[no_mangle]
pub unsafe extern "C" fn mkff_sw_audio_decoder_decode(
    decoder: *mut MKFF_SwAudioDecoder,
    data: *const u8,
    size: usize,
    ts: i64,
    out_audio: *mut *mut MKFF_SwAudio,
) -> MKFF_Result {
    guard(|| {
        if out_audio.is_null() {
            return invalid("null output");
        }
        *out_audio = ptr::null_mut();
        let Some(MKFF_SwAudioDecoder(d)) = decoder.as_mut() else {
            return invalid("null decoder");
        };
        let Some(pkt) = bytes(data, size).filter(|b| !b.is_empty()) else {
            return invalid("empty packet");
        };
        match d.decode(pkt, ts.max(0) as u64) {
            Ok(b) => {
                *out_audio = audio_handle(b);
                MKFF_RESULT_OK
            }
            Err(e) => fail(e),
        }
    })
}
