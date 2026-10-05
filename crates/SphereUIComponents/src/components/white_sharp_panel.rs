//! The view of WhiteSharp's native editor, laid out like Auto-Tune Pro's
//! Auto mode: the voice and key settings along the top, the pitch
//! correction meter, the correction knobs with Retune Speed at their
//! centre, the keyboard, and Create Vibrato.
//!
//! Pure rendering over [`WhiteSharpEditor`]. The meter and the keyboard's
//! live lights are not drawn here: this panel reserves their places and
//! records their bounds, and the window's overlay paints them every frame
//! (see `white_sharp_window`).

use gpui::{
    canvas, div, px, relative, AnyElement, Context, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, Window,
};

use crate::components::controls::{
    fb_button, fb_checkbox, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::quick_sampler_panel::knob_cell;
use crate::components::white_sharp_meter::{key_frame, BLACK_KEY_HEIGHT, METER_CENTS};
use crate::components::white_sharp_model::{default_value, knob, value};
use crate::components::white_sharp_window::{Menu, NoteList, WhiteSharpEditor};
use crate::theme::{radius, space, state, typography, Colors};

type Cx<'a> = Context<'a, WhiteSharpEditor>;

const KNOB: f32 = 34.0;
const SIDE_KNOB: f32 = 52.0;
/// Retune Speed is the control the whole plug-in turns on.
const HERO_KNOB: f32 = 84.0;
const KNOB_PITCH: f32 = 58.0 + space::TIGHT;
const METER_H: f32 = 52.0;
const KEYBOARD_H: f32 = 96.0;

/// The whole editor.
pub(crate) fn white_sharp_panel(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(w, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .p(px(space::LOOSE))
                .gap(px(space::BASE))
                .child(settings(w, cx))
                .child(meter(w))
                .child(correction(w, cx))
                .child(keyboard(w, cx))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::BASE))
                        .child(vibrato(w, cx))
                        .child(output(w, cx)),
                ),
        )
        .into_any_element()
}

// ── Small pieces ─────────────────────────────────────────────────────────────

fn caption(text: impl Into<String>) -> AnyElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(text.into())
        .into_any_element()
}

fn cluster() -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(space::TIGHT))
}

fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
}

fn segment_position(index: usize, count: usize) -> FbSegment {
    match index {
        0 => FbSegment::First,
        i if i + 1 == count => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

fn track_width(labels: &[&str]) -> f32 {
    let widest = labels
        .iter()
        .map(|label| (label.chars().count() as f32 * 8.5 + 2.0 * space::BASE).max(44.0))
        .fold(0.0f32, f32::max);
    widest * labels.len() as f32 + 2.0 * space::TIGHT + 2.0
}

/// A labelled setting: its caption over its control.
fn setting(title: &'static str, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .child(caption(title))
        .child(control)
        .into_any_element()
}

/// A picker that opens `menu` where it is pressed.
fn picker(
    cx: &mut Cx,
    id: &'static str,
    label: String,
    width: f32,
    menu: fn(f32, f32) -> Menu,
) -> AnyElement {
    let entity = cx.entity().clone();
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .w(px(width))
        .h(px(26.0))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::button_border())
        .bg(Colors::surface_canvas())
        .hover(|style| style.border_color(Colors::border_normal()))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(px(typography::UI_SM))
        .text_color(Colors::text_primary())
        .child(div().flex_1().truncate().child(label))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child("▾"),
        )
        .on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, _: &mut Window, app: &mut gpui::App| {
                let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                let _ = entity.update(app, |this, cx| this.open_menu(menu(x, y), cx));
            },
        )
        .into_any_element()
}

/// The knob for param `id` at `size`, greyed with `why` in place of its
/// value when it has nothing to do.
fn knob_for(
    w: &WhiteSharpEditor,
    cx: &mut Cx,
    id: &'static str,
    size: f32,
    why_not: Option<&'static str>,
) -> AnyElement {
    let Some(spec) = knob(id) else {
        return div().into_any_element();
    };
    let current = value(&w.params, id);
    let (min, max) = spec.knob_range();
    let element_id = format!("whitesharp-{id}");
    if let Some(why) = why_not {
        return div()
            .opacity(state::DISABLED_CONTENT)
            .child(knob_cell(
                spec.label,
                why.to_string(),
                knob_with_default(
                    element_id,
                    spec.to_knob(current),
                    min,
                    max,
                    size,
                    Colors::text_disabled(),
                    spec.to_knob(current),
                    |_, _, _| {},
                ),
            ))
            .into_any_element();
    }
    let on_change = w.knob_cb(cx, spec);
    let default = spec.to_knob(default_value(id));
    let control = if spec.bipolar {
        knob_bipolar(
            element_id,
            spec.to_knob(current),
            min,
            max,
            size,
            Colors::accent_primary(),
            None,
            default,
            on_change,
        )
        .into_any_element()
    } else {
        knob_with_default(
            element_id,
            spec.to_knob(current),
            min,
            max,
            size,
            Colors::accent_primary(),
            default,
            on_change,
        )
        .into_any_element()
    };
    knob_cell(spec.label, spec.readout(current), control)
}

