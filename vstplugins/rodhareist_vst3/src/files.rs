//! NAM captures and impulse responses: the plug-in's side of what Studio's
//! plug-in host does for a Rodhareist insert.
//!
//! The editor picks a file and keeps a copy in Rodhareist's folder
//! (`Documents/Futureboard Studio/Rodhareist/{NAMs,IRs}`, shared with
//! Studio), then hands the bytes here. They are built on a file-load thread
//! through the DSP's own loaders and adopted lock-free at
//! the next `Dsp::begin_block`. As in Studio, every answer is recorded for the
//! editor, and the saved file name changes only when the load succeeded.
//!
//! A host that restores state calls [`restore`] from `initialize`, on the
//! same lock the audio callback needs. Reading a file and building a model
//! stay on a file-load thread. This thread only decides what to load and,
//! when the state names nothing usable, asks the audio thread to drop what
//! it is playing.

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

use rodharerist::{IrLoadError, NamLoadError, PreparedIrRuntime, PreparedNamRuntime};

use crate::components::builtin_plugin_editor::{
    self as mirror, FileLoad, RodhareistFile, RODHAREIST_FILES_FOLDER,
};
use crate::components::builtin_plugin_files::{
    plugin_files_root, sanitize_file_name, BuiltinFileKind,
};
use crate::params::RodhareistParams;
use crate::shared::{ControlSide, Shared};

/// Work the editor hands to a file-load thread.
pub enum Task {
    LoadCapture { name: String, json: String },
    LoadImpulse { name: String, bytes: Vec<u8> },
}

/// One file a restore or an editor load is allowed to build. `generation`
/// and `epoch` are what was current when the load was asked for; a newer
/// request or a new `Dsp` makes the finished build drop itself.
struct Ticket {
    name: String,
    generation: u64,
    epoch: u64,
    sample_rate: f64,
}

/// Runs one [`Task`]. File-load thread only: parsing and building allocate
/// and can take a while. The control lock is taken only to publish the result.
pub fn run(task: Task, shared: &Shared, params: &RodhareistParams, insert_id: &str) {
    let capture = matches!(task, Task::LoadCapture { .. });
    let Some(ticket) = claim(shared, capture) else {
        // Not processing yet: remember the name. `restore` loads it once a
        // `Dsp` exists.
        match &task {
            Task::LoadCapture { name, .. } => params.set_nam_file(Some(name.clone())),
            Task::LoadImpulse { name, .. } => params.set_ir_file(Some(name.clone())),
        }
        shared.answered(capture);
        return;
    };
    match task {
        Task::LoadCapture { name, json } => {
            let built = rodharerist::prepare_nam_runtime(
                &json,
                name.clone(),
                ticket.sample_rate,
                false,
                false,
            );
            publish_capture(shared, params, insert_id, &name, built, &ticket);
        }
        Task::LoadImpulse { name, bytes } => {
            let built = rodharerist::prepare_ir_runtime(&bytes, name.clone(), ticket.sample_rate);
            publish_impulse(shared, params, insert_id, &name, built, &ticket);
        }
    }
    // A build a newer request replaced does not answer: that request does.
    if still_current(shared, capture, &ticket) {
        shared.answered(capture);
    }
}

/// Brings a running `Dsp` in line with the files the state names. Returns
/// before any file is read.
///
/// A name that is not a plain file inside the Rodhareist library — an
/// absolute path, a `..` segment, a network path — is rejected and reported
/// missing. The plug-in does not open it.
pub fn restore(shared: &Arc<Shared>, params: &Arc<RodhareistParams>, insert_id: &str) {
    let insert_id = insert_id.to_string();
    let nam_name = params.nam_file();
    let ir_name = params.ir_file();

    let mut discard_nam = None;
    let mut discard_ir = None;
    let (nam, ir) = {
        let mut guard = shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(control) = guard.as_mut() else {
            return;
        };
        let nam = plan_capture(shared, control, &mut discard_nam, nam_name, &insert_id);
        let ir = plan_impulse(shared, control, &mut discard_ir, ir_name, &insert_id);
        (nam, ir)
    };
    // File names live on the parameter mirror. Wake the editor now; a load
    // that is still building answers again when it finishes or fails.
    shared.answered(true);
    shared.answered(false);

    if nam.is_none() && ir.is_none() && discard_nam.is_none() && discard_ir.is_none() {
        return;
    }
    let shared = Arc::clone(shared);
    let params = Arc::clone(params);
    let spawned = std::thread::Builder::new()
        .name("rodhareist-file-load".to_string())
        .spawn(move || {
            // Dropping a model is the slow part of an unload. Not on the
            // thread that is holding the audio lock.
            drop(discard_nam);
            drop(discard_ir);
            if let Some(ticket) = nam {
                load_named_capture(&shared, &params, &insert_id, ticket);
            }
            if let Some(ticket) = ir {
                load_named_impulse(&shared, &params, &insert_id, ticket);
            }
        });
    if spawned.is_err() {
        nih_plug::nih_log!("rodhareist: could not start a file-load thread");
    }
}

