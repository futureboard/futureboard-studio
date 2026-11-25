//! Audio › Strip Silence: the settings window.
//!
//! A small separate window with the detection settings and Apply / Cancel.
//! It only edits settings; Apply hands them to the Studio (`on_apply`),
//! which analyses the clips off the UI thread and cuts them in one undo
//! step. Each value drags up or down to change, and a double-click types
//! one. Enter applies and Escape cancels, unless a value is being typed.
//!
//! The settings last for the session: the next Strip Silence opens with
//! the ones last applied.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, fb_form_row, FbButtonKind};
use crate::components::inspector::{inspector_numeric_stepper, route_numeric_entry_key};
use crate::components::timeline::timeline_state::StripSilenceParams;
use crate::components::title_bar::external_window_titlebar_compact;
use crate::theme::Colors;

const TITLE: &str = "Strip Silence";
const WINDOW_W: f32 = 340.0;
const WINDOW_H: f32 = 330.0;
/// Width of a value field.
const FIELD_W: f32 = 112.0;

/// Called with the settings when Apply is pressed.
pub type StripSilenceApplyCb = Arc<dyn Fn(StripSilenceParams, &mut App) + 'static>;

thread_local! {
    /// The settings last applied this session.
    static LAST_PARAMS: Cell<Option<StripSilenceParams>> = const { Cell::new(None) };
    /// The open window, if any: a second Strip Silence brings it forward.
    static OPEN_WINDOW: RefCell<Option<WindowHandle<StripSilenceWindow>>> =
        const { RefCell::new(None) };
}

/// The settings the next Strip Silence opens with.
pub fn last_strip_silence_params() -> StripSilenceParams {
    LAST_PARAMS
        .with(|last| last.get())
        .unwrap_or_default()
        .clamped()
}

pub struct StripSilenceWindow {
    params: StripSilenceParams,
    /// What Apply acts on, as the footer says it ("3 clips").
    target: String,
    on_apply: StripSilenceApplyCb,
    focus: FocusHandle,
}

impl StripSilenceWindow {
    fn new(target: String, on_apply: StripSilenceApplyCb, cx: &mut Context<Self>) -> Self {
        Self {
            params: last_strip_silence_params(),
            target,
            on_apply,
            focus: cx.focus_handle(),
        }
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let params = self.params.clamped();
        LAST_PARAMS.with(|last| last.set(Some(params)));
        let on_apply = self.on_apply.clone();
        // The Studio's work runs after this window is gone, not inside its
        // update.
        cx.defer(move |cx| on_apply(params, cx));
        window.remove_window();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A value being typed takes its keys first: Enter writes it, Escape
        // drops it, and neither closes the window.
        if route_numeric_entry_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" | "numpad_enter" => {
                cx.stop_propagation();
                self.apply(window, cx);
            }
            "escape" => {
                cx.stop_propagation();
                window.remove_window();
            }
            _ => {}
        }
    }

    /// One labelled value: drag to change, double-click to type.
    #[allow(clippy::too_many_arguments)]
    fn field(
        &self,
        id: &'static str,
        label: &'static str,
        value: f32,
        (min, max): (f32, f32),
        step: f64,
        display: String,
        set: fn(&mut StripSilenceParams, f32),
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let this: WeakEntity<Self> = cx.entity().downgrade();
        fb_form_row(
            label,
            div()
                .flex()
                .flex_row()
                .justify_end()
                .child(div().w(px(FIELD_W)).child(inspector_numeric_stepper(
                    id,
                    value as f64,
                    display,
                    min as f64,
                    max as f64,
                    step,
                    false,
                    move |next, _window, cx| {
                        let _ = this.update(cx, |dialog, cx| {
                            set(&mut dialog.params, next as f32);
                            dialog.params = dialog.params.clamped();
                            cx.notify();
                        });
                    },
                ))),
        )
        .into_any_element()
    }
}

fn ms(value: f32) -> String {
    format!("{value:.0} ms")
}

