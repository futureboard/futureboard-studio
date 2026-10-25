//! WhiteSharp's faceplate: the wordmark and the transport chips, the key and
//! scale rows, the ring you drag for retune speed, the knobs along the
//! floor with the In/Out meters and the level beside them, and the dock
//! under the plate (Keys, Voice, Vibrato).
//!
//! Pure rendering over [`WhiteSharpEditor`], transparent where the stage
//! shows through. The stage itself — the plate's fields, the band, the dial
//! — and the live text and meter fills are painted by the window's live
//! layers into the bounds this panel records (see `white_sharp_window`).
//!
//! The plate is laid out at its design size and scaled to the window by
//! one factor, `s`; the dock under it keeps Studio's sizes.

use std::f32::consts::PI;

use gpui::{
    anchored, canvas, deferred, div, point, px, relative, svg, Anchor, AnyElement, App, AppContext,
    Bounds, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, PathBuilder,
    Pixels, ScrollWheelEvent, StatefulInteractiveElement, Styled, Window,
};

use crate::components::controls::{
    fb_button, fb_checkbox, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::fader::{drag_to, is_reset_click, FaderDrag};
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::quick_sampler_panel::knob_cell;
use crate::components::title_bar::TITLEBAR_HEIGHT;
use crate::components::white_sharp_meter::{piano_key, BLACK_KEY_HEIGHT, PIANO_KEYS};
use crate::components::white_sharp_model::{default_value, knob, presets, value};
use crate::components::white_sharp_style::{
    color, color_a, outfit, pitch_hsl, plate_scale, scale_short, tracked, type_px, DIM, FLOOR,
    KEY_LABELS, ORB, PLATE, PLATE_W,
};
use crate::components::white_sharp_window::{Dock, NoteList, WhiteSharpEditor};
use crate::theme::{space, state, Colors};

type Cx<'a> = Context<'a, WhiteSharpEditor>;

/// The title bar and the identity strip over the plate.
pub(crate) const SHELL_CHROME_H: f32 = TITLEBAR_HEIGHT + 30.0;
/// The dock's height; the window grows by it when a dock opens.
pub(crate) const DOCK_H: f32 = 156.0;

/// The header's and the floor's shares of the plate's height.
const HEADER_SHARE: f32 = 0.21;
const FOOTER_SHARE: f32 = 0.20;
/// The vertical travel, at the design size, that sweeps a control end to end.
const DRAG_SPAN: f32 = 150.0;
const DOCK_KNOB: f32 = 34.0;

/// The whole editor.
pub(crate) fn white_sharp_panel(
    w: &WhiteSharpEditor,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let viewport = window.viewport_size();
    let dock_h = if w.dock.is_some() { DOCK_H } else { 0.0 };
    let plate_w = f32::from(viewport.width);
    let plate_h = (f32::from(viewport.height) - SHELL_CHROME_H - dock_h).max(1.0);
    let s = plate_scale(plate_w, plate_h);

    div()
        .flex()
        .flex_col()
        .size_full()
        .font(outfit(FontWeight::NORMAL))
        .child(plate(w, cx, s, plate_w, plate_h))
        .children(w.dock.map(|dock| dock_panel(w, cx, dock)))
        .into_any_element()
}

// ── The plate ──────────────────────────────────────────────────────────────

fn plate(w: &WhiteSharpEditor, cx: &mut Cx, s: f32, plate_w: f32, plate_h: f32) -> AnyElement {
    let stage_cell = w.cells.stage.clone();
    let d = ORB * PLATE_W * s;
    div()
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .overflow_hidden()
        .child(
            canvas(
                move |bounds, _, _| stage_cell.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(orb(w, cx, (plate_w - d) * 0.5, (plate_h - d) * 0.5, d))
        .child(header(w, cx, s, plate_h))
        .child(footer(w, cx, s, plate_h))
        .into_any_element()
}

/// The ring: a drag up or down, or the wheel, sets retune speed.
fn orb(w: &WhiteSharpEditor, cx: &mut Cx, left: f32, top: f32, d: f32) -> AnyElement {
    let orb_cell = w.cells.orb.clone();
    let Some(spec) = knob("retuneMs") else {
        return div().into_any_element();
    };
    let current = spec.fraction(value(&w.params, "retuneMs"));
    let entity = cx.entity().clone();
    let on_drag = entity.clone();
    let on_wheel = entity.clone();
    let id = "whitesharp-orb";
    div()
        .id(id)
        .absolute()
        .left(px(left))
        .top(px(top))
        .size(px(d))
        .rounded_full()
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(
            canvas(
                move |bounds, _, _| orb_cell.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_drag(FaderDrag::new(id.to_string()), |drag, _, _, cx| {
            cx.new(|_| FaderDrag::new(drag.id.clone()))
        })
        .on_drag_move::<FaderDrag>(move |event, _window, app| {
            let drag = event.drag(app);
            if drag.id != id {
                return;
            }
            let y = f32::from(event.event.position.y);
            let next = drag_to(
                drag,
                current,
                y,
                event.event.modifiers.shift,
                DRAG_SPAN,
                |dy| -dy,
            );
            let _ = on_drag.update(app, |this, cx| {
                this.set_value(spec.id, spec.value_at(next), cx)
            });
        })
        .on_scroll_wheel(move |event: &ScrollWheelEvent, _window, app| {
            let dy = f32::from(event.delta.pixel_delta(px(20.0)).y);
            if dy == 0.0 {
                return;
            }
            let step = if event.modifiers.shift {
                1_200.0
            } else {
                600.0
            };
            let _ = on_wheel.update(app, |this, cx| {
                let now = spec.fraction(value(&this.params, spec.id));
                let next = (now + dy / step).clamp(0.0, 1.0);
                this.set_value(spec.id, spec.value_at(next), cx)
            });
        })
        .on_click(move |event, _window, app| {
            if is_reset_click(event) {
                let _ = entity.update(app, |this, cx| {
                    this.set_value(spec.id, default_value(spec.id), cx)
                });
            }
        })
        .into_any_element()
}

// ── Header ─────────────────────────────────────────────────────────────────

fn header(w: &WhiteSharpEditor, cx: &mut Cx, s: f32, plate_h: f32) -> AnyElement {
    let on_b = w.compare.on_b;
    let notice = w.notice().map(|text| {
        div()
            .text_size(type_px(10.0, s))
            .text_color(color(0x7a7a7a))
            .child(tracked(text))
    });
    let brand = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0 * s))
        .min_w(px(0.0))
        .child(
            svg()
                .path(crate::assets::WHITESHARP_WORDMARK_PATH)
                .w(px(16.0 * s * 948.0 / 96.0))
                .h(px(16.0 * s))
                .flex_shrink_0()
                .text_color(color(0xececec)),
        )
        .children(notice);

    let dock_chip = |dock: Dock, label: &'static str, id: &'static str| {
        chip(
            id,
            label,
            w.dock == Some(dock),
            s,
            w.click_window_cb(cx, move |this, window, cx| {
                this.toggle_dock(dock, window, cx)
            }),
        )
    };
    let transport = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0 * s))
        .child(dock_chip(Dock::Keys, "Keys", "whitesharp-dock-keys"))
        .child(dock_chip(Dock::Voice, "Voice", "whitesharp-dock-voice"))
        .child(dock_chip(
            Dock::Vibrato,
            "Vibrato",
            "whitesharp-dock-vibrato",
        ))
        .child(div().w(px(6.0 * s)))
        .child(chip(
            "whitesharp-a",
            "A",
            !on_b,
            s,
            w.click_cb(cx, |this, cx| {
                if this.compare.on_b {
                    this.swap_compare(cx)
                }
            }),
        ))
        .child(chip(
            "whitesharp-b",
            "B",
            on_b,
            s,
            w.click_cb(cx, |this, cx| {
                if !this.compare.on_b {
                    this.swap_compare(cx)
                }
            }),
        ))
        .child(chip(
            "whitesharp-copy",
            if on_b { "Copy to A" } else { "Copy to B" },
            false,
            s,
            w.click_cb(cx, |this, cx| this.copy_compare(cx)),
        ))
        .child(div().w(px(6.0 * s)))
        .child(chip(
            "whitesharp-bypass",
            "Bypass",
            !w.params.power,
            s,
            w.click_cb(cx, |this, cx| this.toggle("power", cx)),
        ))
        .child(preset_picker(w, cx, s));

    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(plate_h * HEADER_SHARE))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(6.0 * s))
        .px(relative(0.03))
        .pb(px(6.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0 * s))
                .child(brand)
                .child(transport),
        )
        .child(key_row(w, cx, s))
        .child(scale_row(w, cx, s))
        .into_any_element()
}

