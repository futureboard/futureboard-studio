use super::{
    format::{decode_project, decode_project_with_options, encode_project, ProjectError},
    now_secs, ClipSource, FutureboardProject, ProjectAsset,
};
use crate::paths::{FutureboardPaths, ProjectFolderLayout};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const PROJECT_FILE_EXT: &str = "fbproj";
pub const LEGACY_PROJECT_FILE_EXT: &str = "fbs";
pub const SUPPORTED_PROJECT_FILE_EXTS: &[&str] = &[PROJECT_FILE_EXT, LEGACY_PROJECT_FILE_EXT];

/// Temp path used for atomic saves: `<project>.fbproj.tmp`.
pub fn project_temp_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.tmp", path.display()))
}

/// Backup path written before each successful save: `<project>.fbproj.bak`.
pub fn project_backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bak", path.display()))
}

/// Platform-aware default projects directory: `~/Documents/Futureboard Studio/Projects/`.
///
/// Delegates to [`FutureboardPaths::resolve()`] so the path string is defined
/// in exactly one place.
pub fn default_projects_dir() -> PathBuf {
    FutureboardPaths::resolve().projects
}

/// Strips characters that are illegal in file/folder names on Windows, macOS, and Linux.
pub fn sanitize_project_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = sanitized.trim_matches(|c: char| c == ' ' || c == '.');
    if trimmed.is_empty() {
        "Untitled Project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Creates the project folder tree under `base_dir/project_name/`.
/// Returns the root folder path.
///
/// Delegates to [`ProjectFolderLayout`] for the actual subfolder structure.
pub fn create_project_folder(base_dir: &Path, project_name: &str) -> Result<PathBuf, ProjectError> {
    let safe_name = sanitize_project_name(project_name);
    let mut root = base_dir.join(&safe_name);
    if root.exists() {
        for index in 1..=999 {
            let candidate = base_dir.join(format!("{safe_name}-{index}"));
            if !candidate.exists() {
                root = candidate;
                break;
            }
        }
    }

    let layout = ProjectFolderLayout::from_root(root.clone());
    layout.ensure_dirs()?;

    Ok(root)
}

fn project_save_log(args: std::fmt::Arguments<'_>) {
    eprintln!("[ProjectSave] {args}");
}

/// Atomically writes `project` to `path`:
/// serialize → temp file → flush/fsync → backup existing → rename.
pub fn save_project(project: &mut FutureboardProject, path: &Path) -> Result<(), ProjectError> {
    project_save_log(format_args!("serialize start"));
    prepare_portable_assets(project, path)?;
    project.modified_at = now_secs();
    let bytes = encode_project(project);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = project_temp_path(path);
    let backup_path = project_backup_path(path);

    project_save_log(format_args!("writing temp: {}", tmp_path.display()));
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    project_save_log(format_args!("temp bytes written: {}", bytes.len()));
    project_save_log(format_args!("fsync complete"));

    if path.exists() {
        fs::copy(path, &backup_path)?;
        project_save_log(format_args!("backup written: {}", backup_path.display()));
        // Windows cannot rename over an existing file.
        let _ = fs::remove_file(path);
    }

    match fs::rename(&tmp_path, path) {
        Ok(()) => {
            project_save_log(format_args!("atomic rename complete: {}", path.display()));
            let _ = fs::remove_file(&tmp_path);
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp_path);
            project_save_log(format_args!("save failed: {error}"));
            Err(ProjectError::Io(error))
        }
    }
}

/// Load a project from disk, optionally allowing old versions.
pub fn load_project(path: &Path, allow_old_version: bool) -> Result<FutureboardProject, ProjectError> {
    project_load_log(format_args!("opening: {} (allow_old_version={})", path.display(), allow_old_version));
    if super::import::is_import_path(path) {
        let project = super::import::import_project(path)?;
        project_load_log(format_args!("imported ok: {}", project.name));
        return Ok(project);
    }
    let bytes = fs::read(path).map_err(|error| {
        project_load_log(format_args!("failed: I/O error: {error}"));
        ProjectError::Io(error)
    })?;
    let mut project = decode_project_with_options(&bytes, allow_old_version)?;
    resolve_project_relative_assets(&mut project, path);
    project_load_log(format_args!("loaded ok: {}", project.name));
    Ok(project)
}

/// Load a project from disk without allowing old versions (default).
pub fn load_project_strict(path: &Path) -> Result<FutureboardProject, ProjectError> {
    load_project(path, false)
}

/// Round-trip verify that `path` contains a loadable project file.
pub fn verify_project_file(path: &Path) -> Result<(), ProjectError> {
    load_project_strict(path).map(|_| ())
}

