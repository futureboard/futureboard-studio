//! On-disk cache of analyses and edits, per audio file.
//!
//! Analysis of a three-minute take costs seconds; reopening it shouldn't.
//! Each entry is keyed by the file's fingerprint (path, size, modified time)
//! and starts with that fingerprint on its own line, so a file that changed
//! on disk misses instead of restoring notes that no longer match it — the
//! same layout the Android app's cache uses.
//!
//! The caller picks the root (Studio's cache folder, the desktop app's, the
//! plug-in's); nothing here knows about platforms. Not realtime.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::session::VoxDocument;

/// Bumped when the analysis changes so old entries miss.
pub const ANALYSIS_VERSION: u32 = 1;

pub struct TakeStore {
    root: PathBuf,
}

impl TakeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `v|path|size|modified-ms`, or None when the file can't be read.
    pub fn fingerprint(audio: &Path) -> Option<String> {
        let meta = fs::metadata(audio).ok()?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = audio.canonicalize().unwrap_or_else(|_| audio.to_path_buf());
        Some(format!(
            "v{ANALYSIS_VERSION}|{}|{}|{modified}",
            path.display(),
            meta.len()
        ))
    }

    /// FNV-1a: stable across runs and platforms, no dependency.
    fn key(fingerprint: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in fingerprint.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    fn entry(&self, kind: &str, audio: &Path) -> Option<(PathBuf, String)> {
        let fp = Self::fingerprint(audio)?;
        Some((
            self.root
                .join(kind)
                .join(format!("{}.json", Self::key(&fp))),
            fp,
        ))
    }

    fn read(&self, kind: &str, audio: &Path) -> Option<String> {
        let (file, fp) = self.entry(kind, audio)?;
        let text = fs::read_to_string(file).ok()?;
        let (head, body) = text.split_once('\n')?;
        (head == fp).then(|| body.to_string())
    }

    fn write(&self, kind: &str, audio: &Path, body: &str) -> std::io::Result<()> {
        let Some((file, fp)) = self.entry(kind, audio) else {
            return Err(std::io::Error::other("audio file is not readable"));
        };
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        // Write then rename, so a crash never leaves half an entry.
        let tmp = file.with_extension("tmp");
        fs::write(&tmp, format!("{fp}\n{body}"))?;
        fs::rename(tmp, file)
    }

    /// The cached analysis JSON (`CantoAnalysisResult`) for `audio`.
    pub fn read_analysis(&self, audio: &Path) -> Option<String> {
        self.read("analysis", audio)
    }

    pub fn write_analysis(&self, audio: &Path, json: &str) -> std::io::Result<()> {
        self.write("analysis", audio, json)
    }

    /// The edits last saved for `audio`.
    pub fn read_document(&self, audio: &Path) -> Option<VoxDocument> {
        VoxDocument::from_json(&self.read("sessions", audio)?)
    }

    pub fn write_document(&self, audio: &Path, doc: &VoxDocument) -> std::io::Result<()> {
        self.write("sessions", audio, &doc.to_json())
    }

    pub fn remove_document(&self, audio: &Path) {
        if let Some((file, _)) = self.entry("sessions", audio) {
            let _ = fs::remove_file(file);
        }
    }

    /// Bytes used by every entry.
    pub fn total_bytes(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| match e.metadata() {
                            Ok(m) if m.is_dir() => walk(&e.path()),
                            Ok(m) => m.len(),
                            Err(_) => 0,
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_follow_the_file_and_miss_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("voxtune-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("take.wav");
        fs::write(&audio, b"one").unwrap();
        let store = TakeStore::new(dir.join("cache"));

        assert!(store.read_analysis(&audio).is_none());
        store.write_analysis(&audio, "{\"ok\":true}").unwrap();
        assert_eq!(
            store.read_analysis(&audio).as_deref(),
            Some("{\"ok\":true}")
        );

        let doc = VoxDocument {
            key: 4,
            ..VoxDocument::default()
        };
        store.write_document(&audio, &doc).unwrap();
        assert_eq!(store.read_document(&audio), Some(doc));
        assert!(store.total_bytes() > 0);

        // A different size is a different fingerprint: both entries miss.
        fs::write(&audio, b"changed").unwrap();
        assert!(store.read_analysis(&audio).is_none());
        assert!(store.read_document(&audio).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
