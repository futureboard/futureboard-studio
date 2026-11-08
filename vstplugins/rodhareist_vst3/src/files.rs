//! NAM captures and impulse responses: the plug-in's side of what Studio's
//! plug-in host does for a Rodhareist insert.
//!
//! The editor picks a file and keeps a copy in Rodhareist's folder
//! (`Documents/Futureboard Studio/Rodhareist/{NAMs,IRs}`, shared with
//! Studio), then hands the bytes here. They are built on a file-load thread
//! through the DSP's own loaders and adopted lock-free at
//! the next `Dsp::begin_block`. As in Studio, every answer is recorded for the
//! editor, and the saved file name changes only when the load succeeded. A
//! pick that does not load changes nothing: the capture or IR that was
//! playing keeps playing under the name the state has.
//!
//! A host that restores state calls [`restore`] from `initialize`, on the
//! same lock the audio callback needs. Reading a file and building a model
//! stay on a file-load thread. This thread only decides what to load and,
//! when the state names nothing usable, asks the audio thread to drop what
//! it is playing. nih-plug also runs `initialize` when the host merely
//! re-activates the plug-in; a pick still building then stays the newest
//! request. Only a state loaded after the pick replaces it.

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
use crate::shared::{ControlSide, Pick, Picked, Shared};

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
    /// What an editor pick handed over. `None`: the file the state names,
    /// read from the library.
    picked: Option<Picked>,
}

/// Runs one [`Task`]. File-load thread only: parsing and building allocate
/// and can take a while. The control lock is taken only to publish the result.
pub fn run(task: Task, shared: &Shared, params: &RodhareistParams, insert_id: &str) {
    let (file, name, picked) = match task {
        Task::LoadCapture { name, json } => {
            (RodhareistFile::Capture, name, Picked::Capture(json.into()))
        }
        Task::LoadImpulse { name, bytes } => {
            (RodhareistFile::Impulse, name, Picked::Impulse(bytes.into()))
        }
    };
    let capture = file == RodhareistFile::Capture;
    // The name is saved with the state and opened from the library on the
    // next restore: it passes the same check a restore makes, or nothing is
    // loaded or saved.
    let Some(name) = sanitize_file_name(file.kind(), &name) else {
        rejected(insert_id, file, &name);
        shared.answered(capture);
        return;
    };
    let Some(ticket) = claim(shared, params, &name, picked) else {
        // Not processing yet: remember the name. `restore` loads it once a
        // `Dsp` exists.
        if capture {
            params.set_nam_file(Some(name));
        } else {
            params.set_ir_file(Some(name));
        }
        shared.answered(capture);
        return;
    };
    if capture {
        load_capture(shared, params, insert_id, ticket);
    } else {
        load_impulse(shared, params, insert_id, ticket);
    }
}

/// Brings a running `Dsp` in line with the files the state names. Returns
/// before any file is read.
///
/// A name that is not a plain file inside the Rodhareist library — an
/// absolute path, a `..` segment, a network path, a device name — is
/// rejected and reported missing. The plug-in does not open it.
pub fn restore(shared: &Arc<Shared>, params: &Arc<RodhareistParams>, insert_id: &str) {
    let Some(work) = plan_restore(shared, params, insert_id) else {
        return;
    };
    let shared = Arc::clone(shared);
    let params = Arc::clone(params);
    let insert_id = insert_id.to_string();
    let spawned = std::thread::Builder::new()
        .name("rodhareist-file-load".to_string())
        .spawn(move || work.run(&shared, &params, &insert_id));
    if spawned.is_err() {
        nih_plug::nih_log!("rodhareist: could not start a file-load thread");
    }
}

/// What a restore leaves for the file-load thread.
struct Restore {
    discard_nam: Option<Box<PreparedNamRuntime>>,
    discard_ir: Option<Box<PreparedIrRuntime>>,
    nam: Option<Ticket>,
    ir: Option<Ticket>,
}

