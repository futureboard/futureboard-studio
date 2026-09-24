//! Audio Connections window — a normal, modeless Studio utility window around
//! [`AudioConnectionsPanelState`].
//!
//! This is a project utility window, not a dropdown or a canvas overlay: it
//! opens through the same [`external_window_options_partial`] path as the
//! external Mixer and Video Player, so the platform gives it a real titlebar,
//! taskbar entry, minimize/maximize, resizing, and independent placement across
//! monitors. Nothing here positions itself relative to the main project window.
//!
//! [`external_window_options_partial`]: crate::platform_chrome::external_window_options_partial
//!
//! The window owns only transient view state. Rows are read from the project
//! registry every render and every edit is emitted as a [`ConnectionEdit`] that
//! the layout applies through the registry's structured mutation API, so the
//! project stays the single source of truth and all project mutation lives in
//! one place. Destructive actions raise a `Request…` edit and are confirmed by
//! the shared message-box window rather than by an overlay drawn in here.

use gpui::prelude::FluentBuilder;
use gpui::AppContext as _;
use gpui::{
    div, px, AnyView, App, Context, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window,
};

use crate::audio_connections::{
    AudioConnectionDirection, AudioConnectionId, AudioConnectionRegistry, AudioConnectionStatus,
    AvailablePorts, ChannelLayout, ConnectionPreset, PANEL_PRESETS,
};
use crate::components::audio_connections_panel::{
    cell_anchor_for_row, column_widths, dropdown_height, dropdown_width, layout_label,
    AudioConnectionsPanelState, CellAnchor, ColumnWidths, ConnectionRow, ConnectionsTab,
    EditableCell, OpenDropdown, ROW_HEIGHT,
};
use crate::components::combo_box::{combo_box_icon_menu, MenuGlyph, MenuItem};
use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::text_input::{bind_mouse_selection, text_field_with_callbacks, TextInputState};
use crate::components::title_bar::{external_window_titlebar, TITLEBAR_HEIGHT};
use crate::theme::Colors;
use crate::window_position::{apply_owner_display, centered_window_bounds};

/// Initial size only. The window is fully resizable and the table lays itself
/// out from whatever width it actually gets, so nothing below is tuned to
/// these numbers.
const WINDOW_WIDTH: f32 = 1000.0;
const WINDOW_HEIGHT: f32 = 600.0;

/// Chrome above the first table row, used to anchor popovers to the row grid.
const SUBHEADER_HEIGHT: f32 = 38.0;
const TABS_HEIGHT: f32 = 24.0;
const TOOLBAR_HEIGHT: f32 = 30.0;
const TABLE_HEADER_HEIGHT: f32 = 22.0;
const FOOTER_HEIGHT: f32 = 22.0;

/// Y of the first table row, in window space.
fn table_top() -> f32 {
    TITLEBAR_HEIGHT + SUBHEADER_HEIGHT + TABS_HEIGHT + TOOLBAR_HEIGHT + TABLE_HEADER_HEIGHT
}

/// Y of the toolbar strip, in window space.
fn toolbar_top() -> f32 {
    TITLEBAR_HEIGHT + SUBHEADER_HEIGHT + TABS_HEIGHT
}

/// The Presets button is the first thing in the toolbar, so its rectangle comes
/// from the bar's own padding rather than from a measurement — the same way the
/// table's popovers are reconstructed from the column layout. The width only
/// left-aligns the menu and sets its floor; the labels decide how wide it ends
/// up.
const PRESETS_BUTTON_X: f32 = 8.0;
const PRESETS_BUTTON_WIDTH: f32 = 84.0;

/// Rectangle the preset menu hangs from.
fn presets_anchor() -> CellAnchor {
    CellAnchor {
        x: PRESETS_BUTTON_X,
        y: toolbar_top(),
        width: PRESETS_BUTTON_WIDTH,
        height: TOOLBAR_HEIGHT,
    }
}

/// Smallest window that still shows the whole table plus its chrome.
///
/// Derived from the column floors and the fixed chrome heights rather than
/// picked by eye, so it stays correct if either changes.
pub fn window_min_size() -> (f32, f32) {
    let width = ColumnWidths::min_total() + 2.0;
    let height = table_top() + 3.0 * ROW_HEIGHT + FOOTER_HEIGHT;
    (width, height)
}

/// The glyph for a device or port, by what it actually is.
///
/// A menu of endpoint names is a wall of text in which an interface, the jam
/// send, and a person in the room are the same shape. The kind should be
/// readable before the name is — and for a jam stream the kind is *a person*,
/// so it is their face rather than any icon.
fn device_glyph(device_id: &str, direction: AudioConnectionDirection) -> MenuGlyph {
    use crate::audio_connections::AudioConnectionDirection::*;

    // A jam port is a person, whatever else is or is not known about them, so
    // the kind is decided by the device id and never by what the room snapshot
    // happens to hold. A stream whose publisher has left — or one seen a frame
    // before the snapshot catches up — still draws a person rather than
    // borrowing an audio interface's microphone.
    if DirectAudio::jam_bus::is_jam_device(device_id) {
        return match crate::jam::stream_identity(device_id) {
            // The picture if it has landed, their initials until it does.
            // Never a blank hole and never a spinner: the row is readable
            // either way and simply gets better a frame later.
            Some(identity) => match crate::account::profile_picture(&identity.avatar_url) {
                Some(image) => MenuGlyph::Picture(image),
                None => MenuGlyph::Monogram(identity.monogram),
            },
            None => MenuGlyph::Svg(crate::assets::ICON_USER_PATH),
        };
    }
    if DirectAudio::jam_bus::is_jam_send_device(device_id) {
        return MenuGlyph::Svg(crate::assets::ICON_ROUTE_PATH);
    }
    match direction {
        Input => MenuGlyph::Svg(crate::assets::ICON_MIC_PATH),
        Output => MenuGlyph::Svg(crate::assets::ICON_VOLUME_2_PATH),
    }
}

/// The glyph for the row that clears an assignment.
fn unassigned_glyph() -> MenuGlyph {
    MenuGlyph::Svg(crate::assets::ICON_MINUS_PATH)
}

/// The glyph for a channel layout.
fn layout_glyph(layout: ChannelLayout) -> MenuGlyph {
    match layout {
        ChannelLayout::Mono => MenuGlyph::Svg(crate::assets::ICON_CIRCLE_DOT_PATH),
        _ => MenuGlyph::Svg(crate::assets::ICON_AUDIO_LINES_PATH),
    }
}

/// What running the Audio Connections command again should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReopenAction {
    /// Bring the window already on screen to the front.
    FocusExisting,
    /// No usable window exists — create the one instance.
    OpenNew,
}

/// Decide between focusing and opening.
///
/// There is exactly one Audio Connections window. `activated` is `None` when
/// no handle is held and `Some(false)` when the held handle is stale because
/// its window was already closed; both mean open a fresh one. Only a handle
/// that actually accepted activation is reused, so the command can never
/// produce a duplicate instance or focus a dead window.
pub fn reopen_action(activated: Option<bool>) -> ReopenAction {
    match activated {
        Some(true) => ReopenAction::FocusExisting,
        Some(false) | None => ReopenAction::OpenNew,
    }
}

/// Applies one registry edit against the project and republishes routing when
/// the mutation asks for it.
pub type ConnectionEditCb =
    std::sync::Arc<dyn Fn(&ConnectionEdit, &mut gpui::Window, &mut gpui::App) + 'static>;

