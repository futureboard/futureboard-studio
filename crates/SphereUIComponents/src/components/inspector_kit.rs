//! The right dock's shared visual language.
//!
//! Inspect, Solfège, Chords, Lyrics and Edit are five views of one panel, and
//! before this they each grew their own spacing, label widths and text sizes —
//! `10.5` here, `26.0` there, a `66.0` label column in one file and `86.0` in
//! another. Switching tabs read as switching applications.
//!
//! Everything a dock tab needs to lay out a form lives here, and every value it
//! uses comes from the ladders in [`crate::theme`] rather than a literal. The
//! primitives in [`crate::components::controls`] stay the source for anything
//! that is a *control* (buttons, toggles, segments, badges); this module is the
//! layer above them — the surfaces, headers, rows and readouts that arrange
//! those controls into a panel.
//!
//! ## The shape
//!
//! ```txt
//! panel (surface_panel)
//!   header            ← identity: icon, name, type badge
//!   scroll body
//!     section card    ← surface_card + border_subtle + radius::SURFACE
//!       title         ← (icon +) title, inside the plate
//!       row           ← label column | control
//!       row
//!     section card
//! ```
//!
//! **One card level.** A section is the only thing that draws a card; rows
//! inside it separate by alignment and spacing, never by nesting another
//! surface. Nested cards are what make a dense panel read as visual noise.
//!
//! **Accent is scarce.** Cyan marks the active tab, focus, selection, a live
//! slider fill and the one primary action — nothing else. Everything at rest is
//! neutral, which is what lets the accent mean something.

use gpui::prelude::FluentBuilder;
use gpui::{
    App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    Window, div, px, svg,
};

use crate::theme::{Colors, radius, size, space, typography};

// ── Metrics ──────────────────────────────────────────────────────────────────
// Named so a tab never writes a literal, and so a density change is one edit.

/// Gap between section cards in the scroll body.
pub const SECTION_GAP: f32 = space::LOOSE;
/// Padding inside a section card.
pub const SECTION_PAD: f32 = space::LOOSE;
/// Gap between rows inside a section.
pub const ROW_GAP: f32 = space::SNUG;
/// Minimum height of a label/control row. The control inside sets the real
/// height (`size::COMFORTABLE` for inputs); this only keeps a text-only row
/// from collapsing.
pub const ROW_MIN_HEIGHT: f32 = size::DEFAULT;
/// Padding inside a card. Roomier than a plain section: the card is the only
/// frame in the panel now, so it carries the breathing room the removed inner
/// borders used to fake.
pub const CARD_PAD: f32 = space::LOOSE;
/// Corner of a card: a containing surface.
pub const CARD_RADIUS: f32 = radius::SURFACE;
/// Gap between a card's rows.
pub const CARD_ROW_GAP: f32 = space::SNUG;
/// One round latching state button (M / S / I / R).
pub const STATE_BUTTON: f32 = 28.0;

/// Glyph size for a section header or an inline row icon. Sized to sit on the
/// same optical weight as 11–12 px text rather than to compete with it.
pub const ICON: f32 = 13.0;
/// Glyph size for the panel identity header, one step up.
pub const ICON_HEADER: f32 = 15.0;
/// Ideal width of a row's label column.
///
/// Not a hard width: [`ins_row`] lets the column shrink so a narrow dock keeps
/// the control usable instead of pushing it out of the panel.
pub const LABEL_COL: f32 = 72.0;
/// Floor for that column before the label starts truncating.
pub const LABEL_COL_MIN: f32 = 44.0;

// ── Panel scaffolding ────────────────────────────────────────────────────────

/// Scrollable body every dock tab puts its sections in.
///
/// Owns the panel's only vertical scroll and its only horizontal clip: a tab
/// that adds its own scroller ends up with two, and content that overflows
/// sideways is a layout bug rather than something to scroll to.
pub fn ins_body(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .overflow_x_hidden()
        .flex()
        .flex_col()
        .gap(px(SECTION_GAP))
        .p(px(SECTION_PAD))
}

