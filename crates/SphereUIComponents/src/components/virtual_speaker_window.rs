//! Virtual Speaker — hear the mix as it would play somewhere else: in a car,
//! on a phone, through a PA, in a living room.
//!
//! A Control Room stage (`solfege_spatialaudio::ListeningSimulator`): it
//! colours what the monitoring output plays and nothing else — never the
//! master, an export or a recording. The window is a thin view over the
//! Studio's [`SimulationSettings`]; the Studio owns them, sends them to the
//! engine and remembers the last profile and listening device. Whether the
//! simulation is on is never remembered: one left on from the last session
//! would be mixed against by mistake.
//!
//! Like the other utility windows, it never reads the Studio from `render`:
//! the Studio pushes a [`VirtualSpeakerSnapshot`] in, and a choice goes back
//! out through [`SimulationChangeCb`].

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowHandle,
    WindowKind, div, px, size,
};
use solfege_spatialaudio::{ListeningDevice, ListeningGroup, ListeningProfile, SimulationSettings};

use crate::components::controls::{
    FbLatch, FbSegment, fb_badge, fb_segment, fb_segmented_track, fb_toggle,
};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{self, Colors, radius, space, typography};
use crate::window_position::{apply_owner_display, centered_window_bounds};

const WINDOW_WIDTH: f32 = 560.0;
const WINDOW_HEIGHT: f32 = 680.0;
const WINDOW_MIN_WIDTH: f32 = 440.0;
const WINDOW_MIN_HEIGHT: f32 = 460.0;

/// What the window draws, pushed in by the Studio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VirtualSpeakerSnapshot {
    pub settings: SimulationSettings,
    /// Whether monitoring runs through the Control Room, where the simulation
    /// sits. When Master writes the hardware directly nothing here is heard.
    pub control_room_in_path: bool,
    /// Whether an audio engine is open to hear anything with.
    pub engine_ready: bool,
}

/// A new choice, for the Studio to apply.
pub type SimulationChangeCb = Arc<dyn Fn(SimulationSettings, &mut App) + 'static>;

pub struct VirtualSpeakerWindow {
    focus_handle: FocusHandle,
    snapshot: VirtualSpeakerSnapshot,
    on_change: SimulationChangeCb,
}

impl VirtualSpeakerWindow {
    fn new(
        snapshot: VirtualSpeakerSnapshot,
        on_change: SimulationChangeCb,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            snapshot,
            on_change,
        }
    }

    /// The Studio's current state, after any change from anywhere.
    pub fn set_snapshot(&mut self, snapshot: VirtualSpeakerSnapshot, cx: &mut Context<Self>) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            cx.notify();
        }
    }

    /// Show `settings` at once and hand them to the Studio.
    fn choose(&mut self, settings: SimulationSettings, cx: &mut Context<Self>) {
        self.snapshot.settings = settings;
        (self.on_change)(settings, cx);
        cx.notify();
    }
}

impl Render for VirtualSpeakerWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot;
        let settings = snapshot.settings;

        let mut body = div()
            .flex_1()
            .min_h_0()
            .id("virtual-speaker-body")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(space::LOOSE))
            .p(px(space::SECTION))
            .child(control_card(settings, cx));

        for group in ListeningGroup::ALL {
            let profiles: Vec<ListeningProfile> = ListeningProfile::ALL
                .into_iter()
                .filter(|p| p.group() == group)
                .collect();
            let mut rows = div().flex().flex_col().gap(px(space::SNUG));
            for pair in profiles.chunks(2) {
                let mut row = div().flex().flex_row().gap(px(space::SNUG));
                for &profile in pair {
                    row = row.child(profile_card(profile, settings, cx));
                }
                if pair.len() == 1 {
                    // Keep a lone card at half width, in its column.
                    row = row.child(div().flex_1());
                }
                rows = rows.child(row);
            }
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_muted())
                            .child(group.name().to_uppercase()),
                    )
                    .child(rows),
            );
        }

        body = body.child(notes(snapshot));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .text_size(px(typography::UI_SM))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_this, event: &KeyDownEvent, window, _cx| {
                if event.keystroke.key.as_str() == "escape" {
                    window.remove_window();
                }
            }))
            .child(external_window_titlebar(
                "Virtual Speaker",
                "virtual-speaker-close",
                move |window, _cx| window.remove_window(),
            ))
            .child(body)
    }
}

