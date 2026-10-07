//! Setup: the interface, and where and how the recorder writes.
//!
//! Device changes are drafted here and applied together, because reopening
//! an interface interrupts the sound: picking the input, then the output,
//! then the rate should cost one dropout, not three.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    WeakEntity, div, px,
};
use livestage_engine::device::DeviceInfo;
use livestage_engine::{AudioSettings, Command, RecordFormat, RecordSettings, RecordTap, Session};
use sphere_ui_components::components::controls::{
    FbButtonKind, FbSegment, fb_button, fb_section_header, fb_segment, fb_segmented_track,
};
use sphere_ui_components::theme::{Colors, radius, space, typography};

use crate::app::{LiveStageApp, with_app};

pub struct SetupState {
    pub hosts: Vec<String>,
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
    pub draft: AudioSettings,
}

impl SetupState {
    pub fn from_session(session: &Session) -> Self {
        let draft = session.audio.clone();
        let (inputs, outputs) = livestage_engine::device::list_devices(draft.host.as_deref());
        Self {
            hosts: livestage_engine::device::host_names(),
            inputs,
            outputs,
            draft,
        }
    }

    fn rescan(&mut self) {
        let (inputs, outputs) = livestage_engine::device::list_devices(self.draft.host.as_deref());
        self.inputs = inputs;
        self.outputs = outputs;
        self.hosts = livestage_engine::device::host_names();
    }
}

const RATES: [(u32, &str); 5] = [
    (0, "Device"),
    (44_100, "44.1k"),
    (48_000, "48k"),
    (88_200, "88.2k"),
    (96_000, "96k"),
];
const BUFFERS: [(u32, &str); 6] = [
    (64, "64"),
    (128, "128"),
    (256, "256"),
    (512, "512"),
    (1024, "1024"),
    (0, "Auto"),
];

