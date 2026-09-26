//! Audio Plug-in Manager — the library's maintenance surface.
//!
//! Where the Add Insert picker is for *using* plug-ins, this window is for
//! *keeping the library healthy*: scanning, seeing what failed, registering
//! what was skipped, and finding a plug-in's files. So the layout leads with
//! the library's state and its one primary action:
//!
//! ```txt
//! summary · search · Scan · ⋯        ← how the library stands, and Scan
//! [scan progress]                    ← only while scanning
//! rail │ table │ details             ← filter │ plug-ins │ the selected one
//! status bar                         ← last result, database location
//! ```
//!
//! Every action here is real. The details pane used to offer "Insert on
//! Selected Track" and "Open Plug-in Editor", which only printed "not connected
//! yet", and the rail offered a permanently disabled "Add Location"; those are
//! gone rather than dressed up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, svg, uniform_list, App, AppContext, Bounds, ClipboardItem, Context, Entity,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Rgba,
    ScrollHandle, ScrollStrategy, StatefulInteractiveElement, Styled, UniformListScrollHandle,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};
use SpherePluginHost::load_au_cache_state;
use SpherePluginHost::preset::register_plugin;
use SpherePluginHost::registry::{
    NativeHostStatus, PluginFormat, PluginKind, PluginRegistry, PluginStatus, RegistryPlugin,
    RegistryScanResult, ScanOptions, ScanProgress,
};
use SpherePluginHost::PluginScanStatus;

use crate::assets;
use crate::components::context_menu::{context_menu_overlay, ContextMenuEntry};
use crate::components::controls::{
    fb_button, fb_icon_button, fb_progress, fb_tooltip, FbButtonKind,
};
use crate::components::plugin_format_badge::plugin_format_badge;
use crate::components::progress_dialog::{
    open_progress_dialog_window, open_standalone_progress_dialog_window, ProgressBarValue,
    ProgressDialogCancelCb, ProgressDialogOptions,
};
use crate::components::scroll_thumb::vertical_scrollbar_thumb;
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks_and_ime, TextInputCallbacks, TextInputState,
};
use crate::components::title_bar::external_window_titlebar;
use crate::i18n::I18n;
use crate::theme::{self, radius, size, space, state as state_tokens, typography, Colors};

pub const PLUGIN_MANAGER_WINDOW_WIDTH: f32 = 1080.0;
pub const PLUGIN_MANAGER_WINDOW_HEIGHT: f32 = 680.0;
pub const PLUGIN_MANAGER_WINDOW_MIN_WIDTH: f32 = 880.0;
pub const PLUGIN_MANAGER_WINDOW_MIN_HEIGHT: f32 = 520.0;

type VoidCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;
type StrCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

const RAIL_WIDTH: f32 = 196.0;
const DETAILS_WIDTH: f32 = 300.0;
const SEARCH_WIDTH: f32 = 260.0;

/// Table column metrics, read by both the header and the rows.
const COL_KIND: f32 = 14.0;
const COL_NAME_MIN: f32 = 160.0;
const COL_VENDOR_W: f32 = 150.0;
const COL_CATEGORY_W: f32 = 110.0;
const COL_FORMAT_W: f32 = 64.0;
const COL_STATUS_W: f32 = 120.0;
const COL_GAP: f32 = space::BASE;
const ROW_PAD_X: f32 = space::LOOSE;
const ROW_HEIGHT: f32 = size::COMFORTABLE;
/// Truncation width for the database path in the status bar, so a deep
/// install directory cannot push the counters off the window.
const DB_PATH_MAX_W: f32 = 360.0;

