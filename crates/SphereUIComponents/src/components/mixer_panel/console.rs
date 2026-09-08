//! The console vocabulary every mixer strip is built from.
//!
//! # What changed and why
//!
//! The mixer used to identify a channel with its track colour: a 2 px bar
//! across the top of the strip, a 3 px edge down the header, a coloured border
//! when selected. Twenty channels of that is twenty vertical stripes competing
//! with the meters — and the strip is 88 px wide, so the colour was spending
//! width the controls needed. Worse, selection was *also* a coloured border
//! (cyan), so the two meanings collided: a border could be identity or state.
//!
//! This follows Logic's console instead. The strip body is neutral end to end,
//! and the track colour is confined to the **name bands** — never the body, the
//! border, or the controls. Selection is then free to mean one thing: a lifted
//! strip surface and a bright rule against the bands.
//!
//! The colour is stated at both ends. It used to be the bottom plate alone, on
//! the argument that one statement is enough; with the racks in place a strip
//! is tall enough that scrolling a panel leaves the top of every column
//! unidentified, so the header repeats it where the eye enters.
//!
//! ```txt
//! ┌──────────┐
//! │▓▓ 1 Vocal│  header — track colour, where the eye enters
//! │ AUD    ⌄ │  top row — channel type, group expander
//! │ INSERTS +│  rack label
//! │ ▭ EQ     │  slot
//! │ SENDS    │
//! │ ▭ → Verb │
//! │ Main   ⌄ │  I/O
//! │   ( )    │  pan
//! │ 0┤▮   █  │  fader + meter, against a printed dB scale
//! │10┤▮   █  │
//! │20┤▮   █  │
//! │  -3.4    │  value
//! │ M S R I  │
//! ├──────────┤
//! │▓▓ 1 Vocal│  name plate — the same colour, where it leaves
//! └──────────┘
//! ```
//!
//! # Rules the kit enforces
//!
//! * **Colour is meaning.** Track colour: identity, name bands only. Blue/amber
//!   /red/green: mute/solo/record/input, matching the console conventions
//!   players already know. Accent cyan: selection and drag targets. Nothing
//!   decorative.
//! * **A scale is a promise.** The dB numbers beside the meter and the meter's
//!   own fill are positioned by one function (`vu_meter::db_fraction`). A scale
//!   printed against a bar drawn some other way is worse than no scale, because
//!   it is read as fact.
//! * **Flat, not chipped.** Controls are flat fills with no border at rest;
//!   depth comes from the recess of the rack area, not from outlining every
//!   element. Eight bordered chips in an 88 px column read as noise.
//! * **One type ramp.** 10.5 px for values the user reads, 9.5 px for control
//!   labels, 8.5 px uppercase for section names. Nothing else.

use gpui::prelude::FluentBuilder;
use gpui::{div, px, svg, InteractiveElement, IntoElement, ParentElement, Styled};

use crate::assets;
use crate::theme::Colors;

// ── Section metrics ─────────────────────────────────────────────────────────
//
// Every strip stacks the same fixed rows in the same order, so a row of strips
// reads across as well as down: all the pans on one line, all the faders in one
// bay, all the plates on one baseline. The two racks (inserts, sends) are the
// only rows the user can resize, which is why they are the only ones passed in.

/// Channel type + group expander. Logic's "Setting" row sits here; this is the
/// same slot spent on what the model actually has.
pub(crate) const TOP_ROW_H: f32 = 16.0;
/// Rack caption ("INSERTS", "SENDS").
pub(crate) const RACK_LABEL_H: f32 = 14.0;
/// One insert slot.
pub(crate) const SLOT_H: f32 = 17.0;
/// One send slot: a target line over its level bar. Taller than an insert
/// because it carries a control as well as a name.
pub(crate) const SEND_SLOT_H: f32 = 28.0;
/// Output / routing row.
pub(crate) const IO_ROW_H: f32 = 20.0;
/// Pan knob and its readout.
pub(crate) const PAN_H: f32 = 44.0;
/// Smallest fader bay that still leaves the cap somewhere to travel.
pub(crate) const FADER_MIN_H: f32 = 86.0;
/// Two rows of channel toggles.
pub(crate) const BUTTONS_H: f32 = 34.0;
/// The coloured name plate.
pub(crate) const PLATE_H: f32 = 24.0;
/// The coloured name header at the top of the strip.
pub(crate) const HEADER_H: f32 = 20.0;
/// Width of the printed dB scale beside the meter. Two digits and a minus at
/// 8.5 px; anything wider is spending strip on a number nobody reads twice.
pub(crate) const METER_SCALE_W: f32 = 17.0;

