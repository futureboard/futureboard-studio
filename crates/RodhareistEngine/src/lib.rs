//! RodhareistEngine: a C ABI over the Rodhareist guitar multi-effect core
//! (`rodharerist`), for C and C++ hosts. `include/rodhareist_engine.h` is the
//! C declaration of everything exported here; `include/rodhareist_engine.hpp`
//! wraps it in RAII C++.
//!
//! # Threads
//!
//! An engine has two sides:
//!
//! - the **audio** side — [`rodhareist_engine_process`] — which owns the DSP
//!   and runs on one audio thread at a time. It never allocates, locks,
//!   blocks or logs;
//! - the **control** side — parameters, state, presets, NAM captures and
//!   impulse responses — which may run on any other thread, concurrently with
//!   processing. Control calls serialize on an internal lock; their changes
//!   reach the DSP through a preallocated lock-free queue that the next
//!   process call drains at its start, in the order they were made.
//!
//! Tempo, latency, meters and telemetry are atomics, safe from any thread.
//!
//! # Errors
//!
//! Every function that can fail returns a status (`RODHAREIST_OK` = 0). No
//! panic crosses the boundary: one is caught and reported as
//! `RODHAREIST_PANIC`. A control call that fails explains why in
//! [`rodhareist_engine_last_error`].

mod queue;
mod tables;

use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use builtin_dsp_core::StereoEffect;
use rodharerist::{
    CabModel, Dsp, IrLoader, NamLoader, Params, RodhareistState, telemetry, ui_param_id,
};

use queue::{Message, Queue};

/// The ABI this library implements. Bumped on any incompatible change to a
/// function, a struct or a constant.
pub const RODHAREIST_ABI_VERSION: u32 = 1;
/// Floats in the telemetry block.
pub const RODHAREIST_TELEMETRY_SLOTS: usize = telemetry::SLOTS;
/// Control messages that can wait for the next process call.
const QUEUE_CAPACITY: usize = 4_096;

pub type RodhareistStatus = i32;
pub const RODHAREIST_OK: RodhareistStatus = 0;
pub const RODHAREIST_NULL_POINTER: RodhareistStatus = 1;
pub const RODHAREIST_UNKNOWN_PARAM: RodhareistStatus = 2;
pub const RODHAREIST_INVALID_ARGUMENT: RodhareistStatus = 3;
pub const RODHAREIST_BUFFER_TOO_SMALL: RodhareistStatus = 4;
pub const RODHAREIST_QUEUE_FULL: RodhareistStatus = 5;
pub const RODHAREIST_PARSE_FAILED: RodhareistStatus = 6;
pub const RODHAREIST_LOAD_FAILED: RodhareistStatus = 7;
pub const RODHAREIST_PANIC: RodhareistStatus = 8;
pub const RODHAREIST_NO_SUCH_PRESET: RodhareistStatus = 9;
pub const RODHAREIST_BUSY: RodhareistStatus = 10;

/// One wire parameter. The strings live as long as the library is loaded.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RodhareistParamInfo {
    pub id: *const c_char,
    pub name: *const c_char,
    pub unit: *const c_char,
    pub min_value: f32,
    pub max_value: f32,
    pub default_value: f32,
    /// `RODHAREIST_PARAM_*` flags.
    pub flags: u32,
}

/// One factory preset. The strings live as long as the library is loaded.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RodhareistPresetInfo {
    /// Bank position, e.g. `"03B"`.
    pub id: *const c_char,
    pub name: *const c_char,
}

/// Input and output levels after the trims, linear; clip flags are sticky
/// until cleared.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RodhareistMeters {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub in_clip: u32,
    pub out_clip: u32,
}

/// What a loaded NAM capture is.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RodhareistNamInfo {
    pub sample_rate: f64,
    pub receptive_field: u32,
    pub submodel_count: u32,
    /// The capture models a whole rig (amp, cabinet and microphone).
    pub full_rig: u32,
    pub slimmable: u32,
    /// NUL-terminated, cut to fit.
    pub architecture: [c_char; 64],
    pub family: [c_char; 64],
}

/// What a loaded impulse response is.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RodhareistIrInfo {
    pub source_sample_rate: f64,
    pub frames: u64,
    pub latency_samples: u32,
    pub stereo: u32,
    /// The file was longer than the engine keeps, and was cut.
    pub truncated: u32,
}

struct AudioSide {
    dsp: Dsp,
}

