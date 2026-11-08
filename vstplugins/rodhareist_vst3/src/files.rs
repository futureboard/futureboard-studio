//! NAM captures and impulse responses: the plug-in's side of what Studio's
//! plug-in host does for a Rodhareist insert.
//!
//! The editor picks a file and keeps a copy in Rodhareist's folder
//! (`Documents/Futureboard Studio/Rodhareist/{NAMs,IRs}`, shared with
//! Studio), then hands the bytes here. They are built on a file-load thread
//! through the DSP's own loaders and adopted lock-free at
//! the next `Dsp::begin_block`. As in Studio, every answer is recorded for the
//! editor, and the saved file name changes only when the load succeeded.

use std::sync::PoisonError;

use crate::components::builtin_plugin_editor::{
    self as mirror, FileLoad, RodhareistFile, RODHAREIST_FILES_FOLDER,
};
use crate::components::builtin_plugin_files::plugin_files_root;
use crate::params::RodhareistParams;
use crate::shared::{ControlSide, Shared};

/// Work the editor hands to a file-load thread.
pub enum Task {
    LoadCapture { name: String, json: String },
    LoadImpulse { name: String, bytes: Vec<u8> },
}

/// Runs one [`Task`]. File-load thread only: parsing and building allocate
/// and can take a while.
pub fn run(task: Task, shared: &Shared, params: &RodhareistParams, insert_id: &str) {
    let mut control = shared
        .control
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let capture = matches!(task, Task::LoadCapture { .. });
    match task {
        Task::LoadCapture { name, json } => match control.as_mut() {
            Some(control) => load_capture(control, params, insert_id, &name, &json),
            // Not processing yet: `restore` loads it on initialize.
            None => params.set_nam_file(Some(name)),
        },
        Task::LoadImpulse { name, bytes } => match control.as_mut() {
            Some(control) => load_impulse(control, params, insert_id, &name, &bytes),
            None => params.set_ir_file(Some(name)),
        },
    }
    drop(control);
    shared.answered(capture);
}

/// Brings a running `Dsp` in line with the files the state names, reading
/// them from Rodhareist's folder. Called from `initialize`, after a state load
/// or a new `Dsp`. A file that is gone is recorded as a failed load, so the
/// editor says which file is missing.
pub fn restore(shared: &Shared, params: &RodhareistParams, insert_id: &str) {
    let mut control = shared
        .control
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(control) = control.as_mut() else {
        return;
    };
    let root = plugin_files_root(RODHAREIST_FILES_FOLDER);
    if let Some(name) = params.nam_file() {
        if control.nam_loaded.as_deref() != Some(name.as_str()) {
            let path = root
                .join(RodhareistFile::Capture.kind().dir_name())
                .join(&name);
            match std::fs::read_to_string(&path) {
                Ok(json) => load_capture(control, params, insert_id, &name, &json),
                Err(error) => failed(insert_id, RodhareistFile::Capture, &name, error),
            }
        }
    }
    if let Some(name) = params.ir_file() {
        if control.ir_loaded.as_deref() != Some(name.as_str()) {
            let path = root
                .join(RodhareistFile::Impulse.kind().dir_name())
                .join(&name);
            match std::fs::read(&path) {
                Ok(bytes) => load_impulse(control, params, insert_id, &name, &bytes),
                Err(error) => failed(insert_id, RodhareistFile::Impulse, &name, error),
            }
        }
    }
    shared.answered(true);
    shared.answered(false);
}

fn load_capture(
    control: &mut ControlSide,
    params: &RodhareistParams,
    insert_id: &str,
    name: &str,
    json: &str,
) {
    // A mono model on both sides, not marked full rig: what Studio's editor
    // asks for. The DSP upgrades a capture that includes a cab itself.
    let result = control
        .nam
        .load_json(json, name.to_string(), control.sample_rate, false, false);
    let error = result.as_ref().err().map(|e| e.to_string());
    if error.is_none() {
        control.nam_loaded = Some(name.to_string());
        params.set_nam_file(Some(name.to_string()));
    }
    mirror::record_file_load(
        insert_id,
        RodhareistFile::Capture,
        FileLoad {
            name: name.to_string(),
            error,
            frames: 0,
        },
    );
}

fn load_impulse(
    control: &mut ControlSide,
    params: &RodhareistParams,
    insert_id: &str,
    name: &str,
    bytes: &[u8],
) {
    let result = control
        .ir
        .load_wav(bytes, name.to_string(), control.sample_rate);
    let (error, frames) = match &result {
        Ok(info) => (None, info.frames as u64),
        Err(error) => (Some(error.to_string()), 0),
    };
    if error.is_none() {
        control.ir_loaded = Some(name.to_string());
        params.set_ir_file(Some(name.to_string()));
    }
    mirror::record_file_load(
        insert_id,
        RodhareistFile::Impulse,
        FileLoad {
            name: name.to_string(),
            error,
            frames,
        },
    );
}

fn failed(insert_id: &str, file: RodhareistFile, name: &str, error: std::io::Error) {
    mirror::record_file_load(
        insert_id,
        file,
        FileLoad {
            name: name.to_string(),
            error: Some(format!("could not read it: {error}")),
            frames: 0,
        },
    );
}
