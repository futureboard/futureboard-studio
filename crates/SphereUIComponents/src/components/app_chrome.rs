use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, AccessibleAction, App, AppContext, DragMoveEvent, Empty, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Role, StatefulInteractiveElement,
    Styled, Toggled, Window, WindowControlArea,
};

use crate::assets;
use crate::components::controls::{fb_shortcut_hint, fb_tooltip};
use crate::components::menu_bar;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::components::title_bar::{
    chrome_button, chrome_button_hover, chrome_button_pressed, chrome_cluster, draggable_spacer,
    section_separator, CHROME_TITLE_SIZE, WINDOW_CONTROL_WIDTH,
};
use crate::i18n::I18n;
use crate::keymap::accel_display;
use crate::platform_chrome::PlatformChromePolicy;
use crate::theme::{self, space, Colors};

/// Click handler for top-level menu buttons. Receives `(menu_id, anchor_x)`
/// — anchor_x is the click X position which the dropdown overlay uses to
/// align itself under the clicked label.
pub type MenuOpenCb = menu_bar::MenuOpenCb;
pub type ChromeActionCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;
pub type ChromeRightClickCb = Arc<dyn Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static>;
pub type ProjectOpenCb = Arc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;
pub type BpmChangeCb = Arc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;
pub type BpmDragCb = Arc<dyn Fn(&BpmDragSample, &mut Window, &mut App) + 'static>;
/// Opens the compact tempo menu. Payload is the (x, y) screen position used to
/// anchor the popover beneath the BPM display.
pub type BpmMenuCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;

pub const BPM_MIN: f32 = 20.0;
pub const BPM_MAX: f32 = 999.0;

/// Width budget for the centred project control in the titlebar.
///
/// Bounded rather than content-sized: a long project name must truncate instead
/// of pushing the chip off centre, and the dropdown anchor is derived from this
/// width, so the two have to agree.
pub(super) const PROJECT_CHIP_MAX_WIDTH: f32 = 280.0;

static BPM_DRAG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn next_bpm_drag_id() -> u64 {
    BPM_DRAG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// One drag move sample. The handler on the owning entity accumulates
/// `cur_y - prev_y` deltas across samples — never the absolute distance
/// from the drag origin — so the cursor hitting the top/bottom of the
/// window doesn't cap the BPM range (FL Studio–style behavior).
#[derive(Clone, Copy, Debug)]
pub struct BpmDragSample {
    pub drag_id: u64,
    pub start_bpm: f32,
    pub cur_y: f32,
    pub shift: bool,
    pub control: bool,
    pub platform: bool,
    pub alt: bool,
}

/// Drag state for the transport BPM display. Carries the unique `drag_id`
/// so the receiver can tell a new drag from a continuation of the active
/// one, plus the captured `start_bpm`.
#[derive(Clone, Debug)]
pub struct BpmDrag {
    pub drag_id: u64,
    pub start_bpm: f32,
}

impl Render for BpmDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn chrome_action_button(
    id: &'static str,
    icon_path: &'static str,
    label: impl Into<gpui::SharedString>,
    toggled: Option<bool>,
    color: gpui::Rgba,
    action: ChromeActionCb,
    shortcut: Option<String>,
    on_right_click: Option<ChromeRightClickCb>,
) -> impl gpui::IntoElement {
    let label = label.into();
    // Build the 26×26 stateful button first, then wrap it in a flex-col outer
    // container so the optional shortcut-hint pill sits *below* the icon rather
    // than beside it inside the fixed-size box. Previously the pill was appended
    // as a child of the button div itself (flex-row), which pushed it inline
    // next to the icon and caused the button to overflow its 26 px width.
    let button = chrome_button(
        Some(icon_path),
        label.clone(),
        toggled.unwrap_or(false),
        color,
    )
    .id(id)
    .role(Role::Button)
    .aria_label(label)
    .when_some(toggled, |button, toggled| {
        button.aria_toggled(if toggled {
            Toggled::True
        } else {
            Toggled::False
        })
    })
    .focusable()
    .tab_stop(true)
    .focus_visible(|style| {
        style.shadow(crate::theme::elevation::focus_ring(
            Colors::state_focus_ring(),
        ))
    })
    // `.hover()`/`.active()` are applied here rather than inside `chrome_button`
    // so a caller that wants its own hover (the red close caption) is not fighting
    // GPUI's "hover style already set" assertion.
    .hover(move |style| style.bg(chrome_button_hover(toggled.unwrap_or(false))))
    .active(move |style| style.bg(chrome_button_pressed(toggled.unwrap_or(false))))
    .cursor(gpui::CursorStyle::PointingHand)
    .on_click(move |_, window, cx| action(&(), window, cx))
    .when_some(on_right_click, |button, handler| {
        button.on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
            handler(event, window, cx)
        })
    })
    .occlude();

    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::HAIR))
        .child(button)
        .children(shortcut.as_deref().map(|s| fb_shortcut_hint(s)))
}

#[derive(Clone)]
pub struct ProjectChromeState {
    pub name: String,
    pub is_dirty: bool,
    pub on_open_project_menu: ProjectOpenCb,
}

#[derive(Clone)]
pub struct PanelChromeState {
    pub browser_visible: bool,
    pub inspector_visible: bool,
    /// The bottom dock, whichever tab it holds — not "the Mixer is on screen".
    /// The button carries the `panel-bottom` glyph and sits beside the browser
    /// and inspector toggles, so it is read as the third dock toggle; scoping
    /// it to the Mixer left the Editor tab with no toggle at all.
    pub bottom_panel_visible: bool,
    pub on_toggle_browser: ChromeActionCb,
    pub on_toggle_bottom_panel: ChromeActionCb,
    pub on_toggle_inspector: ChromeActionCb,
}

