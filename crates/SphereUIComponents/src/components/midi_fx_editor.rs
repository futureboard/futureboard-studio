//! The editor window of one MIDI FX device: every parameter of the device,
//! drawn from its description in `sphere_midi_fx` (no per-device layout to
//! keep in step with the engine).
//!
//! Values are read from the track each time the window draws, so the editor
//! never disagrees with the project (an undo, a sync); a change goes to the
//! owner through `on_param`, which stores it and sends it to the running
//! device.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};
use sphere_midi_fx::{DeviceKind, ParamDesc, ParamKind, Unit};

use crate::components::controls::{
    fb_checkbox, fb_form_row, fb_segment, fb_segmented_track, fb_stepper_button, FbSegment,
};
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::timeline::timeline_state::MidiFxSlotState;
use crate::components::title_bar::external_window_titlebar;
use crate::i18n::I18n;
use crate::theme::{space, typography, Colors};

pub const MIDI_FX_EDITOR_WIDTH: f32 = 560.0;
pub const MIDI_FX_EDITOR_HEIGHT: f32 = 400.0;

/// Choices with this many options or fewer are a segmented row; more get a
/// stepper.
const SEGMENTED_MAX: usize = 4;
const KNOB_SIZE: f32 = 40.0;
const KNOB_CELL: f32 = 92.0;
/// A knob whose setting has no effect right now.
const INACTIVE_OPACITY: f32 = 0.4;

pub type ReadSlot = Arc<dyn Fn(&App) -> Option<MidiFxSlotState> + 'static>;
pub type OnParam = Arc<dyn Fn(&str, f32, &mut Window, &mut App) + 'static>;

pub struct MidiFxEditor {
    title: String,
    kind: DeviceKind,
    i18n: I18n,
    read: ReadSlot,
    on_param: OnParam,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
}

impl MidiFxEditor {
    pub fn new(
        title: String,
        kind: DeviceKind,
        i18n: I18n,
        read: ReadSlot,
        on_param: OnParam,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
    ) -> Self {
        Self {
            title,
            kind,
            i18n,
            read,
            on_param,
            on_close,
        }
    }
}

/// `value` as the editor shows it.
pub fn format_value(desc: &ParamDesc, value: f32, i18n: I18n) -> String {
    let value = desc.clamp(value);
    match desc.kind {
        ParamKind::Choice(labels) => labels
            .get(value as usize)
            .map(|key| i18n.tr(key))
            .unwrap_or_default(),
        ParamKind::Toggle => i18n.tr(if value >= 0.5 {
            "midi-fx.value.on"
        } else {
            "midi-fx.value.off"
        }),
        _ => match desc.unit {
            Unit::Percent => format!("{value:.0}%"),
            Unit::Semitones => format!("{:+} st", value as i32),
            Unit::Milliseconds => format!("{value:.0} ms"),
            Unit::Note => crate::components::piano_roll::note_name(value as i32),
            Unit::Midi | Unit::None => {
                if desc.kind == ParamKind::Integer {
                    format!("{}", value as i32)
                } else {
                    format!("{value:.1}")
                }
            }
        },
    }
}

fn position(index: usize, count: usize) -> FbSegment {
    match (index, count) {
        (_, 1) => FbSegment::Only,
        (0, _) => FbSegment::First,
        (i, n) if i + 1 == n => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

impl MidiFxEditor {
    fn choice_row(
        &self,
        on_param: &OnParam,
        desc: &'static ParamDesc,
        value: f32,
    ) -> gpui::AnyElement {
        let ParamKind::Choice(labels) = desc.kind else {
            return div().into_any_element();
        };
        let index = desc.clamp(value) as usize;
        let label = self.i18n.tr(desc.label);
        if labels.len() <= SEGMENTED_MAX {
            let count = labels.len();
            let track = fb_segmented_track().children(labels.iter().enumerate().map(|(i, key)| {
                let on_param = on_param.clone();
                div().flex_1().min_w_0().child(fb_segment(
                    (desc.id, i),
                    self.i18n.tr(key),
                    i == index,
                    position(i, count),
                    move |_, window, cx| on_param(desc.id, i as f32, window, cx),
                ))
            }));
            return fb_form_row(label, track).into_any_element();
        }
        // Many options: step through them.
        let (prev, next) = (on_param.clone(), on_param.clone());
        let last = labels.len() - 1;
        let stepper = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .child(fb_stepper_button(
                gpui::SharedString::from(format!("{}-prev", desc.id)),
                "-",
                move |_, window, cx| prev(desc.id, index.saturating_sub(1) as f32, window, cx),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_center()
                    .truncate()
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_primary())
                    .child(format_value(desc, value, self.i18n)),
            )
            .child(fb_stepper_button(
                gpui::SharedString::from(format!("{}-next", desc.id)),
                "+",
                move |_, window, cx| next(desc.id, (index + 1).min(last) as f32, window, cx),
            ));
        fb_form_row(label, stepper).into_any_element()
    }

    /// An on/off parameter: a checkbox (the latched toggle is reserved for
    /// track states such as mute and solo).
    fn toggle_row(
        &self,
        on_param: &OnParam,
        desc: &'static ParamDesc,
        value: f32,
    ) -> gpui::AnyElement {
        let on = value >= 0.5;
        let on_param = on_param.clone();
        fb_checkbox(
            gpui::SharedString::from(format!("{}-toggle", desc.id)),
            self.i18n.tr(desc.label),
            on,
            true,
            move |_, window, cx| on_param(desc.id, if on { 0.0 } else { 1.0 }, window, cx),
        )
        .into_any_element()
    }

    /// A numeric parameter as a knob. `active` is false while the device's
    /// other settings make it have no effect: it stays editable (to set it up
    /// ahead) but is dimmed.
    fn knob_cell(
        &self,
        on_param: &OnParam,
        desc: &'static ParamDesc,
        value: f32,
        active: bool,
    ) -> gpui::AnyElement {
        let on_param = on_param.clone();
        let change =
            move |v: &f32, window: &mut Window, cx: &mut App| on_param(desc.id, *v, window, cx);
        let id = gpui::SharedString::from(format!("{}-knob", desc.id));
        let knob = if desc.min < 0.0 {
            knob_bipolar(
                id,
                value,
                desc.min,
                desc.max,
                KNOB_SIZE,
                Colors::accent_primary(),
                None,
                desc.default,
                change,
            )
            .into_any_element()
        } else {
            knob_with_default(
                id,
                value,
                desc.min,
                desc.max,
                KNOB_SIZE,
                Colors::accent_primary(),
                desc.default,
                change,
            )
            .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(space::HAIR))
            .w(px(KNOB_CELL))
            .when(!active, |this| this.opacity(INACTIVE_OPACITY))
            .child(knob)
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_primary())
                    .child(format_value(desc, value, self.i18n)),
            )
            .child(
                div()
                    .w_full()
                    .text_center()
                    .truncate()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_muted())
                    .child(self.i18n.tr(desc.label)),
            )
            .into_any_element()
    }
}