/// Cheaply validate a project file on disk by reading only its header.
///
/// Importable files have no Futureboard header; they are sniffed instead and
/// report version `0`, which callers only log.
pub fn validate_project_file(path: &Path) -> Result<u32, ProjectError> {
    use std::io::Read;
    project_load_log(format_args!("validating header: {}", path.display()));
    if super::import::is_import_path(path) {
        super::import::validate_import(path)?;
        return Ok(0);
    }
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; 20];
    if file.read_exact(&mut header).is_err() {
        return Err(ProjectError::IncompleteFile {
            reason: "file too small for project header".to_string(),
        });
    }
    super::format::peek_project_header(&header)
}

fn project_load_log(args: std::fmt::Arguments<'_>) {
    eprintln!("[ProjectLoad] {args}");
}

/// Copy an external audio file into a saved project's `Assets/Audio` folder,
/// reusing an existing copy when the bytes already live there (content
/// fingerprint dedup). Returns the absolute project-local path to use as the
/// clip's `source_path`. If `source` is already inside `project_root`, it is
/// returned unchanged. Heavy I/O (hash + copy) — call off the UI thread.
///
/// This is the eager, import-time counterpart to the copy that
/// [`prepare_portable_assets`] performs at save time; both keep a project
/// portable. The clip's asset id (`file_id`) is unaffected, so retargeting
/// `source_path` to the returned path never disturbs the waveform binding.
pub fn import_audio_file_to_project(
    source: &Path,
    project_root: &Path,
) -> Result<PathBuf, ProjectError> {
    if !source.exists() {
        return Err(ProjectError::Corrupted(format!(
            "missing audio source: {}",
            source.display()
        )));
    }
    // Already inside the project folder → nothing to copy.
    if path_relative_to_project(source, project_root).is_some() {
        return Ok(source.to_path_buf());
    }

    let layout = ProjectFolderLayout::from_root(project_root.to_path_buf());
    layout.ensure_dirs()?;

    if let Some(fingerprint) = audio_fingerprint(source) {
        let index = scan_existing_audio_fingerprints(&layout.media_audio, project_root);
        if let Some(relative) = index.get(&fingerprint) {
            let existing = project_root.join(relative);
            eprintln!(
                "[AudioImport] cache hit (content) reuse={relative} source={}",
                source.display()
            );
            return Ok(existing);
        }
    }

    let dest = unique_asset_destination(&layout.media_audio, source)?;
    fs::copy(source, &dest)?;
    eprintln!(
        "[AudioImport] copying to project={} source={}",
        dest.display(),
        source.display()
    );
    Ok(dest)
}

