//! The ABI, called the way a C host calls it.

use super::*;
use std::ffi::CStr;

const SR: f64 = 48_000.0;

/// Counts heap allocations on the thread that arms it, so a test can prove
/// the audio path makes none.
mod alloc_probe {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct Counting;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if ARMED.with(Cell::get) {
                COUNT.with(|c| c.set(c.get() + 1));
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if ARMED.with(Cell::get) {
                COUNT.with(|c| c.set(c.get() + 1));
            }
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    /// Heap calls `body` makes on this thread.
    pub fn count(body: impl FnOnce()) -> u64 {
        COUNT.with(|c| c.set(0));
        ARMED.with(|c| c.set(true));
        body();
        ARMED.with(|c| c.set(false));
        COUNT.with(Cell::get)
    }
}

struct Engine(*mut RodhareistEngine);

impl Engine {
    fn new() -> Self {
        let engine = rodhareist_engine_create(SR);
        assert!(!engine.is_null());
        Self(engine)
    }

    fn process(&self, input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut left = vec![0.0; input.len()];
        let mut right = vec![0.0; input.len()];
        let status = unsafe {
            rodhareist_engine_process(
                self.0,
                input.as_ptr(),
                input.as_ptr(),
                left.as_mut_ptr(),
                right.as_mut_ptr(),
                input.len() as u32,
            )
        };
        assert_eq!(status, RODHAREIST_OK);
        (left, right)
    }

    fn set(&self, id: &str, value: f32) -> RodhareistStatus {
        let id = CString::new(id).unwrap();
        unsafe { rodhareist_engine_set_param_by_id(self.0, id.as_ptr(), value) }
    }

    fn get(&self, id: &str) -> f32 {
        let index = rodharerist::ui_param_index(id).unwrap();
        let mut value = f32::NAN;
        assert_eq!(
            unsafe { rodhareist_engine_get_param(self.0, index, &mut value) },
            RODHAREIST_OK
        );
        value
    }

    fn state(&self) -> String {
        let mut len = 0usize;
        assert_eq!(
            unsafe { rodhareist_engine_get_state(self.0, ptr::null_mut(), 0, &mut len) },
            RODHAREIST_BUFFER_TOO_SMALL
        );
        let mut buf = vec![0 as c_char; len + 1];
        assert_eq!(
            unsafe { rodhareist_engine_get_state(self.0, buf.as_mut_ptr(), buf.len(), &mut len) },
            RODHAREIST_OK
        );
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_str()
            .unwrap()
            .to_string()
    }

    fn set_state(&self, json: &str) -> RodhareistStatus {
        unsafe { rodhareist_engine_set_state(self.0, json.as_ptr().cast(), json.len()) }
    }

    fn last_error(&self) -> String {
        let mut buf = [0 as c_char; 512];
        let mut len = 0usize;
        unsafe { rodhareist_engine_last_error(self.0, buf.as_mut_ptr(), buf.len(), &mut len) };
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { rodhareist_engine_destroy(self.0) };
    }
}

fn sine(frames: usize, level: f32) -> Vec<f32> {
    (0..frames)
        .map(|i| level * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / SR as f32).sin())
        .collect()
}

fn text(p: *const c_char) -> String {
    unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string()
}

#[test]
fn the_library_names_itself_and_its_statuses() {
    assert_eq!(rodhareist_abi_version(), RODHAREIST_ABI_VERSION);
    assert_eq!(text(rodhareist_version()), env!("CARGO_PKG_VERSION"));
    assert_eq!(text(rodhareist_status_name(RODHAREIST_OK)), "ok");
    assert_eq!(
        text(rodhareist_status_name(RODHAREIST_QUEUE_FULL)),
        "control queue full"
    );
    assert_eq!(text(rodhareist_status_name(999)), "unknown status");
}