/// A large knob with its name and value under it, wider than a knob cell.
/// Greyed, with `why` for its value, when it has nothing to do.
fn big_knob(
    w: &WhiteSharpEditor,
    cx: &mut Cx,
    id: &'static str,
    size: f32,
    why_not: Option<&'static str>,
) -> AnyElement {
    let Some(spec) = knob(id) else {
        return div().into_any_element();
    };
    let current = value(&w.params, id);
    let (min, max) = spec.knob_range();
    let element_id = format!("whitesharp-{id}");
    let control = match why_not {
        None if spec.bipolar => knob_bipolar(
            element_id,
            spec.to_knob(current),
            min,
            max,
            size,
            Colors::accent_primary(),
            None,
            spec.to_knob(default_value(id)),
            w.knob_cb(cx, spec),
        )
        .into_any_element(),
        None => knob_with_default(
            element_id,
            spec.to_knob(current),
            min,
            max,
            size,
            Colors::accent_primary(),
            spec.to_knob(default_value(id)),
            w.knob_cb(cx, spec),
        )
        .into_any_element(),
        Some(_) => knob_with_default(
            element_id,
            spec.to_knob(current),
            min,
            max,
            size,
            Colors::text_disabled(),
            spec.to_knob(current),
            |_, _, _| {},
        )
        .into_any_element(),
    };
    let text = if size >= HERO_KNOB {
        typography::UI_MD
    } else {
        typography::UI_SM
    };
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::TIGHT))
        .w(px(size + 48.0))
        .opacity(if why_not.is_some() {
            state::DISABLED_CONTENT
        } else {
            1.0
        })
        .child(control)
        .child(
            div()
                .text_size(px(text))
                .text_color(Colors::text_secondary())
                .child(spec.label),
        )
        .child(
            div()
                .text_size(px(text))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(why_not.map_or_else(|| spec.readout(current), str::to_string)),
        )
        .into_any_element()
}

// ── Header ─────────────────────────────────────────────────────────────────

fn header(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let on_b = w.compare.on_b;
    let presets = cluster()
        .child(fb_button(
            "whitesharp-preset-prev",
            "‹",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.step_preset(-1, cx)),
        ))
        .child(picker(
            cx,
            "whitesharp-preset",
            w.preset_label(),
            180.0,
            Menu::Preset,
        ))
        .child(fb_button(
            "whitesharp-preset-next",
            "›",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.step_preset(1, cx)),
        ))
        .child(fb_button(
            "whitesharp-preset-reset",
            "Reset",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.load_preset(0, cx)),
        ));
    let compare = cluster()
        .child(
            fb_segmented_track()
                .child(fb_segment(
                    "whitesharp-a",
                    "A",
                    !on_b,
                    FbSegment::First,
                    w.click_cb(cx, |this, cx| {
                        if this.compare.on_b {
                            this.swap_compare(cx)
                        }
                    }),
                ))
                .child(fb_segment(
                    "whitesharp-b",
                    "B",
                    on_b,
                    FbSegment::Last,
                    w.click_cb(cx, |this, cx| {
                        if !this.compare.on_b {
                            this.swap_compare(cx)
                        }
                    }),
                ))
                .w(px(track_width(&["A", "B"]))),
        )
        .child(fb_button(
            "whitesharp-copy",
            if on_b { "Copy B → A" } else { "Copy A → B" },
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.copy_compare(cx)),
        ));
    let notice = w.notice().map(|text| {
        div()
            .text_size(px(typography::DENSE_CAPTION))
            .text_color(Colors::text_secondary())
            .child(text.to_string())
    });

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .px(px(space::LOOSE))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .child(
                    div()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::text_primary())
                        .child("WhiteSharp"),
                )
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child("Auto pitch correction"),
                ),
        )
        .child(presets)
        .child(compare)
        .children(notice)
        .child(div().flex_1())
        .child(fb_checkbox(
            "whitesharp-power",
            "Power",
            w.params.power,
            true,
            w.click_cb(cx, |this, cx| this.toggle("power", cx)),
        ))
        .into_any_element()
}

// ── Settings ───────────────────────────────────────────────────────────────