/// A library path for `name`, or `None` when `name` could leave the folder.
fn library_file(kind: BuiltinFileKind, name: &str) -> Option<PathBuf> {
    let clean = sanitize_file_name(kind, name)?;
    Some(
        plugin_files_root(RODHAREIST_FILES_FOLDER)
            .join(kind.dir_name())
            .join(clean),
    )
}

fn claim(shared: &Shared, capture: bool) -> Option<Ticket> {
    let mut guard = shared
        .control
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let control = guard.as_mut()?;
    let generation = bump(if capture {
        &mut control.nam_generation
    } else {
        &mut control.ir_generation
    });
    Some(Ticket {
        name: String::new(),
        generation,
        epoch: shared.control_epoch(),
        sample_rate: control.sample_rate,
    })
}

fn bump(generation: &mut u64) -> u64 {
    *generation = generation.wrapping_add(1);
    *generation
}

fn still_current(shared: &Shared, capture: bool, ticket: &Ticket) -> bool {
    if shared.control_epoch() != ticket.epoch {
        return false;
    }
    let guard = shared
        .control
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(control) = guard.as_ref() else {
        return false;
    };
    let generation = if capture {
        control.nam_generation
    } else {
        control.ir_generation
    };
    generation == ticket.generation
}

fn plan_capture(
    shared: &Shared,
    control: &mut ControlSide,
    discard: &mut Option<Box<PreparedNamRuntime>>,
    name: Option<String>,
    insert_id: &str,
) -> Option<Ticket> {
    match name {
        None => {
            bump(&mut control.nam_generation);
            *discard = control.nam.unload();
            control.nam_loaded = None;
            None
        }
        Some(name) if library_file(BuiltinFileKind::Nams, &name).is_none() => {
            bump(&mut control.nam_generation);
            *discard = control.nam.unload();
            control.nam_loaded = None;
            rejected(insert_id, RodhareistFile::Capture, &name);
            None
        }
        Some(name) if control.nam_loaded.as_deref() == Some(name.as_str()) => None,
        Some(name) => Some(Ticket {
            generation: bump(&mut control.nam_generation),
            epoch: shared.control_epoch(),
            sample_rate: control.sample_rate,
            name,
        }),
    }
}

fn plan_impulse(
    shared: &Shared,
    control: &mut ControlSide,
    discard: &mut Option<Box<PreparedIrRuntime>>,
    name: Option<String>,
    insert_id: &str,
) -> Option<Ticket> {
    match name {
        None => {
            bump(&mut control.ir_generation);
            *discard = control.ir.unload();
            control.ir_loaded = None;
            None
        }
        Some(name) if library_file(BuiltinFileKind::Irs, &name).is_none() => {
            bump(&mut control.ir_generation);
            *discard = control.ir.unload();
            control.ir_loaded = None;
            rejected(insert_id, RodhareistFile::Impulse, &name);
            None
        }
        Some(name) if control.ir_loaded.as_deref() == Some(name.as_str()) => None,
        Some(name) => Some(Ticket {
            generation: bump(&mut control.ir_generation),
            epoch: shared.control_epoch(),
            sample_rate: control.sample_rate,
            name,
        }),
    }
}

fn load_named_capture(shared: &Shared, params: &RodhareistParams, insert_id: &str, ticket: Ticket) {
    let Some(path) = library_file(BuiltinFileKind::Nams, &ticket.name) else {
        return;
    };
    let built = match std::fs::read_to_string(&path) {
        Ok(json) => rodharerist::prepare_nam_runtime(
            &json,
            ticket.name.clone(),
            ticket.sample_rate,
            false,
            false,
        ),
        Err(error) => {
            if still_current(shared, true, &ticket) {
                failed(insert_id, RodhareistFile::Capture, &ticket.name, &error);
                shared.answered(true);
            }
            return;
        }
    };
    publish_capture(shared, params, insert_id, &ticket.name, built, &ticket);
    if still_current(shared, true, &ticket) {
        shared.answered(true);
    }
}