// Commands of the "More" menu.
const CMD_RESCAN_ALL: &str = "plugin-manager:rescan-all";
const CMD_RESCAN_AU: &str = "plugin-manager:rescan-au";
const CMD_OPEN_DB: &str = "plugin-manager:open-db-folder";
const CMD_CLEAR_DB: &str = "plugin-manager:clear-database";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Vendor,
    Category,
    Format,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarFilter {
    All,
    /// Plug-ins the user has to do something about: not registered yet,
    /// failed to scan, or crashed the scanner.
    Attention,
    Instrument,
    Effect,
    Format(PluginFormat),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FilterCounts {
    pub all: usize,
    pub attention: usize,
    pub instruments: usize,
    pub effects: usize,
    pub vst3: usize,
    pub vst2: usize,
    pub clap: usize,
    pub au: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginScanMode {
    /// Scan folders and register missing `.pst` files (overwrites on conflict).
    Rescan,
    /// Delete all `.pst` presets, clear the list, then scan and register everything.
    RescanAll,
    /// Scan AudioUnit plug-ins only (macOS).
    RescanAu,
}

/// What state a plug-in is in, from the user's side: can it be used, and if
/// not, what went wrong. Ordered from healthy to worst for sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Health {
    Ready,
    Unsupported,
    Disabled,
    NotRegistered,
    Failed,
    Crashed,
}

impl Health {
    fn of(plugin: &RegistryPlugin) -> Self {
        match plugin.scan_status {
            PluginScanStatus::Crashed => Health::Crashed,
            PluginScanStatus::Failed | PluginScanStatus::MetadataOnly => Health::Failed,
            PluginScanStatus::Skipped | PluginScanStatus::Disabled => Health::Disabled,
            _ if plugin.status == PluginStatus::MissingPreset => Health::NotRegistered,
            _ if !plugin.supports_insert() => Health::Unsupported,
            _ => Health::Ready,
        }
    }

    /// Something the user can act on. Unsupported and disabled plug-ins are
    /// states, not problems to fix from here.
    fn needs_attention(self) -> bool {
        matches!(
            self,
            Health::NotRegistered | Health::Failed | Health::Crashed
        )
    }

    fn label(self, i18n: I18n) -> String {
        i18n.tr(match self {
            Health::Ready => "plugin-manager.status.ready",
            Health::Unsupported => "plugin-manager.status.unsupported",
            Health::Disabled => "plugin-manager.status.disabled",
            Health::NotRegistered => "plugin-manager.status.not-registered",
            Health::Failed => "plugin-manager.status.failed",
            Health::Crashed => "plugin-manager.status.crashed",
        })
    }

    fn explanation(self, i18n: I18n) -> Option<String> {
        let key = match self {
            Health::Ready => return None,
            Health::Unsupported => "plugin-manager.health.unsupported",
            Health::Disabled => "plugin-manager.health.disabled",
            Health::NotRegistered => "plugin-manager.health.not-registered",
            Health::Failed => "plugin-manager.health.failed",
            Health::Crashed => "plugin-manager.health.crashed",
        };
        Some(i18n.tr(key))
    }

    fn tone(self) -> Rgba {
        match self {
            Health::Ready => Colors::status_success(),
            Health::Unsupported | Health::Disabled => Colors::text_faint(),
            Health::NotRegistered => Colors::status_warning(),
            Health::Failed | Health::Crashed => Colors::status_error(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginManagerDialogState {
    pub plugins: Vec<RegistryPlugin>,
    pub scan_paths: Vec<PathBuf>,
    pub status_text: String,
    pub scanning: bool,
    pub failed_count: u32,
    pub generated_presets: u32,
    pub scan_progress_current: usize,
    pub scan_progress_total: usize,
    pub scan_progress_label: String,
    pub sidebar_filter: SidebarFilter,
    pub sort_key: SortKey,
    pub sort_dir: SortDir,
    pub selected_id: Option<String>,
    pub host: NativeHostStatus,
    /// `created_at_ms` of the most recent `.pst` in the cache. `0` = no cache.
    pub last_scan_at_ms: i64,
    /// True once the cached index has been loaded (or determined to be empty).
    pub cache_loaded: bool,
    pub au_scan_available: bool,
    pub au_scan_error: Option<String>,
    pub au_auto_scan_disabled: bool,
}

impl PluginManagerDialogState {
    pub fn new_empty(i18n: I18n) -> Self {
        let host = PluginRegistry::host_status();
        let au_cache = load_au_cache_state();
        let status_text = if host.available {
            // No dedicated FTL key; closest empty-registry copy until cache load finishes.
            i18n.tr("plugin-manager.list.empty")
        } else {
            host.message.clone()
        };
        Self {
            scan_paths: host.default_scan_paths.clone(),
            status_text,
            scanning: false,
            failed_count: 0,
            generated_presets: 0,
            scan_progress_current: 0,
            scan_progress_total: 0,
            scan_progress_label: String::new(),
            plugins: Vec::new(),
            sidebar_filter: SidebarFilter::All,
            sort_key: SortKey::Name,
            sort_dir: SortDir::Asc,
            selected_id: None,
            host,
            last_scan_at_ms: 0,
            cache_loaded: false,
            au_scan_available: cfg!(target_os = "macos"),
            au_scan_error: au_cache.last_error.clone(),
            au_auto_scan_disabled: au_cache.auto_scan_disabled,
        }
    }

    /// Apply a cached `.pst` load to the dialog. Does not touch any plug-in
    /// binary or trigger an SDK scan.
    pub fn apply_cache_load(
        &mut self,
        plugins: Vec<RegistryPlugin>,
        last_scan_at_ms: i64,
        i18n: I18n,
    ) {
        self.failed_count = PluginRegistry::cached_failed_count(&plugins);
        self.last_scan_at_ms = last_scan_at_ms;
        let count = plugins.len();
        self.plugins = plugins;
        self.cache_loaded = true;
        self.scanning = false;
        self.status_text = if count == 0 {
            i18n.tr("plugin-manager.list.empty")
        } else {
            i18n.tr_vars("plugin-manager.scan.found", &[("count", count.to_string())])
        };
    }

    pub fn apply_scan_result(&mut self, result: RegistryScanResult, i18n: I18n) {
        self.host = PluginRegistry::host_status();
        self.plugins = result.plugins;
        self.scan_paths = result.scanned_paths;
        self.failed_count = result.failed.len() as u32;
        self.generated_presets = result.generated_presets;
        self.au_scan_available = result.au_scan_available;
        self.au_scan_error = result.au_scan_error.clone();
        self.au_auto_scan_disabled = result.au_auto_scan_disabled;
        self.scanning = false;
        self.cache_loaded = true;
        self.last_scan_at_ms = self
            .plugins
            .iter()
            .map(|p| p.scanned_at_ms)
            .max()
            .unwrap_or(0);
        self.scan_progress_current = 0;
        self.scan_progress_total = 0;
        self.scan_progress_label.clear();

        let count = self.plugins.len();
        self.status_text = if let Some(au_error) = &result.au_scan_error {
            if count > 0 {
                format!("AudioUnit scan failed. VST3/CLAP results are still available. {au_error}")
            } else if self.failed_count > 0 {
                format!(
                    "{} {au_error}",
                    i18n.tr_vars(
                        "plugin-manager.scan.path-errors",
                        &[("n", self.failed_count.to_string())],
                    )
                )
            } else {
                format!("AudioUnit scan failed. {au_error}")
            }
        } else if count == 0 && self.failed_count > 0 {
            i18n.tr_vars(
                "plugin-manager.scan.path-errors",
                &[("n", self.failed_count.to_string())],
            )
        } else if count == 0 {
            i18n.tr("plugin-manager.scan.none-found")
        } else if self.failed_count > 0 {
            i18n.tr_vars(
                "plugin-manager.scan.found-with-errors",
                &[
                    ("count", count.to_string()),
                    ("errors", self.failed_count.to_string()),
                ],
            )
        } else if self.generated_presets > 0 {
            i18n.tr_vars(
                "plugin-manager.scan.registered",
                &[
                    ("presets", self.generated_presets.to_string()),
                    ("count", count.to_string()),
                ],
            )
        } else {
            i18n.tr_vars("plugin-manager.scan.found", &[("count", count.to_string())])
        };

        if let Some(id) = &self.selected_id {
            if !self.plugins.iter().any(|p| &p.id == id) {
                self.selected_id = None;
            }
        }
    }

    pub fn begin_scan(&mut self, mode: PluginScanMode, i18n: I18n) {
        self.scanning = true;
        self.scan_progress_current = 0;
        self.scan_progress_total = 0;
        self.scan_progress_label.clear();
        self.failed_count = 0;
        if mode == PluginScanMode::RescanAll {
            self.plugins.clear();
            self.generated_presets = 0;
        }
        self.au_scan_error = None;
        self.status_text = match mode {
            PluginScanMode::Rescan => i18n.tr("plugin-manager.scan.in-progress"),
            PluginScanMode::RescanAll => i18n.tr("plugin-manager.scan.rescan-all"),
            PluginScanMode::RescanAu => i18n.tr("plugin-manager.scan.in-progress"),
        };
    }

    pub fn apply_scan_progress(&mut self, progress: &ScanProgress, i18n: I18n) {
        match progress {
            ScanProgress::Started { bundle_total } => {
                self.scan_progress_total = *bundle_total;
                self.scan_progress_current = 0;
                self.scan_progress_label = i18n.tr("plugin-manager.scan.discovering");
            }
            ScanProgress::ScanningBundle {
                current,
                total,
                path,
            } => {
                self.scan_progress_current = *current;
                self.scan_progress_total = *total;
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("bundle")
                    .to_string();
                self.scan_progress_label =
                    i18n.tr_vars("plugin-manager.scan.reading", &[("name", name)]);
            }
            ScanProgress::Registering {
                current,
                total,
                name,
                plugin,
                generated_presets,
            } => {
                self.scan_progress_current = *current;
                self.scan_progress_total = *total;
                self.scan_progress_label = name.clone();
                self.generated_presets = *generated_presets;
                if let Some(existing) = self.plugins.iter_mut().find(|p| p.id == plugin.id) {
                    *existing = plugin.clone();
                } else {
                    self.plugins.push(plugin.clone());
                }
            }
            ScanProgress::Failed { .. } => {}
            ScanProgress::FormatFinished {
                format,
                success_count,
                failed_count,
                crashed_count,
                error,
            } => {
                if *format == PluginFormat::Au {
                    self.au_scan_error = error.clone();
                    if *crashed_count > 0 {
                        self.status_text = "AudioUnit scan process crashed. VST3/CLAP results are still available.".to_string();
                    } else if let Some(message) = error {
                        self.status_text = format!(
                            "AudioUnit scan failed ({success_count} ok, {failed_count} failed): {message}"
                        );
                    }
                }
            }
        }
    }

    pub fn scan_progress_fraction(&self) -> f32 {
        if self.scan_progress_total == 0 {
            return 0.0;
        }
        (self.scan_progress_current as f32 / self.scan_progress_total as f32).clamp(0.0, 1.0)
    }

    pub fn counts(&self) -> FilterCounts {
        let mut counts = FilterCounts::default();
        for plugin in &self.plugins {
            counts.all += 1;
            if Health::of(plugin).needs_attention() {
                counts.attention += 1;
            }
            if plugin.kind == PluginKind::Instrument {
                counts.instruments += 1;
            }
            // Matches the Effect rail: a plug-in that declared no class is
            // usable as an insert, so it is counted and listed with effects.
            if plugin.kind.usable_as_effect() {
                counts.effects += 1;
            }
            match plugin.format {
                PluginFormat::Vst3 => counts.vst3 += 1,
                PluginFormat::Vst2 => counts.vst2 += 1,
                PluginFormat::Clap => counts.clap += 1,
                PluginFormat::Au => counts.au += 1,
                _ => {}
            }
        }
        counts
    }

    pub fn selected_plugin(&self) -> Option<&RegistryPlugin> {
        let id = self.selected_id.as_ref()?;
        self.plugins.iter().find(|p| &p.id == id)
    }

    pub fn visible_plugins<'a>(&'a self, query: &str) -> Vec<&'a RegistryPlugin> {
        let mut result: Vec<&RegistryPlugin> = self
            .plugins
            .iter()
            .filter(|p| match &self.sidebar_filter {
                SidebarFilter::All => true,
                SidebarFilter::Attention => Health::of(p).needs_attention(),
                SidebarFilter::Instrument => p.kind == PluginKind::Instrument,
                SidebarFilter::Effect => p.kind.usable_as_effect(),
                SidebarFilter::Format(fmt) => p.format == *fmt,
            })
            .collect();

        let q = query.trim().to_ascii_lowercase();
        if !q.is_empty() {
            result.retain(|p| {
                let hay = format!(
                    "{} {} {} {} {}",
                    p.name,
                    p.vendor,
                    p.display_category(),
                    p.raw_category.as_deref().unwrap_or(""),
                    p.path.display()
                )
                .to_ascii_lowercase();
                hay.contains(&q)
            });
        }

        result.sort_by(|a, b| {
            let cmp = match self.sort_key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Vendor => a.vendor.to_lowercase().cmp(&b.vendor.to_lowercase()),
                SortKey::Category => a.display_category().cmp(&b.display_category()),
                SortKey::Format => a.format.label().cmp(b.format.label()),
                SortKey::Status => Health::of(a).cmp(&Health::of(b)),
            }
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            match self.sort_dir {
                SortDir::Asc => cmp,
                SortDir::Desc => cmp.reverse(),
            }
        });

        result
    }
}

fn reveal_path_for_plugin(plugin: &RegistryPlugin) -> &Path {
    if plugin.preset_path.exists() {
        &plugin.preset_path
    } else {
        &plugin.path
    }
}

#[derive(Clone)]
pub struct PluginManagerCallbacks {
    pub on_rescan: VoidCb,
    pub on_select_id: StrCb,
    pub on_clear_selection: VoidCb,
    pub on_sidebar_filter: Arc<dyn Fn(&SidebarFilter, &mut Window, &mut App) + 'static>,
    pub on_sort: Arc<dyn Fn(&SortKey, &mut Window, &mut App) + 'static>,
    pub on_reveal_plugin: StrCb,
    pub on_reveal_preset: StrCb,
    pub on_copy_path: StrCb,
    pub on_register_plugin: StrCb,
    pub on_open_menu: Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>,
    pub on_open_db_folder: VoidCb,
    pub on_confirm_clear: VoidCb,
    pub on_cancel_clear: VoidCb,
}

fn icon(path: &'static str, size: f32, color: Rgba) -> impl IntoElement {
    svg().path(path).text_color(color).size(px(size))
}

fn format_relative_time(ms: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let delta = (now_ms - ms).max(0);
    let secs = delta / 1000;
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} min ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

/// Paint for full-bleed rows and rail items, resolved once per render:
/// `Colors::composite` belongs on the control path, not in a row loop.
#[derive(Clone, Copy)]
struct Paint {
    row: Rgba,
    row_hover: Rgba,
    row_selected: Rgba,
    row_selected_hover: Rgba,
    rail_hover: Rgba,
    rail_selected: Rgba,
    rail_selected_hover: Rgba,
}

impl Paint {
    fn resolve() -> Self {
        let row = Colors::surface_base();
        let rail = Colors::surface_sidebar();
        Self {
            row,
            row_hover: Colors::composite(row, Colors::state_hover()),
            row_selected: Colors::composite(row, Colors::state_selected()),
            row_selected_hover: Colors::composite(row, Colors::state_selected_hover()),
            rail_hover: Colors::composite(rail, Colors::state_hover()),
            rail_selected: Colors::composite(rail, Colors::state_selected()),
            rail_selected_hover: Colors::composite(rail, Colors::state_selected_hover()),
        }
    }
}

/// Leading-edge selection marker. An overlay, so selecting never reflows.
fn selection_marker(inset: f32) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .top(px(inset))
        .bottom(px(inset))
        .w(px(2.0))
        .rounded(px(radius::PILL))
        .bg(Colors::accent_primary())
}

/// Status cell: a dot and a word, so the state is not carried by hue alone.
fn health_chip(health: Health, i18n: I18n) -> impl IntoElement {
    let tone = health.tone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .min_w(px(0.0))
        .child(
            div()
                .w(px(6.0))
                .h(px(6.0))
                .flex_shrink_0()
                .rounded(px(radius::PILL))
                .bg(tone),
        )
        .child(
            div()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(if health == Health::Ready {
                    Colors::text_muted()
                } else {
                    tone
                })
                .child(health.label(i18n)),
        )
}

fn kind_glyph(kind: PluginKind) -> impl IntoElement {
    let (path, color) = match kind {
        PluginKind::Instrument => (assets::ICON_MUSIC_PATH, Colors::accent_primary()),
        PluginKind::Effect => (assets::ICON_SLIDERS_HORIZONTAL_PATH, Colors::text_muted()),
        PluginKind::Unknown => (assets::ICON_SLIDERS_HORIZONTAL_PATH, Colors::text_faint()),
    };
    icon(path, 12.0, color)
}

// ── Header ──────────────────────────────────────────────────────────────────

fn header(
    state: &PluginManagerDialogState,
    counts: FilterCounts,
    search: gpui::AnyElement,
    callbacks: &PluginManagerCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let rescan = callbacks.on_rescan.clone();
    let open_menu = callbacks.on_open_menu.clone();
    let stat = |text: String, color: Rgba| {
        div()
            .flex_shrink_0()
            .text_size(px(typography::UI_XS))
            .text_color(color)
            .child(text)
    };
    let dot = || {
        div()
            .flex_shrink_0()
            .text_size(px(typography::UI_XS))
            .text_color(Colors::text_faint())
            .child("·")
    };
    let last_scan = if state.last_scan_at_ms > 0 {
        i18n.tr_vars(
            "plugin-manager.footer.last-scan",
            &[("when", format_relative_time(state.last_scan_at_ms))],
        )
    } else {
        i18n.tr("plugin-manager.stat.never-scanned")
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .h(px(size::PROMINENT + space::SECTION))
        .px(px(space::LOOSE))
        .bg(Colors::surface_panel())
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(space::HAIR))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(space::SNUG))
                        .child(
                            div()
                                .text_size(px(typography::UI_MD))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(i18n.tr_vars(
                                    "plugin-manager.stat.plugins",
                                    &[("count", counts.all.to_string())],
                                )),
                        )
                        .when(counts.attention > 0, |row| {
                            row.child(dot()).child(stat(
                                i18n.tr_vars(
                                    "plugin-manager.stat.attention",
                                    &[("count", counts.attention.to_string())],
                                ),
                                Colors::status_warning(),
                            ))
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::SNUG))
                        .overflow_hidden()
                        .child(stat(
                            i18n.tr_vars(
                                "plugin-manager.stat.instruments",
                                &[("count", counts.instruments.to_string())],
                            ),
                            Colors::text_muted(),
                        ))
                        .child(dot())
                        .child(stat(
                            i18n.tr_vars(
                                "plugin-manager.stat.effects",
                                &[("count", counts.effects.to_string())],
                            ),
                            Colors::text_muted(),
                        ))
                        .child(dot())
                        .child(stat(last_scan, Colors::text_faint())),
                ),
        )
        .child(div().w(px(SEARCH_WIDTH)).flex_shrink_0().child(search))
        .child(fb_button(
            "plugin-manager-scan-now",
            if state.scanning {
                i18n.tr("plugin-manager.rescan.scanning")
            } else {
                i18n.tr("plugin-manager.rescan")
            },
            FbButtonKind::Primary,
            !state.scanning,
            move |_, window, cx| rescan(&(), window, cx),
        ))
        .child(
            div()
                .id("plugin-manager-more-anchor")
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    cx.stop_propagation();
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    open_menu(&(x, y), window, cx);
                })
                .tooltip(fb_tooltip(i18n.tr("plugin-manager.more")))
                .child(fb_icon_button(
                    "plugin-manager-more",
                    assets::ICON_MENU_PATH,
                    i18n.tr("plugin-manager.more"),
                    size::DEFAULT,
                    None,
                    |_, _, _| {},
                )),
        )
}

