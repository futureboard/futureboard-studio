//! Reusable Color Picker Popover.
//!
//! A single color-selection UI shared by every track-color call site (Add
//! Track, Inspector, Mixer, clips, automation lanes, …). It supports the DAW
//! preset palette as quick presets, arbitrary custom colors (hex + RGB
//! sliders), recent custom colors, and an optional Auto Color toggle.
//!
//! ## Ownership model
//!
//! Like [`crate::components::form::select`], the popover is *state-driven and
//! host-owned*: the host entity stores a [`ColorPickerState`] (open flag, draft
//! color, hex text field, recent colors) and renders [`color_picker_field`].
//! All mutation flows back through [`ColorPickerCallbacks`] closures the host
//! supplies, so the same component drops into any window without bespoke
//! popover plumbing. The deferred popover paints above sibling rows / footers
//! and escapes scroll-container clipping; pair it with a
//! [`crate::components::form::select_dismiss_backdrop`] rendered at the dialog
//! root (gated on `state.open`) for click-outside dismissal.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    anchored, deferred, div, point, px, svg, Anchor, App, AppContext, DragMoveEvent, Empty,
    InteractiveElement, IntoElement, ParentElement, Render, Rgba, StatefulInteractiveElement,
    Styled, Window,
};

use crate::assets;
use crate::color::{
    self, color_picker_debug, hsv_to_rgba, normalize_color, parse_hex_color, push_recent_color,
    rgba_to_hex, rgba_to_hsv,
};
use crate::components::controls::fb_checkbox;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::theme::Colors;

/// Height of the 2D saturation/value picking area.
const SV_AREA_HEIGHT: f32 = 132.0;
/// Height of the hue rainbow strip.
const HUE_BAR_HEIGHT: f32 = 14.0;
/// Opaque white / black / transparent stops for the SV gradients.
const WHITE: Rgba = Rgba {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
};
const OPAQUE_BLACK: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};
const CLEAR_BLACK: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};

/// Drag payload for the 2D saturation/value area.
#[derive(Clone, Debug)]
struct SvDrag;

impl Render for SvDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Drag payload for the hue strip.
#[derive(Clone, Debug)]
struct HueDrag;

impl Render for HueDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Paint priority so the popover sits above ordinary deferred content (0) and
/// the select dropdowns (100) are unaffected — color pickers and selects are
/// never open at the same anchor.
const COLOR_POPOVER_PRIORITY: usize = 120;

/// Popover width — compact enough for DAW UI (spec: 260–320px max).
const POPOVER_WIDTH: f32 = 264.0;

/// Where the popover opens relative to its trigger swatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorPickerPlacement {
    Below,
    Above,
}

/// The value emitted by the picker. `auto` true means "Auto Color"; otherwise
/// `color` carries the chosen custom/preset color.
#[derive(Clone, Copy, Debug)]
pub struct ColorPickerValue {
    pub color: Option<Rgba>,
    pub auto: bool,
}

impl ColorPickerValue {
    pub fn auto() -> Self {
        Self {
            color: None,
            auto: true,
        }
    }

    pub fn custom(color: Rgba) -> Self {
        Self {
            color: Some(normalize_color(color)),
            auto: false,
        }
    }
}

/// Host-owned state for one color picker instance.
pub struct ColorPickerState {
    pub open: bool,
    /// Current working color (the live preview), always a concrete color even
    /// while Auto is selected (so toggling Auto off restores a sensible color).
    pub draft: Rgba,
    /// Auto Color selected.
    pub auto: bool,
    /// Hex text field (non-IME; the host routes key events to it).
    pub hex_input: TextInputState,
    /// Inline parse error for the hex field, if any.
    pub hex_error: Option<String>,
    /// Persisted hue (`0.0..=1.0`) for the HSV picker. Kept separately from
    /// `draft` so dragging saturation/value to an edge (gray or black, where hue
    /// is mathematically undefined) does not lose the chosen hue.
    pub hue: f32,
    /// Recent custom colors (most-recent first), loaded from user prefs.
    pub recent: Vec<String>,
}