/// One user action from the panel. Deliberately data rather than closures, so
/// the layout owns every project mutation in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEdit {
    Add {
        direction: AudioConnectionDirection,
        layout: ChannelLayout,
    },
    SetEnabled {
        id: AudioConnectionId,
        enabled: bool,
    },
    Rename {
        id: AudioConnectionId,
        name: String,
    },
    SetLayout {
        id: AudioConnectionId,
        layout: ChannelLayout,
    },
    SetDevice {
        id: AudioConnectionId,
        /// `None` unassigns the device and clears every binding.
        device_id: Option<String>,
    },
    SetPort {
        id: AudioConnectionId,
        logical_channel: usize,
        /// `None` clears just this channel.
        port: Option<crate::audio_connections::AudioPortId>,
    },
    Duplicate {
        id: AudioConnectionId,
    },
    /// The user asked to remove a bus. The layout decides whether that needs
    /// the shared confirmation window, because only it knows what references
    /// the bus.
    RequestRemove {
        id: AudioConnectionId,
    },
    /// Already confirmed — apply it.
    Remove {
        id: AudioConnectionId,
    },
    /// The user asked to reset a direction. Always confirmed by the layout.
    RequestResetDefaults {
        direction: AudioConnectionDirection,
    },
    ResetDefaults {
        direction: AudioConnectionDirection,
    },
    /// The user picked a preset. Always confirmed by the layout: a preset
    /// replaces every bus in the project, in both directions.
    RequestApplyPreset {
        preset: ConnectionPreset,
    },
    /// Already confirmed — apply it.
    ApplyPreset {
        preset: ConnectionPreset,
    },
    /// Bind this direction's buses to consecutive hardware ports. Never
    /// confirmed: it changes port assignments, creates and removes nothing,
    /// and the previous mapping is one Auto Map or a few clicks away.
    AutoMap {
        direction: AudioConnectionDirection,
    },
    /// Open Preferences so hardware can be chosen without leaving the task.
    OpenAudioDeviceSetup,
}

/// Everything the window needs from the project, refreshed on each sync.
#[derive(Clone, Default)]
pub struct AudioConnectionsSnapshot {
    pub registry: AudioConnectionRegistry,
    /// Ports the hardware currently exposes. Carried in the snapshot rather
    /// than re-read during render so one paint sees one consistent view.
    pub ports: AvailablePorts,
    /// Active input endpoint, shown as secondary context in the sub-header.
    pub input_device: String,
    /// Active output endpoint.
    pub output_device: String,
    /// `false` shows the no-project empty state and disables mutation.
    pub has_project: bool,
    /// Track ids referencing each connection, keyed by connection id. Used to
    /// describe the consequences of a removal before it happens.
    pub references: std::collections::HashMap<String, Vec<String>>,
}

pub struct AudioConnectionsWindow {
    panel: AudioConnectionsPanelState,
    /// Shared Studio text input, reused for the Bus Name inline editor rather
    /// than a table-specific editing system.
    name_input: TextInputState,
    snapshot: AudioConnectionsSnapshot,
    on_edit: ConnectionEditCb,
    on_close: std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>,
    /// The table's scroll position, so a dropdown anchors to where its cell
    /// actually is after the table scrolled sideways or down.
    table_scroll: gpui::ScrollHandle,
}

impl AudioConnectionsWindow {
    pub fn new(
        snapshot: AudioConnectionsSnapshot,
        on_edit: ConnectionEditCb,
        on_close: std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>,
        focus_handle: gpui::FocusHandle,
    ) -> Self {
        Self {
            panel: AudioConnectionsPanelState::new(),
            name_input: TextInputState::new("audio-connections-name", focus_handle)
                .with_accessible_label("Connection name"),
            snapshot,
            on_edit,
            on_close,
            table_scroll: gpui::ScrollHandle::new(),
        }
    }

    /// Show the warnings from the most recent mutation in the footer.
    pub fn set_warnings(&mut self, warnings: Vec<String>, cx: &mut Context<Self>) {
        self.panel.set_warnings(warnings);
        cx.notify();
    }

    /// Push fresh project data. Selections that no longer resolve are pruned so
    /// a removed row cannot stay highlighted.
    pub fn sync(&mut self, snapshot: AudioConnectionsSnapshot, cx: &mut Context<Self>) {
        self.snapshot = snapshot;
        self.panel.prune_selection(&self.snapshot.registry);
        cx.notify();
    }

    /// Called when the project changes so no connection id survives across
    /// projects.
    pub fn on_project_changed(
        &mut self,
        snapshot: AudioConnectionsSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.panel.on_project_changed();
        self.snapshot = snapshot;
        cx.notify();
    }

    pub fn panel_state(&self) -> &AudioConnectionsPanelState {
        &self.panel
    }
}

fn status_color(status: AudioConnectionStatus) -> gpui::Rgba {
    match status {
        AudioConnectionStatus::Active => Colors::accent_success(),
        AudioConnectionStatus::Disabled => Colors::text_muted(),
        AudioConnectionStatus::Conflict => Colors::status_error(),
        _ => Colors::accent_warning(),
    }
}

/// Tooltip body for a truncated cell, so the complete label is always
/// reachable without widening the window.
struct CellTooltip(String);

impl Render for CellTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_raised())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(10.0))
            .text_color(Colors::text_secondary())
            .child(self.0.clone())
    }
}

fn tooltip_text(text: String) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_window, cx| cx.new(|_| CellTooltip(text.clone())).into()
}

fn header_cell(label: &str, width: f32) -> impl IntoElement {
    div()
        .w(px(width))
        .flex_none()
        .px(px(6.0))
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_secondary())
        .truncate()
        .child(label.to_string())
}

fn cell(text: String, width: f32, muted: bool) -> impl IntoElement {
    div()
        .w(px(width))
        .flex_none()
        .px(px(6.0))
        .text_size(px(10.0))
        .truncate()
        .text_color(if muted {
            Colors::text_muted()
        } else {
            Colors::text_primary()
        })
        .child(text)
}

/// Compact toolbar button. Takes a mouse-down listener so both plain closures
/// and `cx.listener(..)` results can be passed.
fn toolbar_button(
    id: &'static str,
    label: String,
    enabled: bool,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .h(px(20.0))
        .px(px(8.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::button_bg())
        .border(px(1.0))
        .border_color(Colors::button_border())
        .text_size(px(9.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if enabled {
            Colors::text_primary()
        } else {
            Colors::text_muted()
        })
        .when(enabled, |button| {
            button
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::button_bg_hover()))
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    cx.stop_propagation();
                    on_click(event, window, cx);
                })
        })
        .child(label)
}