fn position(index: usize, count: usize) -> FbSegment {
    match (index, count) {
        (_, 1) => FbSegment::Only,
        (0, _) => FbSegment::First,
        (i, n) if i + 1 == n => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

/// A row of choices; `pick` stores the chosen value.
fn segments<T: Copy + PartialEq + 'static>(
    id: &'static str,
    choices: &[(T, &'static str)],
    current: T,
    this: &WeakEntity<LiveStageApp>,
    pick: fn(&mut LiveStageApp, T),
) -> impl IntoElement {
    let count = choices.len();
    fb_segmented_track().children(choices.iter().enumerate().map(|(index, (value, label))| {
        let value = *value;
        let this = this.clone();
        fb_segment(
            (id, index),
            *label,
            value == current,
            position(index, count),
            move |_, _, cx| with_app(&this, cx, |app, _| pick(app, value)),
        )
    }))
}

/// A list of devices to pick one of; `None` is the system default.
fn device_list(
    id: &'static str,
    devices: &[DeviceInfo],
    current: Option<&str>,
    this: &WeakEntity<LiveStageApp>,
    pick: fn(&mut LiveStageApp, Option<String>),
) -> impl IntoElement {
    let mut rows: Vec<(Option<String>, String)> = vec![(None, "System default".to_string())];
    rows.extend(devices.iter().map(|d| {
        (
            Some(d.name.clone()),
            format!("{} — {} ch", d.name, d.channels),
        )
    }));
    div()
        .id(id)
        .w_full()
        .max_h(px(180.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .p(px(space::HAIR))
        .rounded(px(radius::CONTROL))
        .bg(Colors::surface_input())
        .border_1()
        .border_color(Colors::border_subtle())
        .children(rows.into_iter().enumerate().map(|(index, (value, label))| {
            let selected = value.as_deref() == current;
            let rest = if selected {
                Colors::composite(Colors::surface_input(), Colors::state_selected())
            } else {
                Colors::with_alpha(Colors::surface_input(), 0.0)
            };
            let hover = Colors::composite(Colors::surface_input(), Colors::state_hover());
            let this = this.clone();
            div()
                .id((id, index))
                .flex_none()
                .px(px(space::BASE))
                .py(px(space::TIGHT))
                .rounded(px(radius::CONTROL_SM))
                .bg(rest)
                .hover(move |s| s.bg(hover))
                .cursor(gpui::CursorStyle::PointingHand)
                .text_size(px(typography::UI_SM))
                .text_color(if selected {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .truncate()
                .on_click(move |_, _, cx| {
                    let value = value.clone();
                    with_app(&this, cx, |app, _| pick(app, value));
                })
                .child(label)
        }))
}

fn field(label: &'static str, child: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(space::TIGHT))
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(child)
}

pub fn setup_view(app: &mut LiveStageApp, cx: &mut Context<LiveStageApp>) -> impl IntoElement {
    let this = cx.entity().downgrade();
    let draft = app.setup.draft.clone();
    let changed = draft != app.engine.session().audio;
    let recording = app.engine.session().recording.clone();
    let record_busy = app.engine.is_recording();

    let hosts: Vec<(Option<String>, String)> = std::iter::once((None, "Default".to_string()))
        .chain(app.setup.hosts.iter().map(|h| (Some(h.clone()), h.clone())))
        .collect();
    let host_row =
        fb_segmented_track().children(hosts.iter().enumerate().map(|(index, (value, label))| {
            let value = value.clone();
            let this = this.clone();
            fb_segment(
                ("setup-host", index),
                label.clone(),
                value == draft.host,
                position(index, hosts.len()),
                move |_, _, cx| {
                    let value = value.clone();
                    with_app(&this, cx, |app, _| {
                        app.setup.draft.host = value;
                        app.setup.draft.input_device = None;
                        app.setup.draft.output_device = None;
                        app.setup.rescan();
                    });
                },
            )
        }));

    let apply = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
            with_app(&this, cx, |app, _| {
                let settings = app.setup.draft.clone();
                app.run(Command::SetAudio { settings });
            });
        }
    };
    let revert = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
            with_app(&this, cx, |app, _| {
                app.setup.draft = app.engine.session().audio.clone();
            });
        }
    };
    let rescan = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
            with_app(&this, cx, |app, _| app.setup.rescan());
        }
    };
    let choose_folder = {
        let this = this.clone();
        move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
            let answer = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Record into".into()),
            });
            let this = this.clone();
            cx.spawn(async move |cx| {
                let Ok(Ok(Some(paths))) = answer.await else {
                    return;
                };
                let Some(folder) = paths.into_iter().next() else {
                    return;
                };
                let _ = this.update(cx, |app, cx| {
                    let mut settings = app.engine.session().recording.clone();
                    settings.folder = Some(folder);
                    app.run(Command::SetRecordSettings { settings });
                    cx.notify();
                });
            })
            .detach();
        }
    };

    fn set_record(app: &mut LiveStageApp, edit: impl FnOnce(&mut RecordSettings)) {
        let mut settings = app.engine.session().recording.clone();
        edit(&mut settings);
        app.run(Command::SetRecordSettings { settings });
    }

    div()
        .id("setup-view")
        .size_full()
        .flex()
        .flex_col()
        .gap(px(space::SECTION))
        .p(px(space::SECTION))
        .overflow_y_scroll()
        .child(fb_section_header("Audio interface"))
        .child(field("Audio system", host_row))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(space::SECTION))
                .child(div().flex_1().child(field(
                    "Inputs",
                    device_list(
                        "setup-input",
                        &app.setup.inputs,
                        draft.input_device.as_deref(),
                        &this,
                        |app, value| app.setup.draft.input_device = value,
                    ),
                )))
                .child(div().flex_1().child(field(
                    "Outputs",
                    device_list(
                        "setup-output",
                        &app.setup.outputs,
                        draft.output_device.as_deref(),
                        &this,
                        |app, value| app.setup.draft.output_device = value,
                    ),
                ))),
        )
        .child(field(
            "Sample rate",
            segments("setup-rate", &RATES, draft.sample_rate, &this, |app, v| {
                app.setup.draft.sample_rate = v
            }),
        ))
        .child(field(
            "Buffer (frames)",
            segments(
                "setup-buffer",
                &BUFFERS,
                draft.buffer_frames,
                &this,
                |app, v| app.setup.draft.buffer_frames = v,
            ),
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(space::BASE))
                .child(fb_button(
                    "setup-apply",
                    "Apply",
                    FbButtonKind::Primary,
                    changed && !record_busy,
                    apply,
                ))
                .child(fb_button(
                    "setup-revert",
                    "Revert",
                    FbButtonKind::Default,
                    changed,
                    revert,
                ))
                .child(fb_button(
                    "setup-rescan",
                    "Rescan devices",
                    FbButtonKind::Ghost,
                    true,
                    rescan,
                )),
        )
        .when(record_busy, |this| {
            this.child(
                div()
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_muted())
                    .child("Stop recording to change the interface or the recorder."),
            )
        })
        .child(fb_section_header("Recording"))
        .child(field(
            "Folder (each take gets its own dated folder inside)",
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .child(recording.resolved_folder().display().to_string()),
                )
                .child(fb_button(
                    "setup-record-folder",
                    "Choose…",
                    FbButtonKind::Default,
                    !record_busy,
                    choose_folder,
                )),
        ))
        .child(field(
            "Format",
            segments(
                "setup-format",
                &[(RecordFormat::Wav, "WAV"), (RecordFormat::Flac, "FLAC")],
                recording.format,
                &this,
                |app, v| set_record(app, |s| s.format = v),
            ),
        ))
        .child(field(
            "Bit depth",
            segments(
                "setup-bits",
                &[(16u16, "16-bit"), (24, "24-bit"), (32, "32-bit float")],
                recording.bit_depth,
                &this,
                |app, v| set_record(app, |s| s.bit_depth = v),
            ),
        ))
        .child(field(
            "Channels record",
            segments(
                "setup-tap",
                &[
                    (RecordTap::Input, "The input (clean multitrack)"),
                    (RecordTap::PostInserts, "After the inserts"),
                ],
                recording.tap,
                &this,
                |app, v| set_record(app, |s| s.tap = v),
            ),
        ))
}
