//! On-disk waveform peak cache (`FBPEAKS1`) stored under
//! `<project>/Cache/Waveforms/`, one file per asset id (named by
//! [`waveform_peak_relative_path_for_asset`]).
//!
//! The cache key is the stored `(asset_id, algorithm version, source file
//! size, source mtime)`. A peak file is only reused when all four match the
//! current source file, so editing/replacing the source media invalidates it
//! and triggers background regeneration. Writes are atomic (temp file + rename)
//! so a crash mid-write never leaves a corrupt cache.

use std::io::Write;
use std::path::Path;

use super::waveform_cache::{
    WaveformLod, WaveformPeak, WaveformPreview, WAVEFORM_ALGORITHM_VERSION,
};

pub const PEAK_FILE_MAGIC: &[u8; 8] = b"FBPEAKS1";
/// v2 added the source `(size, mtime)` fingerprint to the header. Bumping this
/// invalidates every v1 file (they regenerate in the background).
pub const PEAK_FILE_VERSION: u32 = 2;

/// Identity of the source media a peak file was generated from. Editing or
/// replacing the source changes its size and/or mtime, which invalidates the
/// cached peaks. A `for_path` of `None` (file un-stattable) means "skip the
/// freshness check" rather than "discard the cache".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SourceFingerprint {
    pub size: u64,
    pub modified_nanos: u64,
}

impl SourceFingerprint {
    pub fn for_path(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let modified_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        Some(Self {
            size: meta.len(),
            modified_nanos,
        })
    }

    /// A zeroed fingerprint means "unknown" — written when the source couldn't
    /// be stat'd at generation time. Unknown fingerprints never trigger a
    /// freshness rejection.
    fn is_known(&self) -> bool {
        self.size != 0 || self.modified_nanos != 0
    }
}

#[derive(Debug)]
pub enum PeakFileError {
    Io(std::io::Error),
    Truncated {
        field: &'static str,
    },
    BadMagic,
    UnsupportedVersion(u32),
    AlgorithmMismatch {
        expected: u32,
        found: u32,
    },
    AssetMismatch {
        expected: String,
        found: String,
    },
    /// Source media changed since the peaks were generated (size/mtime differ).
    SourceChanged {
        expected: SourceFingerprint,
        found: SourceFingerprint,
    },
    InvalidPeakCount,
}

impl std::fmt::Display for PeakFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Truncated { field } => write!(f, "truncated peak file at {field}"),
            Self::BadMagic => write!(f, "invalid peak file magic"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported peak file version {v}"),
            Self::AlgorithmMismatch { expected, found } => {
                write!(
                    f,
                    "peak algorithm mismatch expected={expected} found={found}"
                )
            }
            Self::AssetMismatch { expected, found } => {
                write!(f, "peak asset mismatch expected={expected} found={found}")
            }
            Self::SourceChanged { expected, found } => write!(
                f,
                "source changed: peaks built for size={} mtime={} but file is size={} mtime={}",
                expected.size, expected.modified_nanos, found.size, found.modified_nanos
            ),
            Self::InvalidPeakCount => write!(f, "invalid peak count in peak file"),
        }
    }
}

impl From<std::io::Error> for PeakFileError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Longest peak file name written, in bytes. Leaves room under the common
/// 255-byte name limit for the `.tmp` suffix of the atomic write.
const MAX_PEAK_FILE_NAME_BYTES: usize = 200;
/// Bytes of the readable prefix kept in a hashed peak file name.
const HASHED_NAME_PREFIX_BYTES: usize = 48;

/// Project-relative path for an asset's peak cache file.
///
/// Asset ids are stable identities and can be absolute paths from any
/// platform (`/Users/…/very/deep/take.wav`, `C:\Samples\kick.wav`). The file
/// name is the id with its separators flattened when that is short and safe
/// on every file system; this is the name earlier builds wrote, so existing
/// caches keep working. Otherwise it is a sanitized, length-bounded prefix of
/// the id's last component plus a stable hash of the whole id
/// (`kick.wav~0123456789abcdef.peaks`). `~` never appears in a kept name, so
/// the two forms cannot collide. The peak file also stores the full id, and
/// reading checks it.
pub fn waveform_peak_relative_path_for_asset(asset_id: &str) -> String {
    format!("Cache/Waveforms/{}", peak_file_name_for_asset(asset_id))
}

