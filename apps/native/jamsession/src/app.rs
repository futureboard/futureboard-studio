//! The window, laid out the way a jam client is used.
//!
//! Studio's jam panel is organised around a session: who is in the room, what
//! the link is, what the wire format costs. That is right for a panel you open
//! beside a project. It is not the shape of the tool when the jam *is* the
//! application — then the questions are in a fixed order, and they are always
//! the same three:
//!
//! ```txt
//! 1. am I getting in, and is it being heard        YOU
//! 2. can I hear them, and how loud is each one     THE ROOM
//! 3. is it coming out                              OUTPUT
//! ```
//!
//! So that is the layout: your input and what you send at the top, a strip per
//! performer in the middle, your output at the bottom. Joining lives above all
//! three because it is the one thing you do before any of them, and the device
//! settings live in their own window because choosing an interface is a
//! different job from playing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, ClipboardItem, Context, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowHandle,
    div, px, size,
};
use sphere_ui_components::components::controls::{
    FbButtonKind, fb_badge, fb_button, fb_section_header,
};
use sphere_ui_components::components::slider::slider;
use sphere_ui_components::components::text_input::{
    TextInputState, bind_mouse_selection, text_field_with_callbacks,
};
use sphere_ui_components::components::title_bar::external_window_titlebar;
use sphere_ui_components::components::{account_chip, account_menu_overlay};
use sphere_ui_components::jam::{self, JamStreamView, JamUiState};
use sphere_ui_components::theme::{Colors, radius, space, typography};
use sphere_ui_components::window_position::centered_window_bounds;

use crate::monitor::{InputStatus, JamMonitor, Levels};

const WINDOW_WIDTH: f32 = 520.0;
const WINDOW_HEIGHT: f32 = 660.0;

/// How often the room and the meters are re-read. 30 Hz is the rate the rest of
/// Futureboard meters at; faster repaints for packets nobody can see arriving.
const REFRESH: Duration = Duration::from_millis(33);

/// Distance from the window's top-left down to the bottom of the row the
/// account chip sits in — the titlebar plus the header's first line — so the
/// menu drops from the chip rather than from the window edge.
const ACCOUNT_MENU_ANCHOR_TOP: f32 =
    sphere_ui_components::components::title_bar::TITLEBAR_HEIGHT + 44.0;

/// Publish keys, as the controller reports them.
const KEY_MASTER: &str = "master";
const KEY_LIVE_INPUT: &str = "live-input";

pub struct JamApp {
    monitor: Arc<JamMonitor>,
    state: JamUiState,
    levels: Levels,
    /// What the capture side is doing, re-read with the meters.
    input: InputStatus,
    /// Input gain as the fader holds it, `0.0..=1.0` mapping to `0.0..=2.0`
    /// linear. Kept here because the engine's monitor gain is write-only.
    input_gain: f32,
    /// The link or code someone pasted to join with.
    link_input: TextInputState,
    /// What the app is doing or what went wrong, and when it started saying so.
    ///
    /// It expires. A network call that fails leaves nothing in the room to
    /// notice, and a status line stuck on "Creating..." for the rest of the
    /// session is worse than one that says nothing: it reports a refusal as a
    /// hang.
    status: Option<(String, Instant)>,
    /// The link most recently copied, and when. Keyed by the link itself so a
    /// freshly minted invite does not inherit the previous one's confirmation.
    copied: Option<(String, Instant)>,
}

/// How long a status line stands before it expires.
const STATUS_LINGER: Duration = Duration::from_secs(6);

/// How long a copy confirmation stands.
const COPY_FEEDBACK: Duration = Duration::from_secs(2);

impl JamApp {
    fn new(monitor: Arc<JamMonitor>, cx: &mut Context<Self>) -> Self {
        Self::spawn_refresh(cx);
        let focus_handle = cx.focus_handle();
        Self {
            monitor,
            state: jam::snapshot(),
            levels: Levels::default(),
            input: InputStatus::default(),
            input_gain: 0.5,
            link_input: TextInputState::new("jam-app-link", focus_handle)
                .with_placeholder("Paste a jam link or code")
                .with_accessible_label("Jam link or code"),
            status: None,
            copied: None,
        }
    }

