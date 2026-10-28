//! C-ABI FFI bindings for `CantoEngine` (real audio loading, pitch/note analysis,
//! and real-time TD-PSOLA playback/audition).
//!
//! The handle owns the engine behind `output::Shared` so the Android audio
//! thread (`canto_output_*`) and the UI thread can use it concurrently. UI
//! calls take the engine lock briefly; transport reads use atomics.

use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::audio_io::{decode_audio_bytes_to_mono, decode_audio_file_to_mono};
use crate::engine::{CantoAnalysisResult, CantoEngine};
use crate::load_job::{JobKind, LoadJob, STAGE_DONE};
use crate::output::Shared;
use crate::recorder::Recorder;
use crate::scale::Scale;
use crate::voicetune::{VoiceTuneAnalysisConfig, VoiceTuneAnalyzer};

/// Number of `f32` fields per note in `canto_engine_sync_notes`.
pub const NOTE_STRIDE: usize = 12;

pub struct CantoHandle {
    shared: Arc<Shared>,
    #[cfg(target_os = "android")]
    output: Option<crate::output::Output>,
}

impl CantoHandle {
    fn engine(&self) -> std::sync::MutexGuard<'_, CantoEngine> {
        self.shared.lock()
    }

    /// Runs `f` on the locked engine and republishes transport state.
    fn edit<R>(&self, f: impl FnOnce(&mut CantoEngine) -> R) -> R {
        let mut engine = self.engine();
        let r = f(&mut engine);
        self.shared.publish(&engine);
        r
    }
}

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
pub extern "C" fn canto_engine_new(sample_rate: f32) -> *mut CantoHandle {
    Box::into_raw(Box::new(CantoHandle {
        shared: Arc::new(Shared::new(CantoEngine::new(sample_rate))),
        #[cfg(target_os = "android")]
        output: None,
    }))
}

/// # Safety
/// `ptr` must be a valid pointer returned by `canto_engine_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_free(ptr: *mut CantoHandle) {
    if !ptr.is_null() {
        // Dropping the handle stops and closes the output stream first.
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
/// `ptr` must be a valid handle and `path_ptr` must be a valid null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_load_audio_file(
    ptr: *mut CantoHandle,
    path_ptr: *const c_char,
) -> *mut c_char {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return result_to_c_json(Err("Null engine pointer".to_string()));
    };
    if path_ptr.is_null() {
        return result_to_c_json(Err("Null file path".to_string()));
    }
    let path_str = match unsafe { CStr::from_ptr(path_ptr) }.to_str() {
        Ok(s) => s,
        Err(e) => return result_to_c_json(Err(format!("Invalid UTF-8 path: {e}"))),
    };
    // Decode without holding the engine lock so the audio thread keeps running.
    let decoded =
        decode_audio_file_to_mono(Path::new(path_str), handle.shared.sample_rate() as u32);
    result_to_c_json(decoded.and_then(|d| handle.edit(|e| e.install_decoded(d))))
}

/// Like `canto_engine_load_audio_file`, but adopts `analysis_ptr` (a JSON
/// `CantoAnalysisResult` this engine produced earlier for the same file)
/// instead of re-analysing. Fails if the analysis doesn't parse or doesn't
/// match the audio; the caller then loads normally.
/// The caller must free the returned string with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid handle; `path_ptr` and `analysis_ptr` must be valid
/// null-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_load_audio_file_cached(
    ptr: *mut CantoHandle,
    path_ptr: *const c_char,
    analysis_ptr: *const c_char,
) -> *mut c_char {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return result_to_c_json(Err("Null engine pointer".to_string()));
    };
    if path_ptr.is_null() || analysis_ptr.is_null() {
        return result_to_c_json(Err("Null argument".to_string()));
    }
    let (Ok(path_str), Ok(analysis_str)) = (
        unsafe { CStr::from_ptr(path_ptr) }.to_str(),
        unsafe { CStr::from_ptr(analysis_ptr) }.to_str(),
    ) else {
        return result_to_c_json(Err("Invalid UTF-8 argument".to_string()));
    };
    // Parse and decode without holding the engine lock.
    let analysis = match serde_json::from_str::<CantoAnalysisResult>(analysis_str) {
        Ok(a) => a,
        Err(e) => return result_to_c_json(Err(format!("Bad cached analysis: {e}"))),
    };
    let decoded =
        decode_audio_file_to_mono(Path::new(path_str), handle.shared.sample_rate() as u32);
    result_to_c_json(
        decoded.and_then(|d| handle.edit(|e| e.install_decoded_with_analysis(d, analysis))),
    )
}

