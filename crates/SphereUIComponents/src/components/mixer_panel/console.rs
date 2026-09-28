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
//! and the track colour appears exactly once, as the fill of the **name plate
//! at the bottom** — the one place the eye already goes to read which channel
//! it is looking at. Selection is then free to mean one thing: a lifted strip
//! surface and a bright rule on the plate.
//!
//! A coloured header at the *top* was tried and taken out again. On paper it
//! labels the head of a column that scrolling would otherwise leave anonymous;
//! on screen the strips are flush, so five channels' headers merge into one
//! unbroken band of saturated colour across the panel — the same wall of stripes
//! the plate-only rule was written to stop, turned on its side.
//!
//! ```txt
//! ┌──────────┐
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
//! │▓▓ 1 Vocal│  name plate — the only colour on the strip
//! └──────────┘
//! ```
//!
//! # Rules the kit enforces
//!
//! * **Colour is meaning.** Track colour: identity, plate only. Blue/amber/red
//!   /green: mute/solo/record/input, matching the console conventions players
//!   already know. Accent cyan: selection and drag targets. Nothing decorative.
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
/// The pan row: the knob and its readout under it.
///
/// Sized to what it holds: the section's 4 px top and bottom, the knob, 2 px,
/// and the readout's line box.
pub(crate) const PAN_H: f32 = 4.0 + PAN_KNOB_SIZE + 2.0 + 12.0 + 4.0;
/// The pan knob's diameter.
pub(crate) const PAN_KNOB_SIZE: f32 = 28.0;

/// The strip's horizontal inset.
///
/// The racks set it: their slots sit 3 px from the strip edge, and they are
/// most of the strip's height, so everything else lines up with them rather
/// than the other way round. The output row used to inset 4 px *and* pad its
/// button another 4, which put its edge and its text each a pixel inside the
/// slots above — not enough to name, enough to look unaligned in a column 88 px
/// wide.
pub(crate) const STRIP_GUTTER: f32 = 3.0;
/// Smallest fader bay that still leaves the cap somewhere to travel.
pub(crate) const FADER_MIN_H: f32 = 86.0;
/// The fader bay's readout row: gain and held peak, side by side.
pub(crate) const READOUT_H: f32 = 15.0;
/// Two rows of channel toggles.
pub(crate) const BUTTONS_H: f32 = 34.0;
/// The coloured name plate.
pub(crate) const PLATE_H: f32 = 24.0;
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
        .px(px(STRIP_GUTTER))
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

// ── Pan ─────────────────────────────────────────────────────────────────────

/// `C`, `L34`, `R100` — the console shorthand, not a signed float. A pan is a
/// side and an amount, and reading "-0.34" forces the eye to decode which side
/// negative means.
pub(crate) fn format_pan(pan: f32) -> String {
    let amount = (pan.abs() * 100.0).round() as i32;
    if amount == 0 {
        "C".to_string()
    } else if pan < 0.0 {
        format!("L{amount}")
    } else {
        format!("R{amount}")
    }
}

// ── Room panner ─────────────────────────────────────────────────────────────

/// The room panner's side: square, filling the pan row's height.
pub(crate) const ROOM_PAD_SIZE: f32 = PAN_H - 8.0;

/// Drag payload for the room panner. Shift drags finely from where the pointer
/// was when Shift went down; a plain drag places the source under the pointer.
#[derive(Clone)]
pub(crate) struct RoomDrag {
    id: String,
    /// `(pointer x, pointer y, position)` when fine dragging began.
    anchor: std::rc::Rc<std::cell::Cell<Option<(f32, f32, solfege_spatialaudio::RoomPosition)>>>,
}

impl gpui::Render for RoomDrag {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        gpui::Empty
    }
}

/// Map a window point inside `bounds` to the room: left wall to right wall on
/// x, front wall at the top.
fn room_at(bounds: gpui::Bounds<gpui::Pixels>, x: f32, y: f32) -> (f32, f32) {
    let left = f32::from(bounds.origin.x);
    let top = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width).max(1.0);
    let h = f32::from(bounds.size.height).max(1.0);
    ((x - left) / w * 2.0 - 1.0, 1.0 - (y - top) / h * 2.0)
}