#[derive(Clone)]
pub struct TransportChromeState {
    pub playing: bool,
    pub recording: bool,
    pub count_in_enabled: bool,
    pub loop_enabled: bool,
    pub metronome_enabled: bool,
    pub follow_playhead: bool,
    /// True when auto-scroll is in continuous (smooth) mode rather than paged.
    /// Drives the FOLLOW button accent and is toggled via right-click.
    pub auto_scroll_continuous: bool,
    pub position_label: String,
    pub bpm: f32,
    pub bpm_label: String,
    /// True when tempo automation is active — drives the small "AUTO" badge.
    pub bpm_has_automation: bool,
    /// Inline BPM editor state (open flag, field contents, focus).
    pub bpm_editing: bool,
    pub bpm_input: TextInputState,
    pub bpm_input_callbacks: TextInputCallbacks,
    pub bpm_edit_focused: bool,
    pub time_signature_label: String,
    pub ts_has_markers: bool,
    pub ts_editing: bool,
    pub ts_num_input: TextInputState,
    pub ts_num_input_callbacks: TextInputCallbacks,
    pub ts_den_input: TextInputState,
    pub ts_den_input_callbacks: TextInputCallbacks,
    pub ts_edit_focus_num: bool,
    pub on_ts_menu: BpmMenuCb,
    pub on_ts_edit_start: ChromeActionCb,
    pub on_return_to_start: ChromeActionCb,
    pub on_play_toggle: ChromeActionCb,
    pub on_stop: ChromeActionCb,
    pub on_record: ChromeActionCb,
    pub on_count_in_toggle: ChromeActionCb,
    /// Opens the count-in duration dropdown at the pointer position.
    pub on_count_in_menu: BpmMenuCb,
    pub on_loop_toggle: ChromeActionCb,
    pub on_metronome_toggle: ChromeActionCb,
    pub on_follow_toggle: ChromeActionCb,
    /// Right-click on FOLLOW: switch auto-scroll between paged and continuous.
    pub on_follow_mode_toggle: ChromeActionCb,
    pub on_set_bpm: BpmChangeCb,
    pub on_bpm_drag: BpmDragCb,
    /// Pointer release after a BPM scrub. The scrub itself writes tempo live
    /// (so playback follows the drag); this is where the whole gesture becomes
    /// one undo entry.
    pub on_bpm_drag_end: ChromeActionCb,
    pub on_bpm_menu: BpmMenuCb,
    /// Opens the inline numeric BPM editor (double-click / "Edit BPM…").
    pub on_bpm_edit_start: ChromeActionCb,
    /// Taps in the current session (0 = idle). Used for brief tap-button feedback only.
    pub tap_tempo_session_taps: u8,
    /// Left-click registers a tap; right-click opens the tap tempo menu.
    pub on_tap_tempo: ChromeActionCb,
    pub on_tap_tempo_menu: BpmMenuCb,
    /// Master level strip (meter + fader), rendered as its own entity so the
    /// meter poll repaints it alone instead of the whole shell. `None` in
    /// surfaces that have no engine behind them.
    pub master_meter: Option<gpui::AnyView>,
    /// Engine load readout (CPU / RAM / voices). Its own entity for the same
    /// reason as `master_meter`.
    pub perf_meter: Option<gpui::AnyView>,
}

fn tap_tempo_chip(
    session_taps: u8,
    on_tap: ChromeActionCb,
    on_menu: BpmMenuCb,
) -> gpui::AnyElement {
    let active = session_taps > 0;
    // Ghost at rest — it already sits inside the readout panel, so a fill and a
    // border here framed a box inside a box.
    let bg = if active {
        Colors::with_alpha(Colors::accent_primary(), 0.2)
    } else {
        Colors::with_alpha(Colors::surface_input(), 0.0)
    };
    let text_color = if active {
        Colors::accent_primary()
    } else {
        Colors::text_secondary()
    };

    div()
        .id("transport-tap-tempo")
        .role(Role::Button)
        .aria_label("Tap tempo")
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(Colors::surface_control_hover()))
        .h(px(20.0))
        .min_w(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(2.0))
        .px(px(6.0))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .bg(bg)
        .text_color(text_color)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_control_hover()))
        .tooltip(fb_tooltip(
            "Tap tempo — click in time; right-click for options",
        ))
        // The glyph replaces the "TAP" text. The tap-count dots beside it stay:
        // they are the only feedback that a tap actually registered, and an
        // icon-only button with no state would give none.
        .child(
            svg()
                .path(assets::ICON_CIRCLE_DOT_PATH)
                .w(px(12.0))
                .h(px(12.0))
                .text_color(text_color),
        )
        .children((1..=session_taps.min(4)).map(|_| {
            div()
                .w(px(3.0))
                .h(px(3.0))
                .rounded(px(crate::theme::radius::PILL))
                .bg(Colors::with_alpha(Colors::accent_primary(), 0.85))
        }))
        .occlude()
        .on_click(move |_, window, cx| {
            on_tap(&(), window, cx);
        })
        .on_mouse_down(
            MouseButton::Right,
            move |event: &gpui::MouseDownEvent, window, cx| {
                let pos = event.position;
                on_menu(&(pos.x.into(), pos.y.into()), window, cx);
            },
        )
        .into_any_element()
}

fn transport_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_TRANSPORT_DEBUG").is_some())
}

