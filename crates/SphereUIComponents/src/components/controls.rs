//! Shared Studio control primitives.
//!
//! Everything here is built from the [`crate::theme`] token modules — `radius`,
//! `space`, `size`, `state`, `motion`, `elevation` — never from local literals.
//! Three mechanics are shared by every control and are worth stating once:
//!
//! * **State layers, not border swaps.** Hover composites a translucent layer
//!   over the control's *rest* fill via [`Colors::composite`]; the border does
//!   not move. GPUI gives a div exactly one background, so `.hover(|s| s.bg(x))`
//!   would otherwise replace the fill rather than lift it.
//! * **Pressed goes darker.** On a dark ground `state.recessed` reads as
//!   physical depression with no bevel; on an accent fill we drop to
//!   `accent.pressed` instead.
//! * **Focus is a ring, not a recolor.** A 1 px border color change is
//!   indistinguishable from hover, so keyboard focus paints a zero-blur spread
//!   shadow outside the bounds, which also keeps layout stable.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, AppContext, InteractiveElement, IntoElement, ParentElement, Rgba, Role,
    StatefulInteractiveElement, Styled, Toggled, Window,
};

use crate::assets;
use crate::theme::{elevation, radius, size, space, state, typography, Colors};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbButtonKind {
    Default,
    Primary,
    /// No rest fill and no border until hovered. For dense toolbars where a row
    /// of boxed buttons would read as noise.
    Ghost,
    /// Destructive confirmation. Never used for anything recoverable.
    Danger,
}

/// Position of a segment inside a flush group. Only the group's outer corners
/// are rounded — a shared edge between two adjacent segments must stay square,
/// or the two arcs leave a notch of background between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbSegment {
    Only,
    First,
    Middle,
    Last,
}

/// Latched DAW track state. Each carries its own hue *and* its own border *and*
/// its own glyph, because a state encoded on hue alone is unreadable at 16 px
/// and invisible to a colour-blind user. `accent.primary` is deliberately not a
/// member: the accent already marks selection, focus and playback, so reusing
/// it here would make "is anything soloed?" unanswerable across a 40-track
/// arrangement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbLatch {
    Mute,
    Solo,
    Arm,
    Monitor,
    Automation,
}

impl FbLatch {
    pub fn color(self) -> Rgba {
        match self {
            FbLatch::Mute => Colors::state_mute(),
            FbLatch::Solo => Colors::state_solo(),
            FbLatch::Arm => Colors::state_arm(),
            FbLatch::Monitor => Colors::state_monitor(),
            FbLatch::Automation => Colors::state_automation(),
        }
    }
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

pub fn fb_section_label(label: &'static str) -> impl IntoElement {
    div()
        .h(px(14.0))
        .flex()
        .items_center()
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(label)
}

pub fn fb_field_label(label: impl Into<String>) -> impl IntoElement {
    let label = label.into();
    div()
        .w(px(86.0))
        .flex_shrink_0()
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_muted())
        .child(label)
}

pub fn fb_form_row(label: impl Into<String>, child: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(size::PROMINENT))
        .child(fb_field_label(label))
        .child(div().flex_1().min_w_0().child(child))
}

/// Compact section header used inside the Inspector — an uppercase title and a
/// hairline rule, so a long section list stays scannable.
pub fn fb_section_header(label: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .h(px(18.0))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::text_faint())
                .child(label.into()),
        )
        .child(div().flex_1().h(px(1.0)).bg(Colors::border_subtle()))
}

/// Same as [`fb_section_header`] but appends a keyboard shortcut hint pill
/// between the title and the hairline rule.
///
/// ```text
/// SECTION TITLE  ⌘K  ─────────────────
/// ```
///
/// Pass the display string exactly as it should appear, e.g. `"Ctrl+1"` or
/// `"⌘K"`. Use [`fb_shortcut_hint`] directly when you only need the pill.
pub fn fb_section_header_shortcut(
    label: impl Into<String>,
    shortcut: impl Into<String>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .h(px(18.0))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::text_faint())
                .child(label.into()),
        )
        .child(fb_shortcut_hint(shortcut))
        .child(div().flex_1().h(px(1.0)).bg(Colors::border_subtle()))
}

