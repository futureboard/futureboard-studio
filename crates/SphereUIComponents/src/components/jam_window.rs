//! Audio Jam.
//!
//! The room, as a Studio surface: who is in it, what they are publishing, what
//! the link between them looks like, and one button per remote stream that
//! turns it into a track.
//!
//! Region contract — owner: this window. State owner:
//! [`crate::jam`] (a process-wide snapshot the network threads publish and this
//! window only reads). Coordinate space: window-local. Size source: fixed
//! dialog bounds. Scroll owner: the participant list. Clip owner: the same
//! list. Layer order: titlebar, header, list, footer. Focus: window-level, with
//! Escape closing.
//!
//! Nothing here touches audio. The panel reads a snapshot the jam controller
//! refreshes on a timer, at a bounded rate — a meter that repainted per packet
//! would make a busy jam a rerender storm.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, ClipboardItem, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind,
};

use crate::components::controls::{
    fb_badge, fb_button, fb_checkbox, fb_section_header, fb_segment, fb_segmented_track,
    FbButtonKind, FbSegment,
};
use crate::components::text_input::{text_field, TextInputState};
use crate::components::title_bar::external_window_titlebar;
use sphere_jam_client::protocol::ParticipantSummary;

use crate::jam::quality::{sample_format_label, sample_rate_label, web_listener_note};
use crate::jam::{
    self, JamPublishQuality, JamStreamMode, JamStreamView, JamUiState, StreamCost, SAMPLE_FORMATS,
    SAMPLE_RATES,
};
use crate::theme::{self, elevation, radius, size as size_token, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const JAM_WINDOW_WIDTH: f32 = 460.0;
pub const JAM_WINDOW_HEIGHT: f32 = 620.0;

/// How often the panel re-reads the jam snapshot.
///
/// 30 Hz is the rate the rest of Studio meters at. Anything faster would repaint
/// for packets nobody can see arriving; anything slower makes a level meter look
/// broken.
const REFRESH: Duration = Duration::from_millis(33);

/// How long the footer keeps reporting what it just did.
///
/// Long enough to read at a glance and to cover a round trip to the server,
/// short enough that a message about an action nobody remembers taking does not
/// outlive the action.
const STATUS_LINGER: Duration = Duration::from_millis(6000);

/// How long a copied link keeps saying so.
///
/// Long enough to be read at a glance, short enough that the control goes back
/// to describing what it will do rather than what it did. The panel already
/// repaints at [`REFRESH`], so the label reverts on its own without a timer of
/// its own.
const COPY_FEEDBACK: Duration = Duration::from_millis(1600);

/// What the window asks the shell to do.
///
/// The window never edits the project itself: making a track and routing it are
/// the shell's job, and the routing goes through Audio Connections like any
/// other input rather than through a private jam-only path.
#[derive(Debug, Clone)]
pub struct CreateTrackFromStream {
    pub stream_id: String,
    pub track_name: String,
    /// The Audio Connections device id for the stream, `jam:<stream_id>`.
    pub device_id: String,
    pub channels: usize,
    /// Per-channel labels, so the bus's ports carry the publisher's own names.
    pub channel_labels: Vec<String>,
}

/// What the shell hands the window so a Create Track button can reach it.
pub type CreateTrackHandler = Arc<dyn Fn(CreateTrackFromStream, &mut App) + Send + Sync>;

/// Share the selected track, or stop sharing it.
///
/// The window has no view of the project, and should not grow one: which track
/// is selected is the shell's state, and the shell is what knows the track's
/// name. `true` starts sharing, `false` stops.
pub type PublishTrackHandler = Arc<dyn Fn(bool, &mut App) + Send + Sync>;

/// Ask the shell which tracks a multitrack stream would carry.
///
/// The window has no view of the project and should not grow one. The shell
/// knows which channels hold audio of their own, what they are called, and what
/// order they sit in — all three of which are the stream's channel layout.
pub type MultitrackTracksHandler = Arc<dyn Fn(&mut App) -> Vec<(String, String)> + Send + Sync>;

/// The three things the shell does on the window's behalf.
#[derive(Clone)]
pub struct JamWindowHandlers {
    pub create_track: CreateTrackHandler,
    pub publish_track: PublishTrackHandler,
    pub multitrack_tracks: MultitrackTracksHandler,
}

pub struct JamWindow {
    focus_handle: FocusHandle,
    state: JamUiState,
    handlers: JamWindowHandlers,
    /// What the user typed for a new jam's name.
    jam_name: String,
    /// The link or code someone pasted to join with.
    link_input: TextInputState,
    /// What the panel is doing, and when it started saying so.
    ///
    /// It expires. The refresh loop clears it when the room visibly changes,
    /// but not every action changes the room — a share that was refused leaves
    /// nothing to notice — and a status line that says "Sharing…" for the rest
    /// of the session is worse than one that says nothing.
    busy: Option<(String, Instant)>,
    /// The link most recently copied, and when. Keyed by the link itself so a
    /// freshly minted invite does not inherit the previous link's confirmation.
    copied: Option<(String, Instant)>,
}

impl JamWindow {
    fn new(handlers: JamWindowHandlers, cx: &mut Context<Self>) -> Self {
        Self::spawn_refresh(cx);
        let focus_handle = cx.focus_handle();
        Self {
            link_input: TextInputState::new("jam-join-link-input", focus_handle.clone())
                .with_placeholder("Paste a jam link or code")
                .with_accessible_label("Jam link or code"),
            focus_handle,
            state: jam::snapshot(),
            handlers,
            jam_name: default_jam_name(),
            busy: None,
            copied: None,
        }
    }

    fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let alive = this
                    .update(cx, |this, cx| {
                        // Read only. The controller's own poll thread advances the
                        // published state, so closing this window does not stop a
                        // jam that tracks are still listening to.
                        let next = jam::snapshot();
                        let changed = next.state_label != this.state.state_label
                            || next.streams.len() != this.state.streams.len()
                            || next.participants.len() != this.state.participants.len()
                            || next.publishing != this.state.publishing;
                        this.state = next;
                        if changed {
                            this.busy = None;
                        }
                        this.busy.take_if(|(_, at)| at.elapsed() >= STATUS_LINGER);
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

    fn create_jam(&mut self, cx: &mut Context<Self>) {
        let name = if self.jam_name.trim().is_empty() {
            default_jam_name()
        } else {
            self.jam_name.trim().to_string()
        };
        self.set_status("Creating…");
        // The REST call is blocking and short; running it here would stall a
        // frame, so it goes to the background pool and the panel picks the
        // result up from the snapshot.
        cx.background_executor()
            .spawn(async move {
                if let Err(error) =
                    jam::with_controller(|controller| controller.create_and_join(&name).map(|_| ()))
                {
                    eprintln!("[jam] create failed: {error}");
                }
            })
            .detach();
    }

    fn leave(&mut self, cx: &mut Context<Self>) {
        self.set_status("Leaving…");
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = jam::with_controller(|controller| controller.leave()) {
                    eprintln!("[jam] leave failed: {error}");
                }
            })
            .detach();
    }

    fn set_master_publish(&mut self, send: bool, cx: &mut Context<Self>) {
        self.set_status(if send { "Publishing…" } else { "Stopping…" });
        cx.background_executor()
            .spawn(async move {
                let result = jam::with_controller(|controller| {
                    if send {
                        controller.publish_master()
                    } else {
                        controller.unpublish_master()
                    }
                });
                if let Err(error) = result {
                    eprintln!("[jam] master publish failed: {}", error.user_message());
                }
            })
            .detach();
    }

    /// Share the arrangement, or stop.
    ///
    /// The track list is read on this thread, because only the shell can supply
    /// it and only the UI thread may ask; the publish itself goes to the
    /// background pool with the layout already resolved.
    fn set_multitrack_publish(&mut self, send: bool, cx: &mut Context<Self>) {
        let tracks = if send {
            (self.handlers.multitrack_tracks)(cx)
        } else {
            Vec::new()
        };
        if send && tracks.is_empty() {
            self.set_status("This project has no tracks to share.");
            return;
        }
        self.set_status(if send { "Sharing…" } else { "Stopping…" });
        cx.background_executor()
            .spawn(async move {
                let result =
                    jam::with_controller(|controller| controller.publish_multitrack(&tracks));
                if let Err(error) = result {
                    eprintln!("[jam] multitrack share failed: {}", error.user_message());
                }
            })
            .detach();
    }

    /// Join whatever the pasted link or code names.
    fn join_with_link(&mut self, cx: &mut Context<Self>) {
        let link = self.link_input.value.trim().to_string();
        if link.is_empty() {
            return;
        }
        self.set_status("Joining…");
        cx.background_executor()
            .spawn(async move {
                if let Err(error) =
                    jam::with_controller(|controller| controller.join_with_link(&link))
                {
                    eprintln!("[jam] join failed: {}", error.user_message());
                }
            })
            .detach();
    }

    /// Replace the wire format.
    ///
    /// A quality change is a lock and two field writes, so it runs here rather
    /// than on the background pool: sending it away would make the control lag
    /// a frame behind the click for no reason.
    fn set_quality(&mut self, quality: JamPublishQuality, cx: &mut Context<Self>) {
        self.state.quality = quality.clone();
        if let Err(error) = jam::with_controller(|controller| {
            controller.set_quality(quality);
            Ok(())
        }) {
            eprintln!("[jam] {}", error.user_message());
        }
        cx.notify();
    }

    fn create_invite(&mut self, cx: &mut Context<Self>) {
        self.set_status("Minting invite…");
        cx.background_executor()
            .spawn(async move {
                match jam::with_controller(|controller| controller.create_invite("performer")) {
                    // The link is a bearer secret: it is shown once so it can be
                    // copied and is never written to a log.
                    Ok(_) => {}
                    Err(error) => eprintln!("[jam] invite failed: {}", error.user_message()),
                }
            })
            .detach();
    }

    /// Put a link on the clipboard.
    ///
    /// The link is a bearer secret, so it is handed to the clipboard and to
    /// nothing else — not to a log, not to the status line, which is why the
    /// confirmation says only that a copy happened.
    fn copy_link(&mut self, link: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
        self.copied = Some((link, Instant::now()));
        cx.notify();
    }

    fn copied_recently(&self, link: &str) -> bool {
        copied_recently(self.copied.as_ref(), link)
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.busy = Some((message.into(), Instant::now()));
    }

    fn publish_selected_track(&mut self, share: bool, cx: &mut App) {
        self.set_status(if share {
            "Sharing track…"
        } else {
            "Stopping…"
        });
        (self.handlers.publish_track)(share, cx);
    }

    fn create_track(&mut self, stream: &JamStreamView, cx: &mut App) {
        (self.handlers.create_track)(
            CreateTrackFromStream {
                stream_id: stream.stream_id.clone(),
                track_name: track_name_for(stream),
                device_id: stream.device_id(),
                channels: stream.channels.max(1),
                channel_labels: stream.channel_labels.clone(),
            },
            cx,
        );
    }
}

impl Render for JamWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let typing = self.link_input.is_focused(window);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .track_focus(&self.focus_handle)
            // While the link field has focus it owns the keyboard, and only
            // the two keys that mean something to the field itself are taken
            // here — Escape closing the window mid-paste would be the worst
            // possible reading of "cancel".
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                match (event.keystroke.key.as_str(), typing) {
                    ("enter", true) => {
                        cx.stop_propagation();
                        this.join_with_link(cx);
                        cx.notify();
                    }
                    ("escape", true) => {
                        cx.stop_propagation();
                        window.blur();
                        cx.notify();
                    }
                    ("escape", false) => window.remove_window(),
                    _ => {}
                }
            }))
            .child(external_window_titlebar(
                "Audio Jam",
                "jam-window-close",
                move |window, _cx| {
                    window.remove_window();
                },
            ))
            .child(self.header(&state))
            .child(self.banners(&state, window, cx))
            .child(self.body(&state, cx))
            .child(self.footer(&state, cx))
    }
}

