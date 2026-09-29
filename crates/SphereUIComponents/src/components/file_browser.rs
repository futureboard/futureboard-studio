//! File browser state + lazy directory index for the left sidebar.
//!
//! Mirrors the Electron browser's IPC model: a long-lived process owns the
//! filesystem access and the UI reads from a cache. Here:
//!
//! * `FileBrowserState` holds **state only** — expand/select sets, drive
//!   roots, and an [`IndexCache`] of previously-loaded directory listings.
//! * `visible_nodes()` never touches the filesystem. It walks the cache
//!   and emits "Loading…" / "Error" placeholder rows for paths the
//!   indexer has not finished (or failed to) load.
//! * The actual `std::fs::read_dir` work runs on `gpui::BackgroundExecutor`
//!   from [`crate::layout`] — the UI thread is never blocked.
//!
//! Realtime / audio rules:
//! * filesystem reads never happen in render/layout.
//! * audio paths must never touch this module.
//!
//! Display strings live here as English defaults paired with a `label_key`;
//! the view resolves the key through `I18n` and falls back to the default.
//! Real file and folder names carry no key — a file name is data, not chrome,
//! and must never be translated.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a type-ahead buffer survives without another keystroke before the
/// next letter starts a fresh search.
const TYPE_AHEAD_RESET: Duration = Duration::from_millis(800);

/// Deepest indent the view is asked to draw. Beyond this the tree keeps
/// nesting but the row indent stops growing, so a name never runs out of the
/// fixed 272 px sidebar.
pub const MAX_VISUAL_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEntryKind {
    Folder,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBrowserEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileEntryKind,
    /// Lowercased extension (without the dot), or empty for folders.
    pub extension: String,
    /// Byte size for files. Read from the directory iteration's own cached
    /// metadata (free on Windows) and only on the background executor.
    pub size_bytes: Option<u64>,
}

impl FileBrowserEntry {
    pub fn is_audio(&self) -> bool {
        is_audio_extension(&self.extension)
    }

    pub fn is_midi(&self) -> bool {
        matches!(self.extension.as_str(), "mid" | "midi")
    }

    pub fn is_plugin_preset(&self) -> bool {
        self.extension == "pst"
    }

    /// Extension-only check shared with the timeline's video import path, so
    /// the browser can never offer a drag the arrangement will reject.
    pub fn is_video(&self) -> bool {
        sphere_video_player::is_supported_video_path(&self.path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserNodeKind {
    /// Subtle grouping header (COLLECTIONS / LIBRARY / PLACES). Collapsible,
    /// never selectable, carries no path.
    GroupHeader,
    Folder,
    File,
    /// Non-interactive hint row — e.g. an honest "No favorites yet" empty state
    /// for a category whose data provider does not exist yet.
    Info,
}

/// Presentation-neutral icon hint resolved by the view layer into a concrete
/// SVG glyph. Keeping this in the model (instead of matching label strings in
/// the renderer) is what lets the same navigation model drive future sources
/// without the view guessing icons from text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserIcon {
    None,
    Favorites,
    Recent,
    Samples,
    Instruments,
    Plugins,
    AudioFiles,
    Projects,
    Templates,
    UserLibrary,
    Home,
    Downloads,
    Desktop,
    Documents,
    Music,
    Drive,
    Folder,
    FolderOpen,
    AudioFile,
    MidiFile,
    VideoFile,
    PresetFile,
    ProjectFile,
    GenericFile,
}

/// What the browser lists: one tab at a time, chosen from the strip at its
/// top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserTab {
    /// Instruments from the plug-in database, by vendor.
    Instruments,
    /// Samples, audio files and the user library.
    Samples,
    /// The computer: places and drives.
    Files,
    /// The plug-in `.pst` folders.
    Plugins,
    /// The current project, projects and templates.
    Projects,
}

impl BrowserTab {
    pub const ALL: [BrowserTab; 5] = [
        BrowserTab::Instruments,
        BrowserTab::Samples,
        BrowserTab::Files,
        BrowserTab::Plugins,
        BrowserTab::Projects,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BrowserTab::Instruments => "Instruments",
            BrowserTab::Samples => "Samples",
            BrowserTab::Files => "Files",
            BrowserTab::Plugins => "Plug-ins",
            BrowserTab::Projects => "Projects",
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            BrowserTab::Instruments => "browser.category.instruments",
            BrowserTab::Samples => "browser.category.samples",
            BrowserTab::Files => "browser.tab.files",
            BrowserTab::Plugins => "browser.category.plugins",
            BrowserTab::Projects => "browser.category.projects",
        }
    }

    pub fn icon(self) -> BrowserIcon {
        match self {
            BrowserTab::Instruments => BrowserIcon::Instruments,
            BrowserTab::Samples => BrowserIcon::Samples,
            BrowserTab::Files => BrowserIcon::Folder,
            BrowserTab::Plugins => BrowserIcon::Plugins,
            BrowserTab::Projects => BrowserIcon::Projects,
        }
    }
}

/// One instrument the Instruments tab lists, read from the plug-in database.
/// `preset_path` is the `.pst` the database points at — the file a drag
/// carries and a double-click loads.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserInstrument {
    pub name: String,
    pub vendor: String,
    pub format: String,
    pub category: String,
    pub preset_path: PathBuf,
}

