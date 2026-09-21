//! Split out of `settings_dialog.rs` (god-file decomposition). `use super::*`.

use super::*;

use crate::components::text_input::bind_mouse_selection;

impl SettingsWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        settings: Entity<SettingsModel>,
        available_inputs: Vec<String>,
        available_outputs: Vec<String>,
        available_backends: Vec<String>,
        available_input_channels: Vec<(String, u32)>,
        available_output_channels: Vec<(String, u32)>,
        device_lists_provider: Option<AudioDeviceListsProvider>,
        latency_provider: AudioLatencySnapshotProvider,
        input_test_start: Option<InputTestStartFn>,
        input_test_stop: Option<InputTestStopFn>,
        input_test_level: Option<InputTestLevelFn>,
        on_update: OnSettingUpdate,
        on_open_keyboard_shortcuts: Option<OnOpenKeyboardShortcuts>,
        on_open_plugin_manager: Option<OnOpenPluginManager>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = TextInputState::new("settings-search", cx.focus_handle())
            .with_placeholder("Search settings...");
        // Seed the device-list cache from whatever the caller pre-supplied
        // (placeholder lists when there is no engine). With an engine present the
        // caller passes empty lists and `device_lists_backend = None` forces the
        // first render to kick an off-thread refresh, so opening Settings never
        // blocks on hardware enumeration / WDM-KS probing.
        let device_lists = SettingsAudioDeviceLists {
            inputs: available_inputs,
            outputs: available_outputs,
            input_channels: available_input_channels,
            output_channels: available_output_channels,
        };
        // No provider ⇒ static placeholder lists are final; mark the cache valid
        // for any backend so render never tries to refresh.
        let device_lists_backend = if device_lists_provider.is_none() {
            Some(String::new())
        } else {
            None
        };
        let latency = latency_provider();
        let mut this = Self {
            settings,
            active_tab: SettingsTab::General,
            search_input,
            search_context_menu: None,
            available_backends,
            device_lists,
            device_lists_backend,
            device_refresh_in_flight: false,
            latency,
            driver_status_details_open: false,
            renders_since_backend_change: 0,
            device_lists_provider,
            latency_provider,
            input_test_start,
            input_test_stop,
            input_test_level,
            input_test_active: false,
            input_test_level_value: 0.0,
            input_test_error: None,
            open_hardware_combo: None,
            hardware_combo_anchor: None,
            midi_refresh_nonce: 0,
            midi_refresh_in_flight: false,
            on_update,
            on_open_keyboard_shortcuts,
            on_open_plugin_manager,
            focus_handle: cx.focus_handle(),
        };
        // Keep Driver Status live without per-frame polling.
        this.schedule_latency_poll(cx);
        this
    }

    pub fn set_active_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        if tab != SettingsTab::Recording && self.input_test_active {
            self.stop_input_test(cx);
        }
        self.active_tab = tab;
        self.open_hardware_combo = None;
        self.hardware_combo_anchor = None;
        cx.notify();
    }

    /// Refresh the backend-scoped device list (and the Driver Status snapshot)
    /// for the current draft backend **off the UI thread**. Coalesced: while one
    /// refresh is in flight, callers are ignored; the next render re-checks and
    /// re-kicks if the backend changed again. On completion a single `notify`
    /// updates the dropdowns and Driver Status at once.
    pub(super) fn refresh_audio_devices(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.device_lists_provider.clone() else {
            return;
        };
        if self.device_refresh_in_flight {
            return;
        }
        let driver_type = self
            .settings
            .read(cx)
            .current
            .hardware
            .audio
            .driver_type
            .clone();
        let latency_provider = self.latency_provider.clone();
        let debug = settings_perf_debug_enabled();
        self.device_refresh_in_flight = true;

        cx.spawn(async move |this, cx| {
            let probe_driver = driver_type.clone();
            // Hardware enumeration / WDM-KS probing runs on a background thread.
            let (lists, latency, probe_ms) = cx
                .background_executor()
                .spawn(async move {
                    let started = std::time::Instant::now();
                    let lists = provider(&probe_driver);
                    let latency = latency_provider();
                    (lists, latency, started.elapsed().as_secs_f64() * 1000.0)
                })
                .await;
            let _ = this.update(cx, |window, cx| {
                window.device_lists = lists;
                window.latency = latency;
                window.device_lists_backend = Some(driver_type.clone());
                window.device_refresh_in_flight = false;
                if debug {
                    eprintln!(
                        "[settings-perf] audio device refresh backend='{driver_type}' \
                         probe={probe_ms:.1}ms renders_during_change={}",
                        window.renders_since_backend_change
                    );
                }
                window.renders_since_backend_change = 0;
                cx.notify();
            });
        })
        .detach();
    }

    /// Retry MIDI enumeration off the foreground thread. WinMM, ALSA and
    /// CoreMIDI can all report zero ports briefly while their service/session
    /// is still initializing; coalesce repeated clicks into one scan job.
    pub(super) fn refresh_midi_devices(&mut self, cx: &mut Context<Self>) {
        if self.midi_refresh_in_flight {
            return;
        }
        self.midi_refresh_in_flight = true;
        cx.spawn(async move |this, cx| {
            let revision = cx
                .background_executor()
                .spawn(async { crate::device_registry::scan_midi_resilient() })
                .await;
            let _ = this.update(cx, |window, cx| {
                window.midi_refresh_in_flight = false;
                window.midi_refresh_nonce = window.midi_refresh_nonce.wrapping_add(1);
                if midi_settings_debug_enabled() {
                    eprintln!(
                        "[MIDI settings] refresh completed (nonce={} registry_revision={revision})",
                        window.midi_refresh_nonce
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn start_input_test(&mut self, cx: &mut Context<Self>) {
        let Some(start) = self.input_test_start.clone() else {
            self.input_test_error = Some("audio engine unavailable".to_string());
            cx.notify();
            return;
        };
        let device = self
            .settings
            .read(cx)
            .current
            .hardware
            .audio
            .device_in
            .clone();
        match start(Some(device)) {
            Ok(()) => {
                self.input_test_active = true;
                self.input_test_level_value = 0.0;
                self.input_test_error = None;
                self.schedule_input_test_poll(cx);
            }
            Err(error) => {
                self.input_test_active = false;
                self.input_test_level_value = 0.0;
                self.input_test_error = Some(error);
            }
        }
        cx.notify();
    }

    pub(super) fn stop_input_test(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = self.input_test_stop.as_ref() {
            stop();
        }
        self.input_test_active = false;
        self.input_test_level_value = 0.0;
        cx.notify();
    }

    /// Poll the (cheap, stats-only) Driver Status snapshot on a slow cadence
    /// while the window is open, re-rendering **only when it actually changes**.
    /// This replaces the old per-render `latency_provider()` call: status stays
    /// live (e.g. `DeviceLost`, post-Apply backend name) without shaping the
    /// badge on every frame. Stops automatically when the entity is dropped.
    pub(super) fn schedule_latency_poll(&mut self, cx: &mut Context<Self>) {
        let provider = self.latency_provider.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(750))
                .await;
            let _ = this.update(cx, |window, cx| {
                let next = provider();
                let changed = next.engine_open != window.latency.engine_open
                    || next.device_state != window.latency.device_state
                    || next.backend_name != window.latency.backend_name
                    || next.last_error != window.latency.last_error
                    || next.buffer_ms != window.latency.buffer_ms
                    || next.active_sample_rate != window.latency.active_sample_rate
                    || next.requested_sample_rate != window.latency.requested_sample_rate
                    || next.restart_pending != window.latency.restart_pending
                    || next.deferred_sample_rate != window.latency.deferred_sample_rate;
                window.latency = next;
                if changed {
                    cx.notify();
                }
                window.schedule_latency_poll(cx);
            });
        })
        .detach();
    }

    pub(super) fn schedule_input_test_poll(&mut self, cx: &mut Context<Self>) {
        if !self.input_test_active {
            return;
        }
        let level_provider = self.input_test_level.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(50))
                .await;
            let _ = this.update(cx, |window, cx| {
                if !window.input_test_active {
                    return;
                }
                if window.active_tab != SettingsTab::Recording {
                    window.stop_input_test(cx);
                    return;
                }
                window.input_test_level_value = level_provider
                    .as_ref()
                    .map(|read| read().clamp(0.0, 1.0))
                    .unwrap_or(0.0);
                window.schedule_input_test_poll(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key.as_str() == "escape" && self.open_hardware_combo.take().is_some() {
            self.hardware_combo_anchor = None;
            cx.notify();
            return;
        }

        let search_focused = self.search_input.is_focused(window);
        if search_focused {
            let action = self.search_input.handle_key_with_clipboard(event, Some(cx));
            match action {
                TextInputAction::Cancel => window.remove_window(),
                _ => {}
            }
            cx.notify();
            return;
        }
        let key = event.keystroke.key.as_str();
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        match key {
            "escape" => {
                self.stop_input_test(cx);
                window.remove_window();
            }
            "f" if ctrl => {
                self.search_input.focus_handle.focus(window, cx);
                cx.notify();
            }
            _ => {}
        }
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_started = std::time::Instant::now();
        let perf_debug = settings_perf_debug_enabled();
        let schema = self.settings.read(cx).current.clone();
        let i18n = I18n::new(&schema.general.language);
        self.search_input.placeholder = Some(i18n.tr("search.settings.placeholder"));
        let target = cx.entity().clone();
        let on_update = self.on_update.clone();
        let search_focused = self.search_input.is_focused(window);

        // Device enumeration / WDM-KS probing must NOT run on the render path.
        // If the cached lists are for a different backend than the current draft,
        // kick exactly one off-thread refresh; render keeps using the (stale)
        // cache until it completes. `device_refresh_in_flight` coalesces repeats.
        let cache_stale = self.device_lists_backend.as_deref()
            != Some(schema.hardware.audio.driver_type.as_str());
        if cache_stale && self.device_lists_provider.is_some() {
            self.renders_since_backend_change = self.renders_since_backend_change.saturating_add(1);
            if !self.device_refresh_in_flight {
                self.refresh_audio_devices(cx);
            }
        }

        let state = SettingsDialogState {
            is_open: true,
            active_tab: self.active_tab,
            search_query: self.search_input.value.clone(),
            driver_status_details_open: self.driver_status_details_open,
        };

        let callbacks = SettingsDialogCallbacks {
            on_close: Arc::new(|_: &(), window: &mut Window, _cx: &mut App| {
                window.remove_window();
            }),
            on_select_tab: Arc::new({
                let target = target.clone();
                move |tab: &SettingsTab, _w: &mut Window, cx: &mut App| {
                    let tab = *tab;
                    let _ = target.update(cx, |this, cx| {
                        if tab != SettingsTab::Recording && this.input_test_active {
                            this.stop_input_test(cx);
                        }
                        this.active_tab = tab;
                        this.open_hardware_combo = None;
                        this.hardware_combo_anchor = None;
                        cx.notify();
                    });
                }
            }),
            on_update_setting: Arc::new({
                let on_update = on_update.clone();
                let target = target.clone();
                move |updater: UpdateSettingFn, _w: &mut Window, cx: &mut App| {
                    (on_update)(updater, cx);
                    let _ = target.update(cx, |this, cx| {
                        this.open_hardware_combo = None;
                        this.hardware_combo_anchor = None;
                        cx.notify();
                    });
                }
            }),
            on_toggle_input_test: Arc::new({
                let target = target.clone();
                move |_: &(), _w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        if this.input_test_active {
                            this.stop_input_test(cx);
                        } else {
                            this.start_input_test(cx);
                        }
                    });
                }
            }),
            on_refresh_midi: Some(Arc::new({
                let target = target.clone();
                move |_w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        this.refresh_midi_devices(cx);
                    });
                }
            })),
            open_hardware_combo: self.open_hardware_combo,
            on_toggle_hardware_combo: Arc::new({
                let target = target.clone();
                move |combo: HardwareCombo,
                      anchor: Option<OverlayAnchor>,
                      _w: &mut Window,
                      cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        if this.open_hardware_combo == Some(combo) {
                            this.open_hardware_combo = None;
                            this.hardware_combo_anchor = None;
                        } else {
                            this.open_hardware_combo = Some(combo);
                            this.hardware_combo_anchor = anchor;
                        }
                        cx.notify();
                    });
                }
            }),
            on_toggle_driver_details: Some(Arc::new({
                let target = target.clone();
                move |_w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        this.driver_status_details_open = !this.driver_status_details_open;
                        cx.notify();
                    });
                }
            })),
            on_open_keyboard_shortcuts: self.on_open_keyboard_shortcuts.clone(),
            on_open_plugin_manager: self.on_open_plugin_manager.clone(),
        };

        let search_mouse_callbacks =
            bind_mouse_selection(target.clone(), |this| &mut this.search_input);
        let search_callbacks = TextInputCallbacks {
            on_context_command: None,
            on_context_menu: Some(Arc::new({
                let target = target.clone();
                move |pos: &(f32, f32), _w, cx| {
                    let pos = *pos;
                    let _ = target.update(cx, |this, cx| {
                        this.search_context_menu = Some(pos);
                        cx.notify();
                    });
                }
            })),
            on_mouse: search_mouse_callbacks.on_mouse,
        };

        // Same entries and enablement as the studio shell's text menu; this
        // window just has to draw its own, since the shell's overlay does not
        // reach a separate window.
        let search_context_overlay = self.search_context_menu.map(|(x, y)| {
            let clipboard_has_text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty());
            let entries = crate::components::text_input::text_input_context_entries(
                &self.search_input,
                clipboard_has_text,
            );
            let command_target = target.clone();
            let close_target = target.clone();
            crate::components::context_menu::context_menu_overlay(
                entries,
                x,
                y,
                SETTINGS_WIDTH,
                SETTINGS_HEIGHT,
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ = command_target.update(cx, |this, cx| {
                        let _ = this.search_input.apply_context_command(&command, cx);
                        this.search_context_menu = None;
                        cx.notify();
                    });
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| {
                        this.search_context_menu = None;
                        cx.notify();
                    });
                }),
            )
        });

        // Read cached snapshots only — no provider calls, no enumeration here.
        let latency = self.latency.clone();
        let input_test = InputTestMeterState {
            active: self.input_test_active,
            level: self.input_test_level_value,
            error: self.input_test_error.clone(),
        };
        let _midi_refresh = self.midi_refresh_nonce;
        let device_lists = self.device_lists.clone();

        let (sidebar_items, sections) = build_settings_content(
            &state,
            &schema,
            &callbacks,
            &latency,
            &input_test,
            &device_lists.inputs,
            &device_lists.outputs,
            &self.available_backends,
            &device_lists.input_channels,
            &device_lists.output_channels,
        );

        let sw_target = target.clone();

        let combo_overlay = if let (Some(open_combo), Some(anchor)) =
            (self.open_hardware_combo, self.hardware_combo_anchor)
        {
            if !anchor_visible_in_window(anchor, window) {
                self.open_hardware_combo = None;
                self.hardware_combo_anchor = None;
                None
            } else {
                let close_target = sw_target.clone();
                let overlay_update = Arc::new({
                    let on_update = on_update.clone();
                    let target = sw_target.clone();
                    move |updater: UpdateSettingFn, _w: &mut Window, cx: &mut App| {
                        (on_update)(updater, cx);
                        let _ = target.update(cx, |this, cx| {
                            this.open_hardware_combo = None;
                            this.hardware_combo_anchor = None;
                            cx.notify();
                        });
                    }
                });
                Some(hardware_combo_overlay(
                    open_combo,
                    anchor,
                    window,
                    &schema,
                    &device_lists.inputs,
                    &device_lists.outputs,
                    &self.available_backends,
                    overlay_update,
                    close_target,
                ))
            }
        } else {
            None
        };

        let root = div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .font(theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .capture_key_down({
                let target = sw_target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar(
                i18n.tr("settings.title"),
                "settings-window-close",
                {
                    let target = sw_target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| {
                            if this.input_test_active {
                                this.stop_input_test(cx);
                            }
                            this.open_hardware_combo = None;
                            this.hardware_combo_anchor = None;
                            cx.notify();
                        });
                        window.remove_window();
                    }
                },
            ))
            // Two-column body — DAW studio control center layout
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("settings-sidebar")
                            .w(px(SETTINGS_SIDEBAR_WIDTH))
                            .flex_shrink_0()
                            .border_r(px(1.0))
                            .border_color(Colors::divider())
                            .bg(Colors::surface_panel_alt())
                            .overflow_y_scroll()
                            .py(px(6.0))
                            .flex()
                            .flex_col()
                            .children(sidebar_items),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .bg(Colors::surface_panel())
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px(px(SETTINGS_CONTENT_PAD))
                                    .pt(px(10.0))
                                    .pb(px(8.0))
                                    .border_b(px(1.0))
                                    .border_color(Colors::divider())
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .justify_between()
                                            .gap(px(12.0))
                                            .child(settings_page_header(
                                                i18n.tr(self.active_tab.label_key()),
                                                i18n.tr(self.active_tab.page_description_key()),
                                            ))
                                            .child(div().w(px(208.0)).flex_shrink_0().child(
                                                text_field_with_callbacks(
                                                    &self.search_input,
                                                    search_focused,
                                                    search_callbacks,
                                                ),
                                            )),
                                    ),
                            )
                            .child({
                                let scroll_close = sw_target.clone();
                                div()
                                    .id("settings-content-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .p(px(SETTINGS_CONTENT_PAD))
                                    .flex()
                                    .flex_col()
                                    .gap(px(SETTINGS_SECTION_GAP))
                                    .on_scroll_wheel(move |_, _window, cx| {
                                        let _ = scroll_close.update(cx, |this, cx| {
                                            if this.open_hardware_combo.take().is_some() {
                                                this.hardware_combo_anchor = None;
                                                cx.notify();
                                            }
                                        });
                                        cx.stop_propagation();
                                    })
                                    .children(sections)
                            }),
                    ),
            )
            .children(combo_overlay)
            .children(search_context_overlay);

        if perf_debug {
            let blocked_ms = render_started.elapsed().as_secs_f64() * 1000.0;
            // Flag any UI-thread render that blocks long enough to drop a frame.
            if blocked_ms >= 4.0 {
                eprintln!(
                    "[settings-perf] render blocked UI thread {blocked_ms:.1}ms \
                     (tab={:?} refresh_in_flight={})",
                    self.active_tab, self.device_refresh_in_flight
                );
            }
        }

        root
    }
}