/// Text sizes. Three of them, deliberately.
pub(crate) mod type_scale {
    /// Values the user reads at a glance: the dB number, the plate name.
    pub const VALUE: f32 = 10.5;
    /// Control labels: slot names, routing, toggles.
    pub const LABEL: f32 = 9.5;
    /// Section captions and units.
    pub const CAPTION: f32 = 8.5;
}

// ── Surfaces ────────────────────────────────────────────────────────────────

/// The strip body.
///
/// One neutral surface for every channel — no odd/even banding. Banding was
/// there to help the eye track a column across the panel; the name plate does
/// that better, in colour, at the end of the column the eye lands on anyway.
pub(crate) fn strip_surface(selected: bool) -> gpui::Rgba {
    let rest = Colors::mixer_strip_bg();
    if selected {
        Colors::composite(rest, Colors::state_selected())
    } else {
        rest
    }
}

/// A VSTi multi-output child, one step darker so the group reads as nested
/// under its instrument without needing the parent's colour to bracket it.
pub(crate) fn sub_strip_surface(selected: bool) -> gpui::Rgba {
    let rest = Colors::mixer_strip_bg_alt();
    if selected {
        Colors::composite(rest, Colors::state_selected())
    } else {
        rest
    }
}

/// The pinned Master / Control Room strips.
pub(crate) fn pinned_surface() -> gpui::Rgba {
    Colors::master_strip_bg()
}

/// Recessed well a rack's slots sit in. The only depth cue in the strip: a
/// control is a flat fill, and the *area* behind it is what looks sunken.
pub(crate) fn well(base: gpui::Rgba) -> gpui::Rgba {
    Colors::composite(base, Colors::state_recessed())
}

/// The hairline between two sections. Not a border colour — a single quiet rule
/// that reads as a fold in one surface rather than a boundary between two.
pub(crate) fn rule() -> gpui::Rgba {
    Colors::border_subtle()
}

// ── Section captions ────────────────────────────────────────────────────────

/// Optional clickable "+" on a rack caption. `None` renders nothing at all —
/// an inert grey plus on a rack that cannot take another slot is a control that
/// lies about being one.
pub(crate) struct RackPlus {
    pub id: gpui::SharedString,
    pub on_click: std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App)>,
}

/// "INSERTS", "SENDS" — the caption above a rack.
pub(crate) fn rack_label(label: impl Into<String>, plus: Option<RackPlus>) -> impl IntoElement {
    let label = label.into();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .h(px(RACK_LABEL_H))
        .pl(px(5.0))
        .pr(px(3.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(type_scale::CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(label.to_uppercase()),
        )
        .children(plus.map(|RackPlus { id, on_click }| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(14.0))
                .h(px(12.0))
                .rounded(px(crate::theme::radius::MICRO))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::state_hover()))
                .child(
                    svg()
                        .path(assets::ICON_PLUS_PATH)
                        .w(px(9.0))
                        .h(px(9.0))
                        .text_color(Colors::text_muted()),
                )
                .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| on_click(w, cx))
                .occlude()
        }))
}

/// A fact the strip states rather than a control it offers — the Master's
/// channel format, an empty rack's "none". No chrome, so it can never be
/// mistaken for something pressable.
pub(crate) fn caption(text: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .truncate()
        .text_size(px(type_scale::CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(text.into())
}

// ── Slot fills ──────────────────────────────────────────────────────────────

/// How a rack slot reads. The state is in the fill and the text, never in a
/// border: a column of outlined chips is the "chipped" look this replaced.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotTone {
    /// Loaded and processing.
    Active,
    /// Loaded but bypassed — present, not in the signal path.
    Bypassed,
    /// Still loading, or disabled by the host.
    Pending,
    /// Missing plug-in or a load failure.
    Failed,
}

impl SlotTone {
    /// `(fill, text)` for this state on `base`.
    pub(crate) fn colors(self, base: gpui::Rgba) -> (gpui::Rgba, gpui::Rgba) {
        match self {
            // Logic's loaded slot is the *lighter* thing in a dark rack — the
            // plug-in is the content, the rack is the container.
            Self::Active => (
                Colors::composite(base, Colors::state_selected()),
                Colors::text_primary(),
            ),
            Self::Bypassed => (well(base), Colors::text_disabled()),
            Self::Pending => (well(base), Colors::text_muted()),
            Self::Failed => (
                Colors::with_alpha(Colors::status_error(), 0.16),
                Colors::status_error(),
            ),
        }
    }
}

// ── Channel toggles (M / S / R / I, PFL / AFL, Mute / Dim / Mono) ───────────

/// Visual state of a channel toggle.
///
/// `Implied` is neither on nor off: this channel's own flag is clear, but the
/// engine treats it as engaged because a parent decided for it — a VSTi
/// multi-out channel under its instrument's solo. It reads as a wash and a
/// coloured glyph rather than a solid fill, so "sounding because of the parent"
/// never looks like "someone pressed this". The button still toggles this
/// channel's own flag, so it keeps full button affordance.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleState {
    Off,
    Implied,
    On,
}