struct ControlSide {
    /// The parameters as the control side last set them, file names
    /// included: what state and parameter reads answer from.
    mirror: Params,
    nam: NamLoader,
    ir: IrLoader,
    last_error: CString,
}

/// An engine instance. Opaque to C.
pub struct RodhareistEngine {
    sample_rate: f64,
    audio: UnsafeCell<AudioSide>,
    /// Set while a process call runs: a second one at the same time is
    /// refused rather than racing the first.
    processing: AtomicBool,
    control: Mutex<ControlSide>,
    queue: Queue,
    /// A whole parameter set waiting for the audio side (a state or preset
    /// load), and the one it replaced, waiting for the control side to free.
    pending: AtomicPtr<Params>,
    retired: AtomicPtr<Params>,
    /// Host tempo as `f32` bits; 0 until set.
    tempo: AtomicU32,
    latency: AtomicU32,
    meters: [AtomicU32; 6],
    telemetry: [AtomicU32; telemetry::SLOTS],
}

// SAFETY: `audio` is touched only inside a process call, and `processing`
// refuses a second one while the first runs. Everything else is atomics, the
// lock-free queue, or behind `control`'s mutex.
unsafe impl Sync for RodhareistEngine {}
unsafe impl Send for RodhareistEngine {}

impl Drop for RodhareistEngine {
    fn drop(&mut self) {
        for slot in [&self.pending, &self.retired] {
            let p = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !p.is_null() {
                // SAFETY: every non-null pointer in these slots came from
                // `Box::into_raw` and is owned by the slot.
                drop(unsafe { Box::from_raw(p) });
            }
        }
    }
}