impl ColorPickerState {
    /// Build a new picker. `hex_field_id` must be a unique static id and
    /// `focus_handle` a fresh handle owned by the host entity.
    pub fn new(
        hex_field_id: &'static str,
        focus_handle: gpui::FocusHandle,
        initial: ColorPickerValue,
        fallback: Rgba,
        recent: Vec<String>,
    ) -> Self {
        let draft = normalize_color(initial.color.unwrap_or(fallback));
        let mut hex_input =
            TextInputState::new(hex_field_id, focus_handle).with_accessible_label("Hex color");
        hex_input.set_value(rgba_to_hex(draft));
        Self {
            open: false,
            draft,
            auto: initial.auto,
            hex_input,
            hex_error: None,
            hue: rgba_to_hsv(draft).0,
            recent,
        }
    }

    /// Re-seed the picker for a new context (e.g. dialog reopened). Closes the
    /// popover and refreshes the draft / hex field; recent colors are kept.
    pub fn reset(&mut self, value: ColorPickerValue, fallback: Rgba) {
        self.draft = normalize_color(value.color.unwrap_or(fallback));
        self.auto = value.auto;
        self.hex_error = None;
        self.open = false;
        self.adopt_hue_from_draft();
        self.sync_hex_text();
    }

    /// Current picker color as `(hue, saturation, value)`, each `0.0..=1.0`.
    /// Uses the persisted `hue` (stable at achromatic edges) with the draft's
    /// saturation / value.
    pub fn hsv(&self) -> (f32, f32, f32) {
        let (_h, s, v) = rgba_to_hsv(self.draft);
        (self.hue, s, v)
    }

    /// Refresh the stored hue from the draft, but only when the draft has enough
    /// chroma for hue to be meaningful (avoids snapping hue to 0 on gray/black).
    fn adopt_hue_from_draft(&mut self) {
        let (h, s, v) = rgba_to_hsv(self.draft);
        if s > 0.001 && v > 0.001 {
            self.hue = h;
        }
    }

    /// Set the hue from the hue strip, preserving the draft's saturation/value.
    pub fn set_hue(&mut self, hue: f32) {
        let (_h, s, v) = rgba_to_hsv(self.draft);
        self.hue = hue.rem_euclid(1.0);
        self.draft = normalize_color(hsv_to_rgba(self.hue, s, v));
        self.auto = false;
        self.hex_error = None;
        self.sync_hex_text();
    }

    /// Set saturation/value from the 2D area, preserving the persisted hue.
    pub fn set_saturation_value(&mut self, saturation: f32, value: f32) {
        self.draft = normalize_color(hsv_to_rgba(
            self.hue,
            saturation.clamp(0.0, 1.0),
            value.clamp(0.0, 1.0),
        ));
        self.auto = false;
        self.hex_error = None;
        self.sync_hex_text();
    }

    pub fn value(&self) -> ColorPickerValue {
        if self.auto {
            ColorPickerValue::auto()
        } else {
            ColorPickerValue::custom(self.draft)
        }
    }

    fn sync_hex_text(&mut self) {
        self.hex_input.set_value(rgba_to_hex(self.draft));
        self.hex_input.clear_selection();
    }

    /// Open the popover, refreshing the hex field from the current draft.
    pub fn open(&mut self) {
        self.open = true;
        self.hex_error = None;
        self.sync_hex_text();
        color_picker_debug(&format!(
            "open auto={} draft={}",
            self.auto,
            rgba_to_hex(self.draft)
        ));
    }

    pub fn close(&mut self) {
        if self.open {
            self.open = false;
            self.hex_error = None;
            color_picker_debug("close");
        }
    }

    /// Set the draft to a concrete color (preset / recent / hex commit).
    /// Clears Auto, adopts the color's hue, and syncs the hex field.
    pub fn set_color(&mut self, color: Rgba) {
        self.draft = normalize_color(color);
        self.adopt_hue_from_draft();
        self.auto = false;
        self.hex_error = None;
        self.sync_hex_text();
        color_picker_debug(&format!("set color={}", rgba_to_hex(self.draft)));
    }