fn bpm_display(
    state_bpm: f32,
    label: String,
    on_bpm_drag: BpmDragCb,
    on_bpm_drag_end: ChromeActionCb,
    on_bpm_menu: BpmMenuCb,
    on_bpm_edit_start: ChromeActionCb,
    editing: bool,
    bpm_input: &TextInputState,
    bpm_input_callbacks: TextInputCallbacks,
    edit_focused: bool,
) -> gpui::AnyElement {
    // Inline numeric editor: replaces the draggable box while open. Keys are
    // routed to the input by the layout's key handler; Enter commits, Escape
    // cancels.
    if editing {
        return div()
            .w(px(48.0))
            .child(text_field_with_callbacks(
                bpm_input,
                edit_focused,
                bpm_input_callbacks,
            ))
            .into_any_element();
    }

    let on_bpm_drag_move = on_bpm_drag.clone();
    let on_bpm_drag_end_up = on_bpm_drag_end.clone();
    let on_bpm_drag_end_out = on_bpm_drag_end;
    let on_bpm_menu_down = on_bpm_menu.clone();
    let on_bpm_edit_accessible = on_bpm_edit_start.clone();
    let accessible_label = format!("Tempo {label} BPM. Activate to edit");
    div()
        .id("transport-bpm")
        .role(Role::Button)
        .aria_label(accessible_label)
        .aria_numeric_value(state_bpm as f64)
        .aria_min_numeric_value(BPM_MIN as f64)
        .aria_max_numeric_value(BPM_MAX as f64)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(Colors::surface_control_hover()))
        .min_w(px(38.0))
        .h(px(19.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .text_color(Colors::text_primary())
        .text_size(px(14.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .hover(|s| s.bg(Colors::surface_control_hover()))
        .child(label)
        .occlude()
        // Double-click opens inline numeric edit; left-drag scrubs the value;
        // right-click opens the tempo menu.
        .on_click(move |event: &gpui::ClickEvent, window, cx| {
            if event.click_count() >= 2 {
                on_bpm_edit_start(&(), window, cx);
            }
        })
        .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
            on_bpm_edit_accessible(&(), window, cx);
        })
        .on_mouse_down(
            gpui::MouseButton::Right,
            move |event: &gpui::MouseDownEvent, window, cx| {
                let pos = event.position;
                on_bpm_menu_down(&(pos.x.into(), pos.y.into()), window, cx);
            },
        )
        .on_drag(
            BpmDrag {
                drag_id: 0,
                start_bpm: state_bpm,
            },
            move |drag, _offset, _window, cx| {
                let id = next_bpm_drag_id();
                let started = BpmDrag {
                    drag_id: id,
                    start_bpm: drag.start_bpm,
                };
                cx.new(|_| started)
            },
        )
        .on_drag_move::<BpmDrag>(move |event: &DragMoveEvent<BpmDrag>, window, cx| {
            let drag = event.drag(cx);
            let mods = event.event.modifiers;
            let sample = BpmDragSample {
                drag_id: drag.drag_id,
                start_bpm: drag.start_bpm,
                cur_y: event.event.position.y.into(),
                shift: mods.shift,
                control: mods.control,
                platform: mods.platform,
                alt: mods.alt,
            };
            on_bpm_drag_move(&sample, window, cx);
        })
        // The scrub warps the cursor back to its anchor, so the release usually
        // lands on this element — but not on platforms without cursor warp, and
        // not for a drag the user ends off-target. Both are wired, and the
        // handler is a no-op when no scrub is in flight.
        .on_mouse_up(
            gpui::MouseButton::Left,
            move |_: &gpui::MouseUpEvent, window, cx| {
                on_bpm_drag_end_up(&(), window, cx);
            },
        )
        .on_mouse_up_out(
            gpui::MouseButton::Left,
            move |_: &gpui::MouseUpEvent, window, cx| {
                on_bpm_drag_end_out(&(), window, cx);
            },
        )
        .into_any_element()
}

/// Per-(logical)-pixel BPM sensitivity for a given modifier combination.
/// DAW-style feel: normal ≈ 1 BPM / 10 px, Shift = fine, Ctrl/Alt = coarse.
/// Because the BPM drag now warps the OS cursor (Windows/macOS/Linux X11), the per-pixel feel
/// is screen-height independent — the cursor never reaches the screen edge.
pub fn bpm_drag_sensitivity(shift: bool, coarse: bool) -> f32 {
    if shift {
        0.02
    } else if coarse {
        0.5
    } else {
        0.1
    }
}

/// Minimum per-event delta (in px) accepted by the BPM drag handler.
/// Below this, the event is treated as cursor jitter and ignored.
pub const BPM_DRAG_DEADZONE_PX: f32 = 0.5;

pub fn bpm_debug_enabled() -> bool {
    transport_debug_enabled()
}

fn menu_area(
    open_menu_id: Option<&str>,
    on_open_menu: MenuOpenCb,
    viewport_width: f32,
    i18n: I18n,
) -> impl IntoElement {
    menu_bar::menu_bar(open_menu_id, on_open_menu, viewport_width, i18n)
}

fn project_title(state: ProjectChromeState, anchor_x: f32, i18n: I18n) -> impl IntoElement {
    let on_open = state.on_open_project_menu.clone();
    let status = if state.is_dirty {
        i18n.tr("chrome.project.unsaved")
    } else {
        i18n.tr("chrome.project.saved")
    };
    let status_color = if state.is_dirty {
        Colors::status_warning()
    } else {
        Colors::status_success()
    };
    let title = crate::platform_chrome::branded_window_title(&state.name);
    let accessible_label = format!("Project {title}, {status}");
    div()
        .id("project-title-menu")
        .role(Role::Button)
        .aria_label(accessible_label)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(Colors::surface_control_hover()))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(crate::theme::space::SNUG))
        .h(px(crate::theme::size::DEFAULT))
        .max_w(px(PROJECT_CHIP_MAX_WIDTH))
        .px(px(crate::theme::space::BASE))
        .rounded(px(crate::theme::radius::CONTROL))
        // Ghost at rest: the project name is a label that happens to be
        // clickable, not a control competing with the transport below it. The
        // hover fill still composites over the titlebar so the hit area is
        // discoverable.
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(Colors::composite(
                Colors::surface_titlebar(),
                Colors::state_hover(),
            ))
        })
        .on_click(move |_event, window, cx| {
            on_open(&anchor_x, window, cx);
        })
        .occlude()
        .child(
            svg()
                .path(assets::ICON_FILE_PATH)
                .w(px(12.0))
                .h(px(12.0))
                .text_color(Colors::text_muted()),
        )
        .child(
            div()
                .min_w(px(0.0))
                .text_color(Colors::text_secondary())
                .text_size(px(CHROME_TITLE_SIZE))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .truncate()
                .child(title),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(5.0))
                .py(px(2.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::surface_input())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(crate::theme::radius::PILL))
                        .bg(status_color),
                )
                .child(
                    div()
                        .text_color(Colors::text_muted())
                        .text_size(px(8.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(status),
                ),
        )
}

// ── Right section — transport + panel toggles + utility ───────────────────────

/// Glyph above a readout field, in place of a text caption.
///
/// Quiet by construction: at `text_faint` it sits well below the value it
/// labels, which is the whole point — the number is what gets read mid-take,
/// the label only has to say which number it is.
fn lcd_caption(icon: &'static str) -> impl IntoElement {
    svg()
        .path(icon)
        .flex_none()
        .w(px(12.0))
        .h(px(12.0))
        .text_color(Colors::text_faint())
}

/// One field of the readout panel: its glyph beside its value.
///
/// Stacking the glyph over the value cost a whole line of the 30px panel for a
/// label, and forced a fixed-height value slot to keep the three glyphs on one
/// baseline. Side by side, the row is a single line of centred content and the
/// panel gets its height back for the numbers.
fn lcd_field(
    id: &'static str,
    icon: &'static str,
    tooltip: &'static str,
    value: gpui::AnyElement,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h_full()
        .gap(px(crate::theme::space::SNUG))
        .px(px(crate::theme::space::LOOSE))
        // The glyph replaced a word, so the word has to survive somewhere: an
        // icon-only label for a numeric readout is not self-describing.
        .tooltip(fb_tooltip(tooltip))
        .child(lcd_caption(icon))
        .child(value)
}

/// Hairline between readout fields. Inset top and bottom so it separates the
/// values without drawing a full-height rule through the panel.
fn lcd_divider() -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(px(20.0))
        .flex_none()
        .bg(Colors::border_normal())
}