/// Where the Instruments tab's list stands.
#[derive(Debug, Clone, PartialEq)]
pub enum InstrumentsLoad {
    NotLoaded,
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct BrowserRootSection {
    pub id: String,
    pub label: String,
    pub root_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BrowserVisibleNode {
    pub id: String,
    /// English default. For filesystem entries this is the real on-disk name.
    pub label: String,
    /// Message key for chrome labels (groups, categories, placeholders). `None`
    /// means `label` is data and must be shown verbatim.
    pub label_key: Option<&'static str>,
    pub path: Option<PathBuf>,
    pub kind: BrowserNodeKind,
    pub depth: usize,
    pub extension: String,
    pub expandable: bool,
    pub expanded: bool,
    pub selected: bool,
    pub error: Option<String>,
    /// File size in bytes, for files whose listing carried metadata.
    pub size_bytes: Option<u64>,
    /// Semantic icon hint, resolved to a glyph by the view layer.
    pub icon: BrowserIcon,
    /// A short tag the row shows at its end — an instrument's format.
    pub detail: Option<String>,
}

impl BrowserVisibleNode {
    /// Rows the user can land on with click / arrow keys. Group headers and
    /// info/empty-state rows are skipped by selection and keyboard navigation.
    pub fn is_selectable(&self) -> bool {
        !matches!(
            self.kind,
            BrowserNodeKind::GroupHeader | BrowserNodeKind::Info
        )
    }

    pub fn is_audio(&self) -> bool {
        is_audio_extension(&self.extension)
    }

    pub fn is_midi(&self) -> bool {
        matches!(self.extension.as_str(), "mid" | "midi")
    }

    pub fn is_plugin_preset(&self) -> bool {
        self.extension == "pst"
    }

    pub fn is_video(&self) -> bool {
        self.path
            .as_deref()
            .is_some_and(sphere_video_player::is_supported_video_path)
    }
}

/// One step of the path trail from a mounted browser root down to the
/// selection.
#[derive(Debug, Clone)]
pub struct BrowserCrumb {
    pub label: String,
    pub label_key: Option<&'static str>,
    pub path: PathBuf,
}

/// Lazy directory cache populated by the background indexer.
///
/// Each known path is in exactly one of three states:
///   * `loaded` — entries cached, render shows children.
///   * `loading` — request in flight, render shows a Loading row.
///   * `errors` — last load failed, render shows an Error row.
/// Paths in none of these maps are treated as "never asked" and the
/// layout will dispatch a load when the user expands them.
#[derive(Debug, Clone, Default)]
pub struct IndexCache {
    pub loaded: HashMap<PathBuf, IndexedDir>,
    pub loading: HashSet<PathBuf>,
    pub errors: HashMap<PathBuf, String>,
}

#[derive(Debug, Clone)]
pub struct IndexedDir {
    pub entries: Vec<FileBrowserEntry>,
}

/// Navigation roots, resolved once.
///
/// `FutureboardPaths::resolve()` walks the platform plug-in search paths and
/// the five `dirs::*` lookups are Win32 `SHGetKnownFolderPath` shell calls.
/// `update_visible_nodes` runs on every selection change, expand, collapse and
/// search keystroke, so resolving them there cost seven shell round-trips per
/// keypress. They are resolved once here and refreshed only on Rescan.
#[derive(Debug, Clone)]
pub struct BrowserRoots {
    pub samples: Option<PathBuf>,
    pub plugins: Option<PathBuf>,
    pub audio_files: Option<PathBuf>,
    pub projects: Option<PathBuf>,
    pub user_library: Option<PathBuf>,
    pub user_data: Option<PathBuf>,
    pub templates: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub documents: Option<PathBuf>,
    pub desktop: Option<PathBuf>,
    pub downloads: Option<PathBuf>,
    pub music: Option<PathBuf>,
}

impl BrowserRoots {
    pub fn resolve() -> Self {
        let paths = crate::paths::FutureboardPaths::resolve();
        let dirs = paths.standard_dirs();
        Self {
            samples: dirs.get("samples").cloned(),
            plugins: dirs.get("plugins").cloned(),
            audio_files: dirs.get("audio_files").cloned(),
            projects: dirs.get("projects").cloned(),
            user_library: dirs.get("user_library").cloned(),
            user_data: dirs.get("user_data").cloned(),
            templates: Some(paths.templates.clone()),
            home: dirs::home_dir(),
            documents: dirs::document_dir(),
            desktop: dirs::desktop_dir(),
            downloads: dirs::download_dir(),
            music: dirs::audio_dir(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileBrowserState {
    pub selected: Option<PathBuf>,
    /// Expand state, keyed exclusively by path. Drive roots and folders
    /// alike live here so toggle / lookup never disagree.
    pub expanded_paths: HashSet<PathBuf>,
    /// Top-level filesystem roots — drive letters on Windows, `/` and
    /// per-volume mounts on Unix-like systems. Enumerated cheaply at
    /// startup (Win32 `GetLogicalDrives` bitmask on Windows).
    pub root_drives: Vec<BrowserRootSection>,
    /// Resolved library/places roots. See [`BrowserRoots`].
    pub roots: BrowserRoots,
    /// Lazy index of expanded directories. Render reads from here; the
    /// layout owns the background loader that populates it.
    pub index: IndexCache,
    /// Active project folder. Pinned to the top of the browser roots when set.
    pub project_folder: Option<PathBuf>,
    /// Current search filter query.
    pub filter: String,
    /// Whether selecting an audio file immediately auditions it through the
    /// engine. This is session-only and defaults on for DAW-style browsing.
    pub preview_enabled: bool,
    /// Audio files whose waveform peaks are being decoded in the background for
    /// the mini preview pane — guards against re-spawning a decode while one is
    /// in flight (e.g. arrowing quickly through a folder).
    pub waveform_inflight: HashSet<PathBuf>,
    /// Files whose peak decode came back empty. The shared waveform cache
    /// writes *nothing* on a failed decode, so without this the preview pane
    /// would show "Decoding waveform…" forever for a file that can never
    /// decode.
    pub waveform_failed: HashSet<PathBuf>,
    /// File the engine is currently auditioning, plus its playhead in seconds of
    /// that file. Both are polled from the engine (never guessed from a UI
    /// timer), and the pair is dropped together the moment the preview voice
    /// retires — so the pane can never draw a stale playhead, nor one belonging
    /// to a different file than the waveform on screen.
    pub preview_playing: Option<PathBuf>,
    pub preview_position_seconds: Option<f32>,
    /// Group headers (`group:*` ids) the user has collapsed. Collapsed groups
    /// hide all their child rows. Default = all groups expanded.
    pub collapsed_groups: HashSet<String>,
    /// Expand state for path-less category rows (e.g. `collections:favorites`)
    /// whose contents are not filesystem paths. Filesystem folders use
    /// `expanded_paths` instead.
    pub expanded_virtual: HashSet<String>,
    /// Cached flattened visible nodes representing the current state. Shared
    /// with the renderer by `Arc` — the sidebar used to deep-copy every node
    /// on every frame.
    pub visible_nodes: Arc<Vec<BrowserVisibleNode>>,
    /// The tab the list shows.
    pub tab: BrowserTab,
    /// The Instruments tab's rows, as the plug-in database last reported them.
    pub instruments: Arc<Vec<BrowserInstrument>>,
    pub instruments_load: InstrumentsLoad,
    /// Type-ahead buffer and the time of its last keystroke.
    type_ahead: String,
    type_ahead_at: Option<Instant>,
}

impl Default for FileBrowserState {
    fn default() -> Self {
        let mut state = Self {
            selected: None,
            expanded_paths: HashSet::new(),
            root_drives: default_root_drives(),
            roots: BrowserRoots::resolve(),
            index: IndexCache::default(),
            project_folder: None,
            filter: String::new(),
            preview_enabled: true,
            waveform_inflight: HashSet::new(),
            waveform_failed: HashSet::new(),
            preview_playing: None,
            preview_position_seconds: None,
            collapsed_groups: {
                // Drives (raw drive letters / `/` / mounted volumes) are
                // advanced territory — start collapsed. Every other group
                // defaults to expanded.
                let mut collapsed = HashSet::new();
                collapsed.insert("group:filesystem".to_string());
                collapsed
            },
            expanded_virtual: HashSet::new(),
            visible_nodes: Arc::new(Vec::new()),
            tab: BrowserTab::Instruments,
            instruments: Arc::new(Vec::new()),
            instruments_load: InstrumentsLoad::NotLoaded,
            type_ahead: String::new(),
            type_ahead_at: None,
        };
        state.update_visible_nodes();
        state
    }
}

impl FileBrowserState {
    pub fn select(&mut self, path: PathBuf) {
        self.selected = Some(path);
        self.update_visible_nodes();
    }

    /// Toggle expand state for a node. Path is the source of truth.
    /// Returns `true` if the path was just expanded (caller should ensure
    /// it is indexed); `false` if it was collapsed.
    pub fn toggle_node(&mut self, node_id: &str, path: Option<&Path>) -> bool {
        // Group headers have no path; their open/closed state lives in
        // `collapsed_groups` keyed by the `group:*` id.
        if path.is_none() {
            if node_id.starts_with("group:") {
                let expanded = if self.collapsed_groups.remove(node_id) {
                    true
                } else {
                    self.collapsed_groups.insert(node_id.to_string());
                    false
                };
                self.update_visible_nodes();
                return expanded;
            }
            // Path-less category (Favorites / Recent …) — expand state keyed
            // by id since there is no filesystem path to track.
            let expanded = if self.expanded_virtual.remove(node_id) {
                false
            } else {
                self.expanded_virtual.insert(node_id.to_string());
                true
            };
            self.update_visible_nodes();
            return expanded;
        }
        let Some(path) = path else {
            return false;
        };
        let path = path.to_path_buf();
        let expanded = if self.expanded_paths.contains(&path) {
            self.expanded_paths.remove(&path);
            false
        } else {
            self.expanded_paths.insert(path);
            true
        };
        self.update_visible_nodes();
        expanded
    }

    /// Expand every ancestor of `path` that belongs to a mounted root, so a
    /// breadcrumb jump lands on a row that is actually on screen.
    pub fn reveal_path(&mut self, path: &Path) {
        // Resolve the root set once — `is_mounted_root` rebuilds it per call.
        let roots = self.mounted_roots();
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut cursor = path.parent();
        while let Some(dir) = cursor {
            ancestors.push(dir.to_path_buf());
            if roots.iter().any(|(_, _, root)| root == dir) {
                break;
            }
            cursor = dir.parent();
        }
        for dir in ancestors {
            self.expanded_paths.insert(dir);
        }
        self.selected = Some(path.to_path_buf());
        self.update_visible_nodes();
    }

    /// Apply a finished directory listing from the background indexer.
    pub fn apply_loaded(&mut self, path: PathBuf, entries: Vec<FileBrowserEntry>) {
        self.index.loading.remove(&path);
        self.index.errors.remove(&path);
        self.index.loaded.insert(path, IndexedDir { entries });
        self.update_visible_nodes();
    }

    /// Apply a finished directory listing failure from the background indexer.
    pub fn apply_error(&mut self, path: PathBuf, error: String) {
        self.index.loading.remove(&path);
        self.index.loaded.remove(&path);
        self.index.errors.insert(path, error);
        self.update_visible_nodes();
    }

    /// Mark a path as having an in-flight load request.
    pub fn mark_loading(&mut self, path: PathBuf) {
        self.index.errors.remove(&path);
        self.index.loading.insert(path);
        self.update_visible_nodes();
    }

    /// Set active project folder and refresh roots.
    pub fn set_project_folder(&mut self, folder: Option<PathBuf>) {
        self.project_folder = folder;
        self.update_visible_nodes();
    }

    /// Show `tab`. Returns whether the Instruments list has yet to be read
    /// (the caller starts the database read).
    pub fn set_tab(&mut self, tab: BrowserTab) -> bool {
        if self.tab != tab {
            self.tab = tab;
            self.update_visible_nodes();
        }
        tab == BrowserTab::Instruments && self.instruments_load == InstrumentsLoad::NotLoaded
    }

    /// A database read has been started.
    pub fn mark_instruments_loading(&mut self) {
        self.instruments_load = InstrumentsLoad::Loading;
        self.update_visible_nodes();
    }

    /// The database read finished.
    pub fn apply_instruments(&mut self, result: Result<Vec<BrowserInstrument>, String>) {
        match result {
            Ok(list) => {
                self.instruments = Arc::new(list);
                self.instruments_load = InstrumentsLoad::Loaded;
            }
            Err(error) => self.instruments_load = InstrumentsLoad::Failed(error),
        }
        self.update_visible_nodes();
    }

    /// The listed instrument whose `.pst` is `path`, on the Instruments tab.
    pub fn instrument_for(&self, path: &Path) -> Option<&BrowserInstrument> {
        (self.tab == BrowserTab::Instruments)
            .then(|| self.instruments.iter().find(|i| i.preset_path == path))
            .flatten()
    }

    /// Set search filter query.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.update_visible_nodes();
    }

    /// Flip the auto-preview toggle. Returns the new state.
    pub fn toggle_preview_enabled(&mut self) -> bool {
        self.preview_enabled = !self.preview_enabled;
        self.preview_enabled
    }

    /// The current selection if (and only if) it is an audio file — drives the
    /// mini waveform preview pane.
    pub fn selected_audio_path(&self) -> Option<&Path> {
        let path = self.selected.as_deref()?;
        is_audio_path(path).then_some(path)
    }

    /// The visible node matching the current selection, if it is on screen.
    pub fn selected_node(&self) -> Option<&BrowserVisibleNode> {
        let selected = self.selected.as_ref()?;
        self.visible_nodes
            .iter()
            .find(|node| node.path.as_ref() == Some(selected))
    }

    /// Row index of the selection, for `scroll_to_item`.
    pub fn index_of_selected(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.visible_nodes
            .iter()
            .position(|node| node.path.as_ref() == Some(selected))
    }

    /// Whether the selected row is a real directory. Read off the cached node
    /// so key and menu handlers never `stat` on the UI thread.
    pub fn selected_is_expandable(&self) -> bool {
        self.selected_node().is_some_and(|node| node.expandable)
    }

    /// Directory check for an arbitrary path, answered from the cache only.
    /// Returns `None` when the browser has never seen the path.
    pub fn known_is_directory(&self, path: &Path) -> Option<bool> {
        if self.index.loaded.contains_key(path)
            || self.index.loading.contains(path)
            || self.index.errors.contains_key(path)
            || self.is_mounted_root(path)
        {
            return Some(true);
        }
        self.visible_nodes
            .iter()
            .find(|node| node.path.as_deref() == Some(path))
            .map(|node| node.kind == BrowserNodeKind::Folder)
    }

    /// Record which file the engine was asked to audition. The playhead stays
    /// empty until the engine reports a real position, so a decode that never
    /// produces sound never draws one.
    pub fn set_preview_playing(&mut self, path: PathBuf) {
        self.preview_playing = Some(path);
        self.preview_position_seconds = None;
    }

    /// Apply the engine's preview playhead. `None` means nothing is auditioning,
    /// which also clears the file the playhead belonged to. Returns whether the
    /// pane needs a repaint.
    pub fn apply_preview_position(&mut self, position_seconds: Option<f32>) -> bool {
        match position_seconds {
            Some(seconds) => {
                let changed = self.preview_position_seconds != Some(seconds);
                self.preview_position_seconds = Some(seconds);
                changed
            }
            None => {
                let changed =
                    self.preview_position_seconds.is_some() || self.preview_playing.is_some();
                self.preview_position_seconds = None;
                self.preview_playing = None;
                changed
            }
        }
    }

    /// Playhead for `path` in seconds, if that is the file currently being
    /// auditioned.
    pub fn preview_position_for(&self, path: &Path) -> Option<f32> {
        (self.preview_playing.as_deref() == Some(path)).then_some(self.preview_position_seconds)?
    }

    /// Mark `path` as having an in-flight waveform decode. Returns `true` if it
    /// was newly inserted (caller should spawn the decode), `false` if one is
    /// already running.
    pub fn begin_waveform_load(&mut self, path: PathBuf) -> bool {
        self.waveform_failed.remove(&path);
        self.waveform_inflight.insert(path)
    }

    /// Clear the in-flight marker once a waveform decode finishes (or fails).
    pub fn end_waveform_load(&mut self, path: &Path) {
        self.waveform_inflight.remove(path);
    }

    /// Record a decode that produced no peaks, so the preview pane can show a
    /// real error instead of a permanent pending state.
    pub fn mark_waveform_failed(&mut self, path: PathBuf) {
        self.waveform_failed.insert(path);
    }

    pub fn waveform_failed(&self, path: &Path) -> bool {
        self.waveform_failed.contains(path)
    }

    /// Returns the list of currently-expanded paths whose contents have
    /// neither been loaded, nor are loading, nor previously failed.
    ///
    /// Excluding failures matters: without it one unreadable folder (an
    /// unplugged drive, `System Volume Information`) is re-dispatched on every
    /// expand, collapse and arrow key, each time spawning a visible background
    /// task that fails again. Rescan and the context menu's Refresh both clear
    /// the error entry, so a deliberate retry still works.
    pub fn paths_needing_load(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for path in &self.expanded_paths {
            if self.index.loaded.contains_key(path)
                || self.index.loading.contains(path)
                || self.index.errors.contains_key(path)
            {
                continue;
            }
            out.push(path.clone());
        }
        out
    }

    pub fn visible_node_count(&self) -> usize {
        self.visible_nodes.len()
    }

    /// The flattened row list. Cheap: the renderer shares the same allocation.
    pub fn visible_nodes(&self) -> Arc<Vec<BrowserVisibleNode>> {
        Arc::clone(&self.visible_nodes)
    }

    /// Re-calculate the cached visible flattened nodes — for the current tab
    /// only.
    ///
    /// * **Instruments** — the plug-in database's instruments, one group per
    ///   vendor, each row pointing at its `.pst`.
    /// * **Samples** — Samples, Audio Files and the User Library.
    /// * **Files** — Places (the project, user folders), then Drives.
    /// * **Plug-ins** — the `.pst` folders.
    /// * **Projects** — the current project, Projects and Templates.
    ///
    /// Folder roots are depth 1 with lazily-loaded children below them.
    pub fn update_visible_nodes(&mut self) {
        let mut nodes = Vec::new();
        // Folders already listed by an earlier, more specific root. See
        // `push_root`.
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let roots = self.roots.clone();

        match self.tab {
            BrowserTab::Instruments => self.push_instruments(&mut nodes),
            BrowserTab::Samples => {
                for (id, label, key, path, icon) in [
                    (
                        "lib:samples",
                        "Samples",
                        "browser.category.samples",
                        &roots.samples,
                        BrowserIcon::Samples,
                    ),
                    (
                        "lib:audio_files",
                        "Audio Files",
                        "browser.category.audio-files",
                        &roots.audio_files,
                        BrowserIcon::AudioFiles,
                    ),
                    (
                        "lib:user_library",
                        "User Library",
                        "browser.category.user-library",
                        &roots.user_library,
                        BrowserIcon::UserLibrary,
                    ),
                ] {
                    if let Some(p) = path {
                        self.push_root(id, label, Some(key), p, icon, &mut seen, &mut nodes);
                    }
                }
            }
            BrowserTab::Files => {
                if self.push_group_header(
                    "group:places",
                    "Places",
                    "browser.group.places",
                    &mut nodes,
                ) {
                    if let Some(proj) = self.project_folder.clone() {
                        self.push_project_root(&proj, &mut seen, &mut nodes);
                    }
                    for (id, label, key, path, icon) in [
                        (
                            "places:user_data",
                            "User Data",
                            "browser.category.user-data",
                            &roots.user_data,
                            BrowserIcon::UserLibrary,
                        ),
                        (
                            "places:home",
                            "Home",
                            "browser.category.home",
                            &roots.home,
                            BrowserIcon::Home,
                        ),
                        (
                            "places:documents",
                            "Documents",
                            "browser.category.documents",
                            &roots.documents,
                            BrowserIcon::Documents,
                        ),
                        (
                            "places:desktop",
                            "Desktop",
                            "browser.category.desktop",
                            &roots.desktop,
                            BrowserIcon::Desktop,
                        ),
                        (
                            "places:downloads",
                            "Downloads",
                            "browser.category.downloads",
                            &roots.downloads,
                            BrowserIcon::Downloads,
                        ),
                        (
                            "places:music",
                            "Music",
                            "browser.category.music",
                            &roots.music,
                            BrowserIcon::Music,
                        ),
                    ] {
                        if let Some(p) = path {
                            self.push_root(id, label, Some(key), p, icon, &mut seen, &mut nodes);
                        }
                    }
                }
                if !self.root_drives.is_empty()
                    && self.push_group_header(
                        "group:filesystem",
                        "Drives",
                        "browser.group.drives",
                        &mut nodes,
                    )
                {
                    let drives = self.root_drives.clone();
                    for drive in &drives {
                        if let Some(drive_path) = drive.root_path.as_ref() {
                            self.push_root(
                                &drive.id,
                                &drive.label,
                                None,
                                drive_path,
                                BrowserIcon::Drive,
                                &mut seen,
                                &mut nodes,
                            );
                        }
                    }
                }
            }
            BrowserTab::Plugins => {
                if let Some(p) = roots.plugins.as_ref() {
                    self.push_root(
                        "lib:plugins",
                        "Plug-ins",
                        Some("browser.category.plugins"),
                        p,
                        BrowserIcon::Plugins,
                        &mut seen,
                        &mut nodes,
                    );
                }
            }
            BrowserTab::Projects => {
                if let Some(proj) = self.project_folder.clone() {
                    self.push_project_root(&proj, &mut seen, &mut nodes);
                }
                for (id, label, key, path, icon) in [
                    (
                        "lib:projects",
                        "Projects",
                        "browser.category.projects",
                        &roots.projects,
                        BrowserIcon::Projects,
                    ),
                    (
                        "lib:templates",
                        "Templates",
                        "browser.category.templates",
                        &roots.templates,
                        BrowserIcon::Templates,
                    ),
                ] {
                    if let Some(p) = path {
                        self.push_root(id, label, Some(key), p, icon, &mut seen, &mut nodes);
                    }
                }
            }
        }

        // Apply the active search filter across the tab's flattened rows.
        if !self.filter.is_empty() {
            nodes = self.filter_flattened_nodes(nodes);
        }

        self.visible_nodes = Arc::new(nodes);
    }

    /// The open project's folder, named after itself.
    fn push_project_root(
        &self,
        proj: &Path,
        seen: &mut HashSet<PathBuf>,
        nodes: &mut Vec<BrowserVisibleNode>,
    ) {
        let label = proj
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Current Project".to_string());
        // The project's own folder name is data; only the row's icon marks it
        // as the project root.
        self.push_root(
            "places:project",
            &label,
            None,
            proj,
            BrowserIcon::Projects,
            seen,
            nodes,
        );
    }

    /// The Instruments tab: one collapsible group per vendor, each instrument
    /// a row carrying its `.pst`. While the database is being read, when it
    /// could not be, and when it lists nothing, one honest row says so.
    fn push_instruments(&self, nodes: &mut Vec<BrowserVisibleNode>) {
        let status = match &self.instruments_load {
            InstrumentsLoad::NotLoaded | InstrumentsLoad::Loading => Some((
                "Reading the plug-in database…",
                "browser.instruments.loading",
                None,
            )),
            InstrumentsLoad::Failed(error) => Some((
                "Could not read the plug-in database",
                "browser.instruments.failed",
                Some(error.clone()),
            )),
            InstrumentsLoad::Loaded if self.instruments.is_empty() => Some((
                "No instruments yet — scan plug-ins in the Plug-in Manager",
                "browser.instruments.empty",
                None,
            )),
            InstrumentsLoad::Loaded => None,
        };
        if let Some((label, key, error)) = status {
            nodes.push(BrowserVisibleNode {
                id: "instruments:status".to_string(),
                label: label.to_string(),
                label_key: Some(key),
                path: None,
                kind: BrowserNodeKind::Info,
                depth: 0,
                extension: String::new(),
                expandable: false,
                expanded: false,
                selected: false,
                error,
                size_bytes: None,
                icon: BrowserIcon::None,
                detail: None,
            });
            return;
        }

        let searching = !self.filter.is_empty();
        let mut vendor: Option<&str> = None;
        let mut open = true;
        for instrument in self.instruments.iter() {
            let this_vendor = if instrument.vendor.trim().is_empty() {
                "Other"
            } else {
                instrument.vendor.as_str()
            };
            if vendor != Some(this_vendor) {
                vendor = Some(this_vendor);
                let id = format!("group:vendor:{this_vendor}");
                open = searching || !self.collapsed_groups.contains(&id);
                nodes.push(BrowserVisibleNode {
                    id,
                    label: this_vendor.to_string(),
                    label_key: None,
                    path: None,
                    kind: BrowserNodeKind::GroupHeader,
                    depth: 0,
                    extension: String::new(),
                    expandable: !searching,
                    expanded: open,
                    selected: false,
                    error: None,
                    size_bytes: None,
                    icon: BrowserIcon::None,
                    detail: None,
                });
            }
            if !open {
                continue;
            }
            let path = instrument.preset_path.clone();
            nodes.push(BrowserVisibleNode {
                id: format!("instrument\u{1f}{}", path.to_string_lossy()),
                label: instrument.name.clone(),
                label_key: None,
                selected: self.selected.as_deref() == Some(path.as_path()),
                path: Some(path),
                kind: BrowserNodeKind::File,
                depth: 1,
                extension: "pst".to_string(),
                expandable: false,
                expanded: false,
                error: None,
                size_bytes: None,
                icon: BrowserIcon::Instruments,
                detail: Some(instrument.format.clone()),
            });
        }
    }

    /// Push a collapsible group header. Returns `true` when the group is open
    /// (caller should emit its items).
    fn push_group_header(
        &self,
        id: &str,
        label: &str,
        label_key: &'static str,
        nodes: &mut Vec<BrowserVisibleNode>,
    ) -> bool {
        // While searching every group is forced open so matches in a collapsed
        // group can still surface. The header is then rendered as a plain
        // caption rather than a chevron the user could click for no visible
        // effect — collapsing is unavailable, so it must not look available.
        let searching = !self.filter.is_empty();
        let open = searching || !self.collapsed_groups.contains(id);
        nodes.push(BrowserVisibleNode {
            id: id.to_string(),
            label: label.to_string(),
            label_key: Some(label_key),
            path: None,
            kind: BrowserNodeKind::GroupHeader,
            depth: 0,
            extension: String::new(),
            expandable: !searching,
            expanded: open,
            selected: false,
            error: None,
            size_bytes: None,
            icon: BrowserIcon::None,
            detail: None,
        });
        open
    }

    /// Push a real, filesystem-backed navigation item (depth 1) and, when
    /// expanded, its lazily-loaded children (depth 2+).
    /// Push one root, unless the same folder is already on the list.
    ///
    /// The roots come from several sources that overlap on a real machine:
    /// `standard_dirs()` resolves Projects, Audio Files and User Library under
    /// paths that can be the same folder, and Music, Documents and Home come
    /// from the platform, which is free to point two of them at one place. Each
    /// of those pushed its own row, so the browser listed the same folder twice
    /// — and because expansion is keyed by path, opening one opened the other,
    /// which is the half of the bug that looked like the tree fighting back.
    ///
    /// First one wins, and the order the groups are built in is the priority:
    /// the named Library entry keeps the folder, and the generic Place that
    /// happens to resolve to it drops out.
    fn push_root(
        &self,
        id: &str,
        label: &str,
        label_key: Option<&'static str>,
        path: &Path,
        icon: BrowserIcon,
        seen: &mut HashSet<PathBuf>,
        nodes: &mut Vec<BrowserVisibleNode>,
    ) {
        if !seen.insert(dedupe_key(path)) {
            return;
        }
        let expanded = self.expanded_paths.contains(path);
        let selected = self.selected.as_deref() == Some(path);
        nodes.push(BrowserVisibleNode {
            id: id.to_string(),
            label: label.to_string(),
            label_key,
            path: Some(path.to_path_buf()),
            kind: BrowserNodeKind::Folder,
            depth: 1,
            extension: String::new(),
            expandable: true,
            expanded,
            selected,
            error: None,
            size_bytes: None,
            icon,
            detail: None,
        });
        if expanded {
            self.append_cached_dir(id, path, 2, nodes);
        }
    }

    /// Collapse every expanded folder/category. Group headers stay open.
    pub fn collapse_all(&mut self) {
        self.expanded_paths.clear();
        self.expanded_virtual.clear();
        self.update_visible_nodes();
    }

    /// Drop cached listings for currently-expanded folders and return them so
    /// the caller can re-dispatch background loads (Rescan). Also re-resolves
    /// the platform roots, which is the one place a user asks for that.
    pub fn invalidate_expanded(&mut self) -> Vec<PathBuf> {
        self.roots = BrowserRoots::resolve();
        let paths: Vec<PathBuf> = self.expanded_paths.iter().cloned().collect();
        for p in &paths {
            self.index.loaded.remove(p);
            self.index.errors.remove(p);
            self.index.loading.remove(p);
        }
        self.update_visible_nodes();
        paths
    }

    /// Flatten one loaded directory into rows under `prefix`.
    ///
    /// `prefix` is the owning root's node id, and every row's id is built from
    /// it. Rows used to be identified by their path alone, which is not unique
    /// in a tree that can show one folder in two places — Documents under
    /// Places and again under Home, say. Two rows with one id is a duplicate
    /// element id to the UI toolkit, and the second row inherits the first's
    /// hit-testing: clicking one folder opened another.
    fn append_cached_dir(
        &self,
        prefix: &str,
        dir: &Path,
        depth: usize,
        nodes: &mut Vec<BrowserVisibleNode>,
    ) {
        if let Some(err) = self.index.errors.get(dir) {
            nodes.push(placeholder_row(dir, depth, err.clone(), true));
            return;
        }
        if self.index.loading.contains(dir) {
            nodes.push(placeholder_row(dir, depth, "Loading…".to_string(), false));
            return;
        }
        let Some(indexed) = self.index.loaded.get(dir) else {
            nodes.push(placeholder_row(dir, depth, "Loading…".to_string(), false));
            return;
        };

        for entry in &indexed.entries {
            let is_folder = entry.kind == FileEntryKind::Folder;
            let expanded = is_folder && self.expanded_paths.contains(&entry.path);
            let selected = self.selected.as_deref() == Some(entry.path.as_path());
            let id = format!("{prefix}\u{1f}{}", entry.path.to_string_lossy());
            nodes.push(BrowserVisibleNode {
                id,
                label: entry.name.clone(),
                label_key: None,
                path: Some(entry.path.clone()),
                kind: if is_folder {
                    BrowserNodeKind::Folder
                } else {
                    BrowserNodeKind::File
                },
                depth,
                extension: entry.extension.clone(),
                expandable: is_folder,
                expanded,
                selected,
                error: None,
                size_bytes: entry.size_bytes,
                icon: entry_icon(is_folder, expanded, entry),
                detail: None,
            });

            if expanded {
                let child_prefix = format!("{prefix}\u{1f}{}", entry.path.to_string_lossy());
                self.append_cached_dir(&child_prefix, &entry.path, depth + 1, nodes);
            }
        }
    }

    fn filter_flattened_nodes(&self, nodes: Vec<BrowserVisibleNode>) -> Vec<BrowserVisibleNode> {
        let query = self.filter.to_lowercase();
        let mut kept = vec![false; nodes.len()];

        for i in (0..nodes.len()).rev() {
            let node = &nodes[i];
            let matches_query = node.label.to_lowercase().contains(&query);
            if matches_query {
                kept[i] = true;
                if node.kind == BrowserNodeKind::GroupHeader {
                    for j in (i + 1)..nodes.len() {
                        if nodes[j].depth <= node.depth {
                            break;
                        }
                        kept[j] = true;
                    }
                }
                continue;
            }

            if node.expandable || node.kind == BrowserNodeKind::GroupHeader {
                let mut has_kept_descendant = false;
                for j in (i + 1)..nodes.len() {
                    let child = &nodes[j];
                    if child.depth <= node.depth {
                        break;
                    }
                    if kept[j] {
                        has_kept_descendant = true;
                        break;
                    }
                }
                if has_kept_descendant {
                    kept[i] = true;
                }
            }
        }

        nodes
            .into_iter()
            .enumerate()
            .filter(|(i, _)| kept[*i])
            .map(|(_, n)| n)
            .collect()
    }

    // ── Roots and breadcrumb ────────────────────────────────────────────

    /// Every mounted navigation root, flat. Derived from the same cached
    /// [`BrowserRoots`] / drive list `update_visible_nodes` emits, so the
    /// breadcrumb can never name a root the tree does not have.
    fn mounted_roots(&self) -> Vec<(Option<&'static str>, String, PathBuf)> {
        let mut out: Vec<(Option<&'static str>, String, PathBuf)> = Vec::new();
        if let Some(proj) = self.project_folder.as_ref() {
            let label = proj
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Current Project".to_string());
            out.push((None, label, proj.clone()));
        }
        let named: [(Option<&'static str>, &str, &Option<PathBuf>); 12] = [
            (
                Some("browser.category.samples"),
                "Samples",
                &self.roots.samples,
            ),
            (
                Some("browser.category.plugins"),
                "Plug-ins",
                &self.roots.plugins,
            ),
            (
                Some("browser.category.audio-files"),
                "Audio Files",
                &self.roots.audio_files,
            ),
            (
                Some("browser.category.projects"),
                "Projects",
                &self.roots.projects,
            ),
            (
                Some("browser.category.user-library"),
                "User Library",
                &self.roots.user_library,
            ),
            (
                Some("browser.category.user-data"),
                "User Data",
                &self.roots.user_data,
            ),
            (
                Some("browser.category.templates"),
                "Templates",
                &self.roots.templates,
            ),
            (Some("browser.category.home"), "Home", &self.roots.home),
            (
                Some("browser.category.documents"),
                "Documents",
                &self.roots.documents,
            ),
            (
                Some("browser.category.desktop"),
                "Desktop",
                &self.roots.desktop,
            ),
            (
                Some("browser.category.downloads"),
                "Downloads",
                &self.roots.downloads,
            ),
            (Some("browser.category.music"), "Music", &self.roots.music),
        ];
        for (key, label, path) in named {
            if let Some(path) = path.as_ref() {
                out.push((key, label.to_string(), path.clone()));
            }
        }
        for drive in &self.root_drives {
            if let Some(path) = drive.root_path.as_ref() {
                out.push((None, drive.label.clone(), path.clone()));
            }
        }
        out
    }

    fn is_mounted_root(&self, path: &Path) -> bool {
        self.mounted_roots().iter().any(|(_, _, root)| root == path)
    }

    /// Path trail from the nearest mounted root down to the selection, ready
    /// for the breadcrumb bar. Empty when nothing is selected.
    ///
    /// The walk is capped so an unrooted path (a drive whose group is
    /// collapsed, say) still produces a bounded, renderable trail instead of
    /// one crumb per directory level.
    pub fn breadcrumb(&self) -> Vec<BrowserCrumb> {
        let Some(selected) = self.selected.as_ref() else {
            return Vec::new();
        };
        let roots = self.mounted_roots();
        let mut trail: Vec<BrowserCrumb> = Vec::new();
        let mut cursor: Option<&Path> = Some(selected.as_path());
        while let Some(path) = cursor {
            if let Some((key, label, _)) = roots.iter().find(|(_, _, root)| root == path) {
                trail.push(BrowserCrumb {
                    label: label.clone(),
                    label_key: *key,
                    path: path.to_path_buf(),
                });
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            trail.push(BrowserCrumb {
                label: name,
                label_key: None,
                path: path.to_path_buf(),
            });
            if trail.len() >= MAX_VISUAL_DEPTH + 2 {
                break;
            }
            cursor = path.parent();
        }
        trail.reverse();
        trail
    }

    // ── Keyboard helpers ────────────────────────────────────────────────

    /// Indices of rows the user can actually land on (skips group headers,
    /// info/empty rows, and synthetic path-less placeholders).
    fn selectable_indices(&self) -> Vec<usize> {
        self.visible_nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_selectable() && n.path.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    fn position_of_selection(&self, selectable: &[usize]) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        selectable
            .iter()
            .position(|&i| self.visible_nodes[i].path.as_ref() == Some(selected))
    }

    /// Move the selection down one row. Returns the new row index so the caller
    /// can scroll it into view.
    pub fn select_next(&mut self) -> Option<usize> {
        let selectable = self.selectable_indices();
        if selectable.is_empty() {
            return None;
        }
        let current_pos = self.position_of_selection(&selectable);
        let target = match current_pos {
            Some(pos) if pos + 1 < selectable.len() => selectable[pos + 1],
            Some(pos) => selectable[pos],
            None => selectable[0],
        };
        self.selected = self.visible_nodes[target].path.clone();
        self.update_visible_nodes();
        Some(target)
    }

    /// Move the selection up one row. Returns the new row index.
    pub fn select_previous(&mut self) -> Option<usize> {
        let selectable = self.selectable_indices();
        if selectable.is_empty() {
            return None;
        }
        let current_pos = self.position_of_selection(&selectable);
        let target = match current_pos {
            Some(pos) if pos > 0 => selectable[pos - 1],
            Some(pos) => selectable[pos],
            None => selectable[0],
        };
        self.selected = self.visible_nodes[target].path.clone();
        self.update_visible_nodes();
        Some(target)
    }

    /// Arrow-right: open a closed folder, or step into an open one.
    ///
    /// Returns whether anything changed. Files are rejected outright — the old
    /// implementation inserted the selection into `expanded_paths` with no
    /// check, which queued a `read_dir` on a *file*, spawned a visible
    /// background task that always failed, and left a permanent bogus entry
    /// that Rescan re-scanned forever.
    pub fn expand_selected(&mut self) -> bool {
        let Some(current) = self.selected.clone() else {
            return false;
        };
        if self.expanded_paths.contains(&current) {
            let Some(idx) = self.index_of_selected() else {
                return false;
            };
            let next = idx + 1;
            if next < self.visible_nodes.len()
                && self.visible_nodes[next].depth > self.visible_nodes[idx].depth
                && self.visible_nodes[next].is_selectable()
                && self.visible_nodes[next].path.is_some()
            {
                self.selected = self.visible_nodes[next].path.clone();
                self.update_visible_nodes();
                return true;
            }
            return false;
        }
        if !self.selected_is_expandable() {
            return false;
        }
        self.expanded_paths.insert(current);
        self.update_visible_nodes();
        true
    }

    /// Arrow-left: close an open folder, otherwise jump to its parent row.
    /// Returns whether anything changed.
    pub fn collapse_selected_or_parent(&mut self) -> bool {
        let Some(current) = self.selected.clone() else {
            return false;
        };
        if self.expanded_paths.remove(&current) {
            self.update_visible_nodes();
            return true;
        }
        let Some(idx) = self.index_of_selected() else {
            return false;
        };
        let current_depth = self.visible_nodes[idx].depth;
        if current_depth == 0 {
            return false;
        }
        for i in (0..idx).rev() {
            // Only jump to a real (path-backed) parent — never a group header
            // or empty-state row.
            if self.visible_nodes[i].depth < current_depth
                && self.visible_nodes[i].path.is_some()
                && self.visible_nodes[i].is_selectable()
            {
                self.selected = self.visible_nodes[i].path.clone();
                self.update_visible_nodes();
                return true;
            }
        }
        false
    }

    /// Type-ahead: jump to the next row whose label starts with the accumulated
    /// buffer. Returns the row index when the selection moved.
    ///
    /// A repeated single character cycles through the rows starting with it;
    /// once the buffer is longer the match restarts at the current row so
    /// refining a query never jumps backwards past a still-valid match.
    pub fn type_ahead(&mut self, ch: char, now: Instant) -> Option<usize> {
        let expired = self
            .type_ahead_at
            .map(|at| now.saturating_duration_since(at) > TYPE_AHEAD_RESET)
            .unwrap_or(true);
        if expired {
            self.type_ahead.clear();
        }
        self.type_ahead_at = Some(now);
        for lower in ch.to_lowercase() {
            self.type_ahead.push(lower);
        }
        let query = self.type_ahead.clone();

        let selectable = self.selectable_indices();
        if selectable.is_empty() {
            return None;
        }
        let current = self.position_of_selection(&selectable);
        let start = match current {
            Some(pos) if query.chars().count() <= 1 => pos + 1,
            Some(pos) => pos,
            None => 0,
        };
        let count = selectable.len();
        for step in 0..count {
            let idx = selectable[(start + step) % count];
            if self.visible_nodes[idx]
                .label
                .to_lowercase()
                .starts_with(&query)
            {
                self.selected = self.visible_nodes[idx].path.clone();
                self.update_visible_nodes();
                return Some(idx);
            }
        }
        None
    }

    /// Drop a stale type-ahead buffer (Escape, focus change, mouse selection).
    pub fn clear_type_ahead(&mut self) {
        self.type_ahead.clear();
        self.type_ahead_at = None;
    }
}

/// Resolve the semantic icon for a filesystem entry.
fn entry_icon(is_folder: bool, expanded: bool, entry: &FileBrowserEntry) -> BrowserIcon {
    if is_folder {
        if expanded {
            BrowserIcon::FolderOpen
        } else {
            BrowserIcon::Folder
        }
    } else if entry.is_audio() {
        BrowserIcon::AudioFile
    } else if entry.is_midi() {
        BrowserIcon::MidiFile
    } else if entry.is_video() {
        BrowserIcon::VideoFile
    } else {
        match entry.extension.as_str() {
            "vst3" | "pst" | "fxp" | "fxb" => BrowserIcon::PresetFile,
            "fbproj" | "fbs" => BrowserIcon::ProjectFile,
            _ => BrowserIcon::GenericFile,
        }
    }
}

/// Key two root paths are considered "the same folder" by.
///
/// Canonicalised where the filesystem allows it, so `C:\Users\me\Music` and a
/// junction pointing at it collapse to one entry; the raw path otherwise,
/// because a root that cannot be resolved is still a root worth listing once.
fn dedupe_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn placeholder_row(dir: &Path, depth: usize, label: String, is_error: bool) -> BrowserVisibleNode {
    let label_key = if is_error {
        directory_error_label_key(&label)
    } else {
        Some("browser.loading")
    };
    BrowserVisibleNode {
        id: format!(
            "{}:{}",
            if is_error { "error" } else { "loading" },
            dir.display()
        ),
        label,
        label_key,
        path: None,
        kind: BrowserNodeKind::Info,
        depth,
        extension: String::new(),
        expandable: false,
        expanded: false,
        selected: false,
        error: if is_error {
            Some("Cannot read folder".to_string())
        } else {
            None
        },
        size_bytes: None,
        icon: BrowserIcon::None,
        detail: None,
    }
}

fn is_audio_extension(ext: &str) -> bool {
    matches!(
        ext,
        "wav" | "wave" | "mp3" | "flac" | "ogg" | "oga" | "m4a" | "aiff" | "aif"
    )
}

/// Whether a path points at an audio file the browser can preview/import.
/// Shared by the auto-preview trigger and the mini waveform pane.
pub fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .map(|ext| is_audio_extension(&ext))
        .unwrap_or(false)
}

/// Human-readable file size.
///
/// Fixed one-decimal form above 1 KB so the readout keeps a stable width and
/// the details pane never reflows while arrowing through a folder.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * KB;
    const GB: f64 = MB * KB;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < MB {
        format!("{:.1} KB", value / KB)
    } else if value < GB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{:.1} GB", value / GB)
    }
}

/// Enumerate logical drive letters on Windows, `/` plus mounted volumes on unix.
fn default_root_drives() -> Vec<BrowserRootSection> {
    let mut out = Vec::new();
    for path in enumerate_filesystem_roots() {
        let label = drive_label(&path);
        let id = format!("root:{}", path.display());
        out.push(BrowserRootSection {
            id,
            label,
            root_path: Some(path),
        });
    }
    out
}

#[cfg(target_os = "windows")]
fn enumerate_filesystem_roots() -> Vec<PathBuf> {
    extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();
    for i in 0u32..26 {
        if mask & (1 << i) != 0 {
            let letter = (b'A' + i as u8) as char;
            drives.push(PathBuf::from(format!("{}:\\", letter)));
        }
    }
    if drives.is_empty() {
        if let Some(home) = dirs::home_dir() {
            drives.push(home);
        }
    }
    drives
}

// Note: the user's home directory is surfaced separately as the friendly
// "Home" place (see `update_visible_nodes`), so it is intentionally not
// duplicated in the raw root list below.

#[cfg(target_os = "macos")]
fn enumerate_filesystem_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/")];
    let volumes = PathBuf::from("/Volumes");
    if let Ok(read) = std::fs::read_dir(&volumes) {
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    roots
}

#[cfg(all(unix, not(target_os = "macos")))]
fn enumerate_filesystem_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/")];
    for parent in ["/media", "/mnt", "/run/media"] {
        if let Ok(read) = std::fs::read_dir(parent) {
            for entry in read.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    roots.push(p);
                }
            }
        }
    }
    roots
}

fn drive_label(path: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        let s = path.to_string_lossy();
        let trimmed = s.trim_end_matches(|c| c == '\\' || c == '/');
        if trimmed.is_empty() {
            return s.into_owned();
        }
        return trimmed.to_string();
    }
    #[cfg(not(target_os = "windows"))]
    {
        if path == Path::new("/") {
            return "/".to_string();
        }
        path.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    }
}

/// Resolves standard folder paths for the file browser sidebar.
///
/// Delegates to [`crate::paths::FutureboardPaths`] — the centralized path
/// system. Directory creation is handled once at app startup via
/// `FutureboardPaths::ensure_user_dirs()`, not on every browser update.
pub fn resolve_standard_dirs() -> HashMap<String, PathBuf> {
    crate::paths::FutureboardPaths::resolve().standard_dirs()
}

/// Read directory into sorted entry lists. Treat .vst3 folders as files/plugins.
pub fn read_directory(path: &Path) -> (Vec<FileBrowserEntry>, Option<String>) {
    let read = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(error) => {
            return (
                Vec::new(),
                Some(directory_error_message(error.kind()).to_string()),
            );
        }
    };

