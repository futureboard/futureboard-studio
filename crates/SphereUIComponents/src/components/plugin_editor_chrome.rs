//! The plug-in editor window's own chrome: what the window contributes above
//! the plug-in's surface.
//!
//! A plug-in editor window is mostly not ours — below the titlebar the whole
//! client area is a native child window the plug-in draws into, and nothing the
//! host paints can appear there. The titlebar strip is therefore the only place
//! the *host's* controls for that plug-in can live, so this is where the insert
//! it belongs to, whether it is active, its presets, and what it costs are
//! shown.
//!
//! # Ownership
//!
//! The window renders this and nothing more. Every value is pushed in by
//! [`crate::layout::StudioLayout`], which owns the insert, the engine and the
//! preset files; every control queues an action the studio drains and applies.
//! That keeps the window a view over real state instead of a second place where
//! a plug-in's active flag or preset list is decided.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, svg, App, AppContext, Bounds, Context, ElementId, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, WindowBounds, WindowHandle,
    WindowKind, WindowOptions,
};

use crate::assets;
use crate::theme::{self, Colors};

/// One plug-in open in a channel's editor window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginEditorTab {
    /// Insert slot id — how the studio and the host both address it.
    pub insert_id: String,
    /// Plug-in name, as the tab shows it.
    pub display_name: String,
    /// 1-based slot position on the channel.
    pub insert_number: usize,
}

/// What the chrome shows, as of the studio's last refresh.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PluginEditorChrome {
    /// Plug-in display name, as the titlebar shows it.
    pub plugin_name: String,
    /// Channel this window belongs to, for the title.
    pub track_name: String,
    /// 1-based insert slot this plug-in occupies on its track.
    pub insert_number: usize,
    /// Whether the insert is processing. `false` covers both a disabled and a
    /// bypassed slot — from the editor's side they are the same statement.
    pub active: bool,
    /// Latency the plug-in reports, and the rate it is measured against.
    pub latency_samples: u32,
    pub sample_rate: u32,
    /// Share of one audio block this insert's processing took, 0..=1.
    ///
    /// `None` while nothing has been measured — no stream open, or the insert
    /// has not been processed yet. It is never guessed: an unmeasured plug-in
    /// shows a dash, not a zero.
    pub cpu_load: Option<f32>,
    /// User presets available for this plug-in, in menu order.
    pub presets: Vec<String>,
    /// Which of `presets` is loaded, when the studio knows.
    pub preset_index: Option<usize>,
}

impl PluginEditorChrome {
    /// The window title: `{PluginName} - Insert {n}`.
    pub fn window_title(&self) -> String {
        let name = if self.plugin_name.is_empty() {
            "Plugin"
        } else {
            self.plugin_name.as_str()
        };
        if self.insert_number == 0 {
            return name.to_string();
        }
        format!("{name} - Insert {}", self.insert_number)
    }

    /// Current preset name, or a placeholder when none is loaded.
    pub fn preset_label(&self) -> String {
        self.preset_index
            .and_then(|index| self.presets.get(index))
            .cloned()
            .unwrap_or_else(|| {
                if self.presets.is_empty() {
                    "No presets".to_string()
                } else {
                    "Unsaved".to_string()
                }
            })
    }

    /// `3.2 ms` / `0 smp` — whichever states the latency most plainly.
    pub fn latency_label(&self) -> String {
        if self.latency_samples == 0 {
            return "0 ms".to_string();
        }
        if self.sample_rate == 0 {
            return format!("{} smp", self.latency_samples);
        }
        let ms = self.latency_samples as f64 * 1000.0 / self.sample_rate as f64;
        format!("{ms:.1} ms")
    }

    /// `12%`, or a dash while nothing has been measured.
    pub fn cpu_label(&self) -> String {
        match self.cpu_load {
            // Rounded to whole percent: a per-block share jitters, and a
            // one-decimal readout that never settles reads as noise.
            Some(load) => format!("{:.0}%", (load * 100.0).clamp(0.0, 999.0)),
            None => "—".to_string(),
        }
    }
}