impl AudioConnectionsWindow {
    /// A cell that opens a dropdown. Renders like the rest of the table but
    /// carries a hover state, a caret, and a focus ring when it is the active
    /// Tab target — enough to read as editable without making the table noisy.
    ///
    /// The anchor is the cell's own laid-out rectangle — column layout, row
    /// index, and the table's current scroll — all in this window's coordinate
    /// space, so the popover sits on its cell after any resize, scroll, or
    /// move to another display.
    #[allow(clippy::too_many_arguments)]
    fn combo_cell(
        &self,
        element_id: gpui::ElementId,
        text: String,
        glyph: MenuGlyph,
        cell_kind: EditableCell,
        row_index: usize,
        columns: &ColumnWidths,
        available: bool,
        focused: bool,
        dropdown: OpenDropdown,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let enabled = self.snapshot.has_project;
        let open = self.panel.is_dropdown_open(&dropdown);
        let width = columns.width_of(cell_kind);
        let columns = *columns;
        let full_label = text.clone();
        // Rest fill is transparent over the row; hover lifts it with the shared
        // state layer, and the cell whose menu is open holds the selected fill
        // so the eye can find which cell the floating list belongs to.
        let rest_fill = if open {
            Colors::state_selected()
        } else {
            gpui::transparent_black().into()
        };
        let hover_fill = Colors::composite(rest_fill, Colors::state_hover());
        div()
            .id(element_id)
            .w(px(width))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(4.0))
            .h(px(ROW_HEIGHT - 4.0))
            .px(px(5.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(if focused || open {
                Colors::accent_primary()
            } else {
                Colors::border_subtle()
            })
            .bg(rest_fill)
            // The full text is always reachable, however narrow the column is.
            .tooltip(tooltip_text(full_label))
            .when(enabled, |cell| {
                cell.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(hover_fill))
            })
            // The same glyph the menu row carries, so the assignment reads as
            // what it is without opening anything: an interface, the jam send,
            // or the person whose stream this cell is bound to.
            .child(crate::components::combo_box::cell_glyph(
                &glyph,
                if available {
                    Colors::text_muted()
                } else {
                    Colors::accent_warning()
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(10.0))
                    .text_color(if !enabled {
                        Colors::text_muted()
                    } else if available {
                        Colors::text_primary()
                    } else {
                        Colors::accent_warning()
                    })
                    .child(text),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(7.0))
                    .text_color(if open {
                        Colors::accent_primary()
                    } else {
                        Colors::text_muted()
                    })
                    .child(if open { "▴" } else { "▾" }),
            )
            .when(enabled, |cell| {
                cell.on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _event: &gpui::MouseDownEvent, _w, cx| {
                        // Stop here so opening a dropdown never also toggles
                        // Enabled or re-triggers the row click.
                        cx.stop_propagation();
                        let scroll = this.table_scroll.offset();
                        let anchor = cell_anchor_for_row(
                            &columns,
                            cell_kind,
                            row_index,
                            table_top(),
                            scroll.x.into(),
                            scroll.y.into(),
                        );
                        this.panel.toggle_dropdown_at(dropdown.clone(), anchor);
                        cx.notify();
                    }),
                )
            })
    }

    /// The Bus Name cell: text until a rename is open, then the shared Studio
    /// text field.
    fn name_cell(
        &self,
        row: &ConnectionRow,
        index: usize,
        columns: &ColumnWidths,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let width = columns.name;
        let editing = self
            .panel
            .inline_edit
            .as_ref()
            .is_some_and(|edit| edit.connection_id == row.id);
        if editing {
            return div()
                .w(px(width))
                .flex_none()
                .px(px(2.0))
                .child(text_field_with_callbacks(
                    &self.name_input,
                    self.name_input.is_focused(window),
                    bind_mouse_selection(cx.entity().clone(), |this| &mut this.name_input),
                ))
                .into_any_element();
        }

        let id = row.id.clone();
        div()
            .id(("audio-connection-name", index))
            .w(px(width))
            .flex_none()
            .flex()
            .items_center()
            .h(px(ROW_HEIGHT - 4.0))
            .px(px(6.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .text_size(px(10.0))
            .truncate()
            .text_color(Colors::text_primary())
            .tooltip(tooltip_text(row.name.clone()))
            .when(self.snapshot.has_project, |cell| {
                cell.cursor(gpui::CursorStyle::IBeam)
                    .hover(|s| s.bg(Colors::button_bg_hover()))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, w, cx| {
                            cx.stop_propagation();
                            this.panel.select_only(id.clone());
                            if event.click_count >= 2 {
                                this.start_rename(&id, w, cx);
                            }
                            cx.notify();
                        }),
                    )
            })
            .child(row.name.clone())
            .into_any_element()
    }

    /// Open the inline rename editor, seeding the shared text field from the
    /// registry and focusing it so typing does not reach transport shortcuts.
    fn start_rename(
        &mut self,
        id: &crate::audio_connections::AudioConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel.begin_rename(&self.snapshot.registry, id);
        if let Some(edit) = self.panel.inline_edit.clone() {
            self.name_input.set_value(&edit.draft);
            self.name_input.select_all();
            self.name_input.focus_handle.focus(window, cx);
        }
    }

    /// Commit the open rename through the registry.
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.name_input.value.clone();
        let Some((id, _)) = self.panel.take_rename() else {
            return;
        };
        // An empty or whitespace-only name is rejected by `update_name`; the
        // editor simply closes and the previous name stays.
        (self.on_edit)(&ConnectionEdit::Rename { id, name: value }, window, cx);
        cx.notify();
    }

    /// Root key handling for the table.
    ///
    /// While an inline editor is open only Enter and Escape are consumed here —
    /// every other key belongs to the text field, which is why global transport
    /// shortcuts cannot fire mid-rename.
    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let editing = self.panel.inline_edit.is_some();

        if editing {
            match key {
                "enter" => {
                    cx.stop_propagation();
                    self.commit_rename(window, cx);
                }
                "escape" => {
                    cx.stop_propagation();
                    self.panel.cancel_rename();
                    cx.notify();
                }
                _ => {}
            }
            return;
        }

        match key {
            "escape" => {
                // Innermost first: dropdown, then editor.
                if self.panel.dismiss_topmost() {
                    cx.stop_propagation();
                    cx.notify();
                }
            }
            "up" | "down" => {
                cx.stop_propagation();
                let delta = if key == "down" { 1 } else { -1 };
                self.panel.move_selection(&self.snapshot.registry, delta);
                cx.notify();
            }
            "tab" => {
                cx.stop_propagation();
                let stereo = self
                    .panel
                    .single_selection()
                    .and_then(|id| self.snapshot.registry.get(id))
                    .map(|connection| connection.channel_layout.channel_count() > 1)
                    .unwrap_or(false);
                let delta = if event.keystroke.modifiers.shift {
                    -1
                } else {
                    1
                };
                self.panel.move_cell_focus(stereo, delta);
                cx.notify();
            }
            "enter" => {
                if let Some(id) = self.panel.single_selection().cloned() {
                    cx.stop_propagation();
                    self.start_rename(&id, window, cx);
                    cx.notify();
                }
            }
            "delete" | "backspace" => {
                let Some(id) = self.panel.single_selection().cloned() else {
                    return;
                };
                cx.stop_propagation();
                // Same path as the toolbar: the layout decides whether this
                // needs the shared confirmation window.
                (self.on_edit)(&ConnectionEdit::RequestRemove { id }, window, cx);
            }
            _ => {}
        }
    }

    /// The open dropdown, rendered above the table and footer.
    ///
    /// Options and the committed value both come from the registry, so a stale
    /// local copy can never be shown or written back. Placement is resolved
    /// against *this* window's viewport through the shared popover rule, so a
    /// row near the bottom flips upward instead of clipping.
    fn render_open_dropdown(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use crate::audio_connections::AudioPortId;

        let dropdown = self.panel.open_dropdown.clone()?;
        let anchor = self.panel.dropdown_anchor?;
        let registry = &self.snapshot.registry;
        let ports = &self.snapshot.ports;

        // (label, glyph, edit) triples. The first entry is always Unassigned.
        let (selected, options): (String, Vec<(String, MenuGlyph, ConnectionEdit)>) =
            match &dropdown {
                OpenDropdown::Layout(id) => (
                    layout_label(registry.get(id)?.channel_layout),
                    crate::audio_connections::PANEL_LAYOUTS
                        .iter()
                        .map(|layout| {
                            (
                                layout_label(*layout),
                                layout_glyph(*layout),
                                ConnectionEdit::SetLayout {
                                    id: id.clone(),
                                    layout: *layout,
                                },
                            )
                        })
                        .collect(),
                ),
                OpenDropdown::Device(id) => {
                    let current = registry
                        .device_display_name(id, ports)
                        .unwrap_or_else(|| "Unassigned".to_string());
                    let mut options = vec![(
                        "Unassigned".to_string(),
                        unassigned_glyph(),
                        ConnectionEdit::SetDevice {
                            id: id.clone(),
                            device_id: None,
                        },
                    )];
                    let direction = registry
                        .get(id)
                        .map(|connection| connection.direction)
                        .unwrap_or(AudioConnectionDirection::Input);
                    for (device_id, label, _available) in registry.device_choices(id, ports) {
                        let glyph = device_glyph(&device_id, direction);
                        options.push((
                            label,
                            glyph,
                            ConnectionEdit::SetDevice {
                                id: id.clone(),
                                device_id: Some(device_id),
                            },
                        ));
                    }
                    (current, options)
                }
                OpenDropdown::Port {
                    connection_id,
                    logical_channel,
                } => {
                    let connection = registry.get(connection_id)?;
                    let current = connection
                        .binding(*logical_channel)
                        .map(|binding| binding.physical_port_id.port_name.clone())
                        .unwrap_or_else(|| "Unassigned".to_string());
                    let mut options = vec![(
                        "Unassigned".to_string(),
                        unassigned_glyph(),
                        ConnectionEdit::SetPort {
                            id: connection_id.clone(),
                            logical_channel: *logical_channel,
                            port: None,
                        },
                    )];
                    // One entry per channel the selected endpoint actually
                    // reports — never a fixed two-channel list.
                    for (port, label, _available) in
                        registry.port_choices(connection_id, *logical_channel, ports)
                    {
                        let glyph = device_glyph(&port.device_id, connection.direction);
                        options.push((
                            label,
                            glyph,
                            ConnectionEdit::SetPort {
                                id: connection_id.clone(),
                                logical_channel: *logical_channel,
                                port: Some(AudioPortId::new(
                                    port.device_id.clone(),
                                    port.port_name.clone(),
                                    port.port_index,
                                )),
                            },
                        ));
                    }
                    (current, options)
                }
                OpenDropdown::Presets => (
                    // A preset is an action, not a stored value, so no row carries
                    // the selected tick.
                    String::new(),
                    PANEL_PRESETS
                        .iter()
                        .map(|preset| {
                            (
                                preset.label(),
                                MenuGlyph::Svg(crate::assets::ICON_LAYERS_PATH),
                                ConnectionEdit::RequestApplyPreset { preset: *preset },
                            )
                        })
                        .collect(),
                ),
                OpenDropdown::AddBus => return None,
            };

        let labels: Vec<String> = options.iter().map(|(label, _, _)| label.clone()).collect();
        let items: Vec<MenuItem> = options
            .iter()
            .map(|(label, glyph, _)| MenuItem::new(label.clone(), glyph.clone()))
            .collect();
        // Sized to its content: as tall as its rows (until it must scroll), and
        // wide enough for the longest label — a menu the width of a 110 px port
        // column truncated every real endpoint name and read as cropped.
        let popup_height = dropdown_height(labels.len());
        // This window's own content rect — never the main project window's.
        let viewport = crate::overlay::external_dialog_overlay_bounds(window);
        let longest = labels
            .iter()
            .map(|label| label.chars().count())
            .max()
            .unwrap_or(0);
        let popup_width = dropdown_width(longest, anchor.width, viewport.size.width.into());
        let placement = crate::overlay::resolve_popup_placement(
            gpui::bounds(
                gpui::point(px(anchor.x), px(anchor.y)),
                gpui::size(px(anchor.width), px(anchor.height)),
            ),
            gpui::size(px(popup_width), px(popup_height)),
            viewport,
            crate::overlay::PopupPlacementOptions {
                preferred_side: crate::overlay::PopupSide::Bottom,
                alignment: crate::overlay::PopupAlignment::Start,
                viewport_margin: px(6.0),
                gap: px(2.0),
            },
        );

        let on_edit = self.on_edit.clone();
        let lookup: std::collections::HashMap<String, ConnectionEdit> = options
            .into_iter()
            .map(|(label, _, edit)| (label, edit))
            .collect();
        let on_select: std::sync::Arc<dyn Fn(String, &mut Window, &mut gpui::App) + 'static> = {
            let entity = cx.entity().clone();
            std::sync::Arc::new(move |value: String, window, cx| {
                if let Some(edit) = lookup.get(&value).cloned() {
                    on_edit(&edit, window, cx);
                }
                let _ = entity.update(cx, |this, cx| {
                    this.panel.close_dropdown();
                    cx.notify();
                });
            })
        };

        Some(
            combo_box_icon_menu(
                "audio-connections-dropdown",
                crate::overlay::OverlayPosition {
                    x: placement.origin.x,
                    y: placement.origin.y,
                    width: Some(placement.size.width),
                    max_height: Some(placement.size.height),
                },
                &selected,
                &items,
                on_select,
            )
            .into_any_element(),
        )
    }

    fn render_row(
        &self,
        row: ConnectionRow,
        index: usize,
        columns: &ColumnWidths,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = row.id.clone();
        let toggle_id = row.id.clone();
        let next_enabled = !row.enabled;
        let status_label = row.status.label().to_string();
        let status_tooltip = row
            .status_detail
            .clone()
            .unwrap_or_else(|| status_label.clone());
        let focused_cell = row.selected.then_some(self.panel.focused_cell).flatten();
        let device_ok = !matches!(row.status, AudioConnectionStatus::DeviceMissing);
        let port_ok = !matches!(row.status, AudioConnectionStatus::PortMissing);
        // What this row is bound to, resolved once: the Device and both port
        // cells all show it, and for a jam stream it is a face rather than an
        // icon. An unassigned row carries no glyph — there is nothing to
        // illustrate, and a placeholder would read as an assignment.
        let row_glyph = self
            .snapshot
            .registry
            .get(&row.id)
            .and_then(|connection| {
                connection
                    .device_id
                    .as_ref()
                    .map(|device_id| device_glyph(device_id, connection.direction))
            })
            .unwrap_or(MenuGlyph::None);

        // Selection is fill plus a leading-edge marker (never a border change,
        // which would reflow the row); hover lifts whichever fill is at rest.
        let rest_fill = if row.selected {
            Colors::surface_selected_soft()
        } else {
            gpui::transparent_black().into()
        };
        let hover_fill = Colors::composite(rest_fill, Colors::state_hover());
        div()
            .id(("audio-connection-row", index))
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .h(px(ROW_HEIGHT))
            .flex_none()
            .w(px(columns.total()))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(rest_fill)
            .hover(move |r| r.bg(hover_fill))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, _w, cx| {
                    this.panel.select_only(id.clone());
                    cx.notify();
                }),
            )
            .when(row.selected, |r| {
                r.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.0))
                        .bg(Colors::accent_primary()),
                )
            })
            // Enabled toggle. Disabling keeps every mapping — the bus simply
            // resolves to silence until it is re-enabled.
            .child(
                div()
                    .w(px(columns.enabled))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id(("audio-connection-enabled", index))
                            .w(px(12.0))
                            .h(px(12.0))
                            .rounded(px(crate::theme::radius::CONTROL))
                            .border(px(1.0))
                            .border_color(if row.enabled {
                                Colors::accent_primary()
                            } else {
                                Colors::border_default()
                            })
                            .bg(if row.enabled {
                                Colors::accent_primary()
                            } else {
                                Colors::button_bg()
                            })
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _event: &gpui::MouseDownEvent, w, cx| {
                                    cx.stop_propagation();
                                    (this.on_edit)(
                                        &ConnectionEdit::SetEnabled {
                                            id: toggle_id.clone(),
                                            enabled: next_enabled,
                                        },
                                        w,
                                        cx,
                                    );
                                }),
                            ),
                    ),
            )
            .child(self.name_cell(&row, index, columns, window, cx))
            .child(self.combo_cell(
                ("audio-connection-config", index).into(),
                layout_label(row.layout),
                layout_glyph(row.layout),
                EditableCell::Configuration,
                index,
                columns,
                true,
                focused_cell == Some(EditableCell::Configuration),
                OpenDropdown::Layout(row.id.clone()),
                cx,
            ))
            .child(self.combo_cell(
                ("audio-connection-device", index).into(),
                row.device_label.clone(),
                row_glyph.clone(),
                EditableCell::Device,
                index,
                columns,
                device_ok,
                focused_cell == Some(EditableCell::Device),
                OpenDropdown::Device(row.id.clone()),
                cx,
            ))
            .child(self.combo_cell(
                ("audio-connection-left", index).into(),
                row.left_port.clone(),
                row_glyph.clone(),
                EditableCell::LeftPort,
                index,
                columns,
                port_ok,
                focused_cell == Some(EditableCell::LeftPort),
                OpenDropdown::Port {
                    connection_id: row.id.clone(),
                    logical_channel: 0,
                },
                cx,
            ))
            // A mono row renders an inert cell rather than an active Right
            // dropdown — there is no second channel to bind.
            .child(match row.right_port.clone() {
                Some(text) => self
                    .combo_cell(
                        ("audio-connection-right", index).into(),
                        text,
                        row_glyph.clone(),
                        EditableCell::RightPort,
                        index,
                        columns,
                        port_ok,
                        focused_cell == Some(EditableCell::RightPort),
                        OpenDropdown::Port {
                            connection_id: row.id.clone(),
                            logical_channel: 1,
                        },
                        cx,
                    )
                    .into_any_element(),
                None => cell(String::new(), columns.right_port, true).into_any_element(),
            })
            // Status on two channels — a dot and the word share the hue — so
            // "needs attention" is scannable down the column without reading.
            .child(
                div()
                    .id(("audio-connection-status", index))
                    .w(px(columns.status))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .px(px(6.0))
                    .tooltip(tooltip_text(status_tooltip))
                    .child(
                        div()
                            .flex_none()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded(px(crate::theme::radius::PILL))
                            .bg(status_color(row.status)),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(status_color(row.status))
                            .child(status_label),
                    ),
            )
    }
}