    let mut folders: Vec<FileBrowserEntry> = Vec::new();
    let mut files: Vec<FileBrowserEntry> = Vec::new();

    for ent in read.flatten() {
        let p = ent.path();
        let name = match p.file_name().and_then(|s| s.to_str()) {
            Some(n) if !n.starts_with('.') => n.to_string(),
            _ => continue,
        };
        let meta = match ent.file_type() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if ext == "vst3" {
                files.push(FileBrowserEntry {
                    name,
                    path: p,
                    kind: FileEntryKind::File,
                    extension: ext,
                    size_bytes: None,
                });
            } else {
                folders.push(FileBrowserEntry {
                    name,
                    path: p,
                    kind: FileEntryKind::Folder,
                    extension: String::new(),
                    size_bytes: None,
                });
            }
        } else if meta.is_file() {
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            // `DirEntry::metadata` reuses the data the directory iteration
            // already returned on Windows, so this costs no extra syscall —
            // and it runs on the background executor either way.
            let size_bytes = ent.metadata().ok().map(|m| m.len());
            if ext == "pst" {
                let display = read_pst_plugin_name(&p).unwrap_or_else(|| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&name)
                        .to_string()
                });
                files.push(FileBrowserEntry {
                    name: display,
                    path: p,
                    kind: FileEntryKind::File,
                    extension: ext,
                    size_bytes,
                });
                continue;
            }
            files.push(FileBrowserEntry {
                name,
                path: p,
                kind: FileEntryKind::File,
                extension: ext,
                size_bytes,
            });
        }
    }

    folders.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    folders.extend(files);
    (folders, None)
}

