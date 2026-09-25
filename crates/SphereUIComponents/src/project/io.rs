use super::{
    ClipSource, FutureboardProject, ProjectAsset,
    format::{
        ProjectError, ProjectIdentity, decode_project, decode_project_identity,
        decode_project_with_options, encode_project,
    },
    now_secs,
};
use crate::components::timeline::timeline_state::AudioClipStretchState;
use crate::paths::{FutureboardPaths, ProjectFolderLayout};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const PROJECT_FILE_EXT: &str = "fbproj";
pub const LEGACY_PROJECT_FILE_EXT: &str = "fbs";
pub const SUPPORTED_PROJECT_FILE_EXTS: &[&str] = &[PROJECT_FILE_EXT, LEGACY_PROJECT_FILE_EXT];

/// Temp path older builds used for every save: `<project>.fbproj.tmp`. Saves
/// now write a per-job name ([`unique_project_temp_path`]); this one is only
/// cleaned up.
pub fn project_temp_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.tmp", path.display()))
}

/// Temp path for one save job: `<project>.fbproj.<pid>-<n>.tmp`. Unique per
/// job, so two writers can never truncate or rename each other's temp file.
fn unique_project_temp_path(path: &Path) -> PathBuf {
    static NEXT_JOB: AtomicU64 = AtomicU64::new(0);
    let job = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "{}.{}-{job}.tmp",
        path.display(),
        std::process::id()
    ))
}

/// Serializes every project-file write in this process: the background and
/// synchronous manual saves, autosave, Save Copy and the shutdown autosave
/// flush. They share the project's `Assets/Audio` folder (whose
/// `unique_asset_destination` check-then-copy is not atomic) and, for one
/// target, its backup, so two interleaved writers could pick the same asset
/// destination or put an older snapshot over a newer one. Ordering between
/// requests is the caller's save queue; this lock only keeps writes apart.
static PROJECT_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Backup path written before each successful save: `<project>.fbproj.bak`.
pub fn project_backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bak", path.display()))
}

/// Stable backup path for a legacy project before it is opened and potentially
/// upgraded on the next save. The first backup for a version is preserved.
pub fn legacy_project_backup_path(path: &Path, version: u32) -> PathBuf {
    PathBuf::from(format!("{}.v{version}.bak", path.display()))
}

/// Preserve the exact legacy bytes before allowing an old project to load.
/// Existing backups are never overwritten, so the original source remains
/// recoverable even if the project is opened and saved repeatedly.
pub fn backup_legacy_project(path: &Path, version: u32) -> Result<PathBuf, ProjectError> {
    let backup = legacy_project_backup_path(path, version);
    if backup.exists() {
        return Ok(backup);
    }
    if let Some(parent) = backup.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(path, &backup)?;
    project_save_log(format_args!(
        "legacy backup written: {} (source version {})",
        backup.display(),
        version
    ));
    Ok(backup)
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

/// What a save found besides writing the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectSaveReport {
    /// Media the project references that could not be found while saving,
    /// resolved to absolute paths. Their clips keep the reference they had:
    /// relative when it lies under the project folder, otherwise absolute.
    pub offline_media: Vec<PathBuf>,
}

/// Atomically writes `project` to `path`:
/// serialize → temp file → flush/fsync → backup existing → rename.
pub fn save_project(project: &mut FutureboardProject, path: &Path) -> Result<(), ProjectError> {
    save_project_with_report(project, path).map(|_| ())
}

/// [`save_project`], also reporting media that was offline. A missing source
/// never fails the save: losing every other edit because one file is
/// unplugged is worse than saving a reference that has to be relinked.
pub fn save_project_with_report(
    project: &mut FutureboardProject,
    path: &Path,
) -> Result<ProjectSaveReport, ProjectError> {
    let _write_guard = PROJECT_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    project_save_log(format_args!("serialize start"));
    let offline_media = prepare_portable_assets(project, path)?;
    for missing in &offline_media {
        project_save_log(format_args!(
            "offline media kept as a reference: {}",
            missing.display()
        ));
    }
    project.modified_at = now_secs();
    let bytes = encode_project(project);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = unique_project_temp_path(path);
    let backup_path = project_backup_path(path);

    project_save_log(format_args!("writing temp: {}", tmp_path.display()));
    let written = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&tmp_path);
        return Err(ProjectError::Io(error));
    }
    project_save_log(format_args!("temp bytes written: {}", bytes.len()));
    project_save_log(format_args!("fsync complete"));

    if path.exists() {
        if let Err(error) = fs::copy(path, &backup_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(ProjectError::Io(error));
        }
        project_save_log(format_args!("backup written: {}", backup_path.display()));
    }

    match replace_project_file(&tmp_path, path) {
        Ok(()) => {
            project_save_log(format_args!("atomic rename complete: {}", path.display()));
            Ok(ProjectSaveReport { offline_media })
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp_path);
            project_save_log(format_args!("save failed: {error}"));
            Err(ProjectError::Io(error))
        }
    }
}

