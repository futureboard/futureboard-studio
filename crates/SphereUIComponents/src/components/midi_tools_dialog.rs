//! The MIDI editor's tool windows: one small window per tool (Strum,
//! Humanize, Length, Velocity, Clean Up), opened from the editor toolbar's
//! Tools menu.
//!
//! A window only draws and edits settings. The notes, the live preview and
//! Accept / Reset / revert all belong to the [`PianoRoll`] that opened it
//! (`piano_roll::advanced_tools`): the window observes that editor and
//! redraws from it. Knobs drag vertically (Shift for fine, double-click to
//! reset), and the notes follow while they move.
//!
//! Enter accepts and Escape closes. Closing the window in any way but Accept
//! puts the notes back as they were.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled,
    Subscription, WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind,
};

use crate::components::controls::{
    fb_button, fb_checkbox, fb_segment, fb_segmented_track, fb_stepper_button, FbButtonKind,
    FbSegment,
};
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::piano_roll::advanced_tools::{
    AdvancedTool, AdvancedToolSettings, CLEANUP_SHORT_CHOICES,
};
use crate::components::piano_roll::PianoRoll;
use crate::components::title_bar::external_window_titlebar;
use crate::theme::Colors;

/// Knob diameter in the tool windows.
const KNOB_SIZE: f32 = 36.0;
/// Width of one knob cell (label, knob, value).
const CELL_W: f32 = 96.0;

pub struct MidiToolWindow {
    roll: WeakEntity<PianoRoll>,
    tool: AdvancedTool,
    focus: FocusHandle,
    _observe_roll: Subscription,
}

type Settings = AdvancedToolSettings;

impl MidiToolWindow {
    pub fn new(roll: Entity<PianoRoll>, tool: AdvancedTool, cx: &mut Context<Self>) -> Self {
        // The settings and the note count live on the editor, so any change
        // there redraws this window.
        let observe = cx.observe(&roll, |_, _, cx| cx.notify());
        Self {
            roll: roll.downgrade(),
            tool,
            focus: cx.focus_handle(),
            _observe_roll: observe,
        }
    }

    fn settings(&self, cx: &App) -> Option<Settings> {
        self.roll
            .upgrade()
            .map(|roll| *roll.read(cx).advanced_settings())
    }

