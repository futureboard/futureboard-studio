//! Centralized filesystem path resolution for Futureboard Studio.
//!
//! Every subsystem (project wizard, file browser, recent projects, settings,
//! plugin scanner) should resolve paths through [`FutureboardPaths`] instead
//! of computing them ad-hoc with raw `dirs::*` calls.
//!
//! # Usage
//!
//! ```rust,ignore
//! let paths = FutureboardPaths::resolve();
//! paths.ensure_user_dirs()?;   // idempotent, safe to call multiple times
//! let dirs = paths.standard_dirs();  // for file browser sidebar
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use SpherePluginHost::registry::default_preset_root;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Canonical application name used in all directory paths.
/// Using a single constant eliminates the `"Futureboard"` vs
/// `"Futureboard Studio"` inconsistency that existed before.
const APP_NAME: &str = "Futureboard Studio";

// ── FutureboardPaths ──────────────────────────────────────────────────────────

/// Resolved filesystem paths for the entire application.
///
/// All fields are absolute paths appropriate for the current platform.
/// Call [`resolve()`](FutureboardPaths::resolve) once at startup, store the
/// result, and pass references to subsystems that need path information.
///
/// Directory creation is NOT performed during resolution — call
/// [`ensure_user_dirs()`](FutureboardPaths::ensure_user_dirs) explicitly
/// when you want to guarantee the folder tree exists on disk.
#[derive(Debug, Clone)]
pub struct FutureboardPaths {
    // ── User Documents (~/Documents/Futureboard Studio/) ──────────────────
    /// Root of user-visible content: `~/Documents/Futureboard Studio/`
    pub user_root: PathBuf,
    /// `~/Documents/Futureboard Studio/Projects/`
    pub projects: PathBuf,
    /// `~/Documents/Futureboard Studio/Samples/`
    pub samples: PathBuf,
    /// `~/Documents/Futureboard Studio/User Library/`
    pub user_library: PathBuf,
    /// `~/Documents/Futureboard Studio/Recordings/`
    pub recordings: PathBuf,
    /// `~/Documents/Futureboard Studio/Presets/`
    pub presets: PathBuf,
    /// `~/Documents/Futureboard Studio/Templates/`
    pub templates: PathBuf,
    /// `~/Documents/Futureboard Studio/Loops/`
    pub loops: PathBuf,
    /// `~/Documents/Futureboard Studio/Exports/`
    pub exports: PathBuf,
    /// `~/Documents/Futureboard Studio/Utilities/`
    pub utilities: PathBuf,
    /// `~/Documents/Futureboard Studio/Utilities/Models/` — MDX-NET / UVR ONNX packs.
    pub models: PathBuf,

    // ── AppData ────────────────────────────────────────────────────────────
    /// Platform application data directory:
    /// - Windows: `%APPDATA%/Futureboard Studio/`
    /// - macOS:   `~/Library/Application Support/Futureboard Studio/`
    /// - Linux:   `~/.config/Futureboard Studio/`
    pub app_data: PathBuf,
    /// `<app_data>/settings.json`
    pub settings_file: PathBuf,
    /// `<app_data>/studio_window.json` — last main workspace window bounds.
    pub studio_window_file: PathBuf,
    /// `<app_data>/workspace_layout.json` — panel visibility, sizes, and tabs.
    pub workspace_layout_file: PathBuf,
    /// `<app_data>/recent.json`
    pub recent_file: PathBuf,
    /// `<app_data>/indexfile.dat` — SQLite index database.
    pub index_db: PathBuf,
    /// `<app_data>/Logs/`
    pub logs: PathBuf,
    /// `<app_data>/Cache/`
    pub app_cache: PathBuf,
    /// `<app_data>/Plugin Database/`
    pub plugin_db: PathBuf,
    /// `<app_data>/Keymaps/` — user keyboard shortcut profiles and overrides.
    pub keymaps: PathBuf,
    /// `<app_data>/Extensions/` — user-installed extension content.
    pub extensions: PathBuf,
    /// `<app_data>/Extensions/Themes/` — Native GPUI + WebUI theme packages.
    pub themes: PathBuf,

    // ── Audio ──────────────────────────────────────────────────────────────
    /// Platform default audio/music directory (e.g. `~/Music`).
    pub audio_files: PathBuf,

    // ── Plugins ────────────────────────────────────────────────────────────
    /// Standard VST3 search paths for the current platform.
    pub vst3_paths: Vec<PathBuf>,
    /// Standard CLAP search paths for the current platform.
    pub clap_paths: Vec<PathBuf>,