/// Loads audio from raw bytes in memory (`bytes_ptr`, `len`) with an optional file extension hint
/// (`ext_ptr`, e.g. `"wav"`, `"mp3"`), runs analysis, and returns a JSON `CantoAnalysisResult` string.
/// The caller must free the returned string with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid handle and `bytes_ptr` must point to `len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_load_audio_bytes(
    ptr: *mut CantoHandle,
    bytes_ptr: *const u8,
    len: usize,
    ext_ptr: *const c_char,
) -> *mut c_char {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
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
    let decoded = decode_audio_bytes_to_mono(bytes, ext_hint, handle.shared.sample_rate() as u32);
    result_to_c_json(decoded.and_then(|d| handle.edit(|e| e.install_decoded(d))))
}

/// Re-runs vocal pitch/note/key/BPM analysis on the currently loaded audio buffer
/// and returns a JSON `CantoAnalysisResult` string.
/// The caller must free the returned string with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_analyze(ptr: *mut CantoHandle) -> *mut c_char {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return result_to_c_json(Err("Null engine pointer".to_string()));
    };
    result_to_c_json(handle.edit(|e| e.analyze_loaded_audio()))
}

/// Starts loading `path_ptr` into the engine on a worker thread (see
/// `load_job`). `cached_ptr` is an earlier analysis JSON of the same file, or
/// null. Poll with `canto_load_job_*`; free with `canto_load_job_free`.
///
/// # Safety
/// `ptr` must be a valid handle; `path_ptr` a valid null-terminated UTF-8
/// string; `cached_ptr` one too, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_load_job_start(
    ptr: *mut CantoHandle,
    path_ptr: *const c_char,
    cached_ptr: *const c_char,
) -> *mut LoadJob {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return std::ptr::null_mut();
    };
    if path_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(path) = unsafe { CStr::from_ptr(path_ptr) }.to_str() else {
        return std::ptr::null_mut();
    };
    let cached = if cached_ptr.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(cached_ptr) }
            .to_str()
            .ok()
            .map(str::to_owned)
    };
    let kind = JobKind::File {
        path: path.into(),
        cached,
    };
    Box::into_raw(Box::new(LoadJob::start(Arc::clone(&handle.shared), kind)))
}

/// Starts analysing the engine's loaded audio again on a worker thread.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_reanalyze_job_start(ptr: *mut CantoHandle) -> *mut LoadJob {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(LoadJob::start(
        Arc::clone(&handle.shared),
        JobKind::Reanalyze,
    )))
}

/// 0 decoding, 1 analysing, 2 installing, 3 done.
///
/// # Safety
/// `job` must come from `canto_*_job_start` and not be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_load_job_stage(job: *const LoadJob) -> u32 {
    unsafe { job.as_ref() }.map_or(STAGE_DONE, |j| j.state.stage.load(Ordering::Acquire))
}

/// Analysis progress, 0..=1.
///
/// # Safety
/// `job` must come from `canto_*_job_start` and not be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_load_job_progress(job: *const LoadJob) -> f32 {
    unsafe { job.as_ref() }.map_or(0.0, |j| {
        j.state.progress.permille.load(Ordering::Relaxed) as f32 / 1000.0
    })
}

/// True if the cached analysis was adopted (no analysis ran).
///
/// # Safety
/// `job` must come from `canto_*_job_start` and not be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_load_job_used_cache(job: *const LoadJob) -> bool {
    unsafe { job.as_ref() }.is_some_and(|j| j.state.used_cache.load(Ordering::Relaxed))
}

/// Asks the worker to stop; the result then reports "Cancelled".
///
/// # Safety
/// `job` must come from `canto_*_job_start` and not be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_load_job_cancel(job: *const LoadJob) {
    if let Some(j) = unsafe { job.as_ref() } {
        j.cancel();
    }
}

/// The result JSON (`CantoAnalysisResult`) once done, else null. Returned
/// once; free it with `canto_free_string`.
///
/// # Safety
/// `job` must come from `canto_*_job_start` and not be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_load_job_take_result(job: *const LoadJob) -> *mut c_char {
    match unsafe { job.as_ref() }.and_then(|j| j.state.take_result()) {
        Some(res) => result_to_c_json(res),
        None => std::ptr::null_mut(),
    }
}

