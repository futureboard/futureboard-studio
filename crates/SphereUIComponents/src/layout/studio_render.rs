use super::*;

impl Render for StudioLayout {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.session_install_status.is_ready() {
            eprintln!("[StudioMount] blocked because session not ready");
            return div()
                .id("futureboard-studio-root")
                .role(Role::Application)
                .aria_label("Futureboard Studio")
                .size_full()
                .bg(Colors::surface_base());
        }

        publish_studio_main_hwnd(window);

        // Perf probe: if the MAIN DAW window re-renders while the insert picker is
        // open, that points the typing-stutter at a full StudioLayout repaint
        // rather than the picker window. Gated by the existing picker debug flag.
        if self.plugin_picker.is_open && crate::components::plugin_picker::picker_perf_debug() {
            eprintln!(
                "[picker-perf] StudioLayout::render (main DAW window repainted while picker open)"
            );
        }

        let _root_scope = crate::perf::PerfScope::enter("StudioLayout");
        let i18n = crate::i18n::I18n::from_app(cx);
        self.project_switcher_search_input.placeholder =
            Some(i18n.tr("search.projects.placeholder"));
        // Frame pacing tick. See FrameDiagnostics docs — only counts
        // real repaints, not display refreshes.
        let reason = self.frame_reason();
        let reason_static: &'static str = match reason {
            "transport" => "transport",
            "panel-resize" => "panel-resize",
            "menu" => "menu",
            _ => "idle/interaction",
        };
        self.frame_diag.tick(reason);
        crate::perf::tick_root_frame(reason_static);
        if self
            .settings
            .read(cx)
            .current
            .performance
            .show_status_performance_metrics
        {
            self.notify_status_bar_if_changed(cx);
        }
        // Re-resolve the frame-pacing mode from settings (env override still
        // wins) and republish the poll cadence. Cheap; applies a Settings change
        // on the next frame without a dedicated observer.
        let frame_rate_mode = self.settings.read(cx).current.performance.frame_rate;
        self.frame_scheduler.refresh_from_settings(frame_rate_mode);
        self.maybe_autosave_project(cx);
        self.window_hooks.cached_bounds = Some(window.bounds());
        self.flush_deferred_insert_editor_opens(window, cx);
        self.flush_pending_audio_tools(window, cx);

        // Keep the OS window title in sync with the project lifecycle state
        // (Part G/H), e.g. "Untitled Project — Unsaved" / "My Song — Saved".
        let title = self.window_title();
        if self.last_window_title.as_deref() != Some(title.as_str()) {
            window.set_window_title(&title);
            self.last_window_title = Some(title.clone());
        }

        // The Inspector's own state lives in the cached right-dock region
        // now (`shell_regions`). What stays here is what the *shell* draws:
        // the routing combo, which anchors over the whole window, and the
        // colour picker's click-outside backdrop.
        let inspector_routing_combo_overlay =
            self.inspector_routing_combo_overlay_element(window, cx);
        let inspector_color_open = self.inspector_name_edit.color_picker.open;

        let on_timeline_context: components::timeline::timeline::TimelineContextMenuCb = {
            let this = cx.entity().clone();
            std::sync::Arc::new(
                move |(target, x, y): &(TimelineContextTarget, f32, f32), window, cx| {
                    let target = target.clone();
                    let x = *x;
                    let y = *y;
                    let window_id = window.window_handle().window_id();
                    StudioLayout::defer_update(&this, cx, move |this, cx| {
                        let context_target = match target {
                            TimelineContextTarget::TimelineEmpty => ContextTarget::TimelineEmpty,
                            TimelineContextTarget::TrackLane { track_id, beat } => {
                                ContextTarget::TrackLane { track_id, beat }
                            }
                            TimelineContextTarget::TrackHeader(id) => {
                                this.timeline.update(cx, |timeline, cx| {
                                    timeline.state.select_track(&id);
                                    cx.notify();
                                });
                                ContextTarget::Track(id)
                            }
                            TimelineContextTarget::Clip(id) => {
                                this.timeline.update(cx, |timeline, cx| {
                                    timeline.state.select_clip(&id);
                                    cx.notify();
                                });
                                ContextTarget::Clip(id)
                            }
                            TimelineContextTarget::AudioClip { clip_id, .. }
                            | TimelineContextTarget::MidiClip { clip_id, .. } => {
                                this.timeline.update(cx, |timeline, cx| {
                                    if !timeline
                                        .state
                                        .selection
                                        .selected_clip_ids
                                        .iter()
                                        .any(|id| id == &clip_id)
                                    {
                                        timeline.state.select_clip(&clip_id);
                                        cx.notify();
                                    }
                                });
                                ContextTarget::Clip(clip_id)
                            }
                            TimelineContextTarget::Marker { marker_id, beat } => {
                                ContextTarget::TimelineMarker { marker_id, beat }
                            }
                            TimelineContextTarget::SongTextMarker { event_id, beat } => {
                                ContextTarget::SongTextMarker { event_id, beat }
                            }
                            TimelineContextTarget::AutomationLane {
                                track_id,
                                lane_id,
                                beat,
                            } => ContextTarget::AutomationLane {
                                track_id,
                                lane_id,
                                beat,
                            },
                            TimelineContextTarget::Ruler(beat) => {
                                ContextTarget::TimelineRuler { beat }
                            }
                            TimelineContextTarget::TempoTrack {
                                beat,
                                bpm,
                                point_id,
                            } => ContextTarget::TempoTrack {
                                beat,
                                bpm,
                                point_id,
                            },
                            TimelineContextTarget::TimeSignatureTrack { beat, point_id } => {
                                ContextTarget::TimeSignatureTrack { beat, point_id }
                            }
                            TimelineContextTarget::MarkerTrack { beat, marker_id } => {
                                ContextTarget::MarkerTrack { beat, marker_id }
                            }
                            TimelineContextTarget::RegionTrack { beat, region_id } => {
                                ContextTarget::RegionTrack { beat, region_id }
                            }
                            TimelineContextTarget::MarkerLaneHeader => ContextTarget::MarkerLane,
                            TimelineContextTarget::RegionLaneHeader => ContextTarget::RegionLane,
                            TimelineContextTarget::TempoLaneHeader => ContextTarget::Tempo,
                            TimelineContextTarget::TimeSignatureLaneHeader => {
                                ContextTarget::TimeSignature
                            }
                            TimelineContextTarget::AutomationTargetPicker { track_id } => {
                                ContextTarget::AutomationTargetPicker { track_id }
                            }
                        };
                        // A right-click that landed on a marker, a region, a
                        // tempo marker or a meter mark deletes it outright.
                        // Removing one used to mean opening the menu and
                        // finding Delete, which is a lot of ceremony for the
                        // most common thing anyone does to them.
                        if this.delete_context_target(&context_target, cx) {
                            return;
                        }
                        this.try_open_context_menu(
                            ContextMenuRequest::new(
                                window_id,
                                x,
                                y,
                                ContextMenuTarget::from_context_target(context_target),
                            ),
                            cx,
                        );
                    });
                },
            )
        };
        let _ = self.timeline.update(cx, |timeline, _cx| {
            timeline.set_context_menu_callback(Some(on_timeline_context));
        });

        let on_tempo_point_edit: components::timeline::timeline::TempoPointEditCb = {
            let this = cx.entity().clone();
            std::sync::Arc::new(
                move |point_id: &str, window: &mut Window, cx: &mut gpui::App| {
                    let point_id = point_id.to_string();
                    // Double-click on a Tempo Track marker fires from Timeline's
                    // `cx.listener`. `begin_bpm_edit` reads Timeline for the
                    // marker BPM, so wait until that lease ends.
                    StudioLayout::defer_update_in_window(
                        &this,
                        window,
                        cx,
                        move |this, _window, cx| {
                            this.begin_bpm_edit(Some(point_id), cx);
                        },
                    );
                },
            )
        };
        let _ = self.timeline.update(cx, |timeline, _cx| {
            timeline.set_tempo_point_edit_callback(Some(on_tempo_point_edit));
        });

        let on_automation_control: components::timeline::automation_control_lane::AutomationControlCallback = {
            let this = cx.entity().clone();
            std::sync::Arc::new(
                move |(track_id, action, x, y): &(
                    String,
                    components::timeline::automation_control_lane::AutomationControlAction,
                    f32,
                    f32,
                ),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    let track_id = track_id.clone();
                    let action = *action;
                    let x = *x;
                    let y = *y;
                    StudioLayout::defer_update_in_window(&this, window, cx, move |this, window, cx| {
                        this.handle_automation_control_action(&track_id, action, x, y, window, cx);
                    });
                },
            )
        };
        let _ = self.timeline.update(cx, |timeline, _cx| {
            timeline.set_automation_control_callback(Some(on_automation_control));
        });

        let on_add_track: components::timeline::timeline::TimelineAddTrackCb = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |request, window, cx| {
                let request = *request;
                // Timeline's Add button fires while Timeline is mid-update
                // (`cx.listener`). Opening the dialog must not `timeline.read`
                // until that lease ends — defer like automation-control.
                StudioLayout::defer_update_in_window(
                    &this,
                    window,
                    cx,
                    move |this, _window, cx| {
                        this.open_add_track_external_window_with_context(
                            AddTrackKind::Audio,
                            request.track_count,
                            request.has_master_track,
                            None,
                            cx,
                        );
                    },
                );
            })
        };
        let _ = self.timeline.update(cx, |timeline, _cx| {
            timeline.set_add_track_callback(Some(on_add_track));
        });

        // ── Top-menu callbacks ─────────────────────────────────────────────
        let on_open_menu = self.menu_open_callback(cx);
        let on_close_menu: std::sync::Arc<dyn Fn(&(), &mut Window, &mut gpui::App) + 'static> = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |_: &(), _w, cx| {
                this.update(cx, |this, cx| {
                    this.menu_bar.open_menu_id = None;
                    this.menu_bar.submenu_path.clear();
                    cx.notify();
                });
            })
        };
        let on_toggle_submenu: std::sync::Arc<
            dyn Fn(&(usize, String), &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |(depth, id): &(usize, String), _w, cx| {
                let depth = *depth;
                let id = id.clone();
                this.update(cx, |this, cx| {
                    // Truncate the path to this depth, then toggle: if the
                    // requested id is already open at this depth, close it;
                    // otherwise open it (closing anything deeper).
                    let already_open = this.menu_bar.submenu_path.get(depth) == Some(&id);
                    this.menu_bar.submenu_path.truncate(depth);
                    if !already_open {
                        this.menu_bar.submenu_path.push(id);
                    }
                    cx.notify();
                });
            })
        };
        let on_menu_command: std::sync::Arc<
            dyn Fn(&String, &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |command: &String, w, cx| {
                let command = command.clone();
                let _ = this.update(cx, |this, cx| {
                    this.dispatch_command_id_from_bounds(&command, Some(w.bounds()), cx);
                    this.overlay.open_popover = None;
                    this.project_switcher.is_open = false;
                    cx.notify();
                });
            })
        };
        let open_menu_id = self.menu_bar.open_menu_id.clone();
        let menu_anchor = self.menu_bar.anchor;
        let submenu_path = self.menu_bar.submenu_path.clone();
        let viewport_width: f32 = window.bounds().size.width.into();
        let viewport_height: f32 = window.bounds().size.height.into();
        let ui_language = self.settings.read(cx).current.general.language.clone();
        let i18n = crate::i18n::I18n::new(&ui_language);

        let chrome_policy = crate::platform_chrome::PlatformChromePolicy::current();
        let dropdown_overlay = if chrome_policy.show_in_window_menubar {
            open_menu_id.as_ref().and_then(|id| {
                if id == components::menu_bar::MENU_PICKER_ID {
                    Some(
                        components::menu_bar::menu_picker_dropdown(
                            menu_anchor,
                            viewport_width,
                            viewport_height,
                            on_open_menu.clone(),
                            on_close_menu.clone(),
                            i18n,
                        )
                        .into_any_element(),
                    )
                } else {
                    let manifest = crate::menu::MenuManifest::load();
                    manifest.menus.iter().find(|m| &m.id == id).map(|menu| {
                        let mut runtime_menu = menu.clone();
                        let perf = self.settings.read(cx).current.performance.clone();
                        crate::menu::patch_checkbox_states(
                            &mut runtime_menu.items,
                            &[
                                ("window.show_browser", self.panels.browser),
                                ("window.show_inspector", self.panels.inspector),
                                // "Show Mixer" is checked when the mixer is on
                                // screen, which is what the command now toggles
                                // — not merely when the dock happens to be open
                                // on some other tab.
                                ("window.show_mixer", self.mixer_panel_chrome_visible()),
                                // The dock itself, not the tab in it: this is
                                // ticked whenever the bottom panel is docked
                                // open, whatever it happens to be showing.
                                ("window.show_bottom_panel", self.panels.bottom_docked),
                                (
                                    "view.developer.perf_metrics",
                                    perf.show_status_performance_metrics,
                                ),
                                ("view.developer.perf_overlay", perf.show_performance_overlay),
                            ],
                        );
                        components::menu_dropdown::menu_dropdown(
                            &runtime_menu,
                            menu_anchor,
                            viewport_width,
                            viewport_height,
                            &submenu_path,
                            on_toggle_submenu.clone(),
                            on_menu_command.clone(),
                            on_close_menu.clone(),
                            i18n,
                        )
                        .into_any_element()
                    })
                }
            })
        } else {
            None
        };
        let on_close_popover: std::sync::Arc<dyn Fn(&(), &mut Window, &mut gpui::App) + 'static> = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |_: &(), _w, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.overlay.open_popover = None;
                    this.project_switcher.is_open = false;
                    this.command_palette.close();
                    this.overlay.text_context_menu = None;
                    cx.notify();
                });
            })
        };
        let on_popover_command: std::sync::Arc<
            dyn Fn(&String, &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |command: &String, w, cx| {
                let command = command.clone();
                let _ = this.update(cx, |this, cx| {
                    if this.overlay.open_popover.is_some()
                        && !this.validate_open_context_menu_action(cx)
                    {
                        eprintln!("[ContextMenu] action target stale, ignored");
                        this.close_context_menu(cx);
                        return;
                    }
                    this.dispatch_command_id_from_bounds(&command, Some(w.bounds()), cx);
                    this.close_context_menu(cx);
                    this.project_switcher.is_open = false;
                    this.command_palette.close();
                    cx.notify();
                });
            })
        };
        let on_switcher_row_action: std::sync::Arc<
            dyn Fn(
                    components::project_switcher::ProjectSwitcherRowEvent,
                    &mut Window,
                    &mut gpui::App,
                ) + Send
                + Sync,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(
                move |event: components::project_switcher::ProjectSwitcherRowEvent, w, cx| {
                    let _ = this.update(cx, |this, cx| {
                        let owner_bounds = Some(w.bounds());
                        match event {
                            components::project_switcher::ProjectSwitcherRowEvent::CurrentProject => {
                                this.handle_project_switch_current_row(cx);
                            }
                            components::project_switcher::ProjectSwitcherRowEvent::SwitchProject {
                                path,
                                name,
                                is_missing: _,
                            } => {
                                this.request_switch_project(
                                    crate::layout::project_switch::ProjectSwitchRequest {
                                        target_path: path,
                                        target_name: Some(name),
                                        source:
                                            crate::layout::project_switch::ProjectSwitchSource::ProjectSwitcher,
                                    },
                                    owner_bounds,
                                    cx,
                                );
                            }
                        }
                    });
                },
            )
        };
        let popover_overlay = if self.command_palette.is_open {
            let search_mouse_callbacks = crate::components::text_input::bind_mouse_selection(
                cx.entity().clone(),
                |layout: &mut StudioLayout| &mut layout.command_palette_input,
            );
            let on_palette_close: std::sync::Arc<
                dyn Fn(&(), &mut Window, &mut gpui::App) + 'static,
            > = {
                let this = cx.entity().clone();
                std::sync::Arc::new(move |_: &(), w, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.command_palette.close();
                        this.focus_handle.focus(w, cx);
                        cx.notify();
                    });
                })
            };
            let on_palette_command: std::sync::Arc<
                dyn Fn(&String, &mut Window, &mut gpui::App) + 'static,
            > = {
                let this = cx.entity().clone();
                std::sync::Arc::new(move |command: &String, w, cx| {
                    let command = command.clone();
                    let _ = this.update(cx, |this, cx| {
                        this.command_palette.close();
                        this.focus_handle.focus(w, cx);
                        this.dispatch_command_id_from_bounds(&command, Some(w.bounds()), cx);
                        cx.notify();
                    });
                })
            };
            Some(
                components::command_palette_overlay(
                    &self.command_palette,
                    &self.command_palette_input,
                    self.command_palette_input.is_focused(window),
                    search_mouse_callbacks,
                    viewport_width,
                    viewport_height,
                    on_palette_command,
                    on_palette_close,
                )
                .into_any_element(),
            )
        } else if self.project_switcher.is_open {
            let search_mouse_callbacks = crate::components::text_input::bind_mouse_selection(
                cx.entity().clone(),
                |layout: &mut StudioLayout| &mut layout.project_switcher_search_input,
            );
            let search_context_callbacks = TextInputCallbacks {
                on_mouse_down_out: None,
                on_context_command: None,
                on_context_menu: Some(Arc::new({
                    let this = cx.entity().clone();
                    move |(x, y): &(f32, f32), _w, cx| {
                        let x = *x;
                        let y = *y;
                        let _ = this.update(cx, |this, cx| {
                            this.overlay.text_context_menu = Some(TextContextMenu {
                                target: TextMenuTarget::ProjectSwitcherSearch,
                                x,
                                y,
                            });
                            cx.notify();
                        });
                    }
                })),
                on_mouse: search_mouse_callbacks.on_mouse,
            };
            Some(
                components::project_switcher::project_switcher_popover(
                    &self.project_switcher,
                    &self.project_switcher_search_input,
                    self.project_switcher_search_input.is_focused(window),
                    search_context_callbacks,
                    viewport_width,
                    viewport_height,
                    on_switcher_row_action.clone(),
                    on_popover_command.clone(),
                    on_close_popover.clone(),
                    i18n,
                )
                .into_any_element(),
            )
        } else {
            match self.overlay.open_popover.clone() {
                // A menu opened from a detached window is drawn *by* that
                // window (see `MixerWindow`): its x/y are in that window's
                // coordinates, so painting it here put it at a meaningless spot
                // over the arrangement.
                Some(OpenPopover::Context { request })
                    if request.window_id == window.window_handle().window_id() =>
                {
                    let target = request.target.to_context_target();
                    Some(
                        components::context_menu::context_menu_overlay(
                            self.context_entries(&target, cx),
                            request.x,
                            request.y,
                            viewport_width,
                            viewport_height,
                            on_popover_command.clone(),
                            on_close_popover.clone(),
                        )
                        .into_any_element(),
                    )
                }
                // Opened from a detached window: that window draws it.
                Some(OpenPopover::Context { .. }) => None,
                Some(OpenPopover::AutomationTargetPicker { track_id, x, y }) => {
                    use crate::components::timeline::automation_target_picker::automation_target_picker_overlay;

                    self.automation_picker_query =
                        self.automation_picker_search_input.value.clone();
                    let model = self
                        .timeline
                        .read(cx)
                        .state
                        .automation_picker_model(&track_id)
                        .unwrap_or_default();
                    let search_callbacks = crate::components::text_input::bind_mouse_selection(
                        cx.entity().clone(),
                        |layout: &mut StudioLayout| &mut layout.automation_picker_search_input,
                    );
                    Some(
                        automation_target_picker_overlay(
                            &model,
                            &track_id,
                            &self.automation_picker_query,
                            &self.automation_picker_search_input,
                            self.automation_picker_search_input.is_focused(window),
                            x,
                            y,
                            viewport_width,
                            viewport_height,
                            on_popover_command.clone(),
                            on_close_popover.clone(),
                            search_callbacks,
                        )
                        .into_any_element(),
                    )
                }
                None => None,
            }
        };
        // Settings is now an external window — no overlay needed.
        let settings_overlay: Option<gpui::AnyElement> = None;
        let text_context_overlay = self.overlay.text_context_menu.map(|menu| {
            let clipboard_has_text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty());
            let entries =
                text_input_context_entries(self.text_input(menu.target), clipboard_has_text);
            let command_target = cx.entity().clone();
            let close_target = cx.entity().clone();
            components::context_menu::context_menu_overlay(
                entries,
                menu.x,
                menu.y,
                viewport_width,
                viewport_height,
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ = command_target.update(cx, |this, cx| {
                        if let Some(menu) = this.overlay.text_context_menu {
                            let input = this.text_input_mut(menu.target);
                            let _ = input.apply_context_command(&command, cx);
                            this.sync_text_input_target(menu.target);
                        }
                        this.overlay.text_context_menu = None;
                        cx.notify();
                    });
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| {
                        this.overlay.text_context_menu = None;
                        cx.notify();
                    });
                }),
            )
        });
        // Add Track moved to an external window.
        let virtual_keyboard_overlay = {
            let visible = self.virtual_keyboard.read(cx).state.visible;
            if visible {
                let window_active = window.is_window_active();
                if self.virtual_keyboard_window_active && !window_active {
                    // Deferred + panel-only: releasing through the sink here (we
                    // are inside StudioLayout::render's lease) would re-enter
                    // StudioLayout::update and panic. This is the multi-window
                    // crash path (focus leaving for / closing the popout editor).
                    self.defer_release_virtual_keyboard_notes(cx);
                }
                self.virtual_keyboard_window_active = window_active;
                let status = self.resolve_virtual_keyboard_target(cx);
                let target_key = status.target.as_ref().map(|target| {
                    format!(
                        "{}:{}",
                        target.track_id,
                        target.plugin_instance_id.as_deref().unwrap_or("")
                    )
                });
                if self.virtual_keyboard_last_target != target_key {
                    self.defer_release_virtual_keyboard_notes(cx);
                    self.virtual_keyboard_last_target = target_key;
                }
                let label = status.label;
                let hint = status.hint;
                let _ = self.virtual_keyboard.update(cx, |panel, cx| {
                    panel.set_target_status(label, hint);
                    cx.notify();
                });
                Some(self.virtual_keyboard.clone().into_any_element())
            } else {
                self.virtual_keyboard_last_target = None;
                self.virtual_keyboard_window_active = window.is_window_active();
                None
            }
        };

        // Phase 2b insert plugin picker overlay.
        let plugin_picker_overlay_el: Option<gpui::AnyElement> = if self.plugin_picker.is_open
            && self.plugin_picker_window.is_none()
        {
            let search_mouse_callbacks = crate::components::text_input::bind_mouse_selection(
                cx.entity().clone(),
                |layout: &mut StudioLayout| &mut layout.plugin_picker_search_input,
            );
            let search_context_callbacks = TextInputCallbacks {
                on_mouse_down_out: None,
                on_context_command: None,
                on_context_menu: Some(Arc::new({
                    let this = cx.entity().clone();
                    move |(x, y): &(f32, f32), _w, cx| {
                        let x = *x;
                        let y = *y;
                        let _ = this.update(cx, |this, cx| {
                            this.overlay.text_context_menu = Some(TextContextMenu {
                                target: TextMenuTarget::PluginPickerSearch,
                                x,
                                y,
                            });
                            cx.notify();
                        });
                    }
                })),
                on_mouse: search_mouse_callbacks.on_mouse,
            };
            let picker_callbacks = PluginPickerCallbacks {
                on_close: Arc::new({
                    let this = cx.entity().clone();
                    move |_: &(), _w, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_picker = PluginPickerState::closed();
                            cx.notify();
                        });
                    }
                }),
                on_select: Arc::new({
                    let this = cx.entity().clone();
                    move |plugin_id: &String, _w, cx| {
                        let plugin_id = plugin_id.clone();
                        let _ = this.update(cx, |this, cx| {
                            if let Some(index) = this.plugin_search_index.as_ref() {
                                let result = compute_filter_result(
                                    index,
                                    &this.plugin_picker.query,
                                    &this.plugin_picker.filters,
                                    &this.plugin_picker_prefs,
                                    std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some(),
                                );
                                if let Some(highlight) = result.indices.iter().position(|&idx| {
                                    index.plugin_at(idx).is_some_and(|p| p.id == plugin_id)
                                }) {
                                    this.plugin_picker.highlighted_index = highlight;
                                }
                            }
                            this.plugin_picker.selected_id = Some(plugin_id);
                            cx.notify();
                        });
                    }
                }),
                on_select_filter: Arc::new({
                    let this = cx.entity().clone();
                    move |filter: &PickerFilter, _w, cx| {
                        let filter = filter.clone();
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_picker.set_sidebar_filter(filter);
                            if let Some(index) = this.plugin_search_index.as_ref() {
                                ensure_default_highlight(
                                    &mut this.plugin_picker,
                                    index,
                                    &this.plugin_picker_prefs,
                                );
                            }
                            cx.notify();
                        });
                    }
                }),
                on_toggle_favorite: Arc::new({
                    let this = cx.entity().clone();
                    move |plugin_id: &String, _w, cx| {
                        let plugin_id = plugin_id.clone();
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_picker_prefs.toggle_favorite(&plugin_id);
                            cx.notify();
                        });
                    }
                }),
                on_pick: Arc::new({
                    let this = cx.entity().clone();
                    move |plugin_id: &String, w, cx| {
                        let plugin_id = plugin_id.clone();
                        let _ = this.update(cx, |this, cx| {
                            if let Some((track_id, insert_index, insert_id)) =
                                this.apply_picked_insert(&plugin_id, cx)
                            {
                                this.open_insert_editor(&track_id, insert_index, &insert_id, w, cx);
                            }
                        });
                    }
                }),
                on_retry_load: Arc::new({
                    let this = cx.entity().clone();
                    move |_: &(), _w, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_catalog.available = None;
                            this.plugin_search_index = None;
                            this.plugin_catalog.status = PluginCatalogStatus::Loading;
                            this.arm_catalog_load(cx);
                            cx.notify();
                        });
                    }
                }),
                on_open_plugin_manager: Arc::new({
                    let this = cx.entity().clone();
                    move |_: &(), window, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_picker = PluginPickerState::closed();
                            let _ = window;
                            this.open_plugin_manager_external_window(None, cx);
                            cx.notify();
                        });
                    }
                }),
                on_rebuild_database: Arc::new({
                    let this = cx.entity().clone();
                    move |_: &(), _w, cx| {
                        let _ = this.update(cx, |this, cx| {
                            // Drop the SQLite file outright; next picker open
                            // reports MissingDatabase, prompting Scan Now.
                            let _ = SpherePluginHost::plugin_db::delete_database_file();
                            this.plugin_catalog.available = None;
                            this.plugin_search_index = None;
                            this.plugin_catalog.status = PluginCatalogStatus::Loading;
                            this.arm_catalog_load(cx);
                            cx.notify();
                        });
                    }
                }),
                on_drop_plugin: Arc::new({
                    let this = cx.entity().clone();
                    move |item, window, cx| {
                        let plugin_id = item.plugin_id.clone();
                        let track_id = match this
                            .read(cx)
                            .timeline
                            .read(cx)
                            .resolve_context_target_from_window_point(window.mouse_position()) {
                            crate::components::timeline::timeline::TimelineContextTarget::TrackLane { track_id, .. }
                            | crate::components::timeline::timeline::TimelineContextTarget::AudioClip { track_id, .. }
                            | crate::components::timeline::timeline::TimelineContextTarget::MidiClip { track_id, .. }
                            | crate::components::timeline::timeline::TimelineContextTarget::TrackHeader(track_id) => track_id,
                            _ => String::new(),
                        };
                        let kind = item.kind;
                        let _ = this.update(cx, |this, cx| {
                            this.apply_dropped_plugin_drag(&plugin_id, &track_id, kind, cx);
                        });
                    }
                }),
            };
            let catalog_status = self.plugin_catalog.status.clone();
            Some(
                plugin_picker_overlay(
                    &self.plugin_picker,
                    self.plugin_search_index.clone(),
                    &self.plugin_picker_prefs,
                    catalog_status,
                    &self.plugin_picker_search_input,
                    self.plugin_picker_search_input.is_focused(window),
                    search_context_callbacks,
                    picker_callbacks,
                    self.plugin_picker_au_error.as_deref(),
                    &self.plugin_picker_scroll,
                )
                .into_any_element(),
            )
        } else {
            None
        };

        self.prune_insert_picker_window(cx);
        self.prune_mixer_window(cx);
        self.prune_midi_editor_window(cx);

        let show_browser = self.panels.browser;
        let show_inspector = self.panels.inspector;
        // A plug-in view is a native child window, outside the GPUI tree, so
        // nothing here would take it off screen when the dock closes or another
        // tab is selected — it would keep floating over whatever is drawn next.
        self.sync_ara_editor_visibility(cx);
        let show_bottom_docked = self.panels.bottom_docked;
        let active_panel = self.active_panel;

        let shortcut_target = cx.entity().clone();
        // Docked MIDI editor — consulted in the key handler so Ctrl+A/C/V/X and
        // Delete route to the piano roll (its own `on_key_down`) when it holds
        // focus, instead of the global timeline clip commands.
        let midi_editor = self.piano_roll.clone();
        let solfege_editor = self.solfege_editor.clone();
        // Physical-keyboard musical typing updates the panel entity *directly*
        // (mirroring the mouse path), never nested inside a `StudioLayout`
        // update. The panel's key handler flushes through the event sink, which
        // re-enters `StudioLayout::update` to route the MIDI; wrapping these in
        // an outer `StudioLayout` lease double-leases and panics (the bug this
        // fixes — mouse clicks worked precisely because they never took that
        // outer lease). Separate clones because each closure is `move`.
        let virtual_keyboard_keydown = self.virtual_keyboard.clone();
        let virtual_keyboard_keyup = self.virtual_keyboard.clone();

        // Keep keyboard focus on our shortcut anchor so transport shortcuts
        // (Space, Enter, L, K, R, Home) reach `capture_key_down` below. GPUI
        // dispatches key events along the focused element's path; when focus is
        // None — OR stale (stuck on a search field whose overlay has since
        // closed, which GPUI still reports as "focused") — the dispatch path
        // falls back to the synthetic root node, which does NOT include this
        // div's `capture_key_down`, so every shortcut silently dies.
        //
        // Reclaim the anchor whenever it isn't focused and no *live* text field
        // is capturing the keyboard. This is intentionally stricter than
        // `window.focused().is_none()`: it also recovers from orphaned focus,
        // while never stealing focus from a field the user is actively typing in.
        // Only treat the docked piano roll as keyboard owner while its tab is
        // actually visible. Once the Editor tab is hidden/closed, GPUI still
        // reports its `FocusHandle` as focused (orphaned), which would otherwise
        // block this reclaim and leave Space/transport shortcuts dead until the
        // user clicks a control. See `docked_midi_editor_visible`.
        let docked_midi_editor_owns_keyboard =
            self.docked_midi_editor_visible() && midi_editor.read(cx).is_focused(window);
        let docked_song_text_editor_owns_keyboard = self.panels.inspector
            && self.right_dock_tab == RightDockTab::LyricEditor
            && self
                .lyric_editor_panel
                .read(cx)
                .is_text_input_focused(window);
        // Seed the shortcut anchor only when the window has no focused element.
        // Do not reclaim focus from buttons or sliders: AccessKit focus actions
        // and Tab navigation must remain stable for assistive technology.
        if window.focused(cx).is_none()
            && !docked_midi_editor_owns_keyboard
            && !docked_song_text_editor_owns_keyboard
            && !self.keyboard_text_capture_live(window)
        {
            self.focus_handle.focus(window, cx);
        }
        if self.command_palette.is_open && !self.command_palette_input.is_focused(window) {
            self.command_palette_input.focus_handle.focus(window, cx);
        }
        let focus_holder = self.focus_handle.clone();
        let focus_holder_on_pointer = self.focus_handle.clone();

        // Systemwide IME bridge: when a main-window text field owns focus, mount
        // the OS composition handler against it (routed to the focused field by
        // `impl EntityInputHandler for StudioLayout`). Coexists with the raw key
        // path; absent when no field is focused, so it never touches shortcuts.
        let ime_bridge = self
            .focused_text_input_handle(window)
            .map(|fh| crate::components::text_input::ime_input_bridge(cx.entity().clone(), fh));
        let shortcut_keydown_target = shortcut_target.clone();

        div()
            .id("futureboard-studio-root")
            .role(Role::Application)
            .aria_label(title.clone())
            // NOTE: `track_focus` deliberately lives on the tiny invisible
            // `focus_holder` child below, NOT on this root. Putting it on
            // the root makes GPUI insert a full-window Normal hitbox
            // (see `should_insert_hitbox` — `tracked_focus_handle.is_some()`
            // triggers it). That hitbox is benign for click dispatch, but
            // on Windows it lands above the chrome's
            // `WindowControlArea::Drag` hitbox in the `mouse_hit_test.ids`
            // vector — which the NCHITTEST callback iterates in
            // window-control-vector order, not z-order — and the OS sees
            // a non-caption hit, refusing to start the window move.
            // Hoisting focus onto a 0×0 child preserves shortcut
            // delivery without adding the full-window hitbox.
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .bg(Colors::surface_base())
            .font(theme::ui_font())
            // Reclaim the studio shortcut anchor before the clicked child handles
            // the pointer. Focusable controls (text fields, piano roll, etc.) take
            // focus again in their own target handler, while non-focusable
            // arrangement clips now reliably route Delete/Cut to the Timeline.
            .capture_any_mouse_down(move |_event, window, cx| {
                focus_holder_on_pointer.focus(window, cx);
            })
            .capture_key_down(move |event, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if !event.is_held {
                    // A fresh press: nothing from the last one is still owed a
                    // key-up. Repeats are left alone — the key-up of a held
                    // Space still belongs to whoever claimed the press.
                    crate::components::transport_key::forget_space_key_up();
                }
                if event.keystroke.key.eq_ignore_ascii_case("tab")
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.function
                {
                    if modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                let handled = shortcut_keydown_target.update(cx, |this, cx| {
                    let handled = this.handle_command_palette_key(event, window, cx)
                        || this.handle_bpm_edit_key(event, window, cx)
                        || this.handle_ts_edit_key(event, window, cx)
                        || this.handle_settings_dialog_key(event, window, cx)
                        || this.handle_add_track_dialog_key(event, window, cx)
                        || this.handle_plugin_picker_key(event, window, cx)
                        || this.handle_automation_picker_key(event, window, cx)
                        || this.handle_project_switcher_key(event, window, cx)
                        || this.handle_inspector_key(event, window, cx)
                        || this.handle_browser_key(event, window, cx);
                    if handled {
                        cx.notify();
                    }
                    handled
                });
                if handled {
                    let key = event.keystroke.key.clone();
                    let _ = shortcut_keydown_target.update(cx, |this, _cx| {
                        this.shortcut_diagnostics.last_key_event = key;
                        this.shortcut_diagnostics.last_key_target = "focused-handler".to_string();
                        this.shortcut_diagnostics.last_key_consumed_by =
                            "pre-global-key-handler".to_string();
                        this.shortcut_diagnostics.focused_widget_kind =
                            this.focused_widget_kind(window);
                        this.shortcut_diagnostics.is_text_editing_context =
                            this.is_text_editing_context(window);
                    });
                    return;
                }
                let song_text_input_focused = shortcut_keydown_target
                    .read(cx)
                    .panels
                    .inspector
                    && shortcut_keydown_target.read(cx).right_dock_tab
                        == RightDockTab::LyricEditor
                    && shortcut_keydown_target
                        .read(cx)
                        .lyric_editor_panel
                        .read(cx)
                        .is_text_input_focused(window);
                let focus = FocusContext {
                    text_input_focused: shortcut_keydown_target
                        .read(cx)
                        .is_text_editing_context(window)
                        || song_text_input_focused,
                };
                let focused_widget_kind = shortcut_keydown_target.read(cx).focused_widget_kind(window);
                let key_for_diag = event.keystroke.key.clone();
                let _ = shortcut_keydown_target.update(cx, |this, _cx| {
                    this.shortcut_diagnostics.last_key_event = key_for_diag;
                    this.shortcut_diagnostics.last_key_target = focused_widget_kind.clone();
                    this.shortcut_diagnostics.last_key_consumed_by = "unhandled".to_string();
                    this.shortcut_diagnostics.focused_widget_kind = focused_widget_kind.clone();
                    this.shortcut_diagnostics.is_text_editing_context = focus.text_input_focused;
                });
                if key_debug() {
                    eprintln!(
                        "[key] key={:?} text_input_focused={} held={} (plugin editor, when active, \
                         consumes keys before this handler)",
                        event.keystroke.key, focus.text_input_focused, event.is_held
                    );
                }
                if focus.text_input_focused && is_text_input_key(event) {
                    let _ = shortcut_keydown_target.update(cx, |this, _cx| {
                        this.shortcut_diagnostics.last_key_consumed_by = "text-input".to_string();
                    });
                    if key_debug() {
                        eprintln!(
                            "[key] ignored key={:?} reason=text-input-focused (typed into field)",
                            event.keystroke.key
                        );
                    }
                    return;
                }
                // Update the panel entity directly — NOT through
                // `shortcut_keydown_target.update` — so the panel's event sink
                // can re-enter `StudioLayout::update` without a double-lease
                // panic. A Ctrl/Cmd/Alt/Fn chord is a shortcut, not a note, so
                // it is passed through to the dispatch path below.
                let mods = event.keystroke.modifiers;
                let command_modifier =
                    mods.control || mods.alt || mods.platform || mods.function;
                let window_id = window.window_handle().window_id();
                let virtual_keyboard_handled =
                    virtual_keyboard_keydown.update(cx, |keyboard, cx| {
                        keyboard.handle_key_down(
                            window_id,
                            event.keystroke.key.as_str(),
                            command_modifier,
                            event.is_held,
                            focus.text_input_focused,
                            cx,
                        )
                    });
                if virtual_keyboard_handled {
                    let _ = shortcut_keydown_target.update(cx, |this, _cx| {
                        this.shortcut_diagnostics.last_key_consumed_by =
                            "virtual-keyboard".to_string();
                    });
                    if components::VirtualKeyboardPanel::should_prevent_default_key(
                        event.keystroke.key.as_str(),
                    ) {
                        window.prevent_default();
                        cx.stop_propagation();
                        if key_debug() {
                            eprintln!(
                                "[VirtualKeyboard] prevented default system key behavior key={}",
                                event.keystroke.key
                            );
                        }
                    }
                    return;
                }
                if event.keystroke.key.as_str() == "escape" {
                    let _ = shortcut_keydown_target.update(cx, |this, cx| {
                        // Cancel an active BPM scrub first, restoring the value
                        // captured at drag start.
                        this.cancel_bpm_drag(cx);
                        let _ = this.timeline.update(cx, |timeline, cx| {
                            timeline.reset_input_state();
                            cx.notify();
                        });
                        this.menu_bar.open_menu_id = None;
                        this.menu_bar.submenu_path.clear();
                        this.command_palette.close();
                        this.overlay.open_popover = None;
                        this.overlay.text_context_menu = None;
                        this.project_switcher.is_open = false;
                        cx.notify();
                    });
                    return;
                }
                let command_id = shortcut_keydown_target.read(cx).shortcut_command_id(event);
                if let Some(command_id) = command_id {
                    // MIDI editor focus gate: when the docked piano roll holds
                    // keyboard focus, the A/C/V/X/Delete family belongs to it.
                    // Skip global dispatch (which would target timeline clips and
                    // could nested-update) and let the event bubble to the piano
                    // roll's `on_key_down`. See PART D/E of the shortcuts task.
                    if is_midi_routable_edit_command(&normalize_command_id(&command_id))
                        && shortcut_keydown_target.read(cx).docked_midi_editor_visible()
                        && midi_editor.read(cx).is_focused(window)
                    {
                        if edit_command_debug() {
                            eprintln!(
                                "[edit-command] command={command_id} target=MidiEditor \
                                 reason=focus-passthrough (handled by piano roll)"
                            );
                        }
                        return;
                    }
                    // Same gate for the Solfege Pitch tab, which replaces the
                    // piano roll in the dock for a Solfege clip and owns Delete
                    // for its pitch points.
                    if is_midi_routable_edit_command(&normalize_command_id(&command_id))
                        && shortcut_keydown_target.read(cx).docked_midi_editor_visible()
                        && solfege_editor.read(cx).pitch_grid_is_focused(window)
                    {
                        if edit_command_debug() {
                            eprintln!(
                                "[edit-command] command={command_id} target=SolfegePitch \
                                 reason=focus-passthrough (handled by pitch editor)"
                            );
                        }
                        return;
                    }
                    // Transport shortcuts go through the same dispatcher as the
                    // chrome Play button (transport:play-pause → PlayPause), so
                    // Spacebar and the button are always one command. Only the
                    // focus gate differs between them.
                    let is_transport = transport_command_from_id(&command_id).is_some();
                    if is_transport && !should_handle_global_transport_shortcut(&focus) {
                        if key_debug() {
                            eprintln!(
                                "[key] ignored command={command_id} reason=global-transport-shortcut-suppressed"
                            );
                        }
                        return;
                    }
                    if is_transport {
                        // The transport owns this key, so nothing downstream may
                        // also act on it. Both halves matter: `prevent_default`
                        // and `stop_propagation` keep the key-down off any
                        // focused control, and the claim does the same for the
                        // key-up, which GPUI would otherwise turn into a click on
                        // whatever the mouse focused last — the Play or Record
                        // button, for a transport shortcut.
                        if event.keystroke.key.eq_ignore_ascii_case("space") {
                            crate::components::transport_key::claim_space_key_up();
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                        // Auto-repeat is one press, one command: the transport
                        // commands are toggles and jumps, and replaying them at
                        // the repeat rate is how a held key ends up starting
                        // playback it just stopped. Matches the Win32 claim path,
                        // which drops repeats on the same grounds.
                        if event.is_held {
                            if key_debug() {
                                eprintln!(
                                    "[key] ignored command={command_id} reason=auto-repeat"
                                );
                            }
                            return;
                        }
                    }
                    if is_tap_tempo_command(&normalize_command_id(&command_id))
                        && shortcut_keydown_target
                            .read(cx)
                            .tap_tempo_shortcut_blocked(window)
                    {
                        if key_debug() {
                            eprintln!(
                                "[key] ignored command={command_id} reason=tap-tempo-shortcut-suppressed"
                            );
                        }
                        return;
                    }
                    if command_id == "transport:play-pause"
                        && event.keystroke.key.eq_ignore_ascii_case("space")
                    {
                        eprintln!("[KeyCommand] Spacebar -> TransportTogglePlay");
                        let _ = shortcut_keydown_target.update(cx, |this, _cx| {
                            this.shortcut_diagnostics.transport_toggle_shortcut_count = this
                                .shortcut_diagnostics
                                .transport_toggle_shortcut_count
                                .saturating_add(1);
                            this.shortcut_diagnostics.last_key_consumed_by =
                                "global-transport-shortcut".to_string();
                            crate::perf::count(
                                "transport_toggle_shortcut_count",
                                this.shortcut_diagnostics.transport_toggle_shortcut_count,
                            );
                        });
                    }
                    if key_debug() {
                        eprintln!("[key] dispatched command={command_id}");
                    }
                    let _ = shortcut_keydown_target.update(cx, |this, cx| {
                        this.dispatch_command_id_from_bounds(&command_id, Some(window.bounds()), cx);
                        cx.notify();
                    });
                } else if event.keystroke.key.eq_ignore_ascii_case("space")
                    && !event.is_held
                    && !focus.text_input_focused
                    && !mods.control
                    && !mods.alt
                    && !mods.platform
                    && !mods.function
                    && should_handle_global_transport_shortcut(&focus)
                {
                    if key_debug() {
                        eprintln!(
                            "[key] dispatched command=transport:play-pause reason=spacebar-fallback"
                        );
                    }
                    crate::components::transport_key::claim_space_key_up();
                    window.prevent_default();
                    cx.stop_propagation();
                    let _ = shortcut_keydown_target.update(cx, |this, cx| {
                        this.shortcut_diagnostics.transport_toggle_shortcut_count = this
                            .shortcut_diagnostics
                            .transport_toggle_shortcut_count
                            .saturating_add(1);
                        this.shortcut_diagnostics.last_key_consumed_by =
                            "spacebar-fallback".to_string();
                        crate::perf::count(
                            "transport_toggle_shortcut_count",
                            this.shortcut_diagnostics.transport_toggle_shortcut_count,
                        );
                        this.dispatch_command_id_from_bounds(
                            "transport:play-pause",
                            Some(window.bounds()),
                            cx,
                        );
                        cx.notify();
                    });
                }
            })
            .capture_key_up({
                // Update the panel entity directly (see the key-down note): the
                // NoteOff flush re-enters `StudioLayout::update` via the sink, so
                // an outer `StudioLayout` lease here would double-lease and panic.
                let virtual_keyboard = virtual_keyboard_keyup.clone();
                move |event, window: &mut Window, cx| {
                    let window_id = window.window_handle().window_id();
                    let handled = virtual_keyboard.update(cx, |keyboard, cx| {
                        keyboard.handle_key_up(window_id, event.keystroke.key.as_str(), cx)
                    });
                    if handled {
                        if components::VirtualKeyboardPanel::should_prevent_default_key(
                            event.keystroke.key.as_str(),
                        ) {
                            window.prevent_default();
                            cx.stop_propagation();
                        }
                        return;
                    }
                    // The key-down routed this Space to the transport, so the
                    // key-up is spoken for as well. Left alone, GPUI fires the
                    // focused element's click listeners on it, and a Play or
                    // Record button focused by an earlier mouse click is pressed
                    // again the instant the key comes back up — Space pausing and
                    // then immediately restarting from one press.
                    if crate::components::transport_key::take_space_key_up_claim() {
                        if key_debug() {
                            eprintln!("[key] swallowed key-up key=space reason=transport-key");
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }
            })
            // Invisible focus anchor. 0×0 means no visible footprint and
            // an effectively unreachable hitbox; `track_focus` only needs
            // it to register the focus handle. The root's
            // `capture_key_down` still fires for any key while this
            // descendant is focused (capture phase: root → focused).
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&focus_holder))
            // Non-visual, non-interactive OS-IME bridge for the focused field.
            .children(ime_bridge)
            // Cached: the chrome carries the bar.beat readout, so the
            // transport poll repaints it directly instead of notifying the
            // shell. See `shell_regions`.
            .child(
                gpui::AnyView::from(self.app_chrome.clone())
                    .cached(shell_regions::AppChromeView::cached_style()),
            )
            .child({
                let mut main_row = div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0();
                if show_browser {
                    // Cached: the browser only rebuilds when the shell
                    // itself changes, not on every playhead/meter frame.
                    main_row = main_row.child(
                        gpui::AnyView::from(self.browser_sidebar.clone())
                            .cached(shell_regions::BrowserSidebarView::cached_style()),
                    );
                }
                main_row = main_row.child({
                    let owner = cx.entity().clone();
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .on_mouse_down(gpui::MouseButton::Left, move |_event, _window, cx| {
                            let _ = owner.update(cx, |layout, cx| {
                                layout.set_active_panel(WorkspaceActivePanel::Arrangement, cx);
                            });
                        })
                        .child(self.timeline.clone())
                });
                if show_inspector {
                    // Cached: the dock rebuilds when the shell changes, not
                    // when the playhead or a meter moves.
                    main_row = main_row.child(
                        gpui::AnyView::from(self.right_dock.clone())
                            .cached(shell_regions::RightDockView::cached_style()),
                    );
                }
                main_row
            })
            .children(if show_bottom_docked {
                let _s = crate::perf::PerfScope::enter("BottomPanel");
                // Cached like the other shell regions. The docked editors
                // (piano roll, mixer, effect editor) live under here, so a
                // playhead or meter frame must not rebuild them.
                Some(
                    gpui::AnyView::from(self.bottom_panel_shell.clone())
                        .cached(shell_regions::bottom_panel_cached_style(
                            self.bottom_panel_state().height_px,
                        ))
                        .into_any_element(),
                )
            } else {
                None
            })
            .child({
                let _s = crate::perf::PerfScope::enter("StatusBar");
                self.status_bar.clone()
            })
            // Click-outside dismissal for the Inspector colour picker. The
            // popover is deferred at a higher priority and occludes its own
            // clicks, so only a genuine outside click reaches this layer.
            .children(inspector_color_open.then(|| {
                let owner = cx.entity().clone();
                crate::components::form::select_dismiss_backdrop(std::sync::Arc::new(
                    move |_: &(), window: &mut Window, cx: &mut gpui::App| {
                        let _ = owner.update(cx, |this, cx| {
                            this.close_inspector_color_picker(window, cx);
                        });
                    },
                ))
            }))
            // Dropdown overlay — rendered last so it sits above every other
            // panel. The dropdown's own backdrop captures click-outside.
            .children(dropdown_overlay)
            .children(popover_overlay)
            .children(inspector_routing_combo_overlay)
            // Add Track moved to external window.
            .children(settings_overlay)
            .children(plugin_picker_overlay_el)
            .children(text_context_overlay)
            .children(virtual_keyboard_overlay)
            .children({
                if debug_active_panel_enabled() {
                    Some(
                        div()
                            .absolute()
                            .right(px(12.0))
                            .bottom(px(28.0))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(crate::theme::radius::CONTROL))
                            .border(px(1.0))
                            .border_color(Colors::panel_border_focused())
                            .bg(Colors::surface_canvas())
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::tab_text_active())
                            .child(format!("active_panel = {}", active_panel.label()))
                            .into_any_element(),
                    )
                } else {
                    None
                }
            })
            .children({
                let show_perf_overlay = self.settings.read(cx).current.performance.show_performance_overlay
                    || crate::perf::perf_hud_enabled();
                // Scope aggregation costs a timestamp per instrumented scope, so
                // it follows the overlay: on while the user is looking at it,
                // off (and cleared) the moment it closes.
                crate::perf::set_collection_requested(show_perf_overlay);
                if show_perf_overlay {
                    let snapshot = self.performance_overlay_snapshot(reason_static);
                    Some(components::performance_overlay(&snapshot).into_any_element())
                } else {
                    None
                }
            })
    }
}

