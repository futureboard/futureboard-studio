//! Channel, bus and master strips, and the mixer they make up.
//!
//! A strip reads top to bottom in signal order: where it comes from, the
//! trim, the insert rack, the sends, the pan, the fader with its meter, the
//! latches, and where it goes. Channels scroll; the buses follow them; the
//! master is pinned at the right edge where it is always in reach.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
};
use livestage_engine::{
    Command, Id, InputPatch, InsertPlugin, InsertSlot, InsertState, MAX_FADER_DB, MIN_FADER_DB,
    StripOutput, StripRef,
};
use sphere_ui_components::components::combo_box::{
    MenuGlyph, TreeMenuNode, combo_box_string_menu, combo_box_tree_menu,
};
use sphere_ui_components::components::controls::{FbLatch, fb_toggle};
use sphere_ui_components::components::fader::fader_with_drag_callbacks;
use sphere_ui_components::components::knob::{knob_bipolar, knob_with_default};
use sphere_ui_components::components::timeline::vu_meter::{amplitude_fraction, meter_surface};
use sphere_ui_components::overlay::OverlayPosition;
use sphere_ui_components::theme::{Colors, radius, space, typography};

use crate::app::{LiveStageApp, MenuKind, OpenMenu, send, with_app};
use crate::fader_law;

const STRIP_WIDTH: f32 = 104.0;
const MASTER_WIDTH: f32 = 112.0;
/// The insert rack and the sends keep a fixed height (scrolling past it), so
/// pans and faders line up across the console however full a strip is.
const INSERT_RACK_HEIGHT: f32 = 4.0 * 22.0 + 20.0;
const SEND_RACK_HEIGHT: f32 = 2.0 * 38.0 + 20.0;

/// Everything a strip draws, copied out of the session so rendering never
/// holds a borrow of the engine.
struct StripModel {
    strip: StripRef,
    name: String,
    input: Option<InputPatch>,
    trim_db: f32,
    phase_invert: bool,
    inserts: Vec<(InsertSlot, InsertState)>,
    sends: Vec<(Id, String, f32, bool)>,
    can_send: bool,
    pan: f32,
    fader_db: f32,
    mute: bool,
    solo: bool,
    record_arm: bool,
    output: Option<StripOutput>,
    output_label: String,
    meter: crate::app::MeterDisplay,
}

fn bus_name(app: &LiveStageApp, id: Id) -> String {
    app.engine
        .session()
        .bus(id)
        .map(|b| b.name.clone())
        .unwrap_or_else(|| "Missing bus".to_string())
}

fn output_label(app: &LiveStageApp, output: StripOutput) -> String {
    match output {
        StripOutput::Master => "Master".to_string(),
        StripOutput::Bus(id) => bus_name(app, id),
        StripOutput::None => "Direct only".to_string(),
    }
}

pub fn input_label(input: InputPatch) -> String {
    match (input.left, input.right) {
        (Some(l), Some(r)) => format!("In {}+{}", l + 1, r + 1),
        (Some(l), None) => format!("In {}", l + 1),
        _ => "No input".to_string(),
    }
}