/// A rounded caption button; `pressed` marks it on.
fn chip(
    id: impl Into<gpui::ElementId>,
    label: &str,
    pressed: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .gap(px(6.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded_full()
        .border(px(1.0))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(type_px(10.0, s))
        .child(tracked(label))
        .on_click(on_click);
    if pressed {
        base.text_color(color(0xffffff))
            .border_color(color(0x6a6a6a))
            .bg(color(0x1c1c1c))
    } else {
        base.text_color(color(0xc2c2c2))
            .border_color(color_a(0, 0.0))
            .hover(|style| style.text_color(color(0xffffff)))
    }
}

/// The factory presets, in a menu under their chip.
fn preset_picker(w: &WhiteSharpEditor, cx: &mut Cx, s: f32) -> AnyElement {
    let open = w.preset_open;
    let chevron = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let unit = f32::from(bounds.size.width) / 12.0;
            let at = |x: f32, y: f32| bounds.origin + point(px(x * unit), px(y * unit));
            let mut path = PathBuilder::stroke(px(1.3 * unit));
            path.move_to(at(2.4, 4.4));
            path.line_to(at(6.0, 8.0));
            path.line_to(at(9.6, 4.4));
            if let Ok(path) = path.build() {
                window.paint_path(path, color(if open { 0xffffff } else { 0xc2c2c2 }));
            }
        },
    )
    .w(px(10.0 * s))
    .h(px(10.0 * s));
    let button = chip(
        "whitesharp-preset",
        &w.preset_label(),
        open,
        s,
        w.click_cb(cx, |this, cx| this.toggle_menu(cx)),
    )
    .child(chevron);

    let mut picker = div().relative().flex_shrink_0().child(button);
    if open {
        let bank = presets();
        let current = w.preset;
        let entity = cx.entity().clone();
        let close = entity.clone();
        let items = bank.iter().enumerate().map(|(index, preset)| {
            let entity = entity.clone();
            let selected = current == Some(index);
            let item = div()
                .id(("whitesharp-preset-item", index))
                .w_full()
                .px(px(10.0 * s))
                .py(px(8.0 * s))
                .text_size(type_px(13.0, s))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(preset.name)
                .on_click(move |_, _, app| {
                    let _ = entity.update(app, |this, cx| this.load_preset(index, cx));
                });
            if selected {
                item.text_color(color(0xffffff)).bg(color(0x242424))
            } else {
                item.text_color(color(0xa6a6a6))
                    .hover(|style| style.text_color(color(0xffffff)).bg(color(0x242424)))
            }
        });
        let menu = div()
            .id("whitesharp-preset-menu")
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(196.0 * s))
            .max_h(px(360.0 * s))
            .overflow_y_scroll()
            .p(px(6.0 * s))
            .bg(color(0x161616))
            .border(px(1.0))
            .border_color(color(0x2e2e2e))
            .shadow(vec![gpui::BoxShadow {
                color: color_a(0, 0.5).into(),
                offset: point(px(0.0), px(18.0 * s)),
                blur_radius: px(40.0 * s),
                spread_radius: px(0.0),
                inset: false,
            }])
            .font(outfit(FontWeight::NORMAL))
            .children(items)
            .on_mouse_down_out(move |_, _, app| {
                let _ = close.update(app, |this, cx| this.close_menu(cx));
            });
        picker = picker.child(
            div().absolute().right_0().bottom_0().child(deferred(
                anchored()
                    .anchor(Anchor::TopRight)
                    .offset(point(px(0.0), px(8.0 * s)))
                    .snap_to_window_with_margin(px(8.0))
                    .child(menu),
            )),
        );
    }
    picker.into_any_element()
}