    fn edit(roll: &WeakEntity<PianoRoll>, cx: &mut App, change: impl FnOnce(&mut Settings)) {
        if let Some(roll) = roll.upgrade() {
            roll.update(cx, |roll, cx| roll.update_advanced_settings(change, cx));
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(roll) = self.roll.upgrade() else {
            return;
        };
        let mods = event.keystroke.modifiers;
        let command = mods.control || mods.platform;
        match event.keystroke.key.as_str() {
            // Undo while a tool previews takes the preview back: it is the
            // last change on screen, and it is not in the history yet.
            "z" | "Z" if command && !mods.shift => {
                cx.stop_propagation();
                roll.update(cx, |roll, cx| roll.cancel_active_tool(cx));
            }
            "enter" => {
                cx.stop_propagation();
                roll.update(cx, |roll, cx| roll.accept_active_tool(cx));
            }
            "escape" => {
                cx.stop_propagation();
                roll.update(cx, |roll, cx| roll.cancel_active_tool(cx));
            }
            _ => {}
        }
    }

    // ── Controls ──────────────────────────────────────────────────────────

    /// A knob with its name above and its value below; `set` writes the
    /// knob's value back into the settings.
    #[allow(clippy::too_many_arguments)]
    fn knob_cell(
        &self,
        id: &'static str,
        label: &'static str,
        value: f32,
        min: f32,
        max: f32,
        default: f32,
        bipolar: bool,
        readout: String,
        set: fn(&mut Settings, f32),
    ) -> gpui::AnyElement {
        use crate::theme::{space, typography};
        let roll = self.roll.clone();
        let on_change = move |v: &f32, _w: &mut Window, cx: &mut App| {
            let v = *v;
            Self::edit(&roll, cx, |s| set(s, v));
        };
        let accent = Colors::accent_primary();
        let control = if bipolar {
            knob_bipolar(
                id, value, min, max, KNOB_SIZE, accent, None, default, on_change,
            )
            .into_any_element()
        } else {
            knob_with_default(id, value, min, max, KNOB_SIZE, accent, default, on_change)
                .into_any_element()
        };
        div()
            .w(px(CELL_W))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(space::TIGHT))
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_muted())
                    .child(label),
            )
            .child(control)
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .child(readout),
            )
            .into_any_element()
    }

    fn check(
        &self,
        id: &'static str,
        label: &'static str,
        checked: bool,
        toggle: fn(&mut Settings),
    ) -> gpui::AnyElement {
        let roll = self.roll.clone();
        fb_checkbox(id, label, checked, true, move |_, _w, cx| {
            Self::edit(&roll, cx, toggle)
        })
        .into_any_element()
    }

    fn choices<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[(T, &'static str)],
        current: T,
        set: fn(&mut Settings, T),
    ) -> gpui::AnyElement {
        let n = options.len();
        let mut track = fb_segmented_track();
        for (i, (value, label)) in options.iter().copied().enumerate() {
            let position = match (i, n) {
                (_, 1) => FbSegment::Only,
                (0, _) => FbSegment::First,
                (i, n) if i + 1 == n => FbSegment::Last,
                _ => FbSegment::Middle,
            };
            let roll = self.roll.clone();
            track = track.child(fb_segment(
                (id, i),
                label,
                current == value,
                position,
                move |_, _w, cx| Self::edit(&roll, cx, |s| set(s, value)),
            ));
        }
        track.into_any_element()
    }

    /// The random seed: a number with ‹ › to roll a different result.
    fn seed(&self, id: &'static str, value: u32, set: fn(&mut Settings, u32)) -> gpui::AnyElement {
        use crate::theme::{space, typography};
        let down = self.roll.clone();
        let up = self.roll.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child("Seed"),
            )
            .child(fb_stepper_button((id, 0usize), "‹", move |_, _w, cx| {
                Self::edit(&down, cx, |s| set(s, value.saturating_sub(1).max(1)))
            }))
            .child(
                div()
                    .min_w(px(28.0))
                    .flex()
                    .justify_center()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .child(value.to_string()),
            )
            .child(fb_stepper_button((id, 1usize), "›", move |_, _w, cx| {
                Self::edit(&up, cx, |s| set(s, value.saturating_add(1)))
            }))
            .into_any_element()
    }

    // ── Tools ─────────────────────────────────────────────────────────────

    fn body(&self, s: Settings) -> Vec<gpui::AnyElement> {
        match self.tool {
            AdvancedTool::Strum => self.strum(s),
            AdvancedTool::Humanize => self.humanize(s),
            AdvancedTool::Length => self.length(s),
            AdvancedTool::Velocity => self.velocity(s),
            AdvancedTool::CleanUp => self.cleanup(s),
        }
    }

    fn strum(&self, s: Settings) -> Vec<gpui::AnyElement> {
        let st = s.strum;
        vec![
            knob_row(vec![
                self.knob_cell(
                    "midi-tool-strum-spread",
                    "SPREAD",
                    st.spread_ms,
                    0.0,
                    200.0,
                    20.0,
                    false,
                    format!("{:.0} ms", st.spread_ms),
                    |s, v| s.strum.spread_ms = v.round(),
                ),
                self.knob_cell(
                    "midi-tool-strum-tension",
                    "TENSION",
                    st.tension,
                    -1.0,
                    1.0,
                    0.0,
                    true,
                    tension_label(st.tension),
                    |s, v| s.strum.tension = (v * 100.0).round() / 100.0,
                ),
                self.knob_cell(
                    "midi-tool-strum-velocity",
                    "VELOCITY",
                    st.velocity_fade as f32,
                    -60.0,
                    60.0,
                    0.0,
                    true,
                    signed(st.velocity_fade),
                    |s, v| s.strum.velocity_fade = v.round() as i32,
                ),
            ]),
            self.choices(
                "midi-tool-strum-dir",
                &[(true, "Up"), (false, "Down")],
                st.up,
                |s, v| s.strum.up = v,
            ),
            self.check(
                "midi-tool-strum-alternate",
                "Alternate direction",
                st.alternate,
                |s| s.strum.alternate = !s.strum.alternate,
            ),
            self.check(
                "midi-tool-strum-ends",
                "Keep note ends",
                st.keep_ends,
                |s| s.strum.keep_ends = !s.strum.keep_ends,
            ),
        ]
    }

    fn humanize(&self, s: Settings) -> Vec<gpui::AnyElement> {
        let h = s.humanize;
        vec![
            knob_row(vec![
                self.knob_cell(
                    "midi-tool-hum-timing",
                    "TIMING",
                    h.timing_ms,
                    0.0,
                    60.0,
                    10.0,
                    false,
                    format!("± {:.0} ms", h.timing_ms),
                    |s, v| s.humanize.timing_ms = v.round(),
                ),
                self.knob_cell(
                    "midi-tool-hum-velocity",
                    "VELOCITY",
                    h.velocity as f32,
                    0.0,
                    40.0,
                    8.0,
                    false,
                    format!("± {}", h.velocity),
                    |s, v| s.humanize.velocity = v.round().clamp(0.0, 40.0) as u8,
                ),
                self.knob_cell(
                    "midi-tool-hum-length",
                    "LENGTH",
                    h.length_pct,
                    0.0,
                    50.0,
                    0.0,
                    false,
                    format!("± {:.0}%", h.length_pct),
                    |s, v| s.humanize.length_pct = v.round(),
                ),
            ]),
            self.check(
                "midi-tool-hum-chords",
                "Keep chords together",
                h.keep_chords,
                |s| s.humanize.keep_chords = !s.humanize.keep_chords,
            ),
            self.seed("midi-tool-hum-seed", s.humanize_seed, |s, v| {
                s.humanize_seed = v
            }),
        ]
    }

    fn length(&self, s: Settings) -> Vec<gpui::AnyElement> {
        let l = s.length;
        let first = if l.legato {
            self.knob_cell(
                "midi-tool-len-gap",
                "GAP",
                l.gap_ms,
                -200.0,
                200.0,
                0.0,
                true,
                gap_label(l.gap_ms),
                |s, v| s.length.gap_ms = v.round(),
            )
        } else {
            self.knob_cell(
                "midi-tool-len-multiply",
                "MULTIPLY",
                l.multiply,
                0.25,
                4.0,
                1.0,
                false,
                format!("× {:.2}", l.multiply),
                |s, v| s.length.multiply = (v * 20.0).round() / 20.0,
            )
        };
        vec![
            knob_row(vec![
                first,
                self.knob_cell(
                    "midi-tool-len-variation",
                    "VARIATION",
                    l.variation_pct,
                    0.0,
                    50.0,
                    0.0,
                    false,
                    format!("± {:.0}%", l.variation_pct),
                    |s, v| s.length.variation_pct = v.round(),
                ),
            ]),
            self.check(
                "midi-tool-len-legato",
                "Legato: each note runs to the next",
                l.legato,
                |s| s.length.legato = !s.length.legato,
            ),
            self.seed("midi-tool-len-seed", s.length_seed, |s, v| {
                s.length_seed = v
            }),
        ]
    }

    fn velocity(&self, s: Settings) -> Vec<gpui::AnyElement> {
        let v = s.velocity;
        vec![
            knob_row(vec![
                self.knob_cell(
                    "midi-tool-vel-offset",
                    "LEVEL",
                    v.offset as f32,
                    -64.0,
                    64.0,
                    0.0,
                    true,
                    signed(v.offset),
                    |s, x| s.velocity.offset = x.round() as i32,
                ),
                self.knob_cell(
                    "midi-tool-vel-compress",
                    "RANGE",
                    v.compress,
                    -100.0,
                    100.0,
                    0.0,
                    true,
                    range_label(v.compress),
                    |s, x| s.velocity.compress = x.round(),
                ),
                self.knob_cell(
                    "midi-tool-vel-random",
                    "RANDOM",
                    v.random as f32,
                    0.0,
                    40.0,
                    0.0,
                    false,
                    format!("± {}", v.random),
                    |s, x| s.velocity.random = x.round().clamp(0.0, 40.0) as u8,
                ),
            ]),
            self.seed("midi-tool-vel-seed", s.velocity_seed, |s, x| {
                s.velocity_seed = x
            }),
        ]
    }

    fn cleanup(&self, s: Settings) -> Vec<gpui::AnyElement> {
        vec![
            self.check(
                "midi-tool-clean-doubles",
                "Doubled notes (same key and start)",
                s.cleanup_doubles,
                |s| s.cleanup_doubles = !s.cleanup_doubles,
            ),
            self.check(
                "midi-tool-clean-short",
                "Notes shorter than",
                s.cleanup_short,
                |s| s.cleanup_short = !s.cleanup_short,
            ),
            div()
                .pl(px(26.0))
                .child(self.choices(
                    "midi-tool-clean-short-res",
                    &CLEANUP_SHORT_CHOICES,
                    s.cleanup_short_res,
                    |s, v| s.cleanup_short_res = v,
                ))
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(self.check(
                    "midi-tool-clean-quiet",
                    "Notes softer than",
                    s.cleanup_quiet,
                    |s| s.cleanup_quiet = !s.cleanup_quiet,
                ))
                .child(self.knob_cell(
                    "midi-tool-clean-quiet-below",
                    "VELOCITY",
                    s.cleanup_quiet_below as f32,
                    1.0,
                    64.0,
                    10.0,
                    false,
                    s.cleanup_quiet_below.to_string(),
                    |s, v| s.cleanup_quiet_below = v.round().clamp(1.0, 64.0) as u8,
                ))
                .into_any_element(),
        ]
    }
}