/// Frees the job, cancelling it if still running (the worker finishes on
/// its own and keeps the engine state alive until then).
///
/// # Safety
/// `job` must come from `canto_*_job_start`, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_load_job_free(job: *mut LoadJob) {
    if !job.is_null() {
        unsafe { drop(Box::from_raw(job)) };
    }
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_transport(
    ptr: *mut CantoHandle,
    playing: bool,
    loop_enabled: bool,
    bypass: bool,
) {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    handle.edit(|engine| {
        engine.set_playing(playing);
        engine.loop_enabled = loop_enabled;
        engine.bypass = bypass;
    });
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_is_playing(ptr: *const CantoHandle) -> bool {
    unsafe { ptr.as_ref() }.is_some_and(|h| h.shared.is_playing())
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_seek(ptr: *mut CantoHandle, time_sec: f64) {
    if let Some(handle) = unsafe { ptr.as_ref() } {
        handle.edit(|e| e.seek(time_sec));
    }
}

/// Lock-free read of the playhead published by the last rendered block.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_playhead(ptr: *const CantoHandle) -> f64 {
    unsafe { ptr.as_ref() }.map_or(0.0, |h| h.shared.playhead())
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_duration(ptr: *const CantoHandle) -> f64 {
    unsafe { ptr.as_ref() }.map_or(0.0, |h| h.engine().duration_sec)
}

/// Lock-free read of the output meter level.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_level(ptr: *const CantoHandle) -> f32 {
    unsafe { ptr.as_ref() }.map_or(0.0, |h| h.shared.level())
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_key_mode(ptr: *mut CantoHandle, key: u8, mode_id: u8) {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    let scale = match mode_id {
        0 => Scale::Minor,
        1 => Scale::Major,
        2 => Scale::MinorPentatonic,
        _ => Scale::Chromatic,
    };
    handle.edit(|e| e.set_key_and_scale(key, scale));
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_correct(ptr: *mut CantoHandle, amount: f32) {
    if let Some(handle) = unsafe { ptr.as_ref() } {
        handle.edit(|e| e.set_correct_amount(amount));
    }
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_update_note(
    ptr: *mut CantoHandle,
    index: u32,
    pitch: f32,
    start: f32,
    dur: f32,
    modulation: f32,
    formant: f32,
    gain_db: f32,
    pinned: bool,
    muted: bool,
    orn_style: u32,
    orn_depth: f32,
    orn_speed: f32,
    accent: f32,
) {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    handle.edit(|e| {
        let i = index as usize;
        e.update_note(
            i, pitch, start, dur, modulation, formant, gain_db, pinned, muted,
        );
        e.set_expression(i, orn_style.min(255) as u8, orn_depth, orn_speed, accent);
    });
}

/// Updates notes `0..count` in one lock from a packed array of
/// `NOTE_STRIDE` floats per note: pitch, start, dur, modulation, formant,
/// gain_db, pinned, muted (non-zero = true), orn_style, orn_depth,
/// orn_speed, accent.
///
/// # Safety
/// `ptr` must be a valid handle and `data` must point to `count * NOTE_STRIDE` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_sync_notes(
    ptr: *mut CantoHandle,
    data: *const f32,
    count: u32,
) {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    if data.is_null() || count == 0 {
        return;
    }
    let values = unsafe { slice::from_raw_parts(data, count as usize * NOTE_STRIDE) };
    handle.edit(|e| {
        for (i, n) in values.chunks_exact(NOTE_STRIDE).enumerate() {
            e.update_note(
                i,
                n[0],
                n[1],
                n[2],
                n[3],
                n[4],
                n[5],
                n[6] != 0.0,
                n[7] != 0.0,
            );
            e.set_expression(i, n[8].clamp(0.0, 255.0) as u8, n[9], n[10], n[11]);
        }
    });
}

/// Replaces the whole note list from JSON (an array of notes as in the
/// analysis result). Used after cutting or joining notes and on undo. Returns
/// false if the JSON doesn't parse; the engine is then left unchanged.
///
/// # Safety
/// `ptr` must be a valid handle and `json_ptr` a null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_notes_json(
    ptr: *mut CantoHandle,
    json_ptr: *const c_char,
) -> bool {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return false;
    };
    if json_ptr.is_null() {
        return false;
    }
    let Ok(json) = unsafe { CStr::from_ptr(json_ptr) }.to_str() else {
        return false;
    };
    // Parse before taking the lock: the audio thread only waits for the swap.
    match serde_json::from_str::<Vec<crate::engine::CantoNote>>(json) {
        Ok(notes) => {
            handle.edit(|e| e.set_notes(notes));
            true
        }
        Err(_) => false,
    }
}

/// Note preview: `index >= 0` starts previewing note `index` from its start
/// (held: loops); `index < 0` stops it with a short fade.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_audition_note(ptr: *mut CantoHandle, index: i32) {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    handle.edit(|e| {
        if index < 0 {
            e.stop_audition();
        } else {
            e.trigger_note_audition(index as usize);
        }
    });
}