impl Render for MidiFxEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let slot = (self.read)(cx);
        let on_close = self.on_close.clone();
        let titlebar = external_window_titlebar(
            self.title.clone(),
            "midi-fx-editor-close",
            move |window, cx| {
                on_close(window, cx);
                window.remove_window();
            },
        );
        let frame = div()
            .flex()
            .flex_col()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .child(titlebar);
        let Some(slot) = slot else {
            // The slot was removed while the editor was open.
            return frame.child(
                div()
                    .p(px(space::SECTION))
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_muted())
                    .child(self.i18n.tr("midi-fx.editor.removed")),
            );
        };

        // Every change redraws this window: its values come from the track,
        // which the owner has just updated.
        let on_param: OnParam = {
            let inner = self.on_param.clone();
            let me = cx.entity().downgrade();
            Arc::new(
                move |id: &str, value: f32, window: &mut Window, cx: &mut App| {
                    inner(id, value, window, cx);
                    let _ = me.update(cx, |_, cx| cx.notify());
                },
            )
        };
        let params = self.kind.params();
        let mut rows = Vec::new();
        let mut knobs = Vec::new();
        for desc in params {
            let value = slot.param(desc.id).unwrap_or(desc.default);
            match desc.kind {
                ParamKind::Choice(_) => rows.push(self.choice_row(&on_param, desc, value)),
                ParamKind::Toggle => rows.push(self.toggle_row(&on_param, desc, value)),
                ParamKind::Continuous | ParamKind::Integer => {
                    let active = self.kind.param_active(desc.id, |id| {
                        let default = self.kind.param_index(id).map_or(0.0, |i| params[i].default);
                        slot.param(id).unwrap_or(default)
                    });
                    knobs.push(self.knob_cell(&on_param, desc, value, active))
                }
            }
        }
        let caption = self.i18n.tr(if self.kind.is_generator() {
            "midi-fx.editor.generator-caption"
        } else {
            "midi-fx.editor.effect-caption"
        });

        frame.child(
            div()
                .id("midi-fx-editor-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap(px(space::BASE))
                .p(px(space::SECTION))
                .overflow_y_scroll()
                .bg(Colors::surface_base())
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(caption),
                )
                .when(!slot.enabled, |this| {
                    this.child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_faint())
                            .child(self.i18n.tr("midi-fx.editor.bypassed")),
                    )
                })
                .children(rows)
                .when(!knobs.is_empty(), |this| {
                    this.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(space::BASE))
                            .pt(px(space::SNUG))
                            .children(knobs),
                    )
                }),
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub fn open_midi_fx_editor(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    title: String,
    kind: DeviceKind,
    i18n: I18n,
    read: ReadSlot,
    on_param: OnParam,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
    cx: &mut App,
) -> Result<WindowHandle<MidiFxEditor>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(MIDI_FX_EDITOR_WIDTH), px(MIDI_FX_EDITOR_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);
    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|_| MidiFxEditor::new(title, kind, i18n, read, on_param, on_close))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_format_by_kind_and_unit() {
        let en = I18n::new("en");
        let semis = &DeviceKind::Transpose.params()[0];
        assert_eq!(format_value(semis, 7.0, en), "+7 st");
        assert_eq!(format_value(semis, -12.4, en), "-12 st");
        let arp = DeviceKind::Arpeggiator.params();
        let gate = arp.iter().find(|p| p.id == "gate").unwrap();
        assert_eq!(format_value(gate, 80.0, en), "80%");
        let note = DeviceKind::PatternGenerator
            .params()
            .iter()
            .find(|p| p.id == "note")
            .unwrap();
        assert_eq!(format_value(note, 60.0, en), "C4");
        let rate = arp.iter().find(|p| p.id == "rate").unwrap();
        assert_eq!(format_value(rate, 10.0, en), "1/16");
    }
}