/// A text button of the key and scale rows; the pressed one is underlined.
fn text_button(
    id: impl Into<gpui::ElementId>,
    label: String,
    size: f32,
    pressed: Option<(gpui::Hsla, gpui::Hsla)>,
    pad_x: f32,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let base = div()
        .id(id)
        .relative()
        .flex_shrink_0()
        .px(px(pad_x * s))
        .py(px(4.0 * s))
        .rounded(px(4.0 * s))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(type_px(size, s))
        .child(label)
        .on_click(on_click);
    match pressed {
        Some((ink, line)) => base
            .text_color(ink)
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(1.0))
                    .bg(line),
            )
            .into_any_element(),
        None => base
            .text_color(color(0x7d7d7d))
            .hover(|style| style.text_color(color(0xd7d7d7)))
            .into_any_element(),
    }
}

fn selector_row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .justify_center()
        .gap_x(px(1.0))
        .gap_y(px(2.0))
}

fn key_row(w: &WhiteSharpEditor, cx: &mut Cx, s: f32) -> AnyElement {
    let current = w.params.key;
    selector_row()
        .children((0u8..12).map(|key| {
            let class = i32::from(key);
            let pressed = (key == current).then(|| {
                (
                    pitch_hsl(class, 64.0, 86.0, 1.0),
                    pitch_hsl(class, 70.0, 64.0, 1.0),
                )
            });
            text_button(
                ("whitesharp-key", usize::from(key)),
                KEY_LABELS[usize::from(key)].to_string(),
                12.0,
                pressed,
                4.0,
                s,
                w.click_cb(cx, move |this, cx| this.set_key(key, cx)),
            )
        }))
        .into_any_element()
}