    /// Toggle Auto Color. Enabling it clears the inline error but preserves the
    /// draft color so it can be restored if Auto is turned back off.
    pub fn set_auto(&mut self, auto: bool) {
        self.auto = auto;
        self.hex_error = None;
        color_picker_debug(&format!("auto={auto}"));
    }

    /// Re-parse the hex field on every keystroke for a live preview. A valid
    /// value updates the draft and clears Auto; an invalid value records an
    /// inline error but does **not** mutate the draft (so the project is not
    /// dirtied until a valid commit).
    pub fn on_hex_changed(&mut self) {
        let raw = self.hex_input.value.clone();
        if raw.trim().is_empty() {
            self.hex_error = None;
            return;
        }
        match parse_hex_color(&raw) {
            Ok(color) => {
                self.draft = normalize_color(color);
                self.adopt_hue_from_draft();
                self.auto = false;
                self.hex_error = None;
                color_picker_debug(&format!(
                    "parsed hex={} -> {}",
                    raw,
                    rgba_to_hex(self.draft)
                ));
            }
            Err(e) => {
                self.hex_error = Some(e.to_string());
                color_picker_debug(&format!("invalid hex={raw}: {e}"));
            }
        }
    }

    /// Commit the hex field (Enter). Returns the committed color when valid.
    pub fn commit_hex(&mut self) -> Option<Rgba> {
        match parse_hex_color(&self.hex_input.value) {
            Ok(color) => {
                self.set_color(color);
                Some(self.draft)
            }
            Err(e) => {
                self.hex_error = Some(e.to_string());
                None
            }
        }
    }

    /// Record the current draft as a recent custom color and persist the list.
    /// No-op while Auto is selected.
    pub fn remember_current(&mut self) {
        if self.auto {
            return;
        }
        push_recent_color(&mut self.recent, &rgba_to_hex(self.draft));
        color::save_recent_colors(&self.recent);
        color_picker_debug(&format!("recent updated -> {:?}", self.recent));
    }
}

/// Closures the host supplies to mutate its [`ColorPickerState`] and propagate
/// the resulting color downstream (e.g. update the track, mark dirty).
#[derive(Clone)]
pub struct ColorPickerCallbacks {
    /// Toggle the popover open/closed (fired by the trigger swatch).
    pub on_toggle: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
    /// Close the popover (Escape / outside click handled by the host).
    pub on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
    /// Commit a concrete color (preset / recent swatch click).
    pub on_pick: Arc<dyn Fn(Rgba, &mut Window, &mut App) + 'static>,
    /// Hue strip dragged — carries the normalized hue (`0.0..=1.0`).
    pub on_hue: Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>,
    /// Saturation/value area dragged — carries `(saturation, value)`.
    pub on_sv: Arc<dyn Fn(f32, f32, &mut Window, &mut App) + 'static>,
    /// Auto Color toggled.
    pub on_auto: Arc<dyn Fn(bool, &mut Window, &mut App) + 'static>,
}

fn section_label(text: &'static str) -> impl IntoElement {
    div()
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(Colors::text_faint())
        .child(text)
}

/// A small color square with an optional selection ring.
fn swatch(
    id: impl Into<gpui::ElementId>,
    color: Rgba,
    size: f32,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .w(px(size))
        .h(px(size))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(if active { 2.0 } else { 1.0 }))
        .border_color(if active {
            Colors::text_primary()
        } else {
            Colors::with_alpha(Colors::text_primary(), 0.22)
        })
        .bg(color)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.border_color(Colors::border_strong()))
        .on_click(on_click)
}

/// "Auto" preview chip — a diagonal-free neutral chip labelled A.
fn auto_chip(size: f32) -> impl IntoElement {
    div()
        .w(px(size))
        .h(px(size))
        .rounded(px(crate::theme::radius::CONTROL))
        .flex()
        .items_center()
        .justify_center()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(Colors::text_faint())
        .child("A")
}