fn prepare_portable_assets(
    project: &mut FutureboardProject,
    project_file: &Path,
) -> Result<(), ProjectError> {
    let Some(project_root) = project_file.parent() else {
        return Ok(());
    };
    let layout = ProjectFolderLayout::from_root(project_root.to_path_buf());
    layout.ensure_dirs()?;

    let mut copied: Vec<(PathBuf, String)> = Vec::new();
    let mut assets: Vec<ProjectAsset> = Vec::new();

    // Fingerprints recorded by previous saves (v11+), keyed by project-relative
    // path. Lets us carry a known fingerprint forward without re-hashing a file
    // that is already inside the project folder.
    let prev_fp_by_rel: HashMap<String, AudioFingerprint> = project
        .assets
        .iter()
        .filter_map(|a| {
            let rel = a.relative_path.clone()?;
            let fp = a
                .source_fingerprint
                .as_deref()
                .and_then(AudioFingerprint::parse)?;
            Some((rel, fp))
        })
        .collect();

    // Content fingerprint → existing project-relative path. Seeded cheaply from
    // persisted fingerprints (no hashing) so re-imports of identical content
    // dedup against bytes copied in an earlier session. The audio folder is only
    // scanned/hashed lazily as a fallback for files that lack a persisted
    // fingerprint (e.g. projects last saved by a pre-v11 build).
    let mut content_index: HashMap<AudioFingerprint, String> = prev_fp_by_rel
        .iter()
        .filter(|(rel, _)| project_root.join(rel).exists())
        .map(|(rel, fp)| (*fp, rel.clone()))
        .collect();
    let mut folder_scanned = false;

    for track in &mut project.tracks {
        for clip in &mut track.clips {
            if let ClipSource::Rauf {
                asset_id,
                source_path,
                metadata_path,
                ..
            } = &mut clip.source
            {
                let source_abs = resolve_source_for_save(source_path, project_root);
                if !source_abs.exists() {
                    return Err(ProjectError::Corrupted(format!(
                        "missing RAUF recording: {}",
                        source_abs.display()
                    )));
                }
                if let Some(relative) = path_relative_to_project(&source_abs, project_root) {
                    let relative_string = path_to_project_string(&relative);
                    *source_path = PathBuf::from(&relative_string);
                    *asset_id = relative_string.clone();
                }
                if let Some(metadata_path) = metadata_path {
                    let metadata_abs = resolve_source_for_save(metadata_path, project_root);
                    if let Some(relative) = path_relative_to_project(&metadata_abs, project_root) {
                        *metadata_path = PathBuf::from(path_to_project_string(&relative));
                    }
                }
                continue;
            }

            let ClipSource::Audio {
                asset_id,
                source_path: Some(source_path),
            } = &mut clip.source
            else {
                continue;
            };

            let source_abs = resolve_source_for_save(source_path, project_root);
            if !source_abs.exists() {
                return Err(ProjectError::Corrupted(format!(
                    "missing audio asset: {}",
                    source_abs.display()
                )));
            }

            if let Some(relative) = path_relative_to_project(&source_abs, project_root) {
                let relative_string = path_to_project_string(&relative);
                // Carry a known fingerprint forward; only hash if this file was
                // never fingerprinted (first save after a pre-v11 upgrade).
                let fingerprint = prev_fp_by_rel
                    .get(&relative_string)
                    .copied()
                    .or_else(|| audio_fingerprint(&source_abs));
                if let Some(fp) = fingerprint {
                    content_index
                        .entry(fp)
                        .or_insert_with(|| relative_string.clone());
                }
                *source_path = PathBuf::from(&relative_string);
                *asset_id = relative_string.clone();
                assets.push(asset_record(
                    asset_id.clone(),
                    &source_abs,
                    relative_string,
                    None,
                    fingerprint,
                    None,
                    None,
                    None,
                    None,
                )?);
                continue;
            }

            // External source. Reuse an identical-content copy already in the
            // project before falling back to path-equality dedup within this
            // save. Mirrors the path-reuse branch: rewrite the reference only,
            // without emitting a second asset record for the same file.
            let fingerprint = audio_fingerprint(&source_abs);
            if let Some(fp) = fingerprint {
                if !content_index.contains_key(&fp) && !folder_scanned {
                    // Persisted fingerprints missed; hash the folder once to
                    // cover legacy/externally-added files before copying.
                    for (existing_fp, rel) in
                        scan_existing_audio_fingerprints(&layout.media_audio, project_root)
                    {
                        content_index.entry(existing_fp).or_insert(rel);
                    }
                    folder_scanned = true;
                }
                if let Some(existing_rel) = content_index.get(&fp) {
                    eprintln!(
                        "[AudioImport] cache hit (content) reuse={existing_rel} source={}",
                        source_abs.display()
                    );
                    *source_path = PathBuf::from(existing_rel);
                    *asset_id = existing_rel.clone();
                    continue;
                }
            }

            if let Some((_, relative_string)) = copied
                .iter()
                .find(|(known_source, _)| same_source(known_source, &source_abs))
            {
                *source_path = PathBuf::from(relative_string);
                *asset_id = relative_string.clone();
                continue;
            }

            let dest = unique_asset_destination(&layout.media_audio, &source_abs)?;
            fs::copy(&source_abs, &dest)?;
            eprintln!(
                "[AudioImport] copying to project={} source={}",
                dest.display(),
                source_abs.display()
            );
            let relative = path_relative_to_project(&dest, project_root).ok_or_else(|| {
                ProjectError::Corrupted(format!(
                    "copied asset escaped project folder: {}",
                    dest.display()
                ))
            })?;
            let relative_string = path_to_project_string(&relative);

            *source_path = PathBuf::from(&relative_string);
            *asset_id = relative_string.clone();
            copied.push((source_abs.clone(), relative_string.clone()));
            // Prefer the fingerprint of the source we just read; fall back to the
            // freshly-written copy.
            let dest_fingerprint = fingerprint.or_else(|| audio_fingerprint(&dest));
            if let Some(fp) = dest_fingerprint {
                content_index.insert(fp, relative_string.clone());
            }
            assets.push(asset_record(
                asset_id.clone(),
                &dest,
                relative_string,
                Some(source_abs),
                dest_fingerprint,
                None,
                None,
                None,
                None,
            )?);
        }
    }

    if !assets.is_empty() {
        project.assets = assets;
    }
    Ok(())
}

fn resolve_project_relative_assets(project: &mut FutureboardProject, project_file: &Path) {
    let Some(project_root) = project_file.parent() else {
        return;
    };
    for track in &mut project.tracks {
        for clip in &mut track.clips {
            let ClipSource::Audio {
                source_path: Some(source_path),
                ..
            } = &mut clip.source
            else {
                continue;
            };
            if source_path.is_relative() {
                *source_path = project_root.join(&source_path);
            }
        }
        for clip in &mut track.clips {
            let ClipSource::Rauf {
                source_path,
                metadata_path,
                ..
            } = &mut clip.source
            else {
                continue;
            };
            if source_path.is_relative() {
                *source_path = project_root.join(&source_path);
            }
            if let Some(metadata_path) = metadata_path {
                if metadata_path.is_relative() {
                    *metadata_path = project_root.join(&metadata_path);
                }
            }
        }
    }
}