/// The transport bar: its own row beneath the titlebar.
///
/// Laid out as three tracks — controls, readout, spare — where the outer two are
/// `flex_1` with a zero basis so they always split the leftover width evenly and
/// the readout panel sits on the true window centre. The readout is the one
/// place in the shell that earns large type: position, tempo and meter are what
/// a player reads mid-take, so they get a recessed panel, tabular figures, and
/// captions rather than being scattered along a toolbar as bare text.
fn transport_bar(state: TransportChromeState, viewport_width: f32, i18n: I18n) -> impl IntoElement {
    let play_color = if state.playing {
        Colors::accent_primary()
    } else {
        Colors::text_secondary()
    };
    let record_color = if state.recording {
        Colors::state_arm()
    } else {
        Colors::text_secondary()
    };
    let loop_color = if state.loop_enabled {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    let metronome_color = if state.metronome_enabled {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    // Continuous mode reads as a distinct hue so the right-click toggle is
    // visible at a glance; paged follow keeps the standard accent.
    let follow_color = if state.follow_playhead {
        if state.auto_scroll_continuous {
            Colors::state_monitor()
        } else {
            Colors::accent_primary()
        }
    } else {
        Colors::text_muted()
    };

    let on_return = state.on_return_to_start.clone();
    let on_play = state.on_play_toggle.clone();
    let on_stop = state.on_stop.clone();
    let on_record = state.on_record.clone();
    let on_count_in_toggle = state.on_count_in_toggle.clone();
    let on_count_in_menu = state.on_count_in_menu.clone();
    let on_loop = state.on_loop_toggle.clone();
    let on_metronome = state.on_metronome_toggle.clone();
    let on_follow = state.on_follow_toggle.clone();
    let on_follow_mode = state.on_follow_mode_toggle.clone();
    let on_bpm_drag = state.on_bpm_drag.clone();
    let on_bpm_drag_end = state.on_bpm_drag_end.clone();
    let on_bpm_menu = state.on_bpm_menu.clone();
    let on_bpm_edit_start = state.on_bpm_edit_start.clone();
    let bpm_value = state.bpm;
    let bpm_label = state.bpm_label.clone();
    let bpm_has_automation = state.bpm_has_automation;
    let bpm_editing = state.bpm_editing;
    let bpm_input = state.bpm_input.clone();
    let bpm_input_callbacks = state.bpm_input_callbacks.clone();
    let bpm_edit_focused = state.bpm_edit_focused;
    let tap_tempo_session_taps = state.tap_tempo_session_taps;
    // The master strip is dropped rather than squeezed on a narrow window: a
    // meter compressed until its segments alias is worse than no meter, and the
    // mixer still carries the full one.
    // Both gutters drop their readout at the same width. Hiding one and not
    // the other would slide the centred transport block off the window centre,
    // which is the one thing the three-track layout exists to hold.
    let gutters_fit = viewport_width
        >= crate::components::master_transport_meter::MASTER_TRANSPORT_METER_MIN_VIEWPORT;
    let master_meter = state.master_meter.clone().filter(|_| gutters_fit);
    let perf_meter = state.perf_meter.clone().filter(|_| gutters_fit);
    let on_tap_tempo = state.on_tap_tempo.clone();
    let on_tap_tempo_menu = state.on_tap_tempo_menu.clone();
    let ts_has_markers = state.ts_has_markers;
    let on_ts_menu = state.on_ts_menu.clone();
    let on_ts_edit_start = state.on_ts_edit_start.clone();
    let ts_editing = state.ts_editing;
    let ts_num_input = state.ts_num_input.clone();
    let ts_num_input_callbacks = state.ts_num_input_callbacks.clone();
    let ts_den_input = state.ts_den_input.clone();
    let ts_den_input_callbacks = state.ts_den_input_callbacks.clone();
    let ts_edit_focus_num = state.ts_edit_focus_num;

    let label_skip_back = i18n.tr_or("transport.skip-back", "<<");
    let label_play = i18n.tr_or("transport.play", ">");
    let label_stop = i18n.tr_or("transport.stop", "[]");
    let label_record = i18n.tr("transport.record");
    let label_loop = i18n.tr("transport.loop");
    let label_metronome = i18n.tr("transport.metronome");
    let label_follow = i18n.tr("transport.follow");

    // ── Left track: transport, count-in, and the mode toggles ────────────────
    let transport_group = chrome_cluster()
        .child(chrome_action_button(
            "transport-return-to-start",
            assets::ICON_SKIP_BACK_PATH,
            label_skip_back,
            None,
            Colors::text_secondary(),
            on_return,
            None,
            None,
        ))
        .child(chrome_action_button(
            "transport-play",
            assets::ICON_PLAY_PATH,
            label_play,
            Some(state.playing),
            play_color,
            on_play,
            None,
            None,
        ))
        .child(chrome_action_button(
            "transport-stop",
            assets::ICON_SQUARE_PATH,
            label_stop,
            None,
            Colors::text_secondary(),
            on_stop,
            None,
            None,
        ))
        .child(chrome_action_button(
            "transport-record",
            assets::ICON_CIRCLE_PATH,
            label_record,
            Some(state.recording),
            record_color,
            on_record,
            None,
            None,
        ));

    // Count-in split control: the label is a true on/off toggle, the chevron
    // opens the duration menu. Split buttons share an edge, so only the group's
    // outer corners round — a radius on the seam would leave a notch between
    // the two halves.
    let count_in_rest = Colors::button_bg();
    let count_in_active = Colors::composite(
        Colors::surface_titlebar(),
        Colors::with_alpha(Colors::accent_primary(), crate::theme::state::ARMED_WASH),
    );
    let count_in_fill = if state.count_in_enabled {
        count_in_active
    } else {
        // Transparent at rest; the split's own border is what holds its two
        // halves together as one control.
        Colors::with_alpha(count_in_rest, 0.0)
    };
    let count_in_hover = Colors::composite(count_in_fill, Colors::state_hover());
    let count_in_split = div()
        .h(px(crate::theme::size::DENSE))
        .flex()
        .flex_row()
        .items_center()
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(if state.count_in_enabled {
            Colors::with_alpha(Colors::accent_primary(), crate::theme::state::ARMED_BORDER)
        } else {
            Colors::button_border()
        })
        .child(
            div()
                .id("transport-count-in")
                .role(Role::Button)
                .aria_label("Count in")
                .aria_toggled(if state.count_in_enabled {
                    Toggled::True
                } else {
                    Toggled::False
                })
                .flex()
                .items_center()
                .h_full()
                .px(px(crate::theme::space::SNUG))
                .bg(count_in_fill)
                .text_color(if state.count_in_enabled {
                    Colors::accent_primary()
                } else {
                    Colors::text_muted()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(count_in_hover))
                .tooltip(fb_tooltip("Count-in before recording"))
                .on_click(move |_, window, cx| on_count_in_toggle(&(), window, cx))
                .occlude()
                .child(
                    svg()
                        .path(assets::ICON_TIMER_PATH)
                        .w(px(13.0))
                        .h(px(13.0))
                        .text_color(if state.count_in_enabled {
                            Colors::accent_primary()
                        } else {
                            Colors::text_muted()
                        }),
                ),
        )
        .child(div().w(px(1.0)).h_full().bg(Colors::border_subtle()))
        .child(
            div()
                .id("transport-count-in-menu")
                .role(Role::Button)
                .aria_label("Count-in duration")
                .flex()
                .items_center()
                .justify_center()
                .h_full()
                .w(px(14.0))
                .bg(count_in_fill)
                .text_color(Colors::text_muted())
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(count_in_hover))
                .tooltip(fb_tooltip("Count-in duration"))
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    on_count_in_menu(&(x, y), window, cx);
                })
                .occlude()
                // A real chevron asset rather than the "▾" character, which
                // depends on the UI font having the glyph and does not scale or
                // align with the SVG icons either side of it.
                .child(
                    svg()
                        .path(assets::ICON_CHEVRON_DOWN_PATH)
                        .w(px(9.0))
                        .h(px(9.0))
                        .text_color(Colors::text_muted()),
                ),
        );

    let mode_group = chrome_cluster()
        .child(chrome_action_button(
            "transport-loop",
            assets::ICON_REPEAT_PATH,
            label_loop,
            Some(state.loop_enabled),
            loop_color,
            on_loop,
            None,
            None,
        ))
        .child(chrome_action_button(
            "transport-metronome",
            assets::ICON_METRONOME_PATH,
            label_metronome,
            Some(state.metronome_enabled),
            metronome_color,
            on_metronome,
            None,
            None,
        ))
        .child(chrome_action_button(
            "transport-follow-playhead",
            assets::TIMELINE_SCROLL_PATH,
            label_follow,
            Some(state.follow_playhead),
            follow_color,
            on_follow,
            None,
            Some(Arc::new(move |_, window, cx| {
                on_follow_mode(&(), window, cx);
            })),
        ));

    // ── Centre track: the readout panel ──────────────────────────────────────
    let position_value = div()
        .id("transport-position")
        .role(Role::Label)
        .aria_label(format!("Playhead position {}", state.position_label))
        .min_w(px(72.0))
        .flex()
        .items_center()
        .justify_center()
        .text_color(Colors::text_primary())
        .text_size(px(17.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(state.position_label)
        .into_any_element();

    let tempo_value = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(crate::theme::space::TIGHT))
        .child(bpm_display(
            bpm_value,
            bpm_label,
            on_bpm_drag,
            on_bpm_drag_end,
            on_bpm_menu,
            on_bpm_edit_start,
            bpm_editing,
            &bpm_input,
            bpm_input_callbacks,
            bpm_edit_focused,
        ))
        // AUTO badge — only when tempo automation is active, so the
        // single-tempo case stays clean.
        .children(if bpm_has_automation {
            Some(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(13.0))
                    .px(px(3.0))
                    .rounded(px(crate::theme::radius::MICRO))
                    .bg(Colors::with_alpha(Colors::state_automation(), 0.18))
                    .border(px(1.0))
                    .border_color(Colors::with_alpha(Colors::state_automation(), 0.45))
                    .text_color(Colors::state_automation())
                    .text_size(px(8.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("AUTO"),
            )
        } else {
            None
        })
        .into_any_element();

    let ts_digit = |text: String| {
        div()
            .min_w(px(14.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(Colors::text_primary())
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(text)
    };
    let ts_value = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(1.0))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
            let x: f32 = event.position.x.into();
            let y: f32 = event.position.y.into();
            on_ts_menu(&(x, y), window, cx);
        })
        .children(if ts_editing {
            vec![
                div()
                    .w(px(22.0))
                    .child(text_field_with_callbacks(
                        &ts_num_input,
                        ts_edit_focus_num,
                        ts_num_input_callbacks,
                    ))
                    .into_any_element(),
                div()
                    .text_color(Colors::text_faint())
                    .text_size(px(12.0))
                    .child("/")
                    .into_any_element(),
                div()
                    .w(px(22.0))
                    .child(text_field_with_callbacks(
                        &ts_den_input,
                        !ts_edit_focus_num,
                        ts_den_input_callbacks,
                    ))
                    .into_any_element(),
            ]
        } else {
            let on_ts_edit = on_ts_edit_start.clone();
            let (num, den) = state
                .time_signature_label
                .split_once('/')
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .unwrap_or_else(|| ("4".to_string(), "4".to_string()));
            vec![div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(1.0))
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    if event.click_count >= 2 {
                        on_ts_edit(&(), window, cx);
                    }
                })
                .child(ts_digit(num))
                .child(
                    div()
                        .text_color(Colors::text_faint())
                        .text_size(px(12.0))
                        .child("/"),
                )
                .child(ts_digit(den))
                .into_any_element()]
        })
        .into_any_element();

    let readout = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .h(px(crate::shell_metrics::TRANSPORT_READOUT_HEIGHT))
        .px(px(crate::theme::space::TIGHT))
        .rounded(px(crate::theme::radius::SURFACE))
        // Darkest plane in the shell. The readout is the one element here that
        // is a display rather than a control, and giving it the canvas token
        // separates it from the recessed plates the buttons sit in.
        .bg(Colors::surface_canvas())
        .border(px(1.0))
        .border_color(Colors::border_normal())
        .child(lcd_field(
            "lcd-position",
            // Not the playhead-handle asset: that is a solid filled triangle
            // drawn for the ruler grip, and at label size it out-weighs the two
            // outline glyphs beside it instead of sitting quietly under them.
            assets::ICON_CLOCK_PATH,
            "Playhead position (bars.beats)",
            position_value,
        ))
        .child(lcd_divider())
        .child(lcd_field(
            "lcd-tempo",
            assets::ICON_METRONOME_PATH,
            "Tempo (BPM) — drag to change, double-click to type",
            tempo_value,
        ))
        .child(lcd_divider())
        .child(lcd_field(
            "lcd-timesig",
            assets::ICON_MUSIC_PATH,
            "Time signature — double-click to edit, right-click for markers",
            ts_value,
        ))
        .children(if ts_has_markers {
            Some(
                div()
                    .mr(px(crate::theme::space::SNUG))
                    .px(px(3.0))
                    .py(px(1.0))
                    .rounded(px(crate::theme::radius::MICRO))
                    .bg(Colors::with_alpha(Colors::state_automation(), 0.16))
                    .text_size(px(8.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::state_automation())
                    .child("AUTO"),
            )
        } else {
            None
        })
        .child(lcd_divider())
        .child(
            div()
                .px(px(crate::theme::space::BASE))
                .child(tap_tempo_chip(
                    tap_tempo_session_taps,
                    on_tap_tempo,
                    on_tap_tempo_menu,
                )),
        );

    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .flex_none()
        .h(px(crate::shell_metrics::TRANSPORT_BAR_HEIGHT))
        .px(px(crate::theme::space::BASE))
        .gap(px(crate::theme::space::BASE))
        // One step up the ramp from the titlebar. The two bands share an edge,
        // so if they share a surface token as well the shell reads as one
        // 76px-tall slab of chrome instead of two rows with different jobs.
        .bg(Colors::surface_base())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        // Left and right tracks are `flex_1` gutters carrying the two
        // readouts; the whole control block between them is centred as one
        // object. Pinning the transport to the far left and the readout to the
        // centre left a dead gap between two things a player uses together.
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_start()
                .children(perf_meter),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                .gap(px(crate::theme::space::SNUG))
                .child(transport_group)
                .child(count_in_split)
                .child(mode_group)
                .child(div().w(px(crate::theme::space::SNUG)))
                .child(readout),
        )
        // The right gutter carries the master strip. It stays `flex_1` with a
        // zero basis so the readout keeps the true window centre whenever there
        // is room for both gutters to reach the strip's width; below that the
        // strip is already gone.
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .children(master_meter),
        )
}