/// The trigger swatch button. Shows the current color (or an Auto chip) and a
/// chevron; clicking toggles the popover.
pub fn color_picker_trigger(
    id: impl Into<gpui::ElementId>,
    value: ColorPickerValue,
    open: bool,
    on_toggle: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let preview: gpui::AnyElement = if value.auto {
        auto_chip(16.0).into_any_element()
    } else {
        div()
            .w(px(16.0))
            .h(px(16.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::with_alpha(Colors::text_primary(), 0.22))
            .bg(value
                .color
                .unwrap_or_else(|| color::auto_color_for_index(0)))
            .into_any_element()
    };
    div()
        .id(id.into())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(26.0))
        .px(px(7.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(if open {
            Colors::border_focus()
        } else {
            Colors::border_subtle()
        })
        .bg(if open {
            Colors::surface_card()
        } else {
            Colors::surface_input()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(Colors::surface_control_hover())
                .border_color(Colors::border_strong())
        })
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            on_toggle(window, cx);
            window.prevent_default();
        })
        .child(preview)
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(if value.auto {
                    "Auto".to_string()
                } else {
                    rgba_to_hex(
                        value
                            .color
                            .unwrap_or_else(|| color::auto_color_for_index(0)),
                    )
                }),
        )
        .child(
            svg()
                .path(assets::ICON_CHEVRON_DOWN_PATH)
                .w(px(9.0))
                .h(px(9.0))
                .flex_shrink_0()
                .text_color(Colors::text_faint()),
        )
}

/// The 2D saturation/value picking area: a white→hue horizontal gradient with a
/// transparent→black vertical gradient on top, and a ring handle at the current
/// (saturation, value). Dragging reports the new saturation/value.
fn saturation_value_area(
    hue: f32,
    saturation: f32,
    value: f32,
    draft: Rgba,
    on_sv: Arc<dyn Fn(f32, f32, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let hue_color = hsv_to_rgba(hue, 1.0, 1.0);
    div()
        .id("color-picker-sv-area")
        .relative()
        .w_full()
        .h(px(SV_AREA_HEIGHT))
        .rounded(px(crate::theme::radius::CONTROL))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_default())
        .cursor(gpui::CursorStyle::Crosshair)
        // Base: white (left) → pure hue (right).
        .child(div().absolute().inset_0().bg(gpui::linear_gradient(
            90.0,
            gpui::linear_color_stop(WHITE, 0.0),
            gpui::linear_color_stop(hue_color, 1.0),
        )))
        // Overlay: transparent (top) → black (bottom).
        .child(div().absolute().inset_0().bg(gpui::linear_gradient(
            180.0,
            gpui::linear_color_stop(CLEAR_BLACK, 0.0),
            gpui::linear_color_stop(OPAQUE_BLACK, 1.0),
        )))
        // Ring handle centered on (saturation, value).
        .child(
            div()
                .absolute()
                .left(gpui::relative(saturation.clamp(0.0, 1.0)))
                .top(gpui::relative((1.0 - value).clamp(0.0, 1.0)))
                .ml(px(-7.0))
                .mt(px(-7.0))
                .w(px(14.0))
                .h(px(14.0))
                .rounded(px(crate::theme::radius::PILL))
                .border(px(2.0))
                .border_color(WHITE)
                .bg(draft),
        )
        .on_drag(SvDrag, |_, _, _, cx| cx.new(|_| SvDrag))
        .on_drag_move::<SvDrag>(move |event: &DragMoveEvent<SvDrag>, window, cx| {
            let bounds = event.bounds;
            let px_x: f32 = event.event.position.x.into();
            let px_y: f32 = event.event.position.y.into();
            let ox: f32 = bounds.origin.x.into();
            let oy: f32 = bounds.origin.y.into();
            let ow: f32 = f32::from(bounds.size.width).max(1.0);
            let oh: f32 = f32::from(bounds.size.height).max(1.0);
            let sat = ((px_x - ox) / ow).clamp(0.0, 1.0);
            let val = (1.0 - (px_y - oy) / oh).clamp(0.0, 1.0);
            on_sv(sat, val, window, cx);
        })
}