/// Where a room point is drawn inside `bounds`.
fn pad_point(
    bounds: gpui::Bounds<gpui::Pixels>,
    position: solfege_spatialaudio::RoomPosition,
) -> gpui::Point<gpui::Pixels> {
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    gpui::point(
        bounds.origin.x + px((position.x + 1.0) / 2.0 * w),
        bounds.origin.y + px((1.0 - position.y) / 2.0 * h),
    )
}

fn dot(window: &mut gpui::Window, at: gpui::Point<gpui::Pixels>, radius: f32, color: gpui::Rgba) {
    let r = px(radius);
    window.paint_quad(
        gpui::fill(
            gpui::Bounds {
                origin: gpui::point(at.x - r, at.y - r),
                size: gpui::size(r * 2.0, r * 2.0),
            },
            color,
        )
        .corner_radii(r),
    );
}

/// A readable account of a placement, for the tooltip.
pub(crate) fn describe_room_position(params: &solfege_spatialaudio::SourceParams) -> String {
    let p = params.position;
    if p.wall_radius() < 0.05 {
        return "Centre of the room".to_string();
    }
    let degrees = p.azimuth().to_degrees().round() as i32;
    let mut text = format!(
        "{degrees}°, {}% out",
        (p.wall_radius() * 100.0).round() as i32
    );
    if p.z > 0.01 {
        text.push_str(&format!(", {}% up", (p.z * 100.0).round() as i32));
    }
    text
}