fn scale_row(w: &WhiteSharpEditor, cx: &mut Cx, s: f32) -> AnyElement {
    let current = w.params.scale;
    selector_row()
        .children(
            whitesharp::Scale::ALL
                .iter()
                .enumerate()
                .map(|(index, &scale)| {
                    let pressed = (scale == current)
                        .then(|| (color(0xf7f7f7).into(), color(0x6f6f6f).into()));
                    text_button(
                        ("whitesharp-scale", index),
                        tracked(scale_short(scale)),
                        10.0,
                        pressed,
                        7.0,
                        s,
                        w.click_cb(cx, move |this, cx| this.set_scale(scale, cx)),
                    )
                }),
        )
        .into_any_element()
}

// ── Floor: knobs, meters, level ────────────────────────────────────────────

/// The knobs on the floor: the name each shows, and why it has nothing to do.
fn floor_knobs(w: &WhiteSharpEditor) -> [(&'static str, &'static str, Option<&'static str>); 6] {
    let classic = w.params.classic.then_some("Classic");
    let throat = (!w.params.latency.shapes_formants()).then_some("Quality");
    [
        ("retuneMs", "Retune", None),
        ("flexTune", "Flex", classic),
        ("humanize", "Human", classic),
        ("throat", "Throat", throat),
        ("vibratoDb", "Vibrato", None),
        ("mix", "Mix", None),
    ]
}

fn footer(w: &WhiteSharpEditor, cx: &mut Cx, s: f32, plate_h: f32) -> AnyElement {
    let gap = (PLATE_W * s * 0.015).clamp(8.0, 22.0);
    let knobs: Vec<AnyElement> = floor_knobs(w)
        .into_iter()
        .map(|(id, name, why_not)| plate_knob(w, cx, id, name, why_not, s))
        .collect();
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(px(plate_h * FOOTER_SHARE))
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .child(div().flex().flex_row().gap(px(gap)).children(knobs))
        .child(monitor(w, cx, s))
        .into_any_element()
}

/// A knob in the plate's style: a dark face in a 270° ring, its value in
/// the middle and its name under it. Up turns it up; a double-click resets.
fn plate_knob(
    w: &WhiteSharpEditor,
    cx: &mut Cx,
    id: &'static str,
    name: &'static str,
    why_not: Option<&'static str>,
    s: f32,
) -> AnyElement {
    let Some(spec) = knob(id) else {
        return div().into_any_element();
    };
    let current = value(&w.params, id);
    let fraction = spec.fraction(current).clamp(0.0, 1.0);
    let enabled = why_not.is_none();
    let face_size = 56.0 * s;
    let element_id = format!("whitesharp-knob-{id}");
    let group = format!("whitesharp-knob-group-{id}");

    let dial = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_plate_knob(bounds, fraction, s, window),
    )
    .absolute()
    .size_full();
    let face = div()
        .relative()
        .size(px(face_size))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border(px(1.5))
        .border_color(color(0x2a2a2a))
        .shadow(vec![gpui::BoxShadow {
            color: color_a(0, 0.35).into(),
            offset: point(px(0.0), px(8.0 * s)),
            blur_radius: px(16.0 * s),
            spread_radius: px(0.0),
            inset: false,
        }])
        .child(dial)
        .child(
            // The face is narrower than a spaced readout: "12ms", not "12 ms".
            div()
                .relative()
                .text_size(type_px(11.0, s))
                .text_color(color(0xdedede))
                .child(
                    why_not.map_or_else(|| spec.readout(current).replace(' ', ""), str::to_string),
                ),
        );
    let face = if enabled {
        face.group_hover(gpui::SharedString::from(group.clone()), |style| {
            style.border_color(color(0x6a6a6a))
        })
    } else {
        face
    };

    let mut cell = div()
        .id(gpui::ElementId::Name(element_id.clone().into()))
        .group(gpui::SharedString::from(group))
        .flex()
        .flex_col()
        .items_center()
        .flex_shrink_0()
        .w(px(76.0 * s))
        .child(face)
        .child(
            div()
                .mt(px(6.0 * s))
                .text_size(type_px(10.0, s))
                .text_color(color(0x9a9a9a))
                .child(tracked(name)),
        );
    if !enabled {
        return cell.opacity(state::DISABLED_CONTENT).into_any_element();
    }
    let entity = cx.entity().clone();
    let on_reset = entity.clone();
    cell = cell
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .on_drag(FaderDrag::new(element_id.clone()), |drag, _, _, cx| {
            cx.new(|_| FaderDrag::new(drag.id.clone()))
        })
        .on_drag_move::<FaderDrag>(move |event, _window, app| {
            let drag = event.drag(app);
            if drag.id != element_id {
                return;
            }
            let y = f32::from(event.event.position.y);
            let next = drag_to(
                drag,
                fraction,
                y,
                event.event.modifiers.shift,
                DRAG_SPAN,
                |dy| -dy,
            );
            let _ = entity.update(app, |this, cx| this.set_value(id, spec.value_at(next), cx));
        })
        .on_click(move |event, _window, app| {
            if is_reset_click(event) {
                let _ = on_reset.update(app, |this, cx| this.set_value(id, default_value(id), cx));
            }
        });
    cell.into_any_element()
}

