//! C-ABI FFI bindings for `CantoEngine` (real audio loading, pitch/note analysis,
//! and real-time TD-PSOLA playback/audition).

use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::slice;

use crate::engine::{CantoAnalysisResult, CantoEngine};
use crate::scale::Scale;
use crate::voicetune::{VoiceTuneAnalysisConfig, VoiceTuneAnalyzer};

fn result_to_c_json(res: Result<CantoAnalysisResult, String>) -> *mut c_char {
    let payload = match res {
        Ok(r) => r,
        Err(err) => CantoAnalysisResult {
            ok: false,
            error: Some(err),
            sample_rate: 48_000,
            duration_sec: 0.0,
            bpm: 120.0,
            key: "A".to_string(),
            mode: "minor".to_string(),
            notes: Vec::new(),
            waveform_peaks: Vec::new(),
        },
    };
    let json = serde_json::to_string(&payload).unwrap_or_else(|e| {
        format!(
            r#"{{"ok":false,"error":"JSON serialization failed: {}","sample_rate":48000,"duration_sec":0.0,"bpm":120.0,"key":"A","mode":"minor","notes":[],"waveform_peaks":[]}}"#,
            e
        )
    });
    CString::new(json)
        .unwrap_or_else(|_| CString::new(r#"{"ok":false,"error":"CString null byte"}"#).unwrap())
        .into_raw()
}

/// Frees a C string returned by `canto_engine_load_audio_file`, `canto_engine_load_audio_bytes`,
/// or `canto_engine_analyze`.
///
/// # Safety
/// `ptr` must be a valid pointer returned by one of the `canto_engine_*` JSON functions, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            drop(CString::from_raw(ptr));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn canto_engine_new(sample_rate: f32) -> *mut CantoEngine {
    Box::into_raw(Box::new(CantoEngine::new(sample_rate)))
}

/// # Safety
/// `ptr` must be a valid pointer returned by `canto_engine_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_free(ptr: *mut CantoEngine) {
    if !ptr.is_null() {
        unsafe {
            drop(Box::from_raw(ptr));
        }
    }
}

/// Loads an audio file from `path_ptr`, decodes it to mono at the engine's sample rate,
/// runs vocal pitch/note/key/BPM analysis, and returns a JSON `CantoAnalysisResult` string.
/// The caller must free the returned string with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid `CantoEngine` pointer and `path_ptr` must be a valid null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_load_audio_file(
    ptr: *mut CantoEngine,
    path_ptr: *const c_char,
) -> *mut c_char {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return result_to_c_json(Err("Null engine pointer".to_string()));
    };
    if path_ptr.is_null() {
        return result_to_c_json(Err("Null file path".to_string()));
    }
    let path_str = match unsafe { CStr::from_ptr(path_ptr) }.to_str() {
        Ok(s) => s,
        Err(e) => return result_to_c_json(Err(format!("Invalid UTF-8 path: {e}"))),
    };
    result_to_c_json(engine.load_audio_file(Path::new(path_str)))
}

/// Loads audio from raw bytes in memory (`bytes_ptr`, `len`) with an optional file extension hint
/// (`ext_ptr`, e.g. `"wav"`, `"mp3"`), runs analysis, and returns a JSON `CantoAnalysisResult` string.
/// The caller must free the returned string with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid `CantoEngine` pointer and `bytes_ptr` must point to `len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_load_audio_bytes(
    ptr: *mut CantoEngine,
    bytes_ptr: *const u8,
    len: usize,
    ext_ptr: *const c_char,
) -> *mut c_char {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return result_to_c_json(Err("Null engine pointer".to_string()));
    };
    if bytes_ptr.is_null() || len == 0 {
        return result_to_c_json(Err("Empty audio byte buffer".to_string()));
    }
    let bytes = unsafe { slice::from_raw_parts(bytes_ptr, len) };
    let ext_hint = if ext_ptr.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(ext_ptr) }.to_str().ok()
    };
    result_to_c_json(engine.load_audio_bytes(bytes, ext_hint))
}