/// The name earlier builds used for `asset_id`, when it differs from today's
/// and could exist on disk (short enough, no `:`), so an existing cache is
/// still found. `None` when there is nothing different to look for.
pub fn legacy_waveform_peak_relative_path_for_asset(asset_id: &str) -> Option<String> {
    let legacy = legacy_peak_file_name(asset_id);
    let current = peak_file_name_for_asset(asset_id);
    (legacy != current && legacy.len() <= 255 && !legacy.contains(':'))
        .then(|| format!("Cache/Waveforms/{legacy}"))
}

fn legacy_peak_file_name(asset_id: &str) -> String {
    format!("{}.peaks", asset_id.replace(['/', '\\'], "__"))
}

fn peak_file_name_for_asset(asset_id: &str) -> String {
    let legacy = legacy_peak_file_name(asset_id);
    if legacy.len() <= MAX_PEAK_FILE_NAME_BYTES && legacy.chars().all(is_kept_name_char) {
        return legacy;
    }
    let last_component = asset_id
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or("audio");
    let mut prefix = String::new();
    for c in last_component.chars() {
        let c = if is_kept_name_char(c) { c } else { '_' };
        if prefix.len() + c.len_utf8() > HASHED_NAME_PREFIX_BYTES {
            break;
        }
        prefix.push(c);
    }
    // No leading dot (a hidden file) and no empty prefix.
    let prefix = prefix.trim_start_matches('.');
    let prefix = if prefix.is_empty() { "audio" } else { prefix };
    format!("{prefix}~{:016x}.peaks", fnv1a_64(asset_id.as_bytes()))
}

/// Characters a kept (unhashed) peak file name may contain: portable on
/// Windows, macOS and Linux file systems. Excludes `~`, the hashed-name marker.
fn is_kept_name_char(c: char) -> bool {
    !c.is_control()
        && !matches!(
            c,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '~'
        )
}

/// FNV-1a, 64-bit: small and stable across builds and platforms (unlike the
/// std hasher), which a file name derived from it must be.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Write peaks to `path` atomically (temp file + rename), creating parent
/// directories as needed. `source` records the media file's `(size, mtime)`
/// so a later [`read_peak_file`] can detect a changed source; pass `None` when
/// the source can't be stat'd.
pub fn write_peak_file(
    path: &Path,
    asset_id: &str,
    preview: &WaveformPreview,
    source: Option<SourceFingerprint>,
) -> Result<usize, PeakFileError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let source = source.unwrap_or_default();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PEAK_FILE_MAGIC);
    bytes.extend_from_slice(&PEAK_FILE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&WAVEFORM_ALGORITHM_VERSION.to_le_bytes());
    let asset_bytes = asset_id.as_bytes();
    if asset_bytes.len() > u32::MAX as usize {
        return Err(PeakFileError::InvalidPeakCount);
    }
    bytes.extend_from_slice(&(asset_bytes.len() as u32).to_le_bytes());
    bytes.extend_from_slice(asset_bytes);
    bytes.extend_from_slice(&preview.sample_rate.to_le_bytes());
    bytes.extend_from_slice(&preview.channels.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&preview.total_frames.to_le_bytes());
    bytes.extend_from_slice(&preview.duration_seconds.to_le_bytes());
    bytes.extend_from_slice(&source.size.to_le_bytes());
    bytes.extend_from_slice(&source.modified_nanos.to_le_bytes());
    bytes.extend_from_slice(&(preview.lods.len() as u32).to_le_bytes());
    for lod in &preview.lods {
        if lod.peaks.len() > u32::MAX as usize {
            return Err(PeakFileError::InvalidPeakCount);
        }
        bytes.extend_from_slice(&(lod.samples_per_peak as u32).to_le_bytes());
        bytes.extend_from_slice(&(lod.peaks.len() as u32).to_le_bytes());
        for peak in &lod.peaks {
            bytes.extend_from_slice(&peak.min.to_le_bytes());
            bytes.extend_from_slice(&peak.max.to_le_bytes());
        }
    }
    let size = bytes.len();

    // Atomic publish: write to a sibling temp file, flush/fsync, then rename
    // over the target. `fs::rename` is atomic-replace on both Windows and Unix,
    // so a crash mid-write can never expose a half-written cache file.
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_os);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        let _ = file.sync_all(); // best-effort durability; ignore on FS without fsync
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(PeakFileError::Io(err));
    }
    Ok(size)
}