/// Renders the open preset list. It fills its window edge to edge.
///
/// `highlighted` is the row the keyboard is on, which is not the same thing as
/// the loaded preset: arrows move a highlight, Enter commits it, and until then
/// the loaded preset is still the selected one.
///
/// # Why the panel has no corner radius and no shadow
///
/// This list *is* a window (see [`PresetMenuWindow`]), and a GPUI window with
/// `WindowBackgroundAppearance::Opaque` clears to opaque **white**
/// (`gpui_windows/src/directx_renderer.rs:328`). Anything the outermost element
/// does not cover — the four corners under a radius, the margin a drop shadow
/// would need — is that white. So the panel is the window: full-bleed fill, one
/// hairline border, square corners. The floating lift `elevation::OVERLAY`
/// would give an in-window popover is carried here by the OS instead, because
/// the list really is a separate surface above the editor.
pub fn render_preset_menu(
    chrome: &PluginEditorChrome,
    highlighted: Option<usize>,
    emit: impl Fn(PluginEditorAction, &mut Window, &mut App) + Clone + 'static,
) -> gpui::AnyElement {
    // Resolved once, outside the row loop: `Colors::composite` is a
    // control-path helper and DESIGN.md forbids calling it per painted row.
    let rest = Colors::surface_panel_raised();
    let transparent = Colors::with_alpha(rest, 0.0);
    let hover = Colors::composite(rest, Colors::state_hover());
    let selected_fill = Colors::composite(rest, Colors::state_selected());
    let selected_hover = Colors::composite(rest, Colors::state_selected_hover());
    let pressed = Colors::composite(rest, Colors::state_recessed());

    let mut list = div()
        .id("plugin-preset-list")
        .flex()
        .flex_col()
        .gap(px(theme::menu::ITEM_GAP))
        .size_full()
        .p(px(PRESET_MENU_PAD))
        .bg(rest)
        .border(px(PRESET_MENU_BORDER))
        .border_color(Colors::border_subtle())
        // Clamped by `resolve_popup_placement` to what the editor's own client
        // rect can hold, so a long list scrolls here instead of being cut off.
        .overflow_y_scroll()
        .occlude();

    if chrome.presets.is_empty() {
        return list
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(PRESET_MENU_ROW_H))
                    .px(px(theme::menu::ROW_PAD_X))
                    .text_size(px(theme::menu::LABEL_TEXT_SIZE))
                    .font(theme::ui_font())
                    .text_color(Colors::text_faint())
                    .truncate()
                    .child("No presets saved for this plug-in"),
            )
            .into_any_element();
    }

    for (index, name) in chrome.presets.iter().enumerate() {
        let selected = chrome.preset_index == Some(index);
        let keyboard = highlighted == Some(index);
        let rest_fill = if selected {
            selected_fill
        } else if keyboard {
            hover
        } else {
            transparent
        };
        let hover_fill = if selected { selected_hover } else { hover };
        let pick = emit.clone();
        list = list.child(
            div()
                .id(ElementId::Name(format!("plugin-preset-{index}").into()))
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::SNUG))
                .h(px(PRESET_MENU_ROW_H))
                .px(px(theme::menu::ROW_PAD_X))
                .rounded(px(theme::radius::CONTROL))
                .bg(rest_fill)
                .text_size(px(theme::menu::LABEL_TEXT_SIZE))
                .font(theme::ui_font())
                .text_color(if selected {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(hover_fill))
                .active(move |style| style.bg(pressed))
                .on_click(move |_, window, cx| {
                    pick(PluginEditorAction::SelectPreset(index), window, cx)
                })
                // Selection on two channels, neither of them hue alone: the
                // `state.selected` fill above, this leading-edge accent marker,
                // and the check glyph in the trailing slot. Absolutely
                // positioned so an unselected row is laid out identically —
                // a marker that took space would reflow the list.
                .when(selected, |row| {
                    row.child(
                        div()
                            .absolute()
                            .left(px(theme::space::HAIR))
                            .top(px(theme::space::TIGHT))
                            .bottom(px(theme::space::TIGHT))
                            .w(px(theme::space::HAIR))
                            .rounded(px(theme::radius::PILL))
                            .bg(Colors::accent_primary()),
                    )
                })
                .child(div().min_w(px(0.0)).flex_1().truncate().child(name.clone()))
                // Always present, so the label's width does not change when a
                // preset is loaded.
                .child(
                    div()
                        .flex_none()
                        .w(px(theme::menu::CHECK_SLOT_W))
                        .flex()
                        .items_center()
                        .justify_center()
                        .children(selected.then(|| {
                            svg()
                                .path(assets::ICON_CHECK_PATH)
                                .w(px(theme::menu::ICON_SIZE))
                                .h(px(theme::menu::ICON_SIZE))
                                .text_color(Colors::accent_primary())
                        })),
                ),
        );
    }
    list.into_any_element()
}