impl Restore {
    fn run(self, shared: &Shared, params: &RodhareistParams, insert_id: &str) {
        // Dropping a model is the slow part of an unload. Not on the thread
        // that is holding the audio lock.
        drop(self.discard_nam);
        drop(self.discard_ir);
        if let Some(ticket) = self.nam {
            load_capture(shared, params, insert_id, ticket);
        }
        if let Some(ticket) = self.ir {
            load_impulse(shared, params, insert_id, ticket);
        }
    }
}

/// The decisions of [`restore`], under the control lock. `None` when there
/// is nothing left for a file-load thread.
fn plan_restore(shared: &Shared, params: &RodhareistParams, insert_id: &str) -> Option<Restore> {
    let nam_revision = params.nam_file.revision();
    let ir_revision = params.ir_file.revision();
    let nam_name = params.nam_file();
    let ir_name = params.ir_file();

    let mut work = Restore {
        discard_nam: None,
        discard_ir: None,
        nam: None,
        ir: None,
    };
    {
        let mut guard = shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let control = guard.as_mut()?;
        work.nam = plan_capture(
            shared,
            control,
            &mut work.discard_nam,
            nam_name,
            nam_revision,
            insert_id,
        );
        work.ir = plan_impulse(
            shared,
            control,
            &mut work.discard_ir,
            ir_name,
            ir_revision,
            insert_id,
        );
    }
    // File names live on the parameter mirror. Wake the editor now; a load
    // that is still building answers again when it finishes or fails.
    shared.answered(true);
    shared.answered(false);

    let idle = work.nam.is_none()
        && work.ir.is_none()
        && work.discard_nam.is_none()
        && work.discard_ir.is_none();
    (!idle).then_some(work)
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

/// Makes an editor pick the newest request, and remembers it until its
/// build lands.
fn claim(shared: &Shared, params: &RodhareistParams, name: &str, picked: Picked) -> Option<Ticket> {
    let mut guard = shared
        .control
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let control = guard.as_mut()?;
    let epoch = shared.control_epoch();
    let (generation, slot, revision) = match picked {
        Picked::Capture(_) => (
            bump(&mut control.nam_generation),
            &mut control.nam_pick,
            params.nam_file.revision(),
        ),
        Picked::Impulse(_) => (
            bump(&mut control.ir_generation),
            &mut control.ir_pick,
            params.ir_file.revision(),
        ),
    };
    *slot = Some(Pick {
        name: name.to_string(),
        data: picked.clone(),
        revision,
        generation,
        epoch,
    });
    Some(Ticket {
        name: name.to_string(),
        generation,
        epoch,
        sample_rate: control.sample_rate,
        picked: Some(picked),
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

/// What a restore does with an editor pick that has not landed.
///
/// `None`: there is none, or a state was loaded after it and the state wins
/// (the caller's bump drops its build). `Some(None)`: no state was loaded
/// since, so this is a re-activation, and the build is still running for
/// this `Dsp`; leave it. `Some(Some(ticket))`: the same, but the `Dsp` is
/// new, so the pick is built again for it.
fn plan_pick(
    slot: &mut Option<Pick>,
    generation: &mut u64,
    revision: u64,
    epoch: u64,
    sample_rate: f64,
) -> Option<Option<Ticket>> {
    let pick = slot.take()?;
    if pick.revision != revision {
        return None;
    }
    if pick.epoch == epoch && pick.generation == *generation {
        *slot = Some(pick);
        return Some(None);
    }
    let generation = bump(generation);
    let ticket = Ticket {
        name: pick.name.clone(),
        generation,
        epoch,
        sample_rate,
        picked: Some(pick.data.clone()),
    };
    *slot = Some(Pick {
        generation,
        epoch,
        ..pick
    });
    Some(Some(ticket))
}

fn plan_capture(
    shared: &Shared,
    control: &mut ControlSide,
    discard: &mut Option<Box<PreparedNamRuntime>>,
    name: Option<String>,
    revision: u64,
    insert_id: &str,
) -> Option<Ticket> {
    if let Some(planned) = plan_pick(
        &mut control.nam_pick,
        &mut control.nam_generation,
        revision,
        shared.control_epoch(),
        control.sample_rate,
    ) {
        return planned;
    }
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
        // The capture that is already running matches this state, so there is
        // nothing to build. Bump anyway: an older build of a different file
        // is still in flight, and its generation would otherwise stay current
        // and replace this one when it finished.
        Some(name) if control.nam_loaded.as_deref() == Some(name.as_str()) => {
            bump(&mut control.nam_generation);
            None
        }
        Some(name) => Some(Ticket {
            generation: bump(&mut control.nam_generation),
            epoch: shared.control_epoch(),
            sample_rate: control.sample_rate,
            name,
            picked: None,
        }),
    }
}

fn plan_impulse(
    shared: &Shared,
    control: &mut ControlSide,
    discard: &mut Option<Box<PreparedIrRuntime>>,
    name: Option<String>,
    revision: u64,
    insert_id: &str,
) -> Option<Ticket> {
    if let Some(planned) = plan_pick(
        &mut control.ir_pick,
        &mut control.ir_generation,
        revision,
        shared.control_epoch(),
        control.sample_rate,
    ) {
        return planned;
    }
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
        Some(name) if control.ir_loaded.as_deref() == Some(name.as_str()) => {
            bump(&mut control.ir_generation);
            None
        }
        Some(name) => Some(Ticket {
            generation: bump(&mut control.ir_generation),
            epoch: shared.control_epoch(),
            sample_rate: control.sample_rate,
            name,
            picked: None,
        }),
    }
}

/// Builds and publishes one capture: an editor pick's JSON, or the library
/// file a state names.
fn load_capture(shared: &Shared, params: &RodhareistParams, insert_id: &str, ticket: Ticket) {
    let built = match &ticket.picked {
        Some(Picked::Capture(json)) => rodharerist::prepare_nam_runtime(
            json,
            ticket.name.clone(),
            ticket.sample_rate,
            false,
            false,
        ),
        Some(Picked::Impulse(_)) => return,
        None => {
            let Some(path) = library_file(BuiltinFileKind::Nams, &ticket.name) else {
                return;
            };
            match std::fs::read_to_string(&path) {
                Ok(json) => rodharerist::prepare_nam_runtime(
                    &json,
                    ticket.name.clone(),
                    ticket.sample_rate,
                    false,
                    false,
                ),
                Err(error) => {
                    if still_current(shared, true, &ticket) {
                        // The name is what the state asked for, and the file
                        // is not usable. Drop the capture that was playing
                        // under the old name.
                        release_nam(shared, &ticket);
                        failed(insert_id, RodhareistFile::Capture, &ticket.name, &error);
                        shared.answered(true);
                    }
                    return;
                }
            }
        }
    };
    let follow_up = publish_capture(shared, params, insert_id, built, &ticket);
    // A build a newer request replaced does not answer: that request does.
    if follow_up.is_some() || still_current(shared, true, &ticket) {
        shared.answered(true);
    }
    if let Some(next) = follow_up {
        load_capture(shared, params, insert_id, next);
    }
}

/// [`load_capture`] for an impulse response.
fn load_impulse(shared: &Shared, params: &RodhareistParams, insert_id: &str, ticket: Ticket) {
    let built = match &ticket.picked {
        Some(Picked::Impulse(bytes)) => {
            rodharerist::prepare_ir_runtime(bytes, ticket.name.clone(), ticket.sample_rate)
        }
        Some(Picked::Capture(_)) => return,
        None => {
            let Some(path) = library_file(BuiltinFileKind::Irs, &ticket.name) else {
                return;
            };
            match std::fs::read(&path) {
                Ok(bytes) => {
                    rodharerist::prepare_ir_runtime(&bytes, ticket.name.clone(), ticket.sample_rate)
                }
                Err(error) => {
                    if still_current(shared, false, &ticket) {
                        release_ir(shared, &ticket);
                        failed(insert_id, RodhareistFile::Impulse, &ticket.name, &error);
                        shared.answered(false);
                    }
                    return;
                }
            }
        }
    };
    let follow_up = publish_impulse(shared, params, insert_id, built, &ticket);
    if follow_up.is_some() || still_current(shared, false, &ticket) {
        shared.answered(false);
    }
    if let Some(next) = follow_up {
        load_impulse(shared, params, insert_id, next);
    }
}

/// Installs a finished capture build, if `ticket` is still the newest
/// request. Returns the load that brings the plug-in back to the state's
/// capture when a pick that failed had cancelled it.
fn publish_capture(
    shared: &Shared,
    params: &RodhareistParams,
    insert_id: &str,
    built: Result<PreparedNamRuntime, NamLoadError>,
    ticket: &Ticket,
) -> Option<Ticket> {
    let mut guard = shared
        .control
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let control = guard.as_mut()?;
    if !owns(shared, control.nam_generation, ticket) {
        return None;
    }
    let picked = ticket.picked.is_some();
    if picked {
        control.nam_pick = None;
    }
    let name = ticket.name.as_str();
    match built {
        Ok(runtime) => {
            let replaced = control.nam.submit(Box::new(runtime));
            control.nam_loaded = Some(name.to_string());
            params.set_nam_file(Some(name.to_string()));
            drop(guard);
            drop(replaced);
            record_ok(insert_id, RodhareistFile::Capture, name, 0);
            None
        }
        // A pick that does not load changes nothing: the running capture
        // stays, and so does the name the state saves. If the pick cancelled
        // the state's own capture before it was built, build that again.
        Err(error) if picked => {
            let mut discard = None;
            let wanted = params.nam_file();
            let follow_up = if wanted != control.nam_loaded {
                plan_capture(
                    shared,
                    control,
                    &mut discard,
                    wanted,
                    params.nam_file.revision(),
                    insert_id,
                )
            } else {
                None
            };
            drop(guard);
            drop(discard);
            record_err(insert_id, RodhareistFile::Capture, name, error.to_string());
            follow_up
        }
        // The state names this capture and it does not load: nothing the
        // state names is playing, so nothing plays.
        Err(error) => {
            let message = error.to_string();
            control.nam_loaded = None;
            let pending = control.nam.unload();
            drop(guard);
            drop(pending);
            record_err(insert_id, RodhareistFile::Capture, name, message);
            None
        }
    }
}

/// [`publish_capture`] for an impulse response.
fn publish_impulse(
    shared: &Shared,
    params: &RodhareistParams,
    insert_id: &str,
    built: Result<PreparedIrRuntime, IrLoadError>,
    ticket: &Ticket,
) -> Option<Ticket> {
    let mut guard = shared
        .control
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let control = guard.as_mut()?;
    if !owns(shared, control.ir_generation, ticket) {
        return None;
    }
    let picked = ticket.picked.is_some();
    if picked {
        control.ir_pick = None;
    }
    let name = ticket.name.as_str();
    match built {
        Ok(runtime) => {
            let frames = runtime.info().frames;
            let replaced = control.ir.submit(Box::new(runtime));
            control.ir_loaded = Some(name.to_string());
            params.set_ir_file(Some(name.to_string()));
            drop(guard);
            drop(replaced);
            record_ok(insert_id, RodhareistFile::Impulse, name, frames as u64);
            None
        }
        Err(error) if picked => {
            let mut discard = None;
            let wanted = params.ir_file();
            let follow_up = if wanted != control.ir_loaded {
                plan_impulse(
                    shared,
                    control,
                    &mut discard,
                    wanted,
                    params.ir_file.revision(),
                    insert_id,
                )
            } else {
                None
            };
            drop(guard);
            drop(discard);
            record_err(insert_id, RodhareistFile::Impulse, name, error.to_string());
            follow_up
        }
        Err(error) => {
            let message = error.to_string();
            control.ir_loaded = None;
            let pending = control.ir.unload();
            drop(guard);
            drop(pending);
            record_err(insert_id, RodhareistFile::Impulse, name, message);
            None
        }
    }
}

fn release_nam(shared: &Shared, ticket: &Ticket) {
    let pending = {
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
        control.nam_loaded = None;
        control.nam.unload()
    };
    drop(pending);
}

fn release_ir(shared: &Shared, ticket: &Ticket) {
    let pending = {
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
        control.ir_loaded = None;
        control.ir.unload()
    };
    drop(pending);
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
    use nih_plug::params::persist::PersistentField;

    #[test]
    fn session_file_names_stay_inside_the_library() {
        assert!(library_file(BuiltinFileKind::Nams, "amp.nam").is_some());
        assert!(library_file(BuiltinFileKind::Nams, "concert.nam").is_some());
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
            "CON.nam",
            "NUL.wav",
            "AUX.nam",
            "COM1.nam",
            "lpt1.wav",
            "CONIN$.nam",
            "CONOUT$.nam",
            "conin$ .nam",
            "Conout$.backup.nam",
            "COM0.nam",
            "lpt\u{b9}.wav",
        ] {
            assert!(
                library_file(BuiltinFileKind::Nams, bad).is_none(),
                "{bad} was accepted"
            );
        }
    }

    const TINY_NAM: &str = r#"{
        "version": "0.5.4", "architecture": "WaveNet",
        "config": { "layers": [{
            "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
            "kernel_size": 1, "dilations": [1], "activation": "ReLU",
            "gated": false, "head_bias": false
        }], "head": null, "head_scale": 1.0 },
        "weights": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0],
        "sample_rate": 48000.0
    }"#;

    fn running_dsp(nam_loaded: Option<&str>, generation: u64) -> (rodharerist::Dsp, Arc<Shared>) {
        let dsp = rodharerist::Dsp::new(48_000.0);
        let shared = Arc::new(Shared::default());
        {
            let mut guard = shared.control.lock().unwrap();
            let mut control = ControlSide::new(dsp.nam_loader(), dsp.ir_loader(), 48_000.0);
            control.nam_loaded = nam_loaded.map(str::to_string);
            control.nam_generation = generation;
            *guard = Some(control);
        }
        (dsp, shared)
    }

    /// A `Dsp` already running `old.nam`, as the state names it.
    fn playing_old_capture() -> (rodharerist::Dsp, Arc<Shared>, Arc<RodhareistParams>) {
        let (mut dsp, shared) = running_dsp(Some("old.nam"), 0);
        let params = Arc::new(RodhareistParams::default());
        params.set_nam_file(Some("old.nam".into()));
        dsp.nam_loader()
            .load_json(TINY_NAM, "old.nam", 48_000.0, false, false)
            .expect("load");
        dsp.begin_block_with_frames(64);
        (dsp, shared, params)
    }

    fn capture_name(dsp: &rodharerist::Dsp) -> Option<String> {
        dsp.nam_capture_info().map(|info| info.name)
    }

    fn nam_loaded(shared: &Shared) -> Option<String> {
        shared
            .control
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .nam_loaded
            .clone()
    }

    #[test]
    fn restoring_the_loaded_capture_cancels_an_older_build() {
        let (dsp, shared) = running_dsp(Some("amp.nam"), 5);
        let params = Arc::new(RodhareistParams::default());
        params.set_nam_file(Some("amp.nam".into()));
        restore(&shared, &params, "insert");
        let guard = shared.control.lock().unwrap();
        let control = guard.as_ref().unwrap();
        assert!(
            control.nam_generation > 5,
            "an in-flight build of another capture must lose its generation"
        );
        assert_eq!(control.nam_loaded.as_deref(), Some("amp.nam"));
        drop(guard);
        drop(dsp);
    }

    #[test]
    fn a_missing_or_corrupt_capture_unloads_the_previous_one() {
        let (mut dsp, shared) = running_dsp(Some("old.nam"), 2);
        let params = Arc::new(RodhareistParams::default());
        dsp.nam_loader()
            .load_json(TINY_NAM, "old.nam", 48_000.0, false, false)
            .expect("load");
        dsp.begin_block_with_frames(64);
        assert!(dsp.nam_capture_info().is_some());

        let ticket = Ticket {
            name: "gone.nam".into(),
            generation: 2,
            epoch: shared.control_epoch(),
            sample_rate: 48_000.0,
            picked: None,
        };
        load_capture(&shared, &params, "insert", ticket);
        dsp.begin_block_with_frames(64);
        assert!(
            dsp.nam_capture_info().is_none(),
            "a missing file must take the capture out of the running slot"
        );
        assert!(nam_loaded(&shared).is_none());

        // A separate instance: the fade from the unload above is the DSP's
        // job, covered by the capture tests. This checks the parse failure.
        let (mut dsp, shared) = running_dsp(Some("old.nam"), 3);
        let params = Arc::new(RodhareistParams::default());
        dsp.nam_loader()
            .load_json(TINY_NAM, "old.nam", 48_000.0, false, false)
            .expect("load");
        dsp.begin_block_with_frames(64);
        assert!(dsp.nam_capture_info().is_some());
        let broken =
            rodharerist::prepare_nam_runtime("{", "broken.nam".into(), 48_000.0, false, false);
        let ticket = Ticket {
            name: "broken.nam".into(),
            generation: 3,
            epoch: shared.control_epoch(),
            sample_rate: 48_000.0,
            picked: None,
        };
        assert!(publish_capture(&shared, &params, "insert", broken, &ticket).is_none());
        dsp.begin_block_with_frames(64);
        assert!(
            dsp.nam_capture_info().is_none(),
            "a capture that fails to parse must leave the running slot"
        );
        assert!(nam_loaded(&shared).is_none());
    }

    /// #253: nih-plug runs `initialize`, and so `restore`, on a plain host
    /// re-activation. That must not cancel a capture picked in the editor.
    #[test]
    fn a_reactivation_keeps_an_editor_pick_building() {
        let (mut dsp, shared, params) = playing_old_capture();
        let ticket = claim(
            &shared,
            &params,
            "new.nam",
            Picked::Capture(TINY_NAM.into()),
        )
        .expect("a running Dsp");
        // The host re-activates while the pick is building: same rate, same
        // `Dsp`, no state loaded.
        assert!(plan_restore(&shared, &params, "insert").is_none());
        load_capture(&shared, &params, "insert", ticket);
        dsp.begin_block_with_frames(64);
        assert_eq!(capture_name(&dsp).as_deref(), Some("new.nam"));
        assert_eq!(params.nam_file().as_deref(), Some("new.nam"));
        assert_eq!(nam_loaded(&shared).as_deref(), Some("new.nam"));
        assert!(shared
            .control
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .nam_pick
            .is_none());
    }

    /// #253: a sample-rate change builds a new `Dsp`. The pick's build was
    /// for the old one; the restore builds the pick again for the new one.
    #[test]
    fn a_new_dsp_builds_the_editor_pick_again() {
        let (_old_dsp, shared, params) = playing_old_capture();
        let ticket = claim(
            &shared,
            &params,
            "new.nam",
            Picked::Capture(TINY_NAM.into()),
        )
        .expect("a running Dsp");
        let mut dsp = rodharerist::Dsp::new(44_100.0);
        shared.install_control(dsp.nam_loader(), dsp.ir_loader(), 44_100.0);
        let work = plan_restore(&shared, &params, "insert").expect("the pick is built again");
        // The first build finishes late, for the `Dsp` that is gone.
        load_capture(&shared, &params, "insert", ticket);
        dsp.begin_block_with_frames(64);
        assert!(dsp.nam_capture_info().is_none());
        work.run(&shared, &params, "insert");
        dsp.begin_block_with_frames(64);
        let info = dsp.nam_capture_info().expect("the pick is running");
        assert_eq!(info.name, "new.nam");
        assert_eq!(params.nam_file().as_deref(), Some("new.nam"));
    }

    /// #253: a state the host loads after the pick is newer, and still
    /// replaces it.
    #[test]
    fn a_state_loaded_after_a_pick_replaces_it() {
        let (_dsp, shared, params) = playing_old_capture();
        let ticket = claim(
            &shared,
            &params,
            "new.nam",
            Picked::Capture(TINY_NAM.into()),
        )
        .expect("a running Dsp");
        // What nih-plug's state load does to the field.
        PersistentField::set(&params.nam_file, Some("state.nam".to_string()));
        let work = plan_restore(&shared, &params, "insert").expect("the state's capture");
        assert_eq!(
            work.nam.as_ref().map(|t| t.name.as_str()),
            Some("state.nam")
        );
        assert!(work.nam.as_ref().unwrap().picked.is_none());
        assert!(!still_current(&shared, true, &ticket));
        assert!(shared
            .control
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .nam_pick
            .is_none());
        load_capture(&shared, &params, "insert", ticket);
        assert_eq!(params.nam_file().as_deref(), Some("state.nam"));
    }

    /// #256: a pick that does not load leaves the running capture and the
    /// name the state saves alone.
    #[test]
    fn a_corrupt_pick_keeps_the_running_capture_and_its_name() {
        let (mut dsp, shared, params) = playing_old_capture();
        run(
            Task::LoadCapture {
                name: "broken.nam".into(),
                json: "{".into(),
            },
            &shared,
            &params,
            "insert",
        );
        dsp.begin_block_with_frames(64);
        assert_eq!(capture_name(&dsp).as_deref(), Some("old.nam"));
        assert_eq!(params.nam_file().as_deref(), Some("old.nam"));
        assert_eq!(nam_loaded(&shared).as_deref(), Some("old.nam"));

        // The same for an impulse response.
        run(
            Task::LoadImpulse {
                name: "broken.wav".into(),
                bytes: b"not a wave file".to_vec(),
            },
            &shared,
            &params,
            "insert",
        );
        assert_eq!(params.ir_file(), None);
        assert!(shared
            .control
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .ir_pick
            .is_none());
    }

    /// #256: a failed pick that had cancelled the state's own capture while
    /// it was building goes back to it, so what plays matches the name.
    #[test]
    fn a_corrupt_pick_returns_to_the_state_capture_it_cancelled() {
        let (mut dsp, shared, params) = playing_old_capture();
        // The state names a capture that is not in the library; its build is
        // in flight when the pick arrives and cancels it.
        let missing = "rodhareist-test-not-in-the-library-7f3a.nam";
        params.set_nam_file(Some(missing.into()));
        run(
            Task::LoadCapture {
                name: "broken.nam".into(),
                json: "{".into(),
            },
            &shared,
            &params,
            "insert",
        );
        dsp.begin_block_with_frames(64);
        // The state's capture was retried and is missing, so nothing plays
        // under its name, as for a restore.
        assert_eq!(capture_name(&dsp), None);
        assert_eq!(nam_loaded(&shared), None);
        assert_eq!(params.nam_file().as_deref(), Some(missing));
    }

    /// #255: a pick's name is saved with the state, so it passes the same
    /// check a restore makes.
    #[test]
    fn an_editor_pick_with_a_device_name_is_refused() {
        for name in [
            "CONIN$.nam",
            "conout$.nam",
            "CON .nam",
            "COM0.nam",
            "lpt\u{b3}.nam",
        ] {
            let (mut dsp, shared, params) = playing_old_capture();
            run(
                Task::LoadCapture {
                    name: name.into(),
                    json: TINY_NAM.into(),
                },
                &shared,
                &params,
                "insert",
            );
            dsp.begin_block_with_frames(64);
            assert_eq!(capture_name(&dsp).as_deref(), Some("old.nam"), "{name}");
            assert_eq!(params.nam_file().as_deref(), Some("old.nam"), "{name}");
            let guard = shared.control.lock().unwrap();
            assert_eq!(guard.as_ref().unwrap().nam_generation, 0, "{name}");
        }
        // Before a `Dsp` exists the name would only be remembered: not that
        // one either.
        let shared = Arc::new(Shared::default());
        let params = Arc::new(RodhareistParams::default());
        run(
            Task::LoadImpulse {
                name: "CONOUT$.wav".into(),
                bytes: Vec::new(),
            },
            &shared,
            &params,
            "insert",
        );
        assert_eq!(params.ir_file(), None);
    }
}