#[allow(clippy::too_many_arguments)]
pub fn open_settings_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    settings: Entity<SettingsModel>,
    available_inputs: Vec<String>,
    available_outputs: Vec<String>,
    available_backends: Vec<String>,
    available_input_channels: Vec<(String, u32)>,
    available_output_channels: Vec<(String, u32)>,
    device_lists_provider: Option<AudioDeviceListsProvider>,
    latency_provider: AudioLatencySnapshotProvider,
    input_test_start: Option<InputTestStartFn>,
    input_test_stop: Option<InputTestStopFn>,
    input_test_level: Option<InputTestLevelFn>,
    on_update: OnSettingUpdate,
    on_open_keyboard_shortcuts: Option<OnOpenKeyboardShortcuts>,
    on_open_plugin_manager: Option<OnOpenPluginManager>,
    cx: &mut App,
) -> Result<WindowHandle<SettingsWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(SETTINGS_WIDTH), px(SETTINGS_HEIGHT)),
        cx,
    );

    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = true;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    options.window_min_size = Some(size(px(SETTINGS_WIDTH), px(SETTINGS_HEIGHT)));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| {
            SettingsWindow::new(
                settings,
                available_inputs,
                available_outputs,
                available_backends,
                available_input_channels,
                available_output_channels,
                device_lists_provider,
                latency_provider,
                input_test_start,
                input_test_stop,
                input_test_level,
                on_update,
                on_open_keyboard_shortcuts,
                on_open_plugin_manager,
                cx,
            )
        })
    })
    .map_err(|error| error.to_string())
}