impl JamWindow {
    /// Name, connection state, and the four readouts that answer "is this
    /// usable right now". The link diagnostics sit under them at caption
    /// weight: they matter while something is wrong, and should not compete
    /// with the room's identity while it is fine.
    fn header(&self, state: &JamUiState) -> impl IntoElement {
        let (tone, label) = status_tone(state);
        let rtt = if state.rtt_ms > 0.0 {
            format!("{:.1} ms", state.rtt_ms)
        } else {
            "—".to_string()
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
                    .justify_between()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::UI_TITLE))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(if state.jam_name.is_empty() {
                                "No jam".to_string()
                            } else {
                                state.jam_name.clone()
                            }),
                    )
                    .child(div().flex_none().child(fb_badge(label, tone))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(space::SECTION))
                    .child(stat("Code", &or_dash(&state.public_id)))
                    .child(stat("Region", &or_dash(&state.region_label)))
                    .child(stat("Transport", &or_dash(&state.transport_label)))
                    .child(stat("RTT", &rtt)),
            )
            .when(state.connected, |this| {
                this.child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_faint())
                        .child(link_diagnostics(state)),
                )
            })
    }

    /// Everything that needs saying before the room itself: what went wrong,
    /// what is missing, and the link that lets someone else in.
    ///
    /// Fixed rather than scrolled with the participant list. An error that
    /// scrolls out of sight while someone reads the room is an error they act
    /// on twice.
    fn banners(
        &self,
        state: &JamUiState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut children: Vec<gpui::AnyElement> = Vec::new();

        if let Some(error) = state.last_error.as_ref() {
            children.push(notice("Error", error, Colors::status_error()).into_any_element());
        }
        if !state.signed_in {
            children.push(
                notice(
                    "Sign in",
                    "Sign in to your Futureboard account to join a jam.",
                    Colors::status_warning(),
                )
                .into_any_element(),
            );
        }
        if let Some(link) = state.invite_link.as_ref() {
            children.push(
                self.link_field(
                    "jam-invite-link",
                    "Invite link",
                    "anyone with it can join",
                    link.clone(),
                    cx,
                )
                .into_any_element(),
            );
        } else if state.connected && !state.join_url.is_empty() {
            children.push(
                self.link_field(
                    "jam-join-link",
                    "Room link",
                    "opens the listener page",
                    state.join_url.clone(),
                    cx,
                )
                .into_any_element(),
            );
        }
        if !state.connected {
            children.push(self.join_field(state, window, cx).into_any_element());
        }

        let filled = !children.is_empty();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(space::BASE))
            .when(filled, |this| {
                this.px(px(space::SECTION))
                    .py(px(space::LOOSE))
                    .border_b(px(1.0))
                    .border_color(Colors::border_subtle())
            })
            .children(children)
    }

    /// A link, and one gesture to take it away with.
    ///
    /// The whole field is the button. A link long enough to be truncated is one
    /// nobody can retype off the screen, so reading it is not the point —
    /// copying it is, and a separate small target for that would be the only
    /// part of the row that works.
    fn link_field(
        &self,
        id: &'static str,
        label: &'static str,
        note: &'static str,
        link: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let copied = self.copied_recently(&link);
        let rest = Colors::surface_input();
        let hover = Colors::composite(rest, Colors::state_hover());
        let pressed = Colors::composite(rest, Colors::state_recessed());
        let focus = Colors::state_focus_ring();
        let clicked = link.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(typography::UI_XS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(Colors::text_muted())
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_faint())
                            .child(format!("— {note}")),
                    ),
            )
            .child(
                div()
                    .id(id)
                    .role(gpui::Role::Button)
                    .aria_label(format!("Copy the {label}"))
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .h(px(size_token::COMFORTABLE))
                    .px(px(space::BASE))
                    .rounded(px(radius::CONTROL))
                    .bg(rest)
                    .border_1()
                    .border_color(Colors::border_subtle())
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |style| style.bg(hover))
                    .active(move |style| style.bg(pressed))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.copy_link(clicked.clone(), cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::UI_SM))
                            .text_color(Colors::text_secondary())
                            .child(link),
                    )
                    .child(
                        // Two channels, because "Copy" and "Copied" differ by
                        // two letters at 11 px: the word changes and so does
                        // its colour.
                        div()
                            .flex_none()
                            .text_size(px(typography::UI_XS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if copied {
                                Colors::status_success()
                            } else {
                                Colors::text_muted()
                            })
                            .child(if copied { "Copied" } else { "Copy" }),
                    ),
            )
    }

    /// Somebody else's room, and the one gesture that gets into it.
    ///
    /// A field and a button rather than a dialog: the link arrives in a chat
    /// message and is pasted, and a modal between the clipboard and the room
    /// would be a step that exists only to be dismissed. The same field takes a
    /// bare code, because that is what gets read out loud.
    fn join_field(
        &self,
        state: &JamUiState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let can_join = state.signed_in && !self.link_input.value.trim().is_empty();

        div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_muted())
                    .child("Join a jam"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(div().flex_1().min_w(px(0.0)).child(text_field(
                        &self.link_input,
                        self.link_input.is_focused(window),
                    )))
                    .child(div().flex_none().child(fb_button(
                        "jam-join",
                        "Join",
                        FbButtonKind::Primary,
                        can_join,
                        cx.listener(|this, _event, _window, cx| {
                            this.join_with_link(cx);
                            cx.notify();
                        }),
                    ))),
            )
    }

    /// What this Studio is sending, and the room it is sending it to.
    fn body(&self, state: &JamUiState, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("jam-participants")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap(px(space::SECTION))
            .px(px(space::SECTION))
            .py(px(space::LOOSE))
            .overflow_y_scroll();

        if state.connected {
            list = list.child(self.sharing_section(state, cx));
            list = list.child(self.flow_section(state));
            list = list.child(self.quality_section(state, cx));
        }

        if state.participants.is_empty() {
            return list.child(
                div()
                    .py(px(space::BLOCK))
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_muted())
                    .child(if state.connected {
                        "Nobody else is here yet. Invite someone with the link above."
                    } else {
                        "Create a jam, or paste a link above, to start playing together."
                    }),
            );
        }

        let mut people = div().flex().flex_col().child(fb_section_header(format!(
            "Participants · {}",
            state.participants.len()
        )));
        for (index, (participant, streams)) in state.by_participant().into_iter().enumerate() {
            people = people.child(self.participant_row(participant, &streams, index > 0, cx));
        }
        list.child(people)
    }

    /// What this Studio can put into the room, each row showing what it is
    /// actually doing rather than only what can be pressed.
    ///
    /// The mode is a choice, not two independent switches, because the two
    /// streams are alternatives: Master is what a listener or a performer
    /// wants, and Multitrack is what another Studio wants when it is going to
    /// record the take. A segmented control says that; two Send buttons would
    /// invite sending both and paying twice for the same audio.
    fn sharing_section(&self, state: &JamUiState, cx: &mut Context<Self>) -> impl IntoElement {
        let publishing_master = state.publishing.iter().any(|key| key == "master");
        let publishing_multitrack = state.publishing.iter().any(|key| key == "multitrack");
        let sharing_track = state.publishing.iter().any(|key| key.starts_with("track:"));
        let mode = state.quality.stream_mode;
        let quality = state.quality.clone();

        let mut section = div()
            .flex()
            .flex_col()
            .child(fb_section_header("Sending from this Studio"))
            .child(self.mode_selector(mode, publishing_master || publishing_multitrack, cx));

        section = match mode {
            JamStreamMode::MasterStereo => section
                .child(share_row(
                    "Master mix",
                    if publishing_master {
                        "Live to everyone in the room".to_string()
                    } else {
                        StreamCost::of(&quality, 2).summary()
                    },
                    publishing_master,
                    fb_button(
                        "jam-publish-master",
                        if publishing_master {
                            "Stop"
                        } else {
                            "Send Master"
                        },
                        if publishing_master {
                            FbButtonKind::Default
                        } else {
                            FbButtonKind::Primary
                        },
                        true,
                        cx.listener(move |this, _event, _window, cx| {
                            this.set_master_publish(!publishing_master, cx);
                            cx.notify();
                        }),
                    ),
                ))
                // The metronome is part of the mix by default: a jam runs to a
                // count, and a guest playing to a mix with no pulse is the
                // commonest way a remote session falls apart. It is a choice
                // because the same stream also reaches an audience, who are not
                // playing along and do not want a click over the music.
                .child(fb_checkbox(
                    "jam-master-click",
                    "Include the metronome",
                    quality.master_click,
                    true,
                    cx.listener(move |this, _event, _window, cx| {
                        let next = JamPublishQuality {
                            master_click: !this.state.quality.master_click,
                            ..this.state.quality.clone()
                        };
                        this.set_quality(next, cx);
                    }),
                )),
            JamStreamMode::Multitrack => {
                let tracks = state.multitrack_tracks.len();
                let cost = StreamCost::of(&quality, tracks.max(1) * 2);
                section.child(share_row(
                    "Arrangement",
                    if publishing_multitrack {
                        format!(
                            "{tracks} tracks · {} channels · {}",
                            tracks * 2,
                            cost.summary()
                        )
                    } else {
                        "One channel pair per track, as one stream".to_string()
                    },
                    publishing_multitrack,
                    fb_button(
                        "jam-publish-multitrack",
                        if publishing_multitrack {
                            "Stop"
                        } else {
                            "Send Tracks"
                        },
                        if publishing_multitrack {
                            FbButtonKind::Default
                        } else {
                            FbButtonKind::Primary
                        },
                        true,
                        cx.listener(move |this, _event, _window, cx| {
                            this.set_multitrack_publish(!publishing_multitrack, cx);
                            cx.notify();
                        }),
                    ),
                ))
            }
        };

        // Sharing one selected track is neither mode: it is a second stream a
        // performer sends alongside whichever of the two is running, so it
        // keeps its own row under both.
        section.child(share_row(
            "Selected track",
            if sharing_track {
                "Live to everyone in the room".to_string()
            } else {
                "Not being sent".to_string()
            },
            sharing_track,
            fb_button(
                "jam-publish-track",
                if sharing_track { "Stop" } else { "Send Track" },
                FbButtonKind::Default,
                true,
                cx.listener(move |this, _event, _window, cx| {
                    this.publish_selected_track(!sharing_track, cx);
                    cx.notify();
                }),
            ),
        ))
    }

    /// Master or Multitrack.
    ///
    /// Disabled while either is live, because the channel layout is announced
    /// to every receiver once and cannot be edited underneath them: switching
    /// mode is a republish, and a control that silently dropped the room's
    /// audio to do it would be worse than one that asks to be stopped first.
    fn mode_selector(
        &self,
        mode: JamStreamMode,
        live: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let modes = [JamStreamMode::MasterStereo, JamStreamMode::Multitrack];
        let mut track = fb_segmented_track().w_full().mb(px(space::SNUG));
        for (index, candidate) in modes.into_iter().enumerate() {
            let position = if index == 0 {
                FbSegment::First
            } else {
                FbSegment::Last
            };
            track = track.child(div().flex_1().min_w(px(0.0)).child(fb_segment(
                gpui::ElementId::Name(format!("jam-mode-{index}").into()),
                candidate.label(),
                candidate == mode,
                position,
                cx.listener(move |this, _event, _window, cx| {
                    if this.state.quality.stream_mode == candidate {
                        return;
                    }
                    // The two streams are alternatives, so choosing the other
                    // one ends the first. Doing it here rather than refusing
                    // the click keeps the control honest: it does what it looks
                    // like it does, and the caption said in advance what that
                    // costs.
                    if live {
                        match this.state.quality.stream_mode {
                            JamStreamMode::MasterStereo => this.set_master_publish(false, cx),
                            JamStreamMode::Multitrack => this.set_multitrack_publish(false, cx),
                        }
                    }
                    let next = JamPublishQuality {
                        stream_mode: candidate,
                        ..this.state.quality.clone()
                    };
                    this.set_quality(next, cx);
                }),
            )));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .py(px(space::SNUG))
            .child(track)
            .child(
                div()
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(if live {
                        Colors::text_faint()
                    } else {
                        Colors::text_muted()
                    })
                    .child(if live {
                        "Switching stops the stream that is running".to_string()
                    } else {
                        mode.detail().to_string()
                    }),
            )
    }

    /// What is actually leaving this Studio right now.
    ///
    /// The controls above say what was asked for; this says what is happening.
    /// The two are not the same thing, and the gap between them is where every
    /// "I am in the room and nobody can hear me" goes: a send bound to a tap
    /// nothing feeds is announced, listed, and silent. A level that never moves
    /// is the symptom, so the level is the row.
    fn flow_section(&self, state: &JamUiState) -> impl IntoElement {
        let mut section = div().flex().flex_col().child(fb_section_header(format!(
            "Leaving this Studio · {}",
            state.sending.len()
        )));

        if state.sending.is_empty() {
            return section.child(
                div()
                    .py(px(space::BASE))
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child("Nothing is being sent. The room cannot hear this Studio."),
            );
        }

        for send in &state.sending {
            let silent = send.level <= 0.0001;
            section = section.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .mt(px(space::TIGHT))
                    .pl(px(space::BASE))
                    .border_l(px(2.0))
                    .border_color(if silent {
                        Colors::border_subtle()
                    } else {
                        Colors::accent_primary()
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(typography::UI_SM))
                                    .child(send.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(typography::UI_XS))
                                    .text_color(if silent {
                                        Colors::accent_warning()
                                    } else {
                                        Colors::text_muted()
                                    })
                                    .child(if silent {
                                        format!("{} · no signal", send.tap)
                                    } else {
                                        format!("{} · live", send.tap)
                                    }),
                            ),
                    )
                    .child(div().flex_none().child(level_meter(send.level))),
            );
        }
        section
    }

    /// Depth, rate, and what the two of them cost.
    ///
    /// There is no bitrate control because there is no compressed codec to
    /// control: this build publishes PCM, so the bitrate is arithmetic on the
    /// other choices rather than a knob. Showing the number is the honest form
    /// of the control the user came looking for — and for a wide layout it is
    /// the number that decides whether the stream is sendable at all.
    fn quality_section(&self, state: &JamUiState, cx: &mut Context<Self>) -> impl IntoElement {
        let quality = state.quality.clone();
        let channels = match quality.stream_mode {
            JamStreamMode::MasterStereo => 2,
            JamStreamMode::Multitrack => state.multitrack_tracks.len().max(1) * 2,
        };
        let cost = StreamCost::of(&quality, channels);
        let live = !state.publishing.is_empty();

        let mut depth = fb_segmented_track().w_full();
        for (index, format) in SAMPLE_FORMATS.into_iter().enumerate() {
            depth = depth.child(div().flex_1().min_w(px(0.0)).child(fb_segment(
                gpui::ElementId::Name(format!("jam-depth-{index}").into()),
                sample_format_label(format),
                format == quality.sample_format,
                segment_at(index, SAMPLE_FORMATS.len()),
                cx.listener(move |this, _event, _window, cx| {
                    let next = JamPublishQuality {
                        sample_format: format,
                        ..this.state.quality.clone()
                    };
                    this.set_quality(next, cx);
                }),
            )));
        }

        let mut rate = fb_segmented_track().w_full();
        for (index, hz) in SAMPLE_RATES.into_iter().enumerate() {
            rate = rate.child(div().flex_1().min_w(px(0.0)).child(fb_segment(
                gpui::ElementId::Name(format!("jam-rate-{index}").into()),
                sample_rate_label(hz),
                hz == quality.sample_rate,
                segment_at(index, SAMPLE_RATES.len()),
                cx.listener(move |this, _event, _window, cx| {
                    let next = JamPublishQuality {
                        sample_rate: hz,
                        ..this.state.quality.clone()
                    };
                    this.set_quality(next, cx);
                }),
            )));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(fb_section_header("Stream quality"))
            .child(quality_row("Depth", depth))
            .child(quality_row("Rate", rate))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(if cost.fits_datagram {
                                Colors::text_secondary()
                            } else {
                                Colors::status_error()
                            })
                            .child(format!("PCM · {} ch · {}", cost.channels, cost.summary())),
                    )
                    // A format not every client declares is a real choice, and
                    // an expensive one to discover by ear: the server does not
                    // transcode, so a listener that cannot take it is refused
                    // and then waits for a format that never comes. Beside the
                    // control is the only place that can be said before the
                    // fact — afterwards there is nothing on any screen that
                    // says it at all.
                    .children(web_listener_note(&quality).map(|note| {
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::status_warning())
                            .child(note)
                    }))
                    // The project's own rate never follows the jam, and saying
                    // so here is cheaper than the support question that follows
                    // from not saying it.
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_faint())
                            .child(if live {
                                "Applies to the next stream you send".to_string()
                            } else {
                                "The project's own sample rate is unaffected".to_string()
                            }),
                    ),
            )
    }

    /// One account in the room: a presence dot, the handle, and the streams it
    /// publishes. A list row, not a card — a hairline separates it from the
    /// next, so a busy room stays a list rather than a stack of boxes.
    fn participant_row(
        &self,
        participant: &ParticipantSummary,
        streams: &[&JamStreamView],
        divided: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let online = participant.connection_state == "connected";
        let dot = if online {
            Colors::status_success()
        } else {
            Colors::text_faint()
        };
        let mut meta = vec![if online { "Online" } else { "Offline" }.to_string()];
        if !participant.device_name.is_empty() {
            meta.push(participant.device_name.clone());
        }
        if !participant.role.is_empty() {
            meta.push(participant.role.clone());
        }

        let mut row = div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .py(px(space::BASE))
            .when(divided, |this| {
                this.border_t(px(1.0)).border_color(Colors::border_subtle())
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .flex_none()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded(px(radius::PILL))
                            .bg(dot),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(typography::UI_SM))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(participant.user.handle()),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(meta.join(" · ")),
            );

        if streams.is_empty() {
            row = row.child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child("Not publishing anything yet"),
            );
        }
        for stream in streams {
            row = row.child(self.stream_row(stream, cx));
        }
        row
    }

    /// One stream: its name, its format, its level, and the button that turns it
    /// into a track. The leading edge lights only while audio is arriving, which
    /// is the one place accent belongs here.
    fn stream_row(&self, stream: &JamStreamView, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = match stream.channels {
            0 | 1 => "Mono",
            2 => "Stereo",
            _ => "Multi",
        };
        let format = format!(
            "{} {} kHz · {}",
            stream.codec.to_uppercase(),
            stream.sample_rate as f32 / 1000.0,
            layout
        );
        let queued = stream.clone();
        let button_id = gpui::ElementId::Name(format!("jam-create-{}", stream.stream_id).into());

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(space::BASE))
            .mt(px(space::TIGHT))
            .pl(px(space::BASE))
            .border_l(px(2.0))
            .border_color(if stream.receiving {
                Colors::accent_primary()
            } else {
                Colors::border_subtle()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_SM))
                            .child(stream.stream_name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(format),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(stream_health_tone(stream))
                            .child(stream_health(stream)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(level_meter(stream.peak))
                    .child(fb_button(
                        button_id,
                        "Create Track",
                        FbButtonKind::Default,
                        stream.receiving,
                        cx.listener(move |this, _event, _window, cx| {
                            this.create_track(&queued, cx);
                            cx.notify();
                        }),
                    )),
            )
    }

    /// Status on the left, one primary action on the right. The publish
    /// controls live with the state they change, up in the body, so this row
    /// keeps a single meaning: what the session is doing, and how to start or
    /// end it.
    fn footer(&self, state: &JamUiState, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = state.connected;

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_none()
            .gap(px(space::BASE))
            .px(px(space::SECTION))
            .py(px(space::LOOSE))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(
                        self.busy
                            .as_ref()
                            .map(|(message, _)| message.clone())
                            .unwrap_or_else(|| state.state_label.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .gap(px(space::BASE))
                    .child(fb_button(
                        "jam-invite",
                        "Invite",
                        FbButtonKind::Default,
                        connected,
                        cx.listener(|this, _event, _window, cx| {
                            this.create_invite(cx);
                            cx.notify();
                        }),
                    ))
                    // One primary slot, two meanings: `fb_button` returns two
                    // different closure types, so the branch is resolved to
                    // `AnyElement` rather than left for the `if` to unify.
                    .child(if connected {
                        fb_button(
                            "jam-leave",
                            "Leave",
                            FbButtonKind::Danger,
                            true,
                            cx.listener(|this, _event, _window, cx| {
                                this.leave(cx);
                                cx.notify();
                            }),
                        )
                        .into_any_element()
                    } else {
                        fb_button(
                            "jam-create",
                            "Create Jam",
                            FbButtonKind::Primary,
                            state.signed_in,
                            cx.listener(|this, _event, _window, cx| {
                                this.create_jam(cx);
                                cx.notify();
                            }),
                        )
                        .into_any_element()
                    }),
            )
    }
}