fn panel_toggle_button(
    id: &'static str,
    icon_path: &'static str,
    fallback: impl Into<gpui::SharedString>,
    active: bool,
    on_click: ChromeActionCb,
    shortcut: Option<String>,
) -> impl IntoElement {
    let color = if active {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    chrome_action_button(id, icon_path, fallback, Some(active), color, on_click, shortcut, None)
}

fn panel_toggles(state: PanelChromeState, i18n: I18n) -> impl IntoElement {
    let on_browser = state.on_toggle_browser.clone();
    let on_bottom_panel = state.on_toggle_bottom_panel.clone();
    let on_inspector = state.on_toggle_inspector.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .child(panel_toggle_button(
            "panel-browser-toggle",
            assets::ICON_FOLDER_OPEN_PATH,
            i18n.tr("panel.browser"),
            state.browser_visible,
            on_browser,
            None,
        ))
        .child(panel_toggle_button(
            "panel-bottom-toggle",
            assets::ICON_PANEL_BOTTOM_PATH,
            i18n.tr("panel.bottom"),
            state.bottom_panel_visible,
            on_bottom_panel,
            None,
        ))
        .child(panel_toggle_button(
            "panel-inspector-toggle",
            assets::ICON_PANEL_RIGHT_PATH,
            i18n.tr("panel.inspector"),
            state.inspector_visible,
            on_inspector,
            None,
        ))
}

#[allow(dead_code)]
fn utility_buttons(_i18n: I18n) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .px(px(2.0))
        // Import audio, Save, Share - actions handled by menu commands
        // Use menu bar or command palette for these instead
}

