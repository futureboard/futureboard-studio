//! Opening a project saved by an older build of Futureboard.
//!
//! The decoder reads every format version since v1 byte for byte, and the
//! older semantics are converted where each one is read: v33 input routing
//! becomes Audio Connections as the timeline is built, v54 folder members are
//! routed through their folder as the track is decoded, and so on. What this
//! module adds is the part around it that makes opening an old file a
//! migration rather than a silent reinterpretation:
//!
//! - the original is copied aside first (`<file>.v<N>.bak`, never overwritten),
//!   so the bytes the old build wrote survive the first save in the new format;
//! - the result says which version it came from and what changed on the way,
//!   in the user's terms, so the session can tell them and stay unsaved until
//!   they save the upgrade.
//!
//! Versions from [`MIN_MIGRATABLE_VERSION`] on are the supported range. An
//! older file still opens as before, but the report says it is older than the
//! range the migration is checked against.

use std::path::{Path, PathBuf};

use super::format::PROJECT_VERSION;

/// Oldest format version whose migration to the current one is supported
/// ([`super::format::MIN_SUPPORTED_VERSION`], v30).
pub const MIN_MIGRATABLE_VERSION: u32 = super::format::MIN_SUPPORTED_VERSION;

/// How an opened project was brought up to the current format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMigration {
    /// The version the file was saved in.
    pub from_version: u32,
    /// The version it is now held in (and will be saved in).
    pub to_version: u32,
    /// Where the original file was copied before it was opened, when the copy
    /// could be made.
    pub backup_path: Option<PathBuf>,
    /// Why the copy could not be made, when it could not.
    pub backup_error: Option<String>,
    /// What changed for the user between the two versions, oldest first.
    pub changes: Vec<&'static str>,
}

/// One version step that converts saved data or changes how it plays.
///
/// Steps that only add a field — which an older file then loads at the state
/// it was saved in (no takes, no MPE, the lanes expanded) — are not listed:
/// nothing about the project the user saved changes.
struct MigrationStep {
    /// The version whose decoder performs the conversion.
    version: u32,
    /// Oldest source version it applies to: data the conversion acts on did
    /// not exist before it.
    applies_from: u32,
    change: &'static str,
}

const MIGRATION_STEPS: &[MigrationStep] = &[
    MigrationStep {
        version: 27,
        applies_from: 24,
        change: "Chord and lyric cues were converted to Song Text events.",
    },
    MigrationStep {
        version: 34,
        applies_from: 1,
        change: "Track audio inputs were converted to Audio Connections. If an \
                 audio interface has changed since, reassign them in Audio \
                 Connections.",
    },
    MigrationStep {
        version: 35,
        applies_from: 1,
        change: "The Master output is now an Audio Connection, assigned on \
                 this first open; check it in Audio Connections.",
    },
    MigrationStep {
        version: 42,
        applies_from: 41,
        change: "ARA plug-ins bound to single clips are not carried over: \
                 add the ARA plug-in to the track again.",
    },
    MigrationStep {
        version: 55,
        applies_from: 30,
        change: "Tracks inside a folder now play through it, so the folder's \
                 fader, mute and inserts act on them.",
    },
];

/// What changes for a project saved in `from_version` when it is opened now.
pub fn migration_changes(from_version: u32) -> Vec<&'static str> {
    MIGRATION_STEPS
        .iter()
        .filter(|step| {
            from_version < step.version
                && from_version >= step.applies_from
                && step.version <= PROJECT_VERSION
        })
        .map(|step| step.change)
        .collect()
}

impl ProjectMigration {
    /// Copy `path` aside and describe the migration of a file saved in
    /// `from_version`. A copy that cannot be made does not stop the open:
    /// nothing is written to `path` until the user saves, and the report says
    /// the copy is missing so they can keep one themselves first.
    pub fn prepare(path: &Path, from_version: u32) -> Self {
        let (backup_path, backup_error) = match super::io::backup_legacy_project(path, from_version)
        {
            Ok(backup) => (Some(backup), None),
            Err(error) => (None, Some(error.technical_detail())),
        };
        Self {
            from_version,
            to_version: PROJECT_VERSION,
            backup_path,
            backup_error,
            changes: migration_changes(from_version),
        }
    }

    /// Whether the file is older than the range this migration is checked
    /// against.
    pub fn below_supported_range(&self) -> bool {
        self.from_version < MIN_MIGRATABLE_VERSION
    }

    /// The notice's main text: what happened and what the user has to do.
    pub fn message(&self) -> String {
        let mut message = format!(
            "This project was saved by an older version of Futureboard (format v{}) \
             and has been upgraded to v{}.",
            self.from_version, self.to_version
        );
        if !self.changes.is_empty() {
            message.push_str("\n\n");
            for change in &self.changes {
                message.push_str("• ");
                message.push_str(change);
                message.push('\n');
            }
            message.pop();
        }
        if self.below_supported_range() {
            message.push_str(&format!(
                "\n\nFiles older than v{MIN_MIGRATABLE_VERSION} are opened as well as \
                 possible; check the project before relying on it."
            ));
        }
        message.push_str(
            "\n\nThe project file is not changed until you save. Saving writes it \
             in the current format.",
        );
        message
    }