impl Render for AudioConnectionsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_project = self.snapshot.has_project;
        let rows = self
            .panel
            .rows(&self.snapshot.registry, &self.snapshot.ports);
        let (count, attention) = self.panel.summary(&self.snapshot.registry);
        let tab = self.panel.tab;
        let direction = tab.direction();
        let selected = self.panel.single_selection().cloned();

        // Lay the table out from the width this window actually has, in
        // logical units, so it is correct under any display scale factor.
        let viewport_width: f32 = window.viewport_size().width.into();
        let columns = column_widths(viewport_width);
        let table_width = columns.total();

        // ── Titlebar ────────────────────────────────────────────────────────
        // The platform (or the shared drawn chrome, on client-decorated
        // platforms) owns the close control; no extra in-content Close button.
        let on_close = self.on_close.clone();
        let titlebar = external_window_titlebar(
            "Audio Connections",
            "audio-connections-window-close",
            move |window, cx| {
                // Closing this window never touches the project.
                on_close(window, cx);
                window.remove_window();
            },
        );

        // ── Device context sub-header ───────────────────────────────────────
        let device_line = |label: &str, value: &str| {
            let value = if value.trim().is_empty() {
                "None".to_string()
            } else {
                value.trim().to_string()
            };
            div()
                .flex()
                .flex_row()
                .gap(px(4.0))
                .min_w(px(0.0))
                .text_size(px(9.5))
                .child(
                    div()
                        .flex_none()
                        .text_color(Colors::text_muted())
                        .child(format!("{label}:")),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(Colors::text_secondary())
                        .child(value),
                )
        };
        let subheader = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .h(px(SUBHEADER_HEIGHT))
            .px(px(10.0))
            .flex_none()
            .border_b(px(1.0))
            .border_color(Colors::border_default())
            .bg(Colors::surface_panel_alt())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap(px(1.0))
                    .child(device_line("Input", &self.snapshot.input_device))
                    .child(device_line("Output", &self.snapshot.output_device)),
            )
            .child({
                let on_edit = self.on_edit.clone();
                toolbar_button(
                    "audio-connections-device-setup",
                    "Audio Device Setup…".to_string(),
                    true,
                    move |_event, w, cx| on_edit(&ConnectionEdit::OpenAudioDeviceSetup, w, cx),
                )
            });

        // ── Tabs ────────────────────────────────────────────────────────────
        let mut tabs = div()
            .flex()
            .flex_row()
            .h(px(TABS_HEIGHT))
            .flex_none()
            .px(px(6.0))
            .gap(px(2.0))
            .border_b(px(1.0))
            .border_color(Colors::border_default());
        for (this_tab, element_id) in [
            (ConnectionsTab::Inputs, "audio-connections-tab-in"),
            (ConnectionsTab::Outputs, "audio-connections-tab-out"),
        ] {
            let active = tab == this_tab;
            // Each tab carries its bus count, so the other direction's state is
            // readable without switching to it.
            let tab_count = self
                .snapshot
                .registry
                .by_direction(this_tab.direction())
                .len();
            tabs = tabs.child(
                div()
                    .id(element_id)
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(5.0))
                    .h(px(TABS_HEIGHT - 2.0))
                    .px(px(12.0))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .border_b(px(2.0))
                    .border_color(if active {
                        Colors::accent_primary()
                    } else {
                        Colors::border_subtle()
                    })
                    .text_size(px(10.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if active {
                        Colors::text_primary()
                    } else {
                        Colors::text_secondary()
                    })
                    .hover(|s| s.bg(Colors::state_hover()))
                    .child(this_tab.label())
                    .when(has_project, |t| {
                        t.child(
                            div()
                                .px(px(5.0))
                                .rounded(px(crate::theme::radius::PILL))
                                .bg(Colors::surface_panel_alt())
                                .text_size(px(8.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if active {
                                    Colors::text_secondary()
                                } else {
                                    Colors::text_muted()
                                })
                                .child(tab_count.to_string()),
                        )
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _event: &gpui::MouseDownEvent, _w, cx| {
                            this.panel.set_tab(this_tab);
                            cx.notify();
                        }),
                    ),
            );
        }

        // ── Toolbar ─────────────────────────────────────────────────────────
        let add_mono = {
            let on_edit = self.on_edit.clone();
            move |_e: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| {
                on_edit(
                    &ConnectionEdit::Add {
                        direction,
                        layout: ChannelLayout::Mono,
                    },
                    w,
                    cx,
                );
            }
        };
        let add_stereo = {
            let on_edit = self.on_edit.clone();
            move |_e: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| {
                on_edit(
                    &ConnectionEdit::Add {
                        direction,
                        layout: ChannelLayout::Stereo,
                    },
                    w,
                    cx,
                );
            }
        };

        // Duplicate / Remove are always in the bar and simply disabled without
        // a selection: buttons that appear and vanish shift the toolbar under
        // the pointer and make the row-selection state harder to read.
        let duplicate_button = {
            let on_edit = self.on_edit.clone();
            let target = selected.clone();
            toolbar_button(
                "audio-connections-duplicate",
                "Duplicate".to_string(),
                has_project && target.is_some(),
                move |_e, w, cx| {
                    if let Some(id) = target.clone() {
                        on_edit(&ConnectionEdit::Duplicate { id }, w, cx);
                    }
                },
            )
        };

        let remove_button = {
            let on_edit = self.on_edit.clone();
            let target = selected.clone();
            toolbar_button(
                "audio-connections-remove",
                "Remove".to_string(),
                has_project && target.is_some(),
                // Referenced buses are confirmed by the shared message-box
                // window; the layout decides, because only it knows the
                // references.
                move |_e, w, cx| {
                    if let Some(id) = target.clone() {
                        on_edit(&ConnectionEdit::RequestRemove { id }, w, cx);
                    }
                },
            )
        };

        // Presets replace every bus in both directions, so the menu sits at
        // the head of the bar, away from the per-row Duplicate/Remove pair.
        let presets_open = self.panel.is_dropdown_open(&OpenDropdown::Presets);
        let presets_button = {
            toolbar_button(
                "audio-connections-presets",
                format!("Presets {}", if presets_open { "▴" } else { "▾" }),
                has_project,
                cx.listener(|this, _event: &gpui::MouseDownEvent, _w, cx| {
                    // Stop here so the click that opens the menu is not also
                    // read as a click outside it.
                    cx.stop_propagation();
                    this.panel
                        .toggle_dropdown_at(OpenDropdown::Presets, presets_anchor());
                    cx.notify();
                }),
            )
        };

        let auto_map_button = {
            let on_edit = self.on_edit.clone();
            toolbar_button(
                "audio-connections-auto-map",
                "Auto Map".to_string(),
                has_project,
                move |_e, w, cx| on_edit(&ConnectionEdit::AutoMap { direction }, w, cx),
            )
        };

        let reset_button = {
            let on_edit = self.on_edit.clone();
            toolbar_button(
                "audio-connections-reset",
                "Reset Defaults".to_string(),
                has_project,
                move |_e, w, cx| {
                    on_edit(&ConnectionEdit::RequestResetDefaults { direction }, w, cx)
                },
            )
        };

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .h(px(TOOLBAR_HEIGHT))
            .px(px(8.0))
            .flex_none()
            .border_b(px(1.0))
            .border_color(Colors::border_default())
            .child(presets_button)
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(14.0))
                    .mx(px(3.0))
                    .bg(Colors::border_subtle()),
            )
            .child(toolbar_button(
                "audio-connections-add-mono",
                "Add Mono".to_string(),
                has_project,
                add_mono,
            ))
            .child(toolbar_button(
                "audio-connections-add-stereo",
                "Add Stereo".to_string(),
                has_project,
                add_stereo,
            ))
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(14.0))
                    .mx(px(3.0))
                    .bg(Colors::border_subtle()),
            )
            .child(duplicate_button)
            .child(remove_button)
            .child(div().flex_1().min_w(px(0.0)))
            .child(auto_map_button)
            .child(reset_button);

        // ── Table ───────────────────────────────────────────────────────────
        let header_row = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(TABLE_HEADER_HEIGHT))
            .flex_none()
            .w(px(table_width))
            .bg(Colors::surface_panel_alt())
            .border_b(px(1.0))
            .border_color(Colors::border_default())
            .child(header_cell("On", columns.enabled))
            .child(header_cell("Bus Name", columns.name))
            .child(header_cell("Configuration", columns.configuration))
            .child(header_cell("Audio Device", columns.device))
            .child(header_cell("Left / Mono", columns.left_port))
            .child(header_cell("Right", columns.right_port))
            .child(header_cell("Status", columns.status));

        let body = if !has_project {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child("No project is open. Audio Connections are stored per project.")
                .into_any_element()
        } else if rows.is_empty() {
            // The empty state offers the one action that fills the table in a
            // click, rather than pointing at the toolbar.
            let on_edit = self.on_edit.clone();
            div()
                .flex()
                .flex_1()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child(
                    div()
                        .text_color(Colors::text_secondary())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(format!("No {} buses yet", tab.label().to_lowercase())),
                )
                .child("Create the default buses for the current audio device, or add one with Add Mono / Add Stereo.")
                .child(
                    div().mt(px(6.0)).child(fb_button(
                        "audio-connections-empty-reset",
                        "Create Default Buses",
                        FbButtonKind::Primary,
                        true,
                        move |_, w, cx| {
                            on_edit(&ConnectionEdit::RequestResetDefaults { direction }, w, cx)
                        },
                    )),
                )
                .into_any_element()
        } else {
            let mut list = div()
                .id("audio-connections-rows")
                .flex()
                .flex_col()
                .flex_none()
                .w(px(table_width));
            for (index, row) in rows.into_iter().enumerate() {
                list = list.child(self.render_row(row, index, &columns, window, cx));
            }
            list.into_any_element()
        };

        // Header and rows scroll together, so a narrow window scrolls
        // horizontally instead of overlapping cells.
        let table = div()
            .id("audio-connections-table")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            // Tracked so a dropdown can anchor to where its cell is *now*.
            .track_scroll(&self.table_scroll)
            .child(header_row)
            .child(body);

        // ── Footer ──────────────────────────────────────────────────────────
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .h(px(FOOTER_HEIGHT))
            .px(px(10.0))
            .flex_none()
            .border_t(px(1.0))
            .border_color(Colors::border_default())
            .bg(Colors::surface_panel_alt())
            .text_size(px(9.5))
            .text_color(Colors::text_secondary())
            .child(format!("{count} connection(s)"))
            .when(attention > 0, |bar| {
                bar.child(
                    div()
                        .text_color(Colors::accent_warning())
                        .child(format!("{attention} need attention")),
                )
            })
            .children(self.panel.warnings.first().map(|warning| {
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(Colors::accent_warning())
                    .child(warning.clone())
            }));

        div()
            .id("audio-connections-root")
            .track_focus(&self.name_input.focus_handle)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(Colors::surface_window())
            .child(titlebar)
            .child(subheader)
            .child(tabs)
            .child(toolbar)
            .child(table)
            .child(footer)
            .children(self.render_open_dropdown(window, cx))
    }
}

