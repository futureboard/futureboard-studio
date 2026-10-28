//! C-ABI FFI bridge for Flutter (`dart:ffi`) to drive `StudioMobileEngine`.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;

use serde::Deserialize;

use crate::engine::{StudioMobileEngine, TrackKind};

pub struct FbEngineHandle {
    inner: Mutex<StudioMobileEngine>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum EngineAction {
    LoadDemo,
    Undo,
    Redo,
    SetLoop {
        enabled: bool,
        start_beats: f64,
        end_beats: f64,
    },
    SetMetronome {
        enabled: bool,
    },
    SetMasterBinaural {
        enabled: bool,
        reverb_mix: Option<f32>,
    },
    SelectTrack {
        track_id: String,
        clip_id: Option<String>,
    },
    InspectPlugin {
        track_id: String,
        slot_index: Option<usize>,
    },
    AddTrack {
        name: String,
        kind: TrackKind,
        instrument_id: Option<String>,
    },
    DeleteTrack {
        track_id: String,
    },
    SetTrackSpatial {
        track_id: String,
        x: f32,
        y: f32,
        z: f32,
        binaural: bool,
    },
    SetTrackSend {
        track_id: String,
        bus_id: String,
        level_db: f32,
        enabled: bool,
    },
    SetTrackInstrument {
        track_id: String,
        plugin_id: String,
    },
    AddTrackInsert {
        track_id: String,
        plugin_id: String,
    },
    RemoveTrackInsert {
        track_id: String,
        slot_index: usize,
    },
    SetPluginEnabled {
        track_id: String,
        slot_index: Option<usize>,
        enabled: bool,
    },
    SetPluginParam {
        track_id: String,
        slot_index: Option<usize>,
        param_id: String,
        value: f32,
    },
    ApplyPluginPreset {
        track_id: String,
        slot_index: Option<usize>,
        preset_name: String,
    },
    AddMidiClip {
        track_id: String,
        start_beat: f64,
        length_beats: f64,
    },
    ImportAudioFile {
        track_id: String,
        file_path: String,
        start_beat: f64,
    },
    UpdateClipBounds {
        track_id: String,
        clip_id: String,
        start_beat: f64,
        length_beats: f64,
        gain_db: Option<f32>,
        pitch_semitones: Option<f32>,
    },
    SplitClip {
        track_id: String,
        clip_id: String,
        split_beat: f64,
    },
    DeleteClip {
        track_id: String,
        clip_id: String,
    },
    UpsertMidiNote {
        track_id: String,
        clip_id: String,
        note_id: Option<String>,
        pitch: u8,
        start_beat: f64,
        duration_beats: f64,
        velocity: u8,
    },
    DeleteMidiNote {
        track_id: String,
        clip_id: String,
        note_id: String,
    },
    QuantizeMidiClip {
        track_id: String,
        clip_id: String,
        grid_beats: f64,
    },
    LoadProjectJson {
        json: String,
    },
    ExportWav {
        output_path: String,
        total_beats: f64,
    },
}

fn to_c_string(s: String) -> *mut c_char {
    CString::new(s)
        .unwrap_or_else(|_| CString::new("{}").unwrap())
        .into_raw()
}

fn from_c_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok().map(|s| s.to_string()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn fb_engine_new(sample_rate: u32) -> *mut FbEngineHandle {
    let engine = StudioMobileEngine::new(sample_rate);
    Box::into_raw(Box::new(FbEngineHandle {
        inner: Mutex::new(engine),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_free(handle: *mut FbEngineHandle) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_string_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            drop(CString::from_raw(ptr));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_render_f32(
    handle: *mut FbEngineHandle,
    out_interleaved: *mut f32,
    frames: u32,
) {
    if handle.is_null() || out_interleaved.is_null() || frames == 0 {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(out_interleaved, (frames as usize) * 2) };
    let Ok(mut engine) = (unsafe { (*handle).inner.lock() }) else {
        slice.fill(0.0);
        return;
    };
    engine.render_interleaved_stereo(slice, frames as usize);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_push_mic_input(
    handle: *mut FbEngineHandle,
    in_samples: *const f32,
    sample_count: u32,
    channels: u32,
) {
    if handle.is_null() || in_samples.is_null() || sample_count == 0 {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(in_samples, sample_count as usize) };
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.push_mic_input_interleaved(slice, channels.max(1) as usize);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_set_playing(handle: *mut FbEngineHandle, playing: bool) {
    if handle.is_null() {
        return;
    }
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.set_playing(playing);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_set_recording(handle: *mut FbEngineHandle, recording: bool) {
    if handle.is_null() {
        return;
    }
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.set_recording(recording);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_seek_beats(handle: *mut FbEngineHandle, beats: f64) {
    if handle.is_null() {
        return;
    }
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.seek_beats(beats);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_set_bpm(handle: *mut FbEngineHandle, bpm: f64) {
    if handle.is_null() {
        return;
    }
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.set_bpm(bpm);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_trigger_midi(
    handle: *mut FbEngineHandle,
    track_id_ptr: *const c_char,
    note: u8,
    velocity: u8,
    is_note_on: bool,
) {
    if handle.is_null() {
        return;
    }
    let track_id = from_c_str(track_id_ptr).unwrap_or_default();
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.trigger_live_midi(&track_id, note, velocity, is_note_on);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_set_track_mixer(
    handle: *mut FbEngineHandle,
    track_id_ptr: *const c_char,
    volume_db: f32,
    pan: f32,
    muted: i32,
    soloed: i32,
    armed: i32,
    input_monitoring: i32,
) {
    if handle.is_null() {
        return;
    }
    let Some(track_id) = from_c_str(track_id_ptr) else {
        return;
    };
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.set_track_mixer(
            &track_id,
            if volume_db.is_finite() {
                Some(volume_db)
            } else {
                None
            },
            if pan.is_finite() { Some(pan) } else { None },
            if muted >= 0 { Some(muted != 0) } else { None },
            if soloed >= 0 { Some(soloed != 0) } else { None },
            if armed >= 0 { Some(armed != 0) } else { None },
            if input_monitoring >= 0 {
                Some(input_monitoring != 0)
            } else {
                None
            },
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_set_plugin_param(
    handle: *mut FbEngineHandle,
    track_id_ptr: *const c_char,
    slot_index: i32,
    param_id_ptr: *const c_char,
    value: f32,
) {
    if handle.is_null() {
        return;
    }
    let (Some(track_id), Some(param_id)) = (from_c_str(track_id_ptr), from_c_str(param_id_ptr))
    else {
        return;
    };
    let slot = if slot_index < 0 {
        None
    } else {
        Some(slot_index as usize)
    };
    if let Ok(mut engine) = unsafe { (*handle).inner.lock() } {
        engine.set_plugin_param(&track_id, slot, &param_id, value);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_get_snapshot_json(handle: *mut FbEngineHandle) -> *mut c_char {
    if handle.is_null() {
        return to_c_string("{}".to_string());
    }
    let Ok(mut engine) = (unsafe { (*handle).inner.lock() }) else {
        return to_c_string("{}".to_string());
    };
    let snap = engine.snapshot();
    to_c_string(serde_json::to_string(&snap).unwrap_or_else(|_| "{}".to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_get_telemetry_json(handle: *mut FbEngineHandle) -> *mut c_char {
    if handle.is_null() {
        return to_c_string("{}".to_string());
    }
    let Ok(engine) = (unsafe { (*handle).inner.lock() }) else {
        return to_c_string("{}".to_string());
    };
    let tel = engine.telemetry();
    to_c_string(serde_json::to_string(&tel).unwrap_or_else(|_| "{}".to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_get_catalog_json(handle: *mut FbEngineHandle) -> *mut c_char {
    if handle.is_null() {
        return to_c_string("[]".to_string());
    }
    let Ok(engine) = (unsafe { (*handle).inner.lock() }) else {
        return to_c_string("[]".to_string());
    };
    let cat = engine.catalog();
    to_c_string(serde_json::to_string(&cat).unwrap_or_else(|_| "[]".to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_engine_dispatch_action_json(
    handle: *mut FbEngineHandle,
    action_json_ptr: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return to_c_string(r#"{"ok":false,"error":"null handle"}"#.to_string());
    }
    let Some(action_str) = from_c_str(action_json_ptr) else {
        return to_c_string(r#"{"ok":false,"error":"null action string"}"#.to_string());
    };
    let action: EngineAction = match serde_json::from_str(&action_str) {
        Ok(a) => a,
        Err(e) => {
            return to_c_string(
                serde_json::json!({ "ok": false, "error": format!("parse error: {e}") })
                    .to_string(),
            );
        }
    };
    let Ok(mut engine) = (unsafe { (*handle).inner.lock() }) else {
        return to_c_string(r#"{"ok":false,"error":"engine lock poisoned"}"#.to_string());
    };

    let result = match action {
        EngineAction::LoadDemo => {
            engine.load_demo_project();
            serde_json::json!({ "ok": true })
        }
        EngineAction::Undo => serde_json::json!({ "ok": engine.undo() }),
        EngineAction::Redo => serde_json::json!({ "ok": engine.redo() }),
        EngineAction::SetLoop {
            enabled,
            start_beats,
            end_beats,
        } => {
            engine.set_loop(enabled, start_beats, end_beats);
            serde_json::json!({ "ok": true })
        }
        EngineAction::SetMetronome { enabled } => {
            engine.metronome_enabled = enabled;
            serde_json::json!({ "ok": true })
        }
        EngineAction::SetMasterBinaural {
            enabled,
            reverb_mix,
        } => {
            engine.master_binaural_enabled = enabled;
            if let Some(rv) = reverb_mix {
                engine.room_settings.reflections = rv.clamp(0.0, 1.0);
            }
            serde_json::json!({ "ok": true })
        }
        EngineAction::SelectTrack { track_id, clip_id } => {
            engine.selected_track_id = track_id;
            if clip_id.is_some() {
                engine.selected_clip_id = clip_id;
            }
            serde_json::json!({ "ok": true })
        }
        EngineAction::InspectPlugin {
            track_id,
            slot_index,
        } => {
            engine.inspected_plugin_target = Some((track_id, slot_index));
            serde_json::json!({ "ok": true })
        }
        EngineAction::AddTrack {
            name,
            kind,
            instrument_id,
        } => {
            let id = engine.add_track(&name, kind, instrument_id.as_deref());
            serde_json::json!({ "ok": true, "trackId": id })
        }
        EngineAction::DeleteTrack { track_id } => {
            serde_json::json!({ "ok": engine.delete_track(&track_id) })
        }
        EngineAction::SetTrackSpatial {
            track_id,
            x,
            y,
            z,
            binaural,
        } => {
            let ok = engine.set_track_spatial(&track_id, x, y, z, binaural);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::SetTrackSend {
            track_id,
            bus_id,
            level_db,
            enabled,
        } => {
            let ok = engine.set_track_send(&track_id, &bus_id, level_db, enabled);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::SetTrackInstrument {
            track_id,
            plugin_id,
        } => {
            let ok = engine.set_track_instrument(&track_id, &plugin_id);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::AddTrackInsert {
            track_id,
            plugin_id,
        } => match engine.add_track_insert(&track_id, &plugin_id) {
            Some(idx) => serde_json::json!({ "ok": true, "slotIndex": idx }),
            None => serde_json::json!({ "ok": false, "error": "failed to add insert" }),
        },
        EngineAction::RemoveTrackInsert {
            track_id,
            slot_index,
        } => {
            let ok = engine.remove_track_insert(&track_id, slot_index);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::SetPluginEnabled {
            track_id,
            slot_index,
            enabled,
        } => {
            let ok = engine.set_plugin_enabled(&track_id, slot_index, enabled);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::SetPluginParam {
            track_id,
            slot_index,
            param_id,
            value,
        } => {
            let ok = engine.set_plugin_param(&track_id, slot_index, &param_id, value);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::ApplyPluginPreset {
            track_id,
            slot_index,
            preset_name,
        } => {
            let ok = engine.apply_plugin_preset(&track_id, slot_index, &preset_name);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::AddMidiClip {
            track_id,
            start_beat,
            length_beats,
        } => match engine.add_midi_clip(&track_id, start_beat, length_beats) {
            Some(id) => serde_json::json!({ "ok": true, "clipId": id }),
            None => serde_json::json!({ "ok": false }),
        },
        EngineAction::ImportAudioFile {
            track_id,
            file_path,
            start_beat,
        } => match engine.import_audio_file_to_track(&track_id, &file_path, start_beat) {
            Ok(id) => serde_json::json!({ "ok": true, "clipId": id }),
            Err(e) => serde_json::json!({ "ok": false, "error": e }),
        },
        EngineAction::UpdateClipBounds {
            track_id,
            clip_id,
            start_beat,
            length_beats,
            gain_db,
            pitch_semitones,
        } => {
            let ok = engine.update_clip_bounds(
                &track_id,
                &clip_id,
                start_beat,
                length_beats,
                gain_db,
                pitch_semitones,
            );
            serde_json::json!({ "ok": ok })
        }
        EngineAction::SplitClip {
            track_id,
            clip_id,
            split_beat,
        } => {
            let ok = engine.split_clip(&track_id, &clip_id, split_beat);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::DeleteClip { track_id, clip_id } => {
            let ok = engine.delete_clip(&track_id, &clip_id);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::UpsertMidiNote {
            track_id,
            clip_id,
            note_id,
            pitch,
            start_beat,
            duration_beats,
            velocity,
        } => match engine.upsert_midi_note(
            &track_id,
            &clip_id,
            note_id.as_deref(),
            pitch,
            start_beat,
            duration_beats,
            velocity,
        ) {
            Some(id) => serde_json::json!({ "ok": true, "noteId": id }),
            None => serde_json::json!({ "ok": false }),
        },
        EngineAction::DeleteMidiNote {
            track_id,
            clip_id,
            note_id,
        } => {
            let ok = engine.delete_midi_note(&track_id, &clip_id, &note_id);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::QuantizeMidiClip {
            track_id,
            clip_id,
            grid_beats,
        } => {
            let ok = engine.quantize_midi_clip(&track_id, &clip_id, grid_beats);
            serde_json::json!({ "ok": ok })
        }
        EngineAction::LoadProjectJson { json } => match engine.load_project_json(&json) {
            Ok(()) => serde_json::json!({ "ok": true }),
            Err(e) => serde_json::json!({ "ok": false, "error": e }),
        },
        EngineAction::ExportWav {
            output_path,
            total_beats,
        } => match engine.export_wav(&output_path, total_beats) {
            Ok(()) => serde_json::json!({ "ok": true, "path": output_path }),
            Err(e) => serde_json::json!({ "ok": false, "error": e }),
        },
    };

    to_c_string(result.to_string())
}