/// Move a finished temp file over `path`. `rename` replaces the target
/// atomically on macOS and Linux, so the target is never removed first: a
/// failed rename leaves the previous project in place instead of no project
/// at all. Windows can refuse to replace a file that is open elsewhere, so only
/// there does a failed rename remove the target and try once more.
fn replace_project_file(tmp_path: &Path, path: &Path) -> std::io::Result<()> {
    match fs::rename(tmp_path, path) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(_) if path.exists() => {
            let _ = fs::remove_file(path);
            fs::rename(tmp_path, path)
        }
        Err(error) => Err(error),
    }
}

/// Load a project from disk, optionally allowing old versions.
pub fn load_project(
    path: &Path,
    allow_old_version: bool,
) -> Result<FutureboardProject, ProjectError> {
    project_load_log(format_args!(
        "opening: {} (allow_old_version={})",
        path.display(),
        allow_old_version
    ));
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

/// Suffix of every autosave file: `<name>.autosave.fbproj`.
pub const AUTOSAVE_FILE_SUFFIX: &str = ".autosave.fbproj";

/// Autosave of a saved project, written next to it: `<stem>.autosave.fbproj`.
pub fn autosave_path_for_project(project_file: &Path) -> PathBuf {
    let stem = project_file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("project");
    project_file.with_file_name(format!("{stem}{AUTOSAVE_FILE_SUFFIX}"))
}

/// Folder holding the autosaves of untitled sessions.
pub fn untitled_autosave_dir(app_data: &Path) -> PathBuf {
    app_data.join("Autosaves")
}

/// Autosave of an untitled session, named by its session id.
pub fn untitled_autosave_path(app_data: &Path, session_id: &str) -> PathBuf {
    untitled_autosave_dir(app_data).join(format!("{session_id}{AUTOSAVE_FILE_SUFFIX}"))
}

pub fn is_autosave_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(AUTOSAVE_FILE_SUFFIX))
}

/// Whether `path` is an untitled session's autosave. Opening one recovers an
/// untitled session: it must never be bound as the project's own file.
pub fn is_untitled_autosave_path(path: &Path, app_data: &Path) -> bool {
    is_autosave_path(path) && path.parent() == Some(untitled_autosave_dir(app_data).as_path())
}

/// Read and fully validate a project file, returning only its identity.
pub fn read_project_identity(path: &Path) -> Result<ProjectIdentity, ProjectError> {
    decode_project_identity(&fs::read(path)?)
}

/// The autosave to offer when opening `project_file`, whose decoded id and
/// modification time are given: it must pass the header and checksum checks,
/// belong to the same project and be strictly newer. `None` otherwise, and
/// always for a file that is itself an autosave or a foreign import.
pub fn newer_autosave_for(
    project_file: &Path,
    project_id: &str,
    saved_modified_at: u64,
) -> Option<PathBuf> {
    if is_autosave_path(project_file) || super::import::is_import_path(project_file) {
        return None;
    }
    let autosave = autosave_path_for_project(project_file);
    if !autosave.is_file() {
        return None;
    }
    let identity = read_project_identity(&autosave).ok()?;
    (identity.id == project_id && identity.modified_at > saved_modified_at).then_some(autosave)
}

/// Newest intact autosave an untitled session left in `dir`. Untitled
/// autosaves are named by a per-session id, so no project open can find them
/// by id; this is how they are offered back.
pub fn newest_untitled_autosave(dir: &Path) -> Option<(PathBuf, ProjectIdentity)> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_autosave_path(path) && path.is_file())
        .filter_map(|path| {
            let identity = read_project_identity(&path).ok()?;
            Some((path, identity))
        })
        .max_by_key(|(_, identity)| identity.modified_at)
}

/// Delete an autosave and what its saves leave next to it: the backup, the
/// shared temp name older builds used, and any per-job temp file a crash
/// left behind. Best effort; a file that is already gone is not an error.
/// Waits for a write in progress, so it never pulls a temp file out from under
/// a running save.
pub fn remove_autosave_files(autosave: &Path) {
    let _write_guard = PROJECT_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for path in [
        autosave.to_path_buf(),
        project_backup_path(autosave),
        project_temp_path(autosave),
    ] {
        let _ = fs::remove_file(path);
    }
    let (Some(dir), Some(name)) = (
        autosave.parent(),
        autosave.file_name().and_then(|name| name.to_str()),
    ) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{name}.");
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(job) = file_name
            .to_str()
            .and_then(|file| file.strip_prefix(&prefix))
            .and_then(|rest| rest.strip_suffix(".tmp"))
        else {
            continue;
        };
        let is_job_temp = job.split_once('-').is_some_and(|(pid, n)| {
            !pid.is_empty()
                && !n.is_empty()
                && pid.bytes().all(|b| b.is_ascii_digit())
                && n.bytes().all(|b| b.is_ascii_digit())
        });
        if is_job_temp {
            let _ = fs::remove_file(entry.path());
        }
    }
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