/// Panel identity header: what is being inspected.
///
/// A colour swatch carries the object's own hue (a track's colour, a clip's
/// kind), the name reads first, and the kind sits under it in words — so the
/// accent cyan never has to mean "this is a track".
pub fn ins_identity(
    accent: gpui::Rgba,
    title: impl Into<String>,
    subtitle: impl Into<String>,
) -> gpui::Div {
    let subtitle = subtitle.into();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .flex_shrink_0()
        .min_w_0()
        .child(
            div()
                .w(px(space::TIGHT))
                .h(px(size::COMFORTABLE))
                .flex_shrink_0()
                .rounded(px(radius::PILL))
                .bg(accent),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(title.into()),
                )
                .when(!subtitle.is_empty(), |col| {
                    col.child(
                        div()
                            .truncate()
                            .text_size(px(typography::DENSE_LABEL))
                            .text_color(Colors::text_muted())
                            .child(subtitle),
                    )
                }),
        )
}

/// [`ins_identity`] with an icon and a type badge, for the dock tabs that
/// name their object by kind rather than by track (Solfège, Chords).
pub fn ins_header(
    icon: &'static str,
    accent: gpui::Rgba,
    title: impl Into<String>,
    badge: impl Into<String>,
) -> impl IntoElement {
    let badge = badge.into();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .h(px(size::PROMINENT))
        .flex_shrink_0()
        .child(
            svg()
                .path(icon)
                .w(px(ICON_HEADER))
                .h(px(ICON_HEADER))
                .flex_shrink_0()
                .text_color(Colors::with_alpha(accent, 0.92)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_MD))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(title.into()),
        )
        .when(!badge.is_empty(), |row| row.child(ins_badge(badge, accent)))
}

// ── Sections ─────────────────────────────────────────────────────────────────
//
// Every section is one plate: `surface_card` on the panel, one subtle
// hairline, the surface radius — and its title inside, at the top, in the
// same place on every card. Value and the hairline separate the plate from
// the panel; no rules inside it, no uppercase, no second surface.

/// The plate every section is drawn on.
fn plate() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(CARD_ROW_GAP))
        .px(px(CARD_PAD))
        .pt(px(space::BASE))
        .pb(px(CARD_PAD))
        .rounded(px(CARD_RADIUS))
        .bg(Colors::surface_card())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
}

/// A section's title, at the top of its plate, with an optional glyph.
pub fn ins_section_header(icon: &'static str, title: impl Into<String>) -> impl IntoElement {
    section_title(Some(icon), title.into())
}

fn section_title(icon: Option<&'static str>, title: String) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::DENSE))
        .flex_shrink_0()
        .min_w_0()
        .children(icon.map(|icon| {
            svg()
                .path(icon)
                .w(px(ICON))
                .h(px(ICON))
                .flex_shrink_0()
                .text_color(Colors::text_muted())
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child(title),
        )
}

/// A section card with a leading glyph, for the dock tabs that identify their
/// sections by icon (Solfège, Settings).
pub fn ins_section(
    icon: &'static str,
    title: impl Into<String>,
    child: impl IntoElement,
) -> impl IntoElement {
    plate()
        .child(section_title(Some(icon), title.into()))
        .child(div().flex().flex_col().gap(px(ROW_GAP)).child(child))
}

/// The bare plate, for call sites that build their children up one at a time
/// (`section = section.child(..)`). Pair it with [`ins_section_header`] as the
/// first child.
pub fn ins_section_container() -> gpui::Div {
    plate()
}

/// A section card whose title carries a trailing control (an Add button, a
/// count, a toggle).
pub fn ins_section_with_action(
    icon: &'static str,
    title: impl Into<String>,
    action: impl IntoElement,
    child: impl IntoElement,
) -> impl IntoElement {
    plate()
        .child(section_title(Some(icon), title.into()).child(div().flex_shrink_0().child(action)))
        .child(div().flex().flex_col().gap(px(ROW_GAP)).child(child))
}

// ── The card ─────────────────────────────────────────────────────────────────

/// A section card: its title and its rows.
///
/// ```txt
/// ╭─────────────────────────────────────────╮
/// │ Track                                   │  ← title, inside the plate
/// │ Name      ( Audio Track 1             ) │  ← label | control
/// │ Volume    ──────────●────────  -3.0 dB  │
/// ╰─────────────────────────────────────────╯
/// ```
pub fn ins_card(title: impl Into<String>, body: impl IntoElement) -> impl IntoElement {
    plate()
        .child(section_title(None, title.into()))
        .child(div().flex().flex_col().gap(px(CARD_ROW_GAP)).child(body))
}

/// The panel's control surface: a pill.
///
/// Every control that takes a value — a name field, a routing menu, a hex code —
/// wears this, so a column of them reads as one instrument panel rather than as
/// five widgets that happen to be stacked. Sliders and the state buttons are the
/// deliberate exceptions: their own shape already says what they do.
pub fn ins_pill() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(size::DEFAULT))
        .px(px(space::LOOSE))
        .rounded(px(radius::PILL))
        .bg(Colors::composite(
            Colors::surface_card(),
            Colors::state_recessed(),
        ))
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_primary())
}

