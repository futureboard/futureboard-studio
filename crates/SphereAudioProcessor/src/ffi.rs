use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use crate::stretching::{
    StretchAlgorithm, StretchBackend, StretchError, StretchMode, StretchParams, StretchProcessor,
    create_stretch_processor, resolve_backend,
};

const VERSION: u32 = 0x0001_0000;

#[repr(C)]
pub struct SphereStretchHandle {
    _private: [u8; 0],
}

struct StretchHandleState {
    sample_rate: f32,
    channels: u32,
    params: StretchParams,
    backend: StretchBackend,
    processor: Box<dyn StretchProcessor + Send>,
}

/// The state behind a handle, for the length of one export call.
///
/// # Safety
///
/// `handle` is null or came from [`sphere_stretch_create`], has not been
/// passed to [`sphere_stretch_destroy`], and no other call uses it while the
/// returned reference lives.
unsafe fn state_from_handle<'a>(
    handle: *mut SphereStretchHandle,
) -> Option<&'a mut StretchHandleState> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: a live handle is a `Box<StretchHandleState>` from
    // `sphere_stretch_create`, used by one call at a time (caller's contract).
    Some(unsafe { &mut *(handle.cast::<StretchHandleState>()) })
}

#[unsafe(no_mangle)]
pub extern "C" fn sphere_audio_processor_version() -> u32 {
    VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn sphere_stretch_create(
    sample_rate: f32,
    channels: u32,
) -> *mut SphereStretchHandle {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if !sample_rate.is_finite() || sample_rate <= 0.0 || channels == 0 || channels > 2 {
            return ptr::null_mut();
        }

        let params = StretchParams::default();
        let backend = resolve_backend(&params);
        let processor =
            match create_stretch_processor(backend, sample_rate, channels as usize, params.clone())
            {
                Ok(processor) => processor,
                Err(_) => return ptr::null_mut(),
            };

        let state = StretchHandleState {
            sample_rate,
            channels,
            params,
            backend,
            processor,
        };
        Box::into_raw(Box::new(state)).cast::<SphereStretchHandle>()
    }));

    result.unwrap_or(ptr::null_mut())
}

/// Free a handle. Null is ignored.
///
/// # Safety
///
/// `handle` is null or came from [`sphere_stretch_create`] and has not been
/// destroyed already; no other call is using it, and it is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sphere_stretch_destroy(handle: *mut SphereStretchHandle) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return;
        }
        // SAFETY: a live handle from `sphere_stretch_create`, freed once
        // (caller's contract).
        unsafe {
            drop(Box::from_raw(handle.cast::<StretchHandleState>()));
        }
    }));
}

/// Clear the processor's history. Null is ignored.
///
/// # Safety
///
/// `handle` is null or a live handle from [`sphere_stretch_create`], used by
/// no other call at the same time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sphere_stretch_reset(handle: *mut SphereStretchHandle) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: forwarded from this function's contract.
        let Some(state) = (unsafe { state_from_handle(handle) }) else {
            return;
        };
        state.processor.reset();
    }));
}

/// Set the stretch mode, algorithm and ratios. Returns 0, or a negative code:
/// -1 null handle, -2 invalid parameters, -3 backend unavailable, -4 panic.
///
/// # Safety
///
/// `handle` is null or a live handle from [`sphere_stretch_create`], used by
/// no other call at the same time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sphere_stretch_set_params(
    handle: *mut SphereStretchHandle,
    mode: u32,
    algorithm: u32,
    time_ratio: f32,
    pitch_ratio: f32,
    source_bpm: f32,
    target_bpm: f32,
    preserve_pitch: bool,
    quality: f32,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: forwarded from this function's contract.
        let Some(state) = (unsafe { state_from_handle(handle) }) else {
            return -1;
        };

        let params = match build_params(
            mode,
            algorithm,
            time_ratio,
            pitch_ratio,
            source_bpm,
            target_bpm,
            preserve_pitch,
            quality,
        ) {
            Ok(params) => params,
            Err(_) => return -2,
        };

        let backend = resolve_backend(&params);
        if backend != state.backend {
            match create_stretch_processor(
                backend,
                state.sample_rate,
                state.channels as usize,
                params.clone(),
            ) {
                Ok(processor) => {
                    state.processor = processor;
                    state.backend = backend;
                }
                Err(_) => return -3,
            }
        }
        state.params = params.clone();
        state.processor.set_params(params);
        0
    }));

    result.unwrap_or(-4)
}

