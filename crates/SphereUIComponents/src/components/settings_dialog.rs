mod combo;
mod layout;
mod pages;
mod sections;
mod window;
pub(crate) use combo::*;
pub(crate) use layout::*;
use pages::*;
pub use sections::*;
pub use window::*;

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, StatefulInteractiveElement, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, div, px, size, svg,
};

use crate::assets;
use crate::components::box_list_view::{
    BoxListBadgeTone, box_list_empty_state, box_list_group_label, box_list_icon_button,
    box_list_item, box_list_item_badge, box_list_item_content, box_list_item_leading_icon,
    box_list_item_title, box_list_item_trailing, box_list_toggle, box_list_view,
};
use crate::components::combo_box::{combo_box_string_menu, combo_box_trigger};
use crate::components::controls::{
    FbButtonKind, FbSegment, fb_button, fb_segment, fb_segmented_track, fb_stepper_button,
};
use crate::components::settings_components::settings_restart_label;
use crate::components::settings_layout::{
    SETTINGS_SIDEBAR_WIDTH, SETTINGS_WINDOW_HEIGHT, SETTINGS_WINDOW_WIDTH, settings_status_badge,
};
use crate::components::slider::slider;
use crate::components::text_input::{
    TextInputAction, TextInputCallbacks, TextInputState, text_field_with_callbacks,
};
use crate::components::timeline::render::cached_gpu_devices;
use crate::components::title_bar::external_window_titlebar;
use crate::device_registry::cached_midi_devices;
use crate::i18n::{I18n, Locale};
use crate::overlay::{
    COMBO_TRIGGER_HEIGHT, OverlayAnchor, OverlayPlacement, OverlaySize, anchor_visible_in_window,
    compute_overlay_position, external_dialog_overlay_bounds, form_combo_trigger_bounds,
    refresh_form_anchor, settings_form_column,
};
use crate::settings::{
    DefaultMonitorMode, GpuDevicePreference, MidiDeviceDirection, MidiDeviceSetting, RenderMode,
    SettingsAudioLatencySnapshot, SettingsModel, SettingsSchema, TextRenderingBackend,
};
use crate::theme::{self, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};
use sphere_midi_service::{midi_settings_debug_enabled, resolve_midi_devices, upsert_midi_device};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Audio,
    Midi,
    Recording,
    /// Transport, metronome and count-in, and the engine's playback safety.
    Playback,
    Editing,
    Appearance,
    Performance,
    About,
}

impl SettingsTab {
    pub fn label_key(self) -> &'static str {
        match self {
            Self::General => "settings.tab.general",
            Self::Audio => "settings.tab.audio",
            Self::Midi => "settings.tab.midi",
            Self::Recording => "settings.tab.recording",
            Self::Playback => "settings.tab.playback",
            Self::Editing => "settings.tab.editing",
            Self::Appearance => "settings.tab.appearance",
            Self::Performance => "settings.tab.performance",
            Self::About => "settings.tab.about",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::General => assets::ICON_SLIDERS_HORIZONTAL_PATH,
            Self::Audio => assets::ICON_AUDIO_LINES_PATH,
            Self::Midi => assets::ICON_KEYBOARD_PATH,
            Self::Recording => assets::ICON_CIRCLE_DOT_PATH,
            Self::Playback => assets::ICON_PLAY_PATH,
            Self::Editing => assets::ICON_PENCIL_PATH,
            Self::Appearance => assets::ICON_PALETTE_PATH,
            Self::Performance => assets::ICON_GAUGE_PATH,
            Self::About => assets::ICON_STAR_PATH,
        }
    }

    /// The sidebar, in groups separated by space rather than captions.
    pub fn nav_groups() -> &'static [&'static [Self]] {
        &[
            &[Self::General],
            &[Self::Audio, Self::Midi, Self::Recording, Self::Playback],
            &[Self::Editing, Self::Appearance],
            &[Self::Performance, Self::About],
        ]
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::General,
            Self::Audio,
            Self::Midi,
            Self::Recording,
            Self::Playback,
            Self::Editing,
            Self::Appearance,
            Self::Performance,
            Self::About,
        ]
    }
}

#[cfg(test)]
mod settings_navigation_tests {
    use super::SettingsTab;