/// The voice and the key, and the controls that set up the song rather
/// than the sound: Auto-Tune Pro's top row.
fn settings(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let key = w.params.key;
    let key_picker = cluster()
        .child(fb_button(
            "whitesharp-key-down",
            "‹",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, move |this, cx| this.set_key((key + 11) % 12, cx)),
        ))
        .child(picker(
            cx,
            "whitesharp-key",
            whitesharp::NOTE_NAMES[usize::from(key)].to_string(),
            64.0,
            Menu::Key,
        ))
        .child(fb_button(
            "whitesharp-key-up",
            "›",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, move |this, cx| this.set_key((key + 1) % 12, cx)),
        ));
    let formant = fb_checkbox(
        "whitesharp-formant",
        "Keep formants",
        w.params.formant,
        true,
        w.click_cb(cx, |this, cx| this.toggle("formant", cx)),
    );
    card()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .child(setting(
            "INPUT TYPE",
            picker(
                cx,
                "whitesharp-input",
                w.params.input_type.label().to_string(),
                128.0,
                Menu::Input,
            ),
        ))
        .child(setting("KEY", key_picker))
        .child(setting(
            "SCALE",
            picker(
                cx,
                "whitesharp-scale",
                w.params.scale.label().to_string(),
                150.0,
                Menu::Scale,
            ),
        ))
        .child(div().flex_1())
        .child(setting(
            "FORMANT",
            div().h(px(26.0)).flex().items_center().child(formant),
        ))
        .child(knob_for(w, cx, "throat", KNOB, None))
        .child(knob_for(w, cx, "transpose", KNOB, None))
        .child(knob_for(w, cx, "detune", KNOB, None))
        .child(knob_for(w, cx, "tracking", KNOB, None))
        .into_any_element()
}

// ── Meter ──────────────────────────────────────────────────────────────────

/// The pitch correction meter's place and its scale. Its canvas records the
/// bounds; the window's overlay paints the meter and the readout.
fn meter(w: &WhiteSharpEditor) -> AnyElement {
    let bounds_out = w.meter_bounds.clone();
    let ticks = [-100.0f32, -50.0, 0.0, 50.0, 100.0];
    // The meter draws its scale inset by ten pixels each side.
    let scale = div()
        .relative()
        .h(px(14.0))
        .mx(px(10.0))
        .children(ticks.map(|cents| {
            div()
                .absolute()
                .left(relative(0.5 + cents / METER_CENTS * 0.5))
                .ml(px(-16.0))
                .w(px(32.0))
                .flex()
                .justify_center()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(if cents > 0.0 {
                    format!("+{cents:.0}")
                } else {
                    format!("{cents:.0}")
                })
        }));
    card()
        .flex_shrink_0()
        .child(caption("PITCH CORRECTION"))
        .child(
            div()
                .relative()
                .h(px(METER_H))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    canvas(
                        move |bounds, _, _| bounds_out.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                ),
        )
        .child(scale)
        .into_any_element()
}

// ── Correction ─────────────────────────────────────────────────────────────

fn correction(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let classic = w.params.classic;
    let modern_only = classic.then_some("Classic");
    let flex = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::SNUG))
        .child(big_knob(w, cx, "flexTune", SIDE_KNOB, modern_only))
        .child(fb_checkbox(
            "whitesharp-classic",
            "Classic",
            classic,
            true,
            w.click_cb(cx, |this, cx| this.toggle("classic", cx)),
        ));
    card()
        .flex_shrink_0()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(space::BLOCK))
                .child(big_knob(w, cx, "humanize", SIDE_KNOB, modern_only))
                .child(big_knob(w, cx, "retuneMs", HERO_KNOB, None))
                .child(flex)
                .child(big_knob(w, cx, "vibratoDb", SIDE_KNOB, None)),
        )
        .into_any_element()
}

// ── Keyboard ───────────────────────────────────────────────────────────────