/// The ring of a plate knob — its track and its sweep from the bottom left
/// — the dark face inside it, and the tick on the rotor.
fn paint_plate_knob(bounds: Bounds<Pixels>, fraction: f32, s: f32, window: &mut Window) {
    let centre = bounds.center();
    let (cx0, cy0) = (f32::from(centre.x), f32::from(centre.y));
    let at = |deg: f32, r: f32| {
        let rad = deg * PI / 180.0;
        point(px(cx0 + r * rad.sin()), px(cy0 - r * rad.cos()))
    };
    let ring_r = 25.25 * s;
    let ring_w = 2.5 * s;
    let arc = |from: f32, to: f32, ink: gpui::Rgba, window: &mut Window| {
        if to - from < 0.1 {
            return;
        }
        let steps = ((to - from) / 4.0).ceil().max(2.0) as usize;
        let mut path = PathBuilder::stroke(px(ring_w));
        path.move_to(at(from, ring_r));
        for step in 1..=steps {
            path.line_to(at(from + (to - from) * step as f32 / steps as f32, ring_r));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, ink);
        }
    };
    arc(-135.0, 135.0, color(0x343434), window);
    arc(-135.0, -135.0 + 270.0 * fraction, color(0xdedede), window);

    let face_r = 23.6 * s;
    window.paint_quad(
        gpui::fill(
            Bounds::new(
                point(px(cx0 - face_r), px(cy0 - face_r)),
                gpui::size(px(face_r * 2.0), px(face_r * 2.0)),
            ),
            color(PLATE),
        )
        .corner_radii(px(face_r)),
    );

    let deg = -135.0 + 270.0 * fraction;
    let mut tick = PathBuilder::stroke(px(2.0 * s));
    tick.move_to(at(deg, 21.5 * s));
    tick.line_to(at(deg, 14.5 * s));
    if let Ok(path) = tick.build() {
        window.paint_path(path, color(0xdedede));
    }
}