/// Geometry of the preset list, taken from the shared menu tokens. The window
/// has to be sized before it is opened, so the list's size cannot live only
/// inside its own layout.
const PRESET_MENU_W: f32 = theme::menu::PANEL_MIN_WIDTH;
const PRESET_MENU_ROW_H: f32 = theme::menu::ROW_HEIGHT;
const PRESET_MENU_PAD: f32 = theme::menu::PANEL_PAD;
/// Hairline around the panel. `theme` has no border-width token; every border
/// in this crate is written as 1 px.
const PRESET_MENU_BORDER: f32 = 1.0;
/// Rows the list opens at its tallest. Past this it scrolls, and
/// `resolve_popup_placement` shortens it further when the editor is short.
/// `theme::menu` has no panel max-height token yet, so this stays local.
const PRESET_MENU_MAX_ROWS: f32 = 16.0;

/// Size the preset list window needs for `count` presets.
pub fn preset_menu_size(count: usize) -> gpui::Size<Pixels> {
    // An empty list still shows one row saying so, which is the whole reason it
    // opens at all when nothing is saved yet.
    let rows = (count.max(1) as f32).min(PRESET_MENU_MAX_ROWS);
    let height = PRESET_MENU_PAD * 2.0
        + PRESET_MENU_BORDER * 2.0
        + rows * PRESET_MENU_ROW_H
        + (rows - 1.0).max(0.0) * theme::menu::ITEM_GAP;
    size(px(PRESET_MENU_W), px(height))
}

/// The preset list, in an **owned, topmost** window of its own, anchored and
/// clamped inside the editor's client rect.
///
/// # Why this is a window and not an element
///
/// The app boots with `GPUI_DISABLE_DIRECT_COMPOSITION=1`
/// (`apps/native/studio/src/main.rs:110`, "Disabling DComp lets child HWNDs
/// composite above the swap chain"), so gpui creates the editor window with
/// `WS_CLIPCHILDREN | WS_CLIPSIBLINGS` (`gpui_windows/src/window.rs:502-514`,
/// whose own comment says clipping "is what keeps this window's own painting
/// out of those children's rectangles"). Below the header the client area *is*
/// the plug-in's `ContentChildHwnd`, so that rectangle is removed from the
/// editor window's visible region and **GPUI cannot paint there at all** while
/// a view is attached. That is why the in-element dropdown commit `67361c02`
/// tried "was built and drawn every frame and never once seen"; the clipping
/// style predates it (`822998f3`), so it was never going to work.
///
/// # Why it is *owned* and *topmost*
///
/// A `WindowKind::PopUp` is created with `(WS_EX_TOOLWINDOW, WINDOW_STYLE(0))`
/// and no owner (`gpui_windows/src/window.rs:497`), while the editor is
/// `WindowKind::Floating` and therefore `WS_EX_TOPMOST` (same file, 537-542).
/// Windows keeps every topmost window above every non-topmost one regardless of
/// activation, so the previous un-owned popup was created, activated, drawn —
/// and sat *underneath* the window it drops from. [`place_owned_popup`] gives
/// it the editor as its owner and puts it in the same z-band, which is what
/// every Windows application does for a menu over a child control.
///
/// [`place_owned_popup`]: crate::components::plugin_content_host::place_owned_popup
pub struct PresetMenuWindow {
    chrome: PluginEditorChrome,
    on_action: Rc<dyn Fn(PluginEditorAction, &mut App)>,
    /// Row the arrow keys are on, or `None` while the pointer owns the list.
    ///
    /// Seeded from the loaded preset so the first Down/Up starts where the user
    /// is, not at the top.
    highlighted: Option<usize>,
    /// Keyboard focus for Escape / arrows / Enter. A window whose root never
    /// takes focus gets no key events at all in GPUI.
    focus: FocusHandle,
    /// Focus is claimed once, on the first frame — re-claiming it every frame
    /// would fight anything the list itself focuses later.
    focus_taken: bool,
    /// Whether this window has ever held activation.
    ///
    /// Dismiss-on-blur must not fire before the list has been shown. The window
    /// it opens over hosts a plug-in's native child, and a plug-in that takes
    /// keyboard focus back on its own would otherwise deactivate the list on the
    /// frame it appeared — closing it before anyone saw it, which is the exact
    /// symptom this window exists to fix.
    seen_active: bool,
    /// Dropped with the window; while it lives, losing focus closes the list.
    _activation: Option<Subscription>,
}

