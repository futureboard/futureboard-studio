//! Shell regions that render as their own cached views.
//!
//! GPUI redraws a window by re-rendering its root view: `Window::draw` calls
//! `draw_roots` unconditionally, so *any* `cx.notify()` anywhere in the studio
//! window rebuilds, re-lays-out and repaints the whole `StudioLayout` tree.
//! Splitting a hot widget (playhead, meters) into its own entity does not
//! change that on its own — `Window::mark_view_dirty` walks the notified
//! view's ancestors, so a nested notify dirties the shell too.
//!
//! What *does* isolate work is `AnyView::cached`: a cached view whose entity is
//! not in `dirty_views` reuses last frame's prepaint and paint ranges
//! (hitboxes, dispatch nodes, mouse listeners, scene primitives) instead of
//! re-rendering. Caching only helps for views that are **siblings** of the
//! notify source, which is exactly the shape of the studio shell: the playhead
//! and the meters live under the timeline, while the browser, the right dock
//! and the chrome sit beside it.
//!
//! Each region therefore:
//!
//! 1. owns nothing — it reads the same `StudioLayout` state the shell did, so
//!    there is no second copy of the browser tree or the selection to keep in
//!    sync;
//! 2. observes `StudioLayout` and notifies itself, so every shell state change
//!    still repaints it exactly as before caching;
//! 3. exposes the root style the shell lays it out with while cached, because
//!    a cached view's `request_layout` uses that style instead of rendering.
//!
//! Two rules follow from (3), and both have already cost a visible regression:
//!
//! * **The cached style must reproduce the element root's outer box.** The
//!   chrome's root is a *column* of a titlebar and a transport bar; a cached
//!   height covering only the titlebar dropped the transport row out of the
//!   window.
//! * **The element root must size itself from available space alone.**
//!   `AnyView::cached` calls `layout_as_root`, so the element has no containing
//!   block and no flex context: `absolute; inset: 0` measures zero and `flex_1`
//!   means nothing. Wrap such an element in a `size_full` div (see
//!   `Timeline::render_arrangement_surface`).

use super::*;

use crate::components::timeline::timeline_state::TrackState;

/// The file-browser sidebar as its own view.
///
/// Rebuilding it meant re-creating eight callbacks per frame, on every frame
/// the transport moved.
pub struct BrowserSidebarView {
    owner: Entity<StudioLayout>,
    /// Focus target for the tree itself.
    ///
    /// It lives here rather than on `StudioLayout` because the sidebar is the
    /// only view that renders it, and `StudioLayout` reaches it through
    /// [`BrowserSidebarView::tree_focus`] when routing keys. Before this the
    /// browser had no focus target at all, so arrows, Enter and type-ahead
    /// only worked while the *search field* was focused — clicking a row
    /// killed the keyboard.
    pub tree_focus: gpui::FocusHandle,
    /// Keeps the region repainting on every shell change, exactly as it did
    /// when the shell rendered it inline.
    _observers: Vec<gpui::Subscription>,
}

impl BrowserSidebarView {
    pub fn new(
        owner: Entity<StudioLayout>,
        settings: &gpui::Entity<crate::settings::SettingsModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _observers = vec![
            cx.observe(&owner, |_, _, cx| cx.notify()),
            cx.observe(settings, |_, _, cx| cx.notify()),
        ];
        Self {
            owner,
            tree_focus: cx.focus_handle(),
            _observers,
        }
    }

    /// Root style used while the view is cached. Must match the wrapper
    /// `render_browser_sidebar` builds, or a cached frame lays out differently
    /// from a rendered one.
    pub fn cached_style() -> gpui::StyleRefinement {
        gpui::StyleRefinement::default()
            .w(px(components::SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
    }
}

impl Render for BrowserSidebarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `StudioLayout` is not leased here: the shell's `render` returned
        // before its element tree was laid out, and this view is rendered from
        // that tree's prepaint.
        let owner = self.owner.clone();
        let tree_focus = self.tree_focus.clone();
        owner.update(cx, |layout, cx| {
            layout.render_browser_sidebar(&tree_focus, window, cx)
        })
    }
}