/// Keyboard shortcut hint pill — a `<kbd>`-style chip that sits after a label
/// or section header to surface the bound accelerator at a glance.
///
/// Styled to read quiet at rest: low-alpha background, `border_subtle` outline,
/// `text_faint` foreground. Monospace text so key combinations stay compact and
/// aligned across rows.
///
/// Use inside [`fb_section_header_shortcut`] for a section with a toggle
/// shortcut, or drop standalone into any flex row where you want to annotate a
/// control with its key binding.
pub fn fb_shortcut_hint(text: impl Into<String>) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .h(px(size::MICRO))
        .px(px(space::TIGHT))
        .rounded(px(radius::MICRO))
        .bg(Colors::with_alpha(Colors::text_faint(), 0.07))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_faint())
        .child(text.into())
}

/// Small color chip. This is the visual swatch only; an interactive caller
/// wraps it in its own clickable container.
pub fn fb_color_swatch(color: gpui::Rgba, size: f32) -> impl IntoElement {
    div()
        .w(px(size))
        .h(px(size))
        // A swatch is an identity object, not a control: fully round below the
        // point where a 4 px radius would eat most of the colour.
        .rounded(px(if size <= 18.0 {
            radius::PILL
        } else {
            radius::CONTROL
        }))
        .border(px(1.0))
        .border_color(Colors::border_strong())
        .bg(color)
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

struct ButtonPaint {
    rest: Rgba,
    hover: Rgba,
    pressed: Rgba,
    border: Rgba,
    text: Rgba,
    bordered: bool,
}

fn button_paint(kind: FbButtonKind, enabled: bool) -> ButtonPaint {
    match kind {
        FbButtonKind::Primary => {
            let rest = Colors::accent_primary();
            ButtonPaint {
                rest,
                hover: Colors::accent_primary_hover(),
                pressed: Colors::accent_pressed(),
                border: Colors::with_alpha(rest, 0.0),
                // Near-black on cyan, not white on cyan. White on this accent
                // measures ~3.2:1; the inverse token measures ~9:1.
                text: Colors::text_inverse(),
                bordered: false,
            }
        }
        FbButtonKind::Danger => {
            let rest = Colors::accent_danger();
            ButtonPaint {
                rest,
                hover: Colors::composite(rest, Colors::with_alpha(Colors::text_primary(), 0.12)),
                pressed: Colors::composite(rest, Colors::state_recessed()),
                border: Colors::with_alpha(rest, 0.0),
                text: Colors::text_inverse(),
                bordered: false,
            }
        }
        FbButtonKind::Ghost => {
            let rest = Colors::with_alpha(Colors::button_bg(), 0.0);
            let base = Colors::surface_panel();
            ButtonPaint {
                rest,
                hover: Colors::composite(base, Colors::state_hover()),
                pressed: Colors::composite(base, Colors::state_recessed()),
                border: rest,
                text: Colors::text_secondary(),
                bordered: false,
            }
        }
        FbButtonKind::Default => {
            let rest = Colors::button_bg();
            ButtonPaint {
                rest,
                hover: Colors::composite(rest, Colors::state_hover()),
                pressed: Colors::composite(rest, Colors::state_recessed()),
                border: Colors::button_border(),
                text: if enabled {
                    Colors::button_text()
                } else {
                    Colors::text_disabled()
                },
                bordered: true,
            }
        }
    }
}

pub fn fb_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    kind: FbButtonKind,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    let p = button_paint(kind, enabled);
    let prominent = matches!(kind, FbButtonKind::Primary | FbButtonKind::Danger);
    let focus = Colors::state_focus_ring();

    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_disabled(!enabled)
        .when(enabled, |this| {
            this.focusable()
                .tab_stop(true)
                .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        })
        .flex()
        .items_center()
        .justify_center()
        .h(px(size::PROMINENT))
        .min_w(px(if prominent { 96.0 } else { 72.0 }))
        .px(px(space::LOOSE))
        .rounded(px(radius::CONTROL))
        .when(p.bordered, |this| {
            this.border(px(1.0)).border_color(p.border)
        })
        .bg(p.rest)
        .text_size(px(typography::UI_SM))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(p.text)
        .when(!enabled, |this| this.opacity(state::DISABLED_CONTENT + 0.2))
        .when(enabled, |this| {
            this.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(p.hover))
                .active(move |s| s.bg(p.pressed))
                .on_click(on_click)
        })
        .child(label)
}