impl From<bool> for ToggleState {
    fn from(active: bool) -> Self {
        if active {
            ToggleState::On
        } else {
            ToggleState::Off
        }
    }
}

/// A console toggle: flat, unbordered at rest, solid in its own colour when on.
///
/// `semantic` is the meaning's colour — blue mute, amber solo, red record,
/// green input — not the app accent. Those four are close to universal across
/// consoles and DAWs, and a player glancing at a strip reads the colour before
/// the letter.
pub(crate) fn toggle(
    id: gpui::ElementId,
    label: &'static str,
    state: ToggleState,
    semantic: gpui::Rgba,
    base: gpui::Rgba,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let rest = well(base);
    let mut btn = div()
        .id(id)
        .flex()
        .flex_1()
        .min_w(px(0.0))
        .items_center()
        .justify_center()
        .h(px(15.0))
        .rounded(px(crate::theme::radius::MICRO))
        .text_size(px(type_scale::CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .cursor(gpui::CursorStyle::PointingHand)
        .on_mouse_down(gpui::MouseButton::Left, on_click)
        .child(label);

    match state {
        ToggleState::On => {
            btn = btn
                .bg(semantic)
                .text_color(Colors::on_color(semantic))
                .hover(|s| s.bg(Colors::state_hover()));
        }
        ToggleState::Implied => {
            let (fill, _) = Colors::latched(rest, semantic);
            let hover = Colors::composite(fill, Colors::state_hover());
            btn = btn
                .bg(fill)
                .text_color(semantic)
                .hover(move |s| s.bg(hover));
        }
        ToggleState::Off => {
            let hover = Colors::composite(rest, Colors::state_hover());
            btn = btn
                .bg(rest)
                .text_color(Colors::text_muted())
                .hover(move |s| s.bg(hover));
        }
    }
    btn
}

// ── Routing ─────────────────────────────────────────────────────────────────

/// The I/O row: where this channel's signal goes. One full-width button, the
/// destination in it, a chevron saying it opens a menu.
///
/// `leading` is an optional caption printed before the value ("OUT", "SRC") for
/// the pinned strips, which have more than one routing row to tell apart.
pub(crate) fn io_button(
    id: gpui::ElementId,
    leading: Option<String>,
    value: String,
    base: gpui::Rgba,
    on_open: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let rest = well(base);
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(IO_ROW_H))
        .px(px(4.0))
        .child(
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .w_full()
                .min_w(px(0.0))
                .h(px(16.0))
                .px(px(4.0))
                .rounded(px(crate::theme::radius::MICRO))
                .bg(rest)
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .children(leading.map(|text| {
                    div()
                        .flex_none()
                        .text_size(px(type_scale::CAPTION))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_faint())
                        .child(text)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(type_scale::LABEL))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_secondary())
                        .child(value),
                )
                .child(
                    svg()
                        .path(assets::ICON_CHEVRON_DOWN_PATH)
                        .w(px(8.0))
                        .h(px(8.0))
                        .flex_shrink_0()
                        .text_color(Colors::text_faint()),
                )
                .on_mouse_down(gpui::MouseButton::Left, on_open)
                .occlude(),
        )
}

// ── Name plate ──────────────────────────────────────────────────────────────