impl RodhareistEngine {
    fn new(sample_rate: f64) -> Self {
        let dsp = Dsp::new(sample_rate as f32);
        let control = ControlSide {
            mirror: rodharerist::default_params(),
            nam: dsp.nam_loader(),
            ir: dsp.ir_loader(),
            last_error: CString::default(),
        };
        let latency = dsp.latency_samples() as u32;
        Self {
            sample_rate,
            audio: UnsafeCell::new(AudioSide { dsp }),
            processing: AtomicBool::new(false),
            control: Mutex::new(control),
            queue: Queue::new(QUEUE_CAPACITY),
            pending: AtomicPtr::new(ptr::null_mut()),
            retired: AtomicPtr::new(ptr::null_mut()),
            tempo: AtomicU32::new(0),
            latency: AtomicU32::new(latency),
            meters: std::array::from_fn(|_| AtomicU32::new(0)),
            telemetry: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    fn control(&self) -> std::sync::MutexGuard<'_, ControlSide> {
        // A panic while the lock was held already reported `PANIC`; the
        // mirror is plain data and still usable.
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The control side, for a control call: the last error is cleared, so
    /// it always explains the most recent call.
    fn begin(&self) -> std::sync::MutexGuard<'_, ControlSide> {
        let mut control = self.control();
        control.last_error = CString::default();
        control
    }

    /// Frees a parameter set the audio side has finished with.
    fn collect_retired(&self) {
        let p = self.retired.swap(ptr::null_mut(), Ordering::AcqRel);
        if !p.is_null() {
            // SAFETY: from `Box::into_raw`, handed back by the audio side.
            drop(unsafe { Box::from_raw(p) });
        }
    }

    /// Hands a whole parameter set to the audio side and makes it the
    /// mirror. Control side, with its lock held.
    fn submit_params(&self, control: &mut ControlSide, params: Params) -> RodhareistStatus {
        self.collect_retired();
        // The DSP keeps no file names, so its copy — cloned on the audio
        // thread — has nothing on the heap.
        let mut for_audio = params.clone();
        for_audio.nam_file = None;
        for_audio.ir_file = None;
        let fresh = Box::into_raw(Box::new(for_audio));
        let replaced = self.pending.swap(fresh, Ordering::AcqRel);
        if !replaced.is_null() {
            // SAFETY: never adopted, so still owned by the slot.
            drop(unsafe { Box::from_raw(replaced) });
        }
        if !self.queue.push(Message::State) {
            let mine = self.pending.swap(ptr::null_mut(), Ordering::AcqRel);
            if mine == fresh {
                // SAFETY: ours, and taken back before the audio side saw it.
                drop(unsafe { Box::from_raw(mine) });
                return fail(control, RODHAREIST_QUEUE_FULL, "the control queue is full");
            }
            // An earlier State message already adopted it.
            if !mine.is_null() {
                // SAFETY: as above, owned by the slot.
                drop(unsafe { Box::from_raw(mine) });
            }
        }
        control.mirror = params;
        RODHAREIST_OK
    }

    /// The audio side's start of block: every queued change, in order.
    fn drain(&self, audio: &mut AudioSide) {
        while let Some(message) = self.queue.peek() {
            match message {
                Message::Param(index, value) => {
                    if let Some(id) = ui_param_id(index) {
                        let _ = audio.dsp.apply_ui_param(id, value);
                    }
                }
                Message::State => {
                    // The last set waits until the control side has freed
                    // it: nothing is freed here.
                    if !self.retired.load(Ordering::Acquire).is_null() {
                        return;
                    }
                    let p = self.pending.swap(ptr::null_mut(), Ordering::AcqRel);
                    if !p.is_null() {
                        // SAFETY: from `Box::into_raw`; the audio side owns
                        // it until it is handed to `retired`.
                        audio.dsp.set_params(unsafe { &*p }.clone());
                        self.retired.store(p, Ordering::Release);
                    }
                }
                Message::Reset => audio.dsp.reset(),
                Message::ClearClip => audio.dsp.clear_clip(),
            }
            self.queue.pop();
        }
    }

    fn publish(&self, audio: &AudioSide) {
        let frame = audio.dsp.meter_frame();
        let meters = [
            frame.in_peak,
            frame.in_rms,
            frame.out_peak,
            frame.out_rms,
            f32::from(u8::from(frame.in_clip)),
            f32::from(u8::from(frame.out_clip)),
        ];
        for (slot, value) in self.meters.iter().zip(meters) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        for (slot, value) in self.telemetry.iter().zip(audio.dsp.telemetry()) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        self.latency
            .store(audio.dsp.latency_samples() as u32, Ordering::Release);
    }
}

fn fail(control: &mut ControlSide, status: RodhareistStatus, why: &str) -> RodhareistStatus {
    control.last_error = CString::new(why.replace('\0', "")).unwrap_or_default();
    status
}

/// Runs `body`, turning a panic into `RODHAREIST_PANIC`.
fn guard(body: impl FnOnce() -> RodhareistStatus) -> RodhareistStatus {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(RODHAREIST_PANIC)
}

/// # Safety
/// `engine` is null or a live engine from [`rodhareist_engine_create`].
unsafe fn engine_ref<'a>(engine: *const RodhareistEngine) -> Option<&'a RodhareistEngine> {
    // SAFETY: the caller's contract.
    unsafe { engine.as_ref() }
}

/// Writes `text` NUL-terminated into `buf` (`capacity` bytes) and its length
/// without the NUL into `out_len`. `buf` may be null to ask the length.
///
/// # Safety
/// `buf` is null or holds `capacity` bytes; `out_len` is null or writable.
unsafe fn write_text(
    text: &[u8],
    buf: *mut c_char,
    capacity: usize,
    out_len: *mut usize,
) -> RodhareistStatus {
    if !out_len.is_null() {
        // SAFETY: the caller's contract.
        unsafe { *out_len = text.len() };
    }
    if buf.is_null() || capacity < text.len() + 1 {
        return RODHAREIST_BUFFER_TOO_SMALL;
    }
    // SAFETY: `buf` holds at least `text.len() + 1` bytes.
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr().cast::<c_char>(), buf, text.len());
        *buf.add(text.len()) = 0;
    }
    RODHAREIST_OK
}

/// # Safety
/// `text` is null or a NUL-terminated string.
unsafe fn c_str<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// # Safety
/// `data` is null or holds `len` bytes.
unsafe fn bytes<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if data.is_null() {
        return (len == 0).then_some(&[]);
    }
    // SAFETY: the caller's contract.
    Some(unsafe { std::slice::from_raw_parts(data, len) })
}

fn fixed_text(text: &str) -> [c_char; 64] {
    let mut out = [0 as c_char; 64];
    for (slot, byte) in out.iter_mut().zip(text.bytes().take(63)) {
        *slot = byte as c_char;
    }
    out
}

// ── Library ───────────────────────────────────────────────────────────────

/// The ABI this library implements: compare with `RODHAREIST_ABI_VERSION`.
#[unsafe(no_mangle)]
pub extern "C" fn rodhareist_abi_version() -> u32 {
    RODHAREIST_ABI_VERSION
}