pub fn fb_stepper_button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let accessible_label = match label {
        "+" => "Increase",
        "-" | "−" => "Decrease",
        label => label,
    };
    let rest = Colors::button_bg();
    let hover = Colors::composite(rest, Colors::state_hover());
    let pressed = Colors::composite(rest, Colors::state_recessed());
    let focus = Colors::state_focus_ring();

    div()
        .id(id)
        .role(Role::Button)
        .aria_label(accessible_label)
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .flex()
        .items_center()
        .justify_center()
        .w(px(size::COMFORTABLE))
        .h(px(size::COMFORTABLE))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::button_border())
        .bg(rest)
        .text_size(px(typography::UI_MD))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_click)
        .child(label)
}

/// Square icon button. `visual` is the drawn size; the clickable area is
/// inflated to [`size::HIT_MIN`] with transparent padding, so the chrome can
/// read tighter and click easier at the same time.
pub fn fb_icon_button(
    id: impl Into<gpui::ElementId>,
    icon_path: &'static str,
    label: impl Into<gpui::SharedString>,
    visual: f32,
    toggled: Option<bool>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = label.into();
    let active = toggled.unwrap_or(false);
    let base = Colors::surface_panel();
    let rest = if active {
        Colors::composite(base, Colors::accent_active())
    } else {
        Colors::with_alpha(base, 0.0)
    };
    let hover = Colors::composite(if active { rest } else { base }, Colors::state_hover());
    let pressed = Colors::composite(if active { rest } else { base }, Colors::state_recessed());
    let tint = if active {
        Colors::accent_primary()
    } else {
        Colors::text_secondary()
    };
    let focus = Colors::state_focus_ring();
    let pad = size::hit_target(visual);
    let glyph = (visual * 0.54).round().max(11.0);

    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .when_some(toggled, |b, t| {
            b.aria_toggled(if t { Toggled::True } else { Toggled::False })
        })
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .flex()
        .items_center()
        .justify_center()
        .w(px(visual + pad * 2.0))
        .h(px(visual + pad * 2.0))
        .rounded(px(if visual >= size::DEFAULT {
            radius::CONTROL
        } else {
            radius::CONTROL_SM
        }))
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_click)
        .child(
            svg()
                .path(icon_path)
                .w(px(glyph))
                .h(px(glyph))
                .text_color(tint),
        )
}

/// Latching DAW toggle — mute, solo, arm, input monitor, automation write.
///
/// Not a switch and not the generic accent. When latched it fills with the
/// semantic hue at `state::ARMED_WASH`, borders it at `state::ARMED_BORDER`,
/// and paints the glyph at full strength: three channels, so the state survives
/// both a 16 px control and a colour-blind reader.
pub fn fb_toggle(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    latch: FbLatch,
    on: bool,
    visual: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    let base = Colors::surface_panel();
    let semantic = latch.color();
    let (rest, border) = if on {
        Colors::latched(base, semantic)
    } else {
        (Colors::button_bg(), Colors::button_border())
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let pressed = Colors::composite(rest, Colors::state_recessed());
    let text = if on { semantic } else { Colors::text_muted() };
    let focus = Colors::state_focus_ring();
    let pad = size::hit_target(visual);

    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(visual + pad * 2.0))
        .h(px(visual + pad * 2.0))
        .px(px(space::TIGHT))
        .rounded(px(if visual >= size::DEFAULT {
            radius::CONTROL
        } else {
            radius::CONTROL_SM
        }))
        .border(px(1.0))
        .border_color(border)
        .bg(rest)
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(if on {
            gpui::FontWeight::BOLD
        } else {
            gpui::FontWeight::MEDIUM
        })
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_click)
        .child(label)
}

/// Tooltip body for an icon-only control.
///
/// The design contract requires text for every icon-only control, and the crate
/// had grown five near-identical private tooltip views to satisfy it. This is
/// the shared one; prefer it over adding a sixth.
pub struct FbTooltip(pub gpui::SharedString);

impl gpui::Render for FbTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(space::BASE))
            .py(px(space::TIGHT))
            .rounded(px(radius::CONTROL))
            .bg(Colors::surface_raised())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .shadow(elevation::shadow(elevation::OVERLAY))
            .text_size(px(typography::UI_XS))
            .text_color(Colors::text_secondary())
            .child(self.0.clone())
    }
}

/// Convenience for `.tooltip(fb_tooltip("…"))`.
pub fn fb_tooltip(
    text: impl Into<gpui::SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |_window, cx| {
        let text = text.clone();
        cx.new(|_| FbTooltip(text)).into()
    }
}