fn directory_error_message(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::NotFound => "Folder is not available",
        std::io::ErrorKind::PermissionDenied => "Permission denied",
        _ => "Unable to read this folder",
    }
}

/// Message key for a stored directory error.
///
/// The cache stores the English text rather than a key because the same string
/// is handed to the background-task panel as a failure reason; this maps it
/// back so the browser row can still be translated.
pub fn directory_error_label_key(message: &str) -> Option<&'static str> {
    match message {
        "Folder is not available" => Some("browser.error.not-available"),
        "Permission denied" => Some("browser.error.permission-denied"),
        "Unable to read this folder" => Some("browser.error.unreadable"),
        _ => None,
    }
}

/// Sniff the display name out of an `FBPST` preset header.
///
/// Bounded reads only: a preset folder holds hundreds of files and some of them
/// are megabytes, so reading each one whole to parse a 24-byte header is what
/// made scanning the plug-in root feel slow.
fn read_pst_plugin_name(path: &Path) -> Option<String> {
    /// Refuse to allocate for a corrupt header claiming an absurd metadata
    /// length. Real preset metadata is a few hundred bytes.
    const MAX_META_LEN: usize = 64 * 1024;

    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header).ok()?;
    if &header[0..5] != b"FBPST" {
        return None;
    }
    let meta_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if meta_len == 0 || meta_len > MAX_META_LEN {
        return None;
    }
    let mut meta = vec![0u8; meta_len];
    file.read_exact(&mut meta).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&meta).ok()?;
    value
        .get("pluginMetadata")
        .and_then(|m| m.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(state: &FileBrowserState) -> Vec<String> {
        state.visible_nodes.iter().map(|n| n.id.clone()).collect()
    }

    /// The tree may show one folder in two places, but it may never show two
    /// rows the UI cannot tell apart: a duplicate id makes the second row
    /// inherit the first's hit-testing, so clicking one folder opens another.
    #[test]
    fn every_visible_row_has_its_own_id() {
        let mut state = FileBrowserState::default();
        let dir = std::env::temp_dir().join("futureboard-browser-id-test");
        let child = dir.join("child");
        state.expanded_paths.insert(dir.clone());
        // The same folder reached through two roots is the case that used to
        // collide: both children were identified by their path alone.
        state.apply_loaded(
            dir.clone(),
            vec![FileBrowserEntry {
                name: "child".to_string(),
                path: child,
                kind: FileEntryKind::Folder,
                extension: String::new(),
                size_bytes: None,
            }],
        );
        let mut nodes = Vec::new();
        let mut seen = HashSet::new();
        state.push_root(
            "test:one",
            "One",
            None,
            &dir,
            BrowserIcon::Folder,
            &mut seen,
            &mut nodes,
        );
        // A second root on the same folder is refused outright.
        state.push_root(
            "test:two",
            "Two",
            None,
            &dir,
            BrowserIcon::Folder,
            &mut seen,
            &mut nodes,
        );
        assert_eq!(
            nodes.iter().filter(|n| n.depth == 1).count(),
            1,
            "the same folder must not be listed as two roots"
        );

        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "duplicate row ids in {ids:?}");
    }

    /// Two roots that resolve to the same folder collapse to one row rather
    /// than listing it twice and sharing its expansion state — on every tab.
    #[test]
    fn roots_pointing_at_one_folder_are_listed_once() {
        let mut state = FileBrowserState::default();
        for tab in BrowserTab::ALL {
            state.set_tab(tab);
            let ids = ids(&state);
            let paths: Vec<PathBuf> = state
                .visible_nodes
                .iter()
                .filter(|node| node.depth == 1 && node.kind == BrowserNodeKind::Folder)
                .filter_map(|node| node.path.clone())
                .map(|path| dedupe_key(&path))
                .collect();
            let unique: HashSet<PathBuf> = paths.iter().cloned().collect();
            assert_eq!(
                unique.len(),
                paths.len(),
                "{tab:?}: a folder is listed under more than one root: {ids:?}"
            );
        }
    }

    fn instrument(vendor: &str, name: &str, format: &str) -> BrowserInstrument {
        BrowserInstrument {
            name: name.to_string(),
            vendor: vendor.to_string(),
            format: format.to_string(),
            category: "Synth".to_string(),
            preset_path: PathBuf::from(format!("C:/presets/{name}.pst")),
        }
    }

    /// Each tab lists its own things and nothing else: the Instruments tab no
    /// folders, the Files tab no library roots.
    #[test]
    fn each_tab_lists_only_its_own_roots() {
        let mut state = FileBrowserState::default();
        assert_eq!(state.tab, BrowserTab::Instruments);
        assert!(
            state
                .visible_nodes
                .iter()
                .all(|n| n.kind != BrowserNodeKind::Folder),
            "the Instruments tab lists no folders"
        );

        state.set_tab(BrowserTab::Files);
        let files = ids(&state);
        assert!(files.iter().any(|id| id == "group:places"));
        assert!(!files.iter().any(|id| id.starts_with("lib:")));

        state.set_tab(BrowserTab::Samples);
        let samples = ids(&state);
        assert!(!samples.iter().any(|id| id.starts_with("places:")));
        assert!(!samples.iter().any(|id| id.starts_with("group:")));
    }

    /// Asking for the Instruments tab the first time asks for the database
    /// read; after the list arrives it does not ask again.
    #[test]
    fn the_instruments_tab_reads_the_database_once() {
        let mut state = FileBrowserState::default();
        state.set_tab(BrowserTab::Files);
        assert!(state.set_tab(BrowserTab::Instruments), "not read yet");
        state.mark_instruments_loading();
        assert!(!state.set_tab(BrowserTab::Instruments), "a read is running");
        state.apply_instruments(Ok(vec![instrument("Acme", "Synth", "VST3")]));
        assert!(!state.set_tab(BrowserTab::Instruments));
    }

    /// The Instruments tab is the database's instruments, one group per
    /// vendor, each row carrying the `.pst` the database points at — which is
    /// what a drag hands the arrangement.
    #[test]
    fn instruments_are_listed_by_vendor_with_their_pst() {
        let mut state = FileBrowserState::default();
        state.apply_instruments(Ok(vec![
            instrument("Acme", "Omega", "CLAP"),
            instrument("Beta Audio", "Alpha", "VST3"),
            instrument("Beta Audio", "Zeta", "VST3"),
        ]));
        let rows: Vec<(BrowserNodeKind, String, Option<String>)> = state
            .visible_nodes
            .iter()
            .map(|n| (n.kind, n.label.clone(), n.detail.clone()))
            .collect();
        assert_eq!(
            rows,
            [
                (BrowserNodeKind::GroupHeader, "Acme".to_string(), None),
                (
                    BrowserNodeKind::File,
                    "Omega".to_string(),
                    Some("CLAP".to_string())
                ),
                (BrowserNodeKind::GroupHeader, "Beta Audio".to_string(), None),
                (
                    BrowserNodeKind::File,
                    "Alpha".to_string(),
                    Some("VST3".to_string())
                ),
                (
                    BrowserNodeKind::File,
                    "Zeta".to_string(),
                    Some("VST3".to_string())
                ),
            ]
        );
        let alpha = state
            .visible_nodes
            .iter()
            .find(|n| n.label == "Alpha")
            .unwrap();
        assert_eq!(
            alpha.path.as_deref(),
            Some(Path::new("C:/presets/Alpha.pst"))
        );
        assert!(alpha.is_plugin_preset());
        assert!(
            state
                .instrument_for(Path::new("C:/presets/Alpha.pst"))
                .is_some()
        );

        // A vendor folds away like any group.
        state.toggle_node("group:vendor:Beta Audio", None);
        assert!(!ids(&state).iter().any(|id| id.contains("Alpha")));
    }

    /// Searching a vendor's name lists its instruments; searching an
    /// instrument's name keeps its vendor as the heading.
    #[test]
    fn instrument_search_matches_names_and_vendors() {
        let mut state = FileBrowserState::default();
        state.apply_instruments(Ok(vec![
            instrument("Acme", "Omega", "CLAP"),
            instrument("Beta Audio", "Alpha", "VST3"),
            instrument("Beta Audio", "Zeta", "VST3"),
        ]));
        state.set_filter("beta");
        let labels: Vec<String> = state
            .visible_nodes
            .iter()
            .map(|n| n.label.clone())
            .collect();
        assert_eq!(labels, ["Beta Audio", "Alpha", "Zeta"]);

        state.set_filter("ome");
        let labels: Vec<String> = state
            .visible_nodes
            .iter()
            .map(|n| n.label.clone())
            .collect();
        assert_eq!(labels, ["Acme", "Omega"]);
    }

    /// Before, during and after a failed read, and with nothing scanned, the
    /// tab says so in one row rather than showing nothing.
    #[test]
    fn the_instruments_tab_says_what_it_is_waiting_for() {
        let mut state = FileBrowserState::default();
        let status = |state: &FileBrowserState| {
            let rows = &state.visible_nodes;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].kind, BrowserNodeKind::Info);
            rows[0].label_key
        };
        assert_eq!(status(&state), Some("browser.instruments.loading"));
        state.apply_instruments(Ok(Vec::new()));
        assert_eq!(status(&state), Some("browser.instruments.empty"));
        state.apply_instruments(Err("locked".to_string()));
        assert_eq!(status(&state), Some("browser.instruments.failed"));
        assert_eq!(state.visible_nodes[0].error.as_deref(), Some("locked"));
    }

    #[test]
    fn preview_playhead_belongs_to_the_auditioned_file_only() {
        let mut state = FileBrowserState::default();
        let kick = PathBuf::from("C:/samples/kick.wav");
        let snare = PathBuf::from("C:/samples/snare.wav");

        state.set_preview_playing(kick.clone());
        assert_eq!(
            state.preview_position_for(&kick),
            None,
            "no playhead before the engine reports one"
        );

        assert!(state.apply_preview_position(Some(1.25)));
        assert_eq!(state.preview_position_for(&kick), Some(1.25));
        assert_eq!(
            state.preview_position_for(&snare),
            None,
            "another file must never inherit the playhead"
        );

        // Same position twice is not a repaint reason.
        assert!(!state.apply_preview_position(Some(1.25)));

        // The voice retiring clears both the playhead and its owner.
        assert!(state.apply_preview_position(None));
        assert_eq!(state.preview_position_for(&kick), None);
        assert!(!state.apply_preview_position(None), "idle stays quiet");
    }

    #[test]
    fn collapsing_a_group_hides_its_items() {
        let mut state = FileBrowserState::default();
        state.set_tab(BrowserTab::Files);
        assert!(ids(&state).iter().any(|id| id.starts_with("places:")));
        // Toggle the Places group closed.
        let expanded = state.toggle_node("group:places", None);
        assert!(!expanded, "group should collapse on first toggle");
        assert!(
            !ids(&state).iter().any(|id| id.starts_with("places:")),
            "place items must be hidden while the group is collapsed"
        );
        // The header itself stays visible and reflects the collapsed state.
        let header = state
            .visible_nodes
            .iter()
            .find(|n| n.id == "group:places")
            .unwrap();
        assert!(!header.expanded);
    }

    #[test]
    fn directory_errors_are_safe_for_browser_display() {
        assert_eq!(
            directory_error_message(std::io::ErrorKind::NotFound),
            "Folder is not available"
        );
        assert_eq!(
            directory_error_message(std::io::ErrorKind::PermissionDenied),
            "Permission denied"
        );
        // Every message the browser can store resolves back to a message key,
        // so an error row is never stuck in English.
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Other,
        ] {
            assert!(directory_error_label_key(directory_error_message(kind)).is_some());
        }
    }

    #[test]
    fn collapse_all_clears_folder_and_category_expansion() {
        let mut state = FileBrowserState::default();
        state.expanded_paths.insert(PathBuf::from("/tmp/example"));
        state.toggle_node("collections:recent", None);
        assert!(!state.expanded_virtual.is_empty());
        state.collapse_all();
        assert!(state.expanded_paths.is_empty());
        assert!(state.expanded_virtual.is_empty());
    }

    #[test]
    fn arrow_right_on_a_file_does_not_queue_a_directory_scan() {
        let mut state = FileBrowserState::default();
        let dir = PathBuf::from("/tmp/browser-test");
        let file = dir.join("kick.wav");
        state.expanded_paths.insert(dir.clone());
        state.apply_loaded(
            dir.clone(),
            vec![FileBrowserEntry {
                name: "kick.wav".to_string(),
                path: file.clone(),
                kind: FileEntryKind::File,
                extension: "wav".to_string(),
                size_bytes: Some(2048),
            }],
        );
        state.select(file.clone());
        assert!(!state.expand_selected(), "a file cannot expand");
        assert!(
            !state.expanded_paths.contains(&file),
            "a file must never enter the expanded set"
        );
        assert!(
            state.paths_needing_load().iter().all(|p| p != &file),
            "no directory scan may be queued for a file"
        );
    }

    #[test]
    fn failed_directories_are_not_rescanned_on_every_drain() {
        let mut state = FileBrowserState::default();
        let dir = PathBuf::from("/tmp/unreadable");
        state.expanded_paths.insert(dir.clone());
        assert!(state.paths_needing_load().contains(&dir));
        state.apply_error(dir.clone(), "Permission denied".to_string());
        assert!(
            !state.paths_needing_load().contains(&dir),
            "a failed folder must not be re-dispatched until the user retries"
        );
        // An explicit retry (context Refresh / Rescan) clears the error.
        state.mark_loading(dir.clone());
        state.apply_loaded(dir.clone(), Vec::new());
        assert!(!state.paths_needing_load().contains(&dir));
    }

    #[test]
    fn type_ahead_buffer_accumulates_then_expires() {
        let mut state = FileBrowserState::default();
        let t0 = Instant::now();
        state.type_ahead('K', t0);
        assert_eq!(state.type_ahead, "k", "letters are lowercased");
        state.type_ahead('I', t0 + Duration::from_millis(100));
        assert_eq!(state.type_ahead, "ki");
        // A long pause starts a fresh search instead of extending a stale one.
        state.type_ahead(
            'S',
            t0 + Duration::from_millis(100) + TYPE_AHEAD_RESET + Duration::from_millis(1),
        );
        assert_eq!(state.type_ahead, "s");
        state.clear_type_ahead();
        assert!(state.type_ahead.is_empty());
    }

    #[test]
    fn format_size_keeps_a_stable_width_above_one_kilobyte() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