fn models(app: &LiveStageApp) -> (Vec<StripModel>, Vec<StripModel>, StripModel) {
    let session = app.engine.session();
    let inserts = |core: &livestage_engine::StripCore| {
        core.inserts
            .iter()
            .map(|slot| (slot.clone(), app.engine.insert_state(slot.id)))
            .collect::<Vec<_>>()
    };
    let meter = |strip: StripRef| app.meters.get(&strip).copied().unwrap_or_default();
    let channels = session
        .channels
        .iter()
        .map(|c| StripModel {
            strip: StripRef::Channel(c.id),
            name: c.name.clone(),
            input: Some(c.input),
            trim_db: c.trim_db,
            phase_invert: c.phase_invert,
            inserts: inserts(&c.core),
            sends: c
                .sends
                .iter()
                .map(|s| (s.bus, bus_name(app, s.bus), s.level_db, s.pre_fader))
                .collect(),
            can_send: true,
            pan: c.core.pan,
            fader_db: c.core.fader_db,
            mute: c.core.mute,
            solo: c.core.solo,
            record_arm: c.record_arm,
            output: Some(c.output),
            output_label: output_label(app, c.output),
            meter: meter(StripRef::Channel(c.id)),
        })
        .collect();
    let buses = session
        .buses
        .iter()
        .map(|b| StripModel {
            strip: StripRef::Bus(b.id),
            name: b.name.clone(),
            input: None,
            trim_db: 0.0,
            phase_invert: false,
            inserts: inserts(&b.core),
            sends: Vec::new(),
            can_send: false,
            pan: b.core.pan,
            fader_db: b.core.fader_db,
            mute: b.core.mute,
            solo: b.core.solo,
            record_arm: b.record_arm,
            output: Some(b.output),
            output_label: output_label(app, b.output),
            meter: meter(StripRef::Bus(b.id)),
        })
        .collect();
    let master = StripModel {
        strip: StripRef::Master,
        name: "Master".to_string(),
        input: None,
        trim_db: 0.0,
        phase_invert: false,
        inserts: inserts(&session.master.core),
        sends: Vec::new(),
        can_send: false,
        pan: session.master.core.pan,
        fader_db: session.master.core.fader_db,
        mute: session.master.core.mute,
        solo: false,
        record_arm: session.master.record_arm,
        output: None,
        output_label: String::new(),
        meter: meter(StripRef::Master),
    };
    (channels, buses, master)
}

fn strip_key(strip: StripRef) -> String {
    match strip {
        StripRef::Channel(id) => format!("ch{id}"),
        StripRef::Bus(id) => format!("bus{id}"),
        StripRef::Master => "master".to_string(),
    }
}

/// A small boxed button that opens a menu at the pointer.
fn menu_button(
    id: String,
    label: String,
    this: &WeakEntity<LiveStageApp>,
    kind: MenuKind,
) -> impl IntoElement {
    let this = this.clone();
    let rest = Colors::surface_input();
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(gpui::ElementId::Name(id.into()))
        .flex_none()
        .h(px(20.0))
        .w_full()
        .px(px(space::TIGHT))
        .flex()
        .items_center()
        .rounded(px(radius::CONTROL_SM))
        .border_1()
        .border_color(Colors::border_subtle())
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_secondary())
        .on_mouse_down(MouseButton::Left, move |event, _, cx| {
            cx.stop_propagation();
            let at = event.position;
            with_app(&this, cx, |app, _| {
                app.menu = Some(OpenMenu { kind, at });
            });
        })
        .child(div().flex_1().min_w(px(0.0)).truncate().child(label))
}

fn section_label(text: &'static str) -> impl IntoElement {
    div()
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_faint())
        .child(text)
}

fn insert_row(
    strip: StripRef,
    slot: InsertSlot,
    state: InsertState,
    this: &WeakEntity<LiveStageApp>,
) -> impl IntoElement {
    let id = slot.id;
    let name = slot.plugin.display_name();
    let failed = matches!(state, InsertState::Failed(_));
    let loading = state == InsertState::Loading;
    let rest = Colors::surface_raised();
    let hover = Colors::composite(rest, Colors::state_hover());
    let open = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut gpui::App| {
            let window_bounds = window.bounds();
            with_app(&this, cx, |app, cx| {
                crate::editors::open_editor(app, strip, id, Some(window_bounds), cx);
            });
        }
    };
    let bypass = {
        let this = this.clone();
        let bypassed = slot.bypass;
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            cx.stop_propagation();
            send(
                &this,
                cx,
                Command::SetInsertBypass {
                    insert: id,
                    bypass: !bypassed,
                },
            );
        }
    };
    let remove = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            cx.stop_propagation();
            with_app(&this, cx, |app, cx| {
                app.editors.close(&mut app.engine, id, cx);
                app.run(Command::RemoveInsert { strip, insert: id });
            });
        }
    };
    let tooltip = match &state {
        InsertState::Failed(error) => error.clone(),
        InsertState::Loading => "Loading…".to_string(),
        InsertState::Ready => name.clone(),
    };
    div()
        .id(gpui::ElementId::Name(format!("insert-{id}").into()))
        .flex_none()
        .h(px(20.0))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::HAIR))
        .pl(px(space::TIGHT))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .tooltip(sphere_ui_components::components::controls::fb_tooltip(
            tooltip,
        ))
        .on_click(open)
        .child(
            div()
                .id(gpui::ElementId::Name(format!("insert-bypass-{id}").into()))
                .size(px(8.0))
                .rounded(px(4.0))
                .bg(if failed {
                    Colors::status_error()
                } else if slot.bypass || loading {
                    Colors::text_faint()
                } else {
                    Colors::state_monitor()
                })
                .on_click(bypass),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(if slot.bypass || failed {
                    Colors::text_faint()
                } else {
                    Colors::text_primary()
                })
                .child(name),
        )
        .child(
            div()
                .id(gpui::ElementId::Name(format!("insert-remove-{id}").into()))
                .px(px(space::HAIR))
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_faint())
                .hover(|s| s.text_color(Colors::status_error()))
                .on_click(remove)
                .child("×"),
        )
}