/// Stretch `frames` stereo frames. Returns 0, or a negative code: -1 null or
/// overlapping buffers, -2 length mismatch, -3 processing error, -4 panic.
///
/// # Safety
///
/// `handle` is a live handle from [`sphere_stretch_create`], used by no other
/// call at the same time. Each buffer is valid for `frames` `f32`s, the inputs
/// for reads and the outputs for writes. An output may not overlap any other
/// buffer; overlapping ones are refused with -1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sphere_stretch_process_stereo(
    handle: *mut SphereStretchHandle,
    input_l: *const f32,
    input_r: *const f32,
    output_l: *mut f32,
    output_r: *mut f32,
    frames: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null()
            || input_l.is_null()
            || input_r.is_null()
            || output_l.is_null()
            || output_r.is_null()
            || frames == 0
        {
            return -1;
        }
        // An output aliasing anything else would be a `&mut` overlapping
        // another reference: refuse it rather than hand that to the processor.
        let Some(bytes) = frames.checked_mul(std::mem::size_of::<f32>()) else {
            return -1;
        };
        let range = |p: *const f32| (p as usize, (p as usize).saturating_add(bytes));
        let overlaps = |a: (usize, usize), b: (usize, usize)| a.0 < b.1 && b.0 < a.1;
        let (il, ir) = (range(input_l), range(input_r));
        let (ol, or) = (range(output_l), range(output_r));
        if overlaps(ol, or) || [il, ir].iter().any(|&i| overlaps(i, ol) || overlaps(i, or)) {
            return -1;
        }

        // SAFETY: forwarded from this function's contract.
        let Some(state) = (unsafe { state_from_handle(handle) }) else {
            return -1;
        };

        // SAFETY: each pointer is non-null and valid for `frames` values
        // (caller's contract), and no output overlaps another buffer (checked
        // above), so the `&mut` slices alias nothing.
        let input_l = unsafe { std::slice::from_raw_parts(input_l, frames) };
        let input_r = unsafe { std::slice::from_raw_parts(input_r, frames) };
        let output_l = unsafe { std::slice::from_raw_parts_mut(output_l, frames) };
        let output_r = unsafe { std::slice::from_raw_parts_mut(output_r, frames) };

        match state
            .processor
            .process_stereo(input_l, input_r, output_l, output_r)
        {
            Ok(()) => 0,
            Err(StretchError::BufferLengthMismatch) => -2,
            Err(_) => -3,
        }
    }));

    result.unwrap_or(-4)
}

// ============================================================================
// Offline analysis FFI (BPM / key / instrument classification).
//
// This is an offline/control-thread surface. It allocates and runs FFTs; never
// call it from a realtime audio callback.
// ============================================================================

use crate::analysis::{AnalysisOptions, InstrumentCategory, KeyMode, PitchClass, analyze_mono};

/// Plain-old-data analysis result for the C ABI.
///
/// `has_*` flags report whether the corresponding estimate is valid. Enum
/// fields use the integer codes documented below; `-1` means "no estimate".
///
/// `key_tonic`: 0..=11 (0 = C ... 11 = B). `key_mode`: 0 = major, 1 = minor.
/// `instrument`: 0 Vocal, 1 Bass, 2 Drums, 3 Keys, 4 Guitar, 5 Strings,
/// 6 Synth, 7 Other.
#[repr(C)]
pub struct SphereAnalysisResult {
    pub has_bpm: bool,
    pub bpm: f32,
    pub bpm_confidence: f32,

    pub has_key: bool,
    pub key_tonic: i32,
    pub key_mode: i32,
    pub key_confidence: f32,

    pub has_instrument: bool,
    pub instrument: i32,
    pub instrument_confidence: f32,

    pub duration_secs: f32,
}

impl SphereAnalysisResult {
    fn empty() -> Self {
        Self {
            has_bpm: false,
            bpm: 0.0,
            bpm_confidence: 0.0,
            has_key: false,
            key_tonic: -1,
            key_mode: -1,
            key_confidence: 0.0,
            has_instrument: false,
            instrument: -1,
            instrument_confidence: 0.0,
            duration_secs: 0.0,
        }
    }
}