/// The library's version, NUL-terminated, valid while it is loaded.
#[unsafe(no_mangle)]
pub extern "C" fn rodhareist_version() -> *const c_char {
    tables::version()
}

/// A status's name, NUL-terminated and static.
#[unsafe(no_mangle)]
pub extern "C" fn rodhareist_status_name(status: RodhareistStatus) -> *const c_char {
    let name: &'static [u8] = match status {
        RODHAREIST_OK => b"ok\0",
        RODHAREIST_NULL_POINTER => b"null pointer\0",
        RODHAREIST_UNKNOWN_PARAM => b"unknown parameter\0",
        RODHAREIST_INVALID_ARGUMENT => b"invalid argument\0",
        RODHAREIST_BUFFER_TOO_SMALL => b"buffer too small\0",
        RODHAREIST_QUEUE_FULL => b"control queue full\0",
        RODHAREIST_PARSE_FAILED => b"parse failed\0",
        RODHAREIST_LOAD_FAILED => b"load failed\0",
        RODHAREIST_PANIC => b"internal error\0",
        RODHAREIST_NO_SUCH_PRESET => b"no such preset\0",
        RODHAREIST_BUSY => b"already processing\0",
        _ => b"unknown status\0",
    };
    name.as_ptr().cast()
}

/// Wire parameters there are. Their indices are stable: an index is an ABI.
#[unsafe(no_mangle)]
pub extern "C" fn rodhareist_param_count() -> u32 {
    tables::params().len() as u32
}

/// Describes wire parameter `index`.
///
/// # Safety
/// `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_param_info(
    index: u32,
    out: *mut RodhareistParamInfo,
) -> RodhareistStatus {
    guard(|| {
        if out.is_null() {
            return RODHAREIST_NULL_POINTER;
        }
        let Some(row) = tables::params().get(index as usize) else {
            return RODHAREIST_UNKNOWN_PARAM;
        };
        // SAFETY: the caller's contract.
        unsafe {
            *out = RodhareistParamInfo {
                id: row.id.as_ptr(),
                name: row.name.as_ptr(),
                unit: row.unit.as_ptr(),
                min_value: row.min,
                max_value: row.max,
                default_value: row.default,
                flags: row.flags,
            };
        }
        RODHAREIST_OK
    })
}

/// The wire index of parameter `id` (NUL-terminated).
///
/// # Safety
/// `id` is null or NUL-terminated; `out_index` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_param_index(
    id: *const c_char,
    out_index: *mut u32,
) -> RodhareistStatus {
    guard(|| {
        if out_index.is_null() {
            return RODHAREIST_NULL_POINTER;
        }
        // SAFETY: the caller's contract.
        let Some(id) = (unsafe { c_str(id) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        match rodharerist::ui_param_index(id) {
            Some(index) => {
                // SAFETY: the caller's contract.
                unsafe { *out_index = index };
                RODHAREIST_OK
            }
            None => RODHAREIST_UNKNOWN_PARAM,
        }
    })
}

/// The `cab_model` value that plays the loaded impulse response.
#[unsafe(no_mangle)]
pub extern "C" fn rodhareist_cab_model_ir() -> u32 {
    CabModel::ALL
        .iter()
        .position(|model| *model == CabModel::Ir)
        .unwrap_or(0) as u32
}

/// Factory presets there are.
#[unsafe(no_mangle)]
pub extern "C" fn rodhareist_preset_count() -> u32 {
    tables::presets().len() as u32
}

/// Describes factory preset `index`.
///
/// # Safety
/// `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_preset_info(
    index: u32,
    out: *mut RodhareistPresetInfo,
) -> RodhareistStatus {
    guard(|| {
        if out.is_null() {
            return RODHAREIST_NULL_POINTER;
        }
        let Some(row) = tables::presets().get(index as usize) else {
            return RODHAREIST_NO_SUCH_PRESET;
        };
        // SAFETY: the caller's contract.
        unsafe {
            *out = RodhareistPresetInfo {
                id: row.id.as_ptr(),
                name: row.name.as_ptr(),
            };
        }
        RODHAREIST_OK
    })
}

// ── Lifecycle ─────────────────────────────────────────────────────────────

/// A new engine at `sample_rate` Hz (8 000 to 384 000), at the default
/// settings; null for a rate out of range. Control side. A different rate
/// needs a new engine.
#[unsafe(no_mangle)]
pub extern "C" fn rodhareist_engine_create(sample_rate: f64) -> *mut RodhareistEngine {
    if !(sample_rate.is_finite() && (8_000.0..=384_000.0).contains(&sample_rate)) {
        return ptr::null_mut();
    }
    catch_unwind(|| Box::into_raw(Box::new(RodhareistEngine::new(sample_rate))))
        .unwrap_or(ptr::null_mut())
}

/// Frees an engine. Null is ignored. No other call may be running on it.
///
/// # Safety
/// `engine` is null or from [`rodhareist_engine_create`], not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_destroy(engine: *mut RodhareistEngine) {
    if engine.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller's contract.
        drop(unsafe { Box::from_raw(engine) });
    }));
}