impl StudioLayout {
    /// Build the browser sidebar. Moved out of `StudioLayout::render` so the
    /// cost lands only on frames where the shell actually changed.
    pub(super) fn render_browser_sidebar(
        &mut self,
        tree_focus: &gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let _s = crate::perf::PerfScope::enter("Sidebar");
        let i18n = crate::i18n::I18n::from_app(cx);
        self.browser_search_input.placeholder = Some(i18n.tr("search.browser.placeholder"));
        let active_panel = self.active_panel;

        // ── File browser callbacks ──────────────────────────────────────
        let on_browser_search_context: std::sync::Arc<
            dyn Fn(&(f32, f32), &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |(x, y): &(f32, f32), _w, cx| {
                let x = *x;
                let y = *y;
                let _ = this.update(cx, |this, cx| {
                    this.menu_bar.open_menu_id = None;
                    this.menu_bar.submenu_path.clear();
                    this.project_switcher.is_open = false;
                    this.overlay.text_context_menu = Some(TextContextMenu {
                        target: TextMenuTarget::BrowserSearch,
                        x,
                        y,
                    });
                    cx.notify();
                });
            })
        };
        let browser_search_mouse_callbacks = crate::components::text_input::bind_mouse_selection(
            cx.entity().clone(),
            |layout: &mut StudioLayout| &mut layout.browser_search_input,
        );
        let browser_search_callbacks = TextInputCallbacks {
            on_mouse_down_out: None,
            on_context_command: None,
            on_context_menu: Some(on_browser_search_context),
            on_mouse: browser_search_mouse_callbacks.on_mouse,
        };

        let on_browser_toggle: std::sync::Arc<
            dyn Fn(&(String, Option<PathBuf>), &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |(id, path): &(String, Option<PathBuf>), _w, cx| {
                let id = id.clone();
                let path = path.clone();
                let _ = this.update(cx, |this, cx| {
                    let expanded = this.file_browser.toggle_node(&id, path.as_deref());
                    if expanded {
                        // Drain any newly-expanded paths whose contents
                        // haven't been indexed yet and kick off a
                        // background load for each.
                        let pending = this.file_browser.paths_needing_load();
                        for p in pending {
                            this.file_browser.mark_loading(p.clone());
                            this.spawn_directory_load(cx, p);
                        }
                    }
                    cx.notify();
                });
            })
        };
        // Mouse selection and keyboard selection go through the same operation
        // (`apply_browser_selection`), so arrowing to a file can no longer give
        // a different result from clicking it.
        let on_browser_select: std::sync::Arc<
            dyn Fn(&PathBuf, &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |path: &PathBuf, _w, cx| {
                let path = path.clone();
                this.update(cx, |this, cx| {
                    this.apply_browser_selection(path, cx);
                    cx.notify();
                });
            })
        };
        // Breadcrumb jump: expand the ancestors so the target lands on a row
        // that is actually on screen, then select it normally.
        let on_browser_reveal: std::sync::Arc<
            dyn Fn(&PathBuf, &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |path: &PathBuf, _w, cx| {
                let path = path.clone();
                this.update(cx, |this, cx| {
                    this.reveal_browser_path(path, cx);
                    cx.notify();
                });
            })
        };
        // Double-click on an audio file imports it onto the timeline using the
        // existing waveform-cache + import_audio_at path.
        let on_browser_activate: std::sync::Arc<
            dyn Fn(&PathBuf, &mut Window, &mut gpui::App) + 'static,
        > = {
            let timeline = self.timeline.clone();
            let layout = cx.entity().clone();
            std::sync::Arc::new(move |path: &PathBuf, _w, cx| {
                // Filter on extension before mutating timeline state so
                // double-clicking a non-audio file (e.g. .txt, .png) does
                // not create a phantom clip with the 8-bar fallback
                // duration that never resolves to real metadata.
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_default();
                if !is_supported_audio_ext(&ext) {
                    eprintln!(
                        "[import] ignoring non-audio activation: ext='{}' path={}",
                        ext,
                        path.display()
                    );
                    return;
                }

                let path = path.clone();
                let path_for_decode = path.clone();
                let timeline_for_decode = timeline.clone();
                timeline.update(cx, |t, cx| {
                    let path_key = path.to_string_lossy().to_string();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "Imported Audio".to_string());
                    t.state
                        .import_audio_to_selected_or_new_track(path_key, name);
                    cx.notify();
                });
                let _ = layout.update(cx, |this, cx| {
                    this.mark_dirty();
                    this.mark_engine_media_dirty();
                    this.schedule_audio_project_sync(cx, false, "timeline_audio_import");
                });
                let path_key = path_for_decode.to_string_lossy().to_string();
                let _ = layout.update(cx, move |this, cx| {
                    this.spawn_timeline_audio_import_jobs(
                        cx,
                        timeline_for_decode,
                        path_for_decode,
                        path_key,
                    );
                });
            })
        };
        let on_browser_context: std::sync::Arc<
            dyn Fn(&(Option<PathBuf>, f32, f32), &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(
                move |(path, x, y): &(Option<PathBuf>, f32, f32), window, cx| {
                    let path = path.clone();
                    let x = *x;
                    let y = *y;
                    let window_id = window.window_handle().window_id();
                    StudioLayout::defer_update(&this, cx, move |this, cx| {
                        this.try_open_context_menu(
                            ContextMenuRequest::new(
                                window_id,
                                x,
                                y,
                                ContextMenuTarget::Extended(ContextTarget::Browser(path)),
                            ),
                            cx,
                        );
                    });
                },
            )
        };

        // Toolbar: collapse every expanded folder in one click.
        let on_browser_collapse_all: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static> = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |_w, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.file_browser.collapse_all();
                    cx.notify();
                });
            })
        };
        // Toolbar: drop cached listings for expanded folders and re-scan them.
        let on_browser_rescan: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static> = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |_w, cx| {
                let _ = this.update(cx, |this, cx| {
                    let paths = this.file_browser.invalidate_expanded();
                    for p in paths {
                        this.file_browser.mark_loading(p.clone());
                        this.spawn_directory_load(cx, p);
                    }
                    cx.notify();
                });
            })
        };
        // Auto-preview: switching it off also silences whatever is playing, so
        // the toggle's state and the audible output can never disagree.
        let on_browser_toggle_preview: std::sync::Arc<
            dyn Fn(&mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |_w, cx| {
                let _ = this.update(cx, |this, cx| {
                    if !this.file_browser.toggle_preview_enabled() {
                        this.stop_browser_audition();
                    }
                    cx.notify();
                });
            })
        };
        let on_browser_stop_preview: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static> = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |_w, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.stop_browser_audition();
                    cx.notify();
                });
            })
        };

        let browser_callbacks = components::BrowserCallbacks {
            on_toggle: on_browser_toggle,
            on_select: on_browser_select,
            on_reveal: on_browser_reveal,
            on_activate_file: on_browser_activate,
            on_context_menu: on_browser_context,
            on_collapse_all: on_browser_collapse_all,
            on_rescan: on_browser_rescan,
            on_toggle_preview: on_browser_toggle_preview,
            on_stop_preview: on_browser_stop_preview,
        };

        let browser_scroll = self.browser_scroll.clone();
        let search_focused = self.browser_search_input.is_focused(window);

        let owner = cx.entity().clone();
        div()
            .w(px(components::SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .on_mouse_down(gpui::MouseButton::Left, move |_event, _window, cx| {
                let _ = owner.update(cx, |layout, cx| {
                    layout.set_active_panel(WorkspaceActivePanel::Browser, cx);
                });
            })
            // `&self.file_browser`, not a clone: `FileBrowserState` owns the
            // whole directory index, and deep-copying it once per render was
            // the single largest allocation in this region.
            .child(components::sidebar(
                &self.file_browser,
                browser_scroll,
                tree_focus,
                &self.browser_search_input,
                search_focused,
                active_panel == WorkspaceActivePanel::Browser,
                browser_search_callbacks,
                browser_callbacks,
                i18n,
            ))
            .into_any_element()
    }
}

