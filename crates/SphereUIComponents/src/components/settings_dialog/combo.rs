//! Split out of `settings_dialog.rs` (god-file decomposition). `use super::*`.

use super::*;

pub(crate) fn combo_menu_position(
    anchor: OverlayAnchor,
    window: &Window,
) -> crate::overlay::OverlayPosition {
    let layout = settings_form_column(window);
    let refreshed = refresh_form_anchor(anchor, layout);
    let content_bounds = external_dialog_overlay_bounds(window);
    let scale = window.scale_factor();
    if crate::components::combo_box::combobox_debug_enabled() {
        eprintln!(
            "[combobox] settings_menu scale_factor={scale:.2} layout=({:.0},{:.0}) anchor={:?} content={:?}",
            layout.value_left, layout.value_width, refreshed.bounds, content_bounds
        );
    }
    compute_overlay_position(
        refreshed.bounds,
        OverlaySize {
            width: layout.value_width,
            height: COMBO_MENU_ESTIMATE_HEIGHT,
        },
        content_bounds,
        OverlayPlacement::BottomStart,
        4.0,
    )
}

pub(crate) fn hardware_combo_overlay(
    open_combo: HardwareCombo,
    anchor: OverlayAnchor,
    window: &Window,
    schema: &SettingsSchema,
    available_inputs: &[String],
    available_outputs: &[String],
    available_backends: &[String],
    on_update: Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
    close_target: Entity<SettingsWindow>,
) -> impl IntoElement {
    let i18n = I18n::new(&schema.general.language);
    let position = combo_menu_position(anchor, window);
    let close_target = close_target.clone();
    let menu = match open_combo {
        HardwareCombo::Theme => {
            let themes = crate::theme::available_theme_summaries();
            let selected = schema.appearance.theme.clone();
            let labels = themes
                .iter()
                .map(|(_, name)| name.clone())
                .collect::<Vec<_>>();
            let selected_label = themes
                .iter()
                .find(|(id, _)| id == &selected)
                .map(|(_, name)| name.clone())
                .unwrap_or(selected);
            let themes_for_selection = themes.clone();
            let up = on_update.clone();
            combo_box_string_menu(
                "settings-theme-menu",
                position,
                &selected_label,
                &labels,
                Arc::new(move |name, window, cx| {
                    let id = themes_for_selection
                        .iter()
                        .find(|(_, theme_name)| theme_name == &name)
                        .map(|(id, _)| id.clone())
                        .unwrap_or(name);
                    up(
                        Arc::new(move |s| s.appearance.theme = id.clone()),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::AudioDriver => {
            let selected =
                sanitized_backend_label(&schema.hardware.audio.driver_type, available_backends);
            let up = on_update.clone();
            combo_box_string_menu(
                "settings-audio-driver-menu",
                position,
                &selected,
                available_backends,
                Arc::new(move |value, window, cx| {
                    up(
                        Arc::new(move |s| {
                            if s.hardware.audio.driver_type != value {
                                s.hardware.audio.driver_type = value.clone();
                                s.hardware.audio.device_in.clear();
                                s.hardware.audio.device_out.clear();
                                s.hardware.audio.active_inputs.clear();
                                s.hardware.audio.active_outputs.clear();
                            }
                        }),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::InputDevice => {
            let selected = schema.hardware.audio.device_in.clone();
            let up = on_update.clone();
            combo_box_string_menu(
                "settings-audio-input-menu",
                position,
                &selected,
                available_inputs,
                Arc::new(move |value, window, cx| {
                    up(
                        Arc::new(move |s| s.hardware.audio.device_in = value.clone()),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::OutputDevice => {
            let selected = schema.hardware.audio.device_out.clone();
            let is_asio =
                sanitized_backend_label(&schema.hardware.audio.driver_type, available_backends)
                    == "ASIO";
            let up = on_update.clone();
            combo_box_string_menu(
                "settings-audio-output-menu",
                position,
                &selected,
                available_outputs,
                Arc::new(move |value, window, cx| {
                    up(
                        Arc::new(move |s| {
                            s.hardware.audio.device_out = value.clone();
                            if is_asio {
                                s.hardware.audio.device_in = value.clone();
                            }
                        }),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::ClockSource => {
            let selected = schema.hardware.sync.clock_source.clone();
            let options: Vec<String> = CLOCK_SOURCE_OPTIONS.iter().map(|s| s.to_string()).collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-clock-source-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    if midi_settings_debug_enabled() {
                        eprintln!("[MIDI settings] clock_source={value}");
                    }
                    up(
                        Arc::new(move |s| s.hardware.sync.clock_source = value.clone()),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::Language => {
            let selected = selected_locale_label(i18n, &schema.general.language);
            let options: Vec<String> = Locale::ALL
                .iter()
                .map(|locale| locale_label(i18n, *locale))
                .collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-general-language-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let locale_code = Locale::ALL
                        .iter()
                        .find(|locale| locale_label(i18n, **locale) == value)
                        .copied()
                        .unwrap_or(Locale::EnUs)
                        .code()
                        .to_string();
                    up(
                        Arc::new(move |s| s.general.language = locale_code.clone()),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::UpdateChannel => {
            let selected = schema.general.update_channel.label().to_string();
            let options = crate::settings::UpdateChannel::ALL
                .iter()
                .map(|channel| channel.label().to_string())
                .collect::<Vec<_>>();
            let up = on_update;
            combo_box_string_menu(
                "settings-general-update-channel-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let channel = crate::settings::UpdateChannel::ALL
                        .iter()
                        .copied()
                        .find(|channel| channel.label() == value)
                        .unwrap_or_default();
                    up(
                        Arc::new(move |s| s.general.update_channel = channel),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::AutosaveInterval => {
            let selected = format!("{} min", schema.general.autosave.interval_minutes);
            let options: Vec<String> = AUTOSAVE_INTERVAL_OPTIONS
                .iter()
                .map(|m| format!("{m} min"))
                .collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-general-autosave-interval-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let minutes = value
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(5);
                    up(
                        Arc::new(move |s| s.general.autosave.interval_minutes = minutes),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::AutosaveMaxBackups => {
            let selected = schema.general.autosave.max_backups.to_string();
            let options: Vec<String> = AUTOSAVE_MAX_BACKUPS_OPTIONS
                .iter()
                .map(|v| v.to_string())
                .collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-general-autosave-backups-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let backups = value.parse::<u32>().unwrap_or(10);
                    up(
                        Arc::new(move |s| s.general.autosave.max_backups = backups),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::SampleRate => {
            let selected = format!("{} Hz", schema.general.project_defaults.sample_rate);
            let options: Vec<String> = SAMPLE_RATE_OPTIONS
                .iter()
                .map(|v| format!("{v} Hz"))
                .collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-audio-sample-rate-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let sr = value
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(48000);
                    up(
                        Arc::new(move |s| s.general.project_defaults.sample_rate = sr),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::BufferSize => {
            let selected = schema.general.project_defaults.buffer_size.to_string();
            let options: Vec<String> = BUFFER_SIZE_OPTIONS.iter().map(|v| v.to_string()).collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-audio-buffer-size-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let buf = value.parse::<u32>().unwrap_or(256);
                    up(
                        Arc::new(move |s| s.general.project_defaults.buffer_size = buf),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::Renderer => {
            let selected = schema.performance.render_mode.label().to_string();
            let options: Vec<String> = vec![
                RenderMode::Auto.label().to_string(),
                RenderMode::GpuAcceleration.label().to_string(),
                RenderMode::CpuRender.label().to_string(),
            ];
            let up = on_update;
            combo_box_string_menu(
                "settings-performance-renderer-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let mode = if value == RenderMode::CpuRender.label() {
                        RenderMode::CpuRender
                    } else if value == RenderMode::GpuAcceleration.label() {
                        RenderMode::GpuAcceleration
                    } else {
                        RenderMode::Auto
                    };
                    up(
                        Arc::new(move |s| s.performance.render_mode = mode),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::GpuDevice => {
            // Enumerate adapters on open. Cheap on Windows/macOS; the
            // dropdown shows the actual device names instead of a stale
            // cached list. Falls back to "Auto" only on enumeration failure.
            let detected = list_available_gpu_devices();
            let mut options: Vec<String> = Vec::with_capacity(detected.len() + 1);
            options.push("Auto".to_string());
            for device in &detected {
                options.push(device.name.clone());
            }
            if detected.is_empty() {
                options.push("No GPU device found".to_string());
            }
            let options = crate::components::combo_box::dedupe_preserve_order(&options);
            let selected = match &schema.performance.gpu_device {
                GpuDevicePreference::Auto => "Auto".to_string(),
                GpuDevicePreference::DeviceId(id) => detected
                    .iter()
                    .find(|d| &d.id == id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "Auto".to_string()),
            };
            // Build a stable label -> id map for commit time.
            let id_lookup: Vec<(String, String)> = detected
                .iter()
                .map(|d| (d.name.clone(), d.id.clone()))
                .collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-performance-gpu-device-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    if value == "No GPU device found" {
                        return;
                    }
                    let next = if value == "Auto" {
                        GpuDevicePreference::Auto
                    } else {
                        id_lookup
                            .iter()
                            .find(|(name, _)| name == &value)
                            .map(|(_, id)| GpuDevicePreference::DeviceId(id.clone()))
                            .unwrap_or(GpuDevicePreference::Auto)
                    };
                    up(
                        Arc::new(move |s| s.performance.gpu_device = next.clone()),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
        }
        HardwareCombo::FrameRate => {
            use crate::frame_scheduler::FrameRateMode;
            let selected = schema.performance.frame_rate.label().to_string();
            let options: Vec<String> = FrameRateMode::all()
                .iter()
                .map(|m| m.label().to_string())
                .collect();
            let up = on_update;
            combo_box_string_menu(
                "settings-performance-frame-rate-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    if let Some(mode) = FrameRateMode::all()
                        .into_iter()
                        .find(|m| m.label() == value)
                    {
                        up(
                            Arc::new(move |s| s.performance.frame_rate = mode),
                            window,
                            cx,
                        );
                    }
                }),
            )
            .into_any_element()
        }
    };

    div()
        .absolute()
        .inset_0()
        .id("settings-hardware-combo-overlay")
        .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
            let _ = close_target.update(cx, |this, cx| {
                this.open_hardware_combo = None;
                this.hardware_combo_anchor = None;
                cx.notify();
            });
        })
        .child(menu)
}
