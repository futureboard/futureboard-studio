//! The C entry points, called the way a C caller would (see tests/c/smoke.c for the header side).

use std::ffi::{CStr, CString};
use std::ptr;

use mkff_swcodecs::*;

fn fixture(name: &str) -> CString {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/codecs")
        .join(name);
    CString::new(p.to_str().unwrap()).unwrap()
}

fn zeroed<T>() -> T {
    // SAFETY: the info structs are plain integers, floats, arrays and raw pointers
    unsafe { std::mem::zeroed() }
}

#[test]
fn media_frames_and_audio() {
    unsafe {
        let mut m = ptr::null_mut();
        assert_eq!(
            mkff_sw_media_open_path(fixture("prores_4444_alpha.mov").as_ptr(), &mut m),
            MKFF_RESULT_OK
        );
        let mut info: MKFF_SwMediaInfo = zeroed();
        assert_eq!(mkff_sw_media_get_info(m, &mut info), MKFF_RESULT_OK);
        assert_eq!(
            info.struct_size as usize,
            std::mem::size_of::<MKFF_SwMediaInfo>()
        );
        assert_eq!((info.width, info.height, info.has_alpha), (64, 48, 1));
        assert!(CStr::from_ptr(info.video_codec.as_ptr())
            .to_str()
            .unwrap()
            .contains("ProRes"));

        let mut f = ptr::null_mut();
        assert_eq!(mkff_sw_media_read_video_frame(m, 2, &mut f), MKFF_RESULT_OK);
        let mut fi: MKFF_SwFrameInfo = zeroed();
        assert_eq!(mkff_sw_frame_get_info(f, &mut fi), MKFF_RESULT_OK);
        assert_eq!(
            (
                fi.layout,
                fi.chroma_format_idc,
                fi.plane_count,
                fi.bit_depth
            ),
            (MKFF_SW_LAYOUT_YUV, 3, 4, 12)
        );
        assert_eq!(fi.stride[0], 64 * 2);
        assert!(fi.data.iter().all(|p| !p.is_null()));

        let mut rgba = vec![0u8; 64 * 48 * 4];
        assert_eq!(
            mkff_sw_frame_copy_rgba8(f, rgba.as_mut_ptr(), rgba.len()),
            MKFF_RESULT_OK
        );
        assert_eq!(
            mkff_sw_frame_copy_rgba8(f, rgba.as_mut_ptr(), rgba.len() - 1),
            MKFF_RESULT_ERROR_INVALID_ARGUMENT
        );
        mkff_sw_frame_destroy(f);
        mkff_sw_media_destroy(m);

        let mut m = ptr::null_mut();
        assert_eq!(
            mkff_sw_media_open_path(fixture("tone.opus").as_ptr(), &mut m),
            MKFF_RESULT_OK
        );
        let mut a = ptr::null_mut();
        assert_eq!(
            mkff_sw_media_read_audio(m, 0, 0, 960, 0, &mut a),
            MKFF_RESULT_OK
        );
        let (mut rate, mut ch, mut frames, mut pcm) = (0, 0, 0, ptr::null());
        assert_eq!(
            mkff_sw_audio_get(a, &mut rate, &mut ch, &mut frames, &mut pcm),
            MKFF_RESULT_OK
        );
        assert_eq!((rate, frames), (48000, 960));
        assert!(!pcm.is_null() && ch >= 1);
        mkff_sw_audio_destroy(a);
        let mut f = ptr::null_mut();
        assert_eq!(
            mkff_sw_media_read_video_frame(m, 0, &mut f),
            MKFF_RESULT_ERROR_INVALID_ARGUMENT
        );
        assert!(f.is_null());
        assert!(CStr::from_ptr(mkff_sw_last_error())
            .to_str()
            .unwrap()
            .contains("video"));
        mkff_sw_media_destroy(m);
    }
}

#[test]
fn struct_size_guards_old_callers() {
    unsafe {
        let mut m = ptr::null_mut();
        assert_eq!(
            mkff_sw_media_open_path(fixture("vp9_opus.webm").as_ptr(), &mut m),
            MKFF_RESULT_OK
        );
        let mut info: MKFF_SwMediaInfo = zeroed();
        info.struct_size = 16;
        assert_eq!(
            mkff_sw_media_get_info(m, &mut info),
            MKFF_RESULT_ERROR_ABI_MISMATCH
        );
        mkff_sw_media_destroy(m);
    }
}

#[test]
fn null_and_unknown_arguments() {
    unsafe {
        let mut d = ptr::null_mut();
        assert_eq!(
            mkff_sw_video_decoder_create(0, ptr::null(), 0, &mut d),
            MKFF_RESULT_ERROR_CODEC_UNAVAILABLE
        );
        assert_eq!(
            mkff_sw_video_decoder_create(1, ptr::null(), 4, &mut d),
            MKFF_RESULT_ERROR_INVALID_ARGUMENT
        );
        assert_eq!(
            mkff_sw_media_open_path(ptr::null(), &mut ptr::null_mut()),
            MKFF_RESULT_ERROR_INVALID_ARGUMENT
        );
        assert_eq!(
            mkff_sw_frame_get_info(ptr::null(), &mut zeroed()),
            MKFF_RESULT_ERROR_INVALID_ARGUMENT
        );
        mkff_sw_frame_destroy(ptr::null_mut());
        mkff_sw_media_destroy(ptr::null_mut());
        assert_eq!(
            CStr::from_ptr(mkff_sw_video_codec_name(8))
                .to_str()
                .unwrap(),
            "prores"
        );
        assert_eq!(
            CStr::from_ptr(mkff_sw_audio_codec_name(5))
                .to_str()
                .unwrap(),
            "flac"
        );
    }
}

#[test]
fn elementary_audio_decoder() {
    let data = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/codecs/tone.aac"),
    )
    .unwrap();
    unsafe {
        let mut d = ptr::null_mut();
        assert_eq!(
            mkff_sw_audio_decoder_create(1, 0, ptr::null(), 0, &mut d),
            MKFF_RESULT_OK
        );
        let len =
            (((data[3] & 3) as usize) << 11) | (data[4] as usize) << 3 | (data[5] as usize) >> 5;
        let mut a = ptr::null_mut();
        assert_eq!(
            mkff_sw_audio_decoder_decode(d, data.as_ptr(), len, 0, &mut a),
            MKFF_RESULT_OK
        );
        let (mut rate, mut frames) = (0, 0);
        mkff_sw_audio_get(a, &mut rate, ptr::null_mut(), &mut frames, ptr::null_mut());
        assert_eq!((rate, frames), (48000, 1024));
        mkff_sw_audio_destroy(a);
        mkff_sw_audio_decoder_destroy(d);
    }
}