/// On/off, what is playing, and what the listener is wearing.
fn control_card(
    settings: SimulationSettings,
    cx: &mut Context<VirtualSpeakerWindow>,
) -> impl IntoElement {
    let status = if settings.enabled {
        format!(
            "{} — heard on {}",
            settings.profile.name(),
            settings.device.name().to_lowercase()
        )
    } else {
        "Off — you hear the mix as it is".to_string()
    };
    let toggle = fb_toggle(
        "virtual-speaker-power",
        if settings.enabled { "On" } else { "Off" },
        FbLatch::Monitor,
        settings.enabled,
        theme::size::DEFAULT,
        cx.listener(move |this, _event, _window, cx| {
            let mut next = this.snapshot.settings;
            next.enabled = !next.enabled;
            this.choose(next, cx);
        }),
    );

    let mut devices = fb_segmented_track();
    for (index, device) in ListeningDevice::ALL.into_iter().enumerate() {
        let position = match index {
            0 => FbSegment::First,
            _ => FbSegment::Last,
        };
        devices = devices.child(fb_segment(
            ("virtual-speaker-device", index),
            device.name(),
            settings.device == device,
            position,
            cx.listener(move |this, _event, _window, cx| {
                let mut next = this.snapshot.settings;
                next.device = device;
                this.choose(next, cx);
            }),
        ));
    }

    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(typography::UI_TITLE))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Simulation"),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(typography::UI_XS))
                                .text_color(if settings.enabled {
                                    Colors::text_secondary()
                                } else {
                                    Colors::text_muted()
                                })
                                .child(status),
                        ),
                )
                .child(toggle),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child("Listening on"),
                )
                .child(div().flex_1().child(devices)),
        )
}

/// One playback system. Choosing it also turns the simulation on: picking a
/// car and hearing nothing change would read as broken.
fn profile_card(
    profile: ListeningProfile,
    settings: SimulationSettings,
    cx: &mut Context<VirtualSpeakerWindow>,
) -> impl IntoElement {
    let selected = settings.profile == profile;
    let live = selected && settings.enabled;
    let rest = if live {
        Colors::composite(Colors::surface_panel(), Colors::state_selected())
    } else {
        Colors::surface_panel()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let border = if live {
        Colors::accent_primary()
    } else if selected {
        Colors::border_default()
    } else {
        Colors::border_subtle()
    };

    div()
        .id(("virtual-speaker-profile", profile as usize))
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .px(px(space::LOOSE))
        .py(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .bg(rest)
        .border(px(1.0))
        .border_color(border)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            let mut next = this.snapshot.settings;
            next.profile = profile;
            next.enabled = true;
            this.choose(next, cx);
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if live {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(profile.name()),
                )
                .when(profile.is_mono(), |row| {
                    row.child(fb_badge("Mono", Colors::text_muted()))
                }),
        )
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(profile.description()),
        )
}

/// What the simulation is, and anything keeping it from being heard.
fn notes(snapshot: VirtualSpeakerSnapshot) -> impl IntoElement {
    let warning = if !snapshot.engine_ready {
        Some("No audio engine is running, so nothing is heard yet.")
    } else if !snapshot.control_room_in_path {
        Some(
            "Monitoring is not running through the Control Room, where the \
             simulation sits, so it is not heard.",
        )
    } else {
        None
    };
    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .when_some(warning, |notes, warning| {
            notes.child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::status_warning())
                    .child(warning),
            )
        })
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(
                    "Monitoring only: the simulation colours what you hear and \
                     never reaches the master, an export or a recording. Every \
                     system is level-matched to the mix, so switching compares \
                     balance, not loudness. The systems are models of their \
                     kind, not measurements of one product.",
                ),
        )
}

/// Open the Virtual Speaker window.
pub fn open_virtual_speaker_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    snapshot: VirtualSpeakerSnapshot,
    on_change: SimulationChangeCb,
    cx: &mut App,
) -> Result<WindowHandle<VirtualSpeakerWindow>, String> {
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        owner_bounds,
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        cx,
    )));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_min_size = Some(size(px(WINDOW_MIN_WIDTH), px(WINDOW_MIN_HEIGHT)));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| VirtualSpeakerWindow::new(snapshot, on_change, cx))
    })
    .map_err(|error| error.to_string())
}
