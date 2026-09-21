mod combo;
mod sections;
mod window;
pub(crate) use combo::*;
pub use sections::*;
pub use window::*;

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, svg, App, AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, StatefulInteractiveElement,
    Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::assets;
use crate::components::box_list_view::{
    box_list_empty_state, box_list_group_label, box_list_icon_button, box_list_item,
    box_list_item_badge, box_list_item_content, box_list_item_leading_icon, box_list_item_subtitle,
    box_list_item_title, box_list_item_trailing, box_list_toggle, box_list_view, BoxListBadgeTone,
};
use crate::components::combo_box::{combo_box_string_menu, combo_box_trigger};
use crate::components::controls::{
    fb_button, fb_segmented_button, fb_stepper_button, FbButtonKind,
};
use crate::components::settings_components::{
    settings_readout, settings_restart_footer, settings_restart_label, settings_row,
    settings_row_restart, settings_section, settings_section_hint_text, settings_segmented,
    settings_toggle,
};
use crate::components::settings_layout::{
    settings_daw_row, settings_daw_row_with_description, settings_nav_group_header,
    settings_nav_item, settings_page_header, settings_section_card, settings_section_hint,
    settings_section_title, settings_status_badge, settings_value_readout, SETTINGS_CONTENT_PAD,
    SETTINGS_SECTION_GAP, SETTINGS_SIDEBAR_WIDTH, SETTINGS_WINDOW_HEIGHT, SETTINGS_WINDOW_WIDTH,
};
use crate::components::slider::slider;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputAction, TextInputCallbacks, TextInputState,
};
use crate::components::timeline::render::list_available_gpu_devices;
use crate::components::title_bar::external_window_titlebar;
use crate::device_registry::cached_midi_devices;
use crate::i18n::{I18n, Locale};
use crate::overlay::{
    anchor_visible_in_window, compute_overlay_position, external_dialog_overlay_bounds,
    form_combo_trigger_bounds, refresh_form_anchor, settings_form_column, OverlayAnchor,
    OverlayPlacement, OverlaySize, COMBO_TRIGGER_HEIGHT,
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
    Metronome,
    Playback,
    Editing,
    Appearance,
    Plugins,
    FilesMedia,
    Shortcuts,
    Performance,
    Advanced,
    About,
}

impl SettingsTab {
    pub fn label_key(self) -> &'static str {
        match self {
            Self::General => "settings.tab.general",
            Self::Audio => "settings.tab.audio",
            Self::Midi => "settings.tab.midi",
            Self::Recording => "settings.tab.recording",
            Self::Metronome => "settings.tab.metronome",
            Self::Playback => "settings.tab.playback",
            Self::Editing => "settings.tab.editing",
            Self::Appearance => "settings.tab.appearance",
            Self::Plugins => "settings.tab.plugins",
            Self::FilesMedia => "settings.tab.files-media",
            Self::Shortcuts => "settings.tab.shortcuts",
            Self::Performance => "settings.tab.performance",
            Self::Advanced => "settings.tab.advanced",
            Self::About => "settings.tab.about",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::General => assets::ICON_FILE_PATH,
            Self::Audio => assets::ICON_MIC_PATH,
            Self::Midi => assets::ICON_LINK_PATH,
            Self::Recording => assets::ICON_CIRCLE_PATH,
            Self::Metronome => assets::ICON_METRONOME_PATH,
            Self::Playback => assets::ICON_PLAY_PATH,
            Self::Editing => assets::ICON_PENCIL_PATH,
            Self::Appearance => assets::ICON_SLIDERS_HORIZONTAL_PATH,
            Self::Plugins => assets::ICON_CPU_PATH,
            Self::FilesMedia => assets::ICON_FOLDER_PATH,
            Self::Shortcuts => assets::ICON_LINK_PATH,
            Self::Performance => assets::ICON_CPU_PATH,
            Self::Advanced => assets::ICON_CLOCK_PATH,
            Self::About => assets::ICON_CIRCLE_DOT_PATH,
        }
    }

    pub fn page_description_key(self) -> &'static str {
        match self {
            Self::General => "settings.tab.general.description",
            Self::Audio => "settings.tab.audio.description",
            Self::Midi => "settings.tab.midi.description",
            Self::Recording => "settings.tab.recording.description",
            Self::Metronome => "settings.tab.metronome.description",
            Self::Playback => "settings.tab.playback.description",
            Self::Editing => "settings.tab.editing.description",
            Self::Appearance => "settings.tab.appearance.description",
            Self::Plugins => "settings.tab.plugins.description",
            Self::FilesMedia => "settings.tab.files-media.description",
            Self::Shortcuts => "settings.tab.shortcuts.description",
            Self::Performance => "settings.tab.performance.description",
            Self::Advanced => "settings.tab.advanced.description",
            Self::About => "settings.tab.about.description",
        }
    }

    pub fn nav_groups() -> &'static [(&'static str, &'static [Self])] {
        &[
            ("settings.nav.general", &[Self::General]),
            (
                "settings.nav.studio",
                &[
                    Self::Audio,
                    Self::Midi,
                    Self::Plugins,
                    Self::Recording,
                    Self::Metronome,
                    Self::Playback,
                ],
            ),
            ("settings.nav.workflow", &[Self::Editing, Self::FilesMedia]),
            (
                "settings.nav.interface",
                &[Self::Appearance, Self::Shortcuts],
            ),
            (
                "settings.nav.system",
                &[Self::Performance, Self::Advanced, Self::About],
            ),
        ]
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::General,
            Self::Audio,
            Self::Midi,
            Self::Recording,
            Self::Metronome,
            Self::Playback,
            Self::Editing,
            Self::Appearance,
            Self::Plugins,
            Self::FilesMedia,
            Self::Shortcuts,
            Self::Performance,
            Self::Advanced,
            Self::About,
        ]
    }
}

#[cfg(test)]
mod settings_navigation_tests {
    use super::SettingsTab;