fn send_row(
    channel: Id,
    (bus, name, level_db, pre_fader): (Id, String, f32, bool),
    this: &WeakEntity<LiveStageApp>,
) -> impl IntoElement {
    let set_level = {
        let this = this.clone();
        move |db: &f32, _: &mut Window, cx: &mut gpui::App| {
            send(
                &this,
                cx,
                Command::SetSend {
                    channel,
                    bus,
                    level_db: *db,
                    pre_fader,
                },
            );
        }
    };
    let toggle_pre = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            send(
                &this,
                cx,
                Command::SetSend {
                    channel,
                    bus,
                    level_db,
                    pre_fader: !pre_fader,
                },
            );
        }
    };
    let remove = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            send(&this, cx, Command::RemoveSend { channel, bus });
        }
    };
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::HAIR))
        .child(knob_with_default(
            format!("send-{channel}-{bus}"),
            level_db.max(-60.0),
            -60.0,
            MAX_FADER_DB,
            22.0,
            Colors::accent_primary(),
            MIN_FADER_DB,
            set_level,
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::DENSE_LABEL))
                        .child(name),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(space::TIGHT))
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_faint())
                        .child(
                            div()
                                .id(gpui::ElementId::Name(
                                    format!("send-pre-{channel}-{bus}").into(),
                                ))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .text_color(if pre_fader {
                                    Colors::state_solo()
                                } else {
                                    Colors::text_faint()
                                })
                                .on_click(toggle_pre)
                                .child(if pre_fader { "PRE" } else { "POST" }),
                        )
                        .child(
                            div()
                                .id(gpui::ElementId::Name(
                                    format!("send-x-{channel}-{bus}").into(),
                                ))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .hover(|s| s.text_color(Colors::status_error()))
                                .on_click(remove)
                                .child("×"),
                        ),
                ),
        )
}