#[allow(dead_code)]
fn report_bug_button(i18n: I18n) -> impl IntoElement {
    let amber_bg = Colors::with_alpha(Colors::status_warning(), 0.07);
    let amber_text = Colors::with_alpha(Colors::status_warning(), 0.70);
    let amber_border = Colors::with_alpha(Colors::status_warning(), 0.22);
    let label = i18n.tr("chrome.report-bug");

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(24.0))
        .px(px(8.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(amber_bg)
        .border_1()
        .border_color(amber_border)
        .hover(|s| {
            s.bg(Colors::with_alpha(Colors::status_warning(), 0.14))
                .border_color(Colors::with_alpha(Colors::status_warning(), 0.40))
        })
        .child(
            svg()
                .path(assets::ICON_BUG_PATH)
                .w(px(11.0))
                .h(px(11.0))
                .text_color(amber_text),
        )
        .child(
            div()
                .text_color(amber_text)
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(label),
        )
        .occlude()
}

// ── Account chip ──────────────────────────────────────────────────────────────

/// Compact app-chrome account control. Present only on builds that install an account
/// provider (Professional Edition); Community builds render nothing. Signed out it
/// is a "Sign in" chip; signed in it shows only the compact avatar and opens
/// the account menu. Reads the account snapshot fresh each render, so sign-in /
/// sign-out reflect here without extra wiring.
///
/// Public because it is not Studio's chip, it is *the* account control: the
/// Welcome window draws it, and so does the standalone Audio Jam client, which
/// needs an account before it can join anything at all. A second implementation
/// of a sign-in affordance is how two surfaces end up disagreeing about who is
/// signed in.
pub fn account_chip() -> Option<impl IntoElement> {
    let snapshot = crate::account::current_account()?;
    let signed_in = snapshot.signed_in;
    let action = if signed_in {
        crate::account::AccountAction::OpenMenu
    } else {
        crate::account::AccountAction::SignIn
    };
    let text_color = if signed_in {
        Colors::text_secondary()
    } else {
        Colors::text_muted()
    };
    let accessible_label = if signed_in {
        snapshot
            .username
            .as_deref()
            .or(snapshot.email.as_deref())
            .map(|name| format!("Account menu for {name}"))
            .unwrap_or_else(|| "Account menu".to_string())
    } else {
        "Sign in".to_string()
    };

    let mut chip = div()
        .id("account-menu")
        .role(Role::Button)
        .aria_label(accessible_label)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(Colors::surface_control_hover()))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(24.0))
        .px(px(6.0))
        .mr(px(4.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_control_hover()))
        .on_click(move |_, window, cx| {
            crate::account::dispatch_account_action(action, window, cx);
        });

    chip = if signed_in {
        chip.child(account_avatar(&snapshot))
    } else {
        chip.child(
            svg()
                .path(assets::ICON_USER_PATH)
                .w(px(13.0))
                .h(px(13.0))
                .text_color(text_color),
        )
    };

    Some(if signed_in {
        chip.occlude()
    } else {
        chip.child(
            div()
                .text_size(px(11.0))
                .text_color(text_color)
                .child("Sign in"),
        )
        .occlude()
    })
}

/// Compact circular avatar badge.
///
/// Shows the identity provider's profile picture once it has been downloaded,
/// and the user's first initial until then — which is also what a provider that
/// serves no picture, or one that failed to fetch, keeps showing. The badge
/// never changes size between the two, so the chrome does not reflow when the
/// image lands.
fn account_avatar(snapshot: &crate::account::AccountSnapshot) -> impl IntoElement {
    account_avatar_sized(snapshot, AVATAR_SIZE)
}

