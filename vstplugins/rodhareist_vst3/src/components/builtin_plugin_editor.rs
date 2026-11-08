//! Studio's state mirror, as far as the Rodhareist editor reads it: the
//! params of the instance an editor is bound to (here the plug-in's own), and
//! what the DSP last answered for each NAM capture or IR load.

use std::sync::{Arc, Mutex, Weak};

use crate::params::RodhareistParams;

/// Open editors' instances by insert id. Touched only when an editor opens
/// and when it re-reads its params, never by the audio thread.
static INSTANCES: Mutex<Vec<(String, Weak<RodhareistParams>)>> = Mutex::new(Vec::new());

pub fn register(insert_id: &str, params: &Arc<RodhareistParams>) {
    let mut instances = INSTANCES.lock().unwrap_or_else(|e| e.into_inner());
    instances.retain(|(id, params)| id != insert_id && params.strong_count() > 0);
    instances.push((insert_id.to_string(), Arc::downgrade(params)));
}

/// The instance's params as the DSP crate's struct, with the capture and IR
/// it names.
pub fn builtin_rodhareist_params(insert_id: &str) -> Option<rodharerist::Params> {
    let params = {
        let instances = INSTANCES.lock().unwrap_or_else(|e| e.into_inner());
        let (_, params) = instances.iter().find(|(id, _)| id == insert_id)?;
        params.upgrade()?
    };
    Some(params.to_core_params_with_files())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RodhareistFile {
    Capture,
    Impulse,
}

impl RodhareistFile {
    /// The plug-in's user folder the file is kept in.
    pub fn kind(self) -> crate::components::builtin_plugin_files::BuiltinFileKind {
        match self {
            Self::Capture => crate::components::builtin_plugin_files::BuiltinFileKind::Nams,
            Self::Impulse => crate::components::builtin_plugin_files::BuiltinFileKind::Irs,
        }
    }
}

/// The folder under `Documents/Futureboard Studio` Rodhareist keeps its
/// captures and impulse responses in — the same one Studio uses, so a file
/// loaded in either is there for both.
pub const RODHAREIST_FILES_FOLDER: &str = "Rodhareist";

/// What the DSP last answered for one file load.
#[derive(Debug, Clone, PartialEq)]
pub struct FileLoad {
    /// The file it was asked to load.
    pub name: String,
    /// `None` when it loaded; why not otherwise.
    pub error: Option<String>,
    /// An IR's length in frames (0 for a capture).
    pub frames: u64,
}

type LoadKey = (String, RodhareistFile);

/// The last answer per (instance, file). Written on the background task
/// thread and in `initialize`, read by the editor; never by the audio thread.
static LOADS: Mutex<Vec<(LoadKey, FileLoad)>> = Mutex::new(Vec::new());

pub fn record_file_load(insert_id: &str, file: RodhareistFile, load: FileLoad) {
    let mut loads = LOADS.lock().unwrap_or_else(|e| e.into_inner());
    loads.retain(|((id, f), _)| !(id == insert_id && *f == file));
    loads.push(((insert_id.to_string(), file), load));
}

pub fn file_load(insert_id: &str, file: RodhareistFile) -> Option<FileLoad> {
    let loads = LOADS.lock().unwrap_or_else(|e| e.into_inner());
    loads
        .iter()
        .find(|((id, f), _)| id == insert_id && *f == file)
        .map(|(_, load)| load.clone())
}
