//! Workspace layout persistence — panel visibility, sizes, and active tabs.
//!
//! Persisted to `<app_data>/workspace_layout.json` on clean shutdown and
//! restored at startup. Designed to be forward-compatible: unknown fields
//! are silently ignored by `serde(default)`, so older files load without error
//! in a newer build.
//!
//! # What is saved
//!
//! - `panels` — which side/bottom panels are visible
//! - `bottom_panel_height_px` — bottom dock height after user resize
//! - `active_bottom_tab` — Mixer / Editor / EffectEditor selection
//! - `right_dock_tab` — Inspector / Chords / Lyrics / Solfege selection
//! - `mixer_*` — mixer section heights and tree sidebar width
//! - `secondary_windows` — bounds of detached windows (mixer, settings, clock)
//!
//! # What is NOT saved
//!
//! Transient overlay state (popovers, drag cursors, text selection), audio
//! engine state (transport, recording), project data, plugin editor positions,
//! or anything that belongs in the project file.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::FutureboardPaths;

// ── Saved panel visibility ────────────────────────────────────────────────────

/// Serialized mirror of `StudioPanelVisibility`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPanelVisibility {
    #[serde(default = "default_true")]
    pub browser: bool,
    #[serde(default = "default_true")]
    pub inspector: bool,
    #[serde(default = "default_true")]
    pub bottom_docked: bool,
}

impl Default for SavedPanelVisibility {
    fn default() -> Self {
        Self {
            browser: true,
            inspector: true,
            bottom_docked: true,
        }
    }
}

// ── Saved bottom tab ──────────────────────────────────────────────────────────

/// Serialized mirror of `BottomTab` (Mixer / Editor / EffectEditor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedBottomTab {
    Mixer,
    Editor,
    EffectEditor,
}

impl Default for SavedBottomTab {
    fn default() -> Self {
        Self::Mixer
    }
}

// ── Saved right dock tab ──────────────────────────────────────────────────────

/// Serialized mirror of `RightDockTab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedRightDockTab {
    Inspector,
    ChordDisplay,
    LyricDisplay,
    LyricEditor,
    Solfege,
}

impl Default for SavedRightDockTab {
    fn default() -> Self {
        Self::Inspector
    }
}

// ── Saved mixer view ──────────────────────────────────────────────────────────

/// Persisted mixer panel geometry (section heights, scroll, sidebar).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedMixerView {
    /// Horizontal scroll offset (logical pixels).
    #[serde(default)]
    pub scroll_x: f32,
    /// Insert-slot section height (logical pixels).
    #[serde(default = "default_mixer_insert_section")]
    pub insert_section_px: f32,
    /// Send-slot section height (logical pixels).
    #[serde(default = "default_mixer_send_section")]
    pub send_section_px: f32,
    /// Mixer-tree sidebar width (logical pixels).
    #[serde(default = "default_mixer_tree_sidebar")]
    pub tree_sidebar_width_px: f32,
}

impl Default for SavedMixerView {
    fn default() -> Self {
        Self {
            scroll_x: 0.0,
            insert_section_px: default_mixer_insert_section(),
            send_section_px: default_mixer_send_section(),
            tree_sidebar_width_px: default_mixer_tree_sidebar(),
        }
    }
}

// ── Saved secondary window bounds ─────────────────────────────────────────────

/// Screen-space bounds for an independent (detached) window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWindowBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Saved positions of optional detached studio windows.
///
/// All fields are `Option` — a window that was not open when the layout was
/// saved simply won't be reopened on restore.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedSecondaryWindows {
    /// Detached mixer window bounds (present when mixer was undocked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mixer: Option<SavedWindowBounds>,
    /// Big clock window bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub big_clock: Option<SavedWindowBounds>,
    /// Timecode display window bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timecode: Option<SavedWindowBounds>,
}

// ── Top-level workspace layout ────────────────────────────────────────────────

/// Complete workspace state snapshot, written to `workspace_layout.json`.
///
/// All fields carry `#[serde(default)]` so any missing key in an older file
/// silently falls back to the struct default instead of failing the load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWorkspaceLayout {
    /// Schema version. Increment when breaking changes require a migration.
    #[serde(default)]
    pub version: u32,
    /// Which docked panels are visible.
    #[serde(default)]
    pub panels: SavedPanelVisibility,
    /// Bottom panel height in logical pixels.
    #[serde(default = "default_bottom_panel_height")]
    pub bottom_panel_height_px: f32,
    /// Which bottom tab was active.
    #[serde(default)]
    pub active_bottom_tab: SavedBottomTab,
    /// Which right-dock tab was active.
    #[serde(default)]
    pub right_dock_tab: SavedRightDockTab,
    /// Mixer panel view state.
    #[serde(default)]
    pub mixer: SavedMixerView,
    /// Detached / secondary window positions saved on last shutdown.
    #[serde(default)]
    pub secondary_windows: SavedSecondaryWindows,
}