/// A label over a tabular value, the way every other Studio readout is built.
fn stat(label: &str, value: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .min_w(px(0.0))
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child(label.to_string()),
        )
        .child(
            div()
                .truncate()
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_secondary())
                .child(value.to_string()),
        )
}

/// One row of the sending section: what it is, what it is doing, and the
/// control that changes it.
///
/// The detail is owned rather than borrowed because half these rows now report
/// a computed cost — a bitrate and a packet rate — rather than one of two fixed
/// phrases.
fn share_row(
    label: &'static str,
    detail: String,
    live: bool,
    action: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::BASE))
        .py(px(space::BASE))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .child(label),
                )
                // Live is said in a word as well as in colour; the row has no
                // other channel to carry it.
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(if live {
                            Colors::status_success()
                        } else {
                            Colors::text_muted()
                        })
                        .child(detail),
                ),
        )
        .child(div().flex_none().child(action))
}

/// A quality control under its label: the label at a fixed width so Depth and
/// Rate line up, the control taking the rest.
fn quality_row(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(
            div()
                .flex_none()
                .w(px(QUALITY_LABEL_WIDTH))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(div().flex_1().min_w(px(0.0)).child(control))
}

/// Where a segment sits in its track, so the ends round and the middles do not.
fn segment_at(index: usize, count: usize) -> FbSegment {
    match (index, count) {
        (_, 0 | 1) => FbSegment::Only,
        (0, _) => FbSegment::First,
        (index, count) if index + 1 == count => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

/// An inline strip rather than a wash: a tone edge, the kind said in a word,
/// and the message. Colour is never the only channel.
fn notice(kind: &'static str, message: &str, tone: gpui::Rgba) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(space::BASE))
        .p(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .bg(Colors::with_alpha(tone, 0.10))
        .border_1()
        .border_color(Colors::with_alpha(tone, 0.4))
        .child(
            div()
                .flex_none()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(tone)
                .child(kind),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_primary())
                .child(message.to_string()),
        )
}

/// A stream's level, square-cornered because the topmost lit pixel must *be*
/// the value.
fn level_meter(peak: f32) -> impl IntoElement {
    let filled = (peak.clamp(0.0, 1.0) * 40.0).round();
    div()
        .flex()
        .flex_row()
        .items_center()
        .w(px(40.0))
        .h(px(4.0))
        .bg(Colors::surface_canvas())
        .child(div().w(px(filled)).h(px(4.0)).bg(if peak > 0.98 {
            Colors::status_error()
        } else {
            Colors::accent_primary()
        }))
}

/// Whether a field should still be confirming a copy.
///
/// Free of the window so the rule can be tested: the confirmation belongs to
/// one link, and it expires. A newly minted invite therefore appears as
/// uncopied even though the field it replaced was copied a moment ago.
fn copied_recently(copied: Option<&(String, Instant)>, link: &str) -> bool {
    copied.is_some_and(|(copied, at)| copied == link && at.elapsed() < COPY_FEEDBACK)
}

/// Clock and packet counters, as one caption line.
///
/// Diagnostics, not identity: they answer "is the link healthy", and belong
/// under the readouts rather than beside them.
fn link_diagnostics(state: &JamUiState) -> String {
    let clock = if state.clock_locked {
        format!(
            "{:+.1} ms · {:+.1} ppm",
            state.clock_offset_ms, state.clock_drift_ppm
        )
    } else {
        "not locked".to_string()
    };
    format!(
        "Clock {clock}  ·  Packets {} in · {} out",
        state.packets_in, state.packets_out
    )
}

/// One line saying why a stream in the room is or is not audible here.
///
/// The four states are genuinely different problems and used to look identical:
/// nobody routed it, the server has not resolved a format, its ring is still
/// filling, or it is playing and has been dropping. A listener who cannot hear
/// a performer needs to know which one before they can do anything about it.
fn stream_health(stream: &JamStreamView) -> String {
    if !stream.routed {
        return "Not routed to a track — create one to hear it".to_string();
    }
    if !stream.receiving {
        return "Waiting for a format from the server".to_string();
    }
    if stream.buffering {
        return "Buffering".to_string();
    }
    if stream.dropouts > 0 {
        return format!("Playing · {} dropout(s)", stream.dropouts);
    }
    "Playing".to_string()
}

fn stream_health_tone(stream: &JamStreamView) -> gpui::Rgba {
    if !stream.routed || !stream.receiving {
        Colors::text_muted()
    } else if stream.buffering || stream.dropouts > 0 {
        Colors::accent_warning()
    } else {
        Colors::text_faint()
    }
}

fn status_tone(state: &JamUiState) -> (gpui::Rgba, &'static str) {
    if state.connected {
        (Colors::status_success(), "Connected")
    } else if state.state_label.starts_with("Reconnect") {
        (Colors::status_warning(), "Reconnecting")
    } else if state.state_label == "Failed" {
        (Colors::status_error(), "Failed")
    } else {
        (Colors::text_muted(), "Disconnected")
    }
}

fn or_dash(value: &str) -> String {
    if value.is_empty() {
        "—".to_string()
    } else {
        value.to_string()
    }
}

/// The name a track created from a stream gets: the performer and the stream,
/// which is what the person reading the arrangement later needs.
pub fn track_name_for(stream: &JamStreamView) -> String {
    let who = if stream.display_name.is_empty() {
        stream.handle.trim_start_matches('@').to_string()
    } else {
        stream.display_name.clone()
    };
    if who.is_empty() {
        stream.stream_name.clone()
    } else {
        format!("{who} - {}", stream.stream_name)
    }
}

fn default_jam_name() -> String {
    "Studio Jam".to_string()
}

/// Width of the Depth/Rate labels, so the two segmented tracks start on the
/// same edge. A shared constant rather than two literals, because the whole
/// point of the value is that both rows use it.
const QUALITY_LABEL_WIDTH: f32 = 44.0;

/// Open the jam panel as an application's own root window.
///
/// The same view, with different chrome, and the difference is not cosmetic.
/// Studio opens the panel as a dialog beside a project: not minimizable, no
/// separate taskbar entry, parented to the window it belongs to — all correct
/// for something that sits next to a document. A standalone client has no
/// document to sit next to; it *is* the window. Minimizing it, finding it in
/// the taskbar, and having it outlive nothing are ordinary expectations of an
/// application, and a dialog satisfies none of them.
pub fn open_jam_main_window(
    handlers: JamWindowHandlers,
    cx: &mut App,
) -> Result<WindowHandle<JamWindow>, String> {
    let mut options = crate::platform_chrome::studio_window_options();
    // `studio_window_options` opens hidden so a heavy first layout cannot show
    // a black client area. The jam panel's first frame is a header and a list;
    // there is nothing to hide behind, and staying hidden would look like a
    // launch that did nothing.
    options.show = true;
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        cx.primary_display().map(|display| display.bounds()),
        size(px(JAM_WINDOW_WIDTH), px(JAM_WINDOW_HEIGHT)),
        cx,
    )));

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| JamWindow::new(handlers, cx))
    })
    .map_err(|error| error.to_string())
}