impl Render for StripSilenceWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::theme::{space, typography};
        let p = self.params;
        let fields = vec![
            self.field(
                "strip-silence-threshold",
                "Threshold",
                p.threshold_db,
                StripSilenceParams::THRESHOLD_RANGE,
                0.5,
                format!("{:.1} dB", p.threshold_db),
                |p, v| p.threshold_db = (v * 2.0).round() / 2.0,
                cx,
            ),
            self.field(
                "strip-silence-min-silence",
                "Min silence",
                p.min_silence_ms,
                StripSilenceParams::MIN_SILENCE_RANGE,
                5.0,
                ms(p.min_silence_ms),
                |p, v| p.min_silence_ms = v.round(),
                cx,
            ),
            self.field(
                "strip-silence-min-clip",
                "Min clip length",
                p.min_clip_ms,
                StripSilenceParams::MIN_CLIP_RANGE,
                5.0,
                ms(p.min_clip_ms),
                |p, v| p.min_clip_ms = v.round(),
                cx,
            ),
            self.field(
                "strip-silence-pre",
                "Pre-attack padding",
                p.pre_attack_ms,
                StripSilenceParams::PRE_ATTACK_RANGE,
                1.0,
                ms(p.pre_attack_ms),
                |p, v| p.pre_attack_ms = v.round(),
                cx,
            ),
            self.field(
                "strip-silence-post",
                "Post-release padding",
                p.post_release_ms,
                StripSilenceParams::POST_RELEASE_RANGE,
                5.0,
                ms(p.post_release_ms),
                |p, v| p.post_release_ms = v.round(),
                cx,
            ),
            self.field(
                "strip-silence-fade",
                "Fade",
                p.fade_ms,
                StripSilenceParams::FADE_RANGE,
                1.0,
                ms(p.fade_ms),
                |p, v| p.fade_ms = v.round(),
                cx,
            ),
        ];

        let reset = cx.entity().downgrade();
        let accept = cx.entity().downgrade();
        let is_default = p == StripSilenceParams::default();

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .capture_key_down(cx.listener(Self::on_key))
            // Focus on a 0×0 child, not the root: a focused root's hitbox
            // wins over the titlebar's drag area on Windows.
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus))
            .when(!crate::platform_chrome::native_tool_window_frame(), |el| {
                el.child(external_window_titlebar_compact(
                    TITLE,
                    "strip-silence-close",
                    |window, _cx| window.remove_window(),
                ))
            })
            .child(
                div()
                    .id("strip-silence-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(space::HAIR))
                    .px(px(space::SECTION))
                    .pt(px(space::LOOSE))
                    .pb(px(space::BASE))
                    .overflow_y_scroll()
                    .bg(Colors::surface_base())
                    .children(fields),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .px(px(space::LOOSE))
                    .py(px(space::BASE))
                    .border_t(px(1.0))
                    .border_color(Colors::panel_border())
                    .bg(Colors::surface_panel())
                    .child(fb_button(
                        "strip-silence-reset",
                        "Reset",
                        FbButtonKind::Default,
                        !is_default,
                        move |_, _w, cx| {
                            let _ = reset.update(cx, |dialog, cx| {
                                dialog.params = StripSilenceParams::default();
                                cx.notify();
                            });
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .justify_center()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(div().truncate().child(self.target.clone())),
                    )
                    .child(fb_button(
                        "strip-silence-cancel",
                        "Cancel",
                        FbButtonKind::Default,
                        true,
                        |_, window, _cx| window.remove_window(),
                    ))
                    .child(fb_button(
                        "strip-silence-apply",
                        "Apply",
                        FbButtonKind::Primary,
                        true,
                        move |_, window, cx| {
                            let _ = accept.update(cx, |dialog, cx| dialog.apply(window, cx));
                        },
                    )),
            )
    }
}

/// Open the Strip Silence window centred on `owner_bounds`, or bring the
/// open one forward (it keeps the clips it was opened for). `target` names
/// what Apply acts on ("3 clips").
pub fn open_strip_silence_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    target: String,
    on_apply: StripSilenceApplyCb,
    cx: &mut App,
) -> Result<(), String> {
    let open = OPEN_WINDOW.with(|open| open.borrow().clone());
    if let Some(handle) = open {
        let raised = handle.update(cx, |dialog, window, cx| {
            dialog.target = target.clone();
            dialog.on_apply = on_apply.clone();
            window.activate_window();
            cx.notify();
        });
        if raised.is_ok() {
            return Ok(());
        }
    }

    let (w, mut h) = (WINDOW_W, WINDOW_H);
    // Windows: a native Win32 frame. Its caption sits outside the client
    // bounds, so the drawn titlebar's height comes off the requested size.
    let native_frame = crate::platform_chrome::native_tool_window_frame();
    if native_frame {
        h -= crate::platform_chrome::PlatformChromePolicy::current().titlebar_height_px;
    }
    let window_bounds =
        crate::window_position::centered_window_bounds(owner_bounds, size(px(w), px(h)), cx);
    let mut options = if native_frame {
        crate::platform_chrome::native_tool_window_options_partial(TITLE)
    } else {
        crate::platform_chrome::external_dialog_window_options_partial()
    };
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = if native_frame {
        WindowBackgroundAppearance::Opaque
    } else {
        WindowBackgroundAppearance::Transparent
    };
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);
    let handle = cx
        .open_window(options, move |window, cx| {
            let view = cx.new(|cx| StripSilenceWindow::new(target, on_apply, cx));
            let focus = view.read(cx).focus.clone();
            window.focus(&focus, cx);
            view
        })
        .map_err(|e| e.to_string())?;
    OPEN_WINDOW.with(|open| *open.borrow_mut() = Some(handle));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readouts_carry_their_units() {
        assert_eq!(ms(250.0), "250 ms");
        assert_eq!(ms(4.6), "5 ms");
    }

    #[test]
    fn the_first_open_uses_the_defaults() {
        let params = last_strip_silence_params();
        assert_eq!(params.threshold_db, -48.0);
        assert_eq!(params.min_silence_ms, 250.0);
        assert_eq!(params.min_clip_ms, 80.0);
        assert_eq!(params.pre_attack_ms, 10.0);
        assert_eq!(params.post_release_ms, 60.0);
        assert_eq!(params.fade_ms, 5.0);
    }
}