/// Make every clip's media reference portable before a save: copy external
/// audio into `Assets/Audio`, rewrite each `source_path` to the project-relative
/// location and rebuild `project.assets` with one record per referenced asset.
///
/// Asset ids (`asset_id`, the timeline's `file_id`) are never rewritten. They
/// are identities, not locations: the ARA audio-source persistentID and the
/// peak-cache key are both derived from them, so an id that changed on save
/// orphaned the ARA document and the peaks on the next open.
///
/// Media that cannot be found is kept as a reference (relative when it lies
/// under the project folder, otherwise absolute) with a record that has no
/// fingerprint, and is returned so the caller can say so. It never fails the
/// save.
///
/// `project.assets` may carry the records of the last load or save. Their
/// fingerprints spare re-hashing unchanged files, and their format metadata
/// (for example a DAW import's frame counts) is kept where this save has
/// nothing newer.
fn prepare_portable_assets(
    project: &mut FutureboardProject,
    project_file: &Path,
) -> Result<Vec<PathBuf>, ProjectError> {
    let Some(project_root) = project_file.parent() else {
        return Ok(Vec::new());
    };
    let layout = ProjectFolderLayout::from_root(project_root.to_path_buf());
    layout.ensure_dirs()?;

    let mut copied: Vec<(PathBuf, String)> = Vec::new();
    let mut assets: Vec<ProjectAsset> = Vec::new();
    let mut offline: Vec<PathBuf> = Vec::new();

    let previous: HashMap<String, ProjectAsset> = project
        .assets
        .iter()
        .map(|asset| (asset.id.clone(), asset.clone()))
        .collect();

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
            let known_format = ClipSourceFormat::of(&clip.stretch);
            if let ClipSource::Rauf {
                source_path,
                metadata_path,
                ..
            } = &mut clip.source
            {
                let source_abs = resolve_source_for_save(source_path, project_root);
                if !source_abs.exists() {
                    *source_path = offline_reference(&source_abs, project_root);
                    note_offline(&mut offline, source_abs);
                } else if let Some(relative) = path_relative_to_project(&source_abs, project_root) {
                    *source_path = PathBuf::from(path_to_project_string(&relative));
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
            let asset_id = asset_id.clone();
            let previous_record = previous.get(&asset_id);

            let source_abs = resolve_source_for_save(source_path, project_root);
            if !source_abs.exists() {
                *source_path = offline_reference(&source_abs, project_root);
                push_asset_record(
                    &mut assets,
                    offline_asset_record(
                        &asset_id,
                        &source_abs,
                        project_root,
                        known_format,
                        previous_record,
                    ),
                );
                note_offline(&mut offline, source_abs);
                continue;
            }

            if let Some(relative) = path_relative_to_project(&source_abs, project_root) {
                let relative_string = path_to_project_string(&relative);
                // Carry a known fingerprint forward while the file still has
                // the length it was hashed at; only hash files never
                // fingerprinted (or visibly changed since).
                let fingerprint = prev_fp_by_rel
                    .get(&relative_string)
                    .copied()
                    .filter(|fp| file_len_is(&source_abs, fp.len))
                    .or_else(|| audio_fingerprint(&source_abs));
                if let Some(fp) = fingerprint {
                    content_index
                        .entry(fp)
                        .or_insert_with(|| relative_string.clone());
                }
                *source_path = PathBuf::from(&relative_string);
                push_asset_record(
                    &mut assets,
                    with_known_metadata(
                        asset_record(
                            asset_id.clone(),
                            &source_abs,
                            relative_string,
                            None,
                            fingerprint,
                            None,
                            None,
                            None,
                            None,
                        )?,
                        known_format,
                        previous_record,
                    ),
                );
                continue;
            }

            // External source. Reuse an identical-content copy already in the
            // project before falling back to path-equality dedup within this
            // save. Either way only the reference moves; the clip keeps its id.
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
                    push_asset_record(
                        &mut assets,
                        with_known_metadata(
                            asset_record(
                                asset_id.clone(),
                                &project_root.join(existing_rel),
                                existing_rel.clone(),
                                Some(source_abs.clone()),
                                Some(fp),
                                None,
                                None,
                                None,
                                None,
                            )?,
                            known_format,
                            previous_record,
                        ),
                    );
                    continue;
                }
            }

            if let Some((_, relative_string)) = copied
                .iter()
                .find(|(known_source, _)| same_source(known_source, &source_abs))
            {
                let relative_string = relative_string.clone();
                *source_path = PathBuf::from(&relative_string);
                push_asset_record(
                    &mut assets,
                    with_known_metadata(
                        asset_record(
                            asset_id.clone(),
                            &project_root.join(&relative_string),
                            relative_string,
                            Some(source_abs.clone()),
                            fingerprint,
                            None,
                            None,
                            None,
                            None,
                        )?,
                        known_format,
                        previous_record,
                    ),
                );
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
            copied.push((source_abs.clone(), relative_string.clone()));
            // Prefer the fingerprint of the source we just read; fall back to the
            // freshly-written copy.
            let dest_fingerprint = fingerprint.or_else(|| audio_fingerprint(&dest));
            if let Some(fp) = dest_fingerprint {
                content_index.insert(fp, relative_string.clone());
            }
            push_asset_record(
                &mut assets,
                with_known_metadata(
                    asset_record(
                        asset_id.clone(),
                        &dest,
                        relative_string,
                        Some(source_abs),
                        dest_fingerprint,
                        None,
                        None,
                        None,
                        None,
                    )?,
                    known_format,
                    previous_record,
                ),
            );
        }
    }

    // Exactly the assets this save references: a record for media no clip
    // uses any more would only schedule a waveform job on the next open.
    project.assets = assets;
    Ok(offline)
}