impl PresetMenuWindow {
    fn new(
        chrome: PluginEditorChrome,
        on_action: Rc<dyn Fn(PluginEditorAction, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            highlighted: chrome.preset_index,
            chrome,
            on_action,
            focus: cx.focus_handle(),
            focus_taken: false,
            seen_active: false,
            _activation: None,
        }
    }

    /// Closes the list and tells the editor it is gone.
    ///
    /// The list always closes its own window — the editor only forgets the
    /// handle — so a dismissal never re-enters the menu entity from a callback
    /// the menu itself is running. Activation returns to the owner, which routes
    /// keyboard focus back into the plug-in's view through
    /// `plugin_content_host`'s `WM_SETFOCUS` handler.
    fn dismiss(&self, window: &mut Window, cx: &mut App) {
        let on_action = self.on_action.clone();
        on_action(PluginEditorAction::TogglePresetMenu(false), cx);
        window.remove_window();
    }

    /// Loads the highlighted preset and closes.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .highlighted
            .filter(|index| *index < self.chrome.presets.len())
        else {
            return;
        };
        let on_action = self.on_action.clone();
        on_action(PluginEditorAction::SelectPreset(index), cx);
        self.dismiss(window, cx);
    }

    /// Moves the keyboard highlight, wrapping at both ends.
    fn move_highlight(&mut self, delta: i32, cx: &mut Context<Self>) {
        let count = self.chrome.presets.len();
        if count == 0 {
            return;
        }
        let count_i = count as i32;
        let next = match self.highlighted {
            // Nothing highlighted yet: Down starts at the top, Up at the bottom.
            None if delta > 0 => 0,
            None => count - 1,
            Some(current) => (((current as i32 + delta) % count_i + count_i) % count_i) as usize,
        };
        if self.highlighted == Some(next) {
            return;
        }
        self.highlighted = Some(next);
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                cx.stop_propagation();
                self.dismiss(window, cx);
            }
            "down" => {
                cx.stop_propagation();
                self.move_highlight(1, cx);
            }
            "up" => {
                cx.stop_propagation();
                self.move_highlight(-1, cx);
            }
            "enter" => {
                cx.stop_propagation();
                self.commit(window, cx);
            }
            _ => {}
        }
    }
}

impl Render for PresetMenuWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_taken {
            self.focus_taken = true;
            window.focus(&self.focus, cx);
        }
        let on_action = self.on_action.clone();
        // Every row commits the same way the keyboard does: emit the choice,
        // tell the editor the list is gone, then close this window.
        let emit = move |action: PluginEditorAction, window: &mut Window, cx: &mut App| {
            on_action(action, cx);
            on_action(PluginEditorAction::TogglePresetMenu(false), cx);
            window.remove_window();
        };
        div()
            .size_full()
            .font(theme::ui_font())
            .key_context("PluginPresetMenu")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(render_preset_menu(&self.chrome, self.highlighted, emit))
    }
}