/// Non-interactive identity chip — plugin format, track type, counts, status.
pub fn fb_badge(label: impl Into<String>, tone: Rgba) -> impl IntoElement {
    let label: String = label.into();
    div()
        .flex()
        .items_center()
        .h(px(size::MICRO))
        .px(px(space::SNUG))
        .rounded(px(radius::PILL))
        .bg(Colors::with_alpha(tone, 0.16))
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(tone)
        .child(label)
}

/// Compact DAW-style checkbox row. Clicking anywhere in the row toggles.
pub fn fb_checkbox(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    checked: bool,
    enabled: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    let row_base = Colors::surface_panel();
    let row_hover = Colors::composite(row_base, Colors::state_hover());
    let focus = Colors::state_focus_ring();

    let mut row = div()
        .id(id)
        .role(Role::CheckBox)
        .aria_label(label.clone())
        .aria_disabled(!enabled)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .when(enabled, |this| {
            this.focusable()
                .tab_stop(true)
                .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        })
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(size::DEFAULT))
        .px(px(space::TIGHT))
        .rounded(px(radius::CONTROL))
        .child(
            div()
                .w(px(14.0))
                .h(px(14.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::CONTROL_SM))
                .border(px(1.0))
                .border_color(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::border_strong()
                })
                .bg(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::surface_input()
                })
                .when(checked, |b| {
                    b.child(
                        svg()
                            .path(assets::ICON_CHECK_PATH)
                            .w(px(10.0))
                            .h(px(10.0))
                            .text_color(Colors::on_accent()),
                    )
                }),
        )
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .text_color(if enabled {
                    Colors::text_secondary()
                } else {
                    Colors::text_disabled()
                })
                .child(label),
        );
    if enabled {
        row = row
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(row_hover))
            .on_click(on_toggle);
    } else {
        row = row.opacity(state::DISABLED_CONTENT + 0.2);
    }
    row
}

/// One segment of a flush segmented control.
///
/// Corners are decided by [`FbSegment`], not by the caller: only the group's
/// outer edges round, and the shared edge between two segments stays square so
/// the pair reads as one object instead of two buttons that happen to touch.
pub fn fb_segment(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    active: bool,
    position: FbSegment,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    let track = Colors::surface_input();
    let rest = if active {
        Colors::composite(track, Colors::state_selected())
    } else {
        Colors::with_alpha(track, 0.0)
    };
    let hover = Colors::composite(if active { rest } else { track }, Colors::state_hover());
    let pressed = Colors::composite(if active { rest } else { track }, Colors::state_recessed());
    let focus = Colors::state_focus_ring();

    // The track already carries the group's outer radius; a segment sits one
    // inset inside it, so its own corners are the nested value.
    let r = radius::inner(radius::SURFACE, space::TIGHT);
    let (tl, tr, bl, br) = match position {
        FbSegment::Only => (r, r, r, r),
        FbSegment::First => (r, radius::NONE, r, radius::NONE),
        FbSegment::Middle => (radius::NONE, radius::NONE, radius::NONE, radius::NONE),
        FbSegment::Last => (radius::NONE, r, radius::NONE, r),
    };

    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_toggled(if active {
            Toggled::True
        } else {
            Toggled::False
        })
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .flex()
        .items_center()
        .justify_center()
        .flex_1()
        .h(px(size::DEFAULT))
        .min_w(px(44.0))
        .px(px(space::BASE))
        .rounded_tl(px(tl))
        .rounded_tr(px(tr))
        .rounded_bl(px(bl))
        .rounded_br(px(br))
        .bg(rest)
        .text_size(px(typography::UI_SM))
        .font_weight(if active {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_muted()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_click)
        .child(label)
}

/// Inset track that holds [`fb_segment`] children. Its padding is what makes
/// the nested-radius arithmetic land.
pub fn fb_segmented_track() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .p(px(space::TIGHT))
        .gap(px(0.0))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_input())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
}

/// Standalone segmented button, kept for call sites that lay out their own
/// group. Prefer [`fb_segment`] inside [`fb_segmented_track`] for new work.
pub fn fb_segmented_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    fb_segment(id, label, active, FbSegment::Only, on_click)
}

// ---------------------------------------------------------------------------
// Dock tabs
// ---------------------------------------------------------------------------