/// The rate the engine was created at; 0 for null.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_sample_rate(engine: *const RodhareistEngine) -> f64 {
    // SAFETY: the caller's contract.
    unsafe { engine_ref(engine) }.map_or(0.0, |engine| engine.sample_rate)
}

// ── Audio ─────────────────────────────────────────────────────────────────

/// Processes `frames` frames. Audio side: one thread at a time; never
/// allocates, locks or blocks.
///
/// `in_right` may be null for a mono input (the left feeds both sides), and
/// `out_right` null to keep only the left output. An output may be the same
/// buffer as an input. Returns `RODHAREIST_BUSY` when another process call is
/// running on the engine.
///
/// # Safety
/// `engine` is live; each non-null buffer holds `frames` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_process(
    engine: *mut RodhareistEngine,
    in_left: *const f32,
    in_right: *const f32,
    out_left: *mut f32,
    out_right: *mut f32,
    frames: u32,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        if in_left.is_null() || out_left.is_null() {
            return RODHAREIST_NULL_POINTER;
        }
        if engine.processing.swap(true, Ordering::Acquire) {
            return RODHAREIST_BUSY;
        }
        struct Done<'a>(&'a AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _done = Done(&engine.processing);
        // SAFETY: `processing` makes this the only live reference.
        let audio = unsafe { &mut *engine.audio.get() };

        engine.drain(audio);
        let tempo = engine.tempo.load(Ordering::Relaxed);
        if tempo != 0 {
            audio.dsp.set_tempo_bpm(f32::from_bits(tempo));
        }
        let frames = frames as usize;
        audio.dsp.begin_block_with_frames(frames);
        for i in 0..frames {
            // Read before write: an output may alias its input.
            // SAFETY: each non-null buffer holds `frames` floats.
            let (l, r) = unsafe {
                let l = *in_left.add(i);
                let r = if in_right.is_null() {
                    l
                } else {
                    *in_right.add(i)
                };
                (l, r)
            };
            let (l, r) = audio.dsp.process_stereo(l, r);
            // SAFETY: as above.
            unsafe {
                *out_left.add(i) = l;
                if !out_right.is_null() {
                    *out_right.add(i) = r;
                }
            }
        }
        engine.publish(audio);
        RODHAREIST_OK
    })
}

/// The host tempo, for synced delays. Any thread; takes effect at the next
/// process call. A non-finite or non-positive tempo is refused.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_set_tempo(
    engine: *mut RodhareistEngine,
    bpm: f64,
) -> RodhareistStatus {
    // SAFETY: the caller's contract.
    let Some(engine) = (unsafe { engine_ref(engine) }) else {
        return RODHAREIST_NULL_POINTER;
    };
    if !(bpm.is_finite() && bpm > 0.0) {
        return RODHAREIST_INVALID_ARGUMENT;
    }
    engine
        .tempo
        .store((bpm as f32).to_bits(), Ordering::Relaxed);
    RODHAREIST_OK
}

/// Samples of delay the engine adds (NAM capture, IR, transpose), as of the
/// last process call. Report it to the host's delay compensation. Any thread.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_latency_samples(engine: *const RodhareistEngine) -> u32 {
    // SAFETY: the caller's contract.
    unsafe { engine_ref(engine) }.map_or(0, |engine| engine.latency.load(Ordering::Acquire))
}