/// The badge at an explicit diameter. The menu draws it larger than the chip,
/// and used to draw its own initial-only version that never showed the
/// downloaded picture.
fn account_avatar_sized(snapshot: &crate::account::AccountSnapshot, size: f32) -> gpui::AnyElement {
    let frame = div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .rounded(px(crate::theme::radius::PILL))
        .overflow_hidden();

    if let Some(image) = crate::account::account_avatar_image() {
        return frame
            .child(
                gpui::img(image)
                    .w(px(size))
                    .h(px(size))
                    .rounded(px(crate::theme::radius::PILL)),
            )
            .into_any_element();
    }

    let initial = snapshot
        .username
        .as_deref()
        .or(snapshot.email.as_deref())
        .and_then(|value| value.trim().chars().next())
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());

    frame
        .bg(Colors::accent_muted())
        .text_size(px(size * 0.55))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::accent_primary())
        .child(initial)
        .into_any_element()
}

/// Diameter of the account badge. One constant so the picture and the initial
/// fallback cannot drift apart and reflow the titlebar between them.
const AVATAR_SIZE: f32 = 18.0;
/// Diameter of the same badge inside the account menu, where there is room for
/// the picture to be legible.
const AVATAR_SIZE_MENU: f32 = 28.0;
/// Width of the account menu panel.
const ACCOUNT_MENU_WIDTH: f32 = 232.0;
/// Gap between the titlebar's lower edge and the menu.
const ACCOUNT_MENU_DROP: f32 = 4.0;
/// Paints above the menu bar's own dropdown, which is the only other overlay
/// the chrome can have open at the same time.
const ACCOUNT_MENU_PRIORITY: usize = 130;

/// The account menu, as an in-window overlay.
///
/// Rendered by whichever window draws the chip — the Studio's chrome and the
/// Welcome window's own header both do — from an element whose origin is the
/// window's top-left. That anchor is what lets the dismiss backdrop cover the
/// window rather than the strip the chip sits in. `deferred` paints it after
/// every sibling, so the host needs to be neither a scroll nor a clip owner.
///
/// `anchor_top` is the distance from that origin down to the bottom of the bar
/// holding the chip; `anchor_right` is that bar's own right padding, so the
/// menu lines up with the chip instead of the window edge.
pub fn account_menu_overlay(
    window: &gpui::Window,
    anchor_top: f32,
    anchor_right: f32,
) -> Option<gpui::AnyElement> {
    if !crate::account::account_menu_open() {
        return None;
    }
    let snapshot = crate::account::current_account()?;
    if !snapshot.signed_in {
        // Signed out between the click and this frame; nothing to show.
        crate::account::set_account_menu_open(false);
        return None;
    }
    let name = snapshot
        .username
        .clone()
        .or_else(|| snapshot.email.clone())
        .unwrap_or_else(|| "Account".to_string());
    let email = snapshot.email.clone().unwrap_or_default();
    let viewport = window.bounds().size;

    let backdrop = div()
        .id("account-menu-backdrop")
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .on_mouse_down(MouseButton::Left, |_, _window, cx| {
            crate::account::set_account_menu_open(false);
            cx.refresh_windows();
        });

    let panel = div()
        .absolute()
        .top(px(anchor_top + ACCOUNT_MENU_DROP))
        .right(px(anchor_right))
        .w(px(ACCOUNT_MENU_WIDTH))
        .flex()
        .flex_col()
        .p(px(crate::theme::space::BASE))
        .gap(px(crate::theme::space::BASE))
        .rounded(px(crate::theme::radius::SURFACE))
        // `surface_overlay` is the modal scrim (65% alpha), not a popover
        // plane — the RECENT list read straight through the menu. This is
        // the surface the select menus already raise onto.
        .bg(Colors::surface_panel_raised())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .shadow(crate::theme::elevation::shadow(
            crate::theme::elevation::OVERLAY,
        ))
        // Clicks inside the menu must not reach the backdrop behind it.
        .occlude()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(crate::theme::space::BASE))
                .child(account_avatar_sized(&snapshot, AVATAR_SIZE_MENU))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(crate::theme::typography::UI_SM))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(name),
                        )
                        .children((!email.is_empty()).then(|| {
                            div()
                                .truncate()
                                .text_size(px(crate::theme::typography::UI_XS))
                                .text_color(Colors::text_muted())
                                .child(email)
                        })),
                ),
        )
        .child(div().h(px(1.0)).w_full().bg(Colors::border_subtle()))
        .child(crate::components::controls::fb_button(
            "account-menu-sign-out",
            "Sign out",
            crate::components::controls::FbButtonKind::Default,
            true,
            |_, window, cx| {
                crate::account::dispatch_account_action(
                    crate::account::AccountAction::SignOut,
                    window,
                    cx,
                );
                cx.refresh_windows();
            },
        ));

    Some(
        gpui::deferred(
            // Sized to the window, not left to collapse: the panel is placed
            // with `right`, which resolves against *this* box. A zero-width
            // wrapper put the menu at `-width` — off the left edge, with only
            // the transparent backdrop still hit-testing, which is why the chip
            // appeared to do nothing at all.
            div()
                .absolute()
                .top_0()
                .left_0()
                .w(viewport.width)
                .h(viewport.height)
                .child(backdrop)
                .child(panel),
        )
        .with_priority(ACCOUNT_MENU_PRIORITY)
        .into_any_element(),
    )
}