/// Opens the preset list at `bounds`, in screen coordinates, owned by the
/// editor window that `owner_hwnd` / `owner_bounds` describe.
///
/// `on_action` is handed every choice the list makes, including the
/// [`PluginEditorAction::TogglePresetMenu`] it sends when it dismisses itself,
/// so the editor window has one place to learn the list is gone.
pub fn open_preset_menu(
    bounds: Bounds<Pixels>,
    owner_bounds: Bounds<Pixels>,
    owner_hwnd: Option<u64>,
    owner_scale: f32,
    chrome: PluginEditorChrome,
    on_action: impl Fn(PluginEditorAction, &mut App) + 'static,
    cx: &mut App,
) -> Option<WindowHandle<PresetMenuWindow>> {
    let mut options = WindowOptions {
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_decorations: None,
        ..Default::default()
    };
    // Without a display id, `retrieve_window_placement`
    // (`gpui_windows/src/window.rs:1713`) validates the requested rect against
    // the PRIMARY monitor and, whenever the editor is on any other one, throws
    // it away for `DEFAULT_WINDOW_SIZE` (1536x1095) centred on primary — which
    // is exactly the "the preset list opens as an overlay outside the editor"
    // report. Around fifteen other windows in this crate already call this.
    crate::window_position::apply_owner_display(&mut options, Some(owner_bounds), cx);

    let on_action: Rc<dyn Fn(PluginEditorAction, &mut App)> = Rc::new(on_action);
    let opened = cx.open_window(options, |window, cx| {
        // Before the first draw: the window exists by now (gpui creates the
        // platform window, then calls this builder), and giving it its owner and
        // its exact rect here means it is never seen in the wrong band or the
        // wrong place.
        own_and_place(window, owner_hwnd, bounds, owner_scale);
        cx.new(|cx| {
            let mut menu = PresetMenuWindow::new(chrome, on_action, cx);
            // A menu that outlives the click that dismissed it is a menu the
            // user has to hunt down. The first activation callback fires for
            // this window becoming active, so only a *loss* closes it.
            menu._activation = Some(cx.observe_window_activation(
                window,
                |menu: &mut PresetMenuWindow, window: &mut Window, cx: &mut Context<_>| {
                    if window.is_window_active() {
                        menu.seen_active = true;
                        return;
                    }
                    // Never shown yet: a plug-in that grabbed focus back is not
                    // the user dismissing anything. Staying open is the right
                    // failure here — the list is at worst sticky, rather than
                    // gone before it was seen.
                    if !menu.seen_active {
                        return;
                    }
                    menu.dismiss(window, cx);
                },
            ));
            menu
        })
    });
    match opened {
        Ok(handle) => Some(handle),
        Err(error) => {
            // Never swallowed: with the handle left at `None` the trigger reads
            // as a button that does nothing, and there was no way to tell that
            // from the list opening somewhere off screen.
            eprintln!("[plugin-preset-menu] open failed: {error}");
            None
        }
    }
}

/// Give the freshly created list window its owner and its exact physical rect.
///
/// The rect is applied here rather than left to gpui because a popup's own
/// scale factor comes from `GetDpiForWindow` at the `CW_USEDEFAULT` position it
/// is created at (`gpui_windows/src/window.rs:118`), which is not necessarily
/// the monitor it is about to be placed on. `owner_scale` is the editor's, and
/// the editor is the surface this list has to line up with.
#[cfg(target_os = "windows")]
fn own_and_place(
    window: &Window,
    owner_hwnd: Option<u64>,
    bounds: Bounds<Pixels>,
    owner_scale: f32,
) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(owner) = owner_hwnd else {
        eprintln!(
            "[plugin-preset-menu] no owner hwnd: the list can be covered by the editor it drops from"
        );
        return;
    };
    let popup = match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get() as u64,
        _ => {
            eprintln!("[plugin-preset-menu] the list window has no native handle");
            return;
        }
    };
    let scale = if owner_scale.is_finite() && owner_scale > 0.1 {
        owner_scale
    } else {
        1.0
    };
    let x = (f32::from(bounds.origin.x) * scale).round() as i32;
    let y = (f32::from(bounds.origin.y) * scale).round() as i32;
    let width = (f32::from(bounds.size.width) * scale).round() as i32;
    let height = (f32::from(bounds.size.height) * scale).round() as i32;
    if !crate::components::plugin_content_host::place_owned_popup(popup, owner, x, y, width, height)
    {
        eprintln!("[plugin-preset-menu] could not own or place the list window");
    }
}

/// No native child exists off Windows (`plugin_content_host`'s stub returns
/// `None` for every host), so nothing occludes an ordinary popup there.
#[cfg(not(target_os = "windows"))]
fn own_and_place(
    _window: &Window,
    _owner_hwnd: Option<u64>,
    _bounds: Bounds<Pixels>,
    _owner_scale: f32,
) {
}

/// Height of the tab strip. Sized to a browser tab, which is what it is.
pub const TAB_STRIP_H: f32 = 30.0;