fn pitch_class_code(pc: PitchClass) -> i32 {
    match pc {
        PitchClass::C => 0,
        PitchClass::Cs => 1,
        PitchClass::D => 2,
        PitchClass::Ds => 3,
        PitchClass::E => 4,
        PitchClass::F => 5,
        PitchClass::Fs => 6,
        PitchClass::G => 7,
        PitchClass::Gs => 8,
        PitchClass::A => 9,
        PitchClass::As => 10,
        PitchClass::B => 11,
    }
}

fn instrument_code(cat: InstrumentCategory) -> i32 {
    match cat {
        InstrumentCategory::Vocal => 0,
        InstrumentCategory::Bass => 1,
        InstrumentCategory::Drums => 2,
        InstrumentCategory::Keys => 3,
        InstrumentCategory::Guitar => 4,
        InstrumentCategory::Strings => 5,
        InstrumentCategory::Synth => 6,
        InstrumentCategory::Other => 7,
    }
}

/// Analyse a mono buffer and fill `out`. Returns 0 on success, negative on
/// error (invalid arguments or a caught panic). On error, `out` is untouched.
///
/// # Safety
/// `samples` must point to `frames` valid `f32` values, and `out` must point to
/// a writable `SphereAnalysisResult`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sphere_analyze_mono(
    samples: *const f32,
    frames: usize,
    sample_rate: f32,
    out: *mut SphereAnalysisResult,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() || samples.is_null() || frames == 0 {
            return -1;
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return -2;
        }

        let input = unsafe { std::slice::from_raw_parts(samples, frames) };
        let analysis = analyze_mono(input, sample_rate, AnalysisOptions::default());

        let mut ffi = SphereAnalysisResult::empty();
        ffi.duration_secs = analysis.duration_secs;
        if let Some(bpm) = analysis.bpm {
            ffi.has_bpm = true;
            ffi.bpm = bpm.bpm;
            ffi.bpm_confidence = bpm.confidence;
        }
        if let Some(key) = analysis.key {
            ffi.has_key = true;
            ffi.key_tonic = pitch_class_code(key.tonic);
            ffi.key_mode = match key.mode {
                KeyMode::Major => 0,
                KeyMode::Minor => 1,
            };
            ffi.key_confidence = key.confidence;
        }
        if let Some(inst) = analysis.instrument {
            ffi.has_instrument = true;
            ffi.instrument = instrument_code(inst.category);
            ffi.instrument_confidence = inst.confidence;
        }

        unsafe { ptr::write(out, ffi) };
        0
    }));

    result.unwrap_or(-4)
}

/// Analyse a mono buffer using a learned ONNX classifier for the instrument
/// field (BPM and key still use the built-in DSP). `model_path` is UTF-8 of
/// length `model_path_len` (not NUL-terminated).
///
/// Returns 0 on success, negative on error. On model-load failure the call
/// returns an error and leaves `out` untouched; per-block inference failures
/// fall back to the heuristic classifier inside the analysis.
///
/// Only available when the crate is built with the `onnx` feature.
///
/// # Safety
/// `samples` must point to `frames` valid `f32`; `model_path` must point to
/// `model_path_len` valid bytes; `out` must be a writable pointer.
#[cfg(feature = "onnx")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sphere_analyze_mono_onnx(
    samples: *const f32,
    frames: usize,
    sample_rate: f32,
    model_path: *const u8,
    model_path_len: usize,
    out: *mut SphereAnalysisResult,
) -> i32 {
    use crate::analysis::{OnnxClassifier, analyze_mono_with};

    let result = catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() || samples.is_null() || frames == 0 || model_path.is_null() {
            return -1;
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return -2;
        }

        let path_bytes = unsafe { std::slice::from_raw_parts(model_path, model_path_len) };
        let Ok(path) = std::str::from_utf8(path_bytes) else {
            return -3;
        };

        let classifier = match OnnxClassifier::from_path(path) {
            Ok(classifier) => classifier,
            Err(_) => return -5,
        };

        let input = unsafe { std::slice::from_raw_parts(samples, frames) };
        let analysis =
            analyze_mono_with(input, sample_rate, AnalysisOptions::default(), &classifier);

        let mut ffi = SphereAnalysisResult::empty();
        ffi.duration_secs = analysis.duration_secs;
        if let Some(bpm) = analysis.bpm {
            ffi.has_bpm = true;
            ffi.bpm = bpm.bpm;
            ffi.bpm_confidence = bpm.confidence;
        }
        if let Some(key) = analysis.key {
            ffi.has_key = true;
            ffi.key_tonic = pitch_class_code(key.tonic);
            ffi.key_mode = match key.mode {
                KeyMode::Major => 0,
                KeyMode::Minor => 1,
            };
            ffi.key_confidence = key.confidence;
        }
        if let Some(inst) = analysis.instrument {
            ffi.has_instrument = true;
            ffi.instrument = instrument_code(inst.category);
            ffi.instrument_confidence = inst.confidence;
        }

        unsafe { ptr::write(out, ffi) };
        0
    }));

    result.unwrap_or(-4)
}

