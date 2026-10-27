use atomic_float::AtomicF32;
use nih_plug::prelude::util;
use nih_plug_gpui::gpui::prelude::*;
use nih_plug_gpui::gpui::{
    div, px, relative, rgb, App, Context, Entity, MouseButton, MouseDownEvent, Window,
};
use nih_plug_gpui::EditorContext;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::GainParams;

/// The range the peak meter displays, in decibels.
const METER_RANGE_DB: f32 = 60.0;

/// The gain plugin's editor. This is a regular GPUI view, the only plugin specific part is the
/// [`EditorContext`] it uses to read and write parameters.
pub struct GainEditor {
    context: EditorContext,
    params: Arc<GainParams>,
    peak_meter: Arc<AtomicF32>,
}

impl GainEditor {
    pub fn build(
        context: &EditorContext,
        params: Arc<GainParams>,
        peak_meter: Arc<AtomicF32>,
        cx: &mut App,
    ) -> Entity<Self> {
        let context = context.clone();

        cx.new(|_cx| GainEditor {
            context,
            params,
            peak_meter,
        })
    }

    /// Nudge the gain parameter by `delta` decibels. Parameter changes are always wrapped in a
    /// begin/end pair so the host knows they belong to a single gesture.
    fn nudge_gain(&self, delta_db: f32) {
        let setter = self.context.setter();
        let new_value = util::db_to_gain(util::gain_to_db(self.params.gain.value()) + delta_db);

        setter.begin_set_parameter(&self.params.gain);
        setter.set_parameter(&self.params.gain, new_value);
        setter.end_set_parameter(&self.params.gain);
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        delta_db: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex()
            .w(px(56.0))
            .h(px(28.0))
            .justify_center()
            .items_center()
            .rounded_md()
            .bg(rgb(0x3a3a3a))
            .hover(|style| style.bg(rgb(0x4a4a4a)))
            .cursor_pointer()
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event: &MouseDownEvent, _window, _cx| {
                    this.nudge_gain(delta_db);
                }),
            )
    }
}

impl Render for GainEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let gain_db = util::gain_to_db(self.params.gain.value());

        // To save resources a plugin should only compute this while the editor is open, which is
        // what the `is_open()` check in `Gain::process()` is for
        let peak_meter_db = util::gain_to_db(self.peak_meter.load(Ordering::Relaxed));
        let peak_meter_fraction =
            ((peak_meter_db + METER_RANGE_DB) / METER_RANGE_DB).clamp(0.0, 1.0);
        let peak_meter_label = if peak_meter_db > util::MINUS_INFINITY_DB {
            format!("{peak_meter_db:.1} dBFS")
        } else {
            String::from("-inf dBFS")
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_4()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xf0f0f0))
            .child(div().text_xl().child("Gain"))
            .child(div().child(format!("{gain_db:.2} dB")))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.button("gain-down", "-1 dB", -1.0, cx))
                    .child(self.button("gain-up", "+1 dB", 1.0, cx)),
            )
            .child(div().child(peak_meter_label))
            .child(
                // The meter is a fixed width track with a bar inside of it whose width follows the
                // measured level
                div()
                    .w_full()
                    .h(px(12.0))
                    .rounded_sm()
                    .bg(rgb(0x2c2c2c))
                    .child(
                        div()
                            .h_full()
                            .w(relative(peak_meter_fraction))
                            .rounded_sm()
                            .bg(rgb(0x4caf50)),
                    ),
            )
    }
}