/// Hue rainbow strip. Rendered as six horizontal gradient segments (GPUI
/// gradients take two stops) so red→yellow→green→cyan→blue→magenta→red reads as
/// one continuous rainbow, with a ring handle at the current hue.
fn hue_strip(
    hue: f32,
    on_hue: Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let mut strip = div()
        .id("color-picker-hue")
        .relative()
        .w_full()
        .h(px(HUE_BAR_HEIGHT))
        .rounded(px(crate::theme::radius::PILL))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_default())
        .flex()
        .flex_row()
        .cursor(gpui::CursorStyle::ResizeLeftRight);
    for i in 0..6 {
        let from = hsv_to_rgba(i as f32 / 6.0, 1.0, 1.0);
        let to = hsv_to_rgba((i + 1) as f32 / 6.0, 1.0, 1.0);
        strip = strip.child(div().h_full().flex_1().bg(gpui::linear_gradient(
            90.0,
            gpui::linear_color_stop(from, 0.0),
            gpui::linear_color_stop(to, 1.0),
        )));
    }
    strip
        .child(
            div()
                .absolute()
                .top(px(-1.0))
                .bottom(px(-1.0))
                .left(gpui::relative(hue.clamp(0.0, 1.0)))
                .ml(px(-6.0))
                .w(px(12.0))
                .rounded(px(crate::theme::radius::PILL))
                .border(px(2.0))
                .border_color(WHITE)
                .bg(hsv_to_rgba(hue, 1.0, 1.0)),
        )
        .on_drag(HueDrag, |_, _, _, cx| cx.new(|_| HueDrag))
        .on_drag_move::<HueDrag>(move |event: &DragMoveEvent<HueDrag>, window, cx| {
            let bounds = event.bounds;
            let px_x: f32 = event.event.position.x.into();
            let ox: f32 = bounds.origin.x.into();
            let ow: f32 = f32::from(bounds.size.width).max(1.0);
            let hue = ((px_x - ox) / ow).clamp(0.0, 1.0);
            on_hue(hue, window, cx);
        })
}