/// Re-runs vocal pitch/note/key/BPM analysis on the currently loaded audio buffer
/// and returns a JSON `CantoAnalysisResult` string.
/// The caller must free the returned string with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid `CantoEngine` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_analyze(ptr: *mut CantoEngine) -> *mut c_char {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return result_to_c_json(Err("Null engine pointer".to_string()));
    };
    result_to_c_json(engine.analyze_loaded_audio())
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_transport(
    ptr: *mut CantoEngine,
    playing: bool,
    loop_enabled: bool,
    bypass: bool,
) {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    engine.playing = playing;
    engine.loop_enabled = loop_enabled;
    engine.bypass = bypass;
    if playing {
        engine.stop_audition();
    }
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_is_playing(ptr: *const CantoEngine) -> bool {
    let Some(engine) = (unsafe { ptr.as_ref() }) else {
        return false;
    };
    engine.playing
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_seek(ptr: *mut CantoEngine, time_sec: f64) {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    engine.seek(time_sec);
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_playhead(ptr: *const CantoEngine) -> f64 {
    let Some(engine) = (unsafe { ptr.as_ref() }) else {
        return 0.0;
    };
    engine.playhead_sec
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_duration(ptr: *const CantoEngine) -> f64 {
    let Some(engine) = (unsafe { ptr.as_ref() }) else {
        return 0.0;
    };
    engine.duration_sec
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_level(ptr: *const CantoEngine) -> f32 {
    let Some(engine) = (unsafe { ptr.as_ref() }) else {
        return 0.0;
    };
    engine.meter_level()
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_key_mode(
    ptr: *mut CantoEngine,
    key: u8,
    mode_id: u8,
) {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    let scale = match mode_id {
        0 => Scale::Minor,
        1 => Scale::Major,
        2 => Scale::MinorPentatonic,
        _ => Scale::Chromatic,
    };
    engine.set_key_and_scale(key, scale);
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_correct(ptr: *mut CantoEngine, amount: f32) {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    engine.set_correct_amount(amount);
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_update_note(
    ptr: *mut CantoEngine,
    index: u32,
    pitch: f32,
    start: f32,
    dur: f32,
    modulation: f32,
    formant: f32,
    gain_db: f32,
    pinned: bool,
) {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    engine.update_note(
        index as usize,
        pitch,
        start,
        dur,
        modulation,
        formant,
        gain_db,
        pinned,
    );
}

/// Triggers or stops real-audio audition of a note (`index >= 0` auditions note `index`;
/// `index < 0` stops audition).
///
/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_audition_note(ptr: *mut CantoEngine, index: i32) {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    if index < 0 {
        engine.stop_audition();
    } else {
        engine.trigger_note_audition(index as usize);
    }
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_direct_voice(
    ptr: *mut CantoEngine,
    hz: f32,
    cutoff: f32,
    gain: f32,
) {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    engine.set_direct_voice(hz, cutoff, gain);
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`, and `out_ptr` must point to at least `frames * 2` f32 values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_render_f32(
    ptr: *mut CantoEngine,
    out_ptr: *mut f32,
    frames: usize,
) -> f32 {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return 0.0;
    };
    if out_ptr.is_null() || frames == 0 {
        return 0.0;
    }
    let slice = unsafe { slice::from_raw_parts_mut(out_ptr, frames * 2) };
    engine.render_stereo_f32(slice)
}

/// # Safety
/// `ptr` must be a valid pointer to `CantoEngine`, and `out_ptr` must point to at least `frames * 2` i16 values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_render_i16(
    ptr: *mut CantoEngine,
    out_ptr: *mut i16,
    frames: usize,
) -> f32 {
    let Some(engine) = (unsafe { ptr.as_mut() }) else {
        return 0.0;
    };
    if out_ptr.is_null() || frames == 0 {
        return 0.0;
    }
    let slice = unsafe { slice::from_raw_parts_mut(out_ptr, frames * 2) };
    engine.render_stereo_i16(slice)
}

/// Analyzes a monophonic float buffer and returns the number of segmented vocal notes detected.
/// # Safety
/// `samples_ptr` must point to `len` valid `f32` samples.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_analyze_mono_note_count(
    samples_ptr: *const f32,
    len: usize,
    sample_rate: u32,
) -> i32 {
    if samples_ptr.is_null() || len == 0 || sample_rate == 0 {
        return 0;
    }
    let samples = unsafe { slice::from_raw_parts(samples_ptr, len) };
    let config = VoiceTuneAnalysisConfig::default();
    match VoiceTuneAnalyzer::analyze_mono(samples, sample_rate, &config) {
        Ok(doc) => doc.notes.len() as i32,
        Err(_) => -1,
    }
}