fn load_named_impulse(shared: &Shared, params: &RodhareistParams, insert_id: &str, ticket: Ticket) {
    let Some(path) = library_file(BuiltinFileKind::Irs, &ticket.name) else {
        return;
    };
    let built = match std::fs::read(&path) {
        Ok(bytes) => {
            rodharerist::prepare_ir_runtime(&bytes, ticket.name.clone(), ticket.sample_rate)
        }
        Err(error) => {
            if still_current(shared, false, &ticket) {
                failed(insert_id, RodhareistFile::Impulse, &ticket.name, &error);
                shared.answered(false);
            }
            return;
        }
    };
    publish_impulse(shared, params, insert_id, &ticket.name, built, &ticket);
    if still_current(shared, false, &ticket) {
        shared.answered(false);
    }
}

fn publish_capture(
    shared: &Shared,
    params: &RodhareistParams,
    insert_id: &str,
    name: &str,
    built: Result<PreparedNamRuntime, NamLoadError>,
    ticket: &Ticket,
) {
    let replaced = {
        let mut guard = shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(control) = guard.as_mut() else {
            return;
        };
        if !owns(shared, control.nam_generation, ticket) {
            return;
        }
        match built {
            Ok(runtime) => {
                let replaced = control.nam.submit(Box::new(runtime));
                control.nam_loaded = Some(name.to_string());
                params.set_nam_file(Some(name.to_string()));
                replaced
            }
            Err(error) => {
                let message = error.to_string();
                drop(guard);
                record_err(insert_id, RodhareistFile::Capture, name, message);
                return;
            }
        }
    };
    drop(replaced);
    record_ok(insert_id, RodhareistFile::Capture, name, 0);
}

fn publish_impulse(
    shared: &Shared,
    params: &RodhareistParams,
    insert_id: &str,
    name: &str,
    built: Result<PreparedIrRuntime, IrLoadError>,
    ticket: &Ticket,
) {
    let replaced = {
        let mut guard = shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(control) = guard.as_mut() else {
            return;
        };
        if !owns(shared, control.ir_generation, ticket) {
            return;
        }
        match built {
            Ok(runtime) => {
                let frames = runtime.info().frames;
                let replaced = control.ir.submit(Box::new(runtime));
                control.ir_loaded = Some(name.to_string());
                params.set_ir_file(Some(name.to_string()));
                (replaced, frames)
            }
            Err(error) => {
                let message = error.to_string();
                drop(guard);
                record_err(insert_id, RodhareistFile::Impulse, name, message);
                return;
            }
        }
    };
    let (replaced, frames) = replaced;
    drop(replaced);
    record_ok(insert_id, RodhareistFile::Impulse, name, frames as u64);
}

fn owns(shared: &Shared, generation: u64, ticket: &Ticket) -> bool {
    shared.control_epoch() == ticket.epoch && generation == ticket.generation
}

fn record_ok(insert_id: &str, file: RodhareistFile, name: &str, frames: u64) {
    mirror::record_file_load(
        insert_id,
        file,
        FileLoad {
            name: name.to_string(),
            error: None,
            frames,
        },
    );
}

fn record_err(insert_id: &str, file: RodhareistFile, name: &str, error: String) {
    mirror::record_file_load(
        insert_id,
        file,
        FileLoad {
            name: name.to_string(),
            error: Some(error),
            frames: 0,
        },
    );
}

fn rejected(insert_id: &str, file: RodhareistFile, name: &str) {
    record_err(
        insert_id,
        file,
        name,
        "that file name is not in the Rodhareist library".to_string(),
    );
}

fn failed(insert_id: &str, file: RodhareistFile, name: &str, error: &std::io::Error) {
    record_err(insert_id, file, name, format!("could not read it: {error}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_names_stay_inside_the_library() {
        assert!(library_file(BuiltinFileKind::Nams, "amp.nam").is_some());
        assert!(library_file(BuiltinFileKind::Irs, "cab.wav").is_some());
        assert!(library_file(BuiltinFileKind::Irs, "room.aiff").is_some());
        for bad in [
            "../secret.nam",
            r"..\secret.nam",
            "/etc/passwd",
            "/tmp/amp.nam",
            r"C:\Windows\ir.wav",
            r"\\server\share\a.nam",
            "amp.nam/../../etc/passwd",
            "not-a-nam.txt",
            "",
            " .nam",
        ] {
            assert!(
                library_file(BuiltinFileKind::Nams, bad).is_none(),
                "{bad} was accepted"
            );
        }
    }
}