    /// The notice's secondary line: where the original went.
    pub fn detail(&self) -> String {
        match (&self.backup_path, &self.backup_error) {
            (Some(backup), _) => format!("Original kept as: {}", backup.display()),
            (None, Some(error)) => format!(
                "The original could not be copied aside ({error}). Keep a copy \
                 before saving if you may need it in the older version."
            ),
            (None, None) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_steps_a_version_crosses_are_listed() {
        assert!(migration_changes(PROJECT_VERSION).is_empty());
        // v55 → now: nothing that converts data.
        assert!(migration_changes(55).is_empty());
        // Folders existed from v30, so a v54 file crosses the folder change.
        assert_eq!(migration_changes(54).len(), 1);
        // v33 crosses inputs, master output and folders; not the v41 ARA step,
        // which only a v41 file had anything for.
        let from_v33 = migration_changes(33);
        assert_eq!(from_v33.len(), 3);
        assert!(!from_v33.iter().any(|change| change.contains("ARA")));
        assert!(migration_changes(41)
            .iter()
            .any(|change| change.contains("ARA")));
    }

    #[test]
    fn a_file_before_folders_does_not_hear_about_folder_routing() {
        assert!(!migration_changes(29)
            .iter()
            .any(|change| change.contains("folder")));
    }

    /// Every `.fbproj` under `FUTUREBOARD_MIGRATION_FIXTURES`, opened the way
    /// the app opens it, built into a session, saved in the current format and
    /// opened again — on copies; the originals are never opened for writing.
    /// Files from v30 on must come through with every track and clip; older
    /// ones are reported, not required. Run by hand against real projects:
    ///
    /// `FUTUREBOARD_MIGRATION_FIXTURES=<dir> cargo test -p sphere_ui_components
    /// --lib real_projects -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_projects_migrate_and_reopen_in_the_current_format() {
        use crate::components::timeline::timeline_state::TimelineState;
        use crate::project::{apply_to_timeline, format, io, FutureboardProject};

        let Some(root) = std::env::var_os("FUTUREBOARD_MIGRATION_FIXTURES") else {
            eprintln!("FUTUREBOARD_MIGRATION_FIXTURES not set; nothing to check");
            return;
        };
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "fbproj")
                    && !path.to_string_lossy().contains(".autosave")
                {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(Path::new(&root), &mut files);
        files.sort();

        let scratch = std::env::temp_dir().join(format!("fb-migrate-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let mut failures = Vec::new();
        for (index, original) in files.iter().enumerate() {
            let Ok(version) = io::validate_project_file(original) else {
                continue;
            };
            let dir = scratch.join(index.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let copy = dir.join(original.file_name().unwrap());
            std::fs::copy(original, &copy).unwrap();
            let name = original
                .strip_prefix(Path::new(&root))
                .unwrap_or(original)
                .display()
                .to_string();

            let outcome = (|| -> Result<String, String> {
                let project = io::load_project(&copy, true).map_err(|e| e.technical_detail())?;
                let migration = project.migration.clone();
                if version < PROJECT_VERSION {
                    let migration = migration.as_ref().ok_or("no migration report")?;
                    let backup = migration.backup_path.as_ref().ok_or("no backup")?;
                    if std::fs::read(backup).map_err(|e| e.to_string())?
                        != std::fs::read(original).map_err(|e| e.to_string())?
                    {
                        return Err("backup differs from the original".to_string());
                    }
                }
                let tracks = project.tracks.len();
                let clips: usize = project.tracks.iter().map(|t| t.clips.len()).sum();

                let mut timeline = TimelineState::default();
                let warnings = apply_to_timeline(&project, &mut timeline);
                let upgraded = FutureboardProject::from(&timeline);
                let bytes = format::encode_project(&upgraded);
                let reopened = format::decode_project(&bytes).map_err(|e| e.technical_detail())?;
                let reopened_clips: usize = reopened.tracks.iter().map(|t| t.clips.len()).sum();
                if reopened.tracks.len() != tracks || reopened_clips != clips {
                    return Err(format!(
                        "tracks {tracks}->{} clips {clips}->{reopened_clips}",
                        reopened.tracks.len()
                    ));
                }
                Ok(format!(
                    "tracks={tracks} clips={clips} changes={} load_warnings={}",
                    migration.map(|m| m.changes.len()).unwrap_or(0),
                    warnings.len()
                ))
            })();
            match outcome {
                Ok(summary) => eprintln!("[migrate] OK   v{version:<3} {name}: {summary}"),
                Err(error) => {
                    eprintln!("[migrate] FAIL v{version:<3} {name}: {error}");
                    if version >= MIN_MIGRATABLE_VERSION {
                        failures.push(format!("v{version} {name}: {error}"));
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(
            failures.is_empty(),
            "supported versions failed: {failures:#?}"
        );
    }

    #[test]
    fn preparing_keeps_the_original_and_says_where() {
        let dir = std::env::temp_dir().join(format!(
            "fb-migrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Old Song.fbproj");
        std::fs::write(&path, b"old bytes").unwrap();

        let migration = ProjectMigration::prepare(&path, 40);
        let backup = migration.backup_path.clone().expect("backup");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old bytes");
        assert!(migration.detail().contains("Old Song.fbproj.v40.bak"));
        assert!(migration.message().contains("v40"));
        assert!(!migration.below_supported_range());

        // A second open keeps the first copy: it is the old build's file.
        std::fs::write(&path, b"saved by this build").unwrap();
        let again = ProjectMigration::prepare(&path, 40);
        assert_eq!(
            std::fs::read(again.backup_path.unwrap()).unwrap(),
            b"old bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