fn resolve_source_for_save(source_path: &Path, project_root: &Path) -> PathBuf {
    if source_path.is_absolute() {
        source_path.to_path_buf()
    } else {
        project_root.join(source_path)
    }
}

fn path_relative_to_project(path: &Path, project_root: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).ok()?;
    let root = fs::canonicalize(project_root).ok()?;
    path.strip_prefix(root).ok().map(Path::to_path_buf)
}

fn unique_asset_destination(asset_dir: &Path, source: &Path) -> Result<PathBuf, ProjectError> {
    fs::create_dir_all(asset_dir)?;
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_project_name)
        .unwrap_or_else(|| "audio".to_string());
    let stem = Path::new(&file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    let ext = Path::new(&file_name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let mut candidate = asset_dir.join(&file_name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    for index in 1..=999 {
        let name = if ext.is_empty() {
            format!("{stem}-{index}")
        } else {
            format!("{stem}-{index}.{ext}")
        };
        candidate = asset_dir.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(ProjectError::Corrupted(format!(
        "could not create a unique asset filename for {}",
        source.display()
    )))
}

fn asset_record(
    id: String,
    copied_path: &Path,
    relative_path: String,
    original_path: Option<PathBuf>,
    fingerprint: Option<AudioFingerprint>,
    duration_samples: Option<u64>,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    duration_secs: Option<f64>,
) -> Result<ProjectAsset, ProjectError> {
    Ok(ProjectAsset {
        id: id.clone(),
        original_filename: copied_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "audio".to_string()),
        relative_path: Some(relative_path),
        absolute_path: original_path,
        duration_secs,
        sample_rate,
        channels,
        source_fingerprint: fingerprint.map(|fp| fp.to_token()),
        waveform_peak_relative_path: Some(
            crate::components::timeline::waveform_peak_file::waveform_peak_relative_path_for_asset(
                &id,
            ),
        ),
        duration_samples,
    })
}

/// Content identity for asset dedup: byte length + CRC32 of the file contents.
/// Two files with the same fingerprint are treated as the same audio asset, so
/// re-importing identical bytes reuses the existing project copy rather than
/// writing a duplicate. Waveform peaks are cached separately under
/// `Cache/Waveforms/` keyed by stable `asset_id` (see `waveform_peak_file`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AudioFingerprint {
    len: u64,
    crc: u32,
}

impl AudioFingerprint {
    /// Persisted token form: `"<len:x>-<crc:08x>"`.
    fn to_token(self) -> String {
        format!("{:x}-{:08x}", self.len, self.crc)
    }

    /// Parse a token written by [`AudioFingerprint::to_token`]. Returns `None`
    /// for malformed or pre-v11 (absent) values.
    fn parse(token: &str) -> Option<Self> {
        let (len, crc) = token.split_once('-')?;
        Some(Self {
            len: u64::from_str_radix(len, 16).ok()?,
            crc: u32::from_str_radix(crc, 16).ok()?,
        })
    }
}

/// Stream `path` through CRC32 without loading it fully into memory. Returns
/// `None` if the file cannot be read (caller falls back to path-equality dedup).
fn audio_fingerprint(path: &Path) -> Option<AudioFingerprint> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(AudioFingerprint {
        len,
        crc: hasher.finalize(),
    })
}

/// Fingerprint every file directly under the project's audio folder so external
/// imports can be matched against bytes already copied in a previous session.
/// On a fingerprint collision the first (lexically encountered) path wins.
fn scan_existing_audio_fingerprints(
    audio_dir: &Path,
    project_root: &Path,
) -> HashMap<AudioFingerprint, String> {
    let mut index = HashMap::new();
    let Ok(entries) = fs::read_dir(audio_dir) else {
        return index;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(relative) = path_relative_to_project(&path, project_root) else {
            continue;
        };
        if let Some(fingerprint) = audio_fingerprint(&path) {
            index
                .entry(fingerprint)
                .or_insert_with(|| path_to_project_string(&relative));
        }
    }
    index
}

