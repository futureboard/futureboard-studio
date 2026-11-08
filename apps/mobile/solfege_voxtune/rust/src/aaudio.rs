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
    pub get_state: unsafe extern "C" fn(*mut Stream) -> i32,
    pub wait_for_state_change: unsafe extern "C" fn(*mut Stream, i32, *mut i32, i64) -> i32,
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
            get_state: sym!(c"AAudioStream_getState"),
            wait_for_state_change: sym!(c"AAudioStream_waitForStateChange"),
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

/// Longest single wait for the stream to change state while stopping.
const STOP_WAIT_NANOS: i64 = 100_000_000;
/// Waits before giving up on the stream stopping (about two seconds).
const STOP_MAX_WAITS: u32 = 20;

/// Stops and closes `stream`, returning whether its callbacks are known to be
/// finished so the context they were registered with may be freed.
///
/// `requestStop` is asynchronous: a data callback can still be running when it
/// returns. This waits for the stream to leave its running states before
/// `close` instead of closing a stream that may be mid-callback. If it never
/// settles, `false` tells the caller to keep the context alive (a bounded leak)
/// rather than free memory a late callback could still read.
///
/// # Safety
/// `stream` must be an open stream from `api` that nothing else closes.
pub unsafe fn stop_and_close(api: &Api, stream: *mut Stream) -> bool {
    unsafe {
        (api.request_stop)(stream);
        let settled = crate::aaudio_state::wait_until_settled(
            (api.get_state)(stream),
            STOP_MAX_WAITS,
            |state| {
                let mut next = state;
                if (api.wait_for_state_change)(stream, state, &mut next, STOP_WAIT_NANOS)
                    == AAUDIO_OK
                {
                    next
                } else {
                    // Timed out, or the stream errored: re-read where it is.
                    (api.get_state)(stream)
                }
            },
        );
        (api.close)(stream);
        settled
    }
}