fn window_controls(
    window: &gpui::Window,
    on_close: Option<ChromeActionCb>,
    i18n: I18n,
) -> impl IntoElement {
    let is_maximized = window.is_maximized();
    let (max_path, max_fallback) = if is_maximized {
        (assets::ICON_RESTORE_PATH, i18n.tr("window.restore"))
    } else {
        (assets::ICON_MAXIMIZE_PATH, i18n.tr("window.maximize"))
    };
    let min_fallback = i18n.tr_or("window.minimize", "-");
    let close_fallback = i18n.tr_or("window.close", "X");

    let control_button = |area: WindowControlArea,
                          icon_path: &'static str,
                          fallback_text: gpui::SharedString,
                          on_linux: Option<ChromeActionCb>| {
        let id = match area {
            WindowControlArea::Min => "studio-window-minimize",
            WindowControlArea::Max => "studio-window-maximize-or-restore",
            WindowControlArea::Close => "studio-window-close",
            WindowControlArea::Drag => "studio-window-drag",
        };
        let accessible_action = on_linux.clone();
        let button = crate::components::title_bar::window_control_icon(
            area,
            icon_path,
            fallback_text.clone(),
        )
        .id(id)
        .role(Role::Button)
        .aria_label(fallback_text)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(Colors::surface_control_hover()))
        .on_a11y_action(AccessibleAction::Click, move |_, window, cx| match area {
            WindowControlArea::Min => window.minimize_window(),
            WindowControlArea::Max => window.zoom_window(),
            WindowControlArea::Close => {
                if let Some(action) = accessible_action.as_ref() {
                    action(&(), window, cx);
                } else {
                    window.remove_window();
                }
            }
            WindowControlArea::Drag => {}
        })
        .w(px(WINDOW_CONTROL_WIDTH))
        .h(px(crate::components::title_bar::TITLEBAR_HEIGHT))
        .rounded(px(crate::theme::radius::NONE))
        .hover(move |style| {
            style.bg(if area == WindowControlArea::Close {
                Colors::accent_danger()
            } else {
                Colors::surface_control_hover()
            })
        })
        .window_control_area(area)
        .occlude();

        #[cfg(target_os = "linux")]
        let button = {
            let action = on_linux.clone();
            button.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                match area {
                    WindowControlArea::Min => window.minimize_window(),
                    WindowControlArea::Max => window.zoom_window(),
                    WindowControlArea::Close => {
                        if let Some(action) = action.as_ref() {
                            action(&(), window, cx);
                        } else {
                            window.remove_window();
                        }
                    }
                    WindowControlArea::Drag => {}
                }
            })
        };

        #[cfg(not(target_os = "linux"))]
        {
            let _ = on_linux;
        }

        button
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .child(control_button(
            WindowControlArea::Min,
            assets::ICON_MINIMIZE_PATH,
            min_fallback.into(),
            None,
        ))
        .child(control_button(
            WindowControlArea::Max,
            max_path,
            max_fallback.into(),
            None,
        ))
        .child(control_button(
            WindowControlArea::Close,
            assets::ICON_X_PATH,
            close_fallback.into(),
            on_close,
        ))
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn app_chrome(
    window: &gpui::Window,
    open_menu_id: Option<&str>,
    on_open_menu: MenuOpenCb,
    project: ProjectChromeState,
    transport: TransportChromeState,
    panels: PanelChromeState,
    on_window_close: Option<ChromeActionCb>,
    i18n: I18n,
) -> impl IntoElement {
    let policy = PlatformChromePolicy::current();
    let viewport_width: f32 = window.bounds().size.width.into();
    let chrome_left: f32 = policy.traffic_light_left_padding().into();
    let menu_width = if policy.show_in_window_menubar {
        menu_bar::menu_bar_chrome_width(viewport_width, i18n) + 7.0
    } else {
        0.0
    };
    // The project control is centred in the window now, so its dropdown must be
    // anchored to where it is actually drawn — not to the end of the menu bar,
    // which is where it used to sit. `project_title` hands this x straight to
    // the overlay, so getting it wrong opens the menu under empty chrome.
    let project_anchor_x = (viewport_width * 0.5 - PROJECT_CHIP_MAX_WIDTH * 0.5)
        .max(chrome_left + menu_width + crate::theme::space::BASE);

    let mut chrome = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(policy.titlebar_height_px))
        .w_full()
        .bg(Colors::surface_titlebar())
        .pl(policy.traffic_light_left_padding())
        // Windows: NCHITTEST callback returns `HTCAPTION` for hitboxes
        // tagged Drag, letting DefWindowProc start the system move.
        .window_control_area(WindowControlArea::Drag)
        // Linux (Wayland / X11) and macOS: `start_window_move` is the
        // implemented drag API there; the WindowControlArea path is a
        // no-op on those platforms. Safe to attach here because every
        // interactive child below (menu buttons, transport buttons,
        // window controls, report-bug) calls `.occlude()`. Occlude is
        // `HitboxBehavior::BlockMouse`, which breaks the `hit_test`
        // iteration at that child — the chrome's id is then NOT in
        // `mouse_hit_test.ids`, so this on_mouse_down does NOT fire
        // for clicks on those buttons.
        .on_mouse_down(MouseButton::Left, |_, window, _cx| {
            window.start_window_move();
        });

    // Three tracks, so the project control lands on the true window centre
    // regardless of how wide the menu bar or the right-hand cluster is. Both
    // side tracks are `flex_1` with a zero basis, so they always share the
    // leftover width equally and the middle stays centred.
    let mut left = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden();
    if policy.show_in_window_menubar {
        left = left.child(menu_area(open_menu_id, on_open_menu, viewport_width, i18n));
    }
    left = left.child(draggable_spacer());

    let mut right = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .flex_1()
        .min_w(px(0.0))
        .child(draggable_spacer())
        .child(panel_toggles(panels, i18n));

    // Account chip sits between the panel toggles and the window controls. Only
    // an Exclusive build with an installed account provider renders it.
    if let Some(chip) = account_chip() {
        right = right.child(section_separator()).child(chip);
    }
    if policy.show_window_controls {
        right = right.child(window_controls(window, on_window_close, i18n));
    }

    chrome = chrome
        .child(left)
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(project_title(project, project_anchor_x, i18n)),
        )
        .child(right);

    div()
        .flex()
        .flex_col()
        .w_full()
        .flex_none()
        // Sized explicitly rather than by its two children: a cached
        // `AppChromeView` lays out from a style with no children at all, so an
        // implicit height there silently collapsed the transport row out of the
        // window. Stating it here means the two paths cannot disagree.
        .h(px(app_chrome_drawn_height()))
        .child(chrome)
        .child(transport_bar(transport, viewport_width, i18n))
        // Anchored here rather than in the titlebar row: this element starts at
        // the window's top-left, which is what lets the dismiss backdrop cover
        // the window instead of a 32px strip.
        .children(account_menu_overlay(
            window,
            policy.titlebar_height_px,
            crate::theme::space::BASE,
        ))
}

/// Drawn height of the whole chrome: the titlebar band plus the transport bar
/// under it.
///
/// Deliberately *not* `shell_metrics::APP_CHROME_HEIGHT`. That constant is the
/// timeline's hit-test origin and carries a hand-tuned titlebar band (36) that
/// is a few pixels taller than the one actually drawn (32); using it here would
/// leave a gap under the transport bar.
pub fn app_chrome_drawn_height() -> f32 {
    PlatformChromePolicy::current().titlebar_height_px + crate::shell_metrics::TRANSPORT_BAR_HEIGHT
}
