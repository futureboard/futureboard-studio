//! Audio Routing Matrix — native "Audio Connections" dialog.
//!
//! Renders a cross-point grid of source tracks (rows) against routing
//! destinations (columns: Main + every Bus/Return track). Each cell shows the
//! track's primary output (read-only marker) and its aux sends (toggle a send
//! on/off by clicking the cell).
//!
//! Like [`crate::components::MixerWindow`], this view renders from a cloned
//! [`RoutingMatrixSnapshot`] and never reads `StudioLayout` during `Render`
//! (avoids GPUI entity re-entrancy while the main studio is updating). All state
//! mutations are routed back to the owner through the `on_toggle_send` callback,
//! which the owner runs via `defer_update` and then pushes a refreshed snapshot.

use std::sync::Arc;

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, StatefulInteractiveElement, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::timeline::timeline_state::{
    is_project_routing_track, TrackOutputRouting, TrackState, TrackType,
};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{self, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const ROUTING_MATRIX_WINDOW_WIDTH: f32 = 880.0;
pub const ROUTING_MATRIX_WINDOW_HEIGHT: f32 = 520.0;
pub const ROUTING_MATRIX_WINDOW_MIN_WIDTH: f32 = 520.0;
pub const ROUTING_MATRIX_WINDOW_MIN_HEIGHT: f32 = 320.0;

const SOURCE_COL_W: f32 = 200.0;
const DEST_COL_W: f32 = 96.0;
const ROW_H: f32 = 28.0;
const HEADER_H: f32 = 44.0;

/// Callback: `(source_track_id, destination_track_id)` — toggle an aux send from
/// the source track to the routing destination. The owner adds or removes the
/// send, marks the project dirty, and pushes a refreshed snapshot.
pub type ToggleSendCb = Arc<dyn Fn(String, String, &mut Window, &mut App) + Send + Sync>;

/// A routing destination column: `Main` (the master output) or a Bus/Return
/// track that can receive both output routing and aux sends.
#[derive(Clone)]
struct Destination {
    /// `None` for the Main/master column; `Some(track_id)` for a Bus/Return.
    track_id: Option<String>,
    name: String,
    /// `true` when this destination can receive aux sends (Bus/Return only).
    accepts_sends: bool,
}

/// View-model for the routing matrix — cloned from the studio timeline.
#[derive(Clone)]
pub struct RoutingMatrixSnapshot {
    pub tracks: Vec<TrackState>,
}

pub struct RoutingMatrixWindow {
    snapshot: RoutingMatrixSnapshot,
    on_toggle_send: ToggleSendCb,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    focus_handle: FocusHandle,
}

impl RoutingMatrixWindow {
    pub fn new(
        snapshot: RoutingMatrixSnapshot,
        on_toggle_send: ToggleSendCb,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            snapshot,
            on_toggle_send,
            on_close,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_snapshot(&mut self, snapshot: RoutingMatrixSnapshot) {
        self.snapshot = snapshot;
    }

    /// Destination columns: Main + every Bus/Return track, in track order.
    fn destinations(&self) -> Vec<Destination> {
        let mut dests = vec![Destination {
            track_id: None,
            name: "Main".to_string(),
            accepts_sends: false,
        }];
        for track in &self.snapshot.tracks {
            if is_project_routing_track(track) {
                dests.push(Destination {
                    track_id: Some(track.id.clone()),
                    name: track.name.clone(),
                    accepts_sends: true,
                });
            }
        }
        dests
    }
}

/// Whether a track's primary output points at the given destination.
fn output_targets(track: &TrackState, dest: &Destination) -> bool {
    match (&track.routing.output, &dest.track_id) {
        (TrackOutputRouting::Main, None) => true,
        (TrackOutputRouting::Bus { bus_id }, Some(dest_id)) => bus_id == dest_id,
        _ => false,
    }
}

impl Render for RoutingMatrixWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window, cx);
        }

        let destinations = self.destinations();
        let on_toggle_send = self.on_toggle_send.clone();
        let on_close = self.on_close.clone();

        // Header: source-column corner + destination labels.
        let mut header = div()
            .flex()
            .flex_row()
            .h(px(HEADER_H))
            .bg(Colors::surface_panel())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex()
                    .items_end()
                    .w(px(SOURCE_COL_W))
                    .h_full()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(10.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_muted())
                    .child("Source \\ Destination"),
            );
        for dest in &destinations {
            header = header.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_end()
                    .w(px(DEST_COL_W))
                    .h_full()
                    .px(px(4.0))
                    .py(px(6.0))
                    .border_l(px(1.0))
                    .border_color(Colors::border_subtle())
                    .text_size(px(10.5))
                    .text_color(if dest.accepts_sends {
                        Colors::text_secondary()
                    } else {
                        Colors::text_primary()
                    })
                    .child(div().overflow_hidden().truncate().child(dest.name.clone())),
            );
        }

        // Body rows: one per non-master track.
        let source_tracks: Vec<TrackState> = self
            .snapshot
            .tracks
            .iter()
            .filter(|t| t.track_type != TrackType::Master)
            .cloned()
            .collect();

        let mut rows = div().flex().flex_col().min_h_0();
        for (index, track) in source_tracks.iter().enumerate() {
            let alt = index % 2 == 1;
            let mut row = div()
                .flex()
                .flex_row()
                .h(px(ROW_H))
                .bg(if alt {
                    Colors::surface_raised()
                } else {
                    Colors::surface_base()
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .w(px(SOURCE_COL_W))
                        .h_full()
                        .px(px(10.0))
                        .child(
                            div()
                                .w(px(9.0))
                                .h(px(9.0))
                                .rounded(px(crate::theme::radius::PILL))
                                .bg(track.color),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(Colors::text_primary())
                                .child(track.name.clone()),
                        ),
                );

            for dest in &destinations {
                let is_output = output_targets(track, dest);
                let has_send = dest
                    .track_id
                    .as_ref()
                    .map(|dest_id| track.sends.iter().any(|s| &s.target_track_id == dest_id))
                    .unwrap_or(false);
                // A send is toggleable from any non-master source into a
                // Bus/Return destination that is not the source itself.
                // Bus/return → bus/return chains are allowed; the engine rejects
                // cycles when the graph is planned.
                let toggleable =
                    dest.accepts_sends && dest.track_id.as_deref() != Some(track.id.as_str());

                let mut cell = div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(DEST_COL_W))
                    .h_full()
                    .border_l(px(1.0))
                    .border_color(Colors::border_subtle());

                let marker = if is_output {
                    // Primary output marker (read-only in this slice).
                    Some(
                        div()
                            .w(px(14.0))
                            .h(px(14.0))
                            .rounded(px(crate::theme::radius::PILL))
                            .border(px(2.0))
                            .border_color(Colors::accent_primary())
                            .bg(Colors::accent_soft()),
                    )
                } else if has_send {
                    Some(
                        div()
                            .w(px(12.0))
                            .h(px(12.0))
                            .rounded(px(crate::theme::radius::MICRO))
                            .bg(Colors::status_success()),
                    )
                } else if toggleable {
                    // Empty, addable cell — faint dot affordance.
                    Some(
                        div()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded(px(crate::theme::radius::PILL))
                            .bg(Colors::with_alpha(Colors::text_muted(), 0.35)),
                    )
                } else {
                    None
                };

                if let Some(marker) = marker {
                    cell = cell.child(marker);
                }

                if toggleable {
                    let source_id = track.id.clone();
                    let dest_id = dest.track_id.clone().expect("send target has a track id");
                    let on_toggle_send = on_toggle_send.clone();
                    cell = cell
                        .cursor(gpui::CursorStyle::PointingHand)
                        // Hover feedback: highlight the cell the click would
                        // toggle. Empty (addable) cells otherwise show only a
                        // faint dot, so without this the interactive target is
                        // ambiguous. Read-only output / unavailable cells are not
                        // toggleable and get no hover, keeping the distinction.
                        .hover(|style| style.bg(Colors::surface_hover()))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            on_toggle_send(source_id.clone(), dest_id.clone(), window, cx);
                        });
                }

                row = row.child(cell);
            }

            rows = rows.child(row);
        }

        let empty_state = if source_tracks.is_empty() {
            Some(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(120.0))
                    .text_size(px(12.0))
                    .text_color(Colors::text_muted())
                    .child("No tracks to route"),
            )
        } else {
            None
        };

        let legend = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .h(px(26.0))
            .px(px(12.0))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .text_size(px(10.0))
            .text_color(Colors::text_muted())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(11.0))
                            .h(px(11.0))
                            .rounded(px(crate::theme::radius::PILL))
                            .border(px(2.0))
                            .border_color(Colors::accent_primary())
                            .bg(Colors::accent_soft()),
                    )
                    .child("Output"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(10.0))
                            .h(px(10.0))
                            .rounded(px(crate::theme::radius::MICRO))
                            .bg(Colors::status_success()),
                    )
                    .child("Send (click a cell to toggle)"),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_window())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|_this, event: &gpui::KeyDownEvent, window, _cx| {
                    if event.keystroke.key.as_str() == "escape" {
                        window.remove_window();
                    }
                }),
            )
            .child(external_window_titlebar(
                "Audio Connections",
                "routing-matrix-window-close",
                move |window, cx| {
                    on_close(window, cx);
                    window.remove_window();
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(header)
                    .child(
                        div()
                            .id("routing-matrix-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(rows)
                            .children(empty_state),
                    ),
            )
            .child(legend)
    }
}

pub fn open_routing_matrix_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    snapshot: RoutingMatrixSnapshot,
    on_toggle_send: ToggleSendCb,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<RoutingMatrixWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(
            px(ROUTING_MATRIX_WINDOW_WIDTH),
            px(ROUTING_MATRIX_WINDOW_HEIGHT),
        ),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(ROUTING_MATRIX_WINDOW_MIN_WIDTH),
        px(ROUTING_MATRIX_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|cx| RoutingMatrixWindow::new(snapshot, on_toggle_send, on_close, cx))
    })
    .map_err(|error| error.to_string())
}