/// In and Out, and the output level, at the floor's right.
fn monitor(w: &WhiteSharpEditor, cx: &mut Cx, s: f32) -> AnyElement {
    let meter_row =
        |label: &'static str, cell: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>| {
            row(label, s).child(
                div()
                    .flex_1()
                    .h(px(4.0 * s))
                    .rounded_full()
                    .bg(color(0x101010))
                    .child(
                        canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
                            .size_full(),
                    ),
            )
        };
    div()
        .absolute()
        .right(px(28.0 * s))
        .top_0()
        .bottom_0()
        .w(px(132.0 * s))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(7.0 * s))
        .child(meter_row("In", w.cells.meter_in.clone()))
        .child(meter_row("Out", w.cells.meter_out.clone()))
        .child(level(w, cx, s))
        .into_any_element()
}

fn row(label: &'static str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0 * s))
        .child(
            div()
                .w(px(42.0 * s))
                .flex_shrink_0()
                .text_size(type_px(9.0, s))
                .text_color(color(0x8a8a8a))
                .child(tracked(label)),
        )
}

/// The output level: a slider, its value in place of its name while the
/// pointer is over it; a double-click resets it.
fn level(w: &WhiteSharpEditor, cx: &mut Cx, s: f32) -> AnyElement {
    let Some(spec) = knob("outputDb") else {
        return div().into_any_element();
    };
    let current = value(&w.params, spec.id);
    let fraction = spec.fraction(current).clamp(0.0, 1.0);
    let id = "whitesharp-level";
    let group = "whitesharp-level-group";
    let span = (132.0 - 42.0 - 8.0) * s;
    let entity = cx.entity().clone();
    let on_reset = entity.clone();
    let label = div()
        .relative()
        .w(px(42.0 * s))
        .flex_shrink_0()
        .text_size(type_px(9.0, s))
        .text_color(color(0x8a8a8a))
        .child(
            div()
                .group_hover(group, |style| style.opacity(0.0))
                .child(tracked("Level")),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .opacity(0.0)
                .text_color(color(0xdedede))
                .group_hover(group, |style| style.opacity(1.0))
                .child(spec.readout(current)),
        );
    let thumb = 10.0 * s;
    let slider = div()
        .id(id)
        .relative()
        .flex_1()
        .h(px(14.0 * s))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(6.0 * s))
                .h(px(2.0 * s))
                .rounded_full()
                .bg(color(0x101010)),
        )
        .child(
            div()
                .absolute()
                .top(px(2.0 * s))
                .left(px((span - thumb) * fraction))
                .size(px(thumb))
                .rounded_full()
                .bg(color(0xdedede)),
        )
        .on_drag(FaderDrag::new(id.to_string()), |drag, _, _, cx| {
            cx.new(|_| FaderDrag::new(drag.id.clone()))
        })
        .on_drag_move::<FaderDrag>(move |event, _window, app| {
            let drag = event.drag(app);
            if drag.id != id {
                return;
            }
            let x = f32::from(event.event.position.x);
            let next = drag_to(drag, fraction, x, event.event.modifiers.shift, span, |dx| {
                dx
            });
            let _ = entity.update(app, |this, cx| {
                this.set_value(spec.id, spec.value_at(next), cx)
            });
        })
        .on_click(move |event, _window, app| {
            if is_reset_click(event) {
                let _ = on_reset.update(app, |this, cx| {
                    this.set_value(spec.id, default_value(spec.id), cx)
                });
            }
        });
    div()
        .group(group)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0 * s))
        .child(label)
        .child(slider)
        .into_any_element()
}

// ── The dock ───────────────────────────────────────────────────────────────

fn dock_panel(w: &WhiteSharpEditor, cx: &mut Cx, dock: Dock) -> AnyElement {
    let body = match dock {
        Dock::Keys => keys_dock(w, cx),
        Dock::Voice => voice_dock(w, cx),
        Dock::Vibrato => vibrato_dock(w, cx),
    };
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .h(px(DOCK_H))
        .bg(color(FLOOR))
        .border_t(px(1.0))
        .border_color(color_a(0xffffff, 0.06))
        .pt(px(12.0))
        .px(px(14.0))
        .pb(px(14.0))
        .child(body)
        .into_any_element()
}