    fn say(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    /// What the footer says.
    ///
    /// The controller's own error outranks everything: a refusal from the jam
    /// service is the most useful thing on the screen at the moment it arrives,
    /// and an unreachable or unconfigured service is exactly that case.
    fn status_line(&self) -> String {
        if let Some(error) = self.state.last_error.as_ref() {
            return error.clone();
        }
        match self.status.as_ref() {
            Some((message, at)) if at.elapsed() < STATUS_LINGER => message.clone(),
            _ => self.state.state_label.clone(),
        }
    }

    /// The link worth handing somebody, if there is one.
    ///
    /// The invite when this Studio minted one — it carries a bearer secret and
    /// admits an account the room does not know yet — and the plain room link
    /// otherwise, which only works for somebody already admitted.
    fn shareable_link(&self) -> Option<(&'static str, String)> {
        if let Some(link) = self
            .state
            .invite_link
            .as_ref()
            .filter(|link| !link.trim().is_empty())
        {
            return Some(("Invite", link.clone()));
        }
        // Shown, but named for what it is. A room link admits the host and
        // existing members and nobody else, so calling it a "link" without
        // qualification is how it gets handed to a guest who is then refused.
        let room = self.state.join_url.trim();
        (!room.is_empty()).then(|| ("Room only", room.to_string()))
    }

    fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let alive = this
                    .update(cx, |this, cx| {
                        // Read only. The controller's own poll thread advances the
                        // published state; this just picks it up, with the meters.
                        this.state = jam::snapshot();
                        this.levels = this.monitor.levels();
                        this.input = this.monitor.input_status();
                        this.follow_room();
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Hear whoever is in the room, and stop hearing whoever left.
    ///
    /// Capture starts on arrival rather than on a click. Studio makes routing a
    /// deliberate act because a project has tracks to route *to* and a mix that
    /// a stranger's audio should not join uninvited. Neither is true here: this
    /// application exists to hear the room, so a stream that has arrived and is
    /// not being listened to is a bug in the client, not a choice the user made.
    ///
    /// Per-stream volume and mute stay: choosing not to hear somebody is still
    /// a choice, it is simply made after they are audible rather than before.
    fn follow_room(&mut self) {
        let listening = self.monitor.listeners();
        for stream in &self.state.streams {
            let device_id = DirectAudio::jam_bus::jam_device_id(&stream.stream_id);
            if listening
                .iter()
                .any(|listener| listener.device_id == device_id)
            {
                continue;
            }
            if let Err(error) = self.monitor.listen_to(
                device_id,
                stream.stream_name.clone(),
                stream.channels.max(1),
            ) {
                eprintln!("[jam-app] could not follow {}: {error}", stream.stream_name);
            }
        }

        // A performer who left leaves a track reading an unbound slot, which is
        // silence with a fader on it. Dropping it keeps the strip list the room
        // rather than the history of the room.
        for listener in &listening {
            let still_here = self.state.streams.iter().any(|stream| {
                DirectAudio::jam_bus::jam_device_id(&stream.stream_id) == listener.device_id
            });
            if !still_here {
                let _ = self.monitor.stop_listening(&listener.device_id);
            }
        }
    }

    // ── Actions ─────────────────────────────────────────────────────────────

    fn create_room(&mut self, cx: &mut Context<Self>) {
        self.say("Creating\u{2026}");
        self.run_jam_call(cx, "create", |controller| {
            controller.create_and_join("Jam").map(|_| ())
        });
        cx.notify();
    }

    fn join(&mut self, cx: &mut Context<Self>) {
        let link = self.link_input.value.trim().to_string();
        if link.is_empty() {
            return;
        }
        self.say("Joining\u{2026}");
        self.run_jam_call(cx, "join", move |controller| {
            controller.join_with_link(&link)
        });
        cx.notify();
    }

    fn leave(&mut self, cx: &mut Context<Self>) {
        self.say("Leaving\u{2026}");
        self.run_jam_call(cx, "leave", |controller| controller.leave());
        cx.notify();
    }

    /// Run one jam call off the UI thread and bring its answer back.
    ///
    /// The answer is the point. These are blocking HTTP calls to a service that
    /// may be unreachable, unconfigured, or simply saying no, and a client that
    /// logs the refusal to stderr while leaving "Creating..." on screen has
    /// reported a hang where there was an error. Every one of them lands in the
    /// status line instead.
    fn run_jam_call(
        &mut self,
        cx: &mut Context<Self>,
        what: &'static str,
        call: impl FnOnce(&mut jam::JamController) -> jam::JamResult<()> + Send + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(
                    async move { jam::with_controller(call).map_err(|error| error.user_message()) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(()) => this.status = None,
                    Err(message) => {
                        eprintln!("[jam-app] {what} failed: {message}");
                        this.say(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy a link, and say so where the user is already looking.
    fn copy_link(&mut self, link: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
        self.copied = Some((link, Instant::now()));
        cx.notify();
    }

    /// Copy a link that actually admits somebody.
    ///
    /// An invite when there is one, and one minted on the spot when there is
    /// not. The two links look alike and are not alike: a room link carries no
    /// secret, so the server admits only the host and existing members through
    /// it — which means handing one to a guest, or opening one in a browser
    /// that is not signed in as the host, is refused. Copying whichever link
    /// happened to be lying around is how that happens, so this never does.
    fn copy_invite(&mut self, cx: &mut Context<Self>) {
        if let Some(link) = self
            .state
            .invite_link
            .as_ref()
            .filter(|link| !link.trim().is_empty())
            .cloned()
        {
            self.copy_link(link, cx);
            return;
        }

        self.say("Minting an invite\u{2026}");
        cx.spawn(async move |this, cx| {
            let minted = cx
                .background_executor()
                .spawn(async move {
                    jam::with_controller(|controller| controller.create_invite("performer"))
                        .map_err(|error| error.user_message())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match minted {
                    Ok(link) => this.copy_link(link, cx),
                    Err(message) => {
                        eprintln!("[jam-app] invite failed: {message}");
                        this.say(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_self_monitor(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Err(error) = self.monitor.set_self_monitor(on) {
            self.say(error);
        }
        cx.notify();
    }

    /// Start or stop one of the two things this machine can send.
    fn set_send(&mut self, key: &'static str, send: bool, cx: &mut Context<Self>) {
        cx.background_executor()
            .spawn(async move {
                let result = jam::with_controller(|controller| match (key, send) {
                    (KEY_LIVE_INPUT, true) => controller.publish_live_input(),
                    (KEY_LIVE_INPUT, false) => controller.unpublish_live_input(),
                    (_, true) => controller.publish_master(),
                    (_, false) => controller.unpublish_master(),
                });
                if let Err(error) = result {
                    eprintln!("[jam-app] send toggle failed: {}", error.user_message());
                }
            })
            .detach();
        cx.notify();
    }

    fn set_input_gain(&mut self, norm: f32, cx: &mut Context<Self>) {
        self.input_gain = norm.clamp(0.0, 1.0);
        if let Err(error) = self.monitor.set_input_gain(self.input_gain * 2.0) {
            eprintln!("[jam-app] input gain: {error}");
        }
        cx.notify();
    }

    /// Route a performer to a track, or drop the track again.
    fn set_listening(&mut self, stream: &JamStreamView, listening: bool, cx: &mut Context<Self>) {
        let device_id = DirectAudio::jam_bus::jam_device_id(&stream.stream_id);
        let result = if listening {
            self.monitor.listen_to(
                device_id,
                stream.stream_name.clone(),
                stream.channels.max(1),
            )
        } else {
            self.monitor.stop_listening(&device_id)
        };
        if let Err(error) = result {
            eprintln!("[jam-app] listen: {error}");
        }
        cx.notify();
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        let monitor = Arc::clone(&self.monitor);
        cx.defer(move |cx| {
            if let Err(error) = crate::settings::open_audio_settings(monitor, cx) {
                eprintln!("[jam-app] settings window: {error}");
            }
        });
    }

    // ── Sections ────────────────────────────────────────────────────────────

    /// Joining, and whether it worked. Above everything else because it is the
    /// one thing that happens before anything else can.
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.state.connected;
        let (tone, badge) = if connected {
            (Colors::status_success(), "Connected")
        } else if self.state.state_label.is_empty() {
            (Colors::text_faint(), "Offline")
        } else {
            (Colors::status_warning(), "Connecting")
        };
        let title = if self.state.jam_name.trim().is_empty() {
            "No room".to_string()
        } else {
            self.state.jam_name.clone()
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(space::BASE))
            .px(px(space::SECTION))
            .py(px(space::LOOSE))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::UI_TITLE))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(div().flex_none().child(fb_badge(badge, tone)))
                    // Signing in is not optional here: the jam client asks the
                    // account service for a bearer token before it can create
                    // or join anything, so the one control that gets you an
                    // account belongs beside the room's own identity rather
                    // than buried in a menu.
                    .children(account_chip()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SECTION))
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(format!("Code {}", or_dash(&self.state.public_id)))
                    .child(if self.state.rtt_ms > 0.0 {
                        format!("RTT {:.0} ms", self.state.rtt_ms)
                    } else {
                        "RTT —".to_string()
                    })
                    .child(if self.state.clock_locked {
                        "Clock locked".to_string()
                    } else {
                        "Clock free".to_string()
                    }),
            )
            .child(if connected {
                let link = self.shareable_link();
                let copied = link
                    .as_ref()
                    .is_some_and(|(_, link)| copied_recently(self.copied.as_ref(), link));
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .children(link.clone().map(|(label, _)| {
                        div()
                            .flex_none()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(label)
                    }))
                    .children(link.clone().map(|(_, value)| {
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_secondary())
                            .child(value)
                    }))
                    .child(fb_button(
                        "jam-app-copy",
                        if copied { "Copied" } else { "Copy invite" },
                        FbButtonKind::Default,
                        true,
                        cx.listener(move |this, _event, _window, cx| {
                            this.copy_invite(cx);
                        }),
                    ))
                    .child(fb_button(
                        "jam-app-leave",
                        "Leave",
                        FbButtonKind::Danger,
                        true,
                        cx.listener(|this, _event, _window, cx| this.leave(cx)),
                    ))
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(text_field_with_callbacks(
                                &self.link_input,
                                false,
                                bind_mouse_selection(cx.entity().clone(), |this| {
                                    &mut this.link_input
                                }),
                            )),
                    )
                    .child(fb_button(
                        "jam-app-join",
                        "Join",
                        FbButtonKind::Primary,
                        !self.link_input.value.trim().is_empty(),
                        cx.listener(|this, _event, _window, cx| this.join(cx)),
                    ))
                    .child(fb_button(
                        "jam-app-create",
                        "New Room",
                        FbButtonKind::Default,
                        true,
                        cx.listener(|this, _event, _window, cx| this.create_room(cx)),
                    ))
                    .into_any_element()
            })
    }

    /// What this machine puts into the room, and whether anything is arriving
    /// at the tap to put there.
    fn you(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.state.connected;
        let sending_mic = self
            .state
            .publishing
            .iter()
            .any(|key| key == KEY_LIVE_INPUT);
        let sending_master = self.state.publishing.iter().any(|key| key == KEY_MASTER);
        let monitoring = self.monitor.self_monitor();
        let gain = self.input_gain;

        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(fb_section_header("You"))
            // Monitoring depends on three invisible things — a capture stream,
            // a live ring, and the engine agreeing some track wants input — and
            // when any of them is false the symptom is the same silence. Saying
            // which one is the difference between "this is broken" and "pick an
            // input device".
            .child(
                div()
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(
                        if self.input.capture_open && self.input.frames_captured > 0 {
                            Colors::text_muted()
                        } else {
                            Colors::accent_warning()
                        },
                    )
                    .child(input_health(&self.input, monitoring)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .w(px(52.0))
                            .flex_none()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child("Input"),
                    )
                    .child(div().flex_none().child(meter(self.levels.input, 96.0)))
                    .child(div().flex_1().min_w(px(0.0)).child(slider(
                        "jam-app-input-gain",
                        gain,
                        Colors::accent_primary(),
                        cx.listener(|this, value: &f32, _window, cx| {
                            this.set_input_gain(*value, cx);
                        }),
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .w(px(52.0))
                            .flex_none()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child("Send"),
                    )
                    .child(fb_button(
                        "jam-app-send-mic",
                        if sending_mic {
                            "Mic ·  on"
                        } else {
                            "Mic · off"
                        },
                        if sending_mic {
                            FbButtonKind::Primary
                        } else {
                            FbButtonKind::Default
                        },
                        connected,
                        cx.listener(move |this, _event, _window, cx| {
                            this.set_send(KEY_LIVE_INPUT, !sending_mic, cx);
                        }),
                    ))
                    .child(fb_button(
                        "jam-app-send-master",
                        if sending_master {
                            "Mix ·  on"
                        } else {
                            "Mix · off"
                        },
                        if sending_master {
                            FbButtonKind::Primary
                        } else {
                            FbButtonKind::Default
                        },
                        connected,
                        cx.listener(move |this, _event, _window, cx| {
                            this.set_send(KEY_MASTER, !sending_master, cx);
                        }),
                    ))
                    // Hearing yourself is not the same as being heard, and it
                    // needs no room: it is how an interface is checked before
                    // joining anything, and how a performer plays to the same
                    // buffer everyone else hears them through.
                    .child(fb_button(
                        "jam-app-monitor",
                        if monitoring {
                            "Monitor \u{b7}  on"
                        } else {
                            "Monitor \u{b7} off"
                        },
                        if monitoring {
                            FbButtonKind::Primary
                        } else {
                            FbButtonKind::Default
                        },
                        true,
                        cx.listener(move |this, _event, _window, cx| {
                            this.set_self_monitor(!monitoring, cx);
                        }),
                    )),
            )
    }

    /// A strip per performer: hear them, how loud, and why not when not.
    fn room(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let listening = self.monitor.listeners();
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .child(fb_section_header(format!(
                "The room · {}",
                self.state.streams.len()
            )));

        if self.state.streams.is_empty() {
            return section.child(
                div()
                    .py(px(space::BLOCK))
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_muted())
                    .child(if self.state.connected {
                        "Nobody else is publishing yet."
                    } else {
                        "Join a room, or start one, to play with somebody."
                    }),
            );
        }

        for (index, stream) in self.state.streams.iter().enumerate() {
            let device_id = DirectAudio::jam_bus::jam_device_id(&stream.stream_id);
            let listener = listening
                .iter()
                .find(|listener| listener.device_id == device_id)
                .cloned();
            section = section.child(self.peer_strip(index, stream, listener, cx));
        }
        section
    }

    fn peer_strip(
        &self,
        index: usize,
        stream: &JamStreamView,
        listener: Option<crate::monitor::Listener>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hearing = listener.is_some();
        let volume = listener.as_ref().map(|l| l.volume / 2.0).unwrap_or(0.5);
        let muted = listener.as_ref().is_some_and(|l| l.muted);
        let device_id = DirectAudio::jam_bus::jam_device_id(&stream.stream_id);
        let queued = stream.clone();
        let mute_target = device_id.clone();
        let volume_target = device_id;

        div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .py(px(space::SNUG))
            .px(px(space::BASE))
            .rounded(px(radius::CONTROL))
            .bg(Colors::surface_panel_alt())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(typography::UI_SM))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(stream.stream_name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(typography::UI_XS))
                                    .text_color(Colors::text_muted())
                                    .child(format!(
                                        "{} · {}",
                                        if stream.display_name.trim().is_empty() {
                                            stream.handle.clone()
                                        } else {
                                            stream.display_name.clone()
                                        },
                                        peer_health(stream, hearing)
                                    )),
                            ),
                    )
                    .child(div().flex_none().child(meter(stream.peak, 56.0)))
                    .child(fb_button(
                        gpui::ElementId::Name(format!("jam-app-hear-{index}").into()),
                        if hearing { "Listening" } else { "Connecting" },
                        if hearing {
                            FbButtonKind::Primary
                        } else {
                            FbButtonKind::Default
                        },
                        hearing,
                        cx.listener(move |this, _event, _window, cx| {
                            this.set_listening(&queued, !hearing, cx);
                        }),
                    )),
            )
            .when(hearing, |strip| {
                strip.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::BASE))
                        .child(div().flex_1().min_w(px(0.0)).child(slider(
                            format!("jam-app-volume-{index}"),
                            volume,
                            Colors::accent_primary(),
                            cx.listener(move |this, value: &f32, _window, cx| {
                                if let Err(error) =
                                    this.monitor.set_volume(&volume_target, *value * 2.0)
                                {
                                    eprintln!("[jam-app] volume: {error}");
                                }
                                cx.notify();
                            }),
                        )))
                        .child(fb_button(
                            gpui::ElementId::Name(format!("jam-app-mute-{index}").into()),
                            if muted { "Muted" } else { "Mute" },
                            if muted {
                                FbButtonKind::Danger
                            } else {
                                FbButtonKind::Default
                            },
                            true,
                            cx.listener(move |this, _event, _window, cx| {
                                if let Err(error) = this.monitor.set_muted(&mute_target, !muted) {
                                    eprintln!("[jam-app] mute: {error}");
                                }
                                cx.notify();
                            }),
                        )),
                )
            })
    }

    /// What is leaving for the speakers, and the way to the device settings.
    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .flex_none()
            .px(px(space::SECTION))
            .py(px(space::LOOSE))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex_none()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child("Output"),
            )
            .child(div().flex_none().child(meter(self.levels.output, 96.0)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(self.status_line()),
            )
            .child(fb_button(
                "jam-app-settings",
                "Audio Engine…",
                FbButtonKind::Default,
                true,
                cx.listener(|this, _event, _window, cx| this.open_settings(cx)),
            ))
    }
}

impl Render for JamApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(sphere_ui_components::theme::ui_font())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key.eq_ignore_ascii_case("enter") && !this.state.connected {
                    this.join(cx);
                    cx.notify();
                }
            }))
            .child(external_window_titlebar(
                "Futureboard Jam",
                "jam-app-close",
                move |window, _cx| {
                    window.remove_window();
                },
            ))
            .relative()
            .children(account_menu_overlay(
                _window,
                ACCOUNT_MENU_ANCHOR_TOP,
                space::SECTION,
            ))
            .child(self.header(cx))
            .child(
                div()
                    .id("jam-app-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .gap(px(space::SECTION))
                    .px(px(space::SECTION))
                    .py(px(space::LOOSE))
                    .overflow_y_scroll()
                    .child(self.you(cx))
                    .child(self.room(cx)),
            )
            .child(self.footer(cx))
    }
}

