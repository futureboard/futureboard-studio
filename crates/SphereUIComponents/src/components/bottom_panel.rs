use crate::components::controls::{fb_dock_tab, FbDockPlanes};
use crate::theme::Colors;
use gpui::{div, px, App, Empty, IntoElement, ParentElement, Render, Styled, Window};

/// See `bottom_panel_shell::dock_planes` — the same two planes, because this is
/// the same dock.
fn dock_planes() -> FbDockPlanes {
    FbDockPlanes {
        strip: Colors::bottom_panel_header_bg(),
        body: Colors::bottom_panel_bg(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BottomTab {
    Mixer,
    Editor,
    EffectEditor,
}

/// Persistent state for the resizable bottom panel.
/// `resize_start_*` are transient — recorded on mouse-down so the on_drag_move
/// handler can recompute height as a pure function of the current mouse Y.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BottomPanelState {
    pub height_px: f32,
    pub min_height_px: f32,
    pub max_height_px: f32,
    pub is_resizing: bool,
    pub resize_start_y: f32,
    pub resize_start_height: f32,
}

impl Default for BottomPanelState {
    fn default() -> Self {
        Self {
            height_px: 280.0,
            min_height_px: 180.0,
            max_height_px: 720.0,
            is_resizing: false,
            resize_start_y: 0.0,
            resize_start_height: 0.0,
        }
    }
}

/// Zero-sized marker used as the drag payload for the bottom panel resize handle.
#[derive(Clone, Copy, Debug, Default)]
pub struct BottomPanelResizeDrag;

impl Render for BottomPanelResizeDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

// ─── Sub-components for Editor ───────────────────────────────────────────────

/// Fallback for the docked Editor tab when no real Piano Roll element is
/// supplied. Previously this drew mock MIDI notes; that violated the
/// "no mock timeline rendering" rule, so it is now an empty placeholder.
/// The normal layout always passes the real `PianoRoll` and never hits this.
fn editor_panel() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size_full()
        .bg(Colors::surface_base())
        .text_size(px(11.0))
        .text_color(Colors::text_muted())
        .child("Select a clip to edit")
}

// ─── Sub-components for Effect Editor ────────────────────────────────────────

fn effect_editor_panel() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size_full()
        .bg(Colors::surface_base())
        .text_size(px(11.0))
        .text_color(Colors::text_muted())
        .child("Effect Editor is provided by the live insert-chain view")
}

// ─── Main Bottom Panel ───────────────────────────────────────────────────────

fn tab_button(
    id: &'static str,
    label: impl Into<String>,
    icon_path: &'static str,
    tab: BottomTab,
    active_tab: BottomTab,
    on_click: std::sync::Arc<impl Fn(&BottomTab, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let shortcut = match tab {
        BottomTab::Mixer => Some(crate::keymap::accel_display("Ctrl+3")),
        BottomTab::Editor => Some(crate::keymap::accel_display("Ctrl+5")),
        BottomTab::EffectEditor => Some(crate::keymap::accel_display("Ctrl+6")),
    };
    fb_dock_tab(
        id,
        label,
        Some(icon_path),
        tab == active_tab,
        dock_planes(),
        move |_event, window, cx| on_click(&tab, window, cx),
        shortcut,
    )
}