fn strip_view(model: StripModel, width: f32, this: &WeakEntity<LiveStageApp>) -> AnyElement {
    let strip = model.strip;
    let key = strip_key(strip);
    let is_master = strip == StripRef::Master;

    let mut column = div()
        .id(gpui::ElementId::Name(format!("strip-{key}").into()))
        .w(px(width))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .p(px(space::TIGHT))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border_1()
        .border_color(Colors::border_subtle());

    // Name.
    column = column.child(
        div()
            .h(px(20.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(typography::UI_SM))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .truncate()
            .child(model.name.clone()),
    );

    // Source.
    if let (StripRef::Channel(id), Some(input)) = (strip, model.input) {
        column = column.child(menu_button(
            format!("{key}-input"),
            input_label(input),
            this,
            MenuKind::ChannelInput(id),
        ));
        let trim = {
            let this = this.clone();
            move |db: &f32, _: &mut Window, cx: &mut gpui::App| {
                send(
                    &this,
                    cx,
                    Command::SetTrim {
                        channel: id,
                        db: *db,
                    },
                );
            }
        };
        let phase = {
            let this = this.clone();
            let invert = model.phase_invert;
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                send(
                    &this,
                    cx,
                    Command::SetPhaseInvert {
                        channel: id,
                        invert: !invert,
                    },
                );
            }
        };
        column = column.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(knob_with_default(
                    format!("{key}-trim"),
                    model.trim_db,
                    -24.0,
                    48.0,
                    26.0,
                    Colors::accent_primary(),
                    0.0,
                    trim,
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_muted())
                        .child("TRIM")
                        .child(format!("{:+.1}", model.trim_db)),
                )
                .child(fb_toggle(
                    format!("{key}-phase"),
                    "Ø",
                    FbLatch::Monitor,
                    model.phase_invert,
                    16.0,
                    phase,
                )),
        );
    }

    // Inserts.
    column = column.child(section_label("INSERTS"));
    let mut rack = div()
        .id(gpui::ElementId::Name(format!("{key}-inserts").into()))
        .h(px(INSERT_RACK_HEIGHT))
        .flex_none()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(space::HAIR));
    for (slot, state) in model.inserts {
        rack = rack.child(insert_row(strip, slot, state, this));
    }
    rack = rack.child(menu_button(
        format!("{key}-add-insert"),
        "+ Effect".to_string(),
        this,
        MenuKind::AddInsert(strip),
    ));
    column = column.child(rack);

    // Sends.
    if model.can_send {
        if let StripRef::Channel(id) = strip {
            column = column.child(section_label("SENDS"));
            let mut sends = div()
                .id(gpui::ElementId::Name(format!("{key}-sends").into()))
                .h(px(SEND_RACK_HEIGHT))
                .flex_none()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(space::HAIR));
            for send in model.sends {
                sends = sends.child(send_row(id, send, this));
            }
            sends = sends.child(menu_button(
                format!("{key}-add-send"),
                "+ Send".to_string(),
                this,
                MenuKind::AddSend(id),
            ));
            column = column.child(sends);
        }
    }

    // Pan.
    let pan = {
        let this = this.clone();
        move |value: &f32, _: &mut Window, cx: &mut gpui::App| {
            send(&this, cx, Command::SetPan { strip, pan: *value });
        }
    };
    column = column.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(space::TIGHT))
            .child(knob_bipolar(
                format!("{key}-pan"),
                model.pan,
                -1.0,
                1.0,
                26.0,
                Colors::accent_primary(),
                None,
                0.0,
                pan,
            ))
            .child(
                div()
                    .w(px(28.0))
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_muted())
                    .child(sphere_ui_components::components::knob::format_pan_label(
                        model.pan,
                    )),
            ),
    );

    // Fader and meter.
    let preview = {
        let this = this.clone();
        move |position: &f32, _: &mut Window, cx: &mut gpui::App| {
            send(
                &this,
                cx,
                Command::SetFader {
                    strip,
                    db: fader_law::position_to_db(*position),
                },
            );
        }
    };
    let reset = {
        let this = this.clone();
        move |_: &mut Window, cx: &mut gpui::App| {
            send(&this, cx, Command::SetFader { strip, db: 0.0 });
        }
    };
    let meter = model.meter;
    column = column.child(
        div()
            .flex_1()
            .min_h(px(120.0))
            .flex()
            .flex_row()
            .justify_center()
            .gap(px(space::SNUG))
            .child(fader_with_drag_callbacks(
                format!("{key}-fader"),
                fader_law::db_to_position(model.fader_db),
                None::<fn(&f32, &mut Window, &mut gpui::App)>,
                Some(preview),
                None::<fn(&mut Window, &mut gpui::App)>,
                Some(reset),
            ))
            .child(meter_surface(
                amplitude_fraction(meter.output.0),
                amplitude_fraction(meter.output.1),
                amplitude_fraction(meter.output.0),
                amplitude_fraction(meter.output.1),
                meter.clipping(),
            )),
    );
    column = column.child(
        div()
            .flex()
            .justify_center()
            .text_size(px(typography::UI_SM))
            .text_color(Colors::text_secondary())
            .child(fader_law::format_db(model.fader_db)),
    );

    // Latches.
    let mute = {
        let this = this.clone();
        let on = model.mute;
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            send(&this, cx, Command::SetMute { strip, mute: !on });
        }
    };
    let solo = {
        let this = this.clone();
        let on = model.solo;
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            send(&this, cx, Command::SetSolo { strip, solo: !on });
        }
    };
    let arm = {
        let this = this.clone();
        let on = model.record_arm;
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            send(&this, cx, Command::SetRecordArm { strip, arm: !on });
        }
    };
    column = column.child(
        div()
            .flex()
            .flex_row()
            .justify_center()
            .gap(px(space::HAIR))
            .child(fb_toggle(
                format!("{key}-mute"),
                "M",
                FbLatch::Mute,
                model.mute,
                16.0,
                mute,
            ))
            .when(!is_master, |row| {
                row.child(fb_toggle(
                    format!("{key}-solo"),
                    "S",
                    FbLatch::Solo,
                    model.solo,
                    16.0,
                    solo,
                ))
            })
            .child(fb_toggle(
                format!("{key}-arm"),
                "●",
                FbLatch::Arm,
                model.record_arm,
                16.0,
                arm,
            )),
    );

    // Destination.
    if model.output.is_some() {
        column = column.child(menu_button(
            format!("{key}-output"),
            format!("→ {}", model.output_label),
            this,
            MenuKind::StripOutput(strip),
        ));
    }

    column.into_any_element()
}