/// Open the application's window.
pub fn open_jam_app_window(
    monitor: Arc<JamMonitor>,
    cx: &mut App,
) -> Result<WindowHandle<JamApp>, String> {
    let mut options = sphere_ui_components::platform_chrome::studio_window_options();
    // `studio_window_options` opens hidden so a heavy first layout cannot show a
    // black client area. This window's first frame is a header and a list;
    // staying hidden would look like a launch that did nothing.
    options.show = true;
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        cx.primary_display().map(|display| display.bounds()),
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        cx,
    )));

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| JamApp::new(monitor, cx))
    })
    .map_err(|error| error.to_string())
}

/// A level bar. Peak, not RMS: on a network path the thing worth seeing is
/// whether anything arrived at all, and a peak says that a block sooner.
fn meter(peak: f32, width: f32) -> impl IntoElement {
    let filled = (peak.clamp(0.0, 1.0) * width).round();
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .w(px(width))
        .h(px(6.0))
        .rounded(px(2.0))
        .bg(Colors::surface_canvas())
        .child(
            div()
                .w(px(filled))
                .h(px(6.0))
                .rounded(px(2.0))
                .bg(if peak > 0.98 {
                    Colors::status_error()
                } else {
                    Colors::accent_primary()
                }),
        )
}

/// Why a performer is or is not audible here.
///
/// The four states are different problems with different fixes, and they used
/// to be one silence: not routed to a track, no format from the server, the
/// ring still filling, or playing and dropping. Naming which one is the
/// difference between "the app is broken" and "your network is".
fn peer_health(stream: &JamStreamView, hearing: bool) -> String {
    if !hearing {
        return "Not being listened to".to_string();
    }
    if !stream.receiving {
        return "Waiting for a format".to_string();
    }
    if stream.buffering {
        return "Buffering".to_string();
    }
    if stream.dropouts > 0 {
        return format!("Playing · {} dropout(s)", stream.dropouts);
    }
    "Playing".to_string()
}

