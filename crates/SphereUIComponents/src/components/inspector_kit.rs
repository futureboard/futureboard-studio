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
    div, px, svg, App, AppContext, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::theme::{radius, size, space, typography, Colors};

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
        .text_size(px(typography::UI_XS))
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
        .h(px(PROP_CONTROL_H))
        .px(px(space::SNUG))
        .gap(px(space::TIGHT))
        .rounded(px(radius::CONTROL_SM))
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
                .text_size(px(typography::UI_XS))
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

// ── Property sheet (the Inspector) ───────────────────────────────────────────
//
// The Inspector is a property sheet: what is selected sits in a bar at the
// top, and its properties follow as dense label | value rows, grouped under
// thin section bands that fold. Every value control sits in the same right
// column at the same height, so a long track reads as one table rather than
// a stack of cards.
//
// ```txt
// ┌───────────────────────────────────────────┐
// │▌3  Instrument Track 3          M S I R    │  ← object bar
// ├───────────────────────────────────────────┤
// │ ▾ Track                                   │  ← section band
// │   Volume        ────────●──     −3.0 dB   │
// │   Pan           ─────●─────        L 20   │
// │ ▸ Routing             All inputs · Master │  ← folded, with summary
// ├═══════════════ splitter ══════════════════┤
// │▌  Chorus Sketch                     ♪     │  ← the selected clip's bar
// │ ▾ Clip                                    │
// │   Start                       [ 16.00 bt ]│
// └───────────────────────────────────────────┘
// ```

/// Height of a section band.
pub const FOLD_HEADER_H: f32 = size::DEFAULT;
/// Height of a property row. Controls inside are `size::DENSE`.
pub const PROP_ROW_H: f32 = size::DEFAULT;
/// Height of a value control in a property row.
pub const PROP_CONTROL_H: f32 = size::DENSE;
/// Share of a row's width the value column takes.
pub const PROP_VALUE_SHARE: f32 = 0.56;
/// Label column of a form row inside a fold section, for the rows that still
/// size their label.
pub const FOLD_LABEL_COL: f32 = LABEL_COL + space::SNUG;
/// Chevron glyph in a fold header.
pub const FOLD_CHEVRON: f32 = 10.0;
/// Edge of the identity block's colour swatch.
pub const IDENTITY_SWATCH: f32 = size::COMFORTABLE;
/// Height of the bar naming the inspected object.
pub const OBJECT_BAR_H: f32 = size::PROMINENT;
/// Edge of the object bar's colour swatch.
pub const OBJECT_SWATCH: f32 = size::DENSE;

/// The Inspector's scroll body: the pane's only vertical scroller and its
/// only horizontal clip. Unlike [`ins_body`] it adds no padding or gap — the
/// blocks inside own their insets and separate by their bands.
pub fn ins_flat_body(id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .overflow_x_hidden()
        .flex()
        .flex_col()
}

/// The block at the top of the Inspector that says what is selected.
pub fn ins_identity_block() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .p(px(space::LOOSE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
}

/// The bar naming what a pane inspects. It draws the colour strip; the caller
/// adds the rest (a swatch, a number, the name, state toggles, a kind glyph).
///
/// The colour is a solid strip down the leading edge — the object's own hue,
/// the one its track header or clip wears — so the two panes of a split
/// Inspector say whose they are before a word is read.
pub fn ins_object_bar(accent: gpui::Rgba) -> gpui::Div {
    div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .h(px(OBJECT_BAR_H))
        .flex_shrink_0()
        .pl(px(space::LOOSE))
        .pr(px(space::BASE))
        .bg(Colors::surface_panel_raised())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(space::TIGHT))
                .bg(accent),
        )
}

/// A track's number in its [`ins_object_bar`], as its header shows it.
pub fn ins_object_number(number: usize) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .font_features(tabular_features())
        .text_color(Colors::text_muted())
        .child(number.to_string())
}

/// Tabular figures, so a track number keeps its width as it changes.
fn tabular_features() -> gpui::FontFeatures {
    static FEATURES: std::sync::OnceLock<gpui::FontFeatures> = std::sync::OnceLock::new();
    FEATURES
        .get_or_init(|| gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".to_string(), 1)])))
        .clone()
}

/// The name in an [`ins_object_bar`].
pub fn ins_object_name(name: impl Into<String>) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .truncate()
        .text_size(px(typography::UI_SM))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_primary())
        .child(name.into())
}