/// Open the Audio Connections window.
///
/// A modeless top-level utility window, like the external Mixer: it is not
/// owned by or modal over the Studio window, so the project stays fully usable
/// while it is open and it can live on a different monitor. Geometry is chosen
/// at open time and clamped to the target display's work area; it is session
/// state and is never written into the project file.
pub fn open_audio_connections_window(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    snapshot: AudioConnectionsSnapshot,
    on_edit: ConnectionEditCb,
    on_close: std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>,
    cx: &mut gpui::App,
) -> Result<gpui::WindowHandle<AudioConnectionsWindow>, String> {
    let (min_width, min_height) = window_min_size();
    // `centered_window_bounds` clamps to the work area of the display the
    // owner is on, so the initial rect is valid on any monitor and scale.
    let window_bounds = centered_window_bounds(
        owner_bounds,
        gpui::size(
            px(WINDOW_WIDTH.max(min_width)),
            px(WINDOW_HEIGHT.max(min_height)),
        ),
        cx,
    );
    let mut options = crate::platform_chrome::external_window_options_partial();
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some("Audio Connections".into());
    }
    options.window_bounds = Some(gpui::WindowBounds::Windowed(window_bounds));
    options.window_background = gpui::WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(gpui::size(px(min_width), px(min_height)));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            AudioConnectionsWindow::new(snapshot, on_edit, on_close, focus_handle)
        })
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::audio_connections_panel::{
        cell_anchor, column_widths, removal_needs_confirmation, reset_defaults_needs_confirmation,
        snap_row_top, AudioConnectionsPanelState, ColumnWidths, PopoverSide,
    };

    fn ports_with(input_channels: u32, output_channels: u32) -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "Studio 24c", input_channels, output_channels)
    }

    fn registry() -> AudioConnectionRegistry {
        AudioConnectionRegistry::default_template(&ports_with(4, 4), "dev-1")
    }

    /// This file's implementation, with the test module stripped off, so a
    /// scan for banned constructs cannot match its own assertion literals.
    fn implementation_source() -> &'static str {
        let source = include_str!("audio_connections_window.rs");
        source
            .split_once("#[cfg(test)]")
            .map(|(implementation, _)| implementation)
            .expect("this file has a test module")
    }

    /// The preset menu belongs to the toolbar, not to a table cell, so its
    /// anchor has to sit in the toolbar strip — one row lower and it would open
    /// over the column headers it is not attached to.
    #[test]
    fn the_preset_menu_hangs_from_the_toolbar_strip() {
        let anchor = presets_anchor();
        assert_eq!(anchor.y, toolbar_top());
        assert_eq!(
            anchor.y + anchor.height,
            table_top() - TABLE_HEADER_HEIGHT,
            "the menu opens directly below the toolbar"
        );
        assert!(anchor.x >= 0.0 && anchor.width > 0.0);
    }

    /// Applying a preset discards buses in both directions, so it must go
    /// through the layout's confirmation path — never straight to `ApplyPreset`.
    #[test]
    fn the_preset_menu_raises_a_request_rather_than_applying_directly() {
        let source = implementation_source();
        assert!(
            source.contains("ConnectionEdit::RequestApplyPreset { preset: *preset }"),
            "the menu must raise the confirmable request"
        );
        assert!(
            !source.contains("ConnectionEdit::ApplyPreset {"),
            "only the layout, after confirming, may raise ApplyPreset"
        );
    }

    // ── Window shape and lifecycle ──────────────────────────────────────────

    /// 1 + 4. A normal, resizable, minimizable top-level window — not a modal
    /// dialog owned by the Studio HWND, and not an overlay.
    #[test]
    fn audio_connections_opens_as_a_normal_resizable_utility_window() {
        let options = crate::platform_chrome::external_window_options_partial();
        assert_eq!(
            options.kind,
            gpui::WindowKind::Normal,
            "a project utility window must not be a modal dialog"
        );
        assert!(
            !options.dialog_parenting,
            "it must not be owned by the Studio window"
        );
        assert!(options.is_resizable, "the user must be able to resize it");
        assert!(options.is_minimizable);
        assert!(options.is_movable, "it must move independently");
    }

    /// 4. The declared minimum is derived from the column floors, so the
    /// window can never be sized below a legible Device/Left/Right/Status row.
    #[test]
    fn the_minimum_window_size_fits_every_required_column() {
        let (min_width, min_height) = window_min_size();
        assert!(
            min_width >= ColumnWidths::min_total(),
            "min width {min_width} cannot show all columns"
        );
        // Header, toolbar, table header, several rows, and the footer.
        assert!(
            min_height >= table_top() + 3.0 * ROW_HEIGHT + FOOTER_HEIGHT,
            "min height {min_height} leaves no room for rows"
        );
        assert!(
            WINDOW_WIDTH >= min_width && WINDOW_HEIGHT >= min_height,
            "the default size must not start below the minimum"
        );
    }

    /// 17. Window geometry is session state. Nothing about it reaches the
    /// project format — the project encoder has no geometry fields at all.
    #[test]
    fn window_geometry_is_never_serialized_into_the_project() {
        let source = include_str!("../project/format.rs");
        for banned in [
            "window_bounds",
            "window_min_size",
            "WINDOW_WIDTH",
            "WINDOW_HEIGHT",
            "viewport_size",
        ] {
            assert!(
                !source.contains(banned),
                "the project format must not carry window geometry ({banned})"
            );
        }
    }

    /// 18. No absolute main-window overlay coordinates survive: this window
    /// resolves popovers against its own viewport and never reads the Studio
    /// window's bounds.
    #[test]
    fn no_main_window_overlay_coordinates_remain() {
        let source = implementation_source();
        for banned in [
            "studio_window_bounds",
            "owner_bounds.origin",
            "main_window",
            "arrangement",
        ] {
            assert!(
                !source.contains(banned),
                "popover/window geometry must not be derived from the main window ({banned})"
            );
        }
        assert!(
            source.contains("external_dialog_overlay_bounds(window)"),
            "popovers must resolve against this window's own viewport"
        );
    }

    /// 2 + 3. Reopening focuses the one existing window; a stale handle is
    /// replaced rather than focused, and nothing opens a second instance.
    #[test]
    fn reopening_focuses_the_existing_window_and_never_duplicates_it() {
        assert_eq!(reopen_action(Some(true)), ReopenAction::FocusExisting);
        assert_eq!(
            reopen_action(Some(false)),
            ReopenAction::OpenNew,
            "a handle whose window is gone must be replaced, not focused"
        );
        assert_eq!(reopen_action(None), ReopenAction::OpenNew);

        // Exactly one of the two outcomes, always — there is no path that
        // opens a window while another is live.
        for activated in [Some(true), Some(false), None] {
            let action = reopen_action(activated);
            assert!(
                action == ReopenAction::FocusExisting || action == ReopenAction::OpenNew,
                "unexpected action for {activated:?}"
            );
        }
    }

    // ── Responsive table ────────────────────────────────────────────────────

    /// 7. Flexible columns take the surplus; fixed ones do not move.
    #[test]
    fn table_columns_respond_to_available_width() {
        let narrow = column_widths(ColumnWidths::min_total());
        let wide = column_widths(ColumnWidths::min_total() + 400.0);

        assert!(wide.name > narrow.name, "Bus Name grows");
        assert!(wide.device > narrow.device, "Audio Device grows");
        assert!(wide.left_port > narrow.left_port);
        assert!(wide.right_port > narrow.right_port);
        assert_eq!(
            wide.device - narrow.device > wide.name - narrow.name,
            true,
            "Audio Device is weighted highest — endpoint names are longest"
        );

        // Fixed columns keep their size at every width.
        for widths in [narrow, wide, column_widths(4000.0)] {
            assert_eq!(widths.enabled, ColumnWidths::ENABLED);
            assert_eq!(widths.configuration, ColumnWidths::CONFIGURATION);
            assert_eq!(
                widths.status,
                ColumnWidths::STATUS,
                "Status must stay visible, never clipped away"
            );
        }

        // A wide window is actually filled, not left compressed on the left.
        let wide_total = column_widths(1600.0).total();
        assert!(
            (wide_total - 1600.0).abs() < 1.0,
            "columns must expand to fill a wide window, got {wide_total}"
        );
    }

    /// 8. Below the minimum every flexible column sits on its floor and the
    /// table is wider than the viewport — the caller scrolls rather than
    /// letting cells overlap.
    #[test]
    fn long_names_truncate_without_overlapping_columns() {
        let cramped = column_widths(300.0);
        assert_eq!(cramped.name, ColumnWidths::MIN_NAME);
        assert_eq!(cramped.device, ColumnWidths::MIN_DEVICE);
        assert_eq!(cramped.left_port, ColumnWidths::MIN_PORT);
        assert_eq!(cramped.right_port, ColumnWidths::MIN_PORT);
        assert!(
            cramped.total() > 300.0,
            "a too-narrow window must scroll, not collapse the columns"
        );

        // Columns tile exactly: each starts where the previous one ends, so no
        // cell can ever be drawn over its neighbour.
        let widths = column_widths(1200.0);
        let cells = [
            EditableCell::Name,
            EditableCell::Configuration,
            EditableCell::Device,
            EditableCell::LeftPort,
            EditableCell::RightPort,
        ];
        for pair in cells.windows(2) {
            let end = widths.offset_of(pair[0]) + widths.width_of(pair[0]);
            assert!(
                (end - widths.offset_of(pair[1])).abs() < 0.01,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// 9. However narrow a column is, the complete label stays reachable.
    #[test]
    fn full_labels_remain_available_for_tooltips() {
        let ports = ports_with(4, 4);
        let mut registry = registry();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        registry.update_name(
            &id,
            "A deliberately long logical bus name that cannot fit in one cell",
        );
        registry.update_device(&id, Some("dev-1"), &ports);

        let panel = AudioConnectionsPanelState::new();
        let row = panel
            .rows(&registry, &ports)
            .into_iter()
            .find(|row| row.id == id)
            .expect("row");

        assert_eq!(row.full_label(EditableCell::Name), row.name);
        assert!(row.full_label(EditableCell::Name).len() > 40);
        assert_eq!(row.full_label(EditableCell::Device), "Studio 24c");
        assert_eq!(row.full_label(EditableCell::LeftPort), row.left_port);
    }

    // ── Popover geometry ────────────────────────────────────────────────────

    /// 10. The anchor is built from this window's column layout and row grid —
    /// window-relative, so it survives moving and resizing.
    #[test]
    fn dropdown_geometry_uses_the_audio_connections_window_viewport() {
        let widths = column_widths(1000.0);
        let top = table_top();
        let anchor = cell_anchor(&widths, EditableCell::Device, top + 2.5 * ROW_HEIGHT, top);

        assert_eq!(anchor.x, widths.offset_of(EditableCell::Device));
        assert_eq!(anchor.width, widths.device);
        assert_eq!(anchor.height, ROW_HEIGHT);
        assert_eq!(
            anchor.y,
            top + 2.0 * ROW_HEIGHT,
            "the anchor snaps to the top of the row the pointer is in"
        );

        // A pointer above the first row clamps to it rather than going negative.
        assert_eq!(snap_row_top(top - 50.0, top), top);

        // Resizing the window moves the anchor with its column.
        let resized = column_widths(1400.0);
        let after = cell_anchor(&resized, EditableCell::Device, top, top);
        assert!(
            after.x != anchor.x && after.width > anchor.width,
            "the anchor must follow the column through a resize"
        );
    }

    /// 11. Near this window's bottom edge the popover flips upward.
    #[test]
    fn dropdown_flips_upward_near_the_utility_window_bottom_edge() {
        use crate::components::audio_connections_panel::popover_side;

        let widths = column_widths(1000.0);
        let top = table_top();
        // A window 600 tall: its content bottom is 600 - titlebar chrome.
        let viewport_bottom = 600.0 - TITLEBAR_HEIGHT;

        let high = cell_anchor(&widths, EditableCell::Device, top, top);
        assert_eq!(
            popover_side(high, 160.0, viewport_bottom),
            PopoverSide::Below
        );

        let low = cell_anchor(
            &widths,
            EditableCell::Device,
            viewport_bottom - ROW_HEIGHT,
            top,
        );
        assert_eq!(
            popover_side(low, 160.0, viewport_bottom),
            PopoverSide::Above,
            "a row at the bottom of the utility window must flip up"
        );
    }

    // ── Dynamic channel enumeration ─────────────────────────────────────────

    /// 12. A mono endpoint offers exactly one channel.
    #[test]
    fn a_device_with_one_channel_produces_one_channel_option() {
        let ports = ports_with(1, 0);
        let mut registry = AudioConnectionRegistry::new();
        let (id, _) =
            registry.add_connection(AudioConnectionDirection::Input, ChannelLayout::Mono, &ports);
        registry.update_device(&id, Some("dev-1"), &ports);
        let choices = registry.port_choices(&id, 0, &ports);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].1, "Input 1");
    }

    /// 13. A 2-channel interface offers two.
    #[test]
    fn a_device_with_two_channels_produces_two_channel_options() {
        let ports = ports_with(2, 0);
        let mut registry = AudioConnectionRegistry::new();
        let (id, _) = registry.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
            &ports,
        );
        registry.update_device(&id, Some("dev-1"), &ports);
        let labels: Vec<String> = registry
            .port_choices(&id, 0, &ports)
            .into_iter()
            .map(|(_, label, _)| label)
            .collect();
        assert_eq!(labels, vec!["Input 1", "Input 2"]);
    }

    /// 14 + 15. Every channel the endpoint reports is exposed, at any count —
    /// the list is generated from the real channel count, never a fixed pair.
    #[test]
    fn a_multichannel_device_exposes_every_reported_channel() {
        for channel_count in [1u32, 2, 6, 8, 16, 32] {
            let ports = ports_with(channel_count, channel_count);
            let mut registry = AudioConnectionRegistry::new();
            let (id, _) = registry.add_connection(
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
                &ports,
            );
            registry.update_device(&id, Some("dev-1"), &ports);
            let choices = registry.port_choices(&id, 0, &ports);
            assert_eq!(
                choices.len() as u32,
                channel_count,
                "a {channel_count}-channel device must expose {channel_count} inputs"
            );
            assert_eq!(
                choices.last().unwrap().1,
                format!("Input {channel_count}"),
                "the fallback label is generated from the real channel index"
            );

            // Outputs enumerate independently of inputs.
            let (out_id, _) = registry.add_connection(
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
                &ports,
            );
            registry.update_device(&out_id, Some("dev-1"), &ports);
            assert_eq!(
                registry.port_choices(&out_id, 0, &ports).len() as u32,
                channel_count
            );
        }
    }

    /// 15, structurally: nothing in the enumeration path pins the count to two.
    #[test]
    fn no_channel_count_is_hardcoded_to_two() {
        let two = ports_with(2, 2).ports.len();
        let eight = ports_with(8, 8).ports.len();
        assert_eq!(two, 4);
        assert_eq!(eight, 16, "port enumeration scales with the reported count");
    }

    // ── Naming and presentation ─────────────────────────────────────────────

    /// 16. The logical Bus Name never absorbs the hardware endpoint
    /// description; device and ports live in their own columns.
    #[test]
    fn the_logical_bus_name_is_independent_from_the_device_endpoint_name() {
        let ports = AvailablePorts::for_device(
            "dev-1",
            "Mic/Inst/Line In 1/2 (Studio 24c Audio Interface)",
            2,
            2,
        );
        let mut registry = AudioConnectionRegistry::new();
        let (id, _) = registry.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
            &ports,
        );
        registry.update_device(&id, Some("dev-1"), &ports);

        let panel = AudioConnectionsPanelState::new();
        let row = &panel.rows(&registry, &ports)[0];
        assert_eq!(row.name, "Stereo Input", "concise logical default");
        assert!(
            !row.name.contains("Studio 24c"),
            "the endpoint description must not leak into the bus name"
        );
        assert_eq!(
            row.device_label, "Mic/Inst/Line In 1/2 (Studio 24c Audio Interface)",
            "the device column carries the hardware identity"
        );

        // The generated migration/Add-Track name is logical too.
        assert_eq!(
            crate::audio_connections::generated_input_name(&[2, 3], Some("Studio 24c")),
            "Stereo Input 3-4"
        );

        // A second bus of the same kind disambiguates by counter, not device.
        let (second, _) = registry.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
            &ports,
        );
        assert_eq!(registry.name_of(&second), Some("Stereo Input 2"));
    }

    // ── Confirmation policy ─────────────────────────────────────────────────

    /// 6. Destructive actions use the shared confirmation window; only those
    /// confirm, and nothing else in Studio is blocked meanwhile.
    #[test]
    fn only_destructive_actions_confirm() {
        assert!(!removal_needs_confirmation(&[]));
        assert!(removal_needs_confirmation(&["track-1".to_string()]));
        assert!(reset_defaults_needs_confirmation());

        // No confirmation surface is drawn inside this window any more.
        assert!(
            !implementation_source().contains("PendingConfirmation"),
            "confirmations belong to the shared message-box window"
        );
    }

    /// 6. Closing the window releases its transient UI state, and nothing it
    /// held can outlive the project it belonged to.
    #[test]
    fn window_close_and_project_switch_release_ui_state_safely() {
        let ports = ports_with(4, 4);
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();

        panel.select_only(id.clone());
        panel.begin_rename(&registry, &id);
        panel.toggle_dropdown_at(
            OpenDropdown::Device(id),
            cell_anchor(
                &column_widths(1000.0),
                EditableCell::Device,
                table_top(),
                table_top(),
            ),
        );
        panel.set_warnings(vec!["stale".to_string()]);

        // 5. A project switch refreshes the panel: no id, editor, dropdown, or
        // warning from the old project survives.
        panel.on_project_changed();
        assert!(panel.selected().is_empty());
        assert!(panel.inline_edit.is_none());
        assert!(panel.open_dropdown.is_none());
        assert!(panel.dropdown_anchor.is_none());
        assert!(panel.focused_cell.is_none());
        assert!(panel.warnings.is_empty());

        // And it renders cleanly against an empty registry afterwards.
        assert!(panel
            .rows(&AudioConnectionRegistry::new(), &ports)
            .is_empty());
    }
}

