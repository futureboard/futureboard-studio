//! The patchbay: what goes into the mixer and what comes out of it.
//!
//! Two matrices, read the way a hardware patchbay is: rows are where signal
//! is needed, columns are the interface's sockets, and a lit cell is a
//! connection. Inputs take one socket per channel (two for a stereo one);
//! outputs take a pair per mix — the master, a bus, or a channel's direct out.

use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    WeakEntity, div, px,
};
use livestage_engine::{Command, Id, InputPatch, OutputPatch, PatchSource};
use sphere_ui_components::components::controls::{FbLatch, fb_section_header, fb_toggle};
use sphere_ui_components::theme::{Colors, radius, space, typography};

use crate::app::{LiveStageApp, send};

const CELL: f32 = 24.0;
const ROW_LABEL: f32 = 160.0;

fn cell(
    id: String,
    lit: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let rest = if lit {
        Colors::accent_primary()
    } else {
        Colors::surface_input()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(gpui::ElementId::Name(id.into()))
        .size(px(CELL - 4.0))
        .m(px(2.0))
        .rounded(px(radius::CONTROL_SM))
        .border_1()
        .border_color(if lit {
            Colors::accent_primary()
        } else {
            Colors::border_subtle()
        })
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(on_click)
}

fn header_row(labels: Vec<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .child(div().w(px(ROW_LABEL)).flex_none())
        .children(labels.into_iter().map(|label| {
            div()
                .w(px(CELL))
                .flex_none()
                .flex()
                .justify_center()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(label)
        }))
}

fn row_label(text: String) -> impl IntoElement {
    div()
        .w(px(ROW_LABEL))
        .flex_none()
        .pr(px(space::BASE))
        .truncate()
        .text_size(px(typography::UI_SM))
        .child(text)
}

fn input_matrix(app: &LiveStageApp, this: &WeakEntity<LiveStageApp>) -> impl IntoElement {
    let inputs = app.engine.input_channels() as u16;
    let mut rows = div()
        .flex()
        .flex_col()
        .child(header_row((1..=inputs).map(|i| i.to_string()).collect()));
    for channel in &app.engine.session().channels {
        let id: Id = channel.id;
        let patch = channel.input;
        let stereo = patch.right.is_some();
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .child(row_label(channel.name.clone()));
        for socket in 0..inputs {
            let lit = patch.left == Some(socket) || patch.right == Some(socket);
            let this = this.clone();
            row = row.child(cell(format!("in-{id}-{socket}"), lit, move |_, _, cx| {
                let input = if lit {
                    InputPatch::default()
                } else if stereo && socket + 1 < inputs {
                    InputPatch::stereo(socket, socket + 1)
                } else {
                    InputPatch::mono(socket)
                };
                send(&this, cx, Command::SetChannelInput { channel: id, input });
            }));
        }
        let toggle = {
            let this = this.clone();
            move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
                let input = match (patch.left, stereo) {
                    (Some(left), false) if left + 1 < inputs => InputPatch::stereo(left, left + 1),
                    (Some(left), true) => InputPatch::mono(left),
                    _ => patch,
                };
                send(&this, cx, Command::SetChannelInput { channel: id, input });
            }
        };
        row = row.child(div().pl(px(space::BASE)).child(fb_toggle(
            format!("in-stereo-{id}"),
            "ST",
            FbLatch::Monitor,
            stereo,
            16.0,
            toggle,
        )));
        rows = rows.child(row);
    }
    rows
}

/// The interface's outputs as the pairs a mix is patched to: 1/2, 3/4, … and
/// a last single output when the count is odd.
fn output_pairs(outputs: u16) -> Vec<(u16, Option<u16>)> {
    (0..outputs)
        .step_by(2)
        .map(|left| (left, (left + 1 < outputs).then_some(left + 1)))
        .collect()
}

fn output_matrix(app: &LiveStageApp, this: &WeakEntity<LiveStageApp>) -> impl IntoElement {
    let outputs = app.engine.output_channels() as u16;
    let pairs = output_pairs(outputs);
    let session = app.engine.session();
    let mut sources: Vec<(String, PatchSource)> = vec![("Master".to_string(), PatchSource::Master)];
    sources.extend(
        session
            .buses
            .iter()
            .map(|b| (b.name.clone(), PatchSource::Bus(b.id))),
    );
    sources.extend(
        session
            .channels
            .iter()
            .map(|c| (format!("{} (direct)", c.name), PatchSource::Channel(c.id))),
    );
    let patches = session.outputs.clone();
    let header: Vec<String> = pairs
        .iter()
        .map(|(l, r)| match r {
            Some(r) => format!("{}/{}", l + 1, r + 1),
            None => (l + 1).to_string(),
        })
        .collect();
    let mut rows = div().flex().flex_col().child(header_row(header));
    for (name, source) in sources {
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .child(row_label(name));
        for (left, right) in &pairs {
            let (left, right) = (*left, *right);
            let lit = patches
                .iter()
                .any(|p| p.source == source && p.left == left && p.right == right);
            let this = this.clone();
            let patches = patches.clone();
            row = row.child(cell(
                format!("out-{source:?}-{left}"),
                lit,
                move |_, _, cx| {
                    let mut next: Vec<OutputPatch> = patches
                        .iter()
                        .filter(|p| !(p.source == source && p.left == left && p.right == right))
                        .copied()
                        .collect();
                    if !lit {
                        next.push(OutputPatch {
                            source,
                            left,
                            right,
                        });
                    }
                    send(&this, cx, Command::SetOutputPatch { patches: next });
                },
            ));
        }
        rows = rows.child(row);
    }
    rows
}

pub fn patch_view(app: &mut LiveStageApp, cx: &mut Context<LiveStageApp>) -> impl IntoElement {
    let this = cx.entity().downgrade();
    let no_inputs = app.engine.input_channels() == 0;
    div()
        .id("patch-view")
        .size_full()
        .flex()
        .flex_col()
        .gap(px(space::SECTION))
        .p(px(space::SECTION))
        .overflow_scroll()
        .child(fb_section_header("Inputs → Channels"))
        .child(if no_inputs {
            div()
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_muted())
                .child("The interface has no inputs open. Pick an input device in Setup.")
                .into_any_element()
        } else {
            input_matrix(app, &this).into_any_element()
        })
        .child(fb_section_header("Mixes → Outputs"))
        .child(output_matrix(app, &this))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_pair_up_and_an_odd_one_stands_alone() {
        assert_eq!(output_pairs(4), vec![(0, Some(1)), (2, Some(3))]);
        assert_eq!(output_pairs(3), vec![(0, Some(1)), (2, None)]);
        assert!(output_pairs(0).is_empty());
    }
}