/// The bottom plate: the channel's number, its name, and the one place its
/// colour appears.
///
/// `number` is `None` for the pinned strips, which are not numbered channels.
/// Selection puts a bright rule along the top of the plate rather than tinting
/// the fill, because the fill is already saying something else — which track
/// this is — and two meanings in one channel is how the old coloured border
/// became unreadable.
pub(crate) fn name_plate(
    fill: gpui::Rgba,
    number: Option<usize>,
    name: impl Into<String>,
    selected: bool,
    // The GPU primitive layer paints the plate fill for scrolling channel
    // strips; when it does, this renders the text over it and nothing else.
    painted_by_gpu: bool,
) -> impl IntoElement {
    let text = Colors::on_color(fill);
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .gap(px(3.0))
        .h(px(PLATE_H))
        .px(px(5.0))
        .when(!painted_by_gpu, |s| s.bg(fill))
        .when(selected, |s| {
            s.border_t(px(2.0)).border_color(Colors::text_primary())
        })
        .children(number.map(|n| {
            div()
                .flex_none()
                .text_size(px(type_scale::CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                // The number is a locator, not a label: it stays legible but
                // never competes with the name beside it.
                .text_color(Colors::with_alpha(text, 0.65))
                .child(format!("{n}"))
        }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(type_scale::VALUE))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(text)
                .child(name.into()),
        )
}

/// The coloured header at the top of the strip.
///
/// The plate at the bottom still carries the name, so the colour is stated
/// twice — once where the eye enters the strip and once where it leaves. That
/// is a deliberate reversal of the rule this file used to hold (see the module
/// header): with the racks in place a strip is tall enough that the bottom
/// plate alone leaves the top of a column unidentified while scrolling.
///
/// It is a band and a name, nothing else. Every control the header could have
/// absorbed already has a row of its own further down, and a header that is
/// also a control is a header you cannot click to select the channel.
pub(crate) fn channel_header(
    fill: gpui::Rgba,
    number: Option<usize>,
    name: impl Into<String>,
    selected: bool,
) -> impl IntoElement {
    let text = Colors::on_color(fill);
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .gap(px(3.0))
        .h(px(HEADER_H))
        .px(px(5.0))
        .bg(fill)
        // Selection reads as a bright rule under the header, matching the one
        // the plate puts above itself: the two ends of a selected strip are
        // bracketed the same way.
        .when(selected, |s| {
            s.border_b(px(2.0)).border_color(Colors::text_primary())
        })
        .children(number.map(|n| {
            div()
                .flex_none()
                .text_size(px(type_scale::CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::with_alpha(text, 0.65))
                .child(format!("{n}"))
        }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(type_scale::VALUE))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(text)
                .child(name.into()),
        )
}

/// The dB scale printed beside the meter.
///
/// Positions come from `vu_meter::db_fraction`, the same function the bar is
/// painted with, so a tick cannot end up somewhere the level it names would not
/// reach. Absolute placement rather than a flex column for the same reason: an
/// evenly distributed stack would space the labels by count instead of by
/// decibel, which looks identical until the floor moves.
///
/// Only every other tick is labelled. Twelve numbers in a 17 px column at
/// 8.5 px type is a grey texture; six numbers and six bare ticks reads as a
/// scale.
pub(crate) fn meter_scale() -> impl IntoElement {
    use crate::components::timeline::vu_meter::{db_fraction, meter_scale_ticks};

    let mut column = div()
        .relative()
        .flex_none()
        .w(px(METER_SCALE_W))
        .h_full()
        .overflow_hidden();

    for db in meter_scale_ticks() {
        let labelled = (db as i32) % 10 == 0;
        // `db_fraction` measures up from the floor; the strip measures down
        // from the top, so a tick's y is the remainder of the height.
        let from_top = 1.0 - db_fraction(db);
        let mut row = div()
            .absolute()
            .left_0()
            .right_0()
            // Half the line box, so the text sits centred on its own tick
            // rather than hanging below it.
            .top(gpui::relative(from_top))
            .mt(px(-4.5))
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(px(2.0));

        if labelled {
            row = row.child(
                div()
                    .text_size(px(type_scale::CAPTION))
                    .text_color(Colors::text_faint())
                    .child(format!("{}", db as i32)),
            );
        }

        column = column.child(
            row.child(
                div()
                    .flex_none()
                    .w(px(if labelled { 3.0 } else { 2.0 }))
                    .h(px(1.0))
                    .bg(Colors::border_subtle()),
            ),
        );
    }
    column
}

/// Plate fill for a strip that is not a track: Master reads as the sum of every
/// colour on the panel, the Control Room as none of them.
pub(crate) fn master_plate_fill() -> gpui::Rgba {
    Colors::track_master()
}

pub(crate) fn monitor_plate_fill() -> gpui::Rgba {
    Colors::surface_raised()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plate picks its text from the fill, so every shipped track colour
    /// has to come out readable — this is the one place in the mixer where the
    /// background is chosen by data rather than by the theme.
    #[test]
    fn every_track_colour_gets_readable_plate_text() {
        for value in Colors::TRACK_COLORS {
            let fill = gpui::Rgba {
                r: ((value >> 16) & 0xFF) as f32 / 255.0,
                g: ((value >> 8) & 0xFF) as f32 / 255.0,
                b: (value & 0xFF) as f32 / 255.0,
                a: 1.0,
            };
            let text = Colors::on_color(fill);
            let contrast = |a: gpui::Rgba, b: gpui::Rgba| {
                let lum = |c: gpui::Rgba| {
                    let ch = |v: f32| {
                        if v <= 0.03928 {
                            v / 12.92
                        } else {
                            ((v + 0.055) / 1.055).powf(2.4)
                        }
                    };
                    0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
                };
                let (l1, l2) = (lum(a), lum(b));
                let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
                (hi + 0.05) / (lo + 0.05)
            };
            let ratio = contrast(fill, text);
            assert!(
                ratio >= 4.5,
                "track colour #{value:06X} plate text contrast is {ratio:.2}:1, below 4.5:1"
            );
        }
    }
}