/// The dock's caption line: its name at the left, its switches at the right.
fn dock_bar(title: &'static str, right: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .h(px(26.0))
        .mb(px(10.0))
        .child(
            div()
                .text_size(type_px(10.0, 1.0))
                .text_color(color(0x8a8a8a))
                .child(tracked(title)),
        )
        .child(right)
}

fn segment_position(index: usize, count: usize) -> FbSegment {
    match index {
        0 => FbSegment::First,
        i if i + 1 == count => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

/// Two octaves of keys: a dot on the scale's notes, the tonic underlined in
/// its colour. A click puts the note on the list the switch picks — removed
/// notes go dark, bypassed ones keep a grey dot.
fn keys_dock(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let scale = whitesharp::scale_mask(w.params.key, w.params.scale);
    let tonic = w.params.key;
    let keyboard_cell = w.cells.keyboard.clone();
    let mut piano = div().relative().flex_1().min_h(px(0.0)).child(
        canvas(
            move |bounds, _, _| keyboard_cell.set(Some(bounds)),
            |_, _, _, _| {},
        )
        .absolute()
        .size_full(),
    );
    // White keys first, black keys over them.
    let mut order: Vec<u8> = (0..PIANO_KEYS).collect();
    order.sort_by_key(|n| piano_key(*n).2);
    for n in order {
        let class = n % 12;
        let (left, width, black) = piano_key(n);
        let bit = 1u16 << class;
        let in_scale = scale & bit != 0;
        let removed = w.params.remove_mask & bit != 0;
        let bypassed = w.params.bypass_mask & bit != 0;
        let hue = i32::from(class);
        let (fill, hover) = match (black, removed) {
            (false, false) => (0x2a2a2a, 0x333333),
            (false, true) => (0x1a1a1a, 0x222222),
            (true, false) => (0x0e0e0e, 0x262626),
            (true, true) => (0x060606, 0x1a1a1a),
        };
        let dot = if removed {
            None
        } else if bypassed {
            Some(color(0x9a9a9a).into())
        } else if in_scale || class == tonic {
            Some(pitch_hsl(hue, 70.0, 70.0, 1.0))
        } else {
            None
        };
        let label = if removed {
            Some("OFF".to_string())
        } else if bypassed {
            Some("BYP".to_string())
        } else if class == 0 && !black {
            Some(format!("C{}", 3 + n / 12))
        } else {
            None
        };
        let mut key = div()
            .id(("whitesharp-piano", usize::from(n)))
            .absolute()
            .top_0()
            .left(relative(left))
            .w(relative(width))
            .h(relative(if black { BLACK_KEY_HEIGHT } else { 1.0 }))
            .bg(color(fill))
            .hover(move |style| style.bg(color(hover)))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_click(w.click_cb(cx, move |this, cx| {
                let list = this.key_edit;
                this.toggle_note(list, class, cx)
            }));
        key = if black {
            key.rounded_b(px(4.0)).shadow(vec![gpui::BoxShadow {
                color: color_a(0, 0.45).into(),
                offset: point(px(0.0), px(6.0)),
                blur_radius: px(10.0),
                spread_radius: px(0.0),
                inset: false,
            }])
        } else {
            key.border_r(px(1.0)).border_color(color(PLATE))
        };
        if class == tonic && !removed {
            key = key.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(3.0))
                    .bg(pitch_hsl(hue, 70.0, 66.0, 1.0)),
            );
        }
        if let Some(dot) = dot {
            key = key.child(
                div()
                    .absolute()
                    .top(px(8.0))
                    .left(relative(0.5))
                    .ml(px(-2.0))
                    .size(px(4.0))
                    .rounded_full()
                    .bg(dot),
            );
        }
        if let (Some(label), false) = (label, black) {
            key = key.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(6.0))
                    .flex()
                    .justify_center()
                    .text_size(type_px(9.0, 1.0))
                    .text_color(color(0x6f6f6f))
                    .child(label),
            );
        }
        piano = piano.child(key);
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
    let switches = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(switch)
        .child(fb_button(
            "whitesharp-notes-clear",
            "Clear",
            FbButtonKind::Ghost,
            w.params.remove_mask != 0 || w.params.bypass_mask != 0,
            w.click_cb(cx, |this, cx| this.clear_notes(cx)),
        ));
    div()
        .flex()
        .flex_col()
        .size_full()
        .child(dock_bar("Keyboard", switches))
        .child(piano)
        .into_any_element()
}