/// Levels as of the last process call. Any thread.
///
/// # Safety
/// `engine` is null or live; `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_meters(
    engine: *const RodhareistEngine,
    out: *mut RodhareistMeters,
) -> RodhareistStatus {
    // SAFETY: the caller's contract.
    let Some(engine) = (unsafe { engine_ref(engine) }) else {
        return RODHAREIST_NULL_POINTER;
    };
    if out.is_null() {
        return RODHAREIST_NULL_POINTER;
    }
    let read = |i: usize| f32::from_bits(engine.meters[i].load(Ordering::Relaxed));
    // SAFETY: the caller's contract.
    unsafe {
        *out = RodhareistMeters {
            in_peak: read(0),
            in_rms: read(1),
            out_peak: read(2),
            out_rms: read(3),
            in_clip: u32::from(read(4) >= 0.5),
            out_clip: u32::from(read(5) >= 0.5),
        };
    }
    RODHAREIST_OK
}

/// Copies up to `count` floats of the telemetry block
/// (`RODHAREIST_TELEMETRY_*` slots) as of the last process call. Any thread.
///
/// # Safety
/// `engine` is null or live; `out` is null or holds `count` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_telemetry(
    engine: *const RodhareistEngine,
    out: *mut f32,
    count: usize,
) -> RodhareistStatus {
    // SAFETY: the caller's contract.
    let Some(engine) = (unsafe { engine_ref(engine) }) else {
        return RODHAREIST_NULL_POINTER;
    };
    if out.is_null() {
        return RODHAREIST_NULL_POINTER;
    }
    for (i, slot) in engine.telemetry.iter().enumerate().take(count) {
        // SAFETY: `out` holds `count` floats.
        unsafe { *out.add(i) = f32::from_bits(slot.load(Ordering::Relaxed)) };
    }
    RODHAREIST_OK
}

// ── Control ───────────────────────────────────────────────────────────────

fn set_param(engine: &RodhareistEngine, index: u32, value: f32) -> RodhareistStatus {
    let mut control = engine.begin();
    let Some(id) = ui_param_id(index) else {
        return fail(&mut control, RODHAREIST_UNKNOWN_PARAM, "no such parameter");
    };
    if !value.is_finite() {
        return fail(
            &mut control,
            RODHAREIST_INVALID_ARGUMENT,
            "a parameter value must be finite",
        );
    }
    let message = if id == "clear_clip" {
        Message::ClearClip
    } else {
        let mut next = control.mirror.clone();
        if !rodharerist::apply_to_params(&mut next, id, value) {
            return fail(&mut control, RODHAREIST_UNKNOWN_PARAM, "no such parameter");
        }
        if !engine.queue.push(Message::Param(index, value)) {
            return fail(
                &mut control,
                RODHAREIST_QUEUE_FULL,
                "the control queue is full",
            );
        }
        control.mirror = next;
        return RODHAREIST_OK;
    };
    if engine.queue.push(message) {
        RODHAREIST_OK
    } else {
        fail(
            &mut control,
            RODHAREIST_QUEUE_FULL,
            "the control queue is full",
        )
    }
}

/// Sets wire parameter `index` to `value`, in the parameter's own units.
/// Control side; reaches the DSP at the next process call.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_set_param(
    engine: *mut RodhareistEngine,
    index: u32,
    value: f32,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        match unsafe { engine_ref(engine) } {
            Some(engine) => set_param(engine, index, value),
            None => RODHAREIST_NULL_POINTER,
        }
    })
}

/// [`rodhareist_engine_set_param`] by the parameter's id.
///
/// # Safety
/// `engine` is null or live; `id` is null or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_set_param_by_id(
    engine: *mut RodhareistEngine,
    id: *const c_char,
    value: f32,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        // SAFETY: the caller's contract.
        let Some(id) = (unsafe { c_str(id) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        match rodharerist::ui_param_index(id) {
            Some(index) => set_param(engine, index, value),
            None => fail(
                &mut engine.control(),
                RODHAREIST_UNKNOWN_PARAM,
                "no such parameter",
            ),
        }
    })
}

/// The value wire parameter `index` was last set to. Control side.
///
/// # Safety
/// `engine` is null or live; `out_value` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_get_param(
    engine: *const RodhareistEngine,
    index: u32,
    out_value: *mut f32,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        if out_value.is_null() {
            return RODHAREIST_NULL_POINTER;
        }
        let control = engine.begin();
        let Some(id) = ui_param_id(index) else {
            return RODHAREIST_UNKNOWN_PARAM;
        };
        let Some(value) = rodharerist::ui_values(&control.mirror)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map(|(_, value)| value)
        else {
            // An action (`clear_clip`) has no value.
            return RODHAREIST_UNKNOWN_PARAM;
        };
        // SAFETY: the caller's contract.
        unsafe { *out_value = value };
        RODHAREIST_OK
    })
}