fn knob_row(cells: Vec<gpui::AnyElement>) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .justify_center()
        .gap(px(crate::theme::space::TIGHT))
        .pb(px(crate::theme::space::SNUG))
        .children(cells)
        .into_any_element()
}

fn signed(value: i32) -> String {
    if value > 0 {
        format!("+{value}")
    } else {
        value.to_string()
    }
}

fn tension_label(tension: f32) -> String {
    if tension.abs() < 0.005 {
        "Even".to_string()
    } else if tension > 0.0 {
        format!("Faster {:.0}%", tension * 100.0)
    } else {
        format!("Slower {:.0}%", -tension * 100.0)
    }
}

fn gap_label(ms: f32) -> String {
    if ms.abs() < 0.5 {
        "Touching".to_string()
    } else if ms > 0.0 {
        format!("{ms:.0} ms over")
    } else {
        format!("{:.0} ms gap", -ms)
    }
}

fn range_label(compress: f32) -> String {
    if compress.abs() < 0.5 {
        "As played".to_string()
    } else if compress > 0.0 {
        format!("Tighter {compress:.0}%")
    } else {
        format!("Wider {:.0}%", -compress)
    }
}

impl Render for MidiToolWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::theme::{space, typography};
        let tool = self.tool;
        let roll = self.roll.upgrade();
        let settings = self.settings(cx);
        let (count, summary) = roll
            .as_ref()
            .map(|roll| roll.read(cx).tool_session_summary())
            .unwrap_or((0, "The MIDI editor is closed".to_string()));
        let can_accept = count > 0 && settings.is_some();

        let body: Vec<gpui::AnyElement> = match settings {
            Some(settings) => self.body(settings),
            None => Vec::new(),
        };
        let title = tool.label();
        let close_roll = self.roll.clone();
        let reset_roll = self.roll.clone();
        let accept_roll = self.roll.clone();

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
            // With the platform's frame (Windows) the OS draws the caption
            // and its close button; `on_window_should_close` reverts there.
            .when(!crate::platform_chrome::native_tool_window_frame(), |el| {
                el.child(external_window_titlebar(
                    title,
                    "midi-tool-close",
                    move |window, cx| {
                        // Closing is "never mind": the notes go back.
                        if let Some(roll) = close_roll.upgrade() {
                            roll.update(cx, |roll, cx| roll.cancel_active_tool(cx));
                        }
                        window.remove_window();
                    },
                ))
            })
            .child(
                div()
                    .id("midi-tool-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(space::SNUG))
                    .px(px(space::SECTION))
                    .pt(px(space::LOOSE))
                    .pb(px(space::BASE))
                    .overflow_y_scroll()
                    .bg(Colors::surface_base())
                    .children(body),
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
                        "midi-tool-reset",
                        "Reset",
                        FbButtonKind::Default,
                        settings.is_some(),
                        move |_, _w, cx| {
                            if let Some(roll) = reset_roll.upgrade() {
                                roll.update(cx, |roll, cx| roll.reset_active_tool(cx));
                            }
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
                            .child(div().truncate().child(summary)),
                    )
                    .child(fb_button(
                        "midi-tool-accept",
                        if tool.deletes() { "Delete" } else { "Accept" },
                        if tool.deletes() {
                            FbButtonKind::Danger
                        } else {
                            FbButtonKind::Primary
                        },
                        can_accept,
                        move |_, _w, cx| {
                            if let Some(roll) = accept_roll.upgrade() {
                                roll.update(cx, |roll, cx| roll.accept_active_tool(cx));
                            }
                        },
                    )),
            )
    }
}

