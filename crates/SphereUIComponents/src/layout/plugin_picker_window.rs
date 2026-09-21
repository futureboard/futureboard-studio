use std::sync::Arc;

use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, Styled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, div, px, size,
};

use crate::components::plugin_picker::{
    CatalogStatus, PickerFilter, PluginPickerCallbacks, PluginPickerPrefs,
    PluginPickerScrollHandles, PluginPickerState, PluginSearchIndex, compute_filter_result,
    ensure_default_highlight, picker_perf_debug, plugin_picker_panel,
};
use crate::components::text_input::{
    TextInputCallbacks, TextInputMouseCb, TextInputMouseEvent, bind_mouse_selection,
};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{self, Colors};
use crate::window_position::resolve_owner_bounds_with_preferred;
use crate::window_position::{apply_owner_display, centered_window_bounds};

use super::StudioLayout;

const INSERT_PICKER_WINDOW_WIDTH: f32 = 960.0;
const INSERT_PICKER_WINDOW_HEIGHT: f32 = 680.0;
const INSERT_PICKER_WINDOW_MIN_WIDTH: f32 = 820.0;
const INSERT_PICKER_WINDOW_MIN_HEIGHT: f32 = 560.0;

pub(crate) struct InsertPickerWindow {
    owner: Entity<StudioLayout>,
    snapshot: InsertPickerSnapshot,
    focus_handle: FocusHandle,
    scroll: PluginPickerScrollHandles,
    /// Focus the search field once the dialog's own window is live.
    needs_search_focus: bool,
    /// Cut/Copy/Paste menu position for the search field, or `None`.
    ///
    /// The field itself lives on `StudioLayout`, but this is a separate window,
    /// so the shell's overlay cannot draw over it — this window owns the menu's
    /// position and renders it.
    search_context_menu: Option<(f32, f32)>,
}

#[derive(Clone)]
pub(crate) struct InsertPickerSnapshot {
    pub picker: PluginPickerState,
    // Cheap `Arc` clone — the snapshot is rebuilt on every keystroke, so it must
    // not deep-copy the plugin index.
    pub index: Option<std::sync::Arc<PluginSearchIndex>>,
    pub prefs: PluginPickerPrefs,
    pub catalog_status: CatalogStatus,
    pub search_input: crate::components::text_input::TextInputState,
    pub au_error: Option<String>,
}

impl InsertPickerWindow {
    fn new(
        owner: Entity<StudioLayout>,
        snapshot: InsertPickerSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            owner,
            snapshot,
            focus_handle: cx.focus_handle(),
            scroll: PluginPickerScrollHandles::default(),
            needs_search_focus: true,
            search_context_menu: None,
        }
    }

    fn set_snapshot(&mut self, snapshot: InsertPickerSnapshot) {
        self.snapshot = snapshot;
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.snapshot.search_input.focus_handle.focus(window, cx);
        self.needs_search_focus = false;
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.owner.update(cx, |layout, cx| {
            layout.plugin_picker = PluginPickerState::closed();
            layout.plugin_picker_window = None;
            cx.notify();
        });
        window.remove_window();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let perf = picker_perf_debug();
        let started = perf.then(std::time::Instant::now);
        let search_focused = self.snapshot.search_input.is_focused(window);
        let (handled, finished, snapshot) = self.owner.update(cx, |layout, cx| {
            if event.keystroke.key.as_str() == "escape" {
                layout.plugin_picker = PluginPickerState::closed();
                layout.plugin_picker_window = None;
                cx.notify();
                return (true, true, layout.insert_picker_snapshot());
            }
            let handled = layout.handle_plugin_picker_key(event, window, cx);
            // Enter inserts the highlighted plug-in and closes the picker state.
            // Without this the window stayed up showing a closed picker, so the
            // keyboard path did not actually finish the flow the way
            // double-click and the Add button do.
            let finished = !layout.plugin_picker.is_open;
            if finished {
                layout.plugin_picker_window = None;
            }
            (handled, finished, layout.insert_picker_snapshot())
        });
        self.set_snapshot(snapshot);
        cx.notify();
        if let Some(started) = started {
            eprintln!(
                "[picker-perf] handle_key key={:?} handled={handled} took_us={}",
                event.keystroke.key,
                started.elapsed().as_micros()
            );
        }
        if handled {
            cx.stop_propagation();
        }
        let mods = event.keystroke.modifiers;
        if !handled
            && !search_focused
            && !event.is_held
            && event.keystroke.key.eq_ignore_ascii_case("space")
            && !mods.control
            && !mods.alt
            && !mods.platform
            && !mods.function
        {
            crate::components::transport_key::claim_space_key_up();
            let _ = self.owner.update(cx, |layout, cx| {
                layout.dispatch_command_id("transport:play-pause", cx);
            });
            window.prevent_default();
            cx.stop_propagation();
        }
        if finished {
            window.remove_window();
        }
    }
}