/// Clears every tail, meter and clip flag at the next process call. Control
/// side.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_reset(
    engine: *mut RodhareistEngine,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        if engine.queue.push(Message::Reset) {
            RODHAREIST_OK
        } else {
            fail(
                &mut engine.control(),
                RODHAREIST_QUEUE_FULL,
                "the control queue is full",
            )
        }
    })
}

/// The whole state as JSON — every parameter, the stage path and the names
/// of the loaded capture and IR (not their contents) — into `buf`.
/// `out_len` receives its length without the NUL; pass a null `buf` to ask
/// the length first. Control side.
///
/// # Safety
/// `engine` is null or live; `buf` is null or holds `capacity` bytes;
/// `out_len` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_get_state(
    engine: *const RodhareistEngine,
    buf: *mut c_char,
    capacity: usize,
    out_len: *mut usize,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let mut control = engine.begin();
        match RodhareistState::new(control.mirror.clone()).to_json() {
            // SAFETY: the caller's contract.
            Ok(json) => unsafe { write_text(json.as_bytes(), buf, capacity, out_len) },
            Err(error) => fail(&mut control, RODHAREIST_PARSE_FAILED, &error.to_string()),
        }
    })
}

/// Restores a state from [`rodhareist_engine_get_state`] (`len` bytes of
/// JSON). The capture and IR it names are not loaded: load them with
/// [`rodhareist_engine_load_nam`] and [`rodhareist_engine_load_ir`]. Control
/// side; reaches the DSP at the next process call.
///
/// # Safety
/// `engine` is null or live; `json` is null or holds `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_set_state(
    engine: *mut RodhareistEngine,
    json: *const c_char,
    len: usize,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let mut control = engine.begin();
        // SAFETY: the caller's contract.
        let Some(text) = (unsafe { bytes(json.cast(), len) }) else {
            return fail(&mut control, RODHAREIST_NULL_POINTER, "no state given");
        };
        let Ok(text) = std::str::from_utf8(text) else {
            return fail(
                &mut control,
                RODHAREIST_PARSE_FAILED,
                "the state is not UTF-8",
            );
        };
        match RodhareistState::from_json(text) {
            Ok(state) => engine.submit_params(&mut control, state.params),
            Err(error) => fail(
                &mut control,
                RODHAREIST_PARSE_FAILED,
                &format!("the state could not be read: {error}"),
            ),
        }
    })
}

/// Loads factory preset `index`. Power, the input trim and the loaded
/// capture and IR stay as they are. Control side.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_load_preset(
    engine: *mut RodhareistEngine,
    index: u32,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let mut control = engine.begin();
        let Some(preset) = rodharerist::factory_presets().get(index as usize) else {
            return fail(&mut control, RODHAREIST_NO_SUCH_PRESET, "no such preset");
        };
        let mut params = preset.params.clone();
        params.power = control.mirror.power;
        params.input_trim_db = control.mirror.input_trim_db;
        params.nam_file = control.mirror.nam_file.clone();
        params.ir_file = control.mirror.ir_file.clone();
        engine.submit_params(&mut control, params)
    })
}

/// Loads a `.nam` capture (`len` bytes of its JSON), named `name`, into the
/// Amp slot. `stereo` runs two models for true stereo; `full_rig` marks a
/// capture that already includes the cabinet. Loading does not switch the
/// amp to it: set `tone_engine` to 1 for that. Control side, and slow for a
/// large capture; the audio side swaps it in at a block start, crossfading.
///
/// # Safety
/// `engine` is null or live; `json` holds `len` bytes; `name` is null or
/// NUL-terminated; `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_load_nam(
    engine: *mut RodhareistEngine,
    json: *const c_char,
    len: usize,
    name: *const c_char,
    stereo: u32,
    full_rig: u32,
    out: *mut RodhareistNamInfo,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let mut control = engine.begin();
        // SAFETY: the caller's contract.
        let Some(text) = (unsafe { bytes(json.cast(), len) }) else {
            return fail(&mut control, RODHAREIST_NULL_POINTER, "no capture given");
        };
        let Ok(text) = std::str::from_utf8(text) else {
            return fail(
                &mut control,
                RODHAREIST_PARSE_FAILED,
                "the capture is not UTF-8 JSON",
            );
        };
        // SAFETY: the caller's contract.
        let name = unsafe { c_str(name) }.unwrap_or("Capture").to_string();
        match control.nam.load_json(
            text,
            name.clone(),
            engine.sample_rate,
            stereo != 0,
            full_rig != 0,
        ) {
            Ok(info) => {
                control.mirror.nam_file = Some(name);
                if !out.is_null() {
                    // SAFETY: the caller's contract.
                    unsafe {
                        *out = RodhareistNamInfo {
                            sample_rate: info.sample_rate,
                            receptive_field: info.receptive_field as u32,
                            submodel_count: info.submodel_count as u32,
                            full_rig: u32::from(info.full_rig),
                            slimmable: u32::from(info.slimmable),
                            architecture: fixed_text(&info.architecture),
                            family: fixed_text(&info.family),
                        };
                    }
                }
                RODHAREIST_OK
            }
            Err(error) => fail(&mut control, RODHAREIST_LOAD_FAILED, &error.to_string()),
        }
    })
}