/// One of a track's switches (M / S / I / R) in its object bar: a square
/// carrying the letter, lit in the switch's own semantic hue — the colours the
/// mixer and the track header use — and outlined in it, so a lit switch is
/// told apart by fill and edge rather than by hue alone.
pub fn ins_bar_toggle(
    id: impl Into<gpui::ElementId>,
    letter: &'static str,
    name: impl Into<gpui::SharedString>,
    on: bool,
    semantic: gpui::Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let fill = if on {
        semantic
    } else {
        Colors::surface_input()
    };
    let hover = Colors::composite(fill, Colors::state_hover());
    let pressed = Colors::composite(fill, Colors::state_recessed());
    let focus = Colors::state_focus_ring();
    let name = name.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(name.clone())
        .aria_toggled(if on {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(crate::theme::elevation::focus_ring(focus)))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size::DENSE))
        .rounded(px(radius::CONTROL_SM))
        .bg(fill)
        .border(px(1.0))
        .border_color(if on {
            semantic
        } else {
            Colors::border_normal()
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
        .active(move |style| style.bg(pressed))
        .tooltip(crate::components::controls::fb_tooltip(name))
        .on_click(on_click)
        .child(letter)
}

/// One fold section: a band that opens and closes it, and its rows.
///
/// `summary` is shown on the band only while the section is folded, so the
/// value the rows would have shown is still readable at a glance.
#[allow(clippy::too_many_arguments)]
pub fn ins_fold(
    id: impl Into<gpui::ElementId>,
    icon: Option<&'static str>,
    title: impl Into<String>,
    summary: impl Into<String>,
    open: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    body: impl IntoElement,
) -> gpui::AnyElement {
    let summary = summary.into();
    let band = Colors::surface_panel_alt();
    let hover = Colors::composite(band, Colors::state_hover());
    let pressed = Colors::composite(band, Colors::state_recessed());
    let header = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(FOLD_HEADER_H))
        .flex_shrink_0()
        .px(px(space::BASE))
        .bg(band)
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(pressed))
        .on_click(move |_, window, cx| on_toggle(window, cx))
        .child(
            svg()
                .path(if open {
                    crate::assets::ICON_CHEVRON_DOWN_PATH
                } else {
                    crate::assets::ICON_CHEVRON_RIGHT_PATH
                })
                .w(px(FOLD_CHEVRON))
                .h(px(FOLD_CHEVRON))
                .flex_shrink_0()
                .text_color(Colors::text_muted()),
        )
        .children(icon.map(|icon| {
            svg()
                .path(icon)
                .w(px(ICON - 1.0))
                .h(px(ICON - 1.0))
                .flex_shrink_0()
                .text_color(Colors::text_muted())
        }))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child(title.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .justify_end()
                .when(!open && !summary.is_empty(), |slot| {
                    slot.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(typography::DENSE_LABEL))
                            .text_color(Colors::text_muted())
                            .child(summary),
                    )
                }),
        );
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .child(header)
        .when(open, |section| {
            section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .px(px(space::LOOSE))
                    .pt(px(space::TIGHT))
                    .pb(px(space::BASE))
                    .child(body),
            )
        })
        .into_any_element()
}

/// Label, then the value control in the right column — the row shape of
/// every section.
///
/// The value column is a fixed share of the row, so every select, field and
/// checkbox down the pane starts on the same edge; the label takes what is
/// left and truncates rather than wrapping into it.
pub fn ins_form_row(label: impl Into<String>, child: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(PROP_ROW_H))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(label.into()),
        )
        .child(
            div()
                .w(gpui::relative(PROP_VALUE_SHARE))
                .flex_shrink_0()
                .min_w_0()
                .child(child),
        )
}

/// A row whose control takes everything after a short label — a slider with
/// its readout, which needs the width more than the label needs the room.
pub fn ins_wide_row(label: impl Into<String>, child: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(PROP_ROW_H))
        .child(
            div()
                .w(px(LABEL_COL_MIN + space::BASE))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(child))
}