fn scan_progress_strip(state: &PluginManagerDialogState, i18n: I18n) -> impl IntoElement {
    let fraction = state.scan_progress_fraction();
    let pct = (fraction * 100.0).round() as u32;
    let label = if state.scan_progress_total > 0 {
        i18n.tr_vars(
            "plugin-manager.scan.progress",
            &[
                (
                    "current",
                    state
                        .scan_progress_current
                        .min(state.scan_progress_total)
                        .to_string(),
                ),
                ("total", state.scan_progress_total.to_string()),
                ("name", state.scan_progress_label.clone()),
            ],
        )
    } else if state.scan_progress_label.is_empty() {
        state.status_text.clone()
    } else {
        state.scan_progress_label.clone()
    };

    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .flex_shrink_0()
        .px(px(space::LOOSE))
        .py(px(space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_panel())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(label),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::UI_XS))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_secondary())
                        .child(format!("{pct}%")),
                ),
        )
        .child(fb_progress(fraction))
}

fn banner(message: String, tone: Rgba) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px(px(space::LOOSE))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::with_alpha(tone, 0.08))
        .text_size(px(typography::DENSE_LABEL))
        .text_color(tone)
        .child(message)
}

/// Clearing the database cannot be undone, so it asks first — in place, with
/// the destructive action and an explicit Cancel.
fn clear_confirmation(
    count: usize,
    on_confirm: VoidCb,
    on_cancel: VoidCb,
    i18n: I18n,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .px(px(space::LOOSE))
        .py(px(space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::with_alpha(Colors::status_error(), 0.08))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_primary())
                .child(i18n.tr_vars(
                    "plugin-manager.clear.confirm",
                    &[("count", count.to_string())],
                )),
        )
        .child(fb_button(
            "plugin-manager-clear-cancel",
            i18n.tr("plugin-manager.clear.cancel"),
            FbButtonKind::Default,
            true,
            move |_, window, cx| on_cancel(&(), window, cx),
        ))
        .child(fb_button(
            "plugin-manager-clear-confirm",
            i18n.tr("plugin-manager.clear-database"),
            FbButtonKind::Danger,
            true,
            move |_, window, cx| on_confirm(&(), window, cx),
        ))
}

// ── Rail ────────────────────────────────────────────────────────────────────