/// Dock tab strip height. The tabs sit flush against the bottom divider so the
/// selected indicator can share that edge.
const RIGHT_DOCK_TAB_STRIP_HEIGHT: f32 = crate::theme::size::COMFORTABLE;
/// Tab height inside the strip.
const RIGHT_DOCK_TAB_HEIGHT: f32 = RIGHT_DOCK_TAB_STRIP_HEIGHT;
/// Tab glyph. One step below the section icon so the label leads.
const RIGHT_DOCK_TAB_ICON: f32 = 12.0;

pub(super) fn right_dock_tab_bar(
    active: RightDockTab,
    owner: Entity<StudioLayout>,
) -> impl IntoElement {
    let popout_kind = match active {
        RightDockTab::Inspector | RightDockTab::Solfege => None,
        RightDockTab::ChordDisplay => Some(components::SongTextPanelKind::ChordDisplay),
        RightDockTab::LyricDisplay => Some(components::SongTextPanelKind::LyricDisplay),
        RightDockTab::LyricEditor => Some(components::SongTextPanelKind::LyricEditor),
    };
    let mut row = div()
        .id("right-dock-tabs")
        .role(Role::TabList)
        .aria_label("Right panel")
        .h(px(RIGHT_DOCK_TAB_STRIP_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px(px(crate::theme::space::TIGHT))
        .gap(px(crate::theme::space::HAIR))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel());
    for tab in [
        RightDockTab::Inspector,
        RightDockTab::Solfege,
        RightDockTab::ChordDisplay,
        RightDockTab::LyricDisplay,
        RightDockTab::LyricEditor,
    ] {
        row = row.child(right_dock_tab_button(tab, active, owner.clone()));
    }
    row.child(div().flex_1()).children(popout_kind.map(|kind| {
        let target = owner.clone();
        components::icon_button(
            Some(crate::assets::ICON_MAXIMIZE_PATH),
            "Open in a separate window",
            px(20.0),
            px(20.0),
            px(11.0),
            Colors::text_muted(),
        )
        .id("right-dock-popout")
        .role(Role::Button)
        .aria_label("Open panel in a separate window")
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(Colors::surface_control_hover()))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |_, window, cx| {
            let bounds = Some(window.bounds());
            let _ = target.update(cx, |layout, cx| {
                layout.open_song_text_external_window(kind, bounds, cx);
            });
        })
    }))
}