/// A checkbox in the value column: the box alone, at the column's end, the
/// row's label saying what it is.
pub fn ins_check(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    checked: bool,
    enabled: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let fill = if checked {
        Colors::accent_primary()
    } else {
        Colors::surface_input()
    };
    let hover = Colors::composite(fill, Colors::state_hover());
    let focus = Colors::state_focus_ring();
    div().flex().flex_row().justify_end().child(
        div()
            .id(id)
            .role(gpui::Role::CheckBox)
            .aria_label(label.into())
            .aria_disabled(!enabled)
            .aria_toggled(if checked {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .flex()
            .items_center()
            .justify_center()
            .size(px(PROP_CONTROL_H - space::TIGHT))
            .rounded(px(radius::CONTROL_SM))
            .bg(fill)
            .border(px(1.0))
            .border_color(if checked {
                Colors::accent_primary()
            } else {
                Colors::border_strong()
            })
            .when(checked, |b| {
                b.child(
                    svg()
                        .path(crate::assets::ICON_CHECK_PATH)
                        .size(px(10.0))
                        .text_color(Colors::on_accent()),
                )
            })
            .when(!enabled, |b| {
                b.opacity(crate::theme::state::DISABLED_CONTENT)
            })
            .when(enabled, |b| {
                b.focusable()
                    .tab_stop(true)
                    .focus_visible(move |style| {
                        style.shadow(crate::theme::elevation::focus_ring(focus))
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |style| style.bg(hover))
                    .on_click(on_toggle)
            }),
    )
}

/// The Inspector's two panes, the track over the selected clip, with a bar
/// between them that drags to share the height.
///
/// Where the bar sits is view state — how the user likes the dock — kept per
/// window under `id`, so it holds as the selection moves from clip to clip.
#[derive(IntoElement)]
pub struct InsSplit {
    pub id: gpui::ElementId,
    pub top: gpui::AnyElement,
    pub bottom: gpui::AnyElement,
}

/// Share of the height the top pane starts with.
pub const SPLIT_DEFAULT: f32 = 0.5;
/// Neither pane goes below this share.
pub const SPLIT_MIN: f32 = 0.18;
/// Height of the bar between the panes.
pub const SPLIT_BAR_H: f32 = space::BASE;

/// Where the split's bar sits, as the top pane's share of the height.
pub struct InsSplitState {
    pub top_share: f32,
}

/// Drag payload of the split's bar.
#[derive(Clone)]
pub struct InsSplitDrag(gpui::ElementId);

impl gpui::Render for InsSplitDrag {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The top pane's share for a pointer at `y` within a split `height` tall,
/// held clear of either end.
pub fn split_share(y: f32, height: f32) -> f32 {
    if !(height > 0.0) || !y.is_finite() {
        return SPLIT_DEFAULT;
    }
    (y / height).clamp(SPLIT_MIN, 1.0 - SPLIT_MIN)
}

impl gpui::RenderOnce for InsSplit {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| InsSplitState {
            top_share: SPLIT_DEFAULT,
        });
        let share = state.read(cx).top_share;
        let drag_id = self.id.clone();
        let move_id = self.id.clone();
        let move_state = state.clone();
        let reset_state = state.clone();
        let grip = Colors::border_strong();
        let bar_hover = Colors::composite(Colors::surface_panel_alt(), Colors::state_hover());
        div()
            .id(self.id.clone())
            .size_full()
            .flex()
            .flex_col()
            .on_drag_move::<InsSplitDrag>(move |event, _window, cx| {
                if event.drag(cx).0 != move_id {
                    return;
                }
                let top: f32 = event.bounds.origin.y.into();
                let height: f32 = event.bounds.size.height.into();
                let y: f32 = event.event.position.y.into();
                let next = split_share(y - top, height);
                move_state.update(cx, |state, cx| {
                    if (state.top_share - next).abs() > f32::EPSILON {
                        state.top_share = next;
                        cx.notify();
                    }
                });
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .overflow_hidden()
                    .h(gpui::relative(share))
                    .child(self.top),
            )
            .child(
                div()
                    .id("inspector-split-bar")
                    .flex_shrink_0()
                    .h(px(SPLIT_BAR_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(Colors::surface_panel_alt())
                    .border_t(px(1.0))
                    .border_b(px(1.0))
                    .border_color(Colors::border_subtle())
                    .cursor(gpui::CursorStyle::ResizeUpDown)
                    .hover(move |style| style.bg(bar_hover))
                    .tooltip(crate::components::controls::fb_tooltip(
                        "Drag to share the height. Double-click to even it.",
                    ))
                    .on_drag(InsSplitDrag(drag_id), |drag, _, _, cx| {
                        cx.new(|_| drag.clone())
                    })
                    .on_click(move |event, _, cx| {
                        if event.click_count() == 2 {
                            reset_state.update(cx, |state, cx| {
                                state.top_share = SPLIT_DEFAULT;
                                cx.notify();
                            });
                        }
                    })
                    .child(
                        div()
                            .w(px(size::COMFORTABLE))
                            .h(px(space::HAIR))
                            .rounded(px(radius::PILL))
                            .bg(grip),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.bottom),
            )
    }
}

/// A wide slider with its name and value on the line above it, and an
/// optional scale under it.
pub fn ins_slider_field(
    label: impl Into<String>,
    value: impl IntoElement,
    slider: impl IntoElement,
    scale: Option<SliderScale>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::UI_SM))
                        .text_color(Colors::text_muted())
                        .child(label.into()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::SNUG))
                        .min_w_0()
                        .text_size(px(typography::UI_SM))
                        .text_color(Colors::text_primary())
                        .child(value),
                ),
        )
        .child(slider)
        .when_some(scale, |field, scale| field.child(slider_scale(scale)))
}