/// The right dock (Inspector / Solfège / song-text tabs) as its own view.
///
/// It reads the same timeline snapshot the shell used to build inline, so
/// nothing is duplicated — only *when* the work happens changes.
pub struct RightDockView {
    owner: Entity<StudioLayout>,
    /// The dock renders from the shell *and* from the timeline (tracks,
    /// selection, routing) and the solfege editor. A cached view only repaints
    /// when it is notified, so it has to hear every source it reads.
    _observers: Vec<gpui::Subscription>,
}

impl RightDockView {
    pub fn new(
        owner: Entity<StudioLayout>,
        timeline: &Entity<components::timeline::Timeline>,
        solfege_editor: &Entity<crate::components::SolfegeEditorPanel>,
        settings: &gpui::Entity<crate::settings::SettingsModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _observers = vec![
            cx.observe(&owner, |_, _, cx| cx.notify()),
            cx.observe(timeline, |_, _, cx| cx.notify()),
            cx.observe(solfege_editor, |_, _, cx| cx.notify()),
            cx.observe(settings, |_, _, cx| cx.notify()),
        ];
        Self { owner, _observers }
    }

    /// Layout-affecting half of the wrapper `render_right_dock` builds. Paint
    /// (border colour, background) is replayed from the cached scene, so only
    /// the box model has to match.
    pub fn cached_style() -> gpui::StyleRefinement {
        gpui::StyleRefinement::default()
            .w(px(crate::components::panel::INSPECTOR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .min_h_0()
            .border_l(px(1.0))
    }
}

impl Render for RightDockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = self.owner.clone();
        owner.update(cx, |layout, cx| layout.render_right_dock(window, cx))
    }
}