/// One octave of keys: lit when the scale has the note, crossed when it is
/// removed, tinted when bypassed. A click puts the note on the list the
/// edit switch picks.
fn keyboard(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let scale = whitesharp::scale_mask(w.params.key, w.params.scale);
    let bounds_out = w.keyboard_bounds.clone();
    let mut keys = div().relative().h(px(KEYBOARD_H)).child(
        canvas(
            move |bounds, _, _| bounds_out.set(Some(bounds)),
            |_, _, _, _| {},
        )
        .absolute()
        .size_full(),
    );
    // White keys first, black keys over them.
    let mut order: Vec<u8> = (0..12).collect();
    order.sort_by_key(|class| key_frame(*class).2);
    for class in order {
        let (left, width, black) = key_frame(class);
        let bit = 1u16 << class;
        let in_scale = scale & bit != 0;
        let removed = w.params.remove_mask & bit != 0;
        let bypassed = w.params.bypass_mask & bit != 0;
        // A piano's colours: the scale's notes bright, the others dimmed,
        // a removed note dark, a bypassed one in the warning hue.
        let fill = match (black, in_scale, removed, bypassed) {
            (_, _, _, true) if !removed => Colors::with_alpha(Colors::accent_warning(), 0.85),
            (false, _, true, _) => Colors::with_alpha(Colors::text_secondary(), 0.14),
            (true, _, true, _) => Colors::surface_canvas(),
            (false, true, _, _) => Colors::text_primary(),
            (false, false, _, _) => Colors::with_alpha(Colors::text_secondary(), 0.45),
            (true, true, _, _) => Colors::text_muted(),
            (true, false, _, _) => Colors::surface_window(),
        };
        let name = whitesharp::NOTE_NAMES[usize::from(class)];
        let status = if removed {
            Some("off")
        } else if bypassed {
            Some("bypass")
        } else {
            None
        };
        let light_key = !black && !removed && (in_scale || bypassed);
        let text = if light_key {
            Colors::surface_canvas()
        } else {
            Colors::text_secondary()
        };
        keys = keys.child(
            div()
                .id(("whitesharp-key-note", usize::from(class)))
                .absolute()
                .top_0()
                .left(relative(left))
                .w(relative(width))
                .h(relative(if black { BLACK_KEY_HEIGHT } else { 1.0 }))
                .border(px(1.0))
                .border_color(Colors::surface_window())
                .rounded_b(px(radius::CONTROL_SM))
                .bg(fill)
                .cursor(gpui::CursorStyle::PointingHand)
                .flex()
                .flex_col()
                .justify_end()
                .items_center()
                .pb(px(space::BASE))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(text)
                .child(name)
                .children(status.map(|status| {
                    div()
                        .text_size(px(typography::DENSE_CAPTION - 1.0))
                        .font_weight(gpui::FontWeight::NORMAL)
                        .child(status)
                }))
                .on_click(w.click_cb(cx, move |this, cx| {
                    let list = this.key_edit;
                    this.toggle_note(list, class, cx)
                })),
        );
    }

    let edit = w.key_edit;
    let mut switch = fb_segmented_track();
    for (index, (list, label)) in [
        (NoteList::Removed, "Remove"),
        (NoteList::Bypassed, "Bypass"),
    ]
    .into_iter()
    .enumerate()
    {
        switch = switch.child(fb_segment(
            ("whitesharp-key-edit", index),
            label,
            edit == list,
            segment_position(index, 2),
            w.click_cb(cx, move |this, cx| this.set_key_edit(list, cx)),
        ));
    }
    card()
        .flex_shrink_0()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::LOOSE))
                .child(caption("KEYBOARD EDIT"))
                .child(switch.w(px(track_width(&["Remove", "Bypass"]))))
                .child(div().flex_1())
                .child(fb_button(
                    "whitesharp-notes-clear",
                    "Clear",
                    FbButtonKind::Ghost,
                    w.params.remove_mask != 0 || w.params.bypass_mask != 0,
                    w.click_cb(cx, |this, cx| this.clear_notes(cx)),
                )),
        )
        .child(keys)
        .into_any_element()
}

// ── Create Vibrato and output ──────────────────────────────────────────────

fn vibrato(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let shapes = whitesharp::VibratoShape::ALL;
    let labels = shapes.map(|shape| shape.label());
    let mut track = fb_segmented_track();
    for (index, shape) in shapes.into_iter().enumerate() {
        track = track.child(fb_segment(
            ("whitesharp-vibrato-shape", index),
            shape.label(),
            w.params.vibrato_shape == shape,
            segment_position(index, shapes.len()),
            w.click_cb(cx, move |this, cx| {
                this.set_value("vibratoShape", shape.to_wire(), cx)
            }),
        ));
    }
    let off = (w.params.vibrato_shape == whitesharp::VibratoShape::None).then_some("off");
    let ids = [
        "vibratoRateHz",
        "vibratoDelayMs",
        "vibratoOnsetMs",
        "vibratoPitch",
        "vibratoAmp",
        "vibratoVariation",
    ];
    let knobs: Vec<AnyElement> = ids
        .iter()
        .map(|id| knob_for(w, cx, id, KNOB, off))
        .collect();
    let mut column = card()
        .flex_basis(px(0.0))
        .min_w(px(ids.len() as f32 * KNOB_PITCH + 2.0 * space::BASE + 2.0));
    column.style().flex_grow = Some(ids.len() as f32);
    column
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(caption("CREATE VIBRATO"))
                .child(track.w(px(track_width(&labels)))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(space::TIGHT))
                .children(knobs),
        )
        .into_any_element()
}

fn output(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let mut column = card()
        .flex_basis(px(0.0))
        .min_w(px(2.0 * KNOB_PITCH + 2.0 * space::BASE + 2.0));
    column.style().flex_grow = Some(2.0);
    column
        .child(caption("OUTPUT"))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(space::TIGHT))
                .child(knob_for(w, cx, "mix", KNOB, None))
                .child(knob_for(w, cx, "outputDb", KNOB, None)),
        )
        .into_any_element()
}