    #[test]
    fn the_device_pages_follow_general() {
        let tabs: Vec<SettingsTab> = SettingsTab::nav_groups()
            .iter()
            .flat_map(|tabs| tabs.iter().copied())
            .collect();
        assert_eq!(
            &tabs[..3],
            &[SettingsTab::General, SettingsTab::Audio, SettingsTab::Midi]
        );
    }

    #[test]
    fn the_sidebar_lists_every_page_once() {
        let mut listed: Vec<SettingsTab> = SettingsTab::nav_groups()
            .iter()
            .flat_map(|tabs| tabs.iter().copied())
            .collect();
        assert_eq!(listed.len(), SettingsTab::all().len());
        listed.dedup();
        assert_eq!(listed.len(), SettingsTab::all().len());
        for tab in SettingsTab::all() {
            assert!(listed.contains(tab), "{tab:?} is not in the sidebar");
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingsDialogState {
    pub is_open: bool,
    pub active_tab: SettingsTab,
    pub search_query: String,
    /// Whether the full (possibly long) Driver Status diagnostic text is
    /// expanded. Collapsed by default so the row only shows a concise summary.
    pub driver_status_details_open: bool,
}

impl SettingsDialogState {
    pub fn closed() -> Self {
        Self {
            is_open: false,
            active_tab: SettingsTab::General,
            search_query: String::new(),
            driver_status_details_open: false,
        }
    }

    pub fn open() -> Self {
        Self {
            is_open: true,
            active_tab: SettingsTab::General,
            search_query: String::new(),
            driver_status_details_open: false,
        }
    }
}

pub type UpdateSettingFn = Arc<dyn Fn(&mut SettingsSchema) + Send + Sync + 'static>;
pub type InputTestStartFn =
    Arc<dyn Fn(Option<String>) -> Result<(), String> + Send + Sync + 'static>;
pub type InputTestStopFn = Arc<dyn Fn() + Send + Sync + 'static>;
pub type InputTestLevelFn = Arc<dyn Fn() -> f32 + Send + Sync + 'static>;
pub type AudioDeviceListsProvider =
    Arc<dyn Fn(&str) -> SettingsAudioDeviceLists + Send + Sync + 'static>;
/// Opens the real keyboard-shortcuts editor ([`crate::components::keymap_window::KeymapWindow`])
/// from within the Settings window — the same window the `help:keyboard-shortcuts`
/// menu command opens. Owned by the studio window that opened Settings, since
/// only it holds the live `KeymapManager`.
pub type OnOpenKeyboardShortcuts = Arc<dyn Fn(&mut Window, &mut App) + 'static>;
/// Opens the existing external Plug-in Manager window. Settings remains an
/// information-architecture entry point; scanning and scan status stay owned by
/// the manager rather than being duplicated here.
pub type OnOpenPluginManager = Arc<dyn Fn(&mut Window, &mut App) + 'static>;

#[derive(Debug, Clone, Default)]
pub struct SettingsAudioDeviceLists {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub input_channels: Vec<(String, u32)>,
    pub output_channels: Vec<(String, u32)>,
}

/// `FUTUREBOARD_SETTINGS_PERF_DEBUG=1` — gates Settings-panel timing diagnostics
/// (open time, audio device refresh time, WDM-KS probe time, UI-thread blocking
/// duration, and re-render count per backend change).
pub(crate) fn settings_perf_debug_enabled() -> bool {
    std::env::var("FUTUREBOARD_SETTINGS_PERF_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Maximum characters rendered in the Driver Status row badge. The full text is
/// available behind the Details toggle.
pub(crate) const DRIVER_STATUS_SUMMARY_MAX: usize = 80;

/// Collapse a possibly-huge driver-status string (e.g. a multi-paragraph WDM-KS
/// / Intel-SST diagnostic) into a single bounded line that is cheap to lay out.
/// Rendering the full text in the row forces an expensive per-render text
/// relayout — far worse at 150–200% DPI — so the row always uses this summary.
pub(crate) fn concise_driver_status(full: &str) -> String {
    let first_line = full.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let first_line = first_line.trim();
    if first_line.chars().count() <= DRIVER_STATUS_SUMMARY_MAX {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(DRIVER_STATUS_SUMMARY_MAX).collect();
        format!("{}…", truncated.trim_end())
    }
}

/// Sanitize a persisted audio backend selection for display. A backend id
/// that isn't valid on the current platform (e.g. a Windows-only driver type
/// loaded from settings.json on Linux) must never show up as "selected" —
/// it falls back to the first available option (`Auto`, except on Windows
/// where the default is `WASAPI Shared`). The persisted value on disk is
/// left untouched; this only affects what's rendered.
pub(crate) fn sanitized_backend_label(driver_type: &str, available_backends: &[String]) -> String {
    if available_backends.iter().any(|b| b == driver_type) {
        driver_type.to_string()
    } else {
        available_backends
            .first()
            .cloned()
            .unwrap_or_else(|| "Auto".to_string())
    }
}

/// Full (untruncated) driver-status text for the Details panel / tooltip.
fn driver_status_full(i18n: &I18n, latency: &SettingsAudioLatencySnapshot) -> String {
    if let Some(error) = latency
        .last_error
        .as_ref()
        .filter(|error| !error.is_empty())
    {
        error.clone()
    } else if latency.engine_open && !latency.backend_name.is_empty() {
        format!("{} · {}", latency.device_state, latency.backend_name)
    } else if latency.engine_open && !latency.device_state.is_empty() {
        latency.device_state.clone()
    } else if latency.engine_open {
        i18n.tr("settings.driver-status.ready")
    } else {
        i18n.tr("settings.latency.engine-closed")
    }
}

/// Driver Status: a concise one-line badge plus a `Details` toggle that opens
/// the full diagnostic text in a height-capped scroll box under it. Keeping the
/// long text out of the row prevents a per-render relayout explosion when a
/// backend reports a multi-paragraph error (e.g. WDM-KS on an Intel-SST system).
pub(crate) fn driver_status_rows(
    i18n: &I18n,
    latency: &SettingsAudioLatencySnapshot,
    state: &SettingsDialogState,
    callbacks: &SettingsDialogCallbacks,
) -> Vec<PrefRow> {
    let full = driver_status_full(i18n, latency);
    let summary = concise_driver_status(&full);
    let ok = latency.last_error.is_none()
        && (!latency.engine_open || latency.device_state != "DeviceLost");
    // There is "more" to show only when the summary actually elided something.
    let has_more = full.chars().count() > summary.chars().count()
        || full.lines().filter(|l| !l.trim().is_empty()).count() > 1;
    let details_open = state.driver_status_details_open && has_more;

    let mut control = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(crate::theme::space::SNUG))
        .min_w(px(0.0))
        .child(settings_status_badge(summary, ok));
    if has_more {
        if let Some(toggle) = callbacks.on_toggle_driver_details.clone() {
            control = control.child(fb_button(
                "settings-driver-status-details",
                if details_open { "Hide" } else { "Details" },
                FbButtonKind::Ghost,
                true,
                move |_, w, cx| toggle(w, cx),
            ));
        }
    }

    let mut rows = vec![PrefRow::field(
        i18n.tr("settings.field.driver-status"),
        &["status", "driver", "device", "error"],
        control,
    )];
    if details_open {
        rows.push(PrefRow::block(
            i18n.tr("settings.field.driver-status"),
            &["status", "driver", "error"],
            div()
                .id("settings-driver-status-details-text")
                .max_h(px(120.0))
                .overflow_y_scroll()
                .p(px(crate::theme::space::BASE))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_input())
                .text_size(px(crate::theme::typography::DENSE_LABEL))
                .text_color(Colors::text_secondary())
                .child(full),
        ));
    }
    rows
}

#[derive(Debug, Clone, Default)]
pub struct InputTestMeterState {
    pub active: bool,
    pub level: f32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareCombo {
    Theme,
    AudioDriver,
    InputDevice,
    OutputDevice,
    Language,
    UpdateChannel,
    AutosaveInterval,
    SampleRate,
    BufferSize,
    Renderer,
    GpuDevice,
    FrameRate,
}

#[derive(Clone)]
pub struct SettingsDialogCallbacks {
    pub on_close: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    pub on_select_tab: Arc<dyn Fn(&SettingsTab, &mut Window, &mut App) + 'static>,
    pub on_update_setting: Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
    pub on_toggle_input_test: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    pub on_refresh_midi: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
    pub open_hardware_combo: Option<HardwareCombo>,
    pub on_toggle_hardware_combo:
        Arc<dyn Fn(HardwareCombo, Option<OverlayAnchor>, &mut Window, &mut App) + 'static>,
    /// Toggle expansion of the full Driver Status diagnostic text. `None` for
    /// surfaces that don't expose a live driver status (legacy embedded dialog).
    pub on_toggle_driver_details: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
    /// Opens the real Keyboard Shortcuts editor window. `None` for surfaces
    /// that don't have a studio window to own the `KeymapManager`.
    pub on_open_keyboard_shortcuts: Option<OnOpenKeyboardShortcuts>,
    /// Opens the existing Plug-in Manager for Scan / Rescan and scan status.
    pub on_open_plugin_manager: Option<OnOpenPluginManager>,
}

/// The sidebar and the page for the current state. With a search query, the
/// page is every page's matching rows, each under its page's name, and the
/// sidebar marks which pages have any.
#[allow(clippy::too_many_arguments)]
fn build_settings_content(
    state: &SettingsDialogState,
    schema: &SettingsSchema,
    callbacks: &SettingsDialogCallbacks,
    latency: &SettingsAudioLatencySnapshot,
    input_test: &InputTestMeterState,
    available_inputs: &[String],
    available_outputs: &[String],
    available_backends: &[String],
    available_input_channels: &[(String, u32)],
    available_output_channels: &[(String, u32)],
) -> (Vec<gpui::AnyElement>, Vec<gpui::AnyElement>) {
    let i18n = I18n::new(&schema.general.language);
    let query = state.search_query.trim().to_lowercase();
    let ctx = PageCtx {
        i18n,
        schema,
        state,
        callbacks,
        latency,
        input_test,
        inputs: available_inputs,
        outputs: available_outputs,
        backends: available_backends,
        input_channels: available_input_channels,
        output_channels: available_output_channels,
    };

    if query.is_empty() {
        let sections = page_groups(state.active_tab, &ctx)
            .into_iter()
            .map(PrefGroup::render)
            .collect();
        let sidebar = build_settings_sidebar_items(state, callbacks, i18n, None);
        return (sidebar, sections);
    }

    let mut hits = Vec::new();
    let mut sections = Vec::new();
    for tab in SettingsTab::all().iter().copied() {
        let page_matches = i18n.tr(tab.label_key()).to_lowercase().contains(&query);
        let groups: Vec<PrefGroup> = page_groups(tab, &ctx)
            .into_iter()
            .filter_map(|group| {
                if page_matches {
                    Some(group)
                } else {
                    group.filtered(&query)
                }
            })
            .collect();
        if groups.is_empty() {
            continue;
        }
        hits.push(tab);
        sections.push(search_page_heading(i18n.tr(tab.label_key()), tab.icon()));
        sections.extend(groups.into_iter().map(PrefGroup::render));
    }
    if sections.is_empty() {
        sections.push(
            div()
                .py(px(crate::theme::space::PAGE))
                .text_align(gpui::TextAlign::Center)
                .text_size(px(crate::theme::typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(format!(
                    "No settings match \u{201C}{}\u{201D}",
                    state.search_query.trim()
                ))
                .into_any_element(),
        );
    }
    let sidebar = build_settings_sidebar_items(state, callbacks, i18n, Some(&hits));
    (sidebar, sections)
}

/// A page's name over its matches in search results.
fn search_page_heading(title: String, icon_path: &'static str) -> gpui::AnyElement {
    use crate::theme::{space, typography};
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .pt(px(space::BASE))
        .child(
            svg()
                .path(icon_path)
                .size(px(14.0))
                .text_color(Colors::accent_primary()),
        )
        .child(
            div()
                .text_size(px(typography::UI_MD))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(title),
        )
        .into_any_element()
}

const SETTINGS_WIDTH: f32 = SETTINGS_WINDOW_WIDTH;
const SETTINGS_HEIGHT: f32 = SETTINGS_WINDOW_HEIGHT;
const COMBO_MENU_ESTIMATE_HEIGHT: f32 = 148.0;
const AUTOSAVE_INTERVAL_OPTIONS: &[u32] = &[1, 2, 3, 5, 10, 15, 30, 60];
const SAMPLE_RATE_OPTIONS: &[u32] = &[44100, 48000, 88200, 96000];
const BUFFER_SIZE_OPTIONS: &[u32] = &[64, 128, 256, 512, 1024];

pub type OnSettingUpdate = Arc<dyn Fn(UpdateSettingFn, &mut App) + 'static>;

pub struct SettingsWindow {
    settings: Entity<SettingsModel>,
    active_tab: SettingsTab,
    search_input: TextInputState,
    /// Cut/Copy/Paste menu position for [`Self::search_input`], or `None`.
    /// This window draws its own overlay; the studio shell's does not reach it.
    search_context_menu: Option<(f32, f32)>,
    available_backends: Vec<String>,
    /// Backend-scoped device lists shown by the dropdowns. This is a *cache*:
    /// `render` only ever reads it. It is repopulated off the UI thread by
    /// [`SettingsWindow::refresh_audio_devices`] on open and when the backend
    /// changes — never by enumerating/probing on the render path.
    device_lists: SettingsAudioDeviceLists,
    /// `driver_type` the cached `device_lists` were built for, or `None` until
    /// the first refresh completes. A mismatch with the current backend triggers
    /// exactly one off-thread refresh (coalesced via `device_refresh_in_flight`).
    device_lists_backend: Option<String>,
    /// True while an off-thread device refresh is running, so concurrent renders
    /// don't kick duplicate refreshes.
    device_refresh_in_flight: bool,
    /// Cached Driver Status / latency snapshot. Refreshed alongside the device
    /// lists so the badge updates at most once per refresh result.
    latency: SettingsAudioLatencySnapshot,
    /// Whether the full Driver Status diagnostic text is expanded.
    driver_status_details_open: bool,
    /// Diagnostics: renders observed since the last backend change settled.
    renders_since_backend_change: u32,
    device_lists_provider: Option<AudioDeviceListsProvider>,
    latency_provider: AudioLatencySnapshotProvider,
    input_test_start: Option<InputTestStartFn>,
    input_test_stop: Option<InputTestStopFn>,
    input_test_level: Option<InputTestLevelFn>,
    input_test_active: bool,
    input_test_level_value: f32,
    input_test_error: Option<String>,
    open_hardware_combo: Option<HardwareCombo>,
    hardware_combo_anchor: Option<OverlayAnchor>,
    midi_refresh_nonce: u64,
    midi_refresh_in_flight: bool,
    on_update: OnSettingUpdate,
    on_open_keyboard_shortcuts: Option<OnOpenKeyboardShortcuts>,
    on_open_plugin_manager: Option<OnOpenPluginManager>,
    focus_handle: FocusHandle,
}

#[cfg(test)]
mod driver_status_tests {
    use super::{DRIVER_STATUS_SUMMARY_MAX, concise_driver_status};

    /// The Driver Status row must stay bounded so a long backend diagnostic
    /// can't force an expensive relayout. A multi-paragraph WDM-KS/Intel-SST
    /// error collapses to a single short line; the full text lives behind the
    /// Details toggle.
    #[test]
    fn long_driver_status_collapses_to_bounded_single_line() {
        let huge = "This system does not expose user-mode WDM-KS streaming pins: \
            every audio filter rejected the KS pin property set (0x80070492 \
            ERROR_SET_NOT_FOUND).\nThis is normal on Intel Smart Sound (SST) / \
            SoundWire / \"Universal Audio\" driver stacks.\n"
            .repeat(40);
        assert!(huge.len() > 4000, "fixture should be large");

        let summary = concise_driver_status(&huge);

        // Single line, no embedded newlines.
        assert!(!summary.contains('\n'));
        // Bounded length (chars), regardless of input size. Allow +1 for the
        // appended ellipsis.
        assert!(
            summary.chars().count() <= DRIVER_STATUS_SUMMARY_MAX + 1,
            "summary too long: {} chars",
            summary.chars().count()
        );
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn short_status_is_passed_through_unchanged() {
        assert_eq!(
            concise_driver_status("Active · WASAPI Exclusive"),
            "Active · WASAPI Exclusive"
        );
        assert_eq!(concise_driver_status(""), "");
    }

    #[test]
    fn first_nonblank_line_is_used() {
        assert_eq!(concise_driver_status("\n\n  Ready  \nmore text"), "Ready");
    }
}