    // ── Factory (read-only, populated by installer) ───────────────────────
    /// `~/Documents/Futureboard Studio/Factory Content/`
    /// Present in struct for reference; NOT created by `ensure_user_dirs()`.
    pub factory_content: PathBuf,
    /// `~/Documents/Futureboard Studio/Factory Presets/`
    /// Present in struct for reference; NOT created by `ensure_user_dirs()`.
    pub factory_presets: PathBuf,
}

impl FutureboardPaths {
    /// Resolves all application paths for the current platform.
    ///
    /// This is a pure computation — no filesystem I/O is performed.
    /// Call [`ensure_user_dirs()`] afterwards to create directories.
    pub fn resolve() -> Self {
        // ── User document root ────────────────────────────────────────────
        // Prefer `dirs::document_dir()`, but fall back to `$HOME/Documents`
        // (never `.`) so Utilities/Models and Projects stay under the real
        // user Documents tree even when XDG user-dirs are unset.
        let doc_dir = dirs::document_dir().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|home| home.join("Documents"))
                .unwrap_or_else(|| PathBuf::from("Documents"))
        });
        let user_root = doc_dir.join(APP_NAME);

        let projects = user_root.join("Projects");
        let samples = user_root.join("Samples");
        let user_library = user_root.join("User Library");
        let recordings = user_root.join("Recordings");
        let presets = user_root.join("Presets");
        let templates = user_root.join("Templates");
        let loops = user_root.join("Loops");
        let exports = user_root.join("Exports");
        let utilities = user_root.join("Utilities");
        let models = utilities.join("Models");

        // ── Factory (installer-managed) ───────────────────────────────────
        let factory_content = user_root.join("Factory Content");
        let factory_presets = user_root.join("Factory Presets");

        // ── AppData root ──────────────────────────────────────────────────
        let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        let app_data = config_dir.join(APP_NAME);

        let settings_file = app_data.join("settings.json");
        let studio_window_file = app_data.join("studio_window.json");
        let workspace_layout_file = app_data.join("workspace_layout.json");
        let recent_file = app_data.join("recent.json");
        let index_db = app_data.join("indexfile.dat");
        let logs = app_data.join("Logs");
        let app_cache = app_data.join("Cache");
        let plugin_db = app_data.join("Plugin Database");
        let keymaps = app_data.join("Keymaps");
        let extensions = app_data.join("Extensions");
        let themes = extensions.join("Themes");

        // ── Audio directory ───────────────────────────────────────────────
        let audio_files = dirs::audio_dir().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join("Music"))
                .unwrap_or_else(|| PathBuf::from("."))
        });

        // ── Plugin search paths ───────────────────────────────────────────
        let vst3_paths = Self::platform_vst3_paths();
        let clap_paths = Self::platform_clap_paths();

        Self {
            user_root,
            projects,
            samples,
            user_library,
            recordings,
            presets,
            templates,
            loops,
            exports,
            utilities,
            models,
            app_data,
            settings_file,
            studio_window_file,
            workspace_layout_file,
            recent_file,
            index_db,
            logs,
            app_cache,
            plugin_db,
            keymaps,
            extensions,
            themes,
            audio_files,
            vst3_paths,
            clap_paths,
            factory_content,
            factory_presets,
        }
    }

    /// Creates all user document and appdata directories.
    ///
    /// Idempotent — safe to call multiple times. Uses `create_dir_all` so
    /// intermediate parents are created as needed.
    ///
    /// Does NOT create factory content directories (those are installer-managed)
    /// or project-specific directories (use [`ProjectFolderLayout`] for those).
    pub fn ensure_user_dirs(&self) -> Result<(), std::io::Error> {
        let user_dirs = [
            &self.projects,
            &self.samples,
            &self.user_library,
            &self.recordings,
            &self.presets,
            &self.templates,
            &self.loops,
            &self.exports,
            &self.utilities,
            &self.models,
        ];
        for dir in &user_dirs {
            fs::create_dir_all(dir)?;
        }

        let app_dirs = [
            &self.app_data,
            &self.logs,
            &self.app_cache,
            &self.plugin_db,
            &self.keymaps,
            &self.extensions,
            &self.themes,
        ];
        for dir in &app_dirs {
            fs::create_dir_all(dir)?;
        }

        Ok(())
    }

    /// Returns standard directory entries for the file browser sidebar.
    ///
    /// Keys match the existing `resolve_standard_dirs()` contract so callers
    /// can migrate without changing their key lookups.
    pub fn standard_dirs(&self) -> HashMap<String, PathBuf> {
        let mut map = HashMap::new();
        map.insert("audio_files".to_string(), self.audio_files.clone());
        map.insert("projects".to_string(), self.projects.clone());
        map.insert("samples".to_string(), self.samples.clone());
        map.insert("user_data".to_string(), self.user_root.clone());
        map.insert("user_library".to_string(), self.user_library.clone());

        // Show registered plug-ins (.pst presets) in the browser.
        // This matches the Plug-in Manager's preset root.
        map.insert("plugins".to_string(), default_preset_root());

        map
    }

    // ── Platform plugin paths (private) ───────────────────────────────────

    fn platform_vst3_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        #[cfg(target_os = "windows")]
        {
            paths.push(PathBuf::from(r"C:\Program Files\Common Files\VST3"));
            // User-local VST3 on Windows
            if let Some(local) = dirs::data_local_dir() {
                let user_vst3 = local.join("Programs").join("Common").join("VST3");
                if user_vst3.exists() {
                    paths.push(user_vst3);
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            paths.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
            if let Some(home) = dirs::home_dir() {
                paths.push(home.join("Library/Audio/Plug-Ins/VST3"));
            }
        }

        #[cfg(target_os = "linux")]
        {
            paths.push(PathBuf::from("/usr/lib/vst3"));
            paths.push(PathBuf::from("/usr/local/lib/vst3"));
            if let Some(home) = dirs::home_dir() {
                paths.push(home.join(".vst3"));
            }
        }

        paths
    }

    fn platform_clap_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        #[cfg(target_os = "windows")]
        {
            paths.push(PathBuf::from(r"C:\Program Files\Common Files\CLAP"));
        }

        #[cfg(target_os = "macos")]
        {
            paths.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
            if let Some(home) = dirs::home_dir() {
                paths.push(home.join("Library/Audio/Plug-Ins/CLAP"));
            }
        }

        #[cfg(target_os = "linux")]
        {
            paths.push(PathBuf::from("/usr/lib/clap"));
            if let Some(home) = dirs::home_dir() {
                paths.push(home.join(".clap"));
            }
        }

        paths
    }
}