/// Renders the tab strip: one tab per plug-in open on this channel.
///
/// Browser-shaped on purpose — a channel's inserts are a chain the user moves
/// along, and a row of named tabs with their own close buttons is the gesture
/// everyone already has for that.
pub fn render_tab_strip(
    tabs: &[PluginEditorTab],
    active: &str,
    emit: impl Fn(PluginEditorAction, &mut App) + Clone + 'static,
) -> gpui::AnyElement {
    let mut strip = div()
        .flex()
        .flex_row()
        .items_end()
        .h(px(TAB_STRIP_H))
        .px(px(4.0))
        .gap(px(2.0))
        .bg(Colors::surface_panel_alt())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .overflow_hidden();

    for tab in tabs {
        let selected = tab.insert_id == active;
        let select = emit.clone();
        let close = emit.clone();
        let select_id = tab.insert_id.clone();
        let close_id = tab.insert_id.clone();
        strip = strip.child(
            div()
                .id(ElementId::Name(
                    format!("plugin-tab-{}", tab.insert_id).into(),
                ))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .h(px(TAB_STRIP_H - 4.0))
                .pl(px(10.0))
                .pr(px(4.0))
                .max_w(px(220.0))
                .rounded_t(px(5.0))
                .bg(if selected {
                    Colors::surface_panel()
                } else {
                    Colors::surface_panel_alt()
                })
                .when(selected, |style| {
                    style
                        .border_t(px(1.0))
                        .border_l(px(1.0))
                        .border_r(px(1.0))
                        .border_color(Colors::border_subtle())
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .occlude()
                .hover(|style| style.bg(Colors::surface_control_hover()))
                .on_click(move |_, _window, cx| {
                    select(PluginEditorAction::SelectTab(select_id.clone()), cx)
                })
                // The slot number leads: on a channel with two of the same
                // plug-in, the name alone does not say which one this is.
                .child(
                    div()
                        .text_size(px(10.0))
                        .font(theme::ui_font())
                        .text_color(Colors::text_faint())
                        .child(tab.insert_number.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .text_size(px(11.0))
                        .font(theme::ui_font())
                        .text_color(if selected {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(tab.display_name.clone()),
                )
                .child(
                    div()
                        .id(ElementId::Name(
                            format!("plugin-tab-close-{}", tab.insert_id).into(),
                        ))
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(16.0))
                        .h(px(16.0))
                        .rounded(px(3.0))
                        .hover(|style| style.bg(Colors::surface_control_hover()))
                        .occlude()
                        .on_click(move |_, _window, cx| {
                            close(PluginEditorAction::CloseTab(close_id.clone()), cx)
                        })
                        .child(
                            svg()
                                .path(assets::ICON_CLOSE_SMALL_PATH)
                                .w(px(10.0))
                                .h(px(10.0))
                                .text_color(Colors::text_faint()),
                        ),
                ),
        );
    }

    strip.into_any_element()
}

/// The chrome's colours, resolved from the active theme for a host that cannot
/// read it.
///
/// Only the host-owned-window platforms need this: there the strip is drawn in
/// the plug-in host process, which has no theme store of its own. Resolving
/// here rather than sending token names keeps one answer to "what colour is a
/// hovered control" — several of these are composites, and a second
/// implementation of `Colors::composite` is a second answer waiting to drift.
///
/// The order matches `EditorChromePalette`'s fields and the `PaletteSlot`
/// indices in `editor_mac_shell.mm`.
pub fn resolve_chrome_palette() -> SpherePluginHost::ipc::EditorChromePalette {
    fn packed(color: gpui::Rgba) -> u32 {
        let channel = |value: f32| ((value.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xFF;
        (channel(color.r) << 24)
            | (channel(color.g) << 16)
            | (channel(color.b) << 8)
            | channel(color.a)
    }
    let control = Colors::surface_base();
    SpherePluginHost::ipc::EditorChromePalette {
        strip_bg: packed(Colors::surface_panel_alt()),
        row_bg: packed(Colors::surface_panel()),
        border: packed(Colors::border_subtle()),
        control_bg: packed(control),
        control_hover: packed(Colors::composite(control, Colors::state_hover())),
        control_pressed: packed(Colors::composite(control, Colors::state_recessed())),
        accent: packed(Colors::accent_primary()),
        text_primary: packed(Colors::text_primary()),
        text_secondary: packed(Colors::text_secondary()),
        text_faint: packed(Colors::text_faint()),
    }
}

/// A control the chrome asks the studio to carry out.
///
/// Queued rather than applied: the window has no access to the insert, the
/// engine, or the preset store, and routing every change through the studio
/// keeps one owner for each of them.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginEditorAction {
    /// Turn the insert's processing on or off.
    SetActive(bool),
    /// Move `delta` places through the preset list, wrapping.
    StepPreset(i32),
    /// Store the plug-in's current state as a new preset.
    SavePreset,
    /// Load the preset at this index.
    SelectPreset(usize),
    /// Open or close the preset list.
    TogglePresetMenu(bool),
    /// Bring another of this channel's open plug-ins to the front.
    SelectTab(String),
    /// Close one plug-in's tab. Closing the last one closes the window.
    CloseTab(String),
}

/// Small square/pill button used across the chrome.
fn chrome_button(
    id: impl Into<ElementId>,
    label: impl Into<String>,
    enabled: bool,
    active: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let mut button = div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .h(px(CHROME_CONTROL_H))
        .px(px(7.0))
        .rounded(px(3.0))
        .text_size(px(10.0))
        .font(theme::ui_font())
        .child(label.into());
    if !enabled {
        return button
            .text_color(Colors::text_faint())
            .bg(Colors::surface_base())
            .into_any_element();
    }
    button = button
        .cursor(gpui::CursorStyle::PointingHand)
        .occlude()
        .on_click(move |_, window, cx| on_click(window, cx));
    if active {
        button
            .text_color(Colors::text_primary())
            .bg(Colors::accent_primary())
            .into_any_element()
    } else {
        button
            .text_color(Colors::text_secondary())
            .bg(Colors::surface_base())
            .hover(|style| style.bg(Colors::surface_control_hover()))
            .into_any_element()
    }
}

/// An icon-only button, for controls whose glyph says it better than a word.
fn chrome_icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    enabled: bool,
    active: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let tint = if !enabled {
        Colors::text_faint()
    } else if active {
        Colors::text_primary()
    } else {
        Colors::text_secondary()
    };
    let glyph = svg().path(icon).w(px(13.0)).h(px(13.0)).text_color(tint);
    let base = div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .w(px(22.0))
        .h(px(CHROME_CONTROL_H))
        .rounded(px(3.0))
        .child(glyph);
    if !enabled {
        return base.into_any_element();
    }
    base.cursor(gpui::CursorStyle::PointingHand)
        .occlude()
        .when(active, |style| style.bg(Colors::accent_primary()))
        .when(!active, |style| {
            style.hover(|style| style.bg(Colors::surface_control_hover()))
        })
        .on_click(move |_, window, cx| on_click(window, cx))
        .into_any_element()
}

/// An icon with a value beside it, for the readouts.
fn chrome_readout(icon: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .child(
            svg()
                .path(icon)
                .w(px(11.0))
                .h(px(11.0))
                .text_color(Colors::text_faint()),
        )
        .child(
            div()
                .text_size(px(10.0))
                .font(theme::ui_font())
                .text_color(Colors::text_secondary())
                .child(value),
        )
}

/// Height of the chrome row. Mirrors `plugin_editor_window::CHROME_H`.
const CHROME_ROW_H: f32 = 26.0;

/// Visual height of every control in the chrome row.
///
/// One source for what the private `chrome_button` / `chrome_icon_button`
/// above already write inline, so the preset trigger cannot drift out of line
/// with its neighbours. `theme::size` has no step between `MICRO` (16) and
/// `DENSE` (20); a 26 px row leaves no room for 20, and 16 reads as a tick box
/// beside a plug-in name. It stays in the 16–20 band, so the radius tier is
/// `radius::CONTROL_SM`.
const CHROME_CONTROL_H: f32 = 18.0;

/// Narrowest the preset trigger goes. Wide enough that a short preset name does
/// not make the chevron jump around as the list is stepped through.
const PRESET_TRIGGER_MIN_W: f32 = 120.0;

/// Builds the chrome row that sits under the titlebar, above the plug-in.
///
/// `emit` queues an action on the window. `preset_anchor` receives the preset
/// trigger's window-space rect on every prepaint: the list is a window of its
/// own and has to be placed against a real measurement, not against a constant
/// that silently stops matching the row the first time it changes.
pub fn render_chrome_tools(
    chrome: &PluginEditorChrome,
    menu_open: bool,
    preset_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    emit: impl Fn(PluginEditorAction, &mut App) + Clone + 'static,
) -> gpui::AnyElement {
    let active = chrome.active;
    let has_presets = !chrome.presets.is_empty();

    let emit_active = emit.clone();
    let emit_prev = emit.clone();
    let emit_next = emit.clone();
    let emit_menu = emit.clone();
    let emit_save = emit;

    // The trigger's rest / hover / pressed fills, resolved before the element
    // is built: a GPUI div has one background, so `.hover(|s| s.bg(token))`
    // would replace the fill instead of lifting it.
    let trigger_rest = if menu_open {
        Colors::composite(Colors::surface_base(), Colors::state_selected())
    } else {
        Colors::surface_base()
    };
    let trigger_hover = Colors::composite(trigger_rest, Colors::state_hover());
    let trigger_pressed = Colors::composite(trigger_rest, Colors::state_recessed());

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .h(px(CHROME_ROW_H))
        .px(px(8.0))
        .bg(Colors::surface_panel())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        // Active: the plug-in's own on/off, in the one strip that is never
        // covered by its view.
        .child(chrome_icon_button(
            "plugin-editor-active",
            assets::ICON_POWER_PATH,
            true,
            active,
            move |_window, cx| emit_active(PluginEditorAction::SetActive(!active), cx),
        ))
        // Presets: step and save. Nothing here invents a preset — the list is
        // whatever the studio found on disk for this plug-in.
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .child(chrome_icon_button(
                    "plugin-editor-preset-prev",
                    assets::ICON_CHEVRON_LEFT_PATH,
                    has_presets,
                    false,
                    move |_window, cx| emit_prev(PluginEditorAction::StepPreset(-1), cx),
                ))
                // The name is the menu trigger. A chain of presets is stepped
                // through with the chevrons; picking one by name needs a list.
                //
                // Wrapped so the trigger's own rect can be captured: the list is
                // a separate window, so its anchor has to come out of the layout
                // that placed the button rather than be guessed alongside it.
                // The bounds handed back are absolute window-space.
                .child(
                    div()
                        .flex_none()
                        .on_children_prepainted(move |bounds, _window, _cx| {
                            if let Some(trigger) = bounds.first().copied() {
                                preset_anchor.set(Some(trigger));
                            }
                        })
                        .child(
                            div()
                                .id("plugin-editor-preset-menu")
                                .flex()
                                .items_center()
                                .gap(px(theme::space::SNUG))
                                .h(px(CHROME_CONTROL_H))
                                .px(px(theme::space::BASE))
                                .min_w(px(PRESET_TRIGGER_MIN_W))
                                .rounded(px(theme::radius::CONTROL_SM))
                                .bg(trigger_rest)
                                .text_size(px(theme::typography::UI_XS))
                                .font(theme::ui_font())
                                // Open is carried on the fill above and on this
                                // label colour, never on one channel alone.
                                .text_color(if menu_open {
                                    Colors::text_primary()
                                } else {
                                    Colors::text_secondary()
                                })
                                .cursor(gpui::CursorStyle::PointingHand)
                                .occlude()
                                .hover(move |style| style.bg(trigger_hover))
                                .active(move |style| style.bg(trigger_pressed))
                                .on_click(move |_, _window, cx| {
                                    emit_menu(PluginEditorAction::TogglePresetMenu(!menu_open), cx)
                                })
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .flex_1()
                                        .truncate()
                                        .child(chrome.preset_label()),
                                )
                                .child(
                                    svg()
                                        .path(assets::ICON_CHEVRON_DOWN_PATH)
                                        .w(px(theme::menu::CHEVRON_SIZE))
                                        .h(px(theme::menu::CHEVRON_SIZE))
                                        .flex_shrink_0()
                                        .text_color(if menu_open {
                                            Colors::text_secondary()
                                        } else {
                                            Colors::text_faint()
                                        }),
                                ),
                        ),
                )
                .child(chrome_icon_button(
                    "plugin-editor-preset-next",
                    assets::ICON_CHEVRON_RIGHT_PATH,
                    has_presets,
                    false,
                    move |_window, cx| emit_next(PluginEditorAction::StepPreset(1), cx),
                ))
                .child(chrome_button(
                    "plugin-editor-preset-save",
                    "Save",
                    true,
                    false,
                    move |_window, cx| emit_save(PluginEditorAction::SavePreset, cx),
                )),
        )
        // Readouts sit at the far end: they are watched, not operated, so they
        // stay clear of the controls the pointer goes for.
        .child(div().flex_1())
        .child(chrome_readout(assets::ICON_CPU_PATH, chrome.cpu_label()))
        .child(chrome_readout(
            assets::ICON_TIMER_PATH,
            chrome.latency_label(),
        ))
        .into_any_element()
}