pub fn open_jam_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    handlers: JamWindowHandlers,
    cx: &mut App,
) -> Result<WindowHandle<JamWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(JAM_WINDOW_WIDTH), px(JAM_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| JamWindow::new(handlers, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(handle: &str, display: &str, name: &str) -> JamStreamView {
        JamStreamView {
            stream_id: "str_1".to_string(),
            user_id: "usr_1".to_string(),
            device_id: "studio-mac".to_string(),
            handle: handle.to_string(),
            display_name: display.to_string(),
            avatar_url: String::new(),
            stream_name: name.to_string(),
            channels: 2,
            channel_labels: vec!["L".to_string(), "R".to_string()],
            sample_rate: 48_000,
            codec: "pcm".to_string(),
            receiving: true,
            peak: 0.0,
            rtt_ms: 0.0,
            routed: true,
            buffering: false,
            dropouts: 0,
        }
    }

    #[test]
    fn a_created_track_is_named_for_the_performer_and_the_stream() {
        assert_eq!(
            track_name_for(&stream("@hachi224", "Hachi", "Guitar")),
            "Hachi - Guitar"
        );
    }

    #[test]
    fn a_performer_with_no_display_name_falls_back_to_the_handle() {
        assert_eq!(
            track_name_for(&stream("@hachi224", "", "Guitar")),
            "hachi224 - Guitar"
        );
    }

    #[test]
    fn an_anonymous_stream_is_named_after_itself_rather_than_a_dangling_dash() {
        assert_eq!(track_name_for(&stream("", "", "Guitar")), "Guitar");
    }

    #[test]
    fn a_disconnected_panel_reports_disconnected_rather_than_blank() {
        let state = JamUiState::default();
        let (_, label) = status_tone(&state);
        assert_eq!(label, "Disconnected");
        assert_eq!(or_dash(""), "—");
    }

    #[test]
    fn a_copied_link_confirms_only_for_itself_and_only_for_a_while() {
        let link = "https://jam.futureboard.studio/j/EWEFDN#secret".to_string();
        let fresh = Some((link.clone(), Instant::now()));
        assert!(copied_recently(fresh.as_ref(), &link));

        // A different link is not the one that was copied, even a moment later.
        assert!(!copied_recently(
            fresh.as_ref(),
            "https://jam.futureboard.studio/j/OTHER#secret"
        ));

        // And the confirmation expires rather than standing for the session.
        let stale = Some((link.clone(), Instant::now() - COPY_FEEDBACK * 2));
        assert!(!copied_recently(stale.as_ref(), &link));
        assert!(!copied_recently(None, &link));
    }

    #[test]
    fn an_unlocked_clock_says_so_rather_than_reporting_a_zero_offset() {
        let state = JamUiState {
            packets_in: 2,
            packets_out: 3,
            ..Default::default()
        };
        assert_eq!(
            link_diagnostics(&state),
            "Clock not locked  ·  Packets 2 in · 3 out"
        );
    }

    #[test]
    fn a_reconnecting_session_is_shown_as_a_warning_not_a_failure() {
        let state = JamUiState {
            state_label: "Reconnecting".to_string(),
            ..Default::default()
        };
        let (_, label) = status_tone(&state);
        assert_eq!(label, "Reconnecting");
    }
}

#[cfg(test)]
mod flow_visibility_tests {
    use super::{stream_health, JamStreamView};

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

    /// "I cannot hear them" has four different causes and they used to look
    /// identical on screen. Each one has to say which it is, because the fix
    /// differs every time: route it, wait for the server, wait for the buffer,
    /// or look at the network.
    #[test]
    fn every_reason_a_stream_is_inaudible_reads_differently() {
        let unrouted = JamStreamView {
            routed: false,
            ..stream()
        };
        assert!(stream_health(&unrouted).contains("Not routed"));

        let no_format = JamStreamView {
            receiving: false,
            ..stream()
        };
        assert!(stream_health(&no_format).contains("format"));

        let buffering = JamStreamView {
            buffering: true,
            ..stream()
        };
        assert_eq!(stream_health(&buffering), "Buffering");

        let glitching = JamStreamView {
            dropouts: 7,
            ..stream()
        };
        assert!(stream_health(&glitching).contains('7'));

        assert_eq!(stream_health(&stream()), "Playing");
    }

    /// Routing is asked about first: an unrouted stream is not subscribed, so
    /// every other reading of it is a consequence rather than the cause.
    #[test]
    fn routing_is_reported_before_anything_downstream_of_it() {
        let nothing_works = JamStreamView {
            routed: false,
            receiving: false,
            buffering: true,
            dropouts: 99,
            ..stream()
        };
        assert!(stream_health(&nothing_works).contains("Not routed"));
    }
}

#[cfg(test)]
mod standalone_chrome_tests {
    /// This file's implementation, with the test modules stripped off, so a scan
    /// for banned constructs cannot match its own assertion literals.
    fn implementation_source() -> &'static str {
        let source = include_str!("jam_window.rs");
        source
            .split_once("#[cfg(test)]")
            .map(|(implementation, _)| implementation)
            .expect("this file has a test module")
    }

    /// The standalone client's root window is an application window, not a
    /// dialog. A dialog cannot be minimized, gets no taskbar entry of its own,
    /// and on Windows is parented to an owner — all correct for a panel beside
    /// a project, all wrong for the only window an application has.
    #[test]
    fn the_standalone_root_window_is_an_application_window() {
        let source = implementation_source();
        let (_, from_opener) = source
            .split_once("pub fn open_jam_main_window")
            .expect("the root-window opener");
        let (opener, _) = from_opener
            .split_once("pub fn open_jam_window")
            .expect("the dialog opener follows it");

        assert!(
            opener.contains("studio_window_options"),
            "the root window must take the ordinary application chrome"
        );
        assert!(
            !opener.contains("external_dialog_window_options_partial"),
            "a dialog is not an application window"
        );
        assert!(
            opener.contains("options.show = true"),
            "a launch that stays hidden reads as a launch that did nothing"
        );
    }

    /// Both openers build the same view. If they ever stop, the standalone
    /// client has quietly become a second implementation of the panel — which
    /// is the one thing it exists not to be.
    #[test]
    fn both_openers_build_the_same_panel() {
        let source = implementation_source();
        let constructions = source.matches("JamWindow::new(handlers, cx)").count();
        assert_eq!(
            constructions, 2,
            "the dialog and the root window must construct one and the same view"
        );
    }
}