impl StudioLayout {
    /// Render-only track snapshot plus the current selection.
    ///
    /// Do not clone every clip here: MIDI clips can contain tens of thousands
    /// of notes/controllers, and a mixer selection clears clip selection
    /// anyway. The Inspector only needs the selected clip, so add that one back
    /// to the otherwise clip-free track snapshot.
    pub(super) fn inspector_track_snapshot(
        &self,
        cx: &gpui::App,
    ) -> (Vec<TrackState>, Option<String>, Option<String>, f64) {
        let t = self.timeline.read(cx);
        let selected_clip_id = t.state.selection.selected_clip_ids.first().cloned();
        let selected_clip = selected_clip_id.as_deref().and_then(|clip_id| {
            t.state.tracks.iter().find_map(|track| {
                track
                    .clips
                    .iter()
                    .find(|clip| clip.id == clip_id)
                    .map(|clip| (track.id.clone(), clip.clone()))
            })
        });
        let mut tracks: Vec<_> = t
            .state
            .tracks
            .iter()
            .map(|track| {
                let mut cloned = super::mixer_ops::clone_track_for_mixer(track);
                let display_volume = t.state.display_track_volume(track);
                cloned.volume = display_volume;
                cloned.volume_effective = display_volume;
                cloned
            })
            .collect();
        if let Some((track_id, clip)) = selected_clip {
            if let Some(track) = tracks.iter_mut().find(|track| track.id == track_id) {
                track.clips.push(clip);
            }
        }
        (
            tracks,
            t.state.selection.selected_track_id.clone(),
            selected_clip_id,
            t.state.bpm as f64,
        )
    }