/// Load and validate a peak file. When `expected_asset_id` is set, the stored
/// asset id must match. When `expected_source` is set, the stored source
/// `(size, mtime)` must match the current source file, otherwise
/// [`PeakFileError::SourceChanged`] is returned so the caller can regenerate.
pub fn read_peak_file(
    path: &Path,
    expected_asset_id: Option<&str>,
    expected_source: Option<SourceFingerprint>,
) -> Result<WaveformPreview, PeakFileError> {
    let bytes = std::fs::read(path)?;
    read_peak_bytes(&bytes, expected_asset_id, expected_source)
}

fn read_peak_bytes(
    bytes: &[u8],
    expected_asset_id: Option<&str>,
    expected_source: Option<SourceFingerprint>,
) -> Result<WaveformPreview, PeakFileError> {
    let mut cursor = bytes;
    let mut take = |n: usize, field: &'static str| -> Result<&[u8], PeakFileError> {
        if cursor.len() < n {
            return Err(PeakFileError::Truncated { field });
        }
        let (head, tail) = cursor.split_at(n);
        cursor = tail;
        Ok(head)
    };

    let magic = take(8, "magic")?;
    if magic != PEAK_FILE_MAGIC {
        return Err(PeakFileError::BadMagic);
    }
    let version = u32::from_le_bytes(take(4, "version")?.try_into().unwrap());
    if version != PEAK_FILE_VERSION {
        return Err(PeakFileError::UnsupportedVersion(version));
    }
    let algo = u32::from_le_bytes(take(4, "algorithm")?.try_into().unwrap());
    if algo != WAVEFORM_ALGORITHM_VERSION {
        return Err(PeakFileError::AlgorithmMismatch {
            expected: WAVEFORM_ALGORITHM_VERSION,
            found: algo,
        });
    }
    let asset_len = u32::from_le_bytes(take(4, "asset_id_len")?.try_into().unwrap()) as usize;
    let asset_raw = take(asset_len, "asset_id")?;
    let asset_id = std::str::from_utf8(asset_raw)
        .map_err(|_| PeakFileError::Truncated {
            field: "asset_id_utf8",
        })?
        .to_string();
    if let Some(expected) = expected_asset_id {
        if asset_id != expected {
            return Err(PeakFileError::AssetMismatch {
                expected: expected.to_string(),
                found: asset_id,
            });
        }
    }
    let sample_rate = u32::from_le_bytes(take(4, "sample_rate")?.try_into().unwrap());
    let channels = u16::from_le_bytes(take(2, "channels")?.try_into().unwrap());
    let _pad = u16::from_le_bytes(take(2, "pad")?.try_into().unwrap());
    let total_frames = u64::from_le_bytes(take(8, "total_frames")?.try_into().unwrap());
    let duration_seconds = f64::from_le_bytes(take(8, "duration_seconds")?.try_into().unwrap());
    let source_size = u64::from_le_bytes(take(8, "source_size")?.try_into().unwrap());
    let source_modified_nanos =
        u64::from_le_bytes(take(8, "source_modified_nanos")?.try_into().unwrap());
    let stored_source = SourceFingerprint {
        size: source_size,
        modified_nanos: source_modified_nanos,
    };
    if let Some(expected) = expected_source {
        // Only reject when both fingerprints are known. An unknown stored
        // fingerprint (older write that couldn't stat) or unknown caller
        // fingerprint stays lenient so we don't drop otherwise-valid peaks.
        if expected.is_known() && stored_source.is_known() && stored_source != expected {
            return Err(PeakFileError::SourceChanged {
                expected,
                found: stored_source,
            });
        }
    }
    let lod_count = u32::from_le_bytes(take(4, "lod_count")?.try_into().unwrap()) as usize;

    let mut lods = Vec::with_capacity(lod_count);
    for _ in 0..lod_count {
        let samples_per_peak =
            u32::from_le_bytes(take(4, "samples_per_peak")?.try_into().unwrap()) as usize;
        let peak_count = u32::from_le_bytes(take(4, "peak_count")?.try_into().unwrap()) as usize;
        let mut peaks = Vec::with_capacity(peak_count);
        for _ in 0..peak_count {
            let min = f32::from_le_bytes(take(4, "peak_min")?.try_into().unwrap());
            let max = f32::from_le_bytes(take(4, "peak_max")?.try_into().unwrap());
            peaks.push(WaveformPeak { min, max });
        }
        lods.push(WaveformLod {
            samples_per_peak,
            peaks,
        });
    }

    Ok(WaveformPreview {
        sample_rate,
        channels,
        duration_seconds,
        total_frames,
        lods,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("fb_peak_{label}_{}_{}", std::process::id(), nanos))
    }

    fn sample_preview() -> WaveformPreview {
        WaveformPreview {
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
                        min: -0.25,
                        max: 0.75,
                    },
                ],
            }],
        }
    }

    #[test]
    fn peak_file_roundtrip() {
        let dir = temp_dir("roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.peaks");
        let preview = sample_preview();
        let asset_id = "Assets/Audio/loop.wav";
        let bytes = write_peak_file(&path, asset_id, &preview, None).unwrap();
        assert!(bytes > 64);
        let loaded = read_peak_file(&path, Some(asset_id), None).unwrap();
        assert_eq!(loaded.sample_rate, preview.sample_rate);
        assert_eq!(loaded.lods[0].peaks.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn peak_file_rejects_bad_magic() {
        let err = read_peak_bytes(b"NOTPEAK1", Some("a"), None).unwrap_err();
        assert!(matches!(err, PeakFileError::BadMagic));
    }

    #[test]
    fn matching_source_fingerprint_loads() {
        let dir = temp_dir("fp_match");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.peaks");
        let asset_id = "Assets/Audio/loop.wav";
        let fp = SourceFingerprint {
            size: 1234,
            modified_nanos: 5678,
        };
        write_peak_file(&path, asset_id, &sample_preview(), Some(fp)).unwrap();
        // Same fingerprint → cache is reused.
        assert!(read_peak_file(&path, Some(asset_id), Some(fp)).is_ok());
        // Unknown caller fingerprint → lenient (still loads).
        assert!(read_peak_file(&path, Some(asset_id), None).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn changed_source_size_or_mtime_invalidates() {
        let dir = temp_dir("fp_changed");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.peaks");
        let asset_id = "Assets/Audio/loop.wav";
        let original = SourceFingerprint {
            size: 1234,
            modified_nanos: 5678,
        };
        write_peak_file(&path, asset_id, &sample_preview(), Some(original)).unwrap();

        // Different size (file edited/replaced) → SourceChanged.
        let bigger = SourceFingerprint {
            size: 9999,
            modified_nanos: 5678,
        };
        assert!(matches!(
            read_peak_file(&path, Some(asset_id), Some(bigger)),
            Err(PeakFileError::SourceChanged { .. })
        ));

        // Same size, newer mtime (re-saved) → SourceChanged.
        let touched = SourceFingerprint {
            size: 1234,
            modified_nanos: 9999,
        };
        assert!(matches!(
            read_peak_file(&path, Some(asset_id), Some(touched)),
            Err(PeakFileError::SourceChanged { .. })
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_stored_fingerprint_stays_lenient() {
        // A write with `None` source stores a zeroed (unknown) fingerprint;
        // a later known caller fingerprint must NOT reject it.
        let dir = temp_dir("fp_unknown");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.peaks");
        let asset_id = "Assets/Audio/loop.wav";
        write_peak_file(&path, asset_id, &sample_preview(), None).unwrap();
        let known = SourceFingerprint {
            size: 1234,
            modified_nanos: 5678,
        };
        assert!(read_peak_file(&path, Some(asset_id), Some(known)).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn peak_relative_path_sanitizes_slashes() {
        assert_eq!(
            waveform_peak_relative_path_for_asset("Assets/Audio/kick.wav"),
            "Cache/Waveforms/Assets__Audio__kick.wav.peaks"
        );
        // Short safe ids keep the name earlier builds wrote: nothing to migrate.
        assert_eq!(
            legacy_waveform_peak_relative_path_for_asset("Assets/Audio/kick.wav"),
            None
        );
    }

    fn file_name(relative: &str) -> &str {
        relative
            .strip_prefix("Cache/Waveforms/")
            .expect("under the cache folder")
    }

    /// A Windows id used to become `C:__Samples__kick.wav.peaks`, which NTFS
    /// reads as a stream of a file named `C` and FAT rejects.
    #[test]
    fn a_windows_id_gets_a_name_without_a_colon() {
        let relative = waveform_peak_relative_path_for_asset("C:\\Samples\\Drums\\kick.wav");
        let name = file_name(&relative);
        assert!(!name.contains(':'), "{name}");
        assert!(name.starts_with("kick.wav~"), "{name}");
        assert!(name.ends_with(".peaks"));
        assert_eq!(
            legacy_waveform_peak_relative_path_for_asset("C:\\Samples\\Drums\\kick.wav"),
            None,
            "a name with ':' is never looked up"
        );
    }

    /// Stable ids of media dropped from deep folders are long absolute paths;
    /// flattening them exceeded the 255-byte file name limit, so the cache
    /// was never written and every reopen decoded the file again.
    #[test]
    fn a_long_id_gets_a_bounded_name_that_can_be_written() {
        let deep = format!(
            "/Users/me/Library/Mobile Documents/com~apple~CloudDocs/Sample Packs/{}/Kicks/{} kick.wav",
            "Vendor Name/Pack Name/Drums".repeat(8),
            "long ".repeat(30)
        );
        assert!(deep.len() > 255);
        let relative = waveform_peak_relative_path_for_asset(&deep);
        let name = file_name(&relative);
        assert!(
            name.len() <= MAX_PEAK_FILE_NAME_BYTES,
            "{} bytes",
            name.len()
        );
        assert!(name.ends_with(".peaks"));

        // Same prefix, different id: different file.
        let sibling = deep.replace("Kicks", "Snares");
        assert_ne!(waveform_peak_relative_path_for_asset(&sibling), relative);
        // Stable: the same id always names the same file.
        assert_eq!(waveform_peak_relative_path_for_asset(&deep), relative);

        let dir = temp_dir("long_id");
        let path = dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        write_peak_file(&path, &deep, &sample_preview(), None).unwrap();
        let loaded = read_peak_file(&path, Some(&deep), None).unwrap();
        assert_eq!(loaded.total_frames, sample_preview().total_frames);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An id whose old name was writable but is now hashed (a `~` in it, as
    /// in iCloud paths) still finds the cache an earlier build wrote.
    #[test]
    fn a_renamed_cache_file_is_still_found_under_its_old_name() {
        let id = "/Users/me/Library/Mobile Documents/com~apple~CloudDocs/kick.wav";
        let relative = waveform_peak_relative_path_for_asset(id);
        assert!(file_name(&relative).starts_with("kick.wav~"));
        assert_eq!(
            legacy_waveform_peak_relative_path_for_asset(id).as_deref(),
            Some(
                "Cache/Waveforms/__Users__me__Library__Mobile Documents__com~apple~CloudDocs__kick.wav.peaks"
            )
        );
    }
}