/// The deferred popover body. Rendered as a child of the [`color_picker_field`]
/// relative wrapper; not normally called directly.
#[allow(clippy::too_many_arguments)]
fn color_picker_popover(
    state: &ColorPickerState,
    presets: &[Rgba],
    allow_auto: bool,
    placement: ColorPickerPlacement,
    hex_focused: bool,
    hex_field_callbacks: TextInputCallbacks,
    callbacks: &ColorPickerCallbacks,
) -> impl IntoElement {
    let draft = state.draft;
    let draft_hex = rgba_to_hex(draft);

    // Preset grid.
    let mut preset_grid = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(5.0))
        .items_center();
    for (i, preset) in presets.iter().enumerate() {
        let preset = normalize_color(*preset);
        let active = !state.auto && rgba_to_hex(preset) == draft_hex;
        let on_pick = callbacks.on_pick.clone();
        preset_grid = preset_grid.child(swatch(
            ("color-picker-preset", i),
            preset,
            16.0,
            active,
            move |_, window, cx| on_pick(preset, window, cx),
        ));
    }

    // Recent row.
    let recent_colors: Vec<Rgba> = state
        .recent
        .iter()
        .filter_map(|h| parse_hex_color(h).ok())
        .collect();
    let recent_section = (!recent_colors.is_empty()).then(|| {
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(5.0))
            .items_center();
        for (i, c) in recent_colors.into_iter().enumerate() {
            let active = !state.auto && rgba_to_hex(c) == draft_hex;
            let on_pick = callbacks.on_pick.clone();
            row = row.child(swatch(
                ("color-picker-recent", i),
                c,
                15.0,
                active,
                move |_, window, cx| on_pick(c, window, cx),
            ));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(section_label("RECENT"))
            .child(row)
    });

    let mut menu = div()
        .w(px(POPOVER_WIDTH))
        .rounded(px(crate::theme::radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_default())
        .bg(Colors::surface_panel_raised())
        .shadow(vec![gpui::BoxShadow {
            color: Colors::surface_overlay().into(),
            offset: gpui::point(
                px(0.0),
                px(if placement == ColorPickerPlacement::Above {
                    -8.0
                } else {
                    8.0
                }),
            ),
            blur_radius: px(24.0),
            spread_radius: px(0.0),
            inset: false,
        }])
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap(px(9.0))
        .id("color-picker-popover")
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, |_, _w, cx| {
            cx.stop_propagation();
        })
        // Header: large preview + hex field.
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .w(px(30.0))
                        .h(px(30.0))
                        .flex_shrink_0()
                        .rounded(px(crate::theme::radius::CONTROL))
                        .border(px(1.0))
                        .border_color(Colors::with_alpha(Colors::text_primary(), 0.22))
                        .bg(draft),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(text_field_with_callbacks(
                            &state.hex_input,
                            hex_focused,
                            hex_field_callbacks,
                        )),
                ),
        );

    // Inline hex error (does not panic, does not dirty).
    if let Some(error) = state.hex_error.as_ref() {
        menu = menu.child(
            div()
                .text_size(px(9.5))
                .text_color(Colors::status_error())
                .child(error.clone()),
        );
    }

    // Presets.
    menu = menu.child(
        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(section_label("PRESETS"))
            .child(preset_grid),
    );

    // Custom HSV picker — 2D saturation/value area + hue strip.
    let (hue, saturation, value) = state.hsv();
    menu = menu.child(
        div()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(section_label("CUSTOM"))
            .child(saturation_value_area(
                hue,
                saturation,
                value,
                draft,
                callbacks.on_sv.clone(),
            ))
            .child(hue_strip(hue, callbacks.on_hue.clone())),
    );

    // Recent.
    menu = menu.children(recent_section);

    // Auto toggle.
    if allow_auto {
        let on_auto = callbacks.on_auto.clone();
        let auto = state.auto;
        menu = menu.child(
            div()
                .pt(px(2.0))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .child(fb_checkbox(
                    "color-picker-auto",
                    "Auto Color",
                    auto,
                    true,
                    move |_, window, cx| on_auto(!auto, window, cx),
                )),
        );
    }

    // Render the popover as a window-fitting overlay: `anchored` snaps the
    // 264px menu inside the window viewport instead of letting it clip at the
    // dialog edge (the fixed-width popover would otherwise overflow when the
    // trigger sits near the right side — see the color box in Add Track). It
    // anchors to the trigger's top-left (grow down) or bottom-left (grow up),
    // then shifts left/up as needed to stay on-screen with an 8px margin.
    let (anchor, offset_y) = match placement {
        ColorPickerPlacement::Below => (Anchor::TopLeft, px(30.0)),
        ColorPickerPlacement::Above => (Anchor::BottomLeft, px(-6.0)),
    };
    anchored()
        .anchor(anchor)
        .snap_to_window_with_margin(px(8.0))
        .offset(point(px(0.0), offset_y))
        .child(menu)
}

/// Render the trigger swatch plus (when open) the deferred popover, wrapped in a
/// relative container so the popover anchors to the trigger. Render a
/// [`crate::components::form::select_dismiss_backdrop`] at the dialog root,
/// gated on `state.open`, for click-outside dismissal.
#[allow(clippy::too_many_arguments)]
pub fn color_picker_field(
    id: impl Into<gpui::ElementId>,
    state: &ColorPickerState,
    presets: &[Rgba],
    allow_auto: bool,
    placement: ColorPickerPlacement,
    hex_focused: bool,
    hex_field_callbacks: TextInputCallbacks,
    callbacks: ColorPickerCallbacks,
) -> impl IntoElement {
    let value = state.value();
    let trigger = color_picker_trigger(id, value, state.open, callbacks.on_toggle.clone());
    with_color_popover(
        trigger.into_any_element(),
        state,
        presets,
        allow_auto,
        placement,
        hex_focused,
        hex_field_callbacks,
        callbacks,
    )
}