    /// The Inspector's routing combo. It anchors over the whole window rather
    /// than inside the dock, so the shell keeps rendering it while the dock
    /// itself is cached. Everything it needs is gathered here, and only while a
    /// combo is actually open — the shell used to clone the track list and read
    /// the port inventories for it on every frame.
    pub(super) fn inspector_routing_combo_overlay_element(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (Some(combo), Some(anchor)) = (
            self.overlay.inspector_routing_combo,
            self.overlay.inspector_routing_combo_anchor,
        ) else {
            return None;
        };
        let (tracks, selected_track_id, _selected_clip_id, _project_bpm) =
            self.inspector_track_snapshot(cx);
        let inspector_callbacks = self.build_inspector_callbacks(cx.entity().clone());
        // The audio-input combo lists logical connections, so no device
        // enumeration happens for it at all. The port inventory is read only
        // while the combo is open, and only to describe the selected bus.
        let audio_input_ports = if self.overlay.inspector_routing_combo
            == Some(crate::components::panel::InspectorRoutingCombo::AudioInput)
        {
            crate::audio_connections::current_available_ports()
        } else {
            crate::audio_connections::AvailablePorts::default()
        };
        let audio_output_buses: Vec<(String, String)> = if self.overlay.inspector_routing_combo
            == Some(crate::components::panel::InspectorRoutingCombo::AudioOutput)
        {
            tracks
                .iter()
                .filter(|track| {
                    crate::components::timeline::timeline_state::is_project_routing_track(track)
                })
                .map(|track| (track.id.clone(), track.name.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let audio_output_device = if self.overlay.inspector_routing_combo
            == Some(crate::components::panel::InspectorRoutingCombo::AudioOutput)
        {
            self.selected_output_device_channels(cx)
        } else {
            None
        };
        let instrument_targets: Vec<(String, String)> = if self.overlay.inspector_routing_combo
            == Some(crate::components::panel::InspectorRoutingCombo::MidiOut)
        {
            tracks
                .iter()
                .filter(|track| {
                    track.track_type
                        == crate::components::timeline::timeline_state::TrackType::Instrument
                })
                .map(|track| (track.id.clone(), track.name.clone()))
                .collect()
        } else {
            Vec::new()
        };
        // Real MIDI hardware/virtual ports, enabled in Preferences → MIDI —
        // the same cached registry Settings renders from (`device_registry`),
        // not a mocked/empty placeholder. Only enabled ports are offered as
        // routing targets, matching the Preferences toggle the user set.
        let (detected_midi_inputs, detected_midi_outputs): (Vec<String>, Vec<String>) = if matches!(
            self.overlay.inspector_routing_combo,
            Some(crate::components::panel::InspectorRoutingCombo::MidiInput)
                | Some(crate::components::panel::InspectorRoutingCombo::MidiOut)
        ) {
            let saved = self.settings.read(cx).current.hardware.midi.devices.clone();
            let detected = crate::device_registry::cached_midi_devices();
            let resolved = sphere_midi_service::resolve_midi_devices(&saved, &detected);
            let inputs = resolved
                .iter()
                .filter(|d| {
                    d.enabled
                        && matches!(
                            d.direction,
                            crate::settings::MidiDeviceDirection::Input
                                | crate::settings::MidiDeviceDirection::InputOutput
                        )
                })
                .map(|d| d.name.clone())
                .collect();
            let outputs = resolved
                .iter()
                .filter(|d| {
                    d.enabled
                        && matches!(
                            d.direction,
                            crate::settings::MidiDeviceDirection::Output
                                | crate::settings::MidiDeviceDirection::InputOutput
                        )
                })
                .map(|d| d.name.clone())
                .collect();
            (inputs, outputs)
        } else {
            (Vec::new(), Vec::new())
        };
        let track = tracks
            .iter()
            .find(|t| Some(t.id.as_str()) == selected_track_id.as_deref())?;
        let combo_audio_connections = self.timeline.read(cx).state.audio_connections.clone();
        let close = Arc::new({
            let this = cx.entity().clone();
            move |cx: &mut gpui::App| {
                let _ = this.update(cx, |layout, cx| {
                    layout.overlay.inspector_routing_combo = None;
                    layout.overlay.inspector_routing_combo_anchor = None;
                    cx.notify();
                });
            }
        });
        Some(
            crate::components::panel::inspector_routing_combo_overlay(
                track,
                &combo_audio_connections,
                combo,
                anchor,
                window,
                &inspector_callbacks,
                close,
                audio_input_ports,
                audio_output_buses,
                audio_output_device,
                instrument_targets,
                detected_midi_inputs,
                detected_midi_outputs,
            )
            .into_any_element(),
        )
    }

    /// Build the right dock. Moved out of `StudioLayout::render` so the track
    /// snapshot, the Inspector callbacks and the panel tree stop being rebuilt
    /// on frames that only moved the playhead or a meter.
    pub(super) fn render_right_dock(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let _s = crate::perf::PerfScope::enter("RightDock");
        let i18n = crate::i18n::I18n::from_app(cx);
        let active_panel = self.active_panel;
        let (tracks, selected_track_id, selected_clip_id, project_bpm) =
            self.inspector_track_snapshot(cx);
        crate::perf::count("tracks", tracks.len() as u64);
        let inspector_callbacks = self.build_inspector_callbacks(cx.entity().clone());

        // Reconcile the Inspector name field with the current track selection.
        // Only reload when the bound track actually changes, so typing into the
        // field for the *selected* track is never clobbered mid-edit.
        if self.inspector_name_edit.name_bound.as_deref() != selected_track_id.as_deref() {
            match selected_track_id
                .as_deref()
                .and_then(|tid| tracks.iter().find(|t| t.id == tid))
            {
                Some(t) => {
                    self.inspector_name_edit
                        .name_input
                        .set_value(t.name.clone());
                    self.inspector_name_edit.name_bound = Some(t.id.clone());
                    self.inspector_name_edit.name_synced = Some(t.name.clone());
                }
                None => {
                    self.inspector_name_edit.name_input.set_value("");
                    self.inspector_name_edit.name_bound = None;
                    self.inspector_name_edit.name_synced = None;
                }
            }
        }
        let inspector_name_focused = self.inspector_name_edit.name_input.is_focused(window);
        // A rename made elsewhere — a track header, undo, redo, another
        // project — changed the name under the bound field: reload it, even
        // while it has focus. A field in step with the track is left alone
        // while it is typed in, and shows the stored form once it lets go.
        let names_revision = self.timeline.read(cx).state.track_names_revision;
        if self.inspector_name_edit.names_revision_seen != names_revision {
            let bound_name = self
                .inspector_name_edit
                .name_bound
                .as_deref()
                .and_then(|tid| tracks.iter().find(|t| t.id == tid))
                .map(|t| t.name.clone());
            match bound_name {
                Some(name)
                    if input_ops::inspector_name_reloads(
                        self.inspector_name_edit.name_synced.as_deref(),
                        &name,
                        inspector_name_focused,
                    ) =>
                {
                    if self.inspector_name_edit.name_input.value != name {
                        self.inspector_name_edit.name_input.set_value(name.clone());
                    }
                    self.inspector_name_edit.name_synced = Some(name);
                    self.inspector_name_edit.names_revision_seen = names_revision;
                }
                // Focused and in step: catch up once it lets go.
                Some(_) => {}
                None => self.inspector_name_edit.names_revision_seen = names_revision,
            }
        }
        if self.inspector_name_edit.clip_name_bound.as_deref() != selected_clip_id.as_deref() {
            match selected_clip_id.as_deref().and_then(|cid| {
                tracks
                    .iter()
                    .find_map(|t| t.clips.iter().find(|c| c.id == cid))
            }) {
                Some(c) => {
                    self.inspector_name_edit
                        .clip_name_input
                        .set_value(c.name.clone());
                    self.inspector_name_edit.clip_name_bound = Some(c.id.clone());
                }
                None => {
                    self.inspector_name_edit.clip_name_input.set_value("");
                    self.inspector_name_edit.clip_name_bound = None;
                }
            }
        }
        if self.panels.inspector {
            if let Some(target) = self.overlay.pending_text_focus.take() {
                match target {
                    TextMenuTarget::InspectorName => {
                        self.inspector_name_edit.name_input.select_all();
                        self.inspector_name_edit
                            .name_input
                            .focus_handle
                            .focus(window, cx);
                    }
                    TextMenuTarget::InspectorClipName => {
                        self.inspector_name_edit.clip_name_input.select_all();
                        self.inspector_name_edit
                            .clip_name_input
                            .focus_handle
                            .focus(window, cx);
                    }
                    _ => {}
                }
            }
        }
        let inspector_clip_name_focused =
            self.inspector_name_edit.clip_name_input.is_focused(window);
        let inspector_name_callbacks = crate::components::text_input::bind_mouse_selection(
            cx.entity().clone(),
            |layout: &mut StudioLayout| &mut layout.inspector_name_edit.name_input,
        );
        let inspector_clip_name_callbacks = crate::components::text_input::bind_mouse_selection(
            cx.entity().clone(),
            |layout: &mut StudioLayout| &mut layout.inspector_name_edit.clip_name_input,
        );
        let inspector_color_hex_focused = self
            .inspector_name_edit
            .color_picker
            .hex_input
            .is_focused(window);
        let inspector_color_hex_callbacks = crate::components::text_input::bind_mouse_selection(
            cx.entity().clone(),
            |layout: &mut StudioLayout| &mut layout.inspector_name_edit.color_picker.hex_input,
        );
        let inspector_color_hex_context: Arc<
            dyn Fn(&(f32, f32), &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            Arc::new(move |(x, y): &(f32, f32), _window, cx| {
                let x = *x;
                let y = *y;
                let _ = this.update(cx, |this, cx| {
                    this.overlay.text_context_menu = Some(TextContextMenu {
                        target: TextMenuTarget::InspectorColorHex,
                        x,
                        y,
                    });
                    cx.notify();
                });
            })
        };
        let inspector_color_hex_callbacks = TextInputCallbacks {
            on_mouse_down_out: None,
            on_context_command: None,
            on_context_menu: Some(inspector_color_hex_context),
            on_mouse: inspector_color_hex_callbacks.on_mouse,
        };
        let inspector_color_callbacks = self
            .build_inspector_color_picker_callbacks(cx.entity().clone(), selected_track_id.clone());

        let right_tab = self.right_dock_tab;
        let owner = cx.entity().clone();
        let content = match right_tab {
            RightDockTab::Inspector => {
                let inspector_audio_connections =
                    self.timeline.read(cx).state.audio_connections.clone();
                let selection_duration_beats = self
                    .timeline
                    .read(cx)
                    .state
                    .arrangement_range
                    .as_ref()
                    .and_then(|range| {
                        let (start, end) = range.as_f32_range();
                        let duration = (end - start).abs();
                        (duration > 0.0001).then_some(duration)
                    });
                let stretch_tempo = selected_clip_id
                    .as_deref()
                    .map(|clip_id| self.stretch_tempo_snapshot(clip_id));
                // The fades the clip plays, crossfades included, for its rows.
                let clip_fades = selected_clip_id
                    .as_deref()
                    .and_then(|clip_id| self.timeline.read(cx).state.clip_fade_summary(clip_id));
                crate::components::panel::inspector_panel(
                    &tracks,
                    &inspector_audio_connections,
                    selected_track_id.as_deref(),
                    selected_clip_id.as_deref(),
                    find_clip_summary(
                        &tracks,
                        selected_clip_id.as_deref(),
                        project_bpm,
                        selection_duration_beats,
                    )
                    .map(|mut summary| {
                        summary.fades = clip_fades;
                        summary
                    }),
                    stretch_tempo,
                    &self.inspector_name_edit.name_input,
                    inspector_name_focused,
                    inspector_name_callbacks,
                    &self.inspector_name_edit.clip_name_input,
                    inspector_clip_name_focused,
                    inspector_clip_name_callbacks,
                    crate::components::panel::InspectorColorPicker {
                        state: &self.inspector_name_edit.color_picker,
                        hex_focused: inspector_color_hex_focused,
                        hex_callbacks: inspector_color_hex_callbacks,
                        callbacks: inspector_color_callbacks,
                    },
                    active_panel == WorkspaceActivePanel::Inspector,
                    &inspector_callbacks,
                    i18n,
                )
                .into_any_element()
            }
            RightDockTab::Solfege => crate::components::panel::solfege_panel(
                &tracks,
                selected_track_id.as_deref(),
                active_panel == WorkspaceActivePanel::Solfege,
                self.solfege_editor.read(cx).selected_pitch_summary(cx),
            )
            .into_any_element(),
            RightDockTab::ChordDisplay => self.chord_display_panel.clone().into_any_element(),
            RightDockTab::LyricDisplay => self.lyric_display_panel.clone().into_any_element(),
            RightDockTab::LyricEditor => self.lyric_editor_panel.clone().into_any_element(),
        };
        div()
            .w(px(crate::components::panel::INSPECTOR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .min_h_0()
            .border_l(px(1.0))
            .border_color(Colors::border_subtle())
            .child(super::studio_render::right_dock_tab_bar(right_tab, owner))
            .child(div().flex_1().min_h_0().overflow_hidden().child(content))
            .into_any_element()
    }
}

/// The title/transport chrome as its own view.
///
/// The chrome is the one shell region the playhead genuinely changes: it shows
/// the bar.beat readout. Keeping it here means the transport poll can repaint
/// *it* on a beat boundary instead of notifying the whole studio root, which is
/// what turned every beat into a full-shell rebuild.
pub struct AppChromeView {
    owner: Entity<StudioLayout>,
    /// Tempo, time signature, loop/metronome/follow and the record state all
    /// come off the timeline, so a cached chrome has to observe it too.
    _observers: Vec<gpui::Subscription>,
}

impl AppChromeView {
    pub fn new(
        owner: Entity<StudioLayout>,
        timeline: &Entity<components::timeline::Timeline>,
        settings: &gpui::Entity<crate::settings::SettingsModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _observers = vec![
            cx.observe(&owner, |_, _, cx| cx.notify()),
            cx.observe(timeline, |_, _, cx| cx.notify()),
            cx.observe(settings, |_, _, cx| cx.notify()),
        ];
        Self { owner, _observers }
    }

    /// Matches the root of `components::app_chrome`, which is a **column** of
    /// two rows — the titlebar band and the transport bar under it — not the
    /// titlebar alone. Getting this wrong sizes the cached view to one row and
    /// drops the transport out of the window entirely.
    pub fn cached_style() -> gpui::StyleRefinement {
        gpui::StyleRefinement::default()
            .flex()
            .flex_col()
            .w_full()
            .flex_none()
            .h(px(components::app_chrome_drawn_height()))
    }
}

impl Render for AppChromeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = self.owner.clone();
        owner.update(cx, |layout, cx| layout.render_app_chrome(window, cx))
    }
}

impl StudioLayout {
    /// Opening a top-level menu. Shared because both the chrome that draws the
    /// menu bar and the shell-level dropdown overlay dispatch it.
    pub(super) fn menu_open_callback(
        &self,
        cx: &mut Context<Self>,
    ) -> std::sync::Arc<dyn Fn(&(String, f32), &mut Window, &mut gpui::App) + 'static> {
        let this = cx.entity().clone();
        std::sync::Arc::new(move |(id, anchor_x): &(String, f32), _w, cx| {
            let id = id.clone();
            let anchor_x = *anchor_x;
            this.update(cx, |this, cx| {
                if this.menu_bar.open_menu_id.as_deref() == Some(id.as_str()) {
                    this.menu_bar.open_menu_id = None;
                } else {
                    this.menu_bar.open_menu_id = Some(id);
                    this.menu_bar.anchor = titlebar_label_anchor(anchor_x);
                }
                this.menu_bar.submenu_path.clear();
                this.overlay.open_popover = None;
                this.project_switcher.is_open = false;
                cx.notify();
            });
        })
    }

    /// Build the title/transport chrome.
    pub(super) fn render_app_chrome(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let _s = crate::perf::PerfScope::enter("AppChrome");
        let i18n = crate::i18n::I18n::from_app(cx);
        let open_menu_id = self.menu_bar.open_menu_id.clone();
        let on_open_menu = self.menu_open_callback(cx);
        let on_project_open: std::sync::Arc<dyn Fn(&f32, &mut Window, &mut gpui::App) + 'static> = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |anchor_x: &f32, w, cx| {
                let anchor_x = *anchor_x;
                let _ = this.update(cx, |this, cx| {
                    this.menu_bar.open_menu_id = None;
                    this.menu_bar.submenu_path.clear();
                    this.overlay.open_popover = None;
                    this.overlay.text_context_menu = None;
                    this.project_switcher.is_open = !this.project_switcher.is_open;
                    this.project_switcher.anchor = project_title_anchor(anchor_x);
                    if this.project_switcher.is_open {
                        this.project_switcher.query.clear();
                        this.project_switcher_search_input.set_value("");
                        this.project_switcher_search_input.focus_handle.focus(w, cx);
                        this.project_switcher.selected_index = 0;
                        // Refresh which recents still exist on disk — off the UI
                        // thread, so opening the switcher never blocks on per-entry
                        // filesystem stats (a multi-hundred-ms stall on OneDrive).
                        this.spawn_refresh_recent_missing(cx);
                    }
                    cx.notify();
                });
            })
        };
        let project_chrome = components::ProjectChromeState {
            name: self.project_session.display_name().to_string(),
            is_dirty: self.project_session.is_dirty,
            on_open_project_menu: on_project_open,
        };
        let transport_chrome = self.transport_chrome_state(cx);
        let panel_chrome = self.panel_chrome_state(cx);
        let close_target = cx.entity().clone();
        let on_window_close: components::ChromeActionCb =
            std::sync::Arc::new(move |_: &(), window: &mut Window, cx: &mut gpui::App| {
                let owner_bounds = Some(window.bounds());
                let _ = close_target.update(cx, |studio, cx| {
                    studio.request_close(PendingCloseAction::QuitApp, owner_bounds, cx);
                });
            });
        components::app_chrome(
            window,
            open_menu_id.as_deref(),
            on_open_menu,
            project_chrome,
            transport_chrome,
            panel_chrome,
            Some(on_window_close),
            i18n,
        )
        .into_any_element()
    }
}