/// The two planes a dock tab strip spans.
///
/// A dock tab is not a chip in a toolbar — it is a notch cut out of the panel
/// below it, the way a browser tab is. That only reads if the active tab is
/// filled with the *body's* own plane while the strip around it sits a plane
/// lower, so both colours have to come from the panel that owns the strip
/// rather than be assumed here. A tab filled with anything but `body` looks
/// like a button parked on a shelf.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FbDockPlanes {
    /// The trench the strip itself paints. One plane below `body`.
    pub strip: Rgba,
    /// The panel plane directly under the strip, which the active tab joins.
    pub body: Rgba,
}

/// Strip that holds a run of [`fb_dock_tab`]s.
///
/// Square by contract: a dock tab bar is full-bleed chrome, and a rounded strip
/// exposes a wedge of whatever is behind it at the panel's corner.
///
/// No bottom rule. The two planes already separate themselves — the strip is a
/// whole step darker than the panel under it — and a hairline across that step
/// only redraws a boundary the values had already made, while cutting the
/// active tab off from the surface it is supposed to be part of.
///
/// Tabs are laid out against the **bottom** edge rather than centred, so the
/// active tab's fill runs all the way into the panel's.
///
/// The caller adds the tabs, then any trailing controls — those want
/// `.items_center()` and `.h_full()` of their own, since the strip aligns to
/// the bottom for the tabs' sake.
pub fn fb_dock_tab_strip(planes: FbDockPlanes) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_end()
        .h(px(size::COMFORTABLE))
        .px(px(space::BASE))
        .gap(px(space::HAIR))
        .bg(planes.strip)
}

/// One tab in an [`fb_dock_tab_strip`], shaped like a browser tab.
///
/// The active tab is rounded on top, square on the bottom, and filled with the
/// body plane. Its bottom edge sits on the strip's, and the strip draws no rule
/// there, so the tab's fill runs straight into the panel's — one continuous
/// surface, with the inactive tabs reading as being behind it.
///
/// An inactive tab paints no fill at all. Filling every tab would turn the run
/// into a segmented control, which says "pick one of these values", not "you
/// are looking at this surface".
///
/// No lines anywhere: no outline on any tab, no accent bar on the active one,
/// and no rule under the strip. Each was drawing a boundary the planes had
/// already drawn — the active tab is a whole step lighter than the trench, its
/// label is brighter and semibold, and it is the only one whose fill continues
/// past the strip. Every added stroke pulls the shape back toward reading as a
/// button rather than as part of the panel.
pub fn fb_dock_tab(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    icon_path: Option<&'static str>,
    active: bool,
    planes: FbDockPlanes,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    let r = radius::CONTROL;
    let rest = if active {
        planes.body
    } else {
        Colors::with_alpha(planes.strip, 0.0)
    };
    let hover = Colors::composite(planes.strip, Colors::state_hover());
    let pressed = Colors::composite(planes.strip, Colors::state_recessed());
    let text = if active {
        Colors::tab_text_active()
    } else {
        Colors::tab_text_muted()
    };
    let text_hover = Colors::tab_text();
    let focus = Colors::state_focus_ring();

    div()
        .id(id)
        .role(Role::Tab)
        .aria_label(label.clone())
        .aria_selected(active)
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::DEFAULT))
        .px(px(space::LOOSE))
        .rounded_tl(px(r))
        .rounded_tr(px(r))
        .rounded_bl(px(radius::NONE))
        .rounded_br(px(radius::NONE))
        .bg(rest)
        .text_size(px(typography::UI_XS))
        .font_weight(if active {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::MEDIUM
        })
        // Set on the tab as well as on the icon: the label is a plain string
        // child and inherits its colour from here, not from the svg.
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        .when(!active, |el| {
            el.hover(move |s| s.bg(hover).text_color(text_hover))
                .active(move |s| s.bg(pressed))
        })
        .children(icon_path.map(|path| svg().path(path).w(px(14.0)).h(px(14.0)).text_color(text)))
        .child(label)
        .on_click(on_click)
}

/// Determinate progress bar. The track is a capsule; the fill is square so the
/// leading edge *is* the value — a rounded fill cap makes small percentages
/// unreadable and never reaches either end.
pub fn fb_progress(fraction: f32) -> impl IntoElement {
    let f = fraction.clamp(0.0, 1.0);
    div()
        .w_full()
        .h(px(4.0))
        .rounded(px(radius::PILL))
        .bg(Colors::fader_rail())
        .child(
            div()
                .h_full()
                .w(gpui::relative(f))
                .rounded(px(radius::NONE))
                .bg(Colors::accent_primary()),
        )
}