impl Default for SavedWorkspaceLayout {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            panels: SavedPanelVisibility::default(),
            bottom_panel_height_px: default_bottom_panel_height(),
            active_bottom_tab: SavedBottomTab::default(),
            right_dock_tab: SavedRightDockTab::default(),
            mixer: SavedMixerView::default(),
            secondary_windows: SavedSecondaryWindows::default(),
        }
    }
}

// ── Constants and serde helpers ───────────────────────────────────────────────

const CURRENT_VERSION: u32 = 1;

fn default_true() -> bool {
    true
}
fn default_bottom_panel_height() -> f32 {
    280.0
}
fn default_mixer_insert_section() -> f32 {
    120.0
}
fn default_mixer_send_section() -> f32 {
    80.0
}
fn default_mixer_tree_sidebar() -> f32 {
    180.0
}

// ── I/O ───────────────────────────────────────────────────────────────────────

fn workspace_layout_file() -> PathBuf {
    FutureboardPaths::resolve().workspace_layout_file
}

fn layout_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_LAYOUT_DEBUG").is_some())
}

fn log_layout(msg: &str) {
    if layout_debug_enabled() {
        eprintln!("[workspace-layout] {msg}");
    }
}

/// Load the saved workspace layout from disk.
///
/// Returns `None` when the file is absent, unreadable, or unparseable.
/// Missing / extra fields are tolerated via `serde(default)`.
pub fn load_workspace_layout() -> Option<SavedWorkspaceLayout> {
    let path = workspace_layout_file();
    let content = fs::read_to_string(&path)
        .map_err(|e| {
            log_layout(&format!("load: read error: {e} path={}", path.display()));
        })
        .ok()?;
    let layout: SavedWorkspaceLayout = serde_json::from_str(&content)
        .map_err(|e| {
            log_layout(&format!("load: parse error: {e}"));
        })
        .ok()?;
    log_layout(&format!(
        "load: ok version={} bottom_height={:.0}",
        layout.version, layout.bottom_panel_height_px
    ));
    Some(layout)
}

/// Validate and clamp a loaded layout in place.
///
/// Resets any value that is out of the accepted range to its default so
/// corrupt files don't leave the UI in an undefined state.
pub fn sanitize_workspace_layout(layout: &mut SavedWorkspaceLayout) {
    // Bottom panel height: clamp to a sensible range.
    layout.bottom_panel_height_px = layout
        .bottom_panel_height_px
        .clamp(MIN_BOTTOM_PANEL_HEIGHT, MAX_BOTTOM_PANEL_HEIGHT);
    if !layout.bottom_panel_height_px.is_finite() {
        layout.bottom_panel_height_px = default_bottom_panel_height();
    }
    // Mixer section heights.
    if !layout.mixer.insert_section_px.is_finite() || layout.mixer.insert_section_px < 0.0 {
        layout.mixer.insert_section_px = default_mixer_insert_section();
    }
    if !layout.mixer.send_section_px.is_finite() || layout.mixer.send_section_px < 0.0 {
        layout.mixer.send_section_px = default_mixer_send_section();
    }
    if !layout.mixer.tree_sidebar_width_px.is_finite()
        || layout.mixer.tree_sidebar_width_px < 10.0
    {
        layout.mixer.tree_sidebar_width_px = default_mixer_tree_sidebar();
    }
    if !layout.mixer.scroll_x.is_finite() {
        layout.mixer.scroll_x = 0.0;
    }
    // Secondary window bounds: drop degenerate entries.
    if let Some(ref b) = layout.secondary_windows.mixer {
        if !bounds_valid(b) {
            layout.secondary_windows.mixer = None;
        }
    }
    if let Some(ref b) = layout.secondary_windows.big_clock {
        if !bounds_valid(b) {
            layout.secondary_windows.big_clock = None;
        }
    }
    if let Some(ref b) = layout.secondary_windows.timecode {
        if !bounds_valid(b) {
            layout.secondary_windows.timecode = None;
        }
    }
}