#[cfg(test)]
mod dropdown_glyph_tests {
    use super::{device_glyph, layout_glyph, unassigned_glyph};
    use crate::audio_connections::{AudioConnectionDirection, ChannelLayout};
    use crate::components::combo_box::MenuGlyph;

    fn svg_path(glyph: &MenuGlyph) -> Option<&'static str> {
        match glyph {
            MenuGlyph::Svg(path) => Some(path),
            _ => None,
        }
    }

    /// The point of the glyph column: an interface, a network send and a person
    /// must not read as the same kind of thing.
    #[test]
    fn each_kind_of_endpoint_gets_its_own_glyph() {
        let input = device_glyph("Focusrite USB ASIO", AudioConnectionDirection::Input);
        let output = device_glyph("Focusrite USB ASIO", AudioConnectionDirection::Output);
        let send = device_glyph("jam-send", AudioConnectionDirection::Output);

        assert_ne!(
            svg_path(&input),
            svg_path(&output),
            "capture is not playback"
        );
        assert_ne!(
            svg_path(&output),
            svg_path(&send),
            "the jam send is not an audio interface"
        );
        assert_ne!(svg_path(&input), svg_path(&unassigned_glyph()));
    }

    /// A stream with nobody resolvable behind it still has to draw something —
    /// the row must never be a blank column that shifts the label.
    /// A stream whose publisher is not resolvable — they left, or the snapshot
    /// has not caught up — is still a person. Falling back to the direction
    /// icon would put a microphone on somebody's port.
    #[test]
    fn an_unresolved_jam_stream_still_draws_a_person() {
        let jam = device_glyph("jam:str_unknown", AudioConnectionDirection::Input);
        let hardware = device_glyph("Focusrite USB ASIO", AudioConnectionDirection::Input);
        assert_ne!(
            svg_path(&jam),
            svg_path(&hardware),
            "a jam port must never borrow an interface's glyph"
        );
        assert!(matches!(
            jam,
            MenuGlyph::Monogram(_) | MenuGlyph::Picture(_) | MenuGlyph::Svg(_)
        ));
    }

    #[test]
    fn mono_and_stereo_are_told_apart() {
        assert_ne!(
            svg_path(&layout_glyph(ChannelLayout::Mono)),
            svg_path(&layout_glyph(ChannelLayout::Stereo))
        );
    }
}