/// The square-room panner: the room from above, the listener at its centre,
/// the layout's speakers on its walls, and the channel as a puck — with its
/// two sides, for a stereo channel, either side of it.
///
/// Pressing places the channel under the pointer and dragging moves it;
/// Shift drags a tenth as far, from where it is. A double-click or Alt-click
/// puts it back front and centre.
pub(crate) fn room_panner(
    id: gpui::SharedString,
    params: solfege_spatialaudio::SourceParams,
    format: solfege_spatialaudio::SpatialFormat,
    base: gpui::Rgba,
    on_change: impl Fn(solfege_spatialaudio::SourceParams, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    use crate::components::fader::is_reset_click;
    use gpui::{AppContext, StatefulInteractiveElement};
    use solfege_spatialaudio::{RoomPosition, SourceParams, SpatialFormat};

    let on_change = std::rc::Rc::new(on_change);
    let bounds_cell: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>> =
        Default::default();
    let drag_id = id.to_string();
    let place = {
        let on_change = on_change.clone();
        move |position: RoomPosition, window: &mut gpui::Window, cx: &mut gpui::App| {
            on_change(
                SourceParams {
                    position: position.clamped(),
                    ..params
                },
                window,
                cx,
            );
        }
    };
    let place_down = place.clone();
    let place_drag = place.clone();
    let on_reset = on_change.clone();
    let down_bounds = bounds_cell.clone();
    let paint_bounds = bounds_cell;

    let speakers: Vec<(RoomPosition, bool)> = match format {
        SpatialFormat::Surround(layout) => layout
            .speakers()
            .iter()
            .filter(|speaker| !speaker.lfe)
            .map(|speaker| {
                (
                    solfege_spatialaudio::speaker_position(
                        speaker.azimuth_deg,
                        speaker.elevation_deg,
                    ),
                    speaker.elevation_deg >= 20.0,
                )
            })
            .collect(),
        _ => Vec::new(),
    };
    let binaural = format == SpatialFormat::Binaural;
    let half_width = params.half_width_radians();
    let sides = (half_width > 1.0e-3).then(|| {
        (
            params.position.rotated(-half_width),
            params.position.rotated(half_width),
        )
    });

    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(PAN_H))
        .px(px(STRIP_GUTTER))
        .border_b(px(1.0))
        .border_color(rule())
        .child(
            div()
                .id(id.clone())
                .relative()
                .size(px(ROOM_PAD_SIZE))
                .rounded(px(crate::theme::radius::MICRO))
                .bg(well(base))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .cursor(gpui::CursorStyle::Crosshair)
                .tooltip(crate::components::controls::fb_tooltip(
                    describe_room_position(&params),
                ))
                .child(
                    gpui::canvas(
                        move |bounds, _window, _cx| {
                            paint_bounds.set(Some(bounds));
                        },
                        move |bounds, _state, window, _cx| {
                            let centre = pad_point(bounds, RoomPosition::CENTRE);
                            let rule = Colors::border_subtle();
                            // Cross through the listener.
                            window.paint_quad(gpui::fill(
                                gpui::Bounds {
                                    origin: gpui::point(bounds.origin.x, centre.y),
                                    size: gpui::size(bounds.size.width, px(1.0)),
                                },
                                rule,
                            ));
                            window.paint_quad(gpui::fill(
                                gpui::Bounds {
                                    origin: gpui::point(centre.x, bounds.origin.y),
                                    size: gpui::size(px(1.0), bounds.size.height),
                                },
                                rule,
                            ));
                            // The layout's speakers on the walls; the top
                            // ring dimmer.
                            for (position, top) in &speakers {
                                let color = if *top {
                                    Colors::with_alpha(Colors::text_muted(), 0.5)
                                } else {
                                    Colors::text_muted()
                                };
                                dot(window, pad_point(bounds, *position), 1.5, color);
                            }
                            // For headphones, the head, facing the front wall.
                            if binaural {
                                dot(window, centre, 3.0, Colors::text_muted());
                                window.paint_quad(gpui::fill(
                                    gpui::Bounds {
                                        origin: gpui::point(centre.x - px(0.5), centre.y - px(5.0)),
                                        size: gpui::size(px(1.0), px(2.0)),
                                    },
                                    Colors::text_muted(),
                                ));
                            }
                            // The channel.
                            if let Some((left, right)) = sides {
                                let side = Colors::with_alpha(Colors::text_primary(), 0.55);
                                dot(window, pad_point(bounds, left), 1.5, side);
                                dot(window, pad_point(bounds, right), 1.5, side);
                            }
                            let puck = pad_point(bounds, params.position);
                            dot(window, puck, 4.0, Colors::surface_base());
                            dot(window, puck, 3.0, Colors::text_primary());
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                )
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    if event.modifiers.alt || event.modifiers.shift || event.click_count > 1 {
                        return;
                    }
                    let Some(bounds) = down_bounds.get() else {
                        return;
                    };
                    let (x, y) = room_at(bounds, event.position.x.into(), event.position.y.into());
                    place_down(RoomPosition::new(x, y, params.position.z), window, cx);
                })
                .on_drag(
                    RoomDrag {
                        id: drag_id.clone(),
                        anchor: Default::default(),
                    },
                    |drag, _offset, _window, cx| cx.new(|_| drag.clone()),
                )
                .on_drag_move::<RoomDrag>(move |event, window, cx| {
                    let drag = event.drag(cx);
                    if drag.id != drag_id {
                        return;
                    }
                    let px_x: f32 = event.event.position.x.into();
                    let px_y: f32 = event.event.position.y.into();
                    let bounds = event.bounds;
                    let position = if event.event.modifiers.shift {
                        let (ax, ay, from) =
                            drag.anchor.get().unwrap_or((px_x, px_y, params.position));
                        drag.anchor.set(Some((ax, ay, from)));
                        let w = f32::from(bounds.size.width).max(1.0) / 2.0;
                        let h = f32::from(bounds.size.height).max(1.0) / 2.0;
                        RoomPosition::new(
                            from.x + (px_x - ax) / w * 0.1,
                            from.y - (px_y - ay) / h * 0.1,
                            from.z,
                        )
                    } else {
                        drag.anchor.set(None);
                        let (x, y) = room_at(bounds, px_x, px_y);
                        RoomPosition::new(x, y, params.position.z)
                    };
                    place_drag(position, window, cx);
                })
                .on_click(move |event, window, cx| {
                    if is_reset_click(event) {
                        on_reset(
                            SourceParams {
                                position: RoomPosition::FRONT,
                                ..params
                            },
                            window,
                            cx,
                        );
                    }
                }),
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

    #[test]
    fn pan_reads_as_a_side_and_an_amount() {
        assert_eq!(format_pan(0.0), "C");
        assert_eq!(format_pan(0.004), "C");
        assert_eq!(format_pan(-0.34), "L34");
        assert_eq!(format_pan(1.0), "R100");
    }

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