fn bounds_valid(b: &SavedWindowBounds) -> bool {
    b.x.is_finite()
        && b.y.is_finite()
        && b.width.is_finite()
        && b.height.is_finite()
        && b.width > MIN_SECONDARY_WINDOW_SIZE
        && b.height > MIN_SECONDARY_WINDOW_SIZE
}

/// Save the workspace layout to disk atomically (write tmp → rename).
///
/// Silently logs and returns on any I/O or serialisation error so a save
/// failure never interrupts the shutdown sequence.
pub fn save_workspace_layout(layout: &SavedWorkspaceLayout) {
    let path = workspace_layout_file();
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log_layout(&format!("save: create_dir_all failed: {e}"));
            return;
        }
    }
    let json = match serde_json::to_string_pretty(layout) {
        Ok(s) => s,
        Err(e) => {
            log_layout(&format!("save: serialize failed: {e}"));
            return;
        }
    };
    // Atomic write: tmp → rename.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, &json) {
        log_layout(&format!("save: write tmp failed: {e} path={}", tmp.display()));
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        log_layout(&format!("save: rename failed: {e}"));
        // Best-effort: leave tmp in place so the data isn't lost entirely.
        return;
    }
    log_layout(&format!(
        "save: ok bottom_height={:.0} path={}",
        layout.bottom_panel_height_px,
        path.display()
    ));
}

/// Convenience: load, sanitize, and return the workspace layout (or default).
pub fn load_or_default_workspace_layout() -> SavedWorkspaceLayout {
    let mut layout = load_workspace_layout().unwrap_or_default();
    sanitize_workspace_layout(&mut layout);
    layout
}

// ── Bounds conversion helpers (for callers without GPUI imports) ──────────────

impl SavedWindowBounds {
    /// Convert from GPUI logical [`gpui::Bounds<gpui::Pixels>`].
    pub fn from_gpui(b: gpui::Bounds<gpui::Pixels>) -> Self {
        Self {
            x: b.origin.x.into(),
            y: b.origin.y.into(),
            width: b.size.width.into(),
            height: b.size.height.into(),
        }
    }

    /// Convert back to GPUI logical bounds.
    pub fn to_gpui(&self) -> gpui::Bounds<gpui::Pixels> {
        gpui::bounds(
            gpui::point(gpui::px(self.x), gpui::px(self.y)),
            gpui::size(gpui::px(self.width), gpui::px(self.height)),
        )
    }
}

// ── Range limits ──────────────────────────────────────────────────────────────

const MIN_BOTTOM_PANEL_HEIGHT: f32 = 120.0;
const MAX_BOTTOM_PANEL_HEIGHT: f32 = 1200.0;
const MIN_SECONDARY_WINDOW_SIZE: f32 = 64.0;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_default() {
        let layout = SavedWorkspaceLayout::default();
        let json = serde_json::to_string(&layout).unwrap();
        let back: SavedWorkspaceLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, CURRENT_VERSION);
        assert!((back.bottom_panel_height_px - default_bottom_panel_height()).abs() < 0.1);
    }

    #[test]
    fn tolerates_unknown_fields() {
        let json = r#"{"version":0,"unknown_future_field":true,"panels":{"browser":false}}"#;
        let layout: SavedWorkspaceLayout = serde_json::from_str(json).unwrap();
        assert!(!layout.panels.browser);
        assert!(layout.panels.inspector); // default
    }

    #[test]
    fn sanitize_clamps_bad_heights() {
        let mut layout = SavedWorkspaceLayout {
            bottom_panel_height_px: -5.0,
            ..Default::default()
        };
        sanitize_workspace_layout(&mut layout);
        assert_eq!(layout.bottom_panel_height_px, MIN_BOTTOM_PANEL_HEIGHT);

        let mut layout2 = SavedWorkspaceLayout {
            bottom_panel_height_px: 9999.0,
            ..Default::default()
        };
        sanitize_workspace_layout(&mut layout2);
        assert_eq!(layout2.bottom_panel_height_px, MAX_BOTTOM_PANEL_HEIGHT);
    }

    #[test]
    fn sanitize_drops_bad_secondary_bounds() {
        let bad = SavedWindowBounds {
            x: f32::NAN,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut layout = SavedWorkspaceLayout {
            secondary_windows: SavedSecondaryWindows {
                mixer: Some(bad),
                big_clock: None,
                timecode: None,
            },
            ..Default::default()
        };
        sanitize_workspace_layout(&mut layout);
        assert!(layout.secondary_windows.mixer.is_none());
    }
}
