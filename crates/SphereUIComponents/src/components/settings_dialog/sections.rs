//! Split out of `settings_dialog.rs` (god-file decomposition). `use super::*`.

use super::*;

pub(crate) fn icon(path: &'static str, size: f32, color: gpui::Rgba) -> impl IntoElement {
    svg().path(path).w(px(size)).h(px(size)).text_color(color)
}

pub(crate) fn reveal_path_os(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("\"{}\"", path.display()))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

pub(crate) fn settings_path_list(paths: &[String]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .children(paths.iter().enumerate().map(|(idx, path)| {
            let path_string = path.clone();
            div()
                .id(("settings-path-row", idx))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .h(px(30.0))
                        .px(px(9.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .border(px(1.0))
                        .border_color(Colors::border_subtle())
                        .bg(Colors::surface_input())
                        .flex()
                        .items_center()
                        .truncate()
                        .text_size(px(theme::typography::DENSE_LABEL))
                        .text_color(Colors::text_secondary())
                        .child(path_string.clone()),
                )
                .child(fb_button(
                    ("settings-path-reveal", idx),
                    "Reveal",
                    FbButtonKind::Default,
                    true,
                    move |_, _w, _cx| reveal_path_os(std::path::Path::new(&path_string)),
                ))
        }))
}

pub(crate) fn hardware_select(
    combo: HardwareCombo,
    trigger_id: &'static str,
    selected: &str,
    open_combo: Option<HardwareCombo>,
    on_toggle: Arc<dyn Fn(HardwareCombo, Option<OverlayAnchor>, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let open = open_combo == Some(combo);
    let toggle = on_toggle.clone();
    div().w_full().child(combo_box_trigger(
        trigger_id,
        selected.to_string(),
        open,
        move |event, window, cx| {
            let layout = settings_form_column(window);
            let bounds = form_combo_trigger_bounds(layout, event, COMBO_TRIGGER_HEIGHT);
            let anchor = if open {
                None
            } else {
                Some(OverlayAnchor { bounds })
            };
            toggle(combo, anchor, window, cx);
        },
    ))
}

pub fn fb_checkbox(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(12.0))
        .h(px(12.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_default())
        .bg(if checked {
            Colors::accent_primary()
        } else {
            Colors::surface_input()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(on_click)
        .children(if checked {
            Some(
                svg()
                    .path(assets::ICON_CHECK_PATH)
                    .w(px(8.0))
                    .h(px(8.0))
                    .text_color(Colors::text_inverse()),
            )
        } else {
            None
        })
}

pub(crate) fn settings_labeled_checkbox(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    label: String,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(fb_checkbox(id, checked, on_click))
        .child(
            div()
                .text_size(px(theme::typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(label),
        )
}

pub(crate) fn settings_header(title: &'static str, _icon_path: &'static str) -> impl IntoElement {
    settings_section_title(title)
}

pub(crate) fn settings_i18n_header(
    i18n: I18n,
    key: &str,
    _icon_path: &'static str,
) -> impl IntoElement {
    settings_section_title(i18n.tr(key))
}

pub(crate) fn locale_label(i18n: I18n, locale: Locale) -> String {
    i18n.tr(locale.language_key())
}

pub(crate) fn selected_locale_label(i18n: I18n, language_code: &str) -> String {
    locale_label(i18n, Locale::from_code(language_code))
}

pub(crate) fn plugins_section(
    schema: &SettingsSchema,
    on_update: Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
    on_open_plugin_manager: Option<OnOpenPluginManager>,
) -> impl IntoElement {
    let vst3_enabled = schema.plugins.vst3.enabled;
    let clap_enabled = schema.plugins.clap.enabled;
    let background_scan = schema.plugins.scan.background_scan;
    let failed_count = schema.plugins.scan.failed_plugins.len();

    settings_section("Plugins")
        .child(settings_section_hint_text(
            "Manage native plugin formats, scan behavior, and watched plugin folders.",
        ))
        .child(settings_row(
            "Enable VST3",
            settings_toggle("settings-vst3-enabled", vst3_enabled, {
                let on_update = on_update.clone();
                move |_, w, cx| {
                    on_update(
                        Arc::new(move |s| s.plugins.vst3.enabled = !vst3_enabled),
                        w,
                        cx,
                    );
                }
            }),
        ))
        .child(settings_row(
            "Enable CLAP",
            settings_toggle("settings-clap-enabled", clap_enabled, {
                let on_update = on_update.clone();
                move |_, w, cx| {
                    on_update(
                        Arc::new(move |s| s.plugins.clap.enabled = !clap_enabled),
                        w,
                        cx,
                    );
                }
            }),
        ))
        .child(settings_row(
            "Background Scan",
            settings_toggle("settings-plugin-background-scan", background_scan, {
                let on_update = on_update.clone();
                move |_, w, cx| {
                    on_update(
                        Arc::new(move |s| s.plugins.scan.background_scan = !background_scan),
                        w,
                        cx,
                    );
                }
            }),
        ))
        .child(settings_row(
            "VST3 Folders",
            settings_path_list(&schema.plugins.vst3.paths),
        ))
        .child(settings_row(
            "CLAP Folders",
            settings_path_list(&schema.plugins.clap.paths),
        ))
        .child(settings_row("Scan / Rescan", {
            let open = on_open_plugin_manager.clone();
            fb_button(
                "settings-open-plugin-manager",
                "Open Plug-in Manager…",
                FbButtonKind::Primary,
                open.is_some(),
                move |_, window, cx| {
                    if let Some(open) = open.as_ref() {
                        open(window, cx);
                    }
                },
            )
        }))
        .child(settings_row(
            "Scan Status",
            settings_readout(format!("{failed_count} failed or ignored")),
        ))
}

pub(crate) fn files_media_section() -> impl IntoElement {
    settings_section("Files & Media")
        .child(settings_section_hint_text(
            "Project folders, sample libraries, recording paths, and media cache settings.",
        ))
        .child(settings_row(
            "Project Folder",
            settings_readout("Use project location"),
        ))
        .child(settings_row(
            "Recording Path",
            settings_readout("Project Media/"),
        ))
        .child(settings_row(
            "Sample Libraries",
            settings_readout("Not configured"),
        ))
        .child(settings_row(
            "Media Cache",
            settings_readout("Project Cache/"),
        ))
}

/// Real keyboard-shortcuts entry point. The editor itself (search, per-command
/// rebinding, conflict detection, reset, persistence) already exists as
/// `KeymapWindow` — opened via the same path as the Help menu's
/// `help:keyboard-shortcuts` command. This section just gives Settings a
/// button into that window instead of duplicating it.
pub(crate) fn shortcuts_section(callbacks: &SettingsDialogCallbacks) -> impl IntoElement {
    let open = callbacks.on_open_keyboard_shortcuts.clone();
    settings_section("Keyboard Shortcuts")
        .child(settings_section_hint_text(
            "Search, edit, and reset keyboard commands grouped by workflow area.",
        ))
        .child(div().pt(px(4.0)).child(fb_button(
            "settings-open-keyboard-shortcuts",
            "Open Keyboard Shortcuts…",
            FbButtonKind::Primary,
            open.is_some(),
            move |_, window, cx| {
                if let Some(open) = open.as_ref() {
                    open(window, cx);
                }
            },
        )))
}

pub(crate) fn advanced_section(
    schema: &SettingsSchema,
    on_update: Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let discord_rpc_enabled = schema.general.discord_rpc_enabled;
    let update_discord_rpc = on_update.clone();
    settings_section("Advanced")
        .child(settings_section_hint_text(
            "Experimental features, developer tools, and low-level engine options.",
        ))
        .child(settings_row(
            "Experimental Flags",
            settings_readout("Default"),
        ))
        .child(settings_row(
            "Developer Logging",
            settings_readout("Environment controlled"),
        ))
        .child(settings_row(
            "Discord RPC",
            settings_toggle(
                "settings-discord-rpc-toggle",
                discord_rpc_enabled,
                move |_, window, cx| {
                    update_discord_rpc(
                        Arc::new(move |settings| {
                            settings.general.discord_rpc_enabled = !discord_rpc_enabled;
                        }),
                        window,
                        cx,
                    );
                },
            ),
        ))
        .child(settings_row(
            "Audio Engine",
            settings_readout("Sphere Direct Audio"),
        ))
}

/// About panel. `edition` is `Some` only on builds that install an edition
/// provider (Professional Edition); Community builds pass `None` and see the plain
/// panel. When present, the license rows re-read verified state each render, so
/// activation — or a background renewal on a later launch — shows up here
/// without extra refresh wiring.
pub(crate) fn about_section(edition: Option<crate::edition::EditionInfo>) -> impl IntoElement {
    let version = crate::edition::app_version();
    let edition_name = edition
        .as_ref()
        .map(|info| info.edition)
        .unwrap_or("Community");

    let mut section = settings_section("Futureboard Studio")
        .child(settings_section_hint_text(format!(
            "Futureboard Studio / Mochi DAW v{version}"
        )))
        .child(settings_row("Edition", settings_readout(edition_name)))
        .child(settings_row("Runtime", settings_readout("GPUI + Rust")))
        .child(settings_row(
            "Plugin Host",
            settings_readout("VST3 / CLAP scaffold"),
        ));

    if let Some(info) = edition.as_ref() {
        // Named only while the licensed engine is registered (see the
        // Professional provider), so this row never claims an unavailable backend.
        if let Some(engine) = info.audio_engine.as_ref() {
            section = section.child(settings_row("Audio Engine", settings_readout(engine.name)));
        }
        section = section.child(license_status_row(info.license.as_ref()));
        if let Some(license) = info.license.as_ref() {
            if let Some(licensee) = license
                .licensee
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                section = section.child(settings_row(
                    "Licensed To",
                    settings_readout(licensee.to_string()),
                ));
            }
            section = section.child(settings_row(
                "Expires",
                settings_readout(license.expiry_text()),
            ));
            if !license.entitlements.is_empty() {
                section = section.child(settings_row(
                    "Entitlements",
                    settings_readout(license.entitlements.join(", ")),
                ));
            }
        }

        // Only offered where activation can actually open. The label follows the
        // state: there is nothing to "manage" on a machine holding no license.
        if crate::edition::license_action_available() {
            let action_label = if info.license.is_some() {
                "Manage License…"
            } else {
                "Activate License…"
            };
            section = section.child(settings_row(
                "Activation",
                fb_button(
                    "settings-license-activate",
                    action_label,
                    FbButtonKind::Default,
                    true,
                    |_, window, cx| crate::edition::dispatch_license_action(window, cx),
                ),
            ));
        }
    }

    section.child(settings_row(
        "Copyright",
        settings_readout("© 2026 Futureboard Studio team"),
    ))
}

/// License row: a status badge that reads Active (accent) or Expired / Not
/// activated (muted). The label comes from [`crate::edition`] so this panel and
/// the activation dialog cannot disagree about the same license.
fn license_status_row(license: Option<&crate::edition::LicenseDisplay>) -> impl IntoElement {
    let (label, ok) = crate::edition::license_status_label(license);
    settings_row("License", settings_status_badge(label, ok))
}

/// Performance > Rendering section. Renderer and GPU Device choices are
/// "restart required" — applied at next launch by `WgpuTimelineRenderer`
/// construction. We deliberately don't hot-swap the renderer at runtime
/// to avoid mid-session GPU device churn.
pub(crate) fn performance_section(
    schema: &SettingsSchema,
    open_combo: Option<HardwareCombo>,
    on_toggle: Arc<dyn Fn(HardwareCombo, Option<OverlayAnchor>, &mut Window, &mut App) + 'static>,
    on_update: Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let render_mode = schema.performance.render_mode;
    let gpu_pref = schema.performance.gpu_device.clone();
    // Enumerate once for label/status; the dropdown re-enumerates on open
    // to stay current with hot-pluggable eGPUs / driver changes.
    let detected = list_available_gpu_devices();
    let detected_count = detected.len();
    let enumeration_failed_unexpectedly = false; // catch_unwind path inside list_available_gpu_devices already returns Vec::new on panic; treat empty as "no GPU" rather than failure.

    let renderer_label = render_mode.label().to_string();
    let renderer_row = hardware_select(
        HardwareCombo::Renderer,
        "settings-performance-renderer-trigger",
        &renderer_label,
        open_combo,
        on_toggle.clone(),
    );

    let gpu_device_label = match &gpu_pref {
        GpuDevicePreference::Auto => "Auto".to_string(),
        GpuDevicePreference::DeviceId(id) => detected
            .iter()
            .find(|d| &d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "Auto".to_string()),
    };
    let gpu_device_row = hardware_select(
        HardwareCombo::GpuDevice,
        "settings-performance-gpu-device-trigger",
        &gpu_device_label,
        open_combo,
        on_toggle.clone(),
    );

    // Frame pacing mode. Applies live (no restart) — the scheduler re-reads it
    // each frame and republishes the poll cadence.
    let frame_rate_label = schema.performance.frame_rate.label().to_string();
    let frame_rate_row = hardware_select(
        HardwareCombo::FrameRate,
        "settings-performance-frame-rate-trigger",
        &frame_rate_label,
        open_combo,
        on_toggle,
    );

    let (status_text, status_color) = match (render_mode, detected_count) {
        (RenderMode::Auto, _) => (
            "Auto — accelerated GPUI paint; experimental GPU layers stay off until verified."
                .to_string(),
            Colors::text_secondary(),
        ),
        (RenderMode::CpuRender, _) => (
            "CPU Render active (GPUI paint fallback).".to_string(),
            Colors::text_secondary(),
        ),
        (RenderMode::GpuAcceleration, 0) => (
            "No GPU adapter detected. CPU Render fallback will be used.".to_string(),
            Colors::status_warning(),
        ),
        (RenderMode::GpuAcceleration, n) => (
            format!("GPU (Experimental) — mixer primitive layer on; {n} adapter(s) detected."),
            Colors::status_success(),
        ),
    };

    let mut card = settings_section("Rendering")
        .child(settings_section_hint(
            "Choose how the timeline is drawn. GPU Acceleration uses WGPU when available; CPU Render forces the GPUI paint fallback (best compatibility).",
        ))
        .child(settings_row_restart("Renderer", true, renderer_row))
        .child(settings_row_restart("GPU Device", true, gpu_device_row))
        .child(settings_daw_row("Frame Rate", frame_rate_row))
        .child(settings_section_hint(
            "Display Sync tracks your monitor refresh rate. Fixed caps and Battery Saver are for debugging or saving power; idle frames are always drawn on demand.",
        ))
        .child(settings_daw_row(
            "Status",
            div()
                .text_size(px(10.5))
                .text_color(status_color)
                .child(status_text),
        ));

    if enumeration_failed_unexpectedly {
        card = card.child(
            div()
                .pt(px(4.0))
                .text_size(px(10.0))
                .text_color(Colors::status_warning())
                .child("GPU enumeration failed. CPU Render fallback is available."),
        );
    }

    let show_status_perf = schema.performance.show_status_performance_metrics;
    let show_perf_overlay = schema.performance.show_performance_overlay;

    card.child(settings_restart_footer())
        .child(settings_section("Developer"))
        .child(settings_section_hint(
            "Optional diagnostics for profiling. Hidden by default in the status bar.",
        ))
        .child(settings_row(
            "Status Performance Metrics",
            settings_toggle("settings-show-status-perf", show_status_perf, {
                let on_update = on_update.clone();
                move |_, w, cx| {
                    on_update(
                        Arc::new(move |s| {
                            s.performance.show_status_performance_metrics = !show_status_perf
                        }),
                        w,
                        cx,
                    );
                }
            }),
        ))
        .child(settings_row(
            "Performance Overlay",
            settings_toggle("settings-show-perf-overlay", show_perf_overlay, {
                let on_update = on_update.clone();
                move |_, w, cx| {
                    on_update(
                        Arc::new(move |s| {
                            s.performance.show_performance_overlay = !show_perf_overlay
                        }),
                        w,
                        cx,
                    );
                }
            }),
        ))
}

pub(crate) fn build_settings_sidebar_items(
    state: &SettingsDialogState,
    callbacks: &SettingsDialogCallbacks,
    i18n: I18n,
    query: &str,
    is_match: &dyn Fn(&str, &[&str]) -> bool,
) -> Vec<gpui::AnyElement> {
    let mut sidebar_items = Vec::new();
    let mut nav_index = 0usize;
    for (group_key, tabs) in SettingsTab::nav_groups() {
        let visible_tabs: Vec<SettingsTab> = tabs
            .iter()
            .copied()
            .filter(|tab| tab_matches_search(*tab, query, is_match))
            .collect();
        if visible_tabs.is_empty() {
            continue;
        }
        sidebar_items.push(settings_nav_group_header(i18n.tr(group_key)).into_any_element());
        for tab in visible_tabs {
            let active = state.active_tab == tab && query.is_empty();
            let search_hit = !query.is_empty();
            let cb = callbacks.on_select_tab.clone();
            let idx = nav_index;
            nav_index += 1;
            sidebar_items.push(
                settings_nav_item(
                    ("settings-tab", idx),
                    i18n.tr(tab.label_key()),
                    tab.icon(),
                    active,
                    search_hit,
                    move |window, cx| cb(&tab, window, cx),
                )
                .into_any_element(),
            );
        }
    }
    sidebar_items
}

pub(crate) fn tab_matches_search(
    tab: SettingsTab,
    query: &str,
    is_match: &dyn Fn(&str, &[&str]) -> bool,
) -> bool {
    if query.is_empty() {
        return true;
    }
    match tab {
        SettingsTab::General => {
            is_match("Language", &["language"])
                || is_match("Start screen", &["wizard", "start"])
                || is_match("Autosave", &["autosave", "backup"])
                || is_match("Tempo", &["tempo", "bpm"])
                || is_match("Sample Rate", &["sample", "rate", "hz"])
                || is_match("Buffer", &["buffer", "latency"])
        }
        SettingsTab::Audio => {
            is_match(
                "Audio Driver",
                &["driver", "wasapi", "wdm", "ks", "backend"],
            ) || is_match("Input Device", &["input", "microphone"])
                || is_match("Output Device", &["output", "speakers"])
                || is_match("Latency", &["latency", "pdc", "delay", "buffer"])
                || is_match("Buffer Size", &["buffer", "sample"])
        }
        SettingsTab::Midi => {
            is_match("MIDI", &["midi", "port", "keyboard"])
                || is_match("Clock", &["clock", "sync", "ltc"])
        }
        SettingsTab::Appearance => {
            is_match("Theme", &["theme"])
                || (cfg!(target_os = "windows")
                    && is_match(
                        "Text Rendering",
                        &["text", "font", "render", "directwrite", "gdi", "blurry"],
                    ))
                || is_match("UI Scale", &["scale"])
                || is_match("Grid", &["grid", "timeline"])
                || is_match("Mixer", &["mixer", "meter"])
        }
        SettingsTab::Editing => {
            is_match("Zoom", &["mouse", "zoom"])
                || is_match("Snap", &["snap", "grid"])
                || is_match("Undo", &["undo", "history"])
        }
        SettingsTab::Recording => is_match("Recording", &["record", "wav", "bit"]),
        SettingsTab::Metronome => {
            is_match("Metronome", &["metronome", "click", "volume", "count-in"])
        }
        SettingsTab::Playback => {
            is_match("Transport", &["transport", "play", "stop"])
                || is_match(
                    "Latency Compensation",
                    &["latency", "pdc", "delay", "compensation"],
                )
        }
        SettingsTab::Plugins => {
            is_match("VST3", &["vst3", "plugin"])
                || is_match("CLAP", &["clap"])
                || is_match("Scan", &["scan"])
        }
        SettingsTab::FilesMedia => {
            is_match("Projects", &["project", "folder", "path"])
                || is_match("Samples", &["sample", "media"])
        }
        SettingsTab::Shortcuts => is_match("Shortcut", &["key", "command"]),
        SettingsTab::Performance => {
            is_match("Renderer", &["renderer", "gpu", "cpu", "wgpu"])
                || is_match("GPU Device", &["gpu", "device", "adapter"])
                || is_match(
                    "Frame Rate",
                    &["frame", "fps", "refresh", "vsync", "display sync"],
                )
                || is_match("Performance", &["cpu", "engine"])
        }
        SettingsTab::Advanced => is_match("Advanced", &["experimental"]),
        SettingsTab::About => is_match("About", &["version"]),
    }
}

pub type AudioLatencySnapshotProvider = Arc<dyn Fn() -> SettingsAudioLatencySnapshot + Send + Sync>;

pub(crate) fn latency_ms_label(i18n: &I18n, ms: f64) -> String {
    if ms > 0.0 {
        i18n.tr_vars("settings.latency.ms-value", &[("ms", format!("{ms:.2}"))])
    } else {
        i18n.tr("settings.latency.unavailable")
    }
}

pub(crate) fn audio_latency_report_section(
    i18n: &I18n,
    latency: &SettingsAudioLatencySnapshot,
    pdc_setting_enabled: bool,
) -> impl IntoElement {
    let engine_ready = latency.engine_open;
    let pdc_label = if !pdc_setting_enabled {
        i18n.tr("settings.latency.pdc-disabled-setting")
    } else if latency.pdc_active {
        i18n.tr("settings.latency.pdc-active")
    } else {
        i18n.tr("settings.latency.pdc-off")
    };
    let pdc_ok = pdc_setting_enabled && latency.pdc_active;

    let mut card = settings_section_card().child(settings_i18n_header(
        *i18n,
        "settings.section.latency-report",
        assets::ICON_CLOCK_PATH,
    ));

    if !engine_ready {
        card = card.child(settings_section_hint(
            i18n.tr("settings.latency.engine-closed"),
        ));
    } else {
        let rate_mismatch = latency.requested_sample_rate > 0
            && latency.active_sample_rate > 0
            && latency.requested_sample_rate != latency.active_sample_rate;

        card = card
            .child(settings_daw_row(
                i18n.tr("settings.field.device-state"),
                settings_status_badge(
                    if latency.device_state.is_empty() {
                        i18n.tr("settings.driver-status.ready")
                    } else {
                        latency.device_state.clone()
                    },
                    latency.device_state != "DeviceLost",
                ),
            ))
            .child(settings_daw_row(
                i18n.tr("settings.field.active-sample-rate"),
                settings_value_readout(i18n.tr_vars(
                    "settings.latency.sample-rate-value",
                    &[("hz", latency.active_sample_rate.max(1).to_string())],
                )),
            ));

        // Deferred preferred-rate change ("Later"): the user picked a new sample
        // rate but chose not to restart the engine yet. Timing keeps using the
        // active rate until the project is re-opened / engine restarted.
        if latency.restart_pending {
            card = card
                .child(settings_daw_row(
                    i18n.tr("settings.field.preferred-sample-rate"),
                    settings_value_readout(i18n.tr_vars(
                        "settings.latency.sample-rate-value",
                        &[("hz", latency.deferred_sample_rate.max(1).to_string())],
                    )),
                ))
                .child(
                    div()
                        .pt(px(4.0))
                        .text_size(px(11.0))
                        .text_color(Colors::status_warning())
                        .child(i18n.tr("settings.latency.sample-rate-restart-pending")),
                );
        } else if rate_mismatch {
            // Requested vs active divergence (shared-mode resample / professional
            // fallback). Allowed but must be visible — timing follows the active
            // device rate, not the requested rate shown elsewhere in Preferences.
            card = card
                .child(settings_daw_row(
                    i18n.tr("settings.field.requested-sample-rate"),
                    settings_value_readout(i18n.tr_vars(
                        "settings.latency.sample-rate-value",
                        &[("hz", latency.requested_sample_rate.to_string())],
                    )),
                ))
                .child(
                    div()
                        .pt(px(4.0))
                        .text_size(px(11.0))
                        .text_color(Colors::status_warning())
                        .child(i18n.tr_vars(
                            "settings.latency.sample-rate-mismatch",
                            &[
                                ("active", latency.active_sample_rate.to_string()),
                                ("requested", latency.requested_sample_rate.to_string()),
                            ],
                        )),
                );
        }

        card = card
            .child(settings_daw_row(
                i18n.tr("settings.field.output-buffer-latency"),
                settings_value_readout(latency_ms_label(i18n, latency.buffer_ms)),
            ))
            // Input/output halves are shown only when the backend reports them
            // (ASIO); the estimate has no input half to show.
            .when(latency.round_trip_reported, |card| {
                card.child(settings_daw_row(
                    i18n.tr("settings.field.input-latency"),
                    settings_value_readout(latency_ms_label(i18n, latency.input_ms)),
                ))
                .child(settings_daw_row(
                    i18n.tr("settings.field.output-latency"),
                    settings_value_readout(latency_ms_label(i18n, latency.output_ms)),
                ))
            })
            .child(settings_daw_row(
                if latency.round_trip_reported {
                    i18n.tr("settings.field.round-trip-latency")
                } else {
                    i18n.tr("settings.field.round-trip-latency-estimated")
                },
                settings_value_readout(latency_ms_label(i18n, latency.round_trip_ms)),
            ))
            .child(settings_daw_row(
                i18n.tr("settings.field.plugin-path-latency"),
                settings_value_readout(latency_ms_label(i18n, latency.max_path_ms)),
            ))
            .child(settings_daw_row(
                i18n.tr("settings.field.master-plugin-latency"),
                settings_value_readout(latency_ms_label(i18n, latency.master_plugin_ms)),
            ))
            .child(settings_daw_row(
                i18n.tr("settings.field.pdc-status"),
                settings_status_badge(pdc_label, pdc_ok),
            ));

        if !latency.track_lines.is_empty() {
            card = card.child(div().mt(px(4.0)).flex().flex_col().gap(px(4.0)).children(
                latency.track_lines.iter().map(|line| {
                    settings_daw_row(
                        line.track_id.clone(),
                        settings_value_readout(i18n.tr_vars(
                            "settings.latency.track-summary",
                            &[
                                ("plugin", format!("{:.1}", line.plugin_ms)),
                                ("path", format!("{:.1}", line.path_ms)),
                                ("pdc", format!("{:.1}", line.pdc_ms)),
                            ],
                        )),
                    )
                }),
            ));
        }
    }

    card.child(settings_section_hint(
        i18n.tr("settings.latency.report-hint"),
    ))
}

pub(crate) fn midi_direction_label(i18n: &I18n, direction: MidiDeviceDirection) -> String {
    match direction {
        MidiDeviceDirection::Input => i18n.tr("settings.midi.type.input"),
        MidiDeviceDirection::Output => i18n.tr("settings.midi.type.output"),
        MidiDeviceDirection::InputOutput => i18n.tr("settings.midi.type.input-output"),
    }
}

pub(crate) fn midi_device_status_label(
    i18n: &I18n,
    device: &MidiDeviceSetting,
) -> (String, BoxListBadgeTone) {
    if !device.connected {
        (
            i18n.tr("settings.midi.status.missing"),
            BoxListBadgeTone::Warning,
        )
    } else if !device.enabled {
        (
            i18n.tr("settings.midi.status.disabled"),
            BoxListBadgeTone::Neutral,
        )
    } else {
        (
            i18n.tr("settings.midi.status.connected"),
            BoxListBadgeTone::Success,
        )
    }
}

pub(crate) fn midi_device_icon(direction: MidiDeviceDirection) -> &'static str {
    match direction {
        MidiDeviceDirection::Input => assets::ICON_MIC_PATH,
        MidiDeviceDirection::Output => assets::ICON_VOLUME_2_PATH,
        MidiDeviceDirection::InputOutput => assets::ICON_ROUTE_PATH,
    }
}

pub(crate) fn midi_device_list_row(
    row_index: usize,
    device: &MidiDeviceSetting,
    i18n: &I18n,
    on_update: &Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let snapshot = device.clone();
    let enabled = device.enabled;
    let up = on_update.clone();
    let (status_label, status_tone) = midi_device_status_label(i18n, device);
    let type_label = midi_direction_label(i18n, device.direction);
    let show_clock = device.clock_enabled && device.direction != MidiDeviceDirection::Input;

    box_list_item()
        .id(("midi-device-row", row_index))
        .child(box_list_item_leading_icon(midi_device_icon(
            device.direction,
        )))
        .child(
            box_list_item_content()
                .child(box_list_item_title(device.name.clone()))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .flex_wrap()
                        .child(box_list_item_badge(type_label, BoxListBadgeTone::Accent))
                        .child(box_list_item_badge(status_label, status_tone))
                        .when(show_clock, |row| {
                            row.child(box_list_item_badge(
                                i18n.tr("settings.midi.clock-badge"),
                                BoxListBadgeTone::Neutral,
                            ))
                        }),
                ),
        )
        .child(box_list_item_trailing().child(box_list_toggle(
            ("midi-device-toggle", row_index),
            enabled,
            move |_, w, cx| {
                let next = !enabled;
                if midi_settings_debug_enabled() {
                    eprintln!("[MIDI settings] toggle {} enabled={next}", snapshot.name);
                }
                let saved_for_update = snapshot.clone();
                up(
                    Arc::new(move |s| {
                        let mut updated = saved_for_update.clone();
                        updated.enabled = next;
                        upsert_midi_device(&mut s.hardware.midi, updated);
                    }),
                    w,
                    cx,
                );
            },
        )))
}

pub(crate) fn midi_device_group(
    title: String,
    devices: &[MidiDeviceSetting],
    row_offset: &mut usize,
    i18n: &I18n,
    on_update: &Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
) -> Option<gpui::AnyElement> {
    if devices.is_empty() {
        return None;
    }
    let rows: Vec<_> = devices
        .iter()
        .enumerate()
        .map(|(idx, device)| {
            let row_ix = *row_offset + idx;
            midi_device_list_row(row_ix, device, i18n, on_update).into_any_element()
        })
        .collect();
    *row_offset += devices.len();
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(box_list_group_label(title))
            .child(box_list_view().children(rows))
            .into_any_element(),
    )
}

pub(crate) fn midi_devices_section(
    schema: &SettingsSchema,
    i18n: &I18n,
    on_update: Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
    on_refresh_midi: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
) -> impl IntoElement {
    let detected = cached_midi_devices();
    let resolved = resolve_midi_devices(&schema.hardware.midi.devices, &detected);

    let inputs: Vec<_> = resolved
        .iter()
        .filter(|d| {
            d.direction == MidiDeviceDirection::Input
                || d.direction == MidiDeviceDirection::InputOutput
        })
        .cloned()
        .collect();
    let outputs: Vec<_> = resolved
        .iter()
        .filter(|d| {
            d.direction == MidiDeviceDirection::Output
                || d.direction == MidiDeviceDirection::InputOutput
        })
        .cloned()
        .collect();

    let mut row_offset = 0usize;
    let mut body = div().flex().flex_col().gap(px(10.0));

    if resolved.is_empty() {
        let refresh = on_refresh_midi.clone();
        body = body.child(box_list_empty_state(
            i18n.tr("settings.midi.empty"),
            i18n.tr("settings.midi.refresh"),
            move |_, w, cx| {
                if let Some(cb) = refresh.as_ref() {
                    cb(w, cx);
                }
            },
        ));
    } else {
        if let Some(group) = midi_device_group(
            i18n.tr("settings.section.midi-inputs"),
            &inputs,
            &mut row_offset,
            i18n,
            &on_update,
        ) {
            body = body.child(group);
        }
        if let Some(group) = midi_device_group(
            i18n.tr("settings.section.midi-outputs"),
            &outputs,
            &mut row_offset,
            i18n,
            &on_update,
        ) {
            body = body.child(group);
        }

        let clock_sync = schema.hardware.midi.clock_sync;
        let up_sync = on_update.clone();
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(box_list_group_label(
                    i18n.tr("settings.section.sync-outputs"),
                ))
                .child(
                    box_list_view().child(
                        box_list_item()
                            .id("midi-clock-sync-row")
                            .child(box_list_item_leading_icon(assets::ICON_CLOCK_PATH))
                            .child(
                                box_list_item_content()
                                    .child(box_list_item_title(
                                        i18n.tr("settings.midi.sync-clock-send"),
                                    ))
                                    .child(box_list_item_subtitle(
                                        i18n.tr("settings.midi.sync-clock-hint"),
                                    )),
                            )
                            .child(box_list_item_trailing().child(box_list_toggle(
                                "midi-clock-sync-toggle",
                                clock_sync,
                                move |_, w, cx| {
                                    let next = !clock_sync;
                                    if midi_settings_debug_enabled() {
                                        eprintln!("[MIDI settings] clock_sync={next}");
                                    }
                                    up_sync(
                                        Arc::new(move |s| s.hardware.midi.clock_sync = next),
                                        w,
                                        cx,
                                    );
                                },
                            ))),
                    ),
                ),
        );

        if let Some(refresh) = on_refresh_midi {
            let refresh_cb = refresh.clone();
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(box_list_icon_button(
                        "midi-devices-refresh",
                        assets::ICON_REPEAT_PATH,
                        "Refresh MIDI devices",
                        move |_, w, cx| refresh_cb(w, cx),
                    )),
            );
        }
    }

    settings_section_card()
        .child(settings_i18n_header(
            *i18n,
            "settings.section.midi-devices",
            assets::ICON_LINK_PATH,
        ))
        .child(body)
}