/// Open `tool`'s window for `roll`, centred on the editor's window.
pub fn open_midi_tool_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    roll: Entity<PianoRoll>,
    tool: AdvancedTool,
    cx: &mut App,
) -> Result<WindowHandle<MidiToolWindow>, String> {
    let (w, mut h) = tool.window_size();
    // Windows: a native Win32 frame. Its caption sits outside the client
    // bounds, so the drawn titlebar's height comes off the requested size.
    let native_frame = crate::platform_chrome::native_tool_window_frame();
    if native_frame {
        h -= crate::platform_chrome::PlatformChromePolicy::current().titlebar_height_px;
    }
    let window_bounds =
        crate::window_position::centered_window_bounds(owner_bounds, size(px(w), px(h)), cx);
    let mut options = if native_frame {
        crate::platform_chrome::native_tool_window_options_partial(tool.label())
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
    let closing = roll.downgrade();
    cx.open_window(options, move |window, cx| {
        // The platform's close button (and Alt+F4) skip the drawn titlebar,
        // so the revert is heard here too. A no-op after Accept.
        window.on_window_should_close(cx, move |_window, cx| {
            if let Some(roll) = closing.upgrade() {
                roll.update(cx, |roll, cx| roll.cancel_active_tool(cx));
            }
            true
        });
        let view = cx.new(|cx| {
            // An editor that goes away (the project was closed or switched)
            // takes its tool window with it.
            cx.observe_release_in(&roll, window, |_, _, window, _| window.remove_window())
                .detach();
            MidiToolWindow::new(roll, tool, cx)
        });
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        view
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readouts_name_what_the_knob_does() {
        assert_eq!(signed(10), "+10");
        assert_eq!(signed(-5), "-5");
        assert_eq!(tension_label(0.0), "Even");
        assert_eq!(tension_label(0.5), "Faster 50%");
        assert_eq!(gap_label(-20.0), "20 ms gap");
        assert_eq!(range_label(30.0), "Tighter 30%");
        assert_eq!(range_label(-30.0), "Wider 30%");
    }
}