/// A round latching button — the M/S/I/R row.
///
/// Round because these four are the only controls in the panel that are pressed
/// rather than set, and because a row of circles is countable at a glance in a
/// way a row of rounded rectangles is not.
pub fn ins_state_button(
    id: gpui::ElementId,
    label: impl Into<String>,
    on: bool,
    semantic: gpui::Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let rest = Colors::composite(Colors::surface_card(), Colors::state_recessed());
    let hover = Colors::composite(if on { semantic } else { rest }, Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(STATE_BUTTON))
        .h(px(STATE_BUTTON))
        .rounded(px(radius::PILL))
        .bg(if on { semantic } else { rest })
        .when(!on, |button| {
            button.border(px(1.0)).border_color(Colors::border_normal())
        })
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(if on {
            Colors::on_color(semantic)
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .on_click(on_click)
        .child(label.into())
}

// ── Rows ─────────────────────────────────────────────────────────────────────

/// Label on the left, control on the right.
///
/// The label column is `LABEL_COL` wide but allowed to shrink to
/// `LABEL_COL_MIN`: the dock can be dragged narrow, and a fixed column there
/// pushes the control off the panel edge. Below the floor the label truncates
/// rather than wrapping into the control.
pub fn ins_row(label: impl Into<String>, child: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(ROW_MIN_HEIGHT))
        .child(
            div()
                .w(px(LABEL_COL))
                .min_w(px(LABEL_COL_MIN))
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(child))
}

/// A row whose value is read-only text — the CONTENTS style of row.
///
/// Unlike [`ins_row`] the label is *elastic*: there is no control to protect, so
/// the label takes the width the value does not need and only truncates when it
/// genuinely cannot fit. Borrowing the fixed column here cut "Automation Lanes"
/// and "Automation Points" to the same "Automatio…", which is a row you cannot
/// read at all.
pub fn ins_kv_row(label: impl Into<String>, value: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(ROW_MIN_HEIGHT))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label.into()),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(160.0))
                .truncate()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(value.into()),
        )
}

/// Read-only value text. Right-aligned and truncating, so a long device or
/// plug-in name shortens instead of widening the panel.
pub fn ins_value(text: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_end()
        .w_full()
        .min_w_0()
        .truncate()
        .text_size(px(typography::UI_SM))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_primary())
        .child(text.into())
}

/// Quieter sibling of [`ins_value`] for units, hints and derived readouts.
pub fn ins_value_muted(text: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_end()
        .w_full()
        .min_w_0()
        .truncate()
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_muted())
        .child(text.into())
}

// ── Small parts ──────────────────────────────────────────────────────────────

/// Tinted pill for a type, format or state name.
pub fn ins_badge(label: impl Into<String>, tone: gpui::Rgba) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .h(px(size::MICRO))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL_SM))
        .bg(Colors::with_alpha(tone, 0.14))
        .border(px(1.0))
        .border_color(Colors::with_alpha(tone, 0.28))
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::with_alpha(tone, 0.95))
        .child(label.into())
}