/// Read-only Input/Output Channels card for Preferences > Audio (roadmap Phase C):
/// the selected device plus the concrete channel routes derived from its channel
/// count (shared `audio_routing` builder). Reactive — reads the current schema
/// device each render.
pub(crate) fn audio_channel_section(
    title: &str,
    device_name: &str,
    options: &[crate::audio_routing::AudioRouteOption],
) -> gpui::AnyElement {
    let card = settings_section_card().child(settings_section_title(title.to_string()));
    if device_name.trim().is_empty() {
        return card
            .child(settings_section_hint("No device selected."))
            .into_any_element();
    }
    let card = card.child(settings_daw_row(
        "Device",
        settings_value_readout(device_name.to_string()),
    ));
    let card = if options.is_empty() {
        card.child(settings_section_hint(
            "No channels reported by this device.",
        ))
    } else {
        let summary = options
            .iter()
            .map(|o| o.label.clone())
            .collect::<Vec<_>>()
            .join("  ·  ");
        card.child(settings_daw_row(
            "Channels",
            settings_value_readout(summary),
        ))
    };
    card.into_any_element()
}

pub(crate) fn input_test_meter_row(
    state: &InputTestMeterState,
    callbacks: &SettingsDialogCallbacks,
) -> impl IntoElement {
    let level = state.level.clamp(0.0, 1.0);
    let level_percent = (level * 100.0).round() as u32;
    let toggle = callbacks.on_toggle_input_test.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(fb_button(
                    "settings-input-test-toggle",
                    if state.active {
                        "Stop Test"
                    } else {
                        "Test Input"
                    },
                    if state.active {
                        FbButtonKind::Primary
                    } else {
                        FbButtonKind::Default
                    },
                    true,
                    move |_, window, cx| toggle(&(), window, cx),
                ))
                .child(
                    div()
                        .flex_1()
                        .h(px(10.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .border(px(1.0))
                        .border_color(Colors::border_subtle())
                        .bg(Colors::meter_rail())
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(level))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .bg(if level >= 0.9 {
                                    Colors::meter_high()
                                } else if level >= 0.65 {
                                    Colors::meter_mid()
                                } else {
                                    Colors::meter_low()
                                }),
                        ),
                )
                .child(
                    div()
                        .w(px(38.0))
                        .text_size(px(10.0))
                        .text_color(Colors::text_muted())
                        .child(format!("{level_percent}%")),
                ),
        )
        .when_some(state.error.clone(), |el, error| {
            el.child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::status_error())
                    .child(error),
            )
        })
}