/// Drops the loaded capture at the next block start. Control side.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_unload_nam(
    engine: *mut RodhareistEngine,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let mut control = engine.begin();
        drop(control.nam.unload());
        control.nam.collect_garbage();
        control.mirror.nam_file = None;
        RODHAREIST_OK
    })
}

/// Loads an impulse response (`len` bytes of a `.wav` file), named `name`,
/// for the Cabinet slot. It sounds once `cab_model` is set to
/// [`rodhareist_cab_model_ir`]. Control side.
///
/// # Safety
/// `engine` is null or live; `wav` holds `len` bytes; `name` is null or
/// NUL-terminated; `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_load_ir(
    engine: *mut RodhareistEngine,
    wav: *const u8,
    len: usize,
    name: *const c_char,
    out: *mut RodhareistIrInfo,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let mut control = engine.begin();
        // SAFETY: the caller's contract.
        let Some(data) = (unsafe { bytes(wav, len) }) else {
            return fail(
                &mut control,
                RODHAREIST_NULL_POINTER,
                "no impulse response given",
            );
        };
        // SAFETY: the caller's contract.
        let name = unsafe { c_str(name) }.unwrap_or("Impulse").to_string();
        match control.ir.load_wav(data, name.clone(), engine.sample_rate) {
            Ok(info) => {
                control.mirror.ir_file = Some(name);
                if !out.is_null() {
                    // SAFETY: the caller's contract.
                    unsafe {
                        *out = RodhareistIrInfo {
                            source_sample_rate: info.source_sample_rate,
                            frames: info.frames as u64,
                            latency_samples: info.latency_samples as u32,
                            stereo: u32::from(info.stereo),
                            truncated: u32::from(info.truncated),
                        };
                    }
                }
                RODHAREIST_OK
            }
            Err(error) => fail(&mut control, RODHAREIST_LOAD_FAILED, &error.to_string()),
        }
    })
}

/// Drops the loaded impulse response at the next block start. Control side.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_unload_ir(
    engine: *mut RodhareistEngine,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let mut control = engine.begin();
        control.ir.clear();
        control.mirror.ir_file = None;
        RODHAREIST_OK
    })
}

/// Frees what the audio side has finished with (a replaced capture, IR or
/// state). Control side; call it now and then — every control call that
/// loads something also does.
///
/// # Safety
/// `engine` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_collect_garbage(
    engine: *mut RodhareistEngine,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        let control = engine.begin();
        engine.collect_retired();
        control.nam.collect_garbage();
        control.ir.collect_garbage();
        RODHAREIST_OK
    })
}

/// Why the last failed control call failed, NUL-terminated, into `buf`
/// (empty when none has). Same buffer rules as
/// [`rodhareist_engine_get_state`].
///
/// # Safety
/// `engine` is null or live; `buf` is null or holds `capacity` bytes;
/// `out_len` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rodhareist_engine_last_error(
    engine: *const RodhareistEngine,
    buf: *mut c_char,
    capacity: usize,
    out_len: *mut usize,
) -> RodhareistStatus {
    guard(|| {
        // SAFETY: the caller's contract.
        let Some(engine) = (unsafe { engine_ref(engine) }) else {
            return RODHAREIST_NULL_POINTER;
        };
        // Reading the reason must not clear it.
        let control = engine.control();
        // SAFETY: the caller's contract.
        unsafe { write_text(control.last_error.as_bytes(), buf, capacity, out_len) }
    })
}

#[cfg(test)]
mod tests;