fn right_dock_tab_button(
    tab: RightDockTab,
    active: RightDockTab,
    owner: Entity<StudioLayout>,
) -> impl IntoElement {
    let selected = tab == active;
    let (icon, label) = match tab {
        RightDockTab::Inspector => (crate::assets::ICON_SLIDERS_HORIZONTAL_PATH, "Inspect"),
        RightDockTab::Solfege => (crate::assets::ICON_AUDIO_LINES_PATH, "Solfege"),
        RightDockTab::ChordDisplay => (crate::assets::ICON_MUSIC_PATH, "Chords"),
        RightDockTab::LyricDisplay => (crate::assets::ICON_NEWSPAPER_PATH, "Lyrics"),
        RightDockTab::LyricEditor => (crate::assets::ICON_PENCIL_PATH, "Edit"),
    };
    // A tab is a text target with an indicator, not a button-shaped block: the
    // five of them sit in a 4 px strip, and five filled pills there read as a
    // toolbar rather than as one control that picks a view. Selection is carried
    // on two channels — accent text *and* the underline — so it survives a theme
    // where the accent is low-contrast.
    //
    // Only the selected tab spells its name. Five labelled tabs plus the pop-out
    // button need about 320 px and the dock is `INSPECTOR_WIDTH` (292), so the
    // last one was pushed off the panel edge. Collapsing the rest to their glyph
    // both fits and makes the current view unmistakable; the name stays
    // reachable through the tooltip and the accessible label.
    let text = if selected {
        Colors::accent_primary()
    } else {
        Colors::tab_text_muted()
    };
    // Hover is attached unconditionally and resolves to the tab's own colour
    // while it is selected, so the selected tab simply has nothing to lift to.
    let hover_text = if selected {
        text
    } else {
        Colors::text_primary()
    };
    div()
        .id(("right-dock-tab", tab as u32))
        .role(Role::Tab)
        .aria_label(label)
        .aria_selected(selected)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| {
            style.shadow(crate::theme::elevation::focus_ring(
                Colors::state_focus_ring(),
            ))
        })
        .relative()
        .h(px(RIGHT_DOCK_TAB_HEIGHT))
        .px(px(crate::theme::space::SNUG))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(crate::theme::space::TIGHT))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .text_size(px(crate::theme::typography::UI_XS))
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::MEDIUM
        })
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.text_color(hover_text))
        .on_click(move |_, _, cx| {
            let _ = owner.update(cx, |layout, cx| {
                layout.panels.inspector = true;
                layout.right_dock_tab = tab;
                layout.set_active_panel(tab.active_panel(), cx);
            });
        })
        .tooltip(components::fb_tooltip(label))
        .child(
            gpui::svg()
                .path(icon)
                .w(px(RIGHT_DOCK_TAB_ICON))
                .h(px(RIGHT_DOCK_TAB_ICON))
                .flex_shrink_0()
                .text_color(text),
        )
        .children(selected.then(|| div().flex_shrink_0().child(label)))
        // Indicator sits on the strip's bottom edge, flush with the divider, so
        // selecting a tab never reflows the row.
        .children(selected.then(|| {
            div()
                .absolute()
                .left(px(crate::theme::space::SNUG))
                .right(px(crate::theme::space::SNUG))
                .bottom_0()
                .h(px(2.0))
                .rounded(px(crate::theme::radius::MICRO))
                .bg(Colors::accent_primary())
        }))
}

