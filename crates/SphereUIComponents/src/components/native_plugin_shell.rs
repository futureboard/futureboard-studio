//! The shell a built-in plug-in's **native** editor sits in.
//!
//! Built-in plug-ins have two kinds of editor (see
//! `SpherePluginHost::BuiltinEditorKind`): a web bundle hosted in CEF by
//! `builtin_plugin_editor_window`, or a native GPUI view drawn by Studio.
//! This module is the frame every native one shares, so they all read as one
//! family with the CEF-hosted editors:
//!
//! * the external-window title bar;
//! * an identity strip — the plug-in, the track and insert it is editing, and
//!   its output meter, polled from the plug-in host's shared region;
//! * the plug-in's own content below.
//!
//! The shell owns no plug-in state. The editor window that uses it keeps the
//! instance key and host ops, and drives [`ShellMeter::poll`] from its timer.

use gpui::{AnyElement, App, IntoElement, ParentElement, Styled, Window, div, px, relative};

use crate::components::builtin_plugin_editor_window::{BuiltinMeterSource, PluginInstanceKey};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{Colors, radius, space, typography};

/// How often a native editor polls its insert's meter: about 30 Hz, the rate
/// the CEF editors' telemetry pump runs at.
pub const SHELL_METER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
const METER_W: f32 = 132.0;
const METER_H: f32 = 6.0;

/// What Studio asks of every native editor window, whichever plug-in it
/// edits: take another insert of the same plug-in, keeping the OS window.
pub trait NativeBuiltinEditor: gpui::Render + Sized {
    fn rebind_insert(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: crate::components::builtin_plugin_editor_window::BuiltinEditorHostOps,
        cx: &mut gpui::Context<Self>,
    );
}

/// The insert the editor is bound to, as the strip names it.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellIdentity {
    pub plugin_name: String,
    pub track_name: String,
    /// 1-based slot on the track's insert chain.
    pub insert_number: usize,
}

/// The output meter in the strip: peak and RMS, linear.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ShellMeter {
    pub peak: f32,
    pub rms: f32,
    pub clip: bool,
}

impl ShellMeter {
    /// Reads the insert's latest meter frame. Returns whether the reading
    /// moved enough to be worth a redraw, so an idle editor stays idle.
    pub fn poll(&mut self, source: Option<&BuiltinMeterSource>, key: &PluginInstanceKey) -> bool {
        let next = source
            .and_then(|source| source(key))
            .map(|frame| ShellMeter {
                peak: frame.out_peak.clamp(0.0, 2.0),
                rms: frame.out_rms.clamp(0.0, 2.0),
                clip: frame.out_clip,
            })
            .unwrap_or_default();
        let moved = (next.peak - self.peak).abs() > 0.004
            || (next.rms - self.rms).abs() > 0.004
            || next.clip != self.clip;
        if moved {
            *self = next;
        }
        moved
    }
}

/// Meter position for a linear level: -60 dBFS at the left, 0 dBFS at the
/// right, so quiet material still moves the bar.
fn meter_fraction(level: f32) -> f32 {
    if level <= 1.0e-6 {
        return 0.0;
    }
    let db = 20.0 * level.log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn meter(meter: ShellMeter) -> AnyElement {
    let peak = meter_fraction(meter.peak);
    let rms = meter_fraction(meter.rms);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child("OUT"),
        )
        .child(
            // Square: a meter fill's top pixel is the value (DESIGN.md).
            div()
                .relative()
                .w(px(METER_W))
                .h(px(METER_H))
                .bg(Colors::meter_bg())
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(relative(rms))
                        .bg(Colors::meter_low()),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .h_full()
                        .w(px(2.0))
                        .left(relative(peak))
                        .bg(if meter.clip {
                            Colors::meter_high()
                        } else if peak > 0.9 {
                            Colors::meter_mid()
                        } else {
                            Colors::text_secondary()
                        }),
                ),
        )
        .into_any_element()
}

/// The identity strip under the title bar.
fn identity_strip(identity: &ShellIdentity, level: ShellMeter) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::BASE))
        .h(px(30.0))
        .px(px(space::SECTION))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_titlebar())
        .child(
            div()
                .px(px(space::SNUG))
                .py(px(1.0))
                .rounded(px(radius::CONTROL_SM))
                .bg(Colors::surface_badge())
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child("NATIVE"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(format!(
                    "{} · Insert {}",
                    identity.track_name, identity.insert_number
                )),
        )
        .child(meter(level))
        .into_any_element()
}

/// The whole shell around `content`.
pub fn native_plugin_shell(
    close_id: &'static str,
    identity: &ShellIdentity,
    level: ShellMeter,
    on_close: impl Fn(&mut Window, &mut App) + Clone + 'static,
    content: AnyElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .font(crate::theme::ui_font())
        .bg(Colors::surface_window())
        .overflow_hidden()
        .child(external_window_titlebar(
            identity.plugin_name.clone(),
            close_id,
            on_close,
        ))
        .child(identity_strip(identity, level))
        .child(div().flex_1().min_h(px(0.0)).relative().child(content))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_spans_sixty_decibels() {
        assert_eq!(meter_fraction(0.0), 0.0);
        assert_eq!(meter_fraction(1.0), 1.0);
        assert!((meter_fraction(0.001) - 0.0).abs() < 1.0e-6);
        assert!((meter_fraction(0.0316) - 0.5).abs() < 0.01);
    }

    #[test]
    fn an_idle_meter_asks_for_no_redraw() {
        let key = PluginInstanceKey {
            track_id: "t".into(),
            insert_id: "i".into(),
        };
        let mut level = ShellMeter::default();
        assert!(!level.poll(None, &key));
    }
}