/// A dock knob in Studio's style, greyed with `why_not` in place of its
/// value when it has nothing to do.
fn dock_knob(
    w: &WhiteSharpEditor,
    cx: &mut Cx,
    id: &'static str,
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
                    DOCK_KNOB,
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
            DOCK_KNOB,
            color(0xdedede),
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
            DOCK_KNOB,
            color(0xdedede),
            default,
            on_change,
        )
        .into_any_element()
    };
    knob_cell(spec.label, spec.readout(current), control)
}

fn setting(title: &'static str, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(type_px(9.0, 1.0))
                .text_color(color(0x8a8a8a))
                .child(tracked(title)),
        )
        .child(control)
        .into_any_element()
}

/// The voice, how the correction is played, and the pitch offsets.
fn voice_dock(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let inputs = whitesharp::InputType::ALL;
    let mut input_track = fb_segmented_track();
    for (index, input) in inputs.into_iter().enumerate() {
        input_track = input_track.child(fb_segment(
            ("whitesharp-input", index),
            input.label(),
            w.params.input_type == input,
            segment_position(index, inputs.len()),
            w.click_cb(cx, move |this, cx| {
                this.set_value("inputType", input.to_wire(), cx)
            }),
        ));
    }
    let mode = w.params.latency;
    let modes = whitesharp::LatencyMode::ALL;
    let mut latency_track = fb_segmented_track();
    for (index, choice) in modes.into_iter().enumerate() {
        latency_track = latency_track.child(fb_segment(
            ("whitesharp-latency", index),
            choice.label(),
            mode == choice,
            segment_position(index, modes.len()),
            w.click_cb(cx, move |this, cx| {
                this.set_value("latency", choice.to_wire(), cx)
            }),
        ));
    }
    let rate = w.sample_rate();
    let ms =
        whitesharp::latency_samples_for(rate, w.params.input_type, mode) as f32 * 1_000.0 / rate;
    let delay = if ms < 1.0 {
        format!("{ms:.2} ms")
    } else {
        format!("{ms:.1} ms")
    };
    // The live path has no grains to reshape: formant correction needs the
    // Quality path's lookahead.
    let shapes = mode.shapes_formants();
    let latency = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(latency_track)
        .child(
            div()
                .text_size(type_px(11.0, 1.0))
                .text_color(color(DIM))
                .child(delay),
        );
    let switches = div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .child(fb_checkbox(
            "whitesharp-formant",
            if shapes {
                "Keep formants"
            } else {
                "Formants: Quality only"
            },
            w.params.formant && shapes,
            shapes,
            w.click_cb(cx, |this, cx| this.toggle("formant", cx)),
        ))
        .child(fb_checkbox(
            "whitesharp-classic",
            "Classic",
            w.params.classic,
            true,
            w.click_cb(cx, |this, cx| this.toggle("classic", cx)),
        ));
    div()
        .flex()
        .flex_col()
        .size_full()
        .child(dock_bar("Voice", div()))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(24.0))
                .child(setting("Input type", input_track))
                .child(setting("Latency", latency))
                .child(switches)
                .child(div().flex_1())
                .child(dock_knob(w, cx, "transpose", None))
                .child(dock_knob(w, cx, "detune", None))
                .child(dock_knob(w, cx, "tracking", None)),
        )
        .into_any_element()
}

/// Create Vibrato: its shape, and the knobs that shape it.
fn vibrato_dock(w: &WhiteSharpEditor, cx: &mut Cx) -> AnyElement {
    let shapes = whitesharp::VibratoShape::ALL;
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
    let off = (w.params.vibrato_shape == whitesharp::VibratoShape::None).then_some("Off");
    let ids = [
        "vibratoRateHz",
        "vibratoDelayMs",
        "vibratoOnsetMs",
        "vibratoPitch",
        "vibratoAmp",
        "vibratoVariation",
    ];
    let knobs: Vec<AnyElement> = ids.iter().map(|id| dock_knob(w, cx, id, off)).collect();
    div()
        .flex()
        .flex_col()
        .size_full()
        .child(dock_bar("Create vibrato", track))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_center()
                .gap(px(space::LOOSE))
                .children(knobs),
        )
        .into_any_element()
}