#[test]
fn the_parameter_table_is_the_wire_table() {
    let count = rodhareist_param_count();
    assert_eq!(count as usize, rodharerist::UI_PARAM_IDS.len());
    for index in 0..count {
        let mut info = std::mem::MaybeUninit::<RodhareistParamInfo>::uninit();
        assert_eq!(
            unsafe { rodhareist_param_info(index, info.as_mut_ptr()) },
            RODHAREIST_OK
        );
        let info = unsafe { info.assume_init() };
        let id = text(info.id);
        assert_eq!(id, rodharerist::UI_PARAM_IDS[index as usize]);
        let c_id = CString::new(id.clone()).unwrap();
        let mut back = u32::MAX;
        assert_eq!(
            unsafe { rodhareist_param_index(c_id.as_ptr(), &mut back) },
            RODHAREIST_OK
        );
        assert_eq!(back, index, "{id}");
        if info.flags & tables::PARAM_DESCRIBED != 0 {
            assert!(info.min_value <= info.default_value && info.default_value <= info.max_value);
        }
    }
    let power = rodharerist::ui_param_index("power").unwrap();
    let mut info = std::mem::MaybeUninit::<RodhareistParamInfo>::uninit();
    unsafe { rodhareist_param_info(power, info.as_mut_ptr()) };
    let info = unsafe { info.assume_init() };
    assert_eq!(text(info.name), "Power");
    assert_ne!(info.flags & tables::PARAM_TOGGLE, 0);
    let mut spare = info;
    assert_eq!(
        unsafe { rodhareist_param_info(count, &mut spare) },
        RODHAREIST_UNKNOWN_PARAM
    );
    let unknown = CString::new("not_a_param").unwrap();
    let mut index = 0;
    assert_eq!(
        unsafe { rodhareist_param_index(unknown.as_ptr(), &mut index) },
        RODHAREIST_UNKNOWN_PARAM
    );
}

#[test]
fn bad_rates_and_null_engines_are_refused() {
    assert!(rodhareist_engine_create(0.0).is_null());
    assert!(rodhareist_engine_create(f64::NAN).is_null());
    assert!(rodhareist_engine_create(1.0e9).is_null());
    let mut x = 0.0f32;
    assert_eq!(
        unsafe { rodhareist_engine_process(ptr::null_mut(), &x, &x, &mut x, ptr::null_mut(), 1) },
        RODHAREIST_NULL_POINTER
    );
    unsafe { rodhareist_engine_destroy(ptr::null_mut()) };
    assert_eq!(
        unsafe { rodhareist_engine_set_param(ptr::null_mut(), 0, 1.0) },
        RODHAREIST_NULL_POINTER
    );
    assert_eq!(unsafe { rodhareist_engine_sample_rate(ptr::null()) }, 0.0);
}

#[test]
fn it_processes_in_place_and_from_a_mono_input() {
    let engine = Engine::new();
    assert_eq!(unsafe { rodhareist_engine_sample_rate(engine.0) }, SR);
    let input = sine(4_800, 0.3);
    let (left, right) = engine.process(&input);
    assert!(left.iter().chain(&right).all(|v| v.is_finite()));
    assert!(left.iter().any(|v| v.abs() > 1.0e-3), "the rig makes sound");

    // The same block again, in place, from one channel.
    let fresh = Engine::new();
    let mut buffer = input;
    let status = unsafe {
        rodhareist_engine_process(
            fresh.0,
            buffer.as_ptr(),
            ptr::null(),
            buffer.as_mut_ptr(),
            ptr::null_mut(),
            buffer.len() as u32,
        )
    };
    assert_eq!(status, RODHAREIST_OK);
    assert_eq!(buffer, left, "in place, mono in: the same left output");
}

#[test]
fn a_parameter_reaches_the_dsp_at_the_next_block() {
    let engine = Engine::new();
    assert_eq!(engine.get("power"), 1.0);
    assert_eq!(engine.set("power", 0.0), RODHAREIST_OK);
    assert_eq!(
        engine.get("power"),
        0.0,
        "the control side reads it back at once"
    );
    // Past the power fade, the input comes through as it went in.
    engine.process(&sine(4_800, 0.3));
    let input = sine(960, 0.3);
    let (left, _) = engine.process(&input);
    let latency = unsafe { rodhareist_engine_latency_samples(engine.0) } as usize;
    assert_eq!(latency, 0);
    for (out, inp) in left.iter().zip(&input) {
        assert!((out - inp).abs() < 1.0e-6);
    }
    assert_eq!(engine.set("power", f32::NAN), RODHAREIST_INVALID_ARGUMENT);
    assert_eq!(engine.set("nope", 1.0), RODHAREIST_UNKNOWN_PARAM);
    assert!(!engine.last_error().is_empty());
}