fn same_source(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

pub fn path_to_project_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Returns a project-relative path string when `path` lives under `project_root`.
pub fn relative_path_in_project(path: &Path, project_root: &Path) -> Option<String> {
    path_relative_to_project(path, project_root).map(|rel| path_to_project_string(&rel))
}

/// Resolve a project-relative audio or cache path against the project folder.
pub fn resolve_project_relative_path(project_root: &Path, relative: &str) -> PathBuf {
    project_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::AudioClipStretchState;
    use crate::project::{
        format::{encode_project, ProjectError, PROJECT_HEADER_SIZE},
        ClipSource, FutureboardProject, ProjectAsset, ProjectClip, ProjectSession, ProjectTrack,
        ProjectTrackType, TrackRouting,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let unique = format!(
            "futureboard-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn load_empty_file_reports_incomplete_project() {
        let dir = temp_dir("empty-file");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Empty.fbproj");
        fs::write(&path, &[]).unwrap();
        let err = load_project_strict(&path).unwrap_err();
        assert_eq!(
            err.user_message(),
            "Could not open this project because the file appears to be incomplete or corrupted."
        );
        assert!(matches!(err, ProjectError::IncompleteFile { .. }));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_truncated_header_reports_incomplete_project() {
        let dir = temp_dir("trunc-header");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Trunc.fbproj");
        fs::write(&path, &[0u8; PROJECT_HEADER_SIZE - 1]).unwrap();
        let err = load_project_strict(&path).unwrap_err();
        assert!(matches!(err, ProjectError::IncompleteFile { .. }));
        assert_eq!(
            err.user_message(),
            "Could not open this project because the file appears to be incomplete or corrupted."
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_truncated_payload_reports_unexpected_eof() {
        let dir = temp_dir("trunc-payload");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("TruncBody.fbproj");
        let mut bytes = encode_project(&FutureboardProject::new("TruncBody"));
        bytes.truncate(PROJECT_HEADER_SIZE + 2);
        fs::write(&path, &bytes).unwrap();
        let err = load_project_strict(&path).unwrap_err();
        assert!(
            matches!(err, ProjectError::IncompleteFile { .. })
                || matches!(err, ProjectError::UnexpectedEof { .. })
                || matches!(err, ProjectError::ChecksumMismatch { .. })
        );
        assert_eq!(
            err.user_message(),
            "Could not open this project because the file appears to be incomplete or corrupted."
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_save_keeps_original_when_temp_is_invalid() {
        let dir = temp_dir("atomic-save");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Song.fbproj");
        let mut project = FutureboardProject::new("Song");
        save_project(&mut project, &path).unwrap();
        let original = fs::read(&path).unwrap();
        verify_project_file(&path).unwrap();

        let tmp = project_temp_path(&path);
        fs::write(&tmp, &[1, 2, 3]).unwrap();
        project.name = "Song Updated".to_string();
        save_project(&mut project, &path).unwrap();
        verify_project_file(&path).unwrap();
        let updated = fs::read(&path).unwrap();
        assert_ne!(updated, original);
        assert!(project_backup_path(&path).exists());
        let backup = load_project_strict(&project_backup_path(&path)).unwrap();
        assert_eq!(backup.name, "Song");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn new_project_save_and_reopen_roundtrip() {
        let base = temp_dir("new-project-reopen");
        fs::create_dir_all(&base).unwrap();
        let folder = create_project_folder(&base, "Test Song").unwrap();
        let project_file = folder.join("Test Song.fbproj");
        let mut project = FutureboardProject::new("Test Song");
        save_project(&mut project, &project_file).unwrap();
        verify_project_file(&project_file).unwrap();
        project.name = "Test Song Updated".to_string();
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        assert_eq!(loaded.name, "Test Song Updated");
        assert!(project_backup_path(&project_file).exists());

        let mut session = ProjectSession::untitled();
        session.bind_saved(
            loaded.id.clone(),
            loaded.name.clone(),
            Some(folder.clone()),
            project_file.clone(),
            loaded.created_at,
            loaded.modified_at,
        );
        assert!(!session.needs_save_as());

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn save_project_copies_external_audio_to_assets_audio() {
        let root = temp_dir("asset-copy");
        let external = temp_dir("external-audio");
        fs::create_dir_all(&external).unwrap();
        let source = external.join("loop.wav");
        fs::write(&source, b"fake wav bytes").unwrap();

        let mut project = FutureboardProject::new("Portable");
        project.tracks.push(ProjectTrack {
            id: "track-1".to_string(),
            name: "Audio 1".to_string(),
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: crate::components::timeline::timeline_state::TrackTimebase::Musical.to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: "#56C7C9".to_string(),
            volume_norm: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: crate::project::InputMonitorMode::Off,
            routing: TrackRouting::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips: vec![ProjectClip {
                id: "clip-1".to_string(),
                name: "loop".to_string(),
                start_beat: 0.0,
                duration_beats: 4.0,
                offset_beats: 0.0,
                gain: 1.0,
                muted: false,
                source: ClipSource::Audio {
                    asset_id: source.to_string_lossy().into_owned(),
                    source_path: Some(source.clone()),
                },
                stretch: AudioClipStretchState::default(),
            }],
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        });

        let project_file = root.join("Portable.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let copied = root.join("Assets").join("Audio").join("loop.wav");
        assert!(copied.exists());
        let loaded = load_project_strict(&project_file).unwrap();
        let ClipSource::Audio {
            source_path: Some(loaded_path),
            ..
        } = &loaded.tracks[0].clips[0].source
        else {
            panic!("expected loaded audio clip source");
        };
        assert_eq!(loaded_path, &copied);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(external);
    }

    fn audio_clip(id: &str, source: &Path) -> ProjectClip {
        ProjectClip {
            id: id.to_string(),
            name: "loop".to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            offset_beats: 0.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Audio {
                asset_id: source.to_string_lossy().into_owned(),
                source_path: Some(source.to_path_buf()),
            },
            stretch: AudioClipStretchState::default(),
        }
    }

    fn audio_track(id: &str, clips: Vec<ProjectClip>) -> ProjectTrack {
        ProjectTrack {
            id: id.to_string(),
            name: "Audio 1".to_string(),
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: crate::components::timeline::timeline_state::TrackTimebase::Musical.to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: "#56C7C9".to_string(),
            volume_norm: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: crate::project::InputMonitorMode::Off,
            routing: TrackRouting::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips,
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        }
    }

    fn audio_files_in(root: &Path) -> Vec<String> {
        let dir = root.join("Assets").join("Audio");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_file())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn save_project_dedups_identical_content_from_different_paths() {
        // Two distinct external files (same name, different folders) with byte-
        // identical content must collapse to a single project copy (spec #3).
        let root = temp_dir("asset-dedup");
        let ext_a = temp_dir("dedup-a");
        let ext_b = temp_dir("dedup-b");
        fs::create_dir_all(&ext_a).unwrap();
        fs::create_dir_all(&ext_b).unwrap();
        let source_a = ext_a.join("loop.wav");
        let source_b = ext_b.join("loop.wav");
        fs::write(&source_a, b"identical wav bytes").unwrap();
        fs::write(&source_b, b"identical wav bytes").unwrap();

        let mut project = FutureboardProject::new("Dedup");
        project.tracks.push(audio_track(
            "track-1",
            vec![
                audio_clip("clip-a", &source_a),
                audio_clip("clip-b", &source_b),
            ],
        ));

        let project_file = root.join("Dedup.fbproj");
        save_project(&mut project, &project_file).unwrap();

        assert_eq!(
            audio_files_in(&root),
            vec!["loop.wav".to_string()],
            "identical content must be copied only once"
        );

        let loaded = load_project_strict(&project_file).unwrap();
        let paths: Vec<PathBuf> = loaded.tracks[0]
            .clips
            .iter()
            .filter_map(|c| match &c.source {
                ClipSource::Audio {
                    source_path: Some(p),
                    ..
                } => Some(p.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], paths[1], "both clips must reference the one copy");

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext_a);
        let _ = fs::remove_dir_all(ext_b);
    }

    #[test]
    fn save_project_keeps_distinct_content_with_same_name() {
        // Same filename, different content → both must coexist (spec #14).
        let root = temp_dir("asset-collision");
        let ext_a = temp_dir("collision-a");
        let ext_b = temp_dir("collision-b");
        fs::create_dir_all(&ext_a).unwrap();
        fs::create_dir_all(&ext_b).unwrap();
        let source_a = ext_a.join("loop.wav");
        let source_b = ext_b.join("loop.wav");
        fs::write(&source_a, b"first content").unwrap();
        fs::write(&source_b, b"second different content").unwrap();

        let mut project = FutureboardProject::new("Collision");
        project.tracks.push(audio_track(
            "track-1",
            vec![
                audio_clip("clip-a", &source_a),
                audio_clip("clip-b", &source_b),
            ],
        ));

        let project_file = root.join("Collision.fbproj");
        save_project(&mut project, &project_file).unwrap();

        assert_eq!(
            audio_files_in(&root),
            vec!["loop-1.wav".to_string(), "loop.wav".to_string()],
            "distinct content with a colliding name must both be kept"
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext_a);
        let _ = fs::remove_dir_all(ext_b);
    }

    #[test]
    fn import_audio_file_to_project_copies_dedups_and_passes_through() {
        let root = temp_dir("eager-import");
        let ext = temp_dir("eager-ext");
        fs::create_dir_all(&ext).unwrap();
        fs::create_dir_all(&root).unwrap();
        let source = ext.join("loop.wav");
        fs::write(&source, b"eager copy bytes").unwrap();

        // External source → copied into Assets/Audio, returns the project-local path.
        let dest = import_audio_file_to_project(&source, &root).unwrap();
        let expected = root.join("Assets").join("Audio").join("loop.wav");
        assert_eq!(dest, expected);
        assert!(dest.exists());

        // Identical content from a different external path → reuse, no second copy.
        let ext2 = temp_dir("eager-ext2");
        fs::create_dir_all(&ext2).unwrap();
        let source2 = ext2.join("again.wav");
        fs::write(&source2, b"eager copy bytes").unwrap();
        let dest2 = import_audio_file_to_project(&source2, &root).unwrap();
        assert_eq!(
            dest2, expected,
            "identical content must reuse the existing copy"
        );
        assert_eq!(audio_files_in(&root), vec!["loop.wav".to_string()]);

        // A file already inside the project folder is returned unchanged.
        let passthrough = import_audio_file_to_project(&dest, &root).unwrap();
        assert_eq!(passthrough, dest);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext);
        let _ = fs::remove_dir_all(ext2);
    }

    #[test]
    fn audio_fingerprint_token_roundtrips() {
        let fp = AudioFingerprint {
            len: 0xDEAD_BEEF,
            crc: 0x0042_00AB,
        };
        let token = fp.to_token();
        assert_eq!(AudioFingerprint::parse(&token), Some(fp));
        assert_eq!(AudioFingerprint::parse("not-a-fingerprint"), None);
        assert_eq!(AudioFingerprint::parse("deadbeef"), None);
    }

    #[test]
    fn save_persists_fingerprint_and_dedups_after_reload() {
        // First save persists a content fingerprint (v11); a later session that
        // re-imports identical bytes from a different path must reuse the copy
        // via that persisted fingerprint instead of copying again (spec #3, #7).
        let root = temp_dir("asset-fp");
        let ext = temp_dir("fp-ext");
        fs::create_dir_all(&ext).unwrap();
        let source = ext.join("loop.wav");
        fs::write(&source, b"fingerprint me please").unwrap();

        let mut project = FutureboardProject::new("FP");
        project
            .tracks
            .push(audio_track("track-1", vec![audio_clip("clip-a", &source)]));
        let project_file = root.join("FP.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        assert!(
            loaded.assets[0].source_fingerprint.is_some(),
            "asset fingerprint must persist across save/load (v11)"
        );

        let ext2 = temp_dir("fp-ext2");
        fs::create_dir_all(&ext2).unwrap();
        let source2 = ext2.join("again.wav");
        fs::write(&source2, b"fingerprint me please").unwrap();

        let mut reloaded = loaded;
        reloaded.tracks[0]
            .clips
            .push(audio_clip("clip-b", &source2));
        save_project(&mut reloaded, &project_file).unwrap();

        assert_eq!(
            audio_files_in(&root),
            vec!["loop.wav".to_string()],
            "identical content re-imported after reload must not be re-copied"
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext);
        let _ = fs::remove_dir_all(ext2);
    }

    fn sample_peak_preview() -> crate::components::timeline::waveform_cache::WaveformPreview {
        use crate::components::timeline::waveform_cache::{WaveformLod, WaveformPeak};
        crate::components::timeline::waveform_cache::WaveformPreview {
            sample_rate: 48_000,
            channels: 2,
            duration_seconds: 1.0,
            total_frames: 48_000,
            lods: vec![WaveformLod {
                samples_per_peak: 256,
                peaks: vec![
                    WaveformPeak {
                        min: -0.5,
                        max: 0.5,
                    },
                    WaveformPeak {
                        min: -0.2,
                        max: 0.8,
                    },
                ],
            }],
        }
    }

    /// Test A: import writes project cache layout and asset metadata.
    #[test]
    fn import_writes_project_audio_and_peak_cache_paths() {
        use crate::components::timeline::waveform_peak_file::{
            read_peak_file, waveform_peak_relative_path_for_asset, write_peak_file,
        };
        use crate::paths::ProjectFolderLayout;

        let root = temp_dir("peak-a");
        let ext = temp_dir("peak-a-ext");
        fs::create_dir_all(&ext).unwrap();
        let source = ext.join("loop.wav");
        fs::write(&source, b"fake wav bytes for cache test").unwrap();

        let dest = import_audio_file_to_project(&source, &root).unwrap();
        assert!(dest.exists());
        assert_eq!(
            relative_path_in_project(&dest, &root).as_deref(),
            Some("Assets/Audio/loop.wav")
        );

        let asset_id = "Assets/Audio/loop.wav";
        let peak_rel = waveform_peak_relative_path_for_asset(asset_id);
        let peak_path = resolve_project_relative_path(&root, &peak_rel);
        write_peak_file(&peak_path, asset_id, &sample_peak_preview(), None).unwrap();
        assert!(peak_path.exists());

        let mut project = FutureboardProject::new("PeakA");
        project
            .tracks
            .push(audio_track("t1", vec![audio_clip("c1", &dest)]));
        let project_file = root.join("PeakA.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        assert_eq!(
            loaded.assets[0].relative_path.as_deref(),
            Some("Assets/Audio/loop.wav")
        );
        assert_eq!(
            loaded.assets[0].waveform_peak_relative_path.as_deref(),
            Some(peak_rel.as_str())
        );
        let layout = ProjectFolderLayout::from_root(root.clone());
        assert!(layout.media_audio.exists());
        assert!(layout.cache_waveforms.exists());
        read_peak_file(&peak_path, Some(asset_id), None).unwrap();

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext);
    }

    /// Test B: reopen loads peak cache from disk without memory cache.
    #[test]
    fn reopen_loads_peak_cache_from_disk() {
        use crate::components::timeline::waveform_peak_file::{
            read_peak_file, waveform_peak_relative_path_for_asset, write_peak_file,
        };

        let root = temp_dir("peak-b");
        let asset_id = "Assets/Audio/loop.wav";
        let audio_path = root.join("Assets/Audio/loop.wav");
        fs::create_dir_all(audio_path.parent().unwrap()).unwrap();
        fs::write(&audio_path, b"audio").unwrap();

        let peak_rel = waveform_peak_relative_path_for_asset(asset_id);
        let peak_path = resolve_project_relative_path(&root, &peak_rel);
        write_peak_file(&peak_path, asset_id, &sample_peak_preview(), None).unwrap();

        let mut project = FutureboardProject::new("PeakB");
        project.assets.push(ProjectAsset {
            id: asset_id.to_string(),
            original_filename: "loop.wav".to_string(),
            relative_path: Some(asset_id.to_string()),
            absolute_path: None,
            duration_secs: Some(1.0),
            sample_rate: Some(48_000),
            channels: Some(2),
            source_fingerprint: None,
            waveform_peak_relative_path: Some(peak_rel.clone()),
            duration_samples: Some(48_000),
        });
        project
            .tracks
            .push(audio_track("t1", vec![audio_clip("c1", &audio_path)]));
        let project_file = root.join("PeakB.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        let peak_path = resolve_project_relative_path(
            &root,
            loaded.assets[0]
                .waveform_peak_relative_path
                .as_ref()
                .unwrap(),
        );
        let preview = read_peak_file(&peak_path, Some(asset_id), None).unwrap();
        assert_eq!(preview.lods[0].peaks.len(), 2);

        let _ = fs::remove_dir_all(root);
    }

    /// Test C: repeated import reuses one project-local audio file.
    #[test]
    fn repeated_import_reuses_project_audio_copy() {
        use crate::components::timeline::waveform_peak_file::waveform_peak_relative_path_for_asset;

        let root = temp_dir("peak-c");
        let ext = temp_dir("peak-c-ext");
        fs::create_dir_all(&ext).unwrap();
        let source_a = ext.join("loop.wav");
        let source_b = ext.join("again.wav");
        fs::write(&source_a, b"same bytes").unwrap();
        fs::write(&source_b, b"same bytes").unwrap();

        let dest_a = import_audio_file_to_project(&source_a, &root).unwrap();
        let dest_b = import_audio_file_to_project(&source_b, &root).unwrap();
        assert_eq!(dest_a, dest_b);
        assert_eq!(audio_files_in(&root), vec!["loop.wav".to_string()]);

        let asset_id = "Assets/Audio/loop.wav";
        let peak_rel = waveform_peak_relative_path_for_asset(asset_id);
        assert_eq!(peak_rel, "Cache/Waveforms/Assets__Audio__loop.wav.peaks");

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext);
    }

    /// Test D: missing peak file is a disk miss (regeneration path).
    #[test]
    fn missing_peak_file_reports_disk_miss() {
        use crate::components::timeline::waveform_peak_file::{read_peak_file, PeakFileError};

        let root = temp_dir("peak-d");
        let asset_id = "Assets/Audio/loop.wav";
        let peak_path = resolve_project_relative_path(
            &root,
            &crate::components::timeline::waveform_peak_file::waveform_peak_relative_path_for_asset(
                asset_id,
            ),
        );
        let err = read_peak_file(&peak_path, Some(asset_id), None).unwrap_err();
        assert!(matches!(
            err,
            PeakFileError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound
        ));
        let _ = fs::remove_dir_all(root);
    }

    /// Test E: project-local audio survives loss of original external source.
    #[test]
    fn project_local_audio_used_when_external_source_missing() {
        let root = temp_dir("peak-e");
        let ext = temp_dir("peak-e-ext");
        fs::create_dir_all(&ext).unwrap();
        let source = ext.join("loop.wav");
        fs::write(&source, b"portable audio").unwrap();

        let dest = import_audio_file_to_project(&source, &root).unwrap();
        fs::remove_file(&source).unwrap();
        assert!(!source.exists());
        assert!(dest.exists());

        let mut project = FutureboardProject::new("PeakE");
        project
            .tracks
            .push(audio_track("t1", vec![audio_clip("c1", &dest)]));
        let project_file = root.join("PeakE.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        let ClipSource::Audio {
            source_path: Some(resolved),
            ..
        } = &loaded.tracks[0].clips[0].source
        else {
            panic!("expected audio clip source");
        };
        assert!(resolved.exists());

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext);
    }

    #[test]
    fn user_message_maps_invalid_magic() {
        let err = ProjectError::InvalidMagic;
        assert_eq!(
            err.user_message(),
            "This file is not a Futureboard project."
        );
    }

    #[test]
    fn user_message_maps_unexpected_eof() {
        let err = ProjectError::UnexpectedEof {
            needed: 4,
            remaining: 1,
            field: "u32",
        };
        assert_eq!(
            err.user_message(),
            "Could not open this project because the file appears to be incomplete or corrupted."
        );
        assert!(err.technical_detail().contains("u32"));
    }
}