fn rail(
    state: &PluginManagerDialogState,
    counts: FilterCounts,
    paint: Paint,
    on_filter: Arc<dyn Fn(&SidebarFilter, &mut Window, &mut App) + 'static>,
    scroll: &ScrollHandle,
    i18n: I18n,
) -> impl IntoElement {
    let item = |id: &'static str, label: String, count: usize, value: SidebarFilter| {
        rail_item(
            id,
            label,
            count,
            state.sidebar_filter == value,
            paint,
            on_filter.clone(),
            value,
        )
    };

    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .px(px(space::TIGHT))
        .pb(px(space::BASE))
        .child(rail_heading(i18n.tr("plugin-manager.filter.library")))
        .child(item(
            "pm-filter-all",
            i18n.tr("plugin-manager.filter.all"),
            counts.all,
            SidebarFilter::All,
        ))
        .child(item(
            "pm-filter-attention",
            i18n.tr("plugin-manager.filter.attention"),
            counts.attention,
            SidebarFilter::Attention,
        ))
        .child(rail_heading(i18n.tr("plugin-manager.filter.kind")))
        .child(item(
            "pm-filter-inst",
            i18n.tr("plugin-manager.filter.instruments"),
            counts.instruments,
            SidebarFilter::Instrument,
        ))
        .child(item(
            "pm-filter-fx",
            i18n.tr("plugin-manager.filter.effects"),
            counts.effects,
            SidebarFilter::Effect,
        ))
        .child(rail_heading(i18n.tr("plugin-manager.filter.format")));

    for (id, label, count, format) in [
        ("pm-filter-vst3", "VST3", counts.vst3, PluginFormat::Vst3),
        ("pm-filter-clap", "CLAP", counts.clap, PluginFormat::Clap),
        ("pm-filter-vst2", "VST2", counts.vst2, PluginFormat::Vst2),
        ("pm-filter-au", "Audio Units", counts.au, PluginFormat::Au),
    ] {
        let value = SidebarFilter::Format(format);
        let offered = if format == PluginFormat::Au {
            state.au_scan_available || count > 0
        } else {
            true
        };
        if offered && (count > 0 || state.sidebar_filter == value) {
            col = col.child(item(id, label.to_string(), count, value));
        }
    }

    // Where the scanner looks. Read-only: the folders are the platform's
    // standard plug-in locations.
    col = col.child(rail_heading(i18n.tr("plugin-manager.scan-locations")));
    if state.scan_paths.is_empty() {
        col = col.child(
            div()
                .px(px(space::BASE))
                .py(px(space::TIGHT))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child(i18n.tr("plugin-manager.scan-locations.empty")),
        );
    } else {
        for (i, path) in state.scan_paths.iter().enumerate() {
            let full = path.display().to_string();
            col = col.child(
                div()
                    .id(("pm-scan-path", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .h(px(size::ROW))
                    .px(px(space::BASE))
                    .tooltip(fb_tooltip(full.clone()))
                    .child(icon(assets::ICON_FOLDER_PATH, 11.0, Colors::text_faint()))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::DENSE_LABEL))
                            .text_color(Colors::text_faint())
                            .child(full),
                    ),
            );
        }
    }

    let thumb = scroll.clone();
    div()
        .flex()
        .flex_col()
        .w(px(RAIL_WIDTH))
        .h_full()
        .flex_shrink_0()
        .border_r(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_sidebar())
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .relative()
                .child(
                    div()
                        .id("plugin-manager-sidebar-scroll")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(scroll)
                        .child(col),
                )
                .child(vertical_scrollbar_thumb(thumb)),
        )
}

fn rail_heading(label: String) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(size::ROW))
        .px(px(space::BASE))
        .mt(px(space::SNUG))
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(Colors::text_faint())
        .child(label.to_uppercase())
}

fn rail_item(
    id: &'static str,
    label: String,
    count: usize,
    active: bool,
    paint: Paint,
    on_filter: Arc<dyn Fn(&SidebarFilter, &mut Window, &mut App) + 'static>,
    value: SidebarFilter,
) -> impl IntoElement {
    let (rest, hover) = if active {
        (paint.rail_selected, paint.rail_selected_hover)
    } else {
        (Colors::with_alpha(paint.rail_hover, 0.0), paint.rail_hover)
    };
    let attention = value == SidebarFilter::Attention && count > 0;
    div()
        .id(id)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .w_full()
        .h(px(size::ROW))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |_, window, cx| on_filter(&value, window, cx))
        .when(active, |el| el.child(selection_marker(space::SNUG)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(label),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(if attention {
                    Colors::status_warning()
                } else if active {
                    Colors::text_secondary()
                } else {
                    Colors::text_faint()
                })
                .child(count.to_string()),
        )
}

// ── Table ───────────────────────────────────────────────────────────────────

/// One row's worth of display data. Owned, because the virtualised list
/// builds rows after this render's borrow of the state has ended.
#[derive(Clone)]
struct RowData {
    id: String,
    name: String,
    vendor: String,
    category: String,
    format: PluginFormat,
    kind: PluginKind,
    health: Health,
}

fn table_header(
    state: &PluginManagerDialogState,
    on_sort: Arc<dyn Fn(&SortKey, &mut Window, &mut App) + 'static>,
    i18n: I18n,
) -> impl IntoElement {
    let column = |id: &'static str, label: String, key: SortKey| {
        let active = state.sort_key == key;
        let on_sort = on_sort.clone();
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .min_w(px(0.0))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_click(move |_, window, cx| on_sort(&key, window, cx))
            .text_size(px(typography::DENSE_CAPTION))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(if active {
                Colors::text_secondary()
            } else {
                Colors::text_faint()
            })
            .child(div().truncate().child(label))
            .when(active, |el| {
                el.child(match state.sort_dir {
                    SortDir::Asc => "▲",
                    SortDir::Desc => "▼",
                })
            })
    };
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(size::ROW_DENSE))
        .px(px(ROW_PAD_X))
        .gap(px(COL_GAP))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_base())
        .child(div().w(px(COL_KIND)).flex_shrink_0())
        .child(div().flex_1().min_w(px(COL_NAME_MIN)).child(column(
            "pm-sort-name",
            i18n.tr("plugin-manager.sort.name"),
            SortKey::Name,
        )))
        .child(div().w(px(COL_VENDOR_W)).flex_shrink_0().child(column(
            "pm-sort-vendor",
            i18n.tr("plugin-manager.sort.vendor"),
            SortKey::Vendor,
        )))
        .child(div().w(px(COL_CATEGORY_W)).flex_shrink_0().child(column(
            "pm-sort-cat",
            i18n.tr("plugin-manager.sort.category"),
            SortKey::Category,
        )))
        .child(div().w(px(COL_FORMAT_W)).flex_shrink_0().child(column(
            "pm-sort-fmt",
            i18n.tr("plugin-manager.sort.format"),
            SortKey::Format,
        )))
        .child(div().w(px(COL_STATUS_W)).flex_shrink_0().child(column(
            "pm-sort-status",
            i18n.tr("plugin-manager.column.status"),
            SortKey::Status,
        )))
}

