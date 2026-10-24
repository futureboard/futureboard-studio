//! Reading project files written by other DAWs.
//!
//! An imported file is a *source*, never a save target: it is parsed into a
//! `FutureboardProject` in memory and the session binds the result as untitled,
//! so the first save goes through Save As and the original file is left alone.

pub mod cubase_xml;

use std::path::Path;

use super::format::ProjectError;
use super::FutureboardProject;

/// Extensions the Open Project dialogs accept in addition to the native
/// project files.
pub const IMPORT_PROJECT_FILE_EXTS: &[&str] = &["xml"];

/// Whether `path` is handled by an importer rather than the native decoder.
pub fn is_import_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext = ext.to_ascii_lowercase();
            IMPORT_PROJECT_FILE_EXTS.contains(&ext.as_str())
        })
        .unwrap_or(false)
}

/// Header-only check used by the open-project validation step, mirroring
/// [`super::io::validate_project_file`] for importable files.
pub fn validate_import(path: &Path) -> Result<(), ProjectError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(ProjectError::Io)?;
    let mut head = [0u8; 4096];
    let read = file.read(&mut head).map_err(ProjectError::Io)?;
    if cubase_xml::sniff(&head[..read]) {
        return Ok(());
    }
    Err(ProjectError::Corrupted(
        "unrecognised XML: not a Cubase/Nuendo track archive".to_string(),
    ))
}

/// Parse an importable file into a project. Heavy — call off the UI thread,
/// like the native decoder.
pub fn import_project(path: &Path) -> Result<FutureboardProject, ProjectError> {
    validate_import(path)?;
    cubase_xml::import(path)
}
