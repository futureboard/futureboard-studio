//! AAudio entry points, resolved at runtime with `dlopen` so the library
//! still loads on devices older than API 26 (callers then fall back).
//! Shared by playback (`output`) and recording (`recorder`).

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

pub type Builder = c_void;
pub type Stream = c_void;
pub type DataCallback = unsafe extern "C" fn(*mut Stream, *mut c_void, *mut c_void, i32) -> i32;
pub type ErrorCallback = unsafe extern "C" fn(*mut Stream, *mut c_void, i32);

pub const AAUDIO_OK: i32 = 0;
pub const DIRECTION_OUTPUT: i32 = 0;
pub const DIRECTION_INPUT: i32 = 1;
pub const FORMAT_PCM_FLOAT: i32 = 2;
pub const SHARING_MODE_SHARED: i32 = 1;
pub const PERFORMANCE_MODE_NONE: i32 = 10;
pub const PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
pub const CALLBACK_CONTINUE: i32 = 0;
/// Raw mic signal (no AGC / noise suppression) — best for singing. API 28+.
pub const INPUT_PRESET_UNPROCESSED: i32 = 9;
/// Fallback with AGC off on most devices.
pub const INPUT_PRESET_VOICE_RECOGNITION: i32 = 6;
const RTLD_NOW: i32 = 2;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

pub struct Api {
    pub create_builder: unsafe extern "C" fn(*mut *mut Builder) -> i32,
    pub set_direction: unsafe extern "C" fn(*mut Builder, i32),
    pub set_sample_rate: unsafe extern "C" fn(*mut Builder, i32),
    pub set_channel_count: unsafe extern "C" fn(*mut Builder, i32),
    pub set_format: unsafe extern "C" fn(*mut Builder, i32),
    pub set_sharing_mode: unsafe extern "C" fn(*mut Builder, i32),
    pub set_performance_mode: unsafe extern "C" fn(*mut Builder, i32),
    pub set_data_callback: unsafe extern "C" fn(*mut Builder, DataCallback, *mut c_void),
    pub set_error_callback: unsafe extern "C" fn(*mut Builder, ErrorCallback, *mut c_void),
    pub open_stream: unsafe extern "C" fn(*mut Builder, *mut *mut Stream) -> i32,
    pub delete_builder: unsafe extern "C" fn(*mut Builder) -> i32,
    pub request_start: unsafe extern "C" fn(*mut Stream) -> i32,
    pub request_pause: unsafe extern "C" fn(*mut Stream) -> i32,
    pub request_stop: unsafe extern "C" fn(*mut Stream) -> i32,
    pub close: unsafe extern "C" fn(*mut Stream) -> i32,
    pub get_sample_rate: unsafe extern "C" fn(*mut Stream) -> i32,
    pub get_channel_count: unsafe extern "C" fn(*mut Stream) -> i32,
    pub get_format: unsafe extern "C" fn(*mut Stream) -> i32,
    pub get_frames_per_burst: unsafe extern "C" fn(*mut Stream) -> i32,
    pub get_buffer_capacity: unsafe extern "C" fn(*mut Stream) -> i32,
    pub set_buffer_size: unsafe extern "C" fn(*mut Stream, i32) -> i32,
    pub get_xrun_count: unsafe extern "C" fn(*mut Stream) -> i32,
    /// API 28+; `None` on Android 8.x.
    pub set_input_preset: Option<unsafe extern "C" fn(*mut Builder, i32)>,
}

impl Api {
    pub fn get() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe { Self::load() }).as_ref()
    }

    unsafe fn load() -> Option<Api> {
        let lib = unsafe { dlopen(c"libaaudio.so".as_ptr(), RTLD_NOW) };
        if lib.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:literal) => {{
                let p = unsafe { dlsym(lib, $name.as_ptr()) };
                if p.is_null() {
                    return None;
                }
                unsafe { std::mem::transmute::<*mut c_void, _>(p) }
            }};
        }
        let preset = unsafe { dlsym(lib, c"AAudioStreamBuilder_setInputPreset".as_ptr()) };
        Some(Api {
            create_builder: sym!(c"AAudio_createStreamBuilder"),
            set_direction: sym!(c"AAudioStreamBuilder_setDirection"),
            set_sample_rate: sym!(c"AAudioStreamBuilder_setSampleRate"),
            set_channel_count: sym!(c"AAudioStreamBuilder_setChannelCount"),
            set_format: sym!(c"AAudioStreamBuilder_setFormat"),
            set_sharing_mode: sym!(c"AAudioStreamBuilder_setSharingMode"),
            set_performance_mode: sym!(c"AAudioStreamBuilder_setPerformanceMode"),
            set_data_callback: sym!(c"AAudioStreamBuilder_setDataCallback"),
            set_error_callback: sym!(c"AAudioStreamBuilder_setErrorCallback"),
            open_stream: sym!(c"AAudioStreamBuilder_openStream"),
            delete_builder: sym!(c"AAudioStreamBuilder_delete"),
            request_start: sym!(c"AAudioStream_requestStart"),
            request_pause: sym!(c"AAudioStream_requestPause"),
            request_stop: sym!(c"AAudioStream_requestStop"),
            close: sym!(c"AAudioStream_close"),
            get_sample_rate: sym!(c"AAudioStream_getSampleRate"),
            get_channel_count: sym!(c"AAudioStream_getChannelCount"),
            get_format: sym!(c"AAudioStream_getFormat"),
            get_frames_per_burst: sym!(c"AAudioStream_getFramesPerBurst"),
            get_buffer_capacity: sym!(c"AAudioStream_getBufferCapacityInFrames"),
            set_buffer_size: sym!(c"AAudioStream_setBufferSizeInFrames"),
            get_xrun_count: sym!(c"AAudioStream_getXRunCount"),
            set_input_preset: if preset.is_null() {
                None
            } else {
                Some(unsafe {
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut Builder, i32)>(
                        preset,
                    )
                })
            },
        })
    }
}