/// Finger lifted: the preview finishes the note, then stops.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_audition_release(ptr: *mut CantoHandle) {
    if let Some(handle) = unsafe { ptr.as_ref() } {
        handle.edit(|e| e.release_audition());
    }
}

/// Lock-free: whether a note preview is still sounding.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_is_auditioning(ptr: *const CantoHandle) -> bool {
    unsafe { ptr.as_ref() }.is_some_and(|h| h.shared.is_auditioning())
}

/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_set_direct_voice(
    ptr: *mut CantoHandle,
    hz: f32,
    cutoff: f32,
    gain: f32,
) {
    if let Some(handle) = unsafe { ptr.as_ref() } {
        handle.edit(|e| e.set_direct_voice(hz, cutoff, gain));
    }
}

/// Pull-mode render, used when no native output stream is running (tests,
/// and the AudioTrack fallback on devices without AAudio).
///
/// # Safety
/// `ptr` must be a valid handle, and `out_ptr` must point to at least `frames * 2` f32 values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_render_f32(
    ptr: *mut CantoHandle,
    out_ptr: *mut f32,
    frames: usize,
) -> f32 {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return 0.0;
    };
    if out_ptr.is_null() || frames == 0 {
        return 0.0;
    }
    let out = unsafe { slice::from_raw_parts_mut(out_ptr, frames * 2) };
    handle.edit(|e| e.render_stereo_f32(out))
}

/// # Safety
/// `ptr` must be a valid handle, and `out_ptr` must point to at least `frames * 2` i16 values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_render_i16(
    ptr: *mut CantoHandle,
    out_ptr: *mut i16,
    frames: usize,
) -> f32 {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return 0.0;
    };
    if out_ptr.is_null() || frames == 0 {
        return 0.0;
    }
    let out = unsafe { slice::from_raw_parts_mut(out_ptr, frames * 2) };
    handle.edit(|e| e.render_stereo_i16(out))
}

/// Opens (if needed) and starts the native output stream. The engine is then
/// rendered on the audio thread; the UI must not call the render functions.
/// Reopens the stream after a device disconnect. Returns `false` when native
/// output is unavailable (non-Android, or no AAudio).
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_output_start(ptr: *mut CantoHandle) -> bool {
    let Some(handle) = (unsafe { ptr.as_mut() }) else {
        return false;
    };
    #[cfg(target_os = "android")]
    {
        if handle.output.as_ref().is_some_and(|o| o.is_disconnected()) {
            handle.output = None;
        }
        if handle.output.is_none() {
            match crate::output::Output::open(handle.shared.clone()) {
                Ok(output) => handle.output = Some(output),
                Err(_) => return false,
            }
        }
        handle.output.as_ref().is_some_and(|o| o.start())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = handle;
        false
    }
}

/// Pauses the native output stream (keeps it open for a fast restart).
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_output_pause(ptr: *mut CantoHandle) {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    #[cfg(target_os = "android")]
    if let Some(output) = &handle.output {
        output.pause();
    }
    #[cfg(not(target_os = "android"))]
    let _ = handle;
}

/// Glitch counter for diagnostics: device underruns plus callbacks that found
/// the engine locked by the UI thread.
///
/// # Safety
/// `ptr` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_output_glitches(ptr: *const CantoHandle) -> u32 {
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return 0;
    };
    #[cfg(target_os = "android")]
    let xruns = handle
        .output
        .as_ref()
        .map_or(0, |o| o.xrun_count().max(0) as u32);
    #[cfg(not(target_os = "android"))]
    let xruns = 0;
    xruns + handle.shared.lock_misses()
}