fn table_row(
    index: usize,
    row: &RowData,
    selected: bool,
    paint: Paint,
    on_select: StrCb,
    i18n: I18n,
) -> impl IntoElement {
    let (rest, hover) = if selected {
        (paint.row_selected, paint.row_selected_hover)
    } else {
        (paint.row, paint.row_hover)
    };
    let id = row.id.clone();
    div()
        .id(("plugin-row", index))
        .relative()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(COL_GAP))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PAD_X))
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |_, window, cx| on_select(&id, window, cx))
        .when(selected, |el| el.child(selection_marker(space::TIGHT)))
        .child(
            div()
                .w(px(COL_KIND))
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(kind_glyph(row.kind)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(COL_NAME_MIN))
                .truncate()
                .text_size(px(typography::UI_SM))
                .font_weight(if selected {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .text_color(Colors::text_primary())
                .child(row.name.clone()),
        )
        .child(
            div()
                .w(px(COL_VENDOR_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(row.vendor.clone()),
        )
        .child(
            div()
                .w(px(COL_CATEGORY_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child(row.category.clone()),
        )
        .child(
            div()
                .w(px(COL_FORMAT_W))
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(plugin_format_badge(row.format)),
        )
        .child(
            div()
                .w(px(COL_STATUS_W))
                .flex_shrink_0()
                .child(health_chip(row.health, i18n)),
        )
}

fn table(
    state: &PluginManagerDialogState,
    rows: Arc<Vec<RowData>>,
    paint: Paint,
    callbacks: &PluginManagerCallbacks,
    list_scroll: &UniformListScrollHandle,
    i18n: I18n,
) -> impl IntoElement {
    let body = if rows.is_empty() {
        let message = if state.scanning {
            i18n.tr("plugin-manager.list.scanning")
        } else if state.plugins.is_empty() {
            // Nothing registered at all: "empty" is honest either way. Only a
            // non-empty registry with no visible rows means the filter
            // excluded everything.
            i18n.tr("plugin-manager.list.empty")
        } else {
            i18n.tr("plugin-manager.list.no-match")
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(space::SNUG))
            .size_full()
            .px(px(space::BLOCK))
            .child(icon(assets::ICON_PLUG_PATH, 20.0, Colors::text_faint()))
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child(message),
            )
            .into_any_element()
    } else {
        let selected = state.selected_id.clone();
        let on_select = callbacks.on_select_id.clone();
        let thumb = list_scroll.0.borrow().base_handle.clone();
        let count = rows.len();
        let list = uniform_list("plugin-manager-list", count, move |range, _window, _cx| {
            range
                .filter_map(|i| {
                    let row = rows.get(i)?;
                    Some(
                        table_row(
                            i,
                            row,
                            selected.as_deref() == Some(row.id.as_str()),
                            paint,
                            on_select.clone(),
                            i18n,
                        )
                        .into_any_element(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(list_scroll)
        .size_full();
        div()
            .relative()
            .size_full()
            .child(list)
            .child(vertical_scrollbar_thumb(thumb))
            .into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .bg(Colors::surface_base())
        .child(table_header(state, callbacks.on_sort.clone(), i18n))
        .child(div().flex_1().min_h(px(0.0)).child(body))
}

// ── Details ─────────────────────────────────────────────────────────────────

fn details_panel(
    plugin: &RegistryPlugin,
    callbacks: &PluginManagerCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let health = Health::of(plugin);
    let kind_label = match plugin.kind {
        PluginKind::Instrument => i18n.tr("plugin-manager.kind.instrument"),
        PluginKind::Effect => i18n.tr("plugin-manager.kind.effect"),
        PluginKind::Unknown => i18n.tr("plugin-manager.kind.unknown"),
    };
    let byline = match plugin.version.as_deref() {
        Some(version) if !plugin.vendor.is_empty() => format!("{} · {version}", plugin.vendor),
        Some(version) => version.to_string(),
        None => plugin.vendor.clone(),
    };
    let can_register = plugin.status == PluginStatus::MissingPreset && plugin.path.exists();
    let preset_exists = plugin.preset_path.exists();
    let plugin_exists = plugin.path.exists();
    let close = callbacks.on_clear_selection.clone();
    let register = callbacks.on_register_plugin.clone();
    let reveal_plugin = callbacks.on_reveal_plugin.clone();
    let reveal_preset = callbacks.on_reveal_preset.clone();
    let copy_path = callbacks.on_copy_path.clone();
    let (id_register, id_plugin, id_preset, id_copy) = (
        plugin.id.clone(),
        plugin.id.clone(),
        plugin.id.clone(),
        plugin.id.clone(),
    );

    let field = |label: String, value: String| {
        div()
            .flex()
            .flex_col()
            .gap(px(space::HAIR))
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_faint())
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_secondary())
                    .child(value),
            )
    };

    div()
        .flex()
        .flex_col()
        .w(px(DETAILS_WIDTH))
        .h_full()
        .flex_shrink_0()
        .border_l(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_panel())
        // Identity.
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .px(px(space::LOOSE))
                .pt(px(space::LOOSE))
                .pb(px(space::BASE))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(space::BASE))
                        .child(div().pt(px(3.0)).child(kind_glyph(plugin.kind)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(px(typography::UI_MD))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(plugin.name.clone()),
                        )
                        .child(fb_icon_button(
                            "plugin-manager-details-close",
                            assets::ICON_X_PATH,
                            i18n.tr("plugin-manager.details.close"),
                            size::DENSE,
                            None,
                            move |_, window, cx| close(&(), window, cx),
                        )),
                )
                .when(!byline.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(byline),
                    )
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::BASE))
                        .child(plugin_format_badge(plugin.format))
                        .child(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_muted())
                                .child(kind_label),
                        )
                        .child(health_chip(health, i18n)),
                ),
        )
        // What is wrong, if anything, in words.
        .when_some(
            health
                .explanation(i18n)
                .map(|text| match plugin.error_message.as_deref() {
                    Some(error) if !error.is_empty() => format!("{text}\n{error}"),
                    _ => text,
                }),
            |el, text| {
                let tone = health.tone();
                el.child(
                    div()
                        .mx(px(space::LOOSE))
                        .mb(px(space::BASE))
                        .px(px(space::BASE))
                        .py(px(space::SNUG))
                        .rounded(px(radius::CONTROL))
                        .border(px(1.0))
                        .border_color(Colors::with_alpha(tone, state_tokens::ARMED_BORDER))
                        .bg(Colors::with_alpha(tone, 0.08))
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_secondary())
                        .child(text),
                )
            },
        )
        .child(
            div()
                .id("plugin-manager-details-scroll")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .px(px(space::LOOSE))
                .py(px(space::BASE))
                .flex()
                .flex_col()
                .gap(px(space::BASE))
                .child(field(
                    i18n.tr("plugin-manager.field.category"),
                    plugin.display_category(),
                ))
                .when_some(plugin.raw_category.clone(), |el, raw| {
                    el.child(field(i18n.tr("plugin-manager.field.sdk-category"), raw))
                })
                .when_some(plugin.class_id.clone(), |el, class_id| {
                    el.child(field(i18n.tr("plugin-manager.field.class-id"), class_id))
                })
                .child(field(
                    i18n.tr("plugin-manager.field.path"),
                    plugin.path.display().to_string(),
                ))
                .child(field(
                    i18n.tr("plugin-manager.field.preset"),
                    if preset_exists {
                        plugin.preset_path.display().to_string()
                    } else {
                        "—".to_string()
                    },
                )),
        )
        // Actions — only ones that do something.
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .px(px(space::LOOSE))
                .py(px(space::BASE))
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .when(can_register, |el| {
                    el.child(fb_button(
                        "plugin-mgr-register",
                        i18n.tr("plugin-manager.action.register"),
                        FbButtonKind::Primary,
                        true,
                        move |_, window, cx| register(&id_register, window, cx),
                    ))
                })
                .child(fb_button(
                    "plugin-mgr-reveal-plugin",
                    i18n.tr("plugin-manager.action.reveal-plugin"),
                    FbButtonKind::Default,
                    plugin_exists,
                    move |_, window, cx| reveal_plugin(&id_plugin, window, cx),
                ))
                .when(preset_exists, |el| {
                    el.child(fb_button(
                        "plugin-mgr-reveal-preset",
                        i18n.tr("plugin-manager.action.reveal-preset"),
                        FbButtonKind::Ghost,
                        true,
                        move |_, window, cx| reveal_preset(&id_preset, window, cx),
                    ))
                })
                .child(fb_button(
                    "plugin-mgr-copy-path",
                    i18n.tr("plugin-manager.action.copy-path"),
                    FbButtonKind::Ghost,
                    true,
                    move |_, window, cx| copy_path(&id_copy, window, cx),
                )),
        )
}

// ── Status bar ──────────────────────────────────────────────────────────────

fn status_bar(
    state: &PluginManagerDialogState,
    on_open_db_folder: VoidCb,
    i18n: I18n,
) -> impl IntoElement {
    let db_path = SpherePluginHost::database_path().display().to_string();
    let link_hover = Colors::composite(Colors::surface_panel(), Colors::state_hover());
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .h(px(size::DEFAULT + space::TIGHT))
        .px(px(space::LOOSE))
        .border_t(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_panel())
        .text_size(px(typography::DENSE_LABEL))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .min_w(px(0.0))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(Colors::text_muted())
                        .child(state.status_text.clone()),
                )
                .when(state.failed_count > 0, |el| {
                    el.child(
                        div()
                            .flex_shrink_0()
                            .text_color(Colors::status_warning())
                            .child(i18n.tr_vars(
                                "plugin-manager.footer.missing",
                                &[("count", state.failed_count.to_string())],
                            )),
                    )
                }),
        )
        .child(
            div()
                .id("plugin-manager-db-path")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .flex_shrink_0()
                .px(px(space::SNUG))
                .h(px(size::DENSE))
                .rounded(px(radius::CONTROL_SM))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(link_hover))
                .tooltip(fb_tooltip(i18n.tr("plugin-manager.open-db-folder")))
                .on_click(move |_, window, cx| on_open_db_folder(&(), window, cx))
                .child(icon(
                    assets::ICON_HARD_DRIVE_PATH,
                    11.0,
                    Colors::text_faint(),
                ))
                .child(
                    div()
                        .max_w(px(DB_PATH_MAX_W))
                        .truncate()
                        .text_color(Colors::text_faint())
                        .child(db_path),
                ),
        )
}

// ── Composition ─────────────────────────────────────────────────────────────

/// Main plug-in manager body (header, rail, table, details, status bar).
#[allow(clippy::too_many_arguments)]
pub fn plugin_manager_panel(
    state: &PluginManagerDialogState,
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    search_ime_target: Entity<PluginManagerWindow>,
    callbacks: PluginManagerCallbacks,
    confirm_clear: bool,
    sidebar_scroll: &ScrollHandle,
    list_scroll: &UniformListScrollHandle,
    i18n: I18n,
) -> impl IntoElement {
    let counts = state.counts();
    let paint = Paint::resolve();
    let rows: Arc<Vec<RowData>> = Arc::new(
        state
            .visible_plugins(&search_input.value)
            .into_iter()
            .map(|plugin| RowData {
                id: plugin.id.clone(),
                name: plugin.name.clone(),
                vendor: plugin.vendor.clone(),
                category: plugin.display_category(),
                format: plugin.format,
                kind: plugin.kind,
                health: Health::of(plugin),
            })
            .collect(),
    );
    let selected = state.selected_plugin();
    let search = text_field_with_callbacks_and_ime(
        search_input,
        search_focused,
        search_callbacks,
        search_ime_target,
    )
    .into_any_element();

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .bg(Colors::surface_window())
        .child(header(state, counts, search, &callbacks, i18n))
        .when(state.scanning, |panel| {
            panel.child(scan_progress_strip(state, i18n))
        })
        .when(confirm_clear, |panel| {
            panel.child(clear_confirmation(
                state.plugins.len(),
                callbacks.on_confirm_clear.clone(),
                callbacks.on_cancel_clear.clone(),
                i18n,
            ))
        })
        .when(
            state.au_auto_scan_disabled && state.au_scan_available,
            |panel| {
                panel.child(banner(
                    i18n.tr("plugin-manager.scan-au.disabled"),
                    Colors::status_warning(),
                ))
            },
        )
        .when_some(
            state
                .au_scan_error
                .clone()
                .filter(|_| !state.scanning && state.au_scan_available),
            |panel, message| panel.child(banner(message, Colors::status_warning())),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .child(rail(
                    state,
                    counts,
                    paint,
                    callbacks.on_sidebar_filter.clone(),
                    sidebar_scroll,
                    i18n,
                ))
                .child(table(state, rows, paint, &callbacks, list_scroll, i18n))
                .when_some(selected, |panel, plugin| {
                    panel.child(details_panel(plugin, &callbacks, i18n))
                }),
        )
        .child(status_bar(state, callbacks.on_open_db_folder.clone(), i18n))
}