/// Read-only horizontal level bar: flat track, accent fill, value on the right.
///
/// Square by construction below a few pixels of height — a rounded fill would
/// stop the topmost lit pixel from *being* the value.
pub fn ins_meter(fraction: f32, value: impl Into<String>) -> impl IntoElement {
    let fraction = fraction.clamp(0.0, 1.0);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(4.0))
                .rounded(px(radius::MICRO))
                .bg(Colors::surface_input())
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction))
                        .rounded(px(radius::MICRO))
                        .bg(Colors::accent_primary()),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(value.into()),
        )
}

/// Flat control surface used by select triggers and value fields.
///
/// Rest is `surface_input` + `border_normal`; hover lifts the fill with the
/// state layer and leaves the border alone, so hover and focus stay
/// distinguishable (focus is a ring, never a recoloured border).
pub fn ins_control_surface() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(size::COMFORTABLE))
        .px(px(space::BASE))
        .gap(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .bg(Colors::surface_input())
        .border(px(1.0))
        .border_color(Colors::border_normal())
}

/// Clickable select/combo trigger: current value, then a chevron.
pub fn ins_select(
    id: impl Into<gpui::ElementId>,
    value: impl Into<String>,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = Colors::composite(Colors::surface_input(), Colors::state_hover());
    let pressed = Colors::composite(Colors::surface_input(), Colors::state_recessed());
    let focus = Colors::state_focus_ring();
    ins_control_surface()
        .id(id)
        .when(enabled, |this| {
            this.focusable()
                .tab_stop(true)
                .focus_visible(move |style| {
                    style.shadow(crate::theme::elevation::focus_ring(focus))
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .active(move |s| s.bg(pressed))
                .on_click(on_click)
        })
        .when(!enabled, |this| {
            this.opacity(crate::theme::state::DISABLED_CONTENT + 0.2)
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_SM))
                .text_color(if enabled {
                    Colors::text_primary()
                } else {
                    Colors::text_disabled()
                })
                .child(value.into()),
        )
        .child(
            svg()
                .path(crate::assets::ICON_CHEVRON_DOWN_PATH)
                .w(px(ICON))
                .h(px(ICON))
                .flex_shrink_0()
                .text_color(Colors::text_muted()),
        )
}

/// What a tab shows when there is nothing selected to inspect.
///
/// Deliberately quiet: a large empty state in a narrow dock reads as an error.
pub fn ins_empty(
    icon: &'static str,
    title: impl Into<String>,
    hint: impl Into<String>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(space::SNUG))
        .flex_1()
        // Width-bound so the hint wraps inside the dock instead of running off
        // its right edge — the panel is `INSPECTOR_WIDTH`, not the window.
        .w_full()
        .min_w_0()
        .p(px(space::SECTION))
        .child(
            svg()
                .path(icon)
                .w(px(20.0))
                .h(px(20.0))
                .text_color(Colors::text_faint()),
        )
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(title.into()),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .text_center()
                .child(hint.into()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dock is user-resizable. A label column that cannot shrink pushes the
    /// control past the panel edge, which is the failure this floor exists to
    /// prevent — so the two must stay ordered.
    #[test]
    fn label_column_can_shrink_before_it_truncates() {
        assert!(LABEL_COL_MIN < LABEL_COL);
        assert!(LABEL_COL_MIN > 0.0);
    }

    /// A read-only pair has no control to protect, so its label must be free to
    /// use the width the value leaves — two different labels truncating to the
    /// same string is the failure this exists to prevent.
    #[test]
    fn kv_value_cannot_claim_the_whole_row() {
        // The value's ceiling has to leave a readable label behind it inside
        // `INSPECTOR_WIDTH` minus the section card's padding.
        let card_inner = crate::components::panel::INSPECTOR_WIDTH - 4.0 * SECTION_PAD;
        assert!(160.0 + LABEL_COL_MIN < card_inner);
    }

    /// Density contract: rows and their controls come off the shared ladder,
    /// and a row never claims more height than the control it wraps.
    #[test]
    fn row_stays_denser_than_the_control_it_holds() {
        assert!(ROW_MIN_HEIGHT <= size::COMFORTABLE);
        assert!(ICON < ICON_HEADER);
        assert!(SECTION_PAD >= ROW_GAP);
    }
}
