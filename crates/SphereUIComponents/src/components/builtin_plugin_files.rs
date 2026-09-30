//! User-content file service for built-in plugin editors.
//!
//! The CEF editor page is sandboxed — it browses and saves presets/IRs/NAM
//! captures through `__bridge` messages, and this module is the native side:
//! a deliberately dumb, sanitized byte store under
//! `Documents/Futureboard Studio/<Plugin>/{Presets, IRs, NAMs}`.
//!
//! File *formats* are owned by the editor page (the preset JSON shape lives in
//! `editorui/src/presetFiles.ts`); native never parses content, it only
//! enforces the directory sandbox: every path is `root/<kind dir>/<sanitized
//! leaf name>` — no separators, no `..`, extension whitelisted per kind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Which per-plugin user folder a bridge file request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinFileKind {
    Presets,
    Irs,
    Nams,
    Samples,
}

impl BuiltinFileKind {
    /// Wire string used by the bridge messages (`kind` field).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "presets" => Some(Self::Presets),
            "irs" => Some(Self::Irs),
            "nams" => Some(Self::Nams),
            "samples" => Some(Self::Samples),
            _ => None,
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::Presets => "presets",
            Self::Irs => "irs",
            Self::Nams => "nams",
            Self::Samples => "samples",
        }
    }

    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Presets => "Presets",
            Self::Irs => "IRs",
            Self::Nams => "NAMs",
            Self::Samples => "Samples",
        }
    }

    fn allowed_exts(self) -> &'static [&'static str] {
        match self {
            Self::Presets => &["json"],
            Self::Irs => &["wav", "aiff", "aif"],
            Self::Nams => &["nam"],
            Self::Samples => &["wav", "aiff", "aif", "mp3", "flac"],
        }
    }
}

/// One listed file, as sent to the editor page.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinFileEntry {
    pub file_name: String,
    pub size_bytes: u64,
    pub modified_ms: u64,
}

/// `Documents/Futureboard Studio/<display>/` for a built-in plugin. Pure path
/// math (no I/O) — pair with [`ensure_plugin_dirs`].
pub fn plugin_files_root(plugin_display: &str) -> PathBuf {
    crate::paths::FutureboardPaths::resolve()
        .user_root
        .join(plugin_display)
}

/// Create the three kind subfolders. Idempotent; errors are returned so the
/// caller can log them (a read-only Documents dir must not crash the editor).
pub fn ensure_plugin_dirs(root: &Path) -> io::Result<()> {
    for kind in [
        BuiltinFileKind::Presets,
        BuiltinFileKind::Irs,
        BuiltinFileKind::Nams,
        BuiltinFileKind::Samples,
    ] {
        fs::create_dir_all(root.join(kind.dir_name()))?;
    }
    Ok(())
}

/// Sandbox gate: accept only a bare leaf name with a whitelisted extension.
/// Returns the cleaned name, or `None` for anything that could escape the
/// kind directory or produce hidden/garbage files.
pub fn sanitize_file_name(kind: BuiltinFileKind, name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 200
        || name.starts_with('.')
        || name.contains(['/', '\\', ':'])
        || name.contains("..")
        || name.chars().any(|c| c.is_control())
    {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    let has_allowed_ext = kind
        .allowed_exts()
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")));
    if has_allowed_ext {
        Some(name.to_string())
    } else if kind == BuiltinFileKind::Presets && !lower.contains('.') {
        // Presets may arrive extension-less from a "save as" name box.
        Some(format!("{name}.json"))
    } else {
        None
    }
}

/// List `root/<kind>/` (extension-filtered, name-sorted). A missing directory
/// lists as empty rather than erroring — first run before any save.
pub fn list_files(root: &Path, kind: BuiltinFileKind) -> Vec<BuiltinFileEntry> {
    let dir = root.join(kind.dir_name());
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<BuiltinFileEntry> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            // Re-run the same gate we apply to requests, so nothing listable
            // is ever unreadable.
            sanitize_file_name(kind, &file_name)?;
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Some(BuiltinFileEntry {
                file_name,
                size_bytes: meta.len(),
                modified_ms,
            })
        })
        .collect();
    out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    out
}