/// Renders the clip with all edits and writes a 16-bit mono WAV to `path_ptr`.
/// Returns null on success, or an error message the caller must free with
/// `canto_free_string`.
///
/// Safe to call from a background thread: the engine is cloned under its lock
/// (a quick copy) and rendered without holding it, so playback and editing
/// carry on while the export runs.
///
/// # Safety
/// `ptr` must be a valid handle and `path_ptr` a null-terminated UTF-8 path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_engine_export_wav(
    ptr: *const CantoHandle,
    path_ptr: *const c_char,
) -> *mut c_char {
    let error = |msg: String| {
        CString::new(msg)
            .unwrap_or_else(|_| CString::new("export failed").unwrap())
            .into_raw()
    };
    let Some(handle) = (unsafe { ptr.as_ref() }) else {
        return error("Null engine pointer".into());
    };
    if path_ptr.is_null() {
        return error("Null export path".into());
    }
    let path = match unsafe { CStr::from_ptr(path_ptr) }.to_str() {
        Ok(p) => p.to_owned(),
        Err(e) => return error(format!("Invalid UTF-8 path: {e}")),
    };
    let snapshot = handle.engine().clone();
    if !snapshot.has_audio() {
        return error("No audio loaded".into());
    }
    let samples = snapshot.render_offline();
    let wav = crate::audio_io::encode_mono_wav_i16(&samples, snapshot.sample_rate() as u32);
    match std::fs::write(&path, wav) {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => error(format!("Could not write {path}: {e}")),
    }
}

// ---- Live recording ---------------------------------------------------------
//
// A recorder is used from the UI thread only; the device callback reaches it
// through its lock-free ring.

fn c_error(msg: String) -> *mut c_char {
    CString::new(msg)
        .unwrap_or_else(|_| CString::new("error").unwrap())
        .into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn canto_rec_new() -> *mut Recorder {
    Box::into_raw(Box::new(Recorder::new()))
}

/// # Safety
/// `ptr` must come from `canto_rec_new` (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_rec_free(ptr: *mut Recorder) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr) }); // closes the mic stream
    }
}

/// Opens the microphone and starts a take. Null on success, otherwise an
/// error message to free with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid recorder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_rec_start(ptr: *mut Recorder) -> *mut c_char {
    let Some(rec) = (unsafe { ptr.as_mut() }) else {
        return c_error("Null recorder".into());
    };
    match rec.start() {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => c_error(e),
    }
}

/// Drains new audio and writes `[seconds, level, pitch_hz, clipped]` to
/// `out` (4 floats). Returns whether the take is still recording.
///
/// # Safety
/// `ptr` must be a valid recorder and `out` must point to 4 floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_rec_poll(ptr: *mut Recorder, out: *mut f32) -> bool {
    let Some(rec) = (unsafe { ptr.as_mut() }) else {
        return false;
    };
    let s = rec.poll();
    if !out.is_null() {
        let out = unsafe { slice::from_raw_parts_mut(out, 4) };
        out[0] = s.seconds as f32;
        out[1] = s.level;
        out[2] = s.pitch_hz;
        out[3] = if s.clipped { 1.0 } else { 0.0 };
    }
    s.recording
}

/// # Safety
/// `ptr` must be a valid recorder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_rec_stop(ptr: *mut Recorder) {
    if let Some(rec) = unsafe { ptr.as_mut() } {
        rec.stop();
    }
}

/// # Safety
/// `ptr` must be a valid recorder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_rec_discard(ptr: *mut Recorder) {
    if let Some(rec) = unsafe { ptr.as_mut() } {
        rec.discard();
    }
}

/// Saves the take as 48 kHz 16-bit mono WAV. Null on success, otherwise an
/// error message to free with `canto_free_string`.
///
/// # Safety
/// `ptr` must be a valid recorder and `path_ptr` a null-terminated UTF-8 path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_rec_save(
    ptr: *mut Recorder,
    path_ptr: *const c_char,
) -> *mut c_char {
    let Some(rec) = (unsafe { ptr.as_mut() }) else {
        return c_error("Null recorder".into());
    };
    if path_ptr.is_null() {
        return c_error("Null path".into());
    }
    let path = match unsafe { CStr::from_ptr(path_ptr) }.to_str() {
        Ok(p) => p,
        Err(e) => return c_error(format!("Invalid UTF-8 path: {e}")),
    };
    rec.stop();
    match rec.save_wav(path, 48_000) {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => c_error(e),
    }
}

/// Test hook: pushes samples as if they came from the microphone and starts
/// a take at `sample_rate` if none is running. Lets the full Dart → Rust
/// recording path be exercised without a device.
///
/// # Safety
/// `ptr` must be a valid recorder and `data` must point to `len` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn canto_rec_feed_for_test(
    ptr: *mut Recorder,
    data: *const f32,
    len: usize,
    sample_rate: u32,
) {
    let Some(rec) = (unsafe { ptr.as_mut() }) else {
        return;
    };
    if !rec.poll().recording {
        rec.begin(sample_rate);
    }
    if !data.is_null() && len > 0 {
        rec.feed(unsafe { slice::from_raw_parts(data, len) });
    }
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