#[test]
fn the_state_round_trips_and_reaches_another_engine() {
    let a = Engine::new();
    assert_eq!(a.set("amp_gain", 7.5), RODHAREIST_OK);
    assert_eq!(a.set("power", 0.0), RODHAREIST_OK);
    let json = a.state();
    assert!(json.contains("schema_version"));

    let b = Engine::new();
    assert_eq!(b.set_state(&json), RODHAREIST_OK);
    assert_eq!(b.get("amp_gain"), 7.5);
    assert_eq!(b.get("power"), 0.0);
    assert_eq!(b.state(), json);
    // It reached b's DSP: power off passes the input through.
    b.process(&sine(4_800, 0.3));
    let input = sine(480, 0.3);
    let (left, _) = b.process(&input);
    assert!(left.iter().zip(&input).all(|(o, i)| (o - i).abs() < 1.0e-6));

    assert_eq!(b.set_state("{not json"), RODHAREIST_PARSE_FAILED);
    assert!(
        b.last_error().contains("could not be read"),
        "{}",
        b.last_error()
    );
    assert_eq!(b.get("amp_gain"), 7.5, "a bad state changes nothing");
}

#[test]
fn a_buffer_too_small_says_how_big_it_must_be() {
    let engine = Engine::new();
    let mut small = [0 as c_char; 4];
    let mut len = 0usize;
    assert_eq!(
        unsafe { rodhareist_engine_get_state(engine.0, small.as_mut_ptr(), small.len(), &mut len) },
        RODHAREIST_BUFFER_TOO_SMALL
    );
    assert_eq!(len, engine.state().len());
}

#[test]
fn a_preset_keeps_power_trim_and_files() {
    let count = rodhareist_preset_count();
    assert!(count > 0);
    let mut info = RodhareistPresetInfo {
        id: ptr::null(),
        name: ptr::null(),
    };
    assert_eq!(
        unsafe { rodhareist_preset_info(0, &mut info) },
        RODHAREIST_OK
    );
    assert!(!text(info.name).is_empty());
    assert_eq!(
        unsafe { rodhareist_preset_info(count, &mut info) },
        RODHAREIST_NO_SUCH_PRESET
    );

    let engine = Engine::new();
    engine.set("power", 0.0);
    engine.set("input_trim", -6.0);
    for index in 0..count {
        assert_eq!(
            unsafe { rodhareist_engine_load_preset(engine.0, index) },
            RODHAREIST_OK
        );
        assert_eq!(engine.get("power"), 0.0);
        assert_eq!(engine.get("input_trim"), -6.0);
        engine.process(&sine(256, 0.2));
    }
    assert_eq!(
        unsafe { rodhareist_engine_load_preset(engine.0, count) },
        RODHAREIST_NO_SUCH_PRESET
    );
}