#[cfg(target_os = "windows")]
fn publish_studio_main_hwnd(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(handle) = HasWindowHandle::window_handle(window) {
        if let RawWindowHandle::Win32(w) = handle.as_raw() {
            SpherePluginHost::plugin_host_main_window::set_main_window_hwnd(w.hwnd.get());
        }
    }
}

/// Publish the studio window's X11 id as the plugin-host owner/DPI reference.
/// Matches `studio_native_hwnd` in plugin_ops: X11/XWayland only; pure Wayland
/// leaves the published handle unset.
#[cfg(not(target_os = "windows"))]
fn publish_studio_main_hwnd(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let id = match handle.as_raw() {
        RawWindowHandle::Xcb(w) => Some(w.window.get() as isize),
        RawWindowHandle::Xlib(w) => Some(w.window as isize),
        _ => None,
    };
    if let Some(id) = id {
        SpherePluginHost::plugin_host_main_window::set_main_window_hwnd(id);
    }
}

/// `FUTUREBOARD_DEBUG_ACTIVE_PANEL=1` — draw the active-panel debug pill.
/// Cached so the root render doesn't hit the OS env store every frame
/// (matches the `OnceLock` idiom used for every other debug flag).
fn debug_active_panel_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_DEBUG_ACTIVE_PANEL").is_some())
}