/// Ticks printed under a slider: both ends, and an optional mark between
/// them at `mark.0` (0..1 of the rail).
#[derive(Clone, Copy)]
pub struct SliderScale {
    pub left: &'static str,
    pub mark: Option<(f32, &'static str)>,
    pub right: &'static str,
}

/// Width of the box a scale mark is centred in.
const SCALE_MARK_W: f32 = 24.0;

fn slider_scale(scale: SliderScale) -> impl IntoElement {
    let text = |label: &'static str| {
        div()
            .text_size(px(typography::DENSE_LABEL))
            .text_color(Colors::text_faint())
            .child(label)
    };
    div()
        .relative()
        .h(px(space::LOOSE))
        .child(div().absolute().left_0().top_0().child(text(scale.left)))
        .children(scale.mark.map(|(at, label)| {
            div()
                .absolute()
                .top_0()
                .left(gpui::relative(at.clamp(0.0, 1.0)))
                .ml(px(-SCALE_MARK_W / 2.0))
                .w(px(SCALE_MARK_W))
                .flex()
                .justify_center()
                .child(text(label))
        }))
        .child(div().absolute().right_0().top_0().child(text(scale.right)))
}

/// One of the four track switches (M / S / I / R), as an equal-width tile
/// carrying its letter and its word.
///
/// Lit in the switch's own semantic hue — the same one the mixer and the
/// track header use — and bordered in it too, so a lit tile is told apart by
/// fill and edge rather than by hue alone.
pub fn ins_state_tile(
    id: impl Into<gpui::ElementId>,
    letter: &'static str,
    word: impl Into<String>,
    on: bool,
    semantic: gpui::Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let fill = if on {
        semantic
    } else {
        Colors::surface_input()
    };
    let hover = Colors::composite(fill, Colors::state_hover());
    let pressed = Colors::composite(fill, Colors::state_recessed());
    div()
        .id(id)
        .flex()
        .flex_row()
        .flex_1()
        .min_w_0()
        .items_center()
        .justify_center()
        .gap(px(space::SNUG))
        .h(px(size::COMFORTABLE))
        .rounded(px(radius::CONTROL))
        .bg(fill)
        .border(px(1.0))
        .border_color(if on {
            semantic
        } else {
            Colors::border_normal()
        })
        .text_color(if on {
            Colors::on_color(semantic)
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(pressed))
        .on_click(on_click)
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::BOLD)
                .child(letter),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(word.into()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dock is user-resizable. A label column that cannot shrink pushes the
    /// control past the panel edge, which is the failure this floor exists to
    /// prevent — so the two must stay ordered.
    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "documents the label-column constants as a named test"
    )]
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

    /// Dragging the split bar never lets either pane close, and a split with no
    /// height yet (first layout) falls back to an even share.
    #[test]
    fn the_split_keeps_both_panes_open() {
        assert_eq!(split_share(250.0, 500.0), 0.5);
        assert_eq!(split_share(0.0, 500.0), SPLIT_MIN);
        assert_eq!(split_share(-40.0, 500.0), SPLIT_MIN);
        assert_eq!(split_share(500.0, 500.0), 1.0 - SPLIT_MIN);
        assert_eq!(split_share(100.0, 0.0), SPLIT_DEFAULT);
        assert_eq!(split_share(f32::NAN, 500.0), SPLIT_DEFAULT);
    }

    /// Density contract: rows and their controls come off the shared ladder,
    /// and a row never claims more height than the control it wraps.
    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "documents the density-ladder constants as a named test"
    )]
    fn row_stays_denser_than_the_control_it_holds() {
        assert!(ROW_MIN_HEIGHT <= size::COMFORTABLE);
        assert!(ICON < ICON_HEADER);
        assert!(SECTION_PAD >= ROW_GAP);
    }
}