fn add_button(
    id: &'static str,
    label: &'static str,
    this: &WeakEntity<LiveStageApp>,
    command: impl Fn(&LiveStageApp) -> Command + 'static,
) -> impl IntoElement {
    let this = this.clone();
    let rest = Colors::surface_panel();
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .w(px(STRIP_WIDTH))
        .h(px(32.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(Colors::border_subtle())
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(px(typography::UI_SM))
        .text_color(Colors::text_secondary())
        .on_click(move |_, _, cx| {
            with_app(&this, cx, |app, _| {
                let command = command(app);
                app.run(command);
            });
        })
        .child(label)
}

pub fn mixer_view(app: &mut LiveStageApp, cx: &mut Context<LiveStageApp>) -> impl IntoElement {
    let this = cx.entity().downgrade();
    let (channels, buses, master) = models(app);
    let channel_count = channels.len();
    let bus_count = buses.len();

    let mut scroller = div()
        .id("mixer-strips")
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .flex()
        .flex_row()
        .gap(px(space::TIGHT))
        .p(px(space::TIGHT))
        .overflow_x_scroll();
    for model in channels {
        scroller = scroller.child(strip_view(model, STRIP_WIDTH, &this));
    }
    scroller = scroller.child(
        div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .child(add_button(
                "mixer-add-channel",
                "+ Channel",
                &this,
                move |app| {
                    let next_input = app.engine.session().channels.len() as u16;
                    Command::AddChannel {
                        name: format!("Channel {}", channel_count + 1),
                        input: if usize::from(next_input) < app.engine.input_channels() {
                            InputPatch::mono(next_input)
                        } else {
                            InputPatch::default()
                        },
                    }
                },
            )),
    );
    if bus_count > 0 {
        scroller = scroller.child(div().w(px(1.0)).h_full().bg(Colors::border_subtle()));
    }
    for model in buses {
        scroller = scroller.child(strip_view(model, STRIP_WIDTH, &this));
    }
    scroller = scroller.child(add_button("mixer-add-bus", "+ Bus", &this, move |_| {
        Command::AddBus {
            name: format!("Bus {}", bus_count + 1),
        }
    }));

    div().size_full().flex().flex_row().child(scroller).child(
        div()
            .h_full()
            .p(px(space::TIGHT))
            .border_l_1()
            .border_color(Colors::border_subtle())
            .child(strip_view(master, MASTER_WIDTH, &this)),
    )
}

/// The open pop-up menu, over everything, with a backdrop that closes it.
pub fn menu_overlay(
    app: &mut LiveStageApp,
    menu: OpenMenu,
    window: &mut Window,
    cx: &mut Context<LiveStageApp>,
) -> AnyElement {
    let this = cx.entity().downgrade();
    let viewport = window.viewport_size();
    let width = 240.0f32;
    let max_height = 420.0f32;
    let x: f32 = f32::from(menu.at.x)
        .min(f32::from(viewport.width) - width - 8.0)
        .max(8.0);
    let y: f32 = f32::from(menu.at.y)
        .min(f32::from(viewport.height) - max_height.min(f32::from(viewport.height) - 16.0) - 8.0)
        .max(8.0);
    let position = OverlayPosition {
        x: px(x),
        y: px(y),
        width: Some(px(width)),
        max_height: Some(px(max_height)),
    };
    let close = {
        let this = this.clone();
        move |cx: &mut gpui::App| {
            with_app(&this, cx, |app, _| app.menu = None);
        }
    };
    let content = match menu.kind {
        MenuKind::AddInsert(strip) => insert_menu(app, strip, position, &this),
        MenuKind::StripOutput(strip) => {
            let session = app.engine.session();
            let mut choices: Vec<(String, StripOutput)> =
                vec![("Master".to_string(), StripOutput::Master)];
            if matches!(strip, StripRef::Channel(_)) {
                choices.extend(
                    session
                        .buses
                        .iter()
                        .map(|b| (format!("Bus: {}", b.name), StripOutput::Bus(b.id))),
                );
            }
            choices.push(("Direct out only".to_string(), StripOutput::None));
            let current = match strip {
                StripRef::Channel(id) => session.channel(id).map(|c| c.output),
                StripRef::Bus(id) => session.bus(id).map(|b| b.output),
                StripRef::Master => None,
            };
            let selected = choices
                .iter()
                .find(|(_, o)| Some(*o) == current)
                .map(|(l, _)| l.clone())
                .unwrap_or_default();
            let labels: Vec<String> = choices.iter().map(|(l, _)| l.clone()).collect();
            let this = this.clone();
            combo_box_string_menu(
                "livestage-output-menu",
                position,
                &selected,
                &labels,
                std::sync::Arc::new(move |value, _, cx| {
                    if let Some((_, output)) = choices.iter().find(|(l, _)| *l == value) {
                        send(
                            &this,
                            cx,
                            Command::SetStripOutput {
                                strip,
                                output: *output,
                            },
                        );
                    }
                    with_app(&this, cx, |app, _| app.menu = None);
                }),
            )
            .into_any_element()
        }
        MenuKind::ChannelInput(channel) => {
            let inputs = app.engine.input_channels() as u16;
            let mut choices: Vec<(String, InputPatch)> =
                vec![("No input".to_string(), InputPatch::default())];
            for i in 0..inputs {
                choices.push((format!("In {}", i + 1), InputPatch::mono(i)));
            }
            for i in (0..inputs.saturating_sub(1)).step_by(2) {
                choices.push((
                    format!("In {}+{} (stereo)", i + 1, i + 2),
                    InputPatch::stereo(i, i + 1),
                ));
            }
            let current = app.engine.session().channel(channel).map(|c| c.input);
            let selected = choices
                .iter()
                .find(|(_, p)| Some(*p) == current)
                .map(|(l, _)| l.clone())
                .unwrap_or_default();
            let labels: Vec<String> = choices.iter().map(|(l, _)| l.clone()).collect();
            let this = this.clone();
            combo_box_string_menu(
                "livestage-input-menu",
                position,
                &selected,
                &labels,
                std::sync::Arc::new(move |value, _, cx| {
                    if let Some((_, input)) = choices.iter().find(|(l, _)| *l == value) {
                        send(
                            &this,
                            cx,
                            Command::SetChannelInput {
                                channel,
                                input: *input,
                            },
                        );
                    }
                    with_app(&this, cx, |app, _| app.menu = None);
                }),
            )
            .into_any_element()
        }
        MenuKind::AddSend(channel) => {
            let session = app.engine.session();
            let taken: Vec<Id> = session
                .channel(channel)
                .map(|c| c.sends.iter().map(|s| s.bus).collect())
                .unwrap_or_default();
            let choices: Vec<(String, Id)> = session
                .buses
                .iter()
                .filter(|b| !taken.contains(&b.id))
                .map(|b| (b.name.clone(), b.id))
                .collect();
            let labels: Vec<String> = if choices.is_empty() {
                vec!["Add a bus first".to_string()]
            } else {
                choices.iter().map(|(l, _)| l.clone()).collect()
            };
            let this = this.clone();
            combo_box_string_menu(
                "livestage-send-menu",
                position,
                "",
                &labels,
                std::sync::Arc::new(move |value, _, cx| {
                    if let Some((_, bus)) = choices.iter().find(|(l, _)| *l == value) {
                        send(
                            &this,
                            cx,
                            Command::SetSend {
                                channel,
                                bus: *bus,
                                level_db: 0.0,
                                pre_fader: false,
                            },
                        );
                    }
                    with_app(&this, cx, |app, _| app.menu = None);
                }),
            )
            .into_any_element()
        }
    };
    div()
        .id("livestage-menu-backdrop")
        .absolute()
        .inset_0()
        .on_mouse_down(MouseButton::Left, move |_, _, cx| close(cx))
        .child(content)
        .into_any_element()
}

/// Built-in effects by category, then the installed ones by format.
fn insert_menu(
    app: &mut LiveStageApp,
    strip: StripRef,
    position: OverlayPosition,
    this: &WeakEntity<LiveStageApp>,
) -> AnyElement {
    let mut categories: Vec<(&'static str, Vec<TreeMenuNode>)> = Vec::new();
    for effect in livestage_engine::builtin_fx::BUILTIN_EFFECTS {
        let leaf = TreeMenuNode::leaf(
            effect.name,
            format!("builtin:{}", effect.stem),
            MenuGlyph::None,
        );
        match categories.iter_mut().find(|(c, _)| *c == effect.category) {
            Some((_, leaves)) => leaves.push(leaf),
            None => categories.push((effect.category, vec![leaf])),
        }
    }
    let builtin: Vec<TreeMenuNode> = categories
        .into_iter()
        .map(|(category, leaves)| TreeMenuNode::branch(category, MenuGlyph::None, leaves, ""))
        .collect();
    let mut nodes = vec![TreeMenuNode::branch(
        "Futureboard",
        MenuGlyph::Svg(sphere_ui_components::assets::ICON_PLUG_PATH),
        builtin,
        "",
    )];

    #[cfg(feature = "external-plugins")]
    let installed = {
        let installed = app
            .installed
            .get_or_insert_with(livestage_engine::external::installed_effects)
            .clone();
        let mut formats: Vec<(String, Vec<TreeMenuNode>)> = Vec::new();
        for (index, effect) in installed.iter().enumerate() {
            let label = match &effect.vendor {
                Some(vendor) if !vendor.is_empty() => format!("{} — {vendor}", effect.name),
                _ => effect.name.clone(),
            };
            let leaf = TreeMenuNode::leaf(label, format!("ext:{index}"), MenuGlyph::None);
            match formats.iter_mut().find(|(f, _)| *f == effect.format) {
                Some((_, leaves)) => leaves.push(leaf),
                None => formats.push((effect.format.clone(), vec![leaf])),
            }
        }
        nodes.push(TreeMenuNode::branch(
            "Installed",
            MenuGlyph::Svg(sphere_ui_components::assets::ICON_FOLDER_PATH),
            formats
                .into_iter()
                .map(|(format, leaves)| TreeMenuNode::branch(format, MenuGlyph::None, leaves, ""))
                .collect(),
            "No plug-ins found: scan them in Futureboard Studio",
        ));
        installed
    };
    #[cfg(not(feature = "external-plugins"))]
    let _ = app;

    let this = this.clone();
    combo_box_tree_menu(
        "livestage-insert-menu",
        position,
        "",
        nodes,
        std::sync::Arc::new(move |value, _, cx| {
            let plugin = if let Some(stem) = value.strip_prefix("builtin:") {
                Some(InsertPlugin::Builtin {
                    stem: stem.to_string(),
                    params: Default::default(),
                })
            } else {
                #[cfg(feature = "external-plugins")]
                {
                    value
                        .strip_prefix("ext:")
                        .and_then(|i| i.parse::<usize>().ok())
                        .and_then(|i| installed.get(i))
                        .map(|effect| InsertPlugin::External {
                            format: effect.format.clone(),
                            path: effect.path.clone(),
                            class_id: effect.class_id.clone(),
                            name: effect.name.clone(),
                            state: None,
                        })
                }
                #[cfg(not(feature = "external-plugins"))]
                None
            };
            if let Some(plugin) = plugin {
                send(
                    &this,
                    cx,
                    Command::AddInsert {
                        strip,
                        plugin,
                        index: None,
                    },
                );
            }
            with_app(&this, cx, |app, _| app.menu = None);
        }),
    )
    .into_any_element()
}