/// What an audio clip already knows about its file, from a decode in this or
/// an earlier session. Recorded in the asset so a reopened clip can show its
/// source duration before (or without) decoding the file again.
#[derive(Debug, Clone, Copy)]
struct ClipSourceFormat {
    sample_rate: u32,
    frames: u64,
}

impl ClipSourceFormat {
    fn of(stretch: &AudioClipStretchState) -> Option<Self> {
        (stretch.original_sample_rate > 0 && stretch.original_duration_samples > 0).then(|| Self {
            sample_rate: stretch.original_sample_rate,
            frames: stretch.original_duration_samples,
        })
    }
}

/// Fill what this save could not measure from the clip's decoded format and
/// from the record the last load or save wrote for the same id. A previous
/// record whose fingerprint no longer matches describes other bytes and is
/// ignored.
fn with_known_metadata(
    mut record: ProjectAsset,
    format: Option<ClipSourceFormat>,
    previous: Option<&ProjectAsset>,
) -> ProjectAsset {
    if let Some(format) = format {
        record.sample_rate = record.sample_rate.or(Some(format.sample_rate));
        record.duration_samples = record.duration_samples.or(Some(format.frames));
        record.duration_secs = record
            .duration_secs
            .or(Some(format.frames as f64 / format.sample_rate as f64));
    }
    let same_bytes = |prev: &&ProjectAsset| match (
        record.source_fingerprint.as_deref(),
        prev.source_fingerprint.as_deref(),
    ) {
        (Some(now), Some(before)) => now == before,
        _ => true,
    };
    if let Some(prev) = previous.filter(same_bytes) {
        record.duration_secs = record.duration_secs.or(prev.duration_secs);
        record.sample_rate = record.sample_rate.or(prev.sample_rate);
        record.channels = record.channels.or(prev.channels);
        record.duration_samples = record.duration_samples.or(prev.duration_samples);
    }
    record
}

/// Record for media that was offline during the save: no fingerprint (the
/// bytes could not be read), the reference as the clip keeps it, and any
/// format known from before.
fn offline_asset_record(
    id: &str,
    source_abs: &Path,
    project_root: &Path,
    format: Option<ClipSourceFormat>,
    previous: Option<&ProjectAsset>,
) -> ProjectAsset {
    let relative_path = offline_project_relative(source_abs, project_root);
    let absolute_path = if relative_path.is_some() {
        previous.and_then(|prev| prev.absolute_path.clone())
    } else {
        Some(source_abs.to_path_buf())
    };
    let record = ProjectAsset {
        id: id.to_string(),
        original_filename: source_abs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "audio".to_string()),
        relative_path,
        absolute_path,
        duration_secs: None,
        sample_rate: None,
        channels: None,
        source_fingerprint: None,
        waveform_peak_relative_path: Some(
            crate::components::timeline::waveform_peak_file::waveform_peak_relative_path_for_asset(
                id,
            ),
        ),
        duration_samples: None,
    };
    with_known_metadata(record, format, previous)
}

/// One record per asset id. Clips sharing an asset merge what they know.
fn push_asset_record(assets: &mut Vec<ProjectAsset>, record: ProjectAsset) {
    let Some(existing) = assets.iter_mut().find(|asset| asset.id == record.id) else {
        assets.push(record);
        return;
    };
    existing.duration_secs = existing.duration_secs.or(record.duration_secs);
    existing.sample_rate = existing.sample_rate.or(record.sample_rate);
    existing.channels = existing.channels.or(record.channels);
    existing.duration_samples = existing.duration_samples.or(record.duration_samples);
    existing.source_fingerprint = existing
        .source_fingerprint
        .take()
        .or(record.source_fingerprint);
}