impl Render for InsertPickerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let build_started = picker_perf_debug().then(std::time::Instant::now);
        if self.needs_search_focus {
            self.focus_search(window, cx);
        }
        let target = cx.entity().clone();
        let snapshot = self.snapshot.clone();
        let search_focused = snapshot.search_input.is_focused(window);

        let picker_callbacks = PluginPickerCallbacks {
            on_close: Arc::new({
                let target = target.clone();
                move |_: &(), window, cx| {
                    let _ = target.update(cx, |this, cx| this.close(window, cx));
                }
            }),
            on_select: Arc::new({
                let owner = self.owner.clone();
                let target = target.clone();
                move |plugin_id: &String, _w, cx| {
                    let plugin_id = plugin_id.clone();
                    let snapshot = owner.update(cx, |layout, cx| {
                        if let Some(index) = layout.plugin_search_index.as_ref() {
                            let result = compute_filter_result(
                                index,
                                &layout.plugin_picker.query,
                                &layout.plugin_picker.filters,
                                &layout.plugin_picker_prefs,
                                std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some(),
                            );
                            if let Some(highlight) = result.indices.iter().position(|&idx| {
                                index.plugin_at(idx).is_some_and(|p| p.id == plugin_id)
                            }) {
                                layout.plugin_picker.highlighted_index = highlight;
                            }
                        }
                        layout.plugin_picker.selected_id = Some(plugin_id);
                        cx.notify();
                        layout.insert_picker_snapshot()
                    });
                    let _ = target.update(cx, |this, cx| {
                        this.set_snapshot(snapshot);
                        cx.notify();
                    });
                }
            }),
            on_pick: Arc::new({
                let owner = self.owner.clone();
                move |plugin_id: &String, window, cx| {
                    let plugin_id = plugin_id.clone();
                    let _ = owner.update(cx, |layout, cx| {
                        if let Some((track_id, insert_index, insert_id)) =
                            layout.apply_picked_insert(&plugin_id, cx)
                        {
                            layout.open_insert_editor(
                                &track_id,
                                insert_index,
                                &insert_id,
                                window,
                                cx,
                            );
                        }
                        layout.plugin_picker_window = None;
                    });
                    window.remove_window();
                }
            }),
            on_select_filter: Arc::new({
                let owner = self.owner.clone();
                let target = target.clone();
                move |filter: &PickerFilter, _w, cx| {
                    let filter = filter.clone();
                    let snapshot = owner.update(cx, |layout, cx| {
                        layout.plugin_picker.set_sidebar_filter(filter);
                        if let Some(index) = layout.plugin_search_index.as_ref() {
                            ensure_default_highlight(
                                &mut layout.plugin_picker,
                                index,
                                &layout.plugin_picker_prefs,
                            );
                        }
                        cx.notify();
                        layout.insert_picker_snapshot()
                    });
                    let _ = target.update(cx, |this, cx| {
                        this.set_snapshot(snapshot);
                        cx.notify();
                    });
                }
            }),
            on_toggle_favorite: Arc::new({
                let owner = self.owner.clone();
                let target = target.clone();
                move |plugin_id: &String, _w, cx| {
                    let plugin_id = plugin_id.clone();
                    let snapshot = owner.update(cx, |layout, cx| {
                        layout.plugin_picker_prefs.toggle_favorite(&plugin_id);
                        cx.notify();
                        layout.insert_picker_snapshot()
                    });
                    let _ = target.update(cx, |this, cx| {
                        this.set_snapshot(snapshot);
                        cx.notify();
                    });
                }
            }),
            on_retry_load: Arc::new({
                let owner = self.owner.clone();
                let target = target.clone();
                move |_: &(), _w, cx| {
                    let snapshot = owner.update(cx, |layout, cx| {
                        layout.plugin_catalog.available = None;
                        layout.plugin_search_index = None;
                        layout.plugin_catalog.status = CatalogStatus::Loading;
                        layout.arm_catalog_load(cx);
                        cx.notify();
                        layout.insert_picker_snapshot()
                    });
                    let _ = target.update(cx, |this, cx| {
                        this.set_snapshot(snapshot);
                        cx.notify();
                    });
                }
            }),
            on_open_plugin_manager: Arc::new({
                let owner = self.owner.clone();
                move |_: &(), window, cx| {
                    let _ = owner.update(cx, |layout, cx| {
                        layout.plugin_picker = PluginPickerState::closed();
                        layout.plugin_picker_window = None;
                        layout.open_plugin_manager_external_window(None, cx);
                        cx.notify();
                    });
                    window.remove_window();
                }
            }),
            on_rebuild_database: Arc::new({
                let owner = self.owner.clone();
                let target = target.clone();
                move |_: &(), _w, cx| {
                    let snapshot = owner.update(cx, |layout, cx| {
                        let _ = SpherePluginHost::plugin_db::delete_database_file();
                        layout.plugin_catalog.available = None;
                        layout.plugin_search_index = None;
                        layout.plugin_catalog.status = CatalogStatus::Loading;
                        layout.arm_catalog_load(cx);
                        cx.notify();
                        layout.insert_picker_snapshot()
                    });
                    let _ = target.update(cx, |this, cx| {
                        this.set_snapshot(snapshot);
                        cx.notify();
                    });
                }
            }),
            on_drop_plugin: Arc::new(|_, _, _| {}),
        };

        let search_mouse_callbacks =
            bind_mouse_selection(self.owner.clone(), |layout: &mut StudioLayout| {
                &mut layout.plugin_picker_search_input
            });
        let owner = self.owner.clone();
        let target_for_search = target.clone();
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
            on_mouse: search_mouse_callbacks.on_mouse.map(|on_mouse| {
                Arc::new(
                    move |event: &TextInputMouseEvent, window: &mut Window, cx: &mut App| {
                        on_mouse(event, window, &mut *cx);
                        let snapshot =
                            owner.update(cx, |layout, _cx| layout.insert_picker_snapshot());
                        let _ = target_for_search.update(cx, |this, cx| {
                            this.set_snapshot(snapshot);
                            cx.notify();
                        });
                    },
                ) as TextInputMouseCb
            }),
        };

        let panel = div().flex_1().min_h(px(0.0)).child(plugin_picker_panel(
            &snapshot.picker,
            snapshot.index.clone(),
            &snapshot.prefs,
            snapshot.catalog_status,
            &snapshot.search_input,
            search_focused,
            search_callbacks,
            picker_callbacks,
            snapshot.au_error.as_deref(),
            &self.scroll,
        ));

        // The search field is owned by `StudioLayout`, so the command applies
        // there and then re-snapshots — otherwise the text would change and the
        // filtered plugin list behind it would not.
        let search_context_overlay = self.search_context_menu.map(|(x, y)| {
            let clipboard_has_text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty());
            let entries = crate::components::text_input::text_input_context_entries(
                &snapshot.search_input,
                clipboard_has_text,
            );
            let owner = self.owner.clone();
            let command_target = target.clone();
            let close_target = target.clone();
            crate::components::context_menu::context_menu_overlay(
                entries,
                x,
                y,
                INSERT_PICKER_WINDOW_WIDTH,
                INSERT_PICKER_WINDOW_HEIGHT,
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let refreshed = owner.update(cx, |layout, _cx| {
                        let applied = layout
                            .plugin_picker_search_input
                            .apply_context_command(&command, _cx);
                        if applied {
                            layout.sync_text_input_target(
                                crate::layout::studio_state::TextMenuTarget::PluginPickerSearch,
                            );
                        }
                        layout.insert_picker_snapshot()
                    });
                    let _ = command_target.update(cx, |this, cx| {
                        this.set_snapshot(refreshed);
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

        let root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Colors::surface_window())
            .font(theme::ui_font())
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx)
            }))
            // The transport key's key-up belongs to the transport, not to the
            // focused row or button GPUI would otherwise click with it. See
            // `transport_key::claim_space_key_up`.
            .capture_key_up(
                |_event: &gpui::KeyUpEvent, window: &mut Window, cx: &mut gpui::App| {
                    if crate::components::transport_key::take_space_key_up_claim() {
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                },
            )
            .child(external_window_titlebar(
                "Add Insert",
                "insert-picker-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(panel)
            .children(search_context_overlay);

        if let Some(started) = build_started {
            eprintln!(
                "[picker-perf] render_build took_us={}",
                started.elapsed().as_micros()
            );
        }
        root
    }
}

pub(crate) fn open_insert_picker_window(
    owner: Entity<StudioLayout>,
    snapshot: InsertPickerSnapshot,
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<InsertPickerWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(
            px(INSERT_PICKER_WINDOW_WIDTH),
            px(INSERT_PICKER_WINDOW_HEIGHT),
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
        px(INSERT_PICKER_WINDOW_MIN_WIDTH),
        px(INSERT_PICKER_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, |_window, cx| {
        cx.new(|cx| InsertPickerWindow::new(owner, snapshot, cx))
    })
    .map_err(|e| e.to_string())
}

impl StudioLayout {
    pub(super) fn insert_picker_snapshot(&self) -> InsertPickerSnapshot {
        InsertPickerSnapshot {
            picker: self.plugin_picker.clone(),
            index: self.plugin_search_index.clone(),
            prefs: self.plugin_picker_prefs.clone(),
            catalog_status: self.plugin_catalog.status.clone(),
            search_input: self.plugin_picker_search_input.clone(),
            au_error: self.plugin_picker_au_error.clone(),
        }
    }

    pub(super) fn open_insert_picker_external_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.plugin_picker_window.clone() {
            if handle
                .update(cx, |picker, window, cx| {
                    picker.needs_search_focus = true;
                    picker.focus_search(window, cx);
                    window.activate_window();
                })
                .is_ok()
            {
                self.notify_insert_picker_window(cx);
                return;
            }
            self.plugin_picker_window = None;
        }

        self.overlay.open_popover = None;
        self.overlay.text_context_menu = None;
        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();

        let owner_bounds =
            resolve_owner_bounds_with_preferred(owner_bounds, self.studio_window_bounds(cx), cx);
        let snapshot = self.insert_picker_snapshot();
        match open_insert_picker_window(cx.entity().clone(), snapshot, owner_bounds, cx) {
            Ok(handle) => {
                self.plugin_picker_window = Some(handle.clone());
            }
            Err(error) => {
                eprintln!("[plugin-picker] failed to open external window: {error}");
                self.plugin_picker = PluginPickerState::closed();
            }
        }
    }

    /// Close the Add Insert window and forget its state.
    ///
    /// The picker targets one track and slot of the *current* project. When the
    /// project is replaced it would otherwise stay up aimed at a track that no
    /// longer exists — a window whose Add button silently does nothing.
    pub(super) fn close_insert_picker_window(&mut self, cx: &mut Context<Self>) {
        self.plugin_picker = PluginPickerState::closed();
        if let Some(handle) = self.plugin_picker_window.take() {
            let _ = handle.update(cx, |_picker, window, _cx| window.remove_window());
        }
    }

    pub(super) fn prune_insert_picker_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.plugin_picker_window.clone() else {
            return;
        };
        if handle.update(cx, |_picker, _window, _cx| ()).is_err() {
            self.plugin_picker_window = None;
            self.plugin_picker = PluginPickerState::closed();
            cx.notify();
        }
    }

    pub(super) fn notify_insert_picker_window(&mut self, cx: &mut App) {
        if let Some(handle) = self.plugin_picker_window.clone() {
            let snapshot = self.insert_picker_snapshot();
            if handle
                .update(cx, |picker, _window, cx| {
                    picker.set_snapshot(snapshot);
                    cx.notify();
                })
                .is_err()
            {
                self.plugin_picker_window = None;
            }
        }
    }
}