    #[test]
    fn primary_device_and_plugin_categories_are_adjacent() {
        let tabs: Vec<SettingsTab> = SettingsTab::nav_groups()
            .iter()
            .flat_map(|(_, tabs)| tabs.iter().copied())
            .collect();
        assert_eq!(
            &tabs[..4],
            &[
                SettingsTab::General,
                SettingsTab::Audio,
                SettingsTab::Midi,
                SettingsTab::Plugins,
            ]
        );
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

/// Driver Status row: a concise one-line badge plus a `Details` toggle that
/// expands the full diagnostic text into a height-capped scroll box. Keeping the
/// long text out of the row prevents a per-render relayout explosion when a
/// backend reports a multi-paragraph error (e.g. WDM-KS on an Intel-SST system).
pub(crate) fn driver_status_row(
    i18n: &I18n,
    latency: &SettingsAudioLatencySnapshot,
    state: &SettingsDialogState,
    callbacks: &SettingsDialogCallbacks,
) -> impl IntoElement {
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
        .gap(px(6.0))
        .min_w(px(0.0))
        .child(settings_status_badge(summary, ok));
    if has_more {
        if let Some(toggle) = callbacks.on_toggle_driver_details.clone() {
            control = control.child(fb_button(
                "settings-driver-status-details",
                if details_open { "Hide" } else { "Details" },
                FbButtonKind::Default,
                true,
                move |_, w, cx| toggle(w, cx),
            ));
        }
    }

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(settings_daw_row(
            i18n.tr("settings.field.driver-status"),
            control,
        ))
        .when(details_open, |col| {
            col.child(
                div()
                    .id("settings-driver-status-details-text")
                    .max_h(px(120.0))
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .rounded(px(crate::theme::radius::CONTROL))
                    .border(px(1.0))
                    .border_color(Colors::border_subtle())
                    .bg(Colors::surface_input())
                    .text_size(px(10.5))
                    .text_color(Colors::text_secondary())
                    .child(full),
            )
        })
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
    ClockSource,
    Language,
    UpdateChannel,
    AutosaveInterval,
    AutosaveMaxBackups,
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
    let is_match = |label: &str, keywords: &[&str]| {
        if query.is_empty() {
            return true;
        }
        let q = query.as_str();
        label.to_lowercase().contains(q) || keywords.iter().any(|k| k.to_lowercase().contains(q))
    };

    let sidebar_items =
        build_settings_sidebar_items(state, callbacks, i18n, query.as_str(), &is_match);

    // Right Side Content Views Builder
    let mut sections = Vec::new();

    // General Panel
    if (state.active_tab == SettingsTab::General && query.is_empty())
        || (!query.is_empty()
            && (is_match("Language", &["language", "english"])
                || is_match("Show start screen", &["start", "screen", "wizard"])
                || is_match("Check updates", &["updates", "check"])))
    {
        let on_update = callbacks.on_update_setting.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.application",
                    assets::ICON_FILE_PATH,
                ))
                .child(settings_daw_row(i18n.tr("settings.field.language"), {
                    let open_combo = callbacks.open_hardware_combo;
                    let on_toggle = callbacks.on_toggle_hardware_combo.clone();
                    let selected = selected_locale_label(i18n, &schema.general.language);
                    hardware_select(
                        HardwareCombo::Language,
                        "settings-general-language",
                        &selected,
                        open_combo,
                        on_toggle,
                    )
                }))
                .child(settings_daw_row(i18n.tr("settings.field.start-wizard"), {
                    let val = schema.general.show_start_screen;
                    let up = on_update.clone();
                    settings_labeled_checkbox(
                        "show-start-screen",
                        val,
                        i18n.tr("settings.show-start-screen"),
                        move |_, w, cx| {
                            up(Arc::new(move |s| s.general.show_start_screen = !val), w, cx);
                        },
                    )
                }))
                .child(settings_daw_row(i18n.tr("settings.field.update-check"), {
                    let val = schema.general.check_updates;
                    let up = on_update.clone();
                    settings_labeled_checkbox(
                        "check-updates",
                        val,
                        i18n.tr("settings.check-updates"),
                        move |_, w, cx| {
                            up(Arc::new(move |s| s.general.check_updates = !val), w, cx);
                        },
                    )
                }))
                .child(settings_daw_row(
                    i18n.tr("settings.field.update-channel"),
                    {
                        hardware_select(
                            HardwareCombo::UpdateChannel,
                            "settings-general-update-channel",
                            schema.general.update_channel.label(),
                            callbacks.open_hardware_combo,
                            callbacks.on_toggle_hardware_combo.clone(),
                        )
                    },
                ))
                .into_any_element(),
        );
    }

    // General Panel > Autosave & Notifications
    if (state.active_tab == SettingsTab::General && query.is_empty())
        || (!query.is_empty()
            && (is_match("Autosave", &["autosave", "backup", "minutes"])
                || is_match("Notifications", &["warnings", "alerts", "notifications"])))
    {
        let on_update = callbacks.on_update_setting.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.autosave-backup",
                    assets::ICON_FILE_PATH,
                ))
                .child(settings_daw_row(i18n.tr("settings.field.autosave"), {
                    let val = schema.general.autosave.enabled;
                    let up = on_update.clone();
                    settings_labeled_checkbox(
                        "autosave-enabled",
                        val,
                        i18n.tr("settings.autosave.enabled"),
                        move |_, w, cx| {
                            up(Arc::new(move |s| s.general.autosave.enabled = !val), w, cx);
                        },
                    )
                }))
                .child(settings_daw_row(i18n.tr("settings.field.interval"), {
                    let open_combo = callbacks.open_hardware_combo;
                    let on_toggle = callbacks.on_toggle_hardware_combo.clone();
                    let interval = schema.general.autosave.interval_minutes;
                    hardware_select(
                        HardwareCombo::AutosaveInterval,
                        "settings-general-autosave-interval",
                        &i18n.tr_vars("settings.interval.minutes", &[("n", interval.to_string())]),
                        open_combo,
                        on_toggle,
                    )
                }))
                .child(settings_daw_row(i18n.tr("settings.field.max-backups"), {
                    let open_combo = callbacks.open_hardware_combo;
                    let on_toggle = callbacks.on_toggle_hardware_combo.clone();
                    hardware_select(
                        HardwareCombo::AutosaveMaxBackups,
                        "settings-general-autosave-backups",
                        &schema.general.autosave.max_backups.to_string(),
                        open_combo,
                        on_toggle,
                    )
                }))
                .into_any_element(),
        );

        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.notifications",
                    assets::ICON_FILE_PATH,
                ))
                .child(settings_daw_row(i18n.tr("settings.field.warnings"), {
                    let val = schema.general.notifications.enable_warnings;
                    let up = on_update.clone();
                    settings_labeled_checkbox(
                        "notif-warnings-enabled",
                        val,
                        i18n.tr("settings.notifications.warnings"),
                        move |_, w, cx| {
                            up(
                                Arc::new(move |s| s.general.notifications.enable_warnings = !val),
                                w,
                                cx,
                            );
                        },
                    )
                }))
                .child(settings_daw_row(
                    i18n.tr("settings.field.system-notifications"),
                    {
                        let val = schema.general.notifications.enable_system_notifications;
                        let up = on_update.clone();
                        settings_labeled_checkbox(
                            "notif-system-enabled",
                            val,
                            i18n.tr("settings.notifications.system"),
                            move |_, w, cx| {
                                up(
                                    Arc::new(move |s| {
                                        s.general.notifications.enable_system_notifications = !val
                                    }),
                                    w,
                                    cx,
                                );
                            },
                        )
                    },
                ))
                .into_any_element(),
        );
    }

    if (state.active_tab == SettingsTab::General && query.is_empty())
        || (!query.is_empty() && (is_match("Tempo", &["tempo", "bpm"])))
    {
        let on_update = callbacks.on_update_setting.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.project-defaults",
                    assets::ICON_FILE_PATH,
                ))
                .child(settings_section_hint(
                    i18n.tr("settings.project-defaults.hint"),
                ))
                // These are application defaults for *new* projects. The open
                // project's own tempo, meter, and sample rate are edited in the
                // Project Settings window (Project → Project Settings), so the
                // two scopes are not confused for each other.
                .child(settings_section_hint(
                    "The open project's tempo, time signature, and sample rate are edited in \
                     Project Settings.",
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.default-tempo"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child({
                            let up = on_update.clone();
                            let tempo = schema.general.project_defaults.tempo;
                            fb_stepper_button("tempo-dec", "-", move |_, w, cx| {
                                up(
                                    Arc::new(move |s| {
                                        s.general.project_defaults.tempo = (tempo - 1.0).max(20.0)
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .w(px(52.0))
                                .h(px(28.0))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .border(px(1.0))
                                .border_color(Colors::border_subtle())
                                .bg(Colors::surface_input())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(11.0))
                                .text_color(Colors::text_primary())
                                .child(format!("{:.0}", schema.general.project_defaults.tempo)),
                        )
                        .child({
                            let up = on_update.clone();
                            let tempo = schema.general.project_defaults.tempo;
                            fb_stepper_button("tempo-inc", "+", move |_, w, cx| {
                                up(
                                    Arc::new(move |s| {
                                        s.general.project_defaults.tempo = (tempo + 1.0).min(999.0)
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(i18n.tr("settings.bpm")),
                        ),
                ))
                .into_any_element(),
        );
    }

    // Audio panel
    if (state.active_tab == SettingsTab::Audio && query.is_empty())
        || (!query.is_empty()
            && (is_match(
                "Audio Driver",
                &["driver", "backend", "wasapi", "wdm", "ks", "asio"],
            ) || is_match("Input Device", &["input", "microphone"])
                || is_match("Output Device", &["output", "speakers"])
                || is_match("Sample Rate", &["sample", "rate", "hz"])
                || is_match("Buffer Size", &["buffer", "latency"])))
    {
        let open_combo = callbacks.open_hardware_combo;
        let on_toggle = callbacks.on_toggle_hardware_combo.clone();

        let driver_label =
            sanitized_backend_label(&schema.hardware.audio.driver_type, available_backends);
        let driver_select = hardware_select(
            HardwareCombo::AudioDriver,
            "settings-audio-driver",
            &driver_label,
            open_combo,
            on_toggle.clone(),
        );

        // Use the effective (edition-sanitized) driver — a latent Exclusive
        // `"ASIO"` pin must not keep the ASIO-only device UI alive on Community.
        let is_asio = driver_label == "ASIO";
        let input_label = if schema.hardware.audio.device_in.trim().is_empty()
            || !available_inputs.contains(&schema.hardware.audio.device_in)
        {
            "Default".to_string()
        } else {
            schema.hardware.audio.device_in.clone()
        };
        let input_select = hardware_select(
            HardwareCombo::InputDevice,
            "settings-audio-input",
            &input_label,
            open_combo,
            on_toggle.clone(),
        );

        let output_label = if schema.hardware.audio.device_out.trim().is_empty()
            || !available_outputs.contains(&schema.hardware.audio.device_out)
        {
            "Default".to_string()
        } else {
            schema.hardware.audio.device_out.clone()
        };
        let output_select = hardware_select(
            HardwareCombo::OutputDevice,
            "settings-audio-output",
            &output_label,
            open_combo,
            on_toggle.clone(),
        );

        let buffer_ms = latency.buffer_ms.max(
            schema.general.project_defaults.buffer_size as f64
                / schema.general.project_defaults.sample_rate as f64
                * 1000.0,
        );

        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.audio-engine",
                    assets::ICON_MIC_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.backend"),
                    driver_select,
                ))
                .when(!is_asio, |section| {
                    section.child(settings_daw_row(
                        i18n.tr("settings.field.input-device"),
                        input_select,
                    ))
                })
                .child(settings_daw_row(
                    if is_asio {
                        "ASIO Device".to_string()
                    } else {
                        i18n.tr("settings.field.output-device")
                    },
                    output_select,
                ))
                .child(driver_status_row(&i18n, latency, state, &callbacks))
                .into_any_element(),
        );

        // Input / Output channel routes for the currently selected devices.
        // When no device is explicitly chosen (device_in/out empty — "Default"),
        // fall back to the first scanned device's channel count instead of 0,
        // so a real scan result isn't reported as "No channels reported".
        let in_count = available_input_channels
            .iter()
            .find(|(name, _)| *name == schema.hardware.audio.device_in)
            .or_else(|| {
                schema
                    .hardware
                    .audio
                    .device_in
                    .trim()
                    .is_empty()
                    .then(|| available_input_channels.first())
                    .flatten()
            })
            .map(|(_, count)| *count)
            .unwrap_or(0);
        sections.push(audio_channel_section(
            "Input Channels",
            &input_label,
            &crate::audio_routing::build_input_channel_options(in_count),
        ));
        let out_count = available_output_channels
            .iter()
            .find(|(name, _)| *name == schema.hardware.audio.device_out)
            .or_else(|| {
                schema
                    .hardware
                    .audio
                    .device_out
                    .trim()
                    .is_empty()
                    .then(|| available_output_channels.first())
                    .flatten()
            })
            .map(|(_, count)| *count)
            .unwrap_or(0);
        sections.push(audio_channel_section(
            "Output Channels",
            &output_label,
            &crate::audio_routing::build_output_channel_options(out_count),
        ));

        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.sample-rate-buffer",
                    assets::ICON_MIC_PATH,
                ))
                .child(settings_daw_row(i18n.tr("settings.field.sample-rate"), {
                    let open_combo = callbacks.open_hardware_combo;
                    let on_toggle = callbacks.on_toggle_hardware_combo.clone();
                    let sr = schema.general.project_defaults.sample_rate;
                    hardware_select(
                        HardwareCombo::SampleRate,
                        "settings-audio-sample-rate",
                        &i18n.tr_vars("settings.sample-rate.hz", &[("rate", sr.to_string())]),
                        open_combo,
                        on_toggle,
                    )
                }))
                .child(settings_daw_row(i18n.tr("settings.field.buffer-size"), {
                    let open_combo = callbacks.open_hardware_combo;
                    let on_toggle = callbacks.on_toggle_hardware_combo.clone();
                    let buf = schema.general.project_defaults.buffer_size;
                    hardware_select(
                        HardwareCombo::BufferSize,
                        "settings-audio-buffer-size",
                        &format!("{buf}"),
                        open_combo,
                        on_toggle,
                    )
                }))
                .child(settings_daw_row(
                    i18n.tr("settings.field.output-buffer-latency"),
                    settings_value_readout(latency_ms_label(
                        &i18n,
                        if latency.engine_open { buffer_ms } else { 0.0 },
                    )),
                ))
                .child(settings_section_hint(i18n.tr("settings.buffer.hint")))
                .into_any_element(),
        );

        sections.push(
            audio_latency_report_section(&i18n, latency, schema.playback.latency_compensation)
                .into_any_element(),
        );
    }

    // MIDI panel
    if (state.active_tab == SettingsTab::Midi && query.is_empty())
        || (!query.is_empty()
            && (is_match(
                "MIDI Enabled Inputs",
                &["midi", "inputs", "outputs", "port", "keyboard"],
            ) || is_match("Sync Clock", &["sync", "clock", "source", "ltc"])))
    {
        let on_update = callbacks.on_update_setting.clone();
        sections.push(
            midi_devices_section(
                schema,
                &i18n,
                on_update.clone(),
                callbacks.on_refresh_midi.clone(),
            )
            .into_any_element(),
        );

        let clock_select = hardware_select(
            HardwareCombo::ClockSource,
            "settings-clock-source",
            &schema.hardware.sync.clock_source,
            callbacks.open_hardware_combo,
            callbacks.on_toggle_hardware_combo.clone(),
        );
        let ltc_enabled = schema.hardware.sync.ltc_enabled;
        let up_ltc = on_update.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.sync-external-clock",
                    assets::ICON_CLOCK_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.clock-source"),
                    clock_select,
                ))
                .child(settings_daw_row_with_description(
                    i18n.tr("settings.field.ltc-reader"),
                    Some(i18n.tr("settings.ltc.description")),
                    box_list_toggle("sync-ltc-enabled", ltc_enabled, move |_, w, cx| {
                        let next = !ltc_enabled;
                        if midi_settings_debug_enabled() {
                            eprintln!("[MIDI settings] ltc_enabled={next}");
                        }
                        up_ltc(Arc::new(move |s| s.hardware.sync.ltc_enabled = next), w, cx);
                    }),
                ))
                .into_any_element(),
        );
    }

    // Appearance Panel (Theme, sliders)
    if (state.active_tab == SettingsTab::Appearance && query.is_empty())
        || (!query.is_empty()
            && (is_match("Theme", &["theme", "fleet", "dark"])
                || (cfg!(target_os = "windows")
                    && is_match(
                        "Text Rendering",
                        &["text", "font", "render", "directwrite", "gdi", "blurry"],
                    ))
                || is_match("UI Scale", &["scale", "size"])
                || is_match("Arrangement Grid", &["grid", "intensity", "opacity"])
                || is_match("Piano Roll Guides", &["piano", "roll", "guides", "keys"])
                || is_match("Mixer Meter", &["mixer", "decay", "peak", "hold"])))
    {
        let on_update = callbacks.on_update_setting.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.theme-interface",
                    assets::ICON_SLIDERS_HORIZONTAL_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.theme-preset"),
                    hardware_select(
                        HardwareCombo::Theme,
                        "settings-theme",
                        &theme::available_theme_summaries()
                            .into_iter()
                            .find(|(id, _)| id == &schema.appearance.theme)
                            .map(|(_, name)| name)
                            .unwrap_or_else(|| schema.appearance.theme.clone()),
                        callbacks.open_hardware_combo,
                        callbacks.on_toggle_hardware_combo.clone(),
                    ),
                ))
                .when(cfg!(target_os = "windows"), |section| {
                    section
                        .child(settings_daw_row(
                            settings_restart_label(i18n.tr("settings.field.text-rendering"), true),
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(settings_segmented(
                                    "settings-text-rendering",
                                    &[
                                        (TextRenderingBackend::DirectWrite, "DirectWrite"),
                                        (TextRenderingBackend::Gdi, "GDI+"),
                                    ],
                                    schema.appearance.text_rendering,
                                    {
                                        let up = on_update.clone();
                                        Arc::new(move |backend: TextRenderingBackend, w, cx| {
                                            up(
                                                Arc::new(move |s| {
                                                    s.appearance.text_rendering = backend
                                                }),
                                                w,
                                                cx,
                                            );
                                        })
                                    },
                                ))
                                .child(settings_section_hint_text(
                                    i18n.tr("settings.hint.text-rendering"),
                                )),
                        ))
                        .child(settings_restart_footer())
                })
                .child(settings_daw_row(
                    i18n.tr("settings.field.ui-scale"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(slider(
                            "ui-scale-slider",
                            (schema.appearance.ui_scale - 0.5) / 2.0, // map [0.5, 2.5] to [0, 1]
                            Colors::accent_primary(),
                            {
                                let up = on_update.clone();
                                move |val, w, cx| {
                                    let actual_val = 0.5 + val * 2.0;
                                    up(
                                        Arc::new(move |s| s.appearance.ui_scale = actual_val),
                                        w,
                                        cx,
                                    );
                                }
                            },
                        ))
                        .child(
                            div()
                                .w(px(32.0))
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(format!("{:.1}x", schema.appearance.ui_scale)),
                        ),
                ))
                .into_any_element(),
        );

        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.timeline",
                    assets::ICON_SLIDERS_HORIZONTAL_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.grid-intensity"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(slider(
                            "grid-intensity-slider",
                            schema.appearance.arrangement.grid_line_intensity,
                            Colors::accent_primary(),
                            {
                                let up = on_update.clone();
                                move |val, w, cx| {
                                    let intensity = *val;
                                    up(
                                        Arc::new(move |s| {
                                            s.appearance.arrangement.grid_line_intensity = intensity
                                        }),
                                        w,
                                        cx,
                                    );
                                }
                            },
                        ))
                        .child(
                            div()
                                .w(px(32.0))
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(format!(
                                    "{:.0}%",
                                    schema.appearance.arrangement.grid_line_intensity * 100.0
                                )),
                        ),
                ))
                .into_any_element(),
        );

        let up = on_update.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.piano-roll",
                    assets::ICON_PENCIL_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.key-guides"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child({
                            let val = schema.appearance.piano_roll.show_key_guides;
                            let up_guides = up.clone();
                            fb_checkbox("appearance-key-guides", val, move |_, w, cx| {
                                up_guides(
                                    Arc::new(move |s| {
                                        s.appearance.piano_roll.show_key_guides = !val
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(i18n.tr("settings.piano-roll.key-guides")),
                        ),
                ))
                .into_any_element(),
        );

        let up = on_update.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.mixer-metering",
                    assets::ICON_SLIDERS_HORIZONTAL_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.meter-decay"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(slider(
                            "mixer-decay-slider",
                            (schema.appearance.mixer.meter_decay_db_per_sec - 12.0) / 36.0, // map [12, 48] to [0, 1]
                            Colors::accent_primary(),
                            {
                                let up_decay = up.clone();
                                move |val, w, cx| {
                                    let actual_val = 12.0 + val * 36.0;
                                    up_decay(
                                        Arc::new(move |s| {
                                            s.appearance.mixer.meter_decay_db_per_sec = actual_val
                                        }),
                                        w,
                                        cx,
                                    );
                                }
                            },
                        ))
                        .child(
                            div()
                                .w(px(52.0))
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(format!(
                                    "{:.1} dB/s",
                                    schema.appearance.mixer.meter_decay_db_per_sec
                                )),
                        ),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.peak-hold"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(slider(
                            "mixer-peak-slider",
                            (schema.appearance.mixer.peak_hold_seconds - 0.5) / 4.5, // map [0.5, 5.0] to [0, 1]
                            Colors::accent_primary(),
                            {
                                let up_peak = up.clone();
                                move |val, w, cx| {
                                    let actual_val = 0.5 + val * 4.5;
                                    up_peak(
                                        Arc::new(move |s| {
                                            s.appearance.mixer.peak_hold_seconds = actual_val
                                        }),
                                        w,
                                        cx,
                                    );
                                }
                            },
                        ))
                        .child(
                            div()
                                .w(px(52.0))
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(format!(
                                    "{:.1} s",
                                    schema.appearance.mixer.peak_hold_seconds
                                )),
                        ),
                ))
                .into_any_element(),
        );
    }

    // Editing Panel (Mouse, snap, undo history)
    if (state.active_tab == SettingsTab::Editing && query.is_empty())
        || (!query.is_empty()
            && (is_match("Mouse Zoom", &["mouse", "zoom", "sensitivity", "natural"])
                || is_match("Snap to Grid", &["snap", "grid", "default"])
                || is_match("Undo History", &["undo", "redo", "history", "max"])))
    {
        let on_update = callbacks.on_update_setting.clone();

        sections.push(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.editing-mouse",
                    assets::ICON_PENCIL_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.zoom-sensitivity"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(slider(
                            "zoom-sensitivity-slider",
                            (schema.editing.mouse.zoom_sensitivity - 0.2) / 1.8, // map [0.2, 2.0] to [0, 1]
                            Colors::accent_primary(),
                            {
                                let up = on_update.clone();
                                move |val, w, cx| {
                                    let actual_val = 0.2 + val * 1.8;
                                    up(
                                        Arc::new(move |s| {
                                            s.editing.mouse.zoom_sensitivity = actual_val
                                        }),
                                        w,
                                        cx,
                                    );
                                }
                            },
                        ))
                        .child(
                            div()
                                .w(px(32.0))
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(format!("{:.1}x", schema.editing.mouse.zoom_sensitivity)),
                        ),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.natural-scroll"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child({
                            let val = schema.editing.mouse.natural_scroll;
                            let up = on_update.clone();
                            fb_checkbox("editing-natural-scroll", val, move |_, w, cx| {
                                up(
                                    Arc::new(move |s| s.editing.mouse.natural_scroll = !val),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(i18n.tr("settings.natural-scroll")),
                        ),
                ))
                .into_any_element(),
        );

        let up = on_update.clone();
        sections.push(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .mt(px(12.0))
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.editing-snap",
                    assets::ICON_SLIDERS_HORIZONTAL_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.snap-to-grid"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child({
                            let val = schema.editing.snap.snap_to_grid;
                            let up_snap = up.clone();
                            fb_checkbox("editing-snap-grid", val, move |_, w, cx| {
                                up_snap(
                                    Arc::new(move |s| s.editing.snap.snap_to_grid = !val),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(i18n.tr("settings.snap-to-grid")),
                        ),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.default-snap"),
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .child({
                            let val = schema.editing.snap.default_snap_value.clone();
                            let up_val = up.clone();
                            fb_segmented_button("snap-1-4", "1/4", val == "1/4", move |_, w, cx| {
                                up_val(
                                    Arc::new(|s| {
                                        s.editing.snap.default_snap_value = "1/4".to_string()
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child({
                            let val = schema.editing.snap.default_snap_value.clone();
                            let up_val = up.clone();
                            fb_segmented_button("snap-1-8", "1/8", val == "1/8", move |_, w, cx| {
                                up_val(
                                    Arc::new(|s| {
                                        s.editing.snap.default_snap_value = "1/8".to_string()
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child({
                            let val = schema.editing.snap.default_snap_value.clone();
                            let up_val = up.clone();
                            fb_segmented_button(
                                "snap-1-16",
                                "1/16",
                                val == "1/16",
                                move |_, w, cx| {
                                    up_val(
                                        Arc::new(|s| {
                                            s.editing.snap.default_snap_value = "1/16".to_string()
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        })
                        .child({
                            let val = schema.editing.snap.default_snap_value.clone();
                            let up_val = up.clone();
                            fb_segmented_button(
                                "snap-1-32",
                                "1/32",
                                val == "1/32",
                                move |_, w, cx| {
                                    up_val(
                                        Arc::new(|s| {
                                            s.editing.snap.default_snap_value = "1/32".to_string()
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        }),
                ))
                .into_any_element(),
        );

        let up = on_update.clone();
        sections.push(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .mt(px(12.0))
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.editing-history",
                    assets::ICON_CLOCK_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.max-undo-steps"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child({
                            let val = schema.editing.history.max_undo_steps;
                            let up_steps = up.clone();
                            fb_stepper_button("undo-steps-dec", "-", move |_, w, cx| {
                                up_steps(
                                    Arc::new(move |s| {
                                        s.editing.history.max_undo_steps =
                                            val.saturating_sub(5).max(10)
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .w(px(40.0))
                                .h(px(28.0))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .border(px(1.0))
                                .border_color(Colors::border_subtle())
                                .bg(Colors::surface_input())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(11.0))
                                .text_color(Colors::text_primary())
                                .child(schema.editing.history.max_undo_steps.to_string()),
                        )
                        .child({
                            let val = schema.editing.history.max_undo_steps;
                            let up_steps = up.clone();
                            fb_stepper_button("undo-steps-inc", "+", move |_, w, cx| {
                                up_steps(
                                    Arc::new(move |s| {
                                        s.editing.history.max_undo_steps = (val + 5).min(500)
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        }),
                ))
                .into_any_element(),
        );
    }

    // Recording Panel (audio format and input test)
    if (state.active_tab == SettingsTab::Recording && query.is_empty())
        || (!query.is_empty()
            && (is_match("Audio Recording Format", &["format", "bit", "depth", "wav"])
                || is_match("Input Test Meter", &["input", "test", "meter", "level"])))
    {
        let on_update = callbacks.on_update_setting.clone();

        sections.push(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.recording-audio-format",
                    assets::ICON_CIRCLE_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.format-type"),
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .child({
                            let val = schema.recording.audio.format.clone();
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-format-wav",
                                i18n.tr("settings.format.wav"),
                                val == "wav",
                                move |_, w, cx| {
                                    up(
                                        Arc::new(|s| s.recording.audio.format = "wav".to_string()),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        })
                        .child({
                            let val = schema.recording.audio.format.clone();
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-format-aiff",
                                i18n.tr("settings.format.aiff"),
                                val == "aiff",
                                move |_, w, cx| {
                                    up(
                                        Arc::new(|s| s.recording.audio.format = "aiff".to_string()),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        })
                        .child({
                            let val = schema.recording.audio.format.clone();
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-format-flac",
                                i18n.tr("settings.format.flac"),
                                val == "flac",
                                move |_, w, cx| {
                                    up(
                                        Arc::new(|s| s.recording.audio.format = "flac".to_string()),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        }),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.bit-depth"),
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .child({
                            let val = schema.recording.audio.bit_depth;
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-depth-16",
                                i18n.tr("settings.bit-depth.16"),
                                val == 16,
                                move |_, w, cx| {
                                    up(Arc::new(|s| s.recording.audio.bit_depth = 16), w, cx);
                                },
                            )
                        })
                        .child({
                            let val = schema.recording.audio.bit_depth;
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-depth-24",
                                i18n.tr("settings.bit-depth.24"),
                                val == 24,
                                move |_, w, cx| {
                                    up(Arc::new(|s| s.recording.audio.bit_depth = 24), w, cx);
                                },
                            )
                        })
                        .child({
                            let val = schema.recording.audio.bit_depth;
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-depth-32",
                                i18n.tr("settings.bit-depth.32-float"),
                                val == 32,
                                move |_, w, cx| {
                                    up(Arc::new(|s| s.recording.audio.bit_depth = 32), w, cx);
                                },
                            )
                        }),
                ))
                .child(settings_daw_row(
                    "Default Monitor",
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .child({
                            let val = schema.recording.default_monitor_mode;
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-monitor-off",
                                "Off",
                                val == DefaultMonitorMode::Off,
                                move |_, w, cx| {
                                    up(
                                        Arc::new(|s| {
                                            s.recording.default_monitor_mode =
                                                DefaultMonitorMode::Off
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        })
                        .child({
                            let val = schema.recording.default_monitor_mode;
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-monitor-auto",
                                "Auto",
                                val == DefaultMonitorMode::Auto,
                                move |_, w, cx| {
                                    up(
                                        Arc::new(|s| {
                                            s.recording.default_monitor_mode =
                                                DefaultMonitorMode::Auto
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        })
                        .child({
                            let val = schema.recording.default_monitor_mode;
                            let up = on_update.clone();
                            fb_segmented_button(
                                "rec-monitor-input",
                                "Input",
                                val == DefaultMonitorMode::Input,
                                move |_, w, cx| {
                                    up(
                                        Arc::new(|s| {
                                            s.recording.default_monitor_mode =
                                                DefaultMonitorMode::Input
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        }),
                ))
                .child(settings_daw_row(
                    "Generate Waveform",
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child({
                            let val = schema.recording.audio.generate_waveform_after_record;
                            let up = on_update.clone();
                            fb_checkbox("rec-generate-waveform", val, move |_, w, cx| {
                                up(
                                    Arc::new(move |s| {
                                        s.recording.audio.generate_waveform_after_record = !val
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child("Build clip waveforms after recording stops"),
                        ),
                ))
                .child(settings_daw_row(
                    "Save Before Record",
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child({
                            let val = schema.recording.audio.save_before_recording;
                            let up = on_update.clone();
                            fb_checkbox("rec-save-before-recording", val, move |_, w, cx| {
                                up(
                                    Arc::new(move |s| {
                                        s.recording.audio.save_before_recording = !val
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child("Save dirty projects before recording starts"),
                        ),
                ))
                .child(settings_daw_row(
                    "Recording Offset",
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child({
                            let val = schema.recording.audio.recording_offset_ms;
                            let up = on_update.clone();
                            fb_stepper_button("rec-offset-dec", "-", move |_, w, cx| {
                                up(
                                    Arc::new(move |s| {
                                        s.recording.audio.recording_offset_ms =
                                            (val - 1).clamp(-2000, 2000)
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .w(px(64.0))
                                .h(px(28.0))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .border(px(1.0))
                                .border_color(Colors::border_subtle())
                                .bg(Colors::surface_input())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(11.0))
                                .text_color(Colors::text_primary())
                                .child(format!(
                                    "{} ms",
                                    schema.recording.audio.recording_offset_ms
                                )),
                        )
                        .child({
                            let val = schema.recording.audio.recording_offset_ms;
                            let up = on_update.clone();
                            fb_stepper_button("rec-offset-inc", "+", move |_, w, cx| {
                                up(
                                    Arc::new(move |s| {
                                        s.recording.audio.recording_offset_ms =
                                            (val + 1).clamp(-2000, 2000)
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child("clip start"),
                        ),
                ))
                .child(settings_daw_row(
                    "Input Test",
                    input_test_meter_row(input_test, callbacks),
                ))
                .into_any_element(),
        );
    }

    // Metronome: click volume, sound, and record count-in.
    if (state.active_tab == SettingsTab::Metronome && query.is_empty())
        || (!query.is_empty()
            && is_match(
                "Metronome Click",
                &[
                    "metronome",
                    "click",
                    "sound",
                    "volume",
                    "count-in",
                    "countin",
                ],
            ))
    {
        let up = callbacks.on_update_setting.clone();
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.metronome",
                    assets::ICON_METRONOME_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.enable-click"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child({
                            let val = schema.recording.metronome.enabled;
                            let up_met = up.clone();
                            fb_checkbox("rec-metronome-enabled", val, move |_, w, cx| {
                                up_met(
                                    Arc::new(move |s| s.recording.metronome.enabled = !val),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(i18n.tr("settings.metronome.enable")),
                        ),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.click-volume"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(slider(
                            "metronome-volume-slider",
                            schema.recording.metronome.volume,
                            Colors::accent_primary(),
                            {
                                let up_vol = up.clone();
                                move |val, w, cx| {
                                    let volume = *val;
                                    up_vol(
                                        Arc::new(move |s| s.recording.metronome.volume = volume),
                                        w,
                                        cx,
                                    );
                                }
                            },
                        ))
                        .child(
                            div()
                                .w(px(32.0))
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(format!(
                                    "{:.0}%",
                                    schema.recording.metronome.volume * 100.0
                                )),
                        ),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.click-sound"),
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .child({
                            let val = schema.recording.metronome.sound_type.clone();
                            let up_snd = up.clone();
                            fb_segmented_button(
                                "met-sound-wood",
                                i18n.tr("settings.metronome.woodblock"),
                                val == "Woodblock",
                                move |_, w, cx| {
                                    up_snd(
                                        Arc::new(|s| {
                                            s.recording.metronome.sound_type =
                                                "Woodblock".to_string()
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        })
                        .child({
                            let val = schema.recording.metronome.sound_type.clone();
                            let up_snd = up.clone();
                            fb_segmented_button(
                                "met-sound-beep",
                                i18n.tr("settings.metronome.beep"),
                                val == "Beep",
                                move |_, w, cx| {
                                    up_snd(
                                        Arc::new(|s| {
                                            s.recording.metronome.sound_type = "Beep".to_string()
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        }),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.enable-count-in"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child({
                            let val = schema.recording.metronome.count_in_enabled;
                            let up_ci = up.clone();
                            fb_checkbox("met-count-in-enabled", val, move |_, w, cx| {
                                up_ci(
                                    Arc::new(move |s| {
                                        s.recording.metronome.count_in_enabled = !val
                                    }),
                                    w,
                                    cx,
                                );
                            })
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(i18n.tr("settings.metronome.count-in")),
                        ),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.count-in-bars"),
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .children((1u32..=4).map(|bars| {
                            let val = schema.recording.metronome.count_in_bars;
                            let up_bars = up.clone();
                            fb_segmented_button(
                                ("met-count-in-bars", bars as usize),
                                format!("{bars}"),
                                val == bars,
                                move |_, w, cx| {
                                    up_bars(
                                        Arc::new(move |s| {
                                            s.recording.metronome.count_in_bars = bars
                                        }),
                                        w,
                                        cx,
                                    );
                                },
                            )
                        })),
                ))
                .into_any_element(),
        );
    }

    // Playback Panel (Transport options)
    if (state.active_tab == SettingsTab::Playback && query.is_empty())
        || (!query.is_empty()
            && (is_match(
                "Transport Playback",
                &["spacebar", "transport", "stop", "start"],
            ) || is_match(
                "Latency Compensation",
                &["latency", "pdc", "delay", "compensation", "plugin"],
            )))
    {
        let on_update = callbacks.on_update_setting.clone();
        let pdc_enabled = schema.playback.latency_compensation;
        sections.push(
            settings_section_card()
                .child(settings_i18n_header(
                    i18n,
                    "settings.section.playback-latency",
                    assets::ICON_CLOCK_PATH,
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.latency-compensation"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(fb_checkbox(
                            "playback-pdc-enabled",
                            pdc_enabled,
                            move |_, w, cx| {
                                (on_update.clone())(
                                    Arc::new(|schema| {
                                        schema.playback.latency_compensation =
                                            !schema.playback.latency_compensation;
                                    }),
                                    w,
                                    cx,
                                );
                            },
                        ))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(i18n.tr("settings.latency.pdc-toggle-hint")),
                        ),
                ))
                .child(settings_section_hint(i18n.tr("settings.latency.pdc-hint")))
                .child({
                    use crate::settings::DropoutProtectionMode as Dp;
                    let mode = schema.playback.dropout_protection;
                    let ou = callbacks.on_update_setting.clone();
                    settings_daw_row(
                        "Dropout Protection",
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .child({
                                let ou = ou.clone();
                                fb_segmented_button(
                                    "dropout-off",
                                    "Off",
                                    mode == Dp::Off,
                                    move |_, w, cx| {
                                        ou(
                                            Arc::new(|s: &mut crate::settings::SettingsSchema| {
                                                s.playback.dropout_protection = Dp::Off;
                                            }),
                                            w,
                                            cx,
                                        );
                                    },
                                )
                            })
                            .child({
                                let ou = ou.clone();
                                fb_segmented_button(
                                    "dropout-light",
                                    "Light",
                                    mode == Dp::Light,
                                    move |_, w, cx| {
                                        ou(
                                            Arc::new(|s: &mut crate::settings::SettingsSchema| {
                                                s.playback.dropout_protection = Dp::Light;
                                            }),
                                            w,
                                            cx,
                                        );
                                    },
                                )
                            })
                            .child({
                                let ou = ou.clone();
                                fb_segmented_button(
                                    "dropout-medium",
                                    "Medium",
                                    mode == Dp::Medium,
                                    move |_, w, cx| {
                                        ou(
                                            Arc::new(|s: &mut crate::settings::SettingsSchema| {
                                                s.playback.dropout_protection = Dp::Medium;
                                            }),
                                            w,
                                            cx,
                                        );
                                    },
                                )
                            })
                            .child({
                                let ou = ou.clone();
                                fb_segmented_button(
                                    "dropout-high",
                                    "High",
                                    mode == Dp::High,
                                    move |_, w, cx| {
                                        ou(
                                            Arc::new(|s: &mut crate::settings::SettingsSchema| {
                                                s.playback.dropout_protection = Dp::High;
                                            }),
                                            w,
                                            cx,
                                        );
                                    },
                                )
                            }),
                    )
                })
                .child(settings_section_hint(
                    "Keeps internal headroom against UI / plugin jitter at the same device buffer. Medium is recommended; Off is lowest latency.",
                ))
                .into_any_element(),
        );

        {
            use crate::settings::SpacebarAction;
            let spacebar = schema.playback.spacebar_action;
            let return_on_stop = schema.playback.return_playhead_on_stop;
            let on_update = callbacks.on_update_setting.clone();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(settings_i18n_header(
                        i18n,
                        "settings.section.playback-transport",
                        assets::ICON_PLAY_PATH,
                    ))
                    .child(settings_daw_row(
                        i18n.tr("settings.field.spacebar-action"),
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .child({
                                let on_update = on_update.clone();
                                fb_segmented_button(
                                    "space-play-pause",
                                    i18n.tr("settings.spacebar.play-pause"),
                                    spacebar == SpacebarAction::PlayPause,
                                    move |_, w, cx| {
                                        on_update(
                                            Arc::new(|s: &mut crate::settings::SettingsSchema| {
                                                s.playback.spacebar_action =
                                                    SpacebarAction::PlayPause;
                                            }),
                                            w,
                                            cx,
                                        );
                                    },
                                )
                            })
                            .child({
                                let on_update = on_update.clone();
                                fb_segmented_button(
                                    "space-play-stop",
                                    i18n.tr("settings.spacebar.play-stop-soon"),
                                    spacebar == SpacebarAction::PlayStop,
                                    move |_, w, cx| {
                                        on_update(
                                            Arc::new(|s: &mut crate::settings::SettingsSchema| {
                                                s.playback.spacebar_action =
                                                    SpacebarAction::PlayStop;
                                            }),
                                            w,
                                            cx,
                                        );
                                    },
                                )
                            }),
                    ))
                    .child(settings_daw_row(
                        i18n.tr("settings.field.return-to-start"),
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child({
                                let on_update = on_update.clone();
                                fb_checkbox("return-on-stop", return_on_stop, move |_, w, cx| {
                                    on_update(
                                        Arc::new(|s: &mut crate::settings::SettingsSchema| {
                                            s.playback.return_playhead_on_stop =
                                                !s.playback.return_playhead_on_stop;
                                        }),
                                        w,
                                        cx,
                                    );
                                })
                            })
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(Colors::text_muted())
                                    .child(i18n.tr("settings.return-on-stop")),
                            ),
                    ))
                    .into_any_element(),
            );
        }
    }

    // Performance Panel — Renderer + GPU Device selection.
    if (state.active_tab == SettingsTab::Performance && query.is_empty())
        || (!query.is_empty()
            && (is_match("Renderer", &["renderer", "gpu", "cpu", "wgpu"])
                || is_match("GPU Device", &["gpu", "device", "adapter"])))
    {
        sections.push(
            performance_section(
                schema,
                callbacks.open_hardware_combo,
                callbacks.on_toggle_hardware_combo.clone(),
                callbacks.on_update_setting.clone(),
            )
            .into_any_element(),
        );
    }

    // Plugins Panel (vst directories list etc.)
    if (state.active_tab == SettingsTab::Plugins && query.is_empty())
        || (!query.is_empty()
            && (is_match("VST3 CLAP Formats", &["vst3", "clap", "plugins"])
                || is_match("Paths Directories", &["paths", "directories", "folders"])))
    {
        sections.push(
            plugins_section(
                schema,
                callbacks.on_update_setting.clone(),
                callbacks.on_open_plugin_manager.clone(),
            )
            .into_any_element(),
        );
    }

    // Files & Media Panel
    if (state.active_tab == SettingsTab::FilesMedia && query.is_empty())
        || (!query.is_empty()
            && (is_match("Projects", &["project", "folder", "path"])
                || is_match("Samples", &["sample", "media"])
                || is_match("Cache", &["cache", "recording"])))
    {
        sections.push(files_media_section().into_any_element());
    }

    // Advanced Panel
    if (state.active_tab == SettingsTab::Advanced && query.is_empty())
        || (!query.is_empty()
            && is_match(
                "Advanced Discord RPC",
                &["experimental", "developer", "engine", "discord", "presence"],
            ))
    {
        sections
            .push(advanced_section(schema, callbacks.on_update_setting.clone()).into_any_element());
    }

    // Shortcuts Panel — entry point into the real keyboard-shortcuts editor.
    if (state.active_tab == SettingsTab::Shortcuts && query.is_empty())
        || (!query.is_empty()
            && is_match(
                "Keyboard Shortcuts",
                &["shortcuts", "keymap", "hotkeys", "keybindings"],
            ))
    {
        sections.push(shortcuts_section(callbacks).into_any_element());
    }

    // About Panel
    if (state.active_tab == SettingsTab::About && query.is_empty())
        || (!query.is_empty() && (is_match("Version About", &["version", "credits", "about"])))
    {
        sections.push(about_section(crate::edition::current_edition_info()).into_any_element());
    }

    if sections.is_empty() {
        sections.push(
            div()
                .px(px(12.0))
                .py(px(24.0))
                .text_align(gpui::TextAlign::Center)
                .text_size(px(11.0))
                .text_color(Colors::text_faint())
                .child(if query.is_empty() {
                    format!(
                        "The {} panel is not fully wired in Native yet.",
                        i18n.tr(state.active_tab.label_key())
                    )
                } else {
                    format!("No settings match \"{}\"", query)
                })
                .into_any_element(),
        );
    }
    (sidebar_items, sections)
}

pub fn settings_dialog(
    state: &SettingsDialogState,
    schema: &SettingsSchema,
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    callbacks: SettingsDialogCallbacks,
    latency: &SettingsAudioLatencySnapshot,
    available_inputs: &[String],
    available_outputs: &[String],
    available_backends: &[String],
) -> impl IntoElement {
    let i18n = I18n::new(&schema.general.language);
    let close_backdrop = callbacks.on_close.clone();
    let close_button = callbacks.on_close.clone();

    let (sidebar_items, sections) = build_settings_content(
        state,
        schema,
        &callbacks,
        latency,
        &InputTestMeterState::default(),
        available_inputs,
        available_outputs,
        available_backends,
        // Channel lists are only populated for the live SettingsWindow path;
        // this legacy embedded dialog has no device-channel source.
        &[],
        &[],
    );

    // Overlay shell
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(56.0))
        .px(px(18.0))
        .pb(px(32.0))
        .id("settings-modal-overlay")
        .bg(gpui::transparent_black())
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            close_backdrop(&(), window, cx);
        })
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(640.0))
                .max_w(px(640.0))
                .h(px(520.0))
                .max_h(px(520.0))
                .overflow_hidden()
                .rounded(px(crate::theme::radius::DIALOG))
                .border(px(1.0))
                .border_color(Colors::border_default())
                .bg(Colors::surface_window())
                .shadow(vec![gpui::BoxShadow {
                    color: Colors::surface_overlay().into(),
                    offset: gpui::point(px(0.0), px(16.0)),
                    blur_radius: px(40.0),
                    spread_radius: px(0.0),
                    inset: false,
                }])
                .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                    cx.stop_propagation();
                })
                // Title Bar — matches project wizard style
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .h(px(32.0))
                        .pl(px(12.0))
                        .border_b(px(1.0))
                        .border_color(Colors::border_subtle())
                        .bg(Colors::surface_titlebar())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .h_full()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(i18n.tr("settings.title")),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .w(px(32.0))
                                .h(px(32.0))
                                .id("settings-close")
                                .cursor(gpui::CursorStyle::PointingHand)
                                .hover(|s| s.bg(Colors::surface_control_hover()))
                                .on_click(move |_, window, cx| close_button(&(), window, cx))
                                .child(icon(assets::ICON_X_PATH, 12.0, Colors::text_faint())),
                        ),
                )
                // Two-column layout
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h_0()
                        // Left Sidebar: Search + Tabs List
                        .child(
                            div()
                                .id("settings-sidebar-scroll")
                                .w(px(160.0))
                                .flex_shrink_0()
                                .border_r(px(1.0))
                                .border_color(Colors::divider())
                                .bg(Colors::surface_panel_alt())
                                .overflow_y_scroll()
                                .p(px(8.0))
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(div().pb(px(4.0)).child(text_field_with_callbacks(
                                    search_input,
                                    search_focused,
                                    search_callbacks,
                                )))
                                .children(sidebar_items),
                        )
                        // Right Content Panel
                        .child(
                            div()
                                .id("settings-content-scroll")
                                .flex_1()
                                .bg(Colors::surface_panel())
                                .overflow_y_scroll()
                                .p(px(16.0))
                                .flex()
                                .flex_col()
                                .gap(px(16.0))
                                .children(sections),
                        ),
                ),
        )
}

const SETTINGS_WIDTH: f32 = SETTINGS_WINDOW_WIDTH;
const SETTINGS_HEIGHT: f32 = SETTINGS_WINDOW_HEIGHT;
const COMBO_MENU_ESTIMATE_HEIGHT: f32 = 148.0;
const CLOCK_SOURCE_OPTIONS: &[&str] = &["Internal", "MIDI"];
const AUTOSAVE_INTERVAL_OPTIONS: &[u32] = &[1, 2, 3, 5, 10, 15, 30, 60];
const AUTOSAVE_MAX_BACKUPS_OPTIONS: &[u32] = &[1, 2, 3, 5, 10, 20, 50, 99];
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
    use super::{concise_driver_status, DRIVER_STATUS_SUMMARY_MAX};

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