/// Read one sanitized file as UTF-8 text (presets and `.nam` captures are
/// both JSON text).
pub fn read_file(root: &Path, kind: BuiltinFileKind, file_name: &str) -> io::Result<String> {
    let clean = sanitize_file_name(kind, file_name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;
    fs::read_to_string(root.join(kind.dir_name()).join(clean))
}

/// Read one sanitized file as raw bytes. IR files are binary `.wav`, so they
/// cannot go through [`read_file`]'s UTF-8 path.
pub fn read_file_bytes(root: &Path, kind: BuiltinFileKind, file_name: &str) -> io::Result<Vec<u8>> {
    let clean = sanitize_file_name(kind, file_name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;
    fs::read(root.join(kind.dir_name()).join(clean))
}

/// Write one sanitized file: temp-then-rename so a crash mid-write never
/// leaves a truncated preset behind.
pub fn write_file(
    root: &Path,
    kind: BuiltinFileKind,
    file_name: &str,
    content: &str,
) -> io::Result<String> {
    let clean = sanitize_file_name(kind, file_name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;
    let dir = root.join(kind.dir_name());
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{clean}.tmp-write"));
    fs::write(&tmp, content)?;
    let target = dir.join(&clean);
    // Windows rename fails onto an existing file — replace explicitly.
    if target.exists() {
        fs::remove_file(&target)?;
    }
    fs::rename(&tmp, &target)?;
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("fb-builtin-files-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn sanitize_rejects_escapes_and_enforces_extensions() {
        use BuiltinFileKind::*;
        assert_eq!(
            sanitize_file_name(Presets, "My Lead"),
            Some("My Lead.json".into())
        );
        assert_eq!(sanitize_file_name(Presets, "a.json"), Some("a.json".into()));
        assert_eq!(sanitize_file_name(Nams, "amp.nam"), Some("amp.nam".into()));
        assert_eq!(sanitize_file_name(Irs, "cab.wav"), Some("cab.wav".into()));
        assert_eq!(
            sanitize_file_name(Samples, "kick.flac"),
            Some("kick.flac".into())
        );
        for bad in [
            "",
            "   ",
            "../x.json",
            "a/b.json",
            "a\\b.json",
            "C:evil.json",
            ".hidden.json",
            "a..json",
            "nul\u{0}.json",
        ] {
            assert_eq!(sanitize_file_name(Presets, bad), None, "{bad:?}");
        }
        // Wrong extension for the kind.
        assert_eq!(sanitize_file_name(Nams, "amp.json"), None);
        assert_eq!(sanitize_file_name(Irs, "cab.nam"), None);
        assert_eq!(sanitize_file_name(Presets, "x.wav"), None);
    }

    #[test]
    fn write_list_read_round_trip_with_ext_filtering() {
        let root = temp_root("roundtrip");
        ensure_plugin_dirs(&root).unwrap();

        let name = write_file(&root, BuiltinFileKind::Presets, "Lead Tone", "{\"a\":1}").unwrap();
        assert_eq!(name, "Lead Tone.json");
        // Overwrite works (temp-then-rename onto existing).
        write_file(
            &root,
            BuiltinFileKind::Presets,
            "Lead Tone.json",
            "{\"a\":2}",
        )
        .unwrap();
        // A stray non-matching file must not be listed.
        fs::write(root.join("Presets").join("junk.txt"), "x").unwrap();

        let listed = list_files(&root, BuiltinFileKind::Presets);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].file_name, "Lead Tone.json");
        assert!(listed[0].size_bytes > 0);

        let content = read_file(&root, BuiltinFileKind::Presets, "Lead Tone.json").unwrap();
        assert_eq!(content, "{\"a\":2}");
        // Escapes refused at read time too.
        assert!(read_file(&root, BuiltinFileKind::Presets, "../Lead Tone.json").is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_directory_lists_empty() {
        let root = temp_root("missing");
        assert!(list_files(&root, BuiltinFileKind::Nams).is_empty());
    }
}