/// Layout-affecting root style of `BottomPanelShell::render`. The height is a
/// user-resizable value, so the shell passes the current one — a different
/// height changes the cached view's bounds, which invalidates the cache by
/// itself.
pub(super) fn bottom_panel_cached_style(height_px: f32) -> gpui::StyleRefinement {
    gpui::StyleRefinement::default()
        .flex()
        .flex_col()
        .h(px(height_px))
        .w_full()
        .border_t(px(1.0))
        .relative()
}

impl StudioLayout {
    /// Repaint only the chrome. Used by the transport poll for the bar.beat
    /// readout: that readout is the one shell-visible thing playback changes,
    /// and routing it here is what keeps a beat boundary from costing a full
    /// shell rebuild.
    pub(super) fn notify_app_chrome(&self, cx: &mut Context<Self>) {
        self.app_chrome.update(cx, |_, cx| cx.notify());
    }

    /// Repaint only the browser sidebar. The sample-audition playhead advances
    /// at the poll rate and is visible nowhere else, so it takes the same route
    /// the bar.beat readout does rather than rebuilding the whole shell.
    pub(super) fn notify_browser_sidebar(&self, cx: &mut Context<Self>) {
        self.browser_sidebar.update(cx, |_, cx| cx.notify());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cached view lays out from its style alone — GPUI does not render its
    /// children to measure it. `components::app_chrome` is a column of two
    /// rows, and a cached height covering only the titlebar dropped the whole
    /// transport row out of the window.
    #[test]
    fn app_chrome_cached_style_covers_the_transport_row() {
        let style = AppChromeView::cached_style();
        let height = match style.size.height {
            Some(gpui::Length::Definite(gpui::DefiniteLength::Absolute(
                gpui::AbsoluteLength::Pixels(pixels),
            ))) => f32::from(pixels),
            other => panic!("chrome cached height must be absolute pixels, got {other:?}"),
        };
        let titlebar = crate::platform_chrome::PlatformChromePolicy::current().titlebar_height_px;
        assert_eq!(
            height,
            titlebar + crate::shell_metrics::TRANSPORT_BAR_HEIGHT,
            "cached chrome height must cover the titlebar band and the transport bar"
        );
    }

    /// Same failure mode, cheaper to state: the two docks are fixed-width
    /// columns, so their cached width has to be the width the panel draws at.
    #[test]
    fn dock_cached_widths_match_their_panels() {
        let width = |style: gpui::StyleRefinement| match style.size.width {
            Some(gpui::Length::Definite(gpui::DefiniteLength::Absolute(
                gpui::AbsoluteLength::Pixels(pixels),
            ))) => f32::from(pixels),
            other => panic!("dock cached width must be absolute pixels, got {other:?}"),
        };
        assert_eq!(
            width(BrowserSidebarView::cached_style()),
            components::SIDEBAR_WIDTH
        );
        assert_eq!(
            width(RightDockView::cached_style()),
            crate::components::panel::INSPECTOR_WIDTH
        );
    }
}