/// A short decaying impulse as a 16-bit mono WAV.
fn impulse_wav() -> Vec<u8> {
    let samples: Vec<i16> = (0..2_048)
        .map(|i| {
            ((0.9 * (-(i as f32) / 200.0).exp()) * 32_767.0) as i16
                * if i % 2 == 0 { 1 } else { -1 }
        })
        .collect();
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&96_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[test]
fn an_impulse_response_loads_plays_and_reports_its_latency() {
    let engine = Engine::new();
    let wav = impulse_wav();
    let name = CString::new("Test IR.wav").unwrap();
    let mut info = RodhareistIrInfo::default();
    assert_eq!(
        unsafe {
            rodhareist_engine_load_ir(engine.0, wav.as_ptr(), wav.len(), name.as_ptr(), &mut info)
        },
        RODHAREIST_OK
    );
    // The loader trims the silent tail the 16-bit decay ends on.
    assert!((1_000..=2_048).contains(&info.frames), "{}", info.frames);
    assert_eq!(info.stereo, 0);
    assert_eq!(info.truncated, 0);
    assert!(engine.state().contains("Test IR.wav"), "the state names it");

    let ir = rodhareist_cab_model_ir() as f32;
    assert_eq!(engine.set("cab_model", ir), RODHAREIST_OK);
    engine.process(&sine(4_800, 0.3));
    let latency = unsafe { rodhareist_engine_latency_samples(engine.0) };
    assert_eq!(latency, info.latency_samples, "the IR's delay is reported");

    let garbage = b"RIFF nope";
    assert_eq!(
        unsafe {
            rodhareist_engine_load_ir(
                engine.0,
                garbage.as_ptr(),
                garbage.len(),
                ptr::null(),
                &mut info,
            )
        },
        RODHAREIST_LOAD_FAILED
    );
    assert!(
        engine.last_error().starts_with("IR"),
        "{}",
        engine.last_error()
    );
    assert_eq!(
        unsafe { rodhareist_engine_unload_ir(engine.0) },
        RODHAREIST_OK
    );
    assert!(!engine.state().contains("Test IR.wav"));
}

#[test]
fn a_bad_capture_is_a_clean_error() {
    let engine = Engine::new();
    let json = b"{\"version\": \"0.5.0\"}";
    let mut info = std::mem::MaybeUninit::<RodhareistNamInfo>::uninit();
    assert_eq!(
        unsafe {
            rodhareist_engine_load_nam(
                engine.0,
                json.as_ptr().cast(),
                json.len(),
                ptr::null(),
                0,
                0,
                info.as_mut_ptr(),
            )
        },
        RODHAREIST_LOAD_FAILED
    );
    assert!(
        engine.last_error().contains("NAM"),
        "{}",
        engine.last_error()
    );
    assert_eq!(
        unsafe { rodhareist_engine_unload_nam(engine.0) },
        RODHAREIST_OK
    );
}

#[test]
fn meters_and_telemetry_follow_the_last_block() {
    let engine = Engine::new();
    engine.process(&sine(4_800, 0.5));
    let mut meters = RodhareistMeters::default();
    assert_eq!(
        unsafe { rodhareist_engine_meters(engine.0, &mut meters) },
        RODHAREIST_OK
    );
    assert!(meters.in_peak > 0.3);
    let mut block = [f32::NAN; RODHAREIST_TELEMETRY_SLOTS];
    assert_eq!(
        unsafe { rodhareist_engine_telemetry(engine.0, block.as_mut_ptr(), block.len()) },
        RODHAREIST_OK
    );
    assert!(block[telemetry::IN_L] > 0.3);
    assert!(block.iter().all(|v| v.is_finite()));
}

#[test]
fn a_full_queue_is_reported_and_drains() {
    let engine = Engine::new();
    let mut full = false;
    for i in 0..QUEUE_CAPACITY + 10 {
        if engine.set("amp_gain", (i % 10) as f32) == RODHAREIST_QUEUE_FULL {
            full = true;
            break;
        }
    }
    assert!(full, "an engine nobody processes fills its queue");
    engine.process(&sine(64, 0.1));
    assert_eq!(engine.set("amp_gain", 3.0), RODHAREIST_OK);
}

#[test]
fn a_second_process_call_at_once_is_refused() {
    let engine = Engine::new();
    unsafe { &*engine.0 }
        .processing
        .store(true, Ordering::Release);
    let x = [0.0f32; 4];
    let mut y = [0.0f32; 4];
    assert_eq!(
        unsafe {
            rodhareist_engine_process(
                engine.0,
                x.as_ptr(),
                x.as_ptr(),
                y.as_mut_ptr(),
                ptr::null_mut(),
                4,
            )
        },
        RODHAREIST_BUSY
    );
    unsafe { &*engine.0 }
        .processing
        .store(false, Ordering::Release);
}

#[test]
fn control_and_audio_run_on_two_threads() {
    let engine = Engine::new();
    let handle = engine.0 as usize;
    let audio = std::thread::spawn(move || {
        let engine = handle as *mut RodhareistEngine;
        let input = sine(256, 0.3);
        let mut left = vec![0.0f32; 256];
        let mut right = vec![0.0f32; 256];
        for _ in 0..400 {
            let status = unsafe {
                rodhareist_engine_process(
                    engine,
                    input.as_ptr(),
                    input.as_ptr(),
                    left.as_mut_ptr(),
                    right.as_mut_ptr(),
                    256,
                )
            };
            assert_eq!(status, RODHAREIST_OK);
            assert!(left.iter().all(|v| v.is_finite()));
        }
    });
    let json = engine.state();
    for i in 0..400 {
        let status = engine.set("amp_gain", (i % 10) as f32);
        assert!(status == RODHAREIST_OK || status == RODHAREIST_QUEUE_FULL);
        if i % 50 == 0 {
            let status = engine.set_state(&json);
            assert!(status == RODHAREIST_OK || status == RODHAREIST_QUEUE_FULL);
        }
        unsafe { rodhareist_engine_set_tempo(engine.0, 90.0 + i as f64 * 0.1) };
        unsafe { rodhareist_engine_collect_garbage(engine.0) };
    }
    audio.join().unwrap();
}

#[test]
fn processing_allocates_nothing() {
    let engine = Engine::new();
    let input = sine(256, 0.3);
    let mut left = vec![0.0f32; 256];
    let mut right = vec![0.0f32; 256];
    let run = |engine: &Engine, left: &mut [f32], right: &mut [f32]| unsafe {
        rodhareist_engine_process(
            engine.0,
            input.as_ptr(),
            input.as_ptr(),
            left.as_mut_ptr(),
            right.as_mut_ptr(),
            256,
        )
    };
    run(&engine, &mut left, &mut right);
    // A parameter edit, a whole state and a reset, all adopted in the block.
    engine.set("amp_gain", 4.0);
    engine.set_state(&engine.state());
    unsafe { rodhareist_engine_reset(engine.0) };
    unsafe { rodhareist_engine_set_tempo(engine.0, 133.0) };
    let heap = alloc_probe::count(|| {
        assert_eq!(run(&engine, &mut left, &mut right), RODHAREIST_OK);
    });
    assert_eq!(heap, 0, "the audio side allocated or freed");
}

/// Every exported function is declared in the C header, and the header
/// declares nothing that is not exported.
#[test]
fn the_header_declares_every_export() {
    let source = include_str!("lib.rs");
    let header = include_str!("../include/rodhareist_engine.h");
    let exported: Vec<&str> = source
        .split("extern \"C\" fn ")
        .skip(1)
        .filter_map(|rest| rest.split('(').next())
        .collect();
    assert!(exported.len() > 20);
    for name in &exported {
        assert!(
            header.contains(&format!("{name}(")),
            "{name} missing from the header"
        );
    }
    // Declarations start their line; the `#define RODHAREIST_API` lines do
    // not.
    let declared: Vec<&str> = header
        .split("\nRODHAREIST_API ")
        .skip(1)
        .filter_map(|rest| {
            let call = rest.split('(').next()?;
            call.split_whitespace()
                .last()
                .map(|n| n.trim_start_matches('*'))
        })
        .collect();
    for name in &declared {
        assert!(exported.contains(name), "{name} declared but not exported");
    }
    // The constants agree.
    for (name, value) in [
        ("RODHAREIST_ABI_VERSION", RODHAREIST_ABI_VERSION as i64),
        (
            "RODHAREIST_TELEMETRY_SLOTS",
            RODHAREIST_TELEMETRY_SLOTS as i64,
        ),
        ("RODHAREIST_OK", RODHAREIST_OK as i64),
        ("RODHAREIST_BUSY", RODHAREIST_BUSY as i64),
        ("RODHAREIST_QUEUE_FULL", RODHAREIST_QUEUE_FULL as i64),
        ("RODHAREIST_PARAM_DESCRIBED", tables::PARAM_DESCRIBED as i64),
        ("RODHAREIST_PARAM_TOGGLE", tables::PARAM_TOGGLE as i64),
        ("RODHAREIST_PARAM_CHOICE", tables::PARAM_CHOICE as i64),
        ("RODHAREIST_PARAM_ACTION", tables::PARAM_ACTION as i64),
        ("RODHAREIST_TELEMETRY_TUNER_HZ", telemetry::TUNER_HZ as i64),
    ] {
        assert!(
            header.contains(&format!("#define {name} {value}")),
            "{name} should be {value} in the header"
        );
    }
}