fn build_params(
    mode: u32,
    algorithm: u32,
    time_ratio: f32,
    pitch_ratio: f32,
    source_bpm: f32,
    target_bpm: f32,
    preserve_pitch: bool,
    quality: f32,
) -> Result<StretchParams, StretchError> {
    let mode = match mode {
        0 => StretchMode::Off,
        1 => StretchMode::Manual,
        2 => StretchMode::TempoSync,
        3 => StretchMode::Warp,
        _ => return Err(StretchError::InvalidParams("invalid stretch mode".into())),
    };
    let algorithm = match algorithm {
        0 => StretchAlgorithm::Off,
        1 => StretchAlgorithm::RePitch,
        2 => StretchAlgorithm::PreservePitch,
        _ => {
            return Err(StretchError::InvalidParams(
                "invalid stretch algorithm".into(),
            ));
        }
    };

    Ok(StretchParams {
        mode,
        algorithm,
        time_ratio,
        pitch_ratio,
        source_bpm: optional_positive(source_bpm),
        target_bpm: optional_positive(target_bpm),
        preserve_pitch,
        quality,
    })
}

fn optional_positive(value: f32) -> Option<f32> {
    if value.is_finite() && value > 0.0 {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_create_process_destroy_roundtrip() {
        let handle = sphere_stretch_create(48_000.0, 2);
        assert!(!handle.is_null());

        assert_eq!(
            unsafe {
                sphere_stretch_set_params(handle, 1, 1, 2.0, 1.0, f32::NAN, f32::NAN, false, 0.75)
            },
            0
        );

        let input_l = [0.0_f32, 0.25, 0.5, 0.75];
        let input_r = [0.0_f32, 0.25, 0.5, 0.75];
        let mut output_l = [0.0; 4];
        let mut output_r = [0.0; 4];

        assert_eq!(
            unsafe {
                sphere_stretch_process_stereo(
                    handle,
                    input_l.as_ptr(),
                    input_r.as_ptr(),
                    output_l.as_mut_ptr(),
                    output_r.as_mut_ptr(),
                    4,
                )
            },
            0
        );

        // SAFETY: `handle` is live and used only here; destroyed once.
        unsafe {
            sphere_stretch_reset(handle);
            sphere_stretch_destroy(handle);
            sphere_stretch_destroy(ptr::null_mut());
        }
    }

    #[test]
    fn overlapping_buffers_are_refused() {
        let handle = sphere_stretch_create(48_000.0, 2);
        assert!(!handle.is_null());
        let input = [0.25_f32; 8];
        let mut shared = [0.0_f32; 8];
        let mut other = [0.0_f32; 8];
        let out = shared.as_mut_ptr();
        // SAFETY: valid buffers of 4 frames each; the call refuses the
        // aliasing before it reads or writes any of them.
        unsafe {
            // Both outputs on one buffer.
            assert_eq!(
                sphere_stretch_process_stereo(handle, input.as_ptr(), input.as_ptr(), out, out, 4),
                -1
            );
            // An output over an input (in place).
            assert_eq!(
                sphere_stretch_process_stereo(
                    handle,
                    out,
                    input.as_ptr(),
                    out,
                    other.as_mut_ptr(),
                    4
                ),
                -1
            );
            // Overlapping halves of one buffer.
            assert_eq!(
                sphere_stretch_process_stereo(
                    handle,
                    input.as_ptr(),
                    input.as_ptr(),
                    out,
                    out.add(2),
                    4
                ),
                -1
            );
            // Two inputs may share a buffer: they are only read.
            assert_eq!(
                sphere_stretch_process_stereo(
                    handle,
                    input.as_ptr(),
                    input.as_ptr(),
                    out,
                    out.add(4),
                    4
                ),
                0
            );
            sphere_stretch_destroy(handle);
        }
    }

    #[test]
    fn version_is_non_zero() {
        assert_ne!(sphere_audio_processor_version(), 0);
    }
}