fn reveal_in_os(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        if path.is_file() {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,\"{}\"", path.display()))
                .spawn();
        } else {
            let _ = std::process::Command::new("explorer")
                .arg(format!("\"{}\"", path.display()))
                .spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(if path.is_file() { "-R" } else { "" })
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = if path.is_file() {
            std::process::Command::new("xdg-open")
                .arg(path.parent().unwrap_or(path))
                .spawn()
        } else {
            std::process::Command::new("xdg-open").arg(path).spawn()
        };
    }
}

// ── Window ──────────────────────────────────────────────────────────────────

pub struct PluginManagerWindow {
    pub state: PluginManagerDialogState,
    search_input: TextInputState,
    focus_handle: FocusHandle,
    initial_cache_loaded: bool,
    sidebar_scroll: ScrollHandle,
    list_scroll: UniformListScrollHandle,
    /// Where the "More" menu is open, in window coordinates.
    menu: Option<(f32, f32)>,
    /// Clear Database was chosen and is waiting for confirmation.
    confirm_clear: bool,
}

impl PluginManagerWindow {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let i18n = I18n::from_app(cx);
        Self {
            state: PluginManagerDialogState::new_empty(i18n),
            search_input: TextInputState::new("plugin-manager-search", cx.focus_handle())
                .with_placeholder(i18n.tr("search.plugins-manager.placeholder")),
            focus_handle: cx.focus_handle(),
            initial_cache_loaded: false,
            sidebar_scroll: ScrollHandle::new(),
            list_scroll: UniformListScrollHandle::new(),
            menu: None,
            confirm_clear: false,
        }
    }

    /// Read the `.pst` cache on a background thread and apply it to the
    /// dialog. No plug-in binary is touched and no SDK scan is performed.
    fn arm_cache_load(cx: &mut Context<Self>) {
        let debug = std::env::var_os("FUTUREBOARD_PLUGIN_MANAGER_DEBUG").is_some();
        let started = std::time::Instant::now();
        cx.spawn(async move |this, cx| {
            let (plugins, last_ms) = cx
                .background_executor()
                .spawn(async { PluginRegistry::load_cached() })
                .await;
            let count = plugins.len();
            let _ = this.update(cx, |win, cx| {
                let i18n = I18n::from_app(cx);
                win.state.apply_cache_load(plugins, last_ms, i18n);
                cx.notify();
            });
            if debug {
                eprintln!(
                    "[plugin-manager] cache_loaded plugins={count} load_ms={}",
                    started.elapsed().as_millis()
                );
            }
        })
        .detach();
    }

    /// Discover, validate, and register plug-ins on a worker thread; stream progress to the UI.
    fn arm_background_scan(cx: &mut Context<Self>, mode: PluginScanMode) {
        let options = ScanOptions {
            paths: None,
            delete_presets_first: mode == PluginScanMode::RescanAll,
            include_au: mode != PluginScanMode::RescanAu || cfg!(target_os = "macos"),
            formats_only: if mode == PluginScanMode::RescanAu {
                Some(vec![PluginFormat::Au])
            } else {
                None
            },
        };

        cx.spawn(async move |this, cx| {
            let (tx, rx) = std::sync::mpsc::channel::<ScanProgress>();
            let scan_options = options;
            let handle = std::thread::spawn(move || {
                PluginRegistry::scan_with_progress(scan_options, |progress| {
                    let _ = tx.send(progress);
                })
            });

            loop {
                while let Ok(progress) = rx.try_recv() {
                    let _ = this.update(cx, |win, cx| {
                        let i18n = I18n::from_app(cx);
                        win.state.apply_scan_progress(&progress, i18n);
                        cx.notify();
                    });
                }
                if handle.is_finished() {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(32))
                    .await;
            }

            while let Ok(progress) = rx.try_recv() {
                let _ = this.update(cx, |win, cx| {
                    let i18n = I18n::from_app(cx);
                    win.state.apply_scan_progress(&progress, i18n);
                    cx.notify();
                });
            }

            match handle.join() {
                Ok(result) => {
                    let _ = this.update(cx, |win, cx| {
                        let i18n = I18n::from_app(cx);
                        win.state.apply_scan_result(result, i18n);
                        cx.notify();
                    });
                }
                Err(_) => {
                    let _ = this.update(cx, |win, cx| {
                        win.state.scanning = false;
                        win.state.status_text = I18n::from_app(cx).tr("plugin-manager.error.panic");
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn start_scan(&mut self, mode: PluginScanMode, cx: &mut Context<Self>) {
        if self.state.scanning {
            return;
        }
        if mode == PluginScanMode::RescanAu && !self.state.au_scan_available {
            return;
        }
        self.confirm_clear = false;
        self.state.begin_scan(mode, I18n::from_app(cx));
        cx.notify();
        Self::arm_background_scan(cx, mode);
    }

    fn clear_database(&mut self, cx: &mut Context<Self>) {
        self.confirm_clear = false;
        if self.state.scanning {
            cx.notify();
            return;
        }
        match PluginRegistry::clear_cache() {
            Ok(removed) => {
                self.state.plugins.clear();
                self.state.selected_id = None;
                self.state.failed_count = 0;
                self.state.last_scan_at_ms = 0;
                self.state.cache_loaded = true;
                self.state.status_text =
                    format!("Cleared {removed} cached preset(s). Click Rescan to rebuild.");
            }
            Err(error) => {
                self.state.status_text = format!("Clear cache failed: {error}");
            }
        }
        cx.notify();
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        self.menu = None;
        match command {
            CMD_RESCAN_ALL => self.start_scan(PluginScanMode::RescanAll, cx),
            CMD_RESCAN_AU => self.start_scan(PluginScanMode::RescanAu, cx),
            CMD_OPEN_DB => {
                let dir = SpherePluginHost::database_dir();
                let _ = std::fs::create_dir_all(&dir);
                reveal_in_os(&dir);
            }
            CMD_CLEAR_DB => {
                if !self.state.scanning && !self.state.plugins.is_empty() {
                    self.confirm_clear = true;
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn menu_entries(&self, i18n: I18n) -> Vec<ContextMenuEntry> {
        let idle = !self.state.scanning;
        let item = |label: String, command: &str, enabled: bool| {
            if enabled {
                ContextMenuEntry::item(label, command)
            } else {
                ContextMenuEntry::disabled_item(label, command)
            }
        };
        let mut entries = vec![item(
            i18n.tr("plugin-manager.rescan-all"),
            CMD_RESCAN_ALL,
            idle,
        )];
        if self.state.au_scan_available {
            entries.push(item(
                if self.state.au_auto_scan_disabled {
                    i18n.tr("plugin-manager.scan-au.retry")
                } else {
                    i18n.tr("plugin-manager.scan-au")
                },
                CMD_RESCAN_AU,
                idle,
            ));
        }
        entries.push(ContextMenuEntry::Separator);
        entries.push(ContextMenuEntry::item(
            i18n.tr("plugin-manager.open-db-folder"),
            CMD_OPEN_DB,
        ));
        entries.push(ContextMenuEntry::Separator);
        if idle && !self.state.plugins.is_empty() {
            entries.push(ContextMenuEntry::danger_item(
                i18n.tr("plugin-manager.clear-database"),
                CMD_CLEAR_DB,
            ));
        } else {
            entries.push(ContextMenuEntry::disabled_item(
                i18n.tr("plugin-manager.clear-database"),
                CMD_CLEAR_DB,
            ));
        }
        entries
    }

    /// Move the selection through the visible rows, keeping it in view.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .state
            .visible_plugins(&self.search_input.value)
            .into_iter()
            .map(|plugin| plugin.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let current = self
            .state
            .selected_id
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id));
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, ids.len() as isize - 1) as usize,
            None if delta < 0 => ids.len() - 1,
            None => 0,
        };
        self.state.selected_id = Some(ids[next].clone());
        self.list_scroll
            .scroll_to_item(next, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        // The list is browsable from the search field too: arrows are not
        // text editing there.
        match key {
            "up" | "arrowup" => {
                self.move_selection(-1, cx);
                cx.stop_propagation();
                return;
            }
            "down" | "arrowdown" => {
                self.move_selection(1, cx);
                cx.stop_propagation();
                return;
            }
            _ => {}
        }
        if key == "escape" {
            // Innermost first: the menu, the confirmation, the query, the
            // selection — and only then the window.
            if self.menu.take().is_some() || std::mem::take(&mut self.confirm_clear) {
            } else if !self.search_input.value.is_empty() {
                self.search_input.set_value("");
            } else if self.state.selected_id.take().is_none() {
                window.remove_window();
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.search_input.is_focused(window) {
            let _ = self.search_input.handle_key_ime(event, Some(cx));
            cx.notify();
        }
    }

    fn with_plugin(&self, id: &str, f: impl FnOnce(&RegistryPlugin)) {
        if let Some(plugin) = self.state.plugins.iter().find(|p| p.id == id) {
            f(plugin);
        }
    }
}

// Route platform IME (CJK/Thai composition + candidate-window positioning) to
// the search field. Coexists with `handle_key_ime` (handle_key); GPUI
// suppresses key dispatch for keystrokes the IME consumes.
crate::impl_single_input_window_ime!(PluginManagerWindow, search_input);

impl Render for PluginManagerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let i18n = I18n::from_app(cx);
        self.search_input.placeholder = Some(i18n.tr("search.plugins-manager.placeholder"));

        if !self.initial_cache_loaded {
            self.initial_cache_loaded = true;
            // Load cached `.pst` index only. Never auto-scan VST3/CLAP binaries
            // — the user must press Rescan explicitly.
            Self::arm_cache_load(cx);
        }

        let target = cx.entity().clone();
        let search_focused = self.search_input.is_focused(window);
        let str_cb = |f: fn(&mut PluginManagerWindow, &str, &mut Context<PluginManagerWindow>)| {
            let target = target.clone();
            Arc::new(move |id: &String, _w: &mut Window, cx: &mut App| {
                let _ = target.update(cx, |this, cx| f(this, id, cx));
            }) as StrCb
        };
        let void_cb = |f: fn(&mut PluginManagerWindow, &mut Context<PluginManagerWindow>)| {
            let target = target.clone();
            Arc::new(move |_: &(), _w: &mut Window, cx: &mut App| {
                let _ = target.update(cx, |this, cx| f(this, cx));
            }) as VoidCb
        };

        let callbacks = PluginManagerCallbacks {
            on_rescan: void_cb(|this, cx| this.start_scan(PluginScanMode::Rescan, cx)),
            on_select_id: str_cb(|this, id, cx| {
                this.state.selected_id = Some(id.to_string());
                cx.notify();
            }),
            on_clear_selection: void_cb(|this, cx| {
                this.state.selected_id = None;
                cx.notify();
            }),
            on_sidebar_filter: Arc::new({
                let target = target.clone();
                move |filter: &SidebarFilter, _w, cx| {
                    let filter = filter.clone();
                    let _ = target.update(cx, |this, cx| {
                        this.state.sidebar_filter = filter;
                        this.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
                        cx.notify();
                    });
                }
            }),
            on_sort: Arc::new({
                let target = target.clone();
                move |key: &SortKey, _w, cx| {
                    let key = *key;
                    let _ = target.update(cx, |this, cx| {
                        if this.state.sort_key == key {
                            this.state.sort_dir = match this.state.sort_dir {
                                SortDir::Asc => SortDir::Desc,
                                SortDir::Desc => SortDir::Asc,
                            };
                        } else {
                            this.state.sort_key = key;
                            this.state.sort_dir = SortDir::Asc;
                        }
                        cx.notify();
                    });
                }
            }),
            on_reveal_plugin: str_cb(|this, id, _cx| {
                this.with_plugin(id, |plugin| reveal_in_os(&plugin.path));
            }),
            on_reveal_preset: str_cb(|this, id, _cx| {
                this.with_plugin(id, |plugin| reveal_in_os(reveal_path_for_plugin(plugin)));
            }),
            on_copy_path: str_cb(|this, id, cx| {
                let mut path = None;
                this.with_plugin(id, |plugin| path = Some(plugin.path.display().to_string()));
                if let Some(path) = path {
                    cx.write_to_clipboard(ClipboardItem::new_string(path));
                    this.state.status_text = I18n::from_app(cx).tr("plugin-manager.copied-path");
                    cx.notify();
                }
            }),
            on_register_plugin: str_cb(|this, id, cx| {
                let Some(plugin) = this.state.plugins.iter_mut().find(|p| p.id == id) else {
                    return;
                };
                let name = plugin.name.clone();
                let i18n = I18n::from_app(cx);
                match register_plugin(plugin) {
                    Ok(()) => {
                        this.state.generated_presets = this
                            .state
                            .plugins
                            .iter()
                            .filter(|p| p.status == PluginStatus::PresetReady)
                            .count() as u32;
                        this.state.status_text =
                            i18n.tr_vars("plugin-manager.register.success", &[("name", name)]);
                    }
                    Err(error) => {
                        this.state.status_text = i18n.tr_vars(
                            "plugin-manager.register.failed",
                            &[("error", error.to_string())],
                        );
                    }
                }
                cx.notify();
            }),
            on_open_menu: Arc::new({
                let target = target.clone();
                move |position: &(f32, f32), _w, cx| {
                    let position = *position;
                    let _ = target.update(cx, |this, cx| {
                        this.menu = if this.menu.is_some() {
                            None
                        } else {
                            Some(position)
                        };
                        cx.notify();
                    });
                }
            }),
            on_open_db_folder: void_cb(|this, cx| this.run_menu_command(CMD_OPEN_DB, cx)),
            on_confirm_clear: void_cb(|this, cx| this.clear_database(cx)),
            on_cancel_clear: void_cb(|this, cx| {
                this.confirm_clear = false;
                cx.notify();
            }),
        };

        let menu = self.menu.map(|(x, y)| {
            let viewport = window.viewport_size();
            let command_target = target.clone();
            let close_target = target.clone();
            context_menu_overlay(
                self.menu_entries(i18n),
                x,
                y,
                viewport.width.into(),
                viewport.height.into(),
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ =
                        command_target.update(cx, |this, cx| this.run_menu_command(&command, cx));
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| {
                        this.menu = None;
                        cx.notify();
                    });
                }),
            )
        });

        div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .font(theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar(
                i18n.tr("plugin-manager.title"),
                "plugin-manager-window-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |_, cx| cx.notify());
                        window.remove_window();
                    }
                },
            ))
            .child(plugin_manager_panel(
                &self.state,
                &self.search_input,
                search_focused,
                bind_mouse_selection(cx.entity().clone(), |this| &mut this.search_input),
                target.clone(),
                callbacks,
                self.confirm_clear,
                &self.sidebar_scroll,
                &self.list_scroll,
                i18n,
            ))
            .children(menu)
    }
}

pub fn open_plugin_manager_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<PluginManagerWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(
            px(PLUGIN_MANAGER_WINDOW_WIDTH),
            px(PLUGIN_MANAGER_WINDOW_HEIGHT),
        ),
        cx,
    );

    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = true;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    options.window_min_size = Some(size(
        px(PLUGIN_MANAGER_WINDOW_MIN_WIDTH),
        px(PLUGIN_MANAGER_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, |_window, cx| cx.new(PluginManagerWindow::new))
        .map_err(|error| error.to_string())
}

/// Scan the default plug-in locations in a compact, standalone progress dialog.
/// Closing the dialog only hides progress; the registry scan continues safely
/// on its worker thread and persists its result for the next plug-in picker.
pub fn open_plugin_scan_progress_dialog(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    cx: &mut App,
) -> Result<(), String> {
    open_plugin_scan_progress_dialog_impl(owner_bounds, None, cx)
}

/// First-launch scan surface. It stays in the startup window handoff and opens
/// the caller's next surface exactly once when the scan finishes or the user
/// closes the progress window. The scan itself remains safe to finish in the
/// background after a manual close.
pub fn open_startup_plugin_scan_progress_dialog(
    on_complete: Arc<dyn Fn(&mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<(), String> {
    open_plugin_scan_progress_dialog_impl(None, Some(on_complete), cx)
}

fn open_plugin_scan_progress_dialog_impl(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    on_startup_complete: Option<Arc<dyn Fn(&mut App) + Send + Sync>>,
    cx: &mut App,
) -> Result<(), String> {
    let startup_flow = on_startup_complete.is_some();
    let options = ProgressDialogOptions::default()
        .title("Plug-in Scan")
        .heading("Scanning installed plug-ins")
        .detail(plugin_scan_discovery_text())
        .progress(ProgressBarValue::Indeterminate)
        .footer(if startup_flow {
            "Welcome will open when scanning finishes."
        } else {
            "You can hide this window; scanning will continue."
        })
        .hide_percent();
    let options = if startup_flow {
        options
    } else {
        options.cancel_label("Hide")
    };

    let startup_finished = Arc::new(AtomicBool::new(false));
    let guarded_complete = on_startup_complete.map(|on_complete| {
        let finished = startup_finished.clone();
        Arc::new(move |cx: &mut App| {
            if !finished.swap(true, Ordering::AcqRel) {
                on_complete(cx);
            }
        }) as Arc<dyn Fn(&mut App) + Send + Sync>
    });
    let on_cancel: Option<ProgressDialogCancelCb> = guarded_complete.as_ref().map(|complete| {
        let complete = complete.clone();
        Arc::new(move |_window: &mut Window, cx: &mut App| complete(cx)) as ProgressDialogCancelCb
    });
    let dialog = if startup_flow {
        open_standalone_progress_dialog_window(options, on_cancel, cx)?
    } else {
        open_progress_dialog_window(owner_bounds, options, on_cancel, cx)?
    };

    cx.spawn(async move |cx| {
        let (tx, rx) = std::sync::mpsc::channel::<ScanProgress>();
        let handle = std::thread::spawn(move || {
            PluginRegistry::scan_with_progress(ScanOptions::default(), |progress| {
                let _ = tx.send(progress);
            })
        });

        loop {
            while let Ok(progress) = rx.try_recv() {
                update_scan_progress_dialog(&dialog, progress, startup_flow, cx);
            }
            if handle.is_finished() {
                break;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(32))
                .await;
        }

        while let Ok(progress) = rx.try_recv() {
            update_scan_progress_dialog(&dialog, progress, startup_flow, cx);
        }

        let completed = match handle.join() {
            Ok(result) => {
                let plugin_count = result.plugins.len();
                let failed_count = result.failed.len();
                let detail = if let Some(au_error) = result.au_scan_error.as_deref() {
                    format!(
                        "{plugin_count} plug-in(s) are ready. AudioUnit scan issue: {au_error}"
                    )
                } else if failed_count == 0 {
                    format!("{plugin_count} plug-in(s) are ready to use.")
                } else {
                    format!(
                        "{plugin_count} plug-in(s) are ready; {failed_count} item(s) could not be scanned."
                    )
                };
                let footer = if result.generated_presets > 0 {
                    format!("Registered {} preset(s).", result.generated_presets)
                } else {
                    "The plug-in index is up to date.".to_string()
                };
                ProgressDialogOptions::default()
                    .title("Plug-in Scan")
                    .heading("Scan complete")
                    .detail(detail)
                    .progress(ProgressBarValue::value(1.0))
                    .footer(footer)
                    .cancel_label("Close")
            }
            Err(_) => ProgressDialogOptions::default()
                .title("Plug-in Scan")
                .heading("Scan failed")
                .detail("The plug-in scanner stopped unexpectedly. You can try again from Plug-in Manager.")
                .progress(ProgressBarValue::Indeterminate)
                .footer("No running scan remains.")
                .cancel_label("Close")
                .hide_percent(),
        };
        if let Some(complete) = guarded_complete {
            // Open Welcome before retiring the progress surface so GPUI never
            // observes a zero-window gap under LastWindowClosed behavior.
            let _ = cx.update(|app| complete(app));
            let _ = dialog.update(cx, |_view, window, _cx| window.remove_window());
        } else {
            let _ = dialog.update(cx, |view, _window, cx| {
                view.set_options(completed, cx);
            });
        }
    })
    .detach();

    Ok(())
}

fn update_scan_progress_dialog(
    dialog: &WindowHandle<crate::components::progress_dialog::ProgressDialogWindow>,
    progress: ScanProgress,
    startup_flow: bool,
    cx: &mut gpui::AsyncApp,
) {
    let mut options = match progress {
        ScanProgress::Started { bundle_total } => ProgressDialogOptions::default()
            .title("Plug-in Scan")
            .heading("Scanning installed plug-ins")
            .detail(format!(
                "Found {bundle_total} plug-in bundle(s) to inspect."
            ))
            .progress(if bundle_total == 0 {
                ProgressBarValue::Indeterminate
            } else {
                ProgressBarValue::value(0.0)
            })
            .footer("You can hide this window; scanning will continue.")
            .cancel_label("Hide"),
        ScanProgress::ScanningBundle {
            current,
            total,
            path,
        } => ProgressDialogOptions::default()
            .title("Plug-in Scan")
            .heading("Reading plug-in metadata")
            .detail(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Plug-in bundle"),
            )
            .progress(scan_fraction(current, total))
            .footer(format!("{} of {} bundle(s)", current.min(total), total))
            .cancel_label("Hide"),
        ScanProgress::Registering {
            current,
            total,
            name,
            generated_presets,
            ..
        } => ProgressDialogOptions::default()
            .title("Plug-in Scan")
            .heading("Registering plug-ins")
            .detail(name)
            .progress(scan_fraction(current, total))
            .footer(format!(
                "{} of {} plug-in(s) · {} preset(s)",
                current.min(total),
                total,
                generated_presets
            ))
            .cancel_label("Hide"),
        ScanProgress::Failed { path, error } => ProgressDialogOptions::default()
            .title("Plug-in Scan")
            .heading("Scanning installed plug-ins")
            .detail(format!(
                "Skipped {}: {error}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("plug-in")
            ))
            .progress(ProgressBarValue::Indeterminate)
            .footer("The scan is continuing with the remaining plug-ins.")
            .cancel_label("Hide")
            .hide_percent(),
        ScanProgress::FormatFinished {
            format,
            success_count,
            failed_count,
            crashed_count,
            error,
        } => {
            let format_name = if format == PluginFormat::Au {
                "AudioUnit"
            } else {
                format.label()
            };
            let detail = error.unwrap_or_else(|| format!("{format_name} scan finished."));
            ProgressDialogOptions::default()
                .title("Plug-in Scan")
                .heading(format!("{format_name} scan complete"))
                .detail(detail)
                .progress(ProgressBarValue::Indeterminate)
                .footer(format!(
                    "{success_count} ready · {failed_count} failed · {crashed_count} crashed"
                ))
                .cancel_label("Hide")
                .hide_percent()
        }
    };

    if startup_flow {
        options.footer = Some("Welcome will open when scanning finishes.".to_string());
        options.cancel_label = None;
    }

    let _ = dialog.update(cx, |view, _window, cx| {
        view.set_options(options, cx);
    });
}

fn scan_fraction(current: usize, total: usize) -> ProgressBarValue {
    if total == 0 {
        ProgressBarValue::Indeterminate
    } else {
        ProgressBarValue::value(current as f32 / total as f32)
    }
}

fn plugin_scan_discovery_text() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Discovering VST3, CLAP, and AudioUnit plug-ins…"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Discovering VST3 and CLAP plug-ins…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(id: &str, status: PluginStatus, scan: PluginScanStatus) -> RegistryPlugin {
        RegistryPlugin {
            id: id.to_string(),
            name: id.to_string(),
            vendor: "Acme".to_string(),
            format: PluginFormat::Vst3,
            category: "EQ".to_string(),
            is_ara: false,
            raw_category: None,
            sub_categories: None,
            kind: PluginKind::Effect,
            path: PathBuf::from(format!("C:/Plugins/{id}.vst3")),
            class_id: None,
            version: None,
            sdk_metadata_loaded: true,
            preset_path: PathBuf::from(format!("C:/Cache/{id}.pst")),
            scanned_at_ms: 0,
            status,
            scan_status: scan,
            error_message: None,
        }
    }

    fn state_with(plugins: Vec<RegistryPlugin>) -> PluginManagerDialogState {
        let mut state = PluginManagerDialogState::new_empty(I18n::new("en"));
        state.plugins = plugins;
        state
    }

    /// "Needs Attention" is what the user can fix from here: unregistered,
    /// failed, or crashed — not a plug-in that is simply fine.
    #[test]
    fn attention_lists_what_the_user_can_fix() {
        let state = state_with(vec![
            plugin(
                "ready",
                PluginStatus::PresetReady,
                PluginScanStatus::Success,
            ),
            plugin(
                "unregistered",
                PluginStatus::MissingPreset,
                PluginScanStatus::Success,
            ),
            plugin(
                "failed",
                PluginStatus::PresetReady,
                PluginScanStatus::Failed,
            ),
            plugin(
                "crashed",
                PluginStatus::PresetReady,
                PluginScanStatus::Crashed,
            ),
        ]);
        assert_eq!(state.counts().attention, 3);
        let mut state = state;
        state.sidebar_filter = SidebarFilter::Attention;
        let names: Vec<&str> = state
            .visible_plugins("")
            .into_iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["crashed", "failed", "unregistered"]);
    }

    #[test]
    fn sorting_by_status_puts_problems_last() {
        let mut state = state_with(vec![
            plugin(
                "b-crashed",
                PluginStatus::PresetReady,
                PluginScanStatus::Crashed,
            ),
            plugin(
                "a-ready",
                PluginStatus::PresetReady,
                PluginScanStatus::Success,
            ),
        ]);
        state.sort_key = SortKey::Status;
        let names: Vec<&str> = state
            .visible_plugins("")
            .into_iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["a-ready", "b-crashed"]);
    }
}