/// [`color_picker_field`] whose trigger is the colour itself: a square swatch
/// of `edge` px, for an identity header where the colour stands for the
/// object (the Inspector's track colour). `glyph` is drawn on the swatch in
/// the colour's own contrast tone — what kind of object it is. Same popover,
/// same state.
#[allow(clippy::too_many_arguments)]
pub fn color_picker_swatch_field(
    id: impl Into<gpui::ElementId>,
    edge: f32,
    glyph: Option<&'static str>,
    state: &ColorPickerState,
    presets: &[Rgba],
    placement: ColorPickerPlacement,
    hex_focused: bool,
    hex_field_callbacks: TextInputCallbacks,
    callbacks: ColorPickerCallbacks,
) -> impl IntoElement {
    let value = state.value();
    let color = value
        .color
        .unwrap_or_else(|| color::auto_color_for_index(0));
    let on_toggle = callbacks.on_toggle.clone();
    let hover = Colors::composite(color, Colors::state_hover());
    let trigger = div()
        .id(id.into())
        .w(px(edge))
        .h(px(edge))
        .flex_shrink_0()
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(if state.open {
            Colors::border_focus()
        } else {
            Colors::border_normal()
        })
        .bg(color)
        .flex()
        .items_center()
        .justify_center()
        .children(glyph.map(|glyph| {
            svg()
                .path(glyph)
                .w(px(edge * SWATCH_GLYPH_RATIO))
                .h(px(edge * SWATCH_GLYPH_RATIO))
                .text_color(Colors::on_color(color))
        }))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .tooltip(crate::components::controls::fb_tooltip(format!(
            "Track colour {}",
            rgba_to_hex(color)
        )))
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            on_toggle(window, cx);
            window.prevent_default();
        });
    with_color_popover(
        trigger.into_any_element(),
        state,
        presets,
        false,
        placement,
        hex_focused,
        hex_field_callbacks,
        callbacks,
    )
}

/// A swatch glyph's edge as a share of the swatch's.
pub const SWATCH_GLYPH_RATIO: f32 = 0.55;

#[allow(clippy::too_many_arguments)]
fn with_color_popover(
    trigger: gpui::AnyElement,
    state: &ColorPickerState,
    presets: &[Rgba],
    allow_auto: bool,
    placement: ColorPickerPlacement,
    hex_focused: bool,
    hex_field_callbacks: TextInputCallbacks,
    callbacks: ColorPickerCallbacks,
) -> impl IntoElement {
    div()
        .relative()
        .child(trigger)
        .when(state.open, move |root| {
            if crate::ui_debug_enabled() {
                eprintln!("[ui-popup] render kind=color-picker placement={placement:?} z=overlay");
            }
            let popover = color_picker_popover(
                state,
                presets,
                allow_auto,
                placement,
                hex_focused,
                hex_field_callbacks,
                &callbacks,
            );
            root.child(deferred(popover).with_priority(COLOR_POPOVER_PRIORITY))
        })
}

/// Convenience: the track palette as runtime colors, for callers that don't
/// maintain their own preset list.
///
/// Reads the *active theme*'s `trackColors` rather than the bundled hex table,
/// so a user theme that ships its own palette gets its own picker presets
/// instead of the built-in ones.
pub fn default_presets() -> Vec<Rgba> {
    (0..crate::theme::Colors::TRACK_COLORS.len())
        .map(crate::theme::Colors::track_color_for_index)
        .collect()
}

/// Diagnostic helper used by integrations to log the anchor bounds of a picker
/// (Part I of the spec). Cheap no-op unless the debug flag is set.
pub fn log_anchor(label: &str, bounds: gpui::Bounds<gpui::Pixels>) {
    if color::color_picker_debug_enabled() {
        let x: f32 = bounds.origin.x.into();
        let y: f32 = bounds.origin.y.into();
        let w: f32 = bounds.size.width.into();
        let h: f32 = bounds.size.height.into();
        color_picker_debug(&format!(
            "anchor {label} bounds=({x:.0},{y:.0},{w:.0},{h:.0})"
        ));
    }
}