/// Why the input is or is not being heard.
///
/// Four states, four different fixes, and they used to be one silence: no
/// capture device open at all, one open that is delivering nothing, one
/// delivering that nothing is listening to, and one working. The first is a
/// device choice, the second is the interface or the wrong channel, the third
/// is this app's own Monitor button.
fn input_health(status: &InputStatus, monitoring: bool) -> String {
    if let Some(error) = status.last_error.as_ref() {
        return error.clone();
    }
    if !status.capture_open {
        return "No capture device \u{2014} choose an input in Audio Engine".to_string();
    }
    if status.frames_captured == 0 {
        return "Capture open, but the device has delivered nothing yet".to_string();
    }
    if !monitoring {
        return "Capturing \u{2014} switch Monitor on to hear yourself".to_string();
    }
    "Capturing and monitoring".to_string()
}

/// Whether a link should still be confirming a copy.
///
/// The confirmation belongs to one link and it expires, so a freshly minted
/// invite reads as uncopied even though the link it replaced was copied a
/// moment ago.
fn copied_recently(copied: Option<&(String, Instant)>, link: &str) -> bool {
    copied.is_some_and(|(copied, at)| copied == link && at.elapsed() < COPY_FEEDBACK)
}

fn or_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "—".to_string()
    } else {
        value.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{COPY_FEEDBACK, copied_recently, input_health, or_dash, peer_health};
    use crate::monitor::InputStatus;
    use sphere_ui_components::jam::JamStreamView;
    use std::time::Instant;

    fn stream() -> JamStreamView {
        JamStreamView {
            stream_id: "str_1".to_string(),
            user_id: "usr_1".to_string(),
            device_id: "studio".to_string(),
            handle: "@nut".to_string(),
            display_name: "Nut".to_string(),
            avatar_url: String::new(),
            stream_name: "Guitar".to_string(),
            channels: 2,
            channel_labels: Vec::new(),
            sample_rate: 48_000,
            codec: "pcm".to_string(),
            receiving: true,
            peak: 0.5,
            rtt_ms: 12.0,
            routed: true,
            buffering: false,
            dropouts: 0,
        }
    }

    /// "I cannot hear them" has four causes with four different fixes. A strip
    /// that reports one silence for all of them sends the user to the network
    /// when the answer was a button on this screen.
    #[test]
    fn every_reason_a_performer_is_inaudible_reads_differently() {
        assert!(peer_health(&stream(), false).contains("Not being listened to"));

        let no_format = JamStreamView {
            receiving: false,
            ..stream()
        };
        assert!(peer_health(&no_format, true).contains("format"));

        let buffering = JamStreamView {
            buffering: true,
            ..stream()
        };
        assert_eq!(peer_health(&buffering, true), "Buffering");

        let glitching = JamStreamView {
            dropouts: 4,
            ..stream()
        };
        assert!(peer_health(&glitching, true).contains('4'));

        assert_eq!(peer_health(&stream(), true), "Playing");
    }

    /// Listening is asked about first: a performer nobody routed is silent for
    /// a reason that has nothing to do with the network, and every other
    /// reading of them is a consequence of it.
    #[test]
    fn listening_is_reported_before_anything_downstream_of_it() {
        let nothing_works = JamStreamView {
            receiving: false,
            buffering: true,
            dropouts: 99,
            ..stream()
        };
        assert!(peer_health(&nothing_works, false).contains("Not being listened to"));
    }

    #[test]
    fn a_missing_readout_is_a_dash_rather_than_a_gap() {
        assert_eq!(or_dash(""), "—");
        assert_eq!(or_dash("   "), "—");
        assert_eq!(or_dash(" SWIFT-42 "), "SWIFT-42");
    }

    /// A confirmation that outlived its link, or stood for the session, would
    /// tell somebody they had copied something they had not.
    #[test]
    fn a_copied_link_confirms_only_for_itself_and_only_for_a_while() {
        let link = "https://jam.futureboard.studio/j/EWEFDN#secret".to_string();
        let fresh = Some((link.clone(), Instant::now()));
        assert!(copied_recently(fresh.as_ref(), &link));
        assert!(!copied_recently(
            fresh.as_ref(),
            "https://jam.futureboard.studio/j/OTHER#secret"
        ));

        let stale = Some((link.clone(), Instant::now() - COPY_FEEDBACK * 2));
        assert!(!copied_recently(stale.as_ref(), &link));
        assert!(!copied_recently(None, &link));
    }

    /// The silent-monitor report. Every state has a different fix, so every
    /// state has to read differently — a single "no input" would send somebody
    /// to the device picker when the answer was the Monitor button.
    #[test]
    fn every_reason_the_input_is_silent_reads_differently() {
        let working = InputStatus {
            capture_open: true,
            monitoring: true,
            frames_captured: 4096,
            last_error: None,
        };
        assert_eq!(input_health(&working, true), "Capturing and monitoring");

        let not_monitoring = input_health(&working, false);
        assert!(not_monitoring.contains("Monitor"));

        let dead = InputStatus {
            frames_captured: 0,
            ..working.clone()
        };
        assert!(input_health(&dead, true).contains("delivered nothing"));

        let closed = InputStatus {
            capture_open: false,
            ..working.clone()
        };
        assert!(input_health(&closed, true).contains("Audio Engine"));

        let failed = InputStatus {
            last_error: Some("Live input unavailable: device in use".to_string()),
            ..working
        };
        assert!(input_health(&failed, true).contains("device in use"));
    }
}