// ── ProjectFolderLayout ───────────────────────────────────────────────────────

/// Resolved subfolder layout for a single Futureboard project.
///
/// Given a project root folder, this struct resolves all standard
/// subdirectories (Assets, Cache, Rendered) and can create them
/// idempotently.
///
/// ```text
/// <root>/
///   Assets/Audio/
///   Assets/MIDI/
///   Assets/Samples/
///   Cache/Waveforms/
///   Cache/Waveform/
///   Cache/Peaks/
///   Cache/Processed/
///   Cache/Analysis/
///   Rendered/Mixdowns/
///   Rendered/Stems/
///   Rendered/Bounces/
/// ```
#[derive(Debug, Clone)]
pub struct ProjectFolderLayout {
    pub root: PathBuf,
    pub media_audio: PathBuf,
    pub media_midi: PathBuf,
    pub media_samples: PathBuf,
    pub cache_waveforms: PathBuf,
    pub cache_waveform: PathBuf,
    pub cache_peaks: PathBuf,
    pub cache_processed: PathBuf,
    pub cache_analysis: PathBuf,
    pub rendered_mixdowns: PathBuf,
    pub rendered_stems: PathBuf,
    pub rendered_bounces: PathBuf,
}

impl ProjectFolderLayout {
    /// Resolves the standard project subfolder layout from a project root path.
    ///
    /// Pure computation — no filesystem I/O.
    pub fn from_root(root: PathBuf) -> Self {
        let media = root.join("Assets");
        let cache = root.join("Cache");
        let rendered = root.join("Rendered");

        Self {
            media_audio: media.join("Audio"),
            media_midi: media.join("MIDI"),
            media_samples: media.join("Samples"),
            cache_waveforms: cache.join("Waveforms"),
            cache_waveform: cache.join("Waveform"),
            cache_peaks: cache.join("Peaks"),
            cache_processed: cache.join("Processed"),
            cache_analysis: cache.join("Analysis"),
            rendered_mixdowns: rendered.join("Mixdowns"),
            rendered_stems: rendered.join("Stems"),
            rendered_bounces: rendered.join("Bounces"),
            root,
        }
    }

    /// Creates all project subdirectories. Idempotent.
    pub fn ensure_dirs(&self) -> Result<(), std::io::Error> {
        let dirs = [
            &self.media_audio,
            &self.media_midi,
            &self.media_samples,
            &self.cache_waveforms,
            &self.cache_waveform,
            &self.cache_peaks,
            &self.cache_processed,
            &self.cache_analysis,
            &self.rendered_mixdowns,
            &self.rendered_stems,
            &self.rendered_bounces,
        ];
        for dir in &dirs {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}