fn note_offline(offline: &mut Vec<PathBuf>, path: PathBuf) {
    if !offline.contains(&path) {
        offline.push(path);
    }
}

/// The reference an offline source keeps: project-relative when it lies under
/// the project folder, so the project stays portable, otherwise absolute.
fn offline_reference(source_abs: &Path, project_root: &Path) -> PathBuf {
    offline_project_relative(source_abs, project_root)
        .map(PathBuf::from)
        .unwrap_or_else(|| source_abs.to_path_buf())
}

/// Project-relative form of a path that may not exist. `path_relative_to_project`
/// canonicalizes, which a missing file cannot do, so this compares the paths
/// as written, then against the canonical project root.
fn offline_project_relative(source_abs: &Path, project_root: &Path) -> Option<String> {
    if let Ok(relative) = source_abs.strip_prefix(project_root) {
        return Some(path_to_project_string(relative));
    }
    let root = fs::canonicalize(project_root).ok()?;
    source_abs
        .strip_prefix(root)
        .ok()
        .map(path_to_project_string)
}

fn file_len_is(path: &Path, len: u64) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.len() == len)
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

#[cfg(test)]
thread_local! {
    /// Full-file hashes computed on this thread, so tests can prove a save
    /// carried a fingerprint forward instead of re-reading the file.
    static FINGERPRINTS_COMPUTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Stream `path` through CRC32 without loading it fully into memory. Returns
/// `None` if the file cannot be read (caller falls back to path-equality dedup).
fn audio_fingerprint(path: &Path) -> Option<AudioFingerprint> {
    #[cfg(test)]
    FINGERPRINTS_COMPUTED.with(|count| count.set(count.get() + 1));
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
        ClipSource, FutureboardProject, ProjectAsset, ProjectClip, ProjectSession, ProjectTrack,
        ProjectTrackType, TrackRouting,
        format::{PROJECT_HEADER_SIZE, ProjectError, encode_project},
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
    fn legacy_backup_preserves_original_bytes_and_is_not_overwritten() {
        let dir = temp_dir("legacy-backup");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Old.fbproj");
        fs::write(&path, b"legacy-project-bytes").unwrap();

        let backup = backup_legacy_project(&path, 7).unwrap();
        assert_eq!(backup, legacy_project_backup_path(&path, 7));
        assert_eq!(fs::read(&backup).unwrap(), b"legacy-project-bytes");

        fs::write(&path, b"mutated-current-project").unwrap();
        assert_eq!(backup_legacy_project(&path, 7).unwrap(), backup);
        assert_eq!(fs::read(&backup).unwrap(), b"legacy-project-bytes");

        let _ = fs::remove_dir_all(dir);
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
            asset_id: loaded_id,
            source_path: Some(loaded_path),
        } = &loaded.tracks[0].clips[0].source
        else {
            panic!("expected loaded audio clip source");
        };
        assert_eq!(loaded_path, &copied);
        // Only the location moves into the project; the asset id is an
        // identity (ARA persistentID, peak-cache key) and stays as it was.
        let original_id = source.to_string_lossy().into_owned();
        assert_eq!(loaded_id, &original_id);
        assert_eq!(loaded.assets.len(), 1);
        assert_eq!(loaded.assets[0].id, original_id);
        assert_eq!(
            loaded.assets[0].relative_path.as_deref(),
            Some("Assets/Audio/loop.wav")
        );
        assert_eq!(loaded.assets[0].absolute_path.as_ref(), Some(&source));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(external);
    }

    fn audio_clip(id: &str, source: &Path) -> ProjectClip {
        audio_clip_with_asset(id, &source.to_string_lossy(), source)
    }

    /// A clip whose asset id is not its path, like every clip of a project
    /// that was saved and reopened (ids are minted once, then kept).
    fn audio_clip_with_asset(id: &str, asset_id: &str, source: &Path) -> ProjectClip {
        ProjectClip {
            id: id.to_string(),
            name: "loop".to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            offset_beats: 0.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Audio {
                asset_id: asset_id.to_string(),
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
        project.tracks.push(audio_track(
            "t1",
            vec![audio_clip_with_asset("c1", asset_id, &dest)],
        ));
        let project_file = root.join("PeakA.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        assert_eq!(loaded.assets[0].id, asset_id);
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
        project.tracks.push(audio_track(
            "t1",
            vec![audio_clip_with_asset("c1", asset_id, &audio_path)],
        ));
        let project_file = root.join("PeakB.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        // The carried record keeps what the earlier save knew about the file.
        assert_eq!(loaded.assets[0].id, asset_id);
        assert_eq!(loaded.assets[0].duration_samples, Some(48_000));
        assert_eq!(loaded.assets[0].channels, Some(2));
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
        use crate::components::timeline::waveform_peak_file::{PeakFileError, read_peak_file};

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

    fn clip_source(project: &FutureboardProject, clip: usize) -> (String, PathBuf) {
        match &project.tracks[0].clips[clip].source {
            ClipSource::Audio {
                asset_id,
                source_path: Some(path),
            } => (asset_id.clone(), path.clone()),
            other => panic!("expected an audio clip, got {other:?}"),
        }
    }

    /// The asset id is the ARA audio-source persistentID and the peak-cache
    /// key. A save used to rewrite it to the project-relative path, so the ARA
    /// document saved alongside no longer matched on reopen.
    #[test]
    fn asset_ids_survive_save_reopen_and_a_second_save() {
        let root = temp_dir("stable-ids");
        let ext = temp_dir("stable-ids-ext");
        fs::create_dir_all(&ext).unwrap();
        let source = ext.join("take.wav");
        fs::write(&source, b"stable id bytes").unwrap();
        let live_id = source.to_string_lossy().into_owned();

        let mut project = FutureboardProject::new("Stable");
        project
            .tracks
            .push(audio_track("t1", vec![audio_clip("c1", &source)]));
        let project_file = root.join("Stable.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let mut reopened = load_project_strict(&project_file).unwrap();
        let (id, path) = clip_source(&reopened, 0);
        assert_eq!(id, live_id);
        assert_eq!(path, root.join("Assets").join("Audio").join("take.wav"));

        save_project(&mut reopened, &project_file).unwrap();
        let again = load_project_strict(&project_file).unwrap();
        assert_eq!(clip_source(&again, 0).0, live_id);
        assert_eq!(again.assets.len(), 1);
        assert_eq!(again.assets[0].id, live_id);
        assert_eq!(audio_files_in(&root), vec!["take.wav".to_string()]);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext);
    }

    /// One unplugged drive used to fail the whole save (and every autosave),
    /// so no other edit reached the file. The save now goes through, keeps the
    /// reference and reports the media as offline.
    #[test]
    fn a_missing_source_is_kept_as_a_reference_and_reported() {
        let root = temp_dir("offline");
        let ext = temp_dir("offline-ext");
        fs::create_dir_all(&ext).unwrap();
        let present = ext.join("present.wav");
        fs::write(&present, b"present bytes").unwrap();
        let missing_external = ext.join("unplugged.wav");
        let missing_inside = root.join("Assets").join("Audio").join("deleted.wav");

        let mut project = FutureboardProject::new("Offline");
        project.tracks.push(audio_track(
            "t1",
            vec![
                audio_clip("c-present", &present),
                audio_clip_with_asset("c-external", "asset-external", &missing_external),
                audio_clip_with_asset("c-inside", "asset-inside", &missing_inside),
            ],
        ));
        let project_file = root.join("Offline.fbproj");
        let report = save_project_with_report(&mut project, &project_file).unwrap();
        assert_eq!(
            report.offline_media,
            vec![missing_external.clone(), missing_inside.clone()]
        );

        // The written file is complete: the present clip was still copied in.
        let loaded = load_project_strict(&project_file).unwrap();
        assert_eq!(loaded.tracks[0].clips.len(), 3);
        assert_eq!(
            clip_source(&loaded, 0).1,
            root.join("Assets").join("Audio").join("present.wav")
        );
        // Outside the project: kept absolute. Inside: kept project-relative,
        // so it resolves under the (possibly moved) project folder.
        assert_eq!(
            clip_source(&loaded, 1),
            ("asset-external".to_string(), missing_external.clone())
        );
        assert_eq!(
            clip_source(&loaded, 2),
            ("asset-inside".to_string(), missing_inside.clone())
        );
        let external = loaded
            .assets
            .iter()
            .find(|asset| asset.id == "asset-external")
            .expect("offline asset recorded");
        assert_eq!(external.source_fingerprint, None);
        assert_eq!(external.relative_path, None);
        assert_eq!(external.absolute_path.as_ref(), Some(&missing_external));
        let inside = loaded
            .assets
            .iter()
            .find(|asset| asset.id == "asset-inside")
            .expect("offline asset recorded");
        assert_eq!(
            inside.relative_path.as_deref(),
            Some("Assets/Audio/deleted.wav")
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ext);
    }

    /// Asset records from the last load or save ride along with the next
    /// snapshot, so an unchanged file is not hashed on every save — which is
    /// what made a background save long enough to race edits and other saves.
    #[test]
    fn carried_asset_records_spare_rehashing_unchanged_files() {
        let root = temp_dir("carry-fp");
        let audio = root.join("Assets").join("Audio").join("loop.wav");
        fs::create_dir_all(audio.parent().unwrap()).unwrap();
        fs::write(&audio, b"inside the project already").unwrap();
        let tracks = vec![audio_track(
            "t1",
            vec![audio_clip_with_asset("c1", "Assets/Audio/loop.wav", &audio)],
        )];
        let project_file = root.join("Carry.fbproj");

        let hashed = || FINGERPRINTS_COMPUTED.with(|count| count.get());
        let mut first = FutureboardProject::new("Carry");
        first.tracks = tracks.clone();
        let before = hashed();
        save_project(&mut first, &project_file).unwrap();
        assert_eq!(hashed() - before, 1, "the first save fingerprints the file");

        // The next snapshot is built fresh from the timeline, plus the records
        // the last save returned.
        let mut second = FutureboardProject::new("Carry");
        second.tracks = tracks.clone();
        second.assets = first.assets.clone();
        let before = hashed();
        save_project(&mut second, &project_file).unwrap();
        assert_eq!(hashed() - before, 0, "an unchanged file is not re-hashed");
        assert_eq!(
            second.assets[0].source_fingerprint,
            first.assets[0].source_fingerprint
        );

        // A file whose length changed is hashed again rather than trusted.
        fs::write(&audio, b"rewritten with different content").unwrap();
        let mut third = FutureboardProject::new("Carry");
        third.tracks = tracks;
        third.assets = second.assets.clone();
        let before = hashed();
        save_project(&mut third, &project_file).unwrap();
        assert_eq!(hashed() - before, 1);
        assert_ne!(
            third.assets[0].source_fingerprint,
            first.assets[0].source_fingerprint
        );

        let _ = fs::remove_dir_all(root);
    }

    /// A DAW import knows each file's format from the archive. That used to be
    /// dropped by the first save, which wrote every record without it.
    #[test]
    fn imported_asset_metadata_survives_a_save() {
        let root = temp_dir("import-meta");
        let audio = root.join("Audio").join("kick.wav");
        fs::create_dir_all(audio.parent().unwrap()).unwrap();
        fs::write(&audio, b"kick").unwrap();

        let mut project = FutureboardProject::new("Imported");
        project.assets.push(ProjectAsset {
            id: "cubase-asset-1".to_string(),
            original_filename: "kick.wav".to_string(),
            relative_path: None,
            absolute_path: Some(audio.clone()),
            duration_secs: Some(2.0),
            sample_rate: Some(44_100),
            channels: Some(2),
            source_fingerprint: None,
            waveform_peak_relative_path: None,
            duration_samples: Some(88_200),
        });
        project.tracks.push(audio_track(
            "t1",
            vec![audio_clip_with_asset("c1", "cubase-asset-1", &audio)],
        ));
        let project_file = root.join("Imported.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        let asset = &loaded.assets[0];
        assert_eq!(asset.id, "cubase-asset-1");
        assert_eq!(asset.relative_path.as_deref(), Some("Audio/kick.wav"));
        assert_eq!(asset.sample_rate, Some(44_100));
        assert_eq!(asset.channels, Some(2));
        assert_eq!(asset.duration_samples, Some(88_200));
        assert_eq!(asset.duration_secs, Some(2.0));

        let _ = fs::remove_dir_all(root);
    }

    /// A clip that has been decoded records its format in the asset, so a
    /// reopened clip knows its source duration without decoding again.
    #[test]
    fn a_decoded_clip_records_its_format_in_the_asset() {
        let root = temp_dir("clip-format");
        let audio = root.join("Assets").join("Audio").join("vox.wav");
        fs::create_dir_all(audio.parent().unwrap()).unwrap();
        fs::write(&audio, b"vox").unwrap();
        let mut clip = audio_clip_with_asset("c1", "Assets/Audio/vox.wav", &audio);
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.original_duration_samples = 96_000;

        let mut project = FutureboardProject::new("Format");
        project.tracks.push(audio_track("t1", vec![clip]));
        let project_file = root.join("Format.fbproj");
        save_project(&mut project, &project_file).unwrap();

        let loaded = load_project_strict(&project_file).unwrap();
        assert_eq!(loaded.assets[0].sample_rate, Some(48_000));
        assert_eq!(loaded.assets[0].duration_samples, Some(96_000));
        assert_eq!(loaded.assets[0].duration_secs, Some(2.0));

        let _ = fs::remove_dir_all(root);
    }

    /// Two saves of one project running at once (a quick double save, or a
    /// save and a save-on-close) must never leave the project file missing.
    /// They used to share one temp path and delete the target before renaming,
    /// so a lost race could remove the `.fbproj` entirely.
    #[test]
    fn concurrent_saves_never_leave_the_project_missing() {
        let root = temp_dir("concurrent-save");
        fs::create_dir_all(&root).unwrap();
        let project_file = root.join("Race.fbproj");
        save_project(&mut FutureboardProject::new("Race 0"), &project_file).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let stop = stop.clone();
            let project_file = project_file.clone();
            std::thread::spawn(move || {
                let mut missing = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if !project_file.exists() {
                        missing += 1;
                    }
                }
                missing
            })
        };
        let writers: Vec<_> = (0..4)
            .map(|writer| {
                let project_file = project_file.clone();
                std::thread::spawn(move || {
                    for round in 0..8 {
                        let mut project =
                            FutureboardProject::new(&format!("Race {writer}-{round}"));
                        save_project(&mut project, &project_file).expect("save");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(watcher.join().unwrap(), 0, "the project file went missing");

        let loaded = load_project_strict(&project_file).expect("a complete project");
        assert!(loaded.name.starts_with("Race "));
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    fn write_project_file(path: &Path, id: &str, modified_at: u64) {
        let mut project = FutureboardProject::new("Recoverable");
        project.id = id.to_string();
        project.modified_at = modified_at;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, encode_project(&project)).unwrap();
    }

    #[test]
    fn project_identity_is_read_without_decoding_and_checks_the_checksum() {
        let dir = temp_dir("identity");
        let path = dir.join("Song.fbproj");
        write_project_file(&path, "id-1", 42);
        let identity = read_project_identity(&path).unwrap();
        assert_eq!(identity.id, "id-1");
        assert_eq!(identity.modified_at, 42);

        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 5;
        bytes[last] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            read_project_identity(&path),
            Err(ProjectError::ChecksumMismatch { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    /// Only an intact autosave of the same project that is newer than the
    /// saved file is offered on open.
    #[test]
    fn a_newer_autosave_of_the_same_project_is_offered() {
        let dir = temp_dir("autosave-offer");
        let project_file = dir.join("Song.fbproj");
        let autosave = autosave_path_for_project(&project_file);
        assert_eq!(autosave, dir.join("Song.autosave.fbproj"));
        write_project_file(&project_file, "song", 100);

        assert_eq!(
            newer_autosave_for(&project_file, "song", 100),
            None,
            "none on disk"
        );

        write_project_file(&autosave, "song", 160);
        assert_eq!(
            newer_autosave_for(&project_file, "song", 100),
            Some(autosave.clone())
        );
        // Older or equally old: the saved file already has everything.
        assert_eq!(newer_autosave_for(&project_file, "song", 160), None);
        assert_eq!(newer_autosave_for(&project_file, "song", 200), None);
        // Another project's autosave under the same name is never offered.
        assert_eq!(newer_autosave_for(&project_file, "other-song", 100), None);
        // Nor is a damaged one.
        fs::write(&autosave, b"not a project").unwrap();
        assert_eq!(newer_autosave_for(&project_file, "song", 100), None);
        // An autosave is never checked for an autosave of its own.
        assert_eq!(newer_autosave_for(&autosave, "song", 0), None);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_newest_intact_untitled_autosave_is_found() {
        let app_data = temp_dir("untitled-autosaves");
        let dir = untitled_autosave_dir(&app_data);
        assert_eq!(newest_untitled_autosave(&dir), None, "no folder yet");

        let older = untitled_autosave_path(&app_data, "session-a");
        let newer = untitled_autosave_path(&app_data, "session-b");
        let damaged = untitled_autosave_path(&app_data, "session-c");
        write_project_file(&older, "session-a", 10);
        write_project_file(&newer, "session-b", 20);
        fs::write(&damaged, b"half written").unwrap();
        fs::write(dir.join("notes.txt"), b"not an autosave").unwrap();

        let (path, identity) = newest_untitled_autosave(&dir).expect("an autosave");
        assert_eq!(path, newer);
        assert_eq!(identity.id, "session-b");
        assert!(is_untitled_autosave_path(&newer, &app_data));
        assert!(!is_untitled_autosave_path(
            &autosave_path_for_project(&app_data.join("Song").join("Song.fbproj")),
            &app_data
        ));

        let _ = fs::remove_dir_all(app_data);
    }

    #[test]
    fn removing_an_autosave_also_removes_its_backup_and_temp_files() {
        let dir = temp_dir("autosave-cleanup");
        let project_file = dir.join("Song.fbproj");
        let autosave = autosave_path_for_project(&project_file);
        write_project_file(&project_file, "song", 1);
        write_project_file(&autosave, "song", 2);
        let backup = project_backup_path(&autosave);
        let legacy_temp = project_temp_path(&autosave);
        let job_temp = PathBuf::from(format!("{}.123-4.tmp", autosave.display()));
        let unrelated = PathBuf::from(format!("{}.notes.tmp", autosave.display()));
        for path in [&backup, &legacy_temp, &job_temp, &unrelated] {
            fs::write(path, b"x").unwrap();
        }

        remove_autosave_files(&autosave);

        for gone in [&autosave, &backup, &legacy_temp, &job_temp] {
            assert!(!gone.exists(), "{} should be removed", gone.display());
        }
        assert!(project_file.exists(), "the project itself is never touched");
        assert!(unrelated.exists(), "only this autosave's own temp files go");

        let _ = fs::remove_dir_all(dir);
    }
}
