//! Split out of `piano_roll.rs` (god-file decomposition). These are
//! `impl PianoRoll` extension blocks; `use super::*` pulls in the shared
//! piano-roll vocabulary (struct fields via the type, consts, free fns).

use super::*;
use gpui::{ExternalPaths, PathBuilder, PathStyle, StrokeOptions};
use std::collections::HashMap;

impl PianoRoll {
    pub(super) fn display_note(&self, n: &MidiNoteState) -> DisplayNote {
        let mut start = n.start;
        let mut pitch = n.pitch;
        let mut duration = n.duration;
        match &self.drag {
            PianoDrag::Move {
                ids,
                dx_beats,
                dpitch,
                anchor_start,
                unsnap,
                ..
            } => {
                // A hash lookup: this runs for every visible note on every
                // frame, and a scan of the moved notes made a select-all drag
                // O(notes × selected).
                if ids.contains(&n.id) {
                    // Snap only the grabbed anchor, then apply its delta to every
                    // peer so an off-grid multi-selection keeps internal spacing.
                    let snapped_anchor = self.snap_beats_live(*anchor_start + *dx_beats, *unsnap);
                    start = (n.start + snapped_anchor - *anchor_start).max(0.0);
                    pitch = self.moved_pitch(n.pitch, *dpitch);
                }
            }
            PianoDrag::Resize {
                prev_durs,
                delta_dur,
                ..
            } => {
                if let Some(prev_duration) = prev_durs.get(&n.id) {
                    duration = (*prev_duration + *delta_dur).max(MIN_NOTE_BEATS);
                }
            }
            PianoDrag::Stretch { ids, spec, .. } => {
                if ids.contains(&n.id) {
                    (start, duration) = spec.map(n.start, n.duration);
                }
            }
            _ => {}
        }
        DisplayNote {
            id: n.id,
            pitch,
            start,
            duration,
            velocity: n.velocity,
        }
    }

    pub(super) fn note_to_rect(&self, note: &DisplayNote) -> (f32, f32, f32, f32) {
        if self.drum_mode {
            // A hit is a diamond at its start: drums have no length to read.
            let (cx, cy, size) = self.drum_hit_geometry(note);
            let half = size * 0.5;
            return (cx - half, cy - half, cx + half, cy + half);
        }
        let x = self.clip_beat_to_x(note.start);
        let w = (note.duration * self.ppb).max(NOTE_MIN_W);
        let y = self.pitch_to_y(note.pitch) + 1.0;
        let h = self.note_row_h() - 2.0;
        (x, y, x + w, y + h)
    }

    /// Centre x, centre y and size of a drum hit's diamond, grid-local.
    pub(super) fn drum_hit_geometry(&self, note: &DisplayNote) -> (f32, f32, f32) {
        let row_h = self.note_row_h();
        let size = (row_h - 4.0).clamp(6.0, 18.0);
        let x = self.clip_beat_to_x(note.start);
        let y = self.pitch_to_y(note.pitch) + row_h * 0.5;
        (x, y, size)
    }

    pub(super) fn marquee_hits(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
        marquee: (f32, f32, f32, f32),
    ) -> HashSet<u64> {
        let tl = self.timeline.read(cx);
        let Some(notes) = tl.state.midi_clip_notes(clip_id) else {
            return HashSet::new();
        };
        notes
            .iter()
            .filter(|n| self.channel_visible(n.channel))
            .filter(|n| {
                let d = self.display_note(n);
                Self::rects_intersect(marquee, self.note_to_rect(&d))
            })
            .map(|n| n.id)
            .collect()
    }

    /// The note being drawn, painted as the note it will commit as: a new
    /// note lands selected, so it shows in the track colour at full strength
    /// with the accent edge from the press. A faint ghost here read as "not
    /// there yet", and the solid note only arrived with the release.
    pub(super) fn build_draw_note_preview(&self, track_color: gpui::Rgba) -> Vec<gpui::AnyElement> {
        let PianoDrag::DrawNote {
            pitch,
            start_beat,
            end_beat,
            unsnap,
            ..
        } = &self.drag
        else {
            return Vec::new();
        };
        let (lo, hi) = normalize_range(*start_beat, *end_beat);
        let duration = self.drawn_note_length(lo, hi, *unsnap);
        let x = self.clip_beat_to_x(lo);
        let w = (duration * self.ppb).max(3.0);
        let h = self.note_row_h() - 2.0;
        let mut fill = track_color;
        fill.a = 1.0;
        self.chord_draw_pitches(*pitch)
            .into_iter()
            .map(|chord_pitch| {
                let y = self.pitch_to_y(chord_pitch);
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y + 1.0))
                    .w(px(w))
                    .h(px(h))
                    .rounded(px(crate::theme::radius::clamped(
                        crate::theme::radius::MICRO,
                        w,
                        h,
                    )))
                    .bg(fill)
                    .border(px(1.5))
                    .border_color(Colors::accent_primary())
                    .into_any_element()
            })
            .collect()
    }

    pub(super) fn build_erase_overlay(&self) -> Option<gpui::AnyElement> {
        let PianoDrag::EraseNotes {
            start_x,
            start_y,
            current_x,
            current_y,
            ..
        } = &self.drag
        else {
            return None;
        };
        let (view_w, view_h) = self.grid_view_size();
        let (left, top, right, bottom) = Self::normalized_marquee_rect(
            *start_x, *start_y, *current_x, *current_y, view_w, view_h,
        );
        let w = (right - left).max(0.0);
        let h = (bottom - top).max(0.0);
        if w < 1.0 && h < 1.0 {
            return None;
        }
        Some(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(w.max(1.0)))
                .h(px(h.max(1.0)))
                .bg(Colors::with_alpha(Colors::status_error(), 0.12))
                .border(px(1.0))
                .border_color(Colors::with_alpha(Colors::status_error(), 0.75))
                .into_any_element(),
        )
    }

    fn build_velocity_context_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (lx, ly) = self.open_velocity_menu?;
        let mut panel = div()
            .absolute()
            .left(px(lx.clamp(4.0, 260.0)))
            .top(px(ly.clamp(4.0, 28.0)))
            .w(px(150.0))
            .max_h(px(LANE_H - 8.0))
            .id("pr-velocity-menu")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p(px(3.0))
            .gap(px(1.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_card())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
            .child(
                div()
                    .px(px(7.0))
                    .py(px(3.0))
                    .text_size(px(9.0))
                    .text_color(Colors::text_muted())
                    .child("Velocity"),
            );
        for (index, operation) in VelocityOperation::ALL.iter().enumerate() {
            let operation = *operation;
            panel = panel.child(
                div()
                    .id(("pr-velocity-operation", index))
                    .flex()
                    .items_center()
                    .h(px(18.0))
                    .px(px(7.0))
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .text_size(px(10.0))
                    .text_color(Colors::text_secondary())
                    .hover(|style| style.bg(Colors::surface_hover()))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(operation.label())
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.apply_velocity_operation(operation, cx);
                    })),
            );
        }
        Some(
            deferred(panel.into_any_element())
                .with_priority(PIANO_ROLL_MENU_PRIORITY)
                .into_any_element(),
        )
    }

    /// Right-click menu for the selected notes, anchored at the click.
    ///
    /// This is the in-context articulation affordance: articulation is a note
    /// property, so it is reachable from the note itself on every track —
    /// including Solfege tracks, where the built-in velocity/controller lanes
    /// stand down. The palette is capability-filtered exactly like the
    /// inspector's row, and the current value is marked.
    fn build_note_context_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (lx, ly) = self.open_note_menu?;
        let (view_w, view_h) = self.grid_view_size();
        let menu_w = 168.0_f32;
        let menu_h = 300.0_f32;
        let available = self.available_articulations(cx);
        let current = self.uniform_selection_articulation(cx);
        let selected_count = self.selection.len();
        let all_muted = self.selection_all_muted(cx);

        // One row shape for the whole menu: a check column that shows which
        // value the selection already carries, then the label. The press sits
        // on the row so the whole width is the target.
        let row = |id: (&'static str, usize),
                   label: String,
                   marked: bool,
                   destructive: bool,
                   on_click: Box<
            dyn Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
        >| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .h(px(20.0))
                .px(px(7.0))
                .rounded(px(crate::theme::radius::CONTROL_SM))
                .text_size(px(10.0))
                .text_color(if destructive {
                    Colors::status_error()
                } else if marked {
                    Colors::accent_primary()
                } else {
                    Colors::text_secondary()
                })
                .hover(|s| s.bg(Colors::surface_hover()))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |ev, window, cx| on_click(ev, window, cx))
                .child(
                    div()
                        .w(px(8.0))
                        .flex_none()
                        .text_size(px(9.0))
                        .child(if marked { "\u{2713}" } else { "" }),
                )
                .child(label)
        };
        let heading = |text: &'static str| {
            div()
                .px(px(7.0))
                .pt(px(4.0))
                .pb(px(2.0))
                .text_size(px(8.5))
                .text_color(Colors::text_faint())
                .child(text)
        };

        let mut panel = div()
            .id("pr-note-menu")
            .absolute()
            // Clamp to the grid so a right-click near an edge keeps the whole
            // menu on screen.
            .left(px(lx.clamp(2.0, (view_w - menu_w).max(2.0))))
            .top(px(ly.clamp(2.0, (view_h - menu_h).max(2.0))))
            .w(px(menu_w))
            .max_h(px(menu_h))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p(px(3.0))
            .gap(px(1.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_card())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .shadow_lg()
            // `occlude` keeps the press off what is painted beneath; the grid's
            // draw listener is an *ancestor*, so it also has to be stopped.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
            .child(heading("Articulation"));

        for (index, articulation) in available.into_iter().enumerate() {
            panel = panel.child(row(
                ("pr-note-menu-art", index),
                articulation.name().to_string(),
                current == Some(Some(articulation)),
                false,
                Box::new(cx.listener(move |this, _ev: &gpui::ClickEvent, _w, cx| {
                    this.set_selection_articulation(Some(articulation), cx);
                    this.open_note_menu = None;
                })),
            ));
        }
        panel = panel.child(row(
            ("pr-note-menu-art", usize::MAX),
            "None".to_string(),
            current == Some(None),
            false,
            Box::new(cx.listener(|this, _ev: &gpui::ClickEvent, _w, cx| {
                this.set_selection_articulation(None, cx);
                this.open_note_menu = None;
            })),
        ));

        panel = panel.child(heading("Note")).child(row(
            ("pr-note-menu-cmd", 0),
            if all_muted { "Unmute" } else { "Mute" }.to_string(),
            all_muted,
            false,
            Box::new(cx.listener(|this, _ev: &gpui::ClickEvent, _w, cx| {
                this.toggle_mute_selection(cx);
                this.open_note_menu = None;
            })),
        ));
        for (index, (label, delta)) in [("Velocity +5", 5_i16), ("Velocity -5", -5)]
            .into_iter()
            .enumerate()
        {
            // Stays open: velocity nudges are repeated, and reopening the menu
            // between each press is the wrong rhythm for that.
            panel = panel.child(row(
                ("pr-note-menu-cmd", index + 1),
                label.to_string(),
                false,
                false,
                Box::new(cx.listener(move |this, _ev: &gpui::ClickEvent, _w, cx| {
                    this.nudge_selected_velocity(delta, cx);
                })),
            ));
        }
        panel = panel.child(row(
            ("pr-note-menu-cmd", 3),
            format!("Delete {selected_count} note{}", plural(selected_count)),
            false,
            true,
            Box::new(cx.listener(|this, _ev: &gpui::ClickEvent, _w, cx| {
                this.open_note_menu = None;
                this.delete_selection(cx);
            })),
        ));

        Some(
            deferred(panel.into_any_element())
                .with_priority(PIANO_ROLL_MENU_PRIORITY)
                .into_any_element(),
        )
    }

    /// The drum lane menu, drawn, where the platform has no native popup
    /// menus: the lane's row actions at the right-click. A press outside it
    /// or Escape closes it.
    fn build_lane_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (pitch, position) = self.open_lane_menu?;
        let row = |id: usize, label: &'static str, destructive: bool, action: LaneAction| {
            let hover = Colors::composite(Colors::surface_card(), Colors::state_hover());
            div()
                .id(("pr-lane-menu", id))
                .flex()
                .items_center()
                .h(px(22.0))
                .px(px(8.0))
                .rounded(px(crate::theme::radius::CONTROL_SM))
                .text_size(px(10.0))
                .text_color(if destructive {
                    Colors::status_error()
                } else {
                    Colors::text_secondary()
                })
                .hover(move |s| s.bg(hover))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _w, cx| {
                    this.run_lane_action(pitch, action, cx);
                }))
                .child(label)
        };
        let panel = div()
            .id("pr-lane-menu")
            .w(px(176.0))
            .flex()
            .flex_col()
            .p(px(3.0))
            .gap(px(1.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_card())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                this.open_lane_menu = None;
                cx.notify();
            }))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(4.0))
                    .pb(px(2.0))
                    .text_size(px(8.5))
                    .text_color(Colors::text_faint())
                    .child(self.step_row_name(pitch)),
            )
            .child(row(
                0,
                "Fill Every 2 Steps",
                false,
                LaneAction::FillEvery(2),
            ))
            .child(row(
                1,
                "Fill Every 4 Steps",
                false,
                LaneAction::FillEvery(4),
            ))
            .child(row(
                2,
                "Randomize Velocity (±15%)",
                false,
                LaneAction::RandomizeVelocity,
            ))
            .child(
                div()
                    .h(px(1.0))
                    .mx(px(4.0))
                    .my(px(2.0))
                    .bg(Colors::border_subtle()),
            )
            .child(row(3, "Clear Row", true, LaneAction::Clear));
        Some(
            deferred(
                gpui::anchored()
                    .position(position)
                    .snap_to_window_with_margin(px(crate::theme::space::BASE))
                    .child(panel),
            )
            .with_priority(PIANO_ROLL_MENU_PRIORITY)
            .into_any_element(),
        )
    }

    pub(super) fn build_velocity_gesture_overlay(&self) -> Option<gpui::AnyElement> {
        match &self.drag {
            PianoDrag::VelocitySelect {
                start_x,
                start_y,
                current_x,
                current_y,
                dragging: true,
                ..
            } => {
                let (view_w, view_h) = self.cc_view_size();
                let (left, top, right, bottom) = Self::normalized_marquee_rect(
                    *start_x, *start_y, *current_x, *current_y, view_w, view_h,
                );
                Some(
                    div()
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px((right - left).max(1.0)))
                        .h(px((bottom - top).max(1.0)))
                        .bg(Colors::with_alpha(Colors::accent_primary(), 0.15))
                        .border(px(1.0))
                        .border_color(Colors::with_alpha(Colors::accent_primary(), 0.85))
                        .into_any_element(),
                )
            }
            PianoDrag::VelocityLine {
                anchor_beat,
                anchor_value,
                current_beat,
                current_value,
                ..
            } => {
                let x0 = self.clip_beat_to_x(*anchor_beat);
                let x1 = self.clip_beat_to_x(*current_beat);
                let (_, lane_h) = self.cc_view_size();
                let usable_h = (lane_h - 8.0).max(1.0);
                let y0 = 2.0 + (1.0 - (*anchor_value as f32 - 1.0) / 126.0) * usable_h;
                let y1 = 2.0 + (1.0 - (*current_value as f32 - 1.0) / 126.0) * usable_h;
                let color = Colors::accent_primary();
                Some(
                    canvas(
                        |_bounds, _window, _cx| {},
                        move |bounds: Bounds<Pixels>, (), window, _cx| {
                            let steps = (x1 - x0).abs().ceil().max(1.0) as usize;
                            for index in 0..=steps {
                                let t = index as f32 / steps as f32;
                                let x = x0 + (x1 - x0) * t;
                                let y = y0 + (y1 - y0) * t;
                                window.paint_quad(fill(
                                    Bounds::new(
                                        bounds.origin + point(px(x), px(y)),
                                        size(px(2.0), px(2.0)),
                                    ),
                                    color,
                                ));
                            }
                        },
                    )
                    .absolute()
                    .inset_0()
                    .into_any_element(),
                )
            }
            _ => None,
        }
    }

    pub(super) fn build_marquee_overlay(&self) -> Option<gpui::AnyElement> {
        let PianoDrag::MarqueeSelect {
            start_x,
            start_y,
            current_x,
            current_y,
            dragging: true,
            ..
        } = &self.drag
        else {
            return None;
        };

        let (view_w, view_h) = self.grid_view_size();
        let (left, top, right, bottom) = Self::normalized_marquee_rect(
            *start_x, *start_y, *current_x, *current_y, view_w, view_h,
        );
        let w = (right - left).max(0.0);
        let h = (bottom - top).max(0.0);
        if w < 1.0 || h < 1.0 {
            return None;
        }

        Some(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(w))
                .h(px(h))
                .bg(Colors::with_alpha(Colors::accent_primary(), 0.15))
                .border(px(1.0))
                .border_color(Colors::with_alpha(Colors::accent_primary(), 0.85))
                .into_any_element(),
        )
    }
}

impl Render for PianoRoll {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window_scale = window.scale_factor();
        // The piano roll had no perf scope at all, so `FUTUREBOARD_UI_PERF=1`
        // attributed every repaint it caused to the arrangement's `Timeline`
        // scope and none of its own work to anything.
        let _scope = crate::perf::PerfScope::enter("PianoRoll");
        crate::perf::count("piano_roll_paint_count", 1);
        if self.focus_lost_subscription.is_none() {
            self.focus_lost_subscription = Some(cx.on_focus_lost(window, |this, _window, cx| {
                if !matches!(this.drag, PianoDrag::None) || this.active_preview_note.is_some() {
                    this.cancel_active_gesture(cx);
                }
            }));
        }

        self.refresh_drum_rows(cx);
        self.refresh_rack_rows(cx);
        let clip_id = self.editing_clip_id(cx);
        if clip_id != self.last_editing_clip && !matches!(self.drag, PianoDrag::None) {
            self.cancel_active_gesture(cx);
        }
        self.prune_transient_state(cx, clip_id.as_deref());
        // Follow-playhead and the wheel scroll without a pointer move: keep a
        // note drag under the cursor through it.
        self.refresh_drag_from_pointer();
        self.drop_stale_tool_session(cx);

        if clip_id != self.last_editing_clip {
            // Editing target changed (clip/track switch) — stop any audition note
            // before it strands on the previous track's instrument.
            if self.active_preview_note.is_some() {
                self.preview_all_notes_off("clip_change", cx);
            }
            if midi_debug_enabled() {
                if let Some(cid) = clip_id.as_deref() {
                    let tl = self.timeline.read(cx);
                    let track_id = tl
                        .state
                        .tracks
                        .iter()
                        .find(|t| t.clips.iter().any(|c| c.id == cid))
                        .map(|t| t.id.as_str())
                        .unwrap_or("<none>");
                    let notes = tl.state.midi_clip_notes(cid).map(|n| n.len()).unwrap_or(0);
                    eprintln!(
                        "[midi] open_editor clip_id={} track_id={} notes={}",
                        cid, track_id, notes
                    );
                }
            }
            self.last_editing_clip = clip_id.clone();
            self.fitted_clip_id = None;
            // A DrumStep track's clips open on the step grid, and leaving one
            // for any other track goes back to the piano roll. Only a change
            // of kind switches the view, so a view picked by hand stays while
            // the user moves between clips of the same kind.
            let drum_step = clip_id.as_deref().is_some_and(|cid| {
                self.timeline
                    .read(cx)
                    .state
                    .find_clip(cid)
                    .is_some_and(|(track, _)| track.drum_step)
            });
            if drum_step != self.on_drum_step_track {
                self.on_drum_step_track = drum_step;
                if drum_step {
                    self.set_step_mode(true, cx);
                } else {
                    self.set_drum_mode(false, cx);
                }
            }
        }

        // The frame every conversion is measured in, resolved *before* the fit
        // rather than after it: `fit_piano_roll_to_notes` places its scroll
        // through the edited clip's origin, and with the refresh further down
        // it was fitting the new clip against the previous clip's origin.
        match clip_id.as_deref() {
            Some(cid) => self.refresh_scope(cx, cid),
            None => {
                self.scope = crate::components::piano_roll::scope::EditorScope::default();
                self.edit_origin_beats = 0.0;
            }
        }

        if let Some(cid) = clip_id.as_deref() {
            if self.fitted_clip_id.as_deref() != Some(cid) {
                self.fit_piano_roll_to_notes(cx, cid);
                self.fitted_clip_id = Some(cid.to_string());
            }
        }

        // Toolbar and footer are always shown; the body shows a hint when no
        // MIDI clip is selected.
        let toolbar = self.render_toolbar(cx, clip_id.as_deref());
        let footer = self.render_footer(cx, clip_id.as_deref());

        let body: gpui::AnyElement = match clip_id {
            Some(cid) => self.render_body(cx, &cid).into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(crate::theme::typography::UI_XS))
                .text_color(Colors::text_muted())
                .child("Select or double-click a MIDI clip to edit")
                .into_any_element(),
        };

        div()
            .key_context("PianoRoll")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .bg(Colors::surface_base())
            .cursor(if matches!(self.drag, PianoDrag::Pan { .. }) {
                gpui::CursorStyle::ClosedHand
            } else {
                gpui::CursorStyle::Arrow
            })
            .on_key_down(cx.listener(Self::on_key))
            // A press anywhere in the editor keeps an open tool's result
            // before the press does anything, so it acts on the notes as they
            // look rather than on a preview that would later be reverted.
            .capture_any_mouse_down(cx.listener(|this, _ev, _window, cx| {
                this.accept_active_tool(cx);
            }))
            .on_mouse_move(cx.listener(Self::on_move))
            // Moves stop reaching the editor once the pointer leaves it; the
            // lit drum lane goes out with it.
            .id("piano-roll-root")
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if !*hovered && this.drum_hover_lane.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::on_up))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.begin_pan(event, window, cx);
                }),
            )
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_up))
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .child(toolbar)
            .child(body)
            .child(footer)
    }
}

/// One row of an editor dropdown. A check marks the current choice — glyph
/// and text weight, not colour alone. The caller attaches the click.
fn menu_row(
    id: impl Into<gpui::ElementId>,
    selected: bool,
    label: impl IntoElement,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, space, typography};
    let hover = Colors::composite(Colors::surface_raised(), Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::ROW_DENSE))
        .px(px(space::SNUG))
        .rounded(px(radius::inner(radius::SURFACE, space::TIGHT)))
        .text_size(px(typography::UI_XS))
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .size(px(12.0))
                .when(selected, |slot| {
                    slot.child(
                        svg()
                            .path(assets::ICON_CHECK_PATH)
                            .size(px(11.0))
                            .text_color(Colors::accent_primary()),
                    )
                }),
        )
        .child(div().flex_1().min_w_0().truncate().child(label))
}

/// A quiet heading inside a dropdown.
fn menu_heading(text: &'static str) -> gpui::AnyElement {
    use crate::theme::{space, typography};
    div()
        .px(px(space::SNUG))
        .pt(px(space::TIGHT))
        .pb(px(space::HAIR))
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(text)
        .into_any_element()
}

fn menu_separator() -> gpui::AnyElement {
    div()
        .h(px(1.0))
        .my(px(crate::theme::space::TIGHT))
        .mx(px(crate::theme::space::SNUG))
        .bg(Colors::divider())
        .into_any_element()
}

/// A small dot saying "this lane has data in the clip".
fn data_dot() -> gpui::AnyElement {
    div()
        .flex_shrink_0()
        .size(px(5.0))
        .rounded(px(crate::theme::radius::PILL))
        .bg(Colors::accent_primary())
        .into_any_element()
}

impl PianoRoll {
    /// A dropdown: a filled trigger that shows the current value, over a
    /// popover of `rows`. The controller lane's opens upward, because the lane
    /// sits at the bottom of the editor.
    #[allow(clippy::too_many_arguments)]
    fn render_select_menu(
        &self,
        menu: PianoSelectMenu,
        id: &'static str,
        caption: Option<&'static str>,
        label: String,
        tip: &'static str,
        panel_w: f32,
        fill: bool,
        rows: Vec<gpui::AnyElement>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        use crate::theme::{elevation, motion, radius, size, space, typography};
        let open = self.open_select_menu == Some(menu);
        let upward = menu == PianoSelectMenu::Lane;
        let dropdown = open.then(|| {
            let panel = div()
                .absolute()
                .when(upward, |p| p.bottom(px(size::DEFAULT + space::HAIR)))
                .when(!upward, |p| p.top(px(size::DEFAULT + space::HAIR)))
                .left_0()
                .w(px(panel_w))
                .max_h(px(360.0))
                .id(("pr-select-menu-scroll", menu as u32))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .p(px(space::TIGHT))
                .rounded(px(radius::SURFACE))
                .bg(Colors::surface_raised())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .shadow(elevation::shadow(elevation::OVERLAY))
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
                .children(rows);
            deferred(
                panel
                    .with_animation(
                        ("pr-select-menu-open", menu as u32),
                        Animation::new(Duration::from_millis(motion::MICRO_MS))
                            .with_easing(gpui::ease_out_quint()),
                        |this, t| this.opacity(t),
                    )
                    .into_any_element(),
            )
            .with_priority(PIANO_ROLL_MENU_PRIORITY)
            .into_any_element()
        });

        let base = Colors::surface_input();
        let rest = if open {
            Colors::composite(base, Colors::state_selected())
        } else {
            base
        };
        let hover = Colors::composite(rest, Colors::state_hover());
        let trigger = div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(tip)
            .flex()
            .flex_row()
            .items_center()
            // A column-filling trigger lives in a narrow column; its padding
            // tightens so the value keeps the room.
            .gap(px(if fill { space::TIGHT } else { space::SNUG }))
            .h(px(size::DEFAULT))
            .when(fill, |t| t.w_full())
            .pl(px(if fill { space::SNUG } else { space::BASE }))
            .pr(px(if fill { space::TIGHT } else { space::SNUG }))
            .rounded(px(radius::CONTROL))
            .bg(rest)
            .border(px(1.0))
            .border_color(if open {
                Colors::border_strong()
            } else {
                Colors::border_subtle()
            })
            .text_size(px(typography::UI_XS))
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(hover))
            .tooltip(crate::components::controls::fb_tooltip(tip))
            .on_click(cx.listener(move |this, _ev, _w, cx| {
                cx.stop_propagation();
                this.open_select_menu = if this.open_select_menu == Some(menu) {
                    None
                } else {
                    Some(menu)
                };
                cx.notify();
            }))
            .children(caption.map(|caption| {
                div()
                    .flex_shrink_0()
                    .text_color(Colors::text_muted())
                    .child(caption)
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .child(label),
            )
            .child(
                svg()
                    .path(if upward {
                        assets::ICON_CHEVRON_UP_PATH
                    } else {
                        assets::ICON_CHEVRON_DOWN_PATH
                    })
                    .size(px(10.0))
                    .flex_shrink_0()
                    .text_color(Colors::text_muted()),
            );

        div()
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .when(fill, |root| root.w_full())
            .occlude()
            .child(trigger)
            .children(dropdown)
    }

    /// The controller lane's selector: which lane the one lane at the bottom
    /// shows (Velocity / common CCs / pitch-bend / pressure / articulations /
    /// custom CC). Lives in the lane's own header. Alt+wheel over it cycles
    /// lanes. Switching only changes what the lane shows, never the data.
    pub(super) fn render_lane_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::theme::{radius, size, space, typography};
        let current = self.current_lane();
        let custom = self.custom_cc;

        // Controller kinds that actually carry points in the clip being
        // edited. Merged into the dropdown below so CC lanes that come from
        // an imported/recorded MIDI clip (any of the 128 CC numbers, not
        // just the six common ones in `LANE_CYCLE`) are still reachable
        // without the user having to already know the CC number to type into
        // the "Custom CC" stepper.
        let clip_lane_kinds: Vec<(MidiControllerKind, bool)> = self
            .editing_clip_id(cx)
            .map(|clip_id| {
                let tl = self.timeline.read(cx);
                tl.state
                    .midi_clip_controller_lanes(&clip_id)
                    .map(|lanes| {
                        lanes
                            .iter()
                            .map(|lane| (lane.kind, !lane.points.is_empty()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let has_data =
            |kind: MidiControllerKind| clip_lane_kinds.iter().any(|(k, has)| *k == kind && *has);
        // CC lanes with real data that aren't already offered by the common
        // `LANE_CYCLE` list get their own "In This Clip" section.
        let mut extra_kinds: Vec<MidiControllerKind> = clip_lane_kinds
            .iter()
            .filter(|(kind, has)| {
                *has && !LANE_CYCLE.contains(&ControllerLaneKind::Controller(*kind))
            })
            .map(|(kind, _)| *kind)
            .collect();
        extra_kinds.sort_by_key(|kind| match kind {
            MidiControllerKind::CC(n) => *n as u16,
            MidiControllerKind::PitchBend => 200,
            MidiControllerKind::ChannelPressure => 201,
            MidiControllerKind::PolyPressure => 202,
        });

        let articulation_lane_has_data = self
            .editing_clip_id(cx)
            .and_then(|clip_id| {
                let tl = self.timeline.read(cx);
                tl.state
                    .midi_clip_articulations(&clip_id)
                    .map(|events| !events.is_empty())
            })
            .unwrap_or(false);

        let lane_label = |text: String, data: bool| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::SNUG))
                .child(div().truncate().child(text))
                .when(data, |row| row.child(data_dot()))
        };

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (i, kind) in LANE_CYCLE.iter().enumerate() {
            let kind = *kind;
            let text = match kind {
                ControllerLaneKind::Velocity => "Velocity".to_string(),
                ControllerLaneKind::Controller(k) => cc_kind_label(k),
                ControllerLaneKind::Articulations => "Articulations".to_string(),
            };
            let lane_has_data = match kind {
                ControllerLaneKind::Velocity => false,
                ControllerLaneKind::Controller(k) => has_data(k),
                ControllerLaneKind::Articulations => articulation_lane_has_data,
            };
            rows.push(
                menu_row(
                    ("pr-lane-opt", i),
                    kind == current,
                    lane_label(text, lane_has_data),
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    cx.stop_propagation();
                    this.set_lane(kind, cx);
                }))
                .into_any_element(),
            );
        }
        if !extra_kinds.is_empty() {
            rows.push(menu_separator());
            rows.push(menu_heading("In This Clip"));
            for (i, kind) in extra_kinds.iter().enumerate() {
                let lane_kind = ControllerLaneKind::Controller(*kind);
                rows.push(
                    menu_row(
                        ("pr-lane-extra", i),
                        lane_kind == current,
                        lane_label(cc_kind_label(*kind), true),
                    )
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.set_lane(lane_kind, cx);
                    }))
                    .into_any_element(),
                );
            }
        }

        // Custom CC: − / CCnn / +. The steppers keep the menu open.
        let custom_selected =
            current == ControllerLaneKind::Controller(MidiControllerKind::CC(custom));
        let step_hover = Colors::composite(Colors::surface_raised(), Colors::state_hover());
        let stepper = |id: usize, glyph: &'static str, label: &'static str| {
            div()
                .id(("pr-lane-custom", id))
                .role(gpui::Role::Button)
                .aria_label(label)
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .size(px(size::DENSE))
                .rounded(px(radius::CONTROL_SM))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(step_hover))
                .child(
                    svg()
                        .path(glyph)
                        .size(px(11.0))
                        .text_color(Colors::text_secondary()),
                )
        };
        rows.push(menu_separator());
        rows.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::HAIR))
                .px(px(space::HAIR))
                .child(
                    stepper(0, assets::ICON_MINUS_PATH, "Previous CC").on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            cx.stop_propagation();
                            this.custom_cc = this.custom_cc.saturating_sub(1);
                            cx.notify();
                        },
                    )),
                )
                .child(
                    menu_row(
                        ("pr-lane-custom", 1usize),
                        custom_selected,
                        format!("Custom CC {custom}"),
                    )
                    .flex_1()
                    .text_size(px(typography::UI_XS))
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.set_lane(
                            ControllerLaneKind::Controller(MidiControllerKind::CC(this.custom_cc)),
                            cx,
                        )
                    })),
                )
                .child(
                    stepper(2, assets::ICON_PLUS_PATH, "Next CC").on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            cx.stop_propagation();
                            this.custom_cc = (this.custom_cc + 1).min(127);
                            cx.notify();
                        },
                    )),
                )
                .into_any_element(),
        );

        self.render_select_menu(
            PianoSelectMenu::Lane,
            "pr-lane-select",
            None,
            self.lane_name(),
            "Controller lane — Alt+wheel to cycle",
            196.0,
            true,
            rows,
            cx,
        )
        // Alt + mouse wheel cycles lanes.
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
            if !ev.modifiers.alt {
                return;
            }
            let dy = match ev.delta {
                gpui::ScrollDelta::Pixels(p) => f32::from(p.y),
                gpui::ScrollDelta::Lines(p) => p.y,
            };
            if dy != 0.0 {
                cx.stop_propagation();
                this.cycle_lane(if dy < 0.0 { 1 } else { -1 }, cx);
            }
        }))
    }

    /// The editor's toolbar, in modules: tools · snap and grid · pitch (scale,
    /// lock, chord) · channel · edit, then view and panels on the right. Every
    /// icon-only control carries a tooltip; counts and live readouts live in
    /// the footer, not here.
    pub(super) fn render_toolbar(
        &self,
        cx: &mut Context<Self>,
        _clip_id: Option<&str>,
    ) -> impl IntoElement {
        use crate::theme::space;
        let tool = self.tool;
        let snap_on = self.snap_on;

        let tool_button = |id: &'static str,
                           icon: &'static str,
                           tip: gpui::SharedString,
                           target: PianoTool,
                           cx: &mut Context<Self>| {
            tool_segment(
                id,
                icon,
                tip,
                tool == target,
                cx.listener(move |this, _, _w, cx| {
                    this.cancel_active_gesture(cx);
                    this.tool = target;
                    cx.notify();
                }),
            )
        };
        let tools = tool_strip()
            .child(tool_button(
                "pr-select",
                assets::ICON_MOUSE_POINTER_PATH,
                command_tip("Select", "midi:tool-select"),
                PianoTool::Select,
                cx,
            ))
            .child(tool_button(
                "pr-draw",
                assets::ICON_PENCIL_PATH,
                command_tip("Draw", "midi:tool-draw"),
                PianoTool::Draw,
                cx,
            ))
            .child(tool_button(
                "pr-line",
                assets::ICON_PEN_LINE_PATH,
                command_tip("Line", "midi:tool-line"),
                PianoTool::Line,
                cx,
            ))
            .child(tool_button(
                "pr-erase",
                assets::ICON_ERASER_PATH,
                "Erase".into(),
                PianoTool::Erase,
                cx,
            ))
            .child(tool_button(
                "pr-split",
                assets::ICON_SCISSORS_PATH,
                "Split".into(),
                PianoTool::Split,
                cx,
            ))
            .child(tool_button(
                "pr-mute-tool",
                assets::ICON_VOLUME_X_PATH,
                "Mute".into(),
                PianoTool::Mute,
                cx,
            ));

        // ── Editor mode: the keyboard or the drum lanes ──
        let drum_mode = self.drum_mode;
        let mut editor_mode = tool_strip()
            .child(tool_segment(
                "pr-mode-piano",
                assets::ICON_KEYBOARD_PATH,
                "Piano editor".into(),
                !drum_mode,
                cx.listener(|this, _, _w, cx| {
                    this.cancel_active_gesture(cx);
                    this.set_drum_mode(false, cx);
                }),
            ))
            .child(tool_segment(
                "pr-mode-drum",
                assets::ICON_DRUM_PATH,
                "Drum editor: lanes by sound, one hit per step".into(),
                drum_mode && !self.step_mode,
                cx.listener(|this, _, _w, cx| {
                    this.cancel_active_gesture(cx);
                    // Step → MIDI: the step view back to the drum lanes.
                    this.set_drum_mode(true, cx);
                }),
            ))
            .child(tool_segment(
                "pr-mode-step",
                assets::ICON_SQUARE_PATH,
                "MIDI → Step: edit this clip as a step sequencer (the notes stay the clip's MIDI)"
                    .into(),
                self.step_mode,
                cx.listener(|this, _, _w, cx| {
                    this.cancel_active_gesture(cx);
                    this.set_step_mode(true, cx);
                }),
            ));
        if self.step_mode {
            // What the rows are: one clip's drum lanes, or a rack of tracks.
            let source = self.step_source();
            for (slot, (label, tip, value)) in [
                (
                    "Drum Sampler",
                    "Drum Sampler: one row per drum sound of this clip",
                    StepSource::DrumSampler,
                ),
                (
                    "Sampler Rack",
                    "Sampler Rack: one row per instrument track in this track's group, \
                     each with its own sampler, mute, solo, volume and pan",
                    StepSource::SamplerRack,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                editor_mode = editor_mode.child(tool_text_segment(
                    ("pr-step-source", slot),
                    label.to_string(),
                    tip.into(),
                    source == value,
                    cx.listener(move |this, _, _w, cx| {
                        this.cancel_active_gesture(cx);
                        this.set_step_source(value, cx);
                    }),
                ));
            }
            // Note repeat: how many hits a new step plays, what Shift-click
            // gives a step, and what the selected steps are rewritten to.
            let current = self.step_repeat;
            for repeat in step_grid::STEP_REPEATS {
                editor_mode = editor_mode.child(tool_text_segment(
                    ("pr-step-repeat", usize::from(repeat)),
                    format!("×{repeat}"),
                    if repeat == 1 {
                        "One hit per step. Applies to the selected steps too".to_string()
                    } else {
                        format!(
                            "Note repeat: {repeat} hits per step. New steps and the \
                             selected steps play it; Shift-drag a step up or down to \
                             change its count"
                        )
                    }
                    .into(),
                    repeat == current,
                    cx.listener(move |this, _, _w, cx| {
                        this.cancel_active_gesture(cx);
                        this.choose_step_repeat(repeat, cx);
                    }),
                ));
            }
            // Repeat curve: the velocity shape of a step's repeated hits.
            let current_curve = self.step_curve();
            for (slot, curve) in step_grid::RepeatCurve::ALL.into_iter().enumerate() {
                editor_mode = editor_mode.child(tool_text_segment(
                    ("pr-step-curve", slot),
                    curve.label().to_string(),
                    format!(
                        "{}. New steps and the selected steps play it",
                        curve.description()
                    )
                    .into(),
                    curve == current_curve,
                    cx.listener(move |this, _, _w, cx| {
                        this.cancel_active_gesture(cx);
                        this.choose_step_curve(curve, cx);
                    }),
                ));
            }
            let off_grid = self.step_off_grid_count(cx);
            if off_grid > 0 {
                editor_mode = editor_mode.child(tool_text_segment(
                    ("pr-step-snap", 0usize),
                    format!("Snap {off_grid} to grid"),
                    format!(
                        "{off_grid} note(s) sit off their step lines: nudged, played \
                         early or late, or in an uneven roll (amber). Put them back \
                         on the lines (one undo)"
                    )
                    .into(),
                    false,
                    cx.listener(|this, _, _w, cx| {
                        this.cancel_active_gesture(cx);
                        this.snap_steps_to_grid(cx);
                    }),
                ));
            }
        }
        if drum_mode {
            // Which lanes: the whole kit, or only what the clip plays.
            let lanes = self.drum_lanes;
            editor_mode = editor_mode
                .child(tool_segment(
                    "pr-drum-kit",
                    assets::ICON_LIST_MUSIC_PATH,
                    "Show the whole General MIDI kit".into(),
                    lanes == DrumLanes::Kit,
                    cx.listener(|this, _, _w, cx| this.set_drum_lanes(DrumLanes::Kit, cx)),
                ))
                .child(tool_segment(
                    "pr-drum-used",
                    assets::ICON_MUSIC_PATH,
                    "Show only the sounds this clip plays".into(),
                    lanes == DrumLanes::Used,
                    cx.listener(|this, _, _w, cx| this.set_drum_lanes(DrumLanes::Used, cx)),
                ))
                // Which way the lanes run. Lit while reversed (high notes at
                // the top), so the default order reads as the rest state.
                .child(tool_segment(
                    "pr-drum-order",
                    assets::ICON_ARROW_DOWN_UP_PATH,
                    match self.drum_order {
                        DrumOrder::LowFirst => "Reverse lanes: high notes at the top",
                        DrumOrder::HighFirst => "Reverse lanes: kick at the top",
                    }
                    .into(),
                    self.drum_order == DrumOrder::HighFirst,
                    cx.listener(|this, _, _w, cx| {
                        this.cancel_active_gesture(cx);
                        this.reverse_drum_order(cx);
                    }),
                ));
        }

        // ── Grid ──
        let grid_rows = GridRes::ALL
            .iter()
            .enumerate()
            .map(|(idx, res)| {
                let res = *res;
                menu_row(("pr-grid-choice", idx), res == self.grid_res, res.label())
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.grid_res = res;
                        // Free mode turns snapping off; other modes re-enable it.
                        this.snap_on = !res.is_free();
                        this.open_select_menu = None;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let grid_menu = self.render_select_menu(
            PianoSelectMenu::Grid,
            "pr-grid-select",
            Some("Grid"),
            self.grid_res.label().to_string(),
            "Grid resolution",
            140.0,
            false,
            grid_rows,
            cx,
        );

        // ── Scale: roots as a grid, then the kinds ──
        let root_hover = Colors::composite(Colors::surface_raised(), Colors::state_hover());
        let root_selected = Colors::composite(Colors::surface_raised(), Colors::accent_active());
        let roots = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(space::HAIR))
            .px(px(space::TIGHT))
            .pb(px(space::TIGHT))
            .children(ScaleRoot::ALL.iter().enumerate().map(|(idx, root)| {
                let root = *root;
                let selected = root == self.pitch_ctx.scale.root;
                div()
                    .id(("pr-root-choice", idx))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(40.0))
                    .h(px(crate::theme::size::DENSE))
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .border(px(1.0))
                    .border_color(if selected {
                        Colors::accent_primary()
                    } else {
                        Colors::with_alpha(Colors::border_subtle(), 0.0)
                    })
                    .bg(if selected {
                        root_selected
                    } else {
                        Colors::with_alpha(root_selected, 0.0)
                    })
                    .text_size(px(crate::theme::typography::UI_XS))
                    .text_color(if selected {
                        Colors::text_primary()
                    } else {
                        Colors::text_secondary()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(root_hover))
                    // Picking a root keeps the menu open for the scale.
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.pitch_ctx.scale.root = root;
                        cx.notify();
                    }))
                    .child(root.label())
            }));
        let mut scale_rows: Vec<gpui::AnyElement> =
            vec![menu_heading("Root"), roots.into_any_element()];
        scale_rows.push(menu_separator());
        scale_rows.push(menu_heading("Scale"));
        for (idx, kind) in ScaleKind::ALL.iter().enumerate() {
            let kind = *kind;
            scale_rows.push(
                menu_row(
                    ("pr-scale-choice", idx),
                    kind == self.pitch_ctx.scale.kind,
                    kind.label(),
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    cx.stop_propagation();
                    this.pitch_ctx.scale.kind = kind;
                    this.pitch_ctx.constrain = kind != ScaleKind::Chromatic;
                    this.open_select_menu = None;
                    cx.notify();
                }))
                .into_any_element(),
            );
        }
        scale_rows.push(menu_separator());
        scale_rows.push(
            menu_row("pr-scale-snap-selection", false, "Snap Selection to Scale")
                .on_click(cx.listener(|this, _ev, _w, cx| {
                    cx.stop_propagation();
                    this.open_select_menu = None;
                    this.snap_selection_to_scale(cx);
                }))
                .into_any_element(),
        );
        let scale = self.pitch_ctx.scale;
        let scale_label = if scale.kind == ScaleKind::Chromatic {
            scale.kind.label().to_string()
        } else {
            format!("{} {}", scale.root.label(), scale.kind.label())
        };
        let scale_menu = self.render_select_menu(
            PianoSelectMenu::Scale,
            "pr-scale",
            None,
            scale_label,
            "Scale guide",
            196.0,
            false,
            scale_rows,
            cx,
        );
        let constrain = self.pitch_ctx.constrain;
        let lock = bar_icon_button(
            "pr-scale-constrain",
            if constrain {
                assets::ICON_LOCK_PATH
            } else {
                assets::ICON_LOCK_OPEN_PATH
            },
            "Keep notes in scale".into(),
            Some(constrain),
            cx.listener(|this, _, _w, cx| {
                this.pitch_ctx.constrain = !this.pitch_ctx.constrain;
                this.open_select_menu = None;
                cx.notify();
            }),
        );

        // ── Chord ──
        let chord_rows = ChordInsertKind::ALL
            .iter()
            .enumerate()
            .map(|(idx, kind)| {
                let kind = *kind;
                let text = match kind {
                    ChordInsertKind::Off => "Single Note",
                    ChordInsertKind::Triad => "Triad",
                    ChordInsertKind::Seventh => "Seventh",
                };
                menu_row(("pr-chord-choice", idx), kind == self.chord_kind, text)
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.chord_kind = kind;
                        this.open_select_menu = None;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let chord_menu = self.render_select_menu(
            PianoSelectMenu::Chord,
            "pr-chord-insert",
            None,
            self.chord_kind.label().to_string(),
            "Draw chords",
            148.0,
            false,
            chord_rows,
            cx,
        );

        // ── Channel ──
        let mut channel_rows: Vec<gpui::AnyElement> = Vec::with_capacity(21);
        channel_rows.push(
            menu_row(
                "pr-channel-choice-all",
                self.channel_view.is_all(),
                "All Channels",
            )
            .on_click(cx.listener(|this, _ev, _w, cx| {
                cx.stop_propagation();
                this.set_channel_view(MidiChannelMask::ALL, cx);
            }))
            .into_any_element(),
        );
        for (idx, ch) in MidiChannel::all().enumerate() {
            channel_rows.push(
                menu_row(
                    ("pr-channel-choice", idx),
                    self.channel_view == MidiChannelMask::single(ch),
                    format!("Channel {}", ch.ui()),
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    cx.stop_propagation();
                    this.set_channel_view(MidiChannelMask::single(ch), cx);
                }))
                .into_any_element(),
            );
        }
        channel_rows.push(menu_separator());
        let assign_channel = self.active_note_channel(cx);
        channel_rows.push(
            menu_row(
                "pr-channel-apply",
                false,
                format!("Move Selection to Ch {}", assign_channel.ui()),
            )
            .on_click(cx.listener(|this, _ev, _w, cx| {
                cx.stop_propagation();
                this.open_select_menu = None;
                let channel = this.active_note_channel(cx);
                this.set_selected_notes_channel(channel, cx);
            }))
            .into_any_element(),
        );
        channel_rows.push(
            menu_row(
                "pr-channel-output-mode",
                self.track_output_per_note(cx),
                "Per-Note Output",
            )
            .on_click(cx.listener(|this, _ev, _w, cx| {
                cx.stop_propagation();
                this.toggle_track_output_per_note(cx);
            }))
            .into_any_element(),
        );
        let channel_menu = self.render_select_menu(
            PianoSelectMenu::Channel,
            "pr-channel-view",
            None,
            self.channel_view_label(),
            "Channels shown and edited",
            196.0,
            false,
            channel_rows,
            cx,
        );

        // ── Edit ──
        let quantize = bar_text_button(
            "pr-quantize",
            "Quantize",
            Some(command_tip("Quantize — hover to preview", "midi:quantize")),
            cx.listener(|this, _, _w, cx| this.quantize_selection(cx)),
        )
        .on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
            this.quantize_preview = *hovered;
            cx.notify();
        }));
        // Each tool opens a small window of its own that previews on the
        // notes live; this menu is the editor's way in.
        let active_tool = self.active_tool();
        let tool_rows = crate::components::piano_roll::advanced_tools::AdvancedTool::ALL
            .iter()
            .copied()
            .enumerate()
            .map(|(idx, tool)| {
                menu_row(
                    ("pr-tool-choice", idx),
                    active_tool == Some(tool),
                    format!("{}…", tool.label()),
                )
                .on_click(cx.listener(move |this, _ev, window, cx| {
                    cx.stop_propagation();
                    this.open_select_menu = None;
                    this.open_tool_window(tool, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let advanced = self.render_select_menu(
            PianoSelectMenu::Tools,
            "pr-tools-menu",
            None,
            "Tools".to_string(),
            "Strum, Humanize, Length, Velocity and Clean Up, each in its own window",
            168.0,
            false,
            tool_rows,
            cx,
        );

        // ── View ──
        let view = div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .gap(px(space::HAIR))
            .child(bar_icon_button(
                "pr-zoom-out",
                assets::ICON_ZOOM_OUT_PATH,
                "Zoom out".into(),
                None,
                cx.listener(|this, _, _w, cx| this.zoom_by(0.5, cx)),
            ))
            .child(bar_icon_button(
                "pr-zoom-in",
                assets::ICON_ZOOM_IN_PATH,
                "Zoom in".into(),
                None,
                cx.listener(|this, _, _w, cx| this.zoom_by(2.0, cx)),
            ))
            .child(bar_icon_button(
                "pr-fit",
                assets::ICON_SCAN_PATH,
                command_tip("Fit notes", "midi:fit-notes"),
                None,
                cx.listener(|this, _, _w, cx| {
                    if let Some(cid) = this.editing_clip_id(cx) {
                        this.fit_piano_roll_to_notes(cx, &cid);
                        cx.notify();
                    }
                }),
            ))
            .child(bar_text_button(
                "pr-c4",
                "C4",
                Some("Center on middle C".into()),
                cx.listener(|this, _, _w, cx| {
                    this.scroll_to_pitch(60);
                    cx.notify();
                }),
            ));

        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            // Modules are set apart by their dividers; within one, controls
            // sit a hair apart. The row has to fit the editor window's minimum
            // width without clipping the panel toggles at its end.
            .gap(px(space::HAIR))
            .h(px(TOOLBAR_H))
            .px(px(space::BASE))
            .overflow_hidden()
            .border_b(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            .child(tools)
            .child(bar_divider())
            .child(editor_mode)
            .child(bar_divider())
            .child(bar_icon_button(
                "pr-snap",
                assets::ICON_MAGNET_PATH,
                command_tip("Snap to grid", "midi:toggle-snap"),
                Some(snap_on),
                cx.listener(|this, _, _w, cx| {
                    this.snap_on = !this.snap_on;
                    cx.notify();
                }),
            ))
            .child(grid_menu)
            .child(bar_divider())
            // Scale lock and chord insert act on pitch; drum lanes are sounds,
            // and drum mode ignores both, so they are not offered there.
            .when(!drum_mode, |row| {
                row.child(scale_menu)
                    .child(lock)
                    .child(chord_menu)
                    .child(bar_divider())
            })
            .child(channel_menu)
            .child(bar_divider())
            .child(quantize)
            .child(advanced)
            .child(div().flex_1().min_w(px(space::BASE)))
            .child(view)
            .child(bar_divider())
            .child(bar_icon_button(
                "pr-tools",
                assets::ICON_PANEL_LEFT_PATH,
                "MIDI tools".into(),
                Some(self.tools.open),
                cx.listener(|this, _, _w, cx| {
                    this.tools.open = !this.tools.open;
                    cx.notify();
                }),
            ))
            .child(bar_icon_button(
                "pr-inspector",
                assets::ICON_PANEL_RIGHT_PATH,
                "Note inspector".into(),
                Some(self.inspector_open),
                cx.listener(|this, _, _w, cx| {
                    this.inspector_open = !this.inspector_open;
                    cx.notify();
                }),
            ))
            .when_some(self.on_pop_out.clone(), |row, pop_out| {
                row.child(bar_icon_button(
                    "pr-pop-out",
                    assets::ICON_POP_OUT_PATH,
                    "Open in a window".into(),
                    None,
                    move |_, window, cx| pop_out(window, cx),
                ))
            })
    }

    /// The status footer: what the pointer or the gesture in flight is doing
    /// on the left, the clip's counts and the window's actions on the right.
    pub(super) fn render_footer(
        &self,
        cx: &mut Context<Self>,
        clip_id: Option<&str>,
    ) -> impl IntoElement {
        use crate::theme::{space, typography};
        let note_count = clip_id
            .map(|cid| note_count_for_clip(cx, &self.timeline, cid))
            .unwrap_or(0);
        let selected = self.selection.len();
        let small_button = |button: gpui::Stateful<gpui::Div>| {
            button
                .h(px(crate::theme::size::MICRO + space::HAIR))
                .px(px(space::SNUG))
                .rounded(px(crate::theme::radius::CONTROL_SM))
        };
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .gap(px(space::LOOSE))
            .h(px(FOOTER_H))
            .px(px(space::BASE))
            .border_t(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            .text_size(px(typography::DENSE_LABEL))
            .text_color(Colors::text_muted())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.pointer_status()),
            )
            .when(clip_id.is_some(), |row| {
                row.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(space::SNUG))
                        .child(format!("{} notes", group_thousands(note_count)))
                        .when(selected > 0, |counts| {
                            counts.child(
                                div()
                                    .text_color(Colors::accent_primary())
                                    .child(format!("{} selected", group_thousands(selected))),
                            )
                        }),
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .child(format!("Grid {}", self.grid_res.label())),
            )
            .when_some(self.on_export_midi.clone(), |row, export| {
                row.child(small_button(bar_text_button(
                    "pr-export-midi",
                    "Export MIDI…",
                    None,
                    move |_, window, cx| export(window, cx),
                )))
            })
            .when_some(self.on_dock.clone(), |row, dock| {
                row.child(small_button(bar_text_button(
                    "pr-dock",
                    "Open in Bottom Panel",
                    None,
                    move |_, window, cx| dock(window, cx),
                )))
            })
    }

    pub(super) fn render_body(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
    ) -> impl IntoElement {
        use crate::theme::{space, typography};
        let (view_w, view_h) = self.grid_view_size();
        let track_color = self.track_color_for_clip(cx, clip_id);

        // Resolved in `render` above, before the fit that depends on it, and
        // again here because the edited clip can be moved on the arrangement
        // while the editor is open.
        self.refresh_scope(cx, clip_id);

        let (meter, clip_len, show_playhead, playing, playhead_project, loop_region) = {
            let tl = self.timeline.read(cx);
            // The whole meter map, not the beats-per-bar at the playhead: the
            // grid spans the track, and one bar length applied from beat 0 put
            // every line after a meter change in the wrong place.
            let meter = EditorMeter::from_map(&tl.state.time_signature_map);
            let clip_len = self
                .scope
                .editing()
                .map(|span| span.duration_beats)
                .unwrap_or(0.0);
            let t = &tl.state.transport;
            // The playhead is a project beat and so is the axis, so it needs no
            // shifting — and it is drawn wherever it is rather than only inside
            // the edited clip. Watching the transport cross the clip before this
            // one is the point of showing them.
            let show_playhead = true;
            // The loop region is stored project-global, which is now also what
            // the axis speaks.
            let loop_region = if t.loop_enabled && t.loop_end_beats > t.loop_start_beats {
                Some((t.loop_start_beats, t.loop_end_beats))
            } else {
                None
            };
            (
                meter,
                clip_len,
                show_playhead,
                t.playing,
                t.playhead_beats,
                loop_region,
            )
        };

        // Visible ranges (only build geometry for what's on screen).
        let first_pitch = (self.y_to_pitch(view_h) as i32 - 1).max(0);
        let last_pitch = (self.y_to_pitch(0.0) as i32 + 1).min(PITCH_CNT - 1);
        self.meter = meter;
        // The same span in project beats, for the things that count from the
        // start of the song rather than from the clip: the ruler's bar numbers,
        // the grid behind every clip, and the clip boundaries themselves.
        let project_start = self.x_to_project_beat(0.0);
        let project_end = self.x_to_project_beat(view_w);

        let keys = if self.rack_active() {
            self.build_rack_lane(cx)
        } else {
            self.build_key_lane(first_pitch, last_pitch)
        };

        let grid_lines = self.build_grid_lines(
            project_start,
            project_end,
            view_w,
            first_pitch,
            last_pitch,
            clip_len,
        );
        // Under the hits, over the lane bands: the hovered lane across the bar.
        let drum_hover_grid = self.drum_hover_band("pr-drum-hover-grid");
        let clip_bounds = self.build_clip_bounds_overlay(view_w, view_h);
        let loop_overlay = self.build_loop_overlay(loop_region, view_w, view_h);
        // The playhead is its own entity, not a child of this render. Building
        // it here would tie a one-pixel translation to a full rebuild of the
        // editor — see `piano_roll::playhead`. This only keeps the shared frame
        // in step, because a scroll or a zoom moves the line without the
        // transport having moved at all.
        if self.playhead_overlay.is_none() {
            let frame = self.playhead_frame.clone();
            self.playhead_overlay = Some(cx.new(|_| {
                crate::components::piano_roll::playhead::PianoRollPlayheadOverlay::new(frame)
            }));
        }
        self.playhead_frame.set(
            crate::components::piano_roll::playhead::PianoRollPlayheadFrame {
                x: self.project_beat_to_x(playhead_project),
                visible: show_playhead,
                playing,
            },
        );
        let playhead_overlay = self.playhead_overlay.clone();
        let mut ruler = self.build_ruler_clip_band(track_color, view_w);
        ruler.extend(self.build_ruler(project_start, project_end));
        ruler.extend(self.build_loop_ruler_markers(loop_region));
        ruler.extend(self.build_stretch_band(cx, clip_id));
        // Under the editable notes and over the grid: context, not content.
        let context_notes = self.build_context_notes(cx, view_w, view_h);
        let notes_geo = if self.step_mode {
            self.build_step_cells(cx, clip_id, track_color, clip_len)
        } else {
            self.build_note_elements(cx, clip_id, track_color)
        };
        let stretch_handles = self.build_stretch_handles(cx, clip_id);
        let quantize_preview = self.build_quantize_preview(cx, clip_id);
        let marquee_overlay = self.build_marquee_overlay();
        let draw_preview = self.build_draw_note_preview(track_color);
        let erase_overlay = self.build_erase_overlay();
        let note_menu = self.build_note_context_menu(cx);
        let lane_menu = self.build_lane_menu(cx);
        let scrollbars = self.render_scrollbars(cx);
        // Empty clip: the musical canvas (ruler, keys, grid) stays, with one
        // quiet hint naming the gesture that actually creates a note. Nothing
        // here is a status readout — the counts live in the footer.
        // In the rack the other rows have their own clips; the hint is only
        // for an editor with nothing in it at all.
        let grid_empty_hint = (note_count_for_clip(cx, &self.timeline, clip_id) == 0
            && !self.rack_active())
        .then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(if self.step_mode {
                    "Click or drag to add hits; Shift-drag sets note repeat; right-drag erases; Alt-drag ramps velocity; Ctrl-drag nudges"
                } else {
                    "Draw notes with the Draw tool, or record onto this clip"
                })
                .into_any_element()
        });
        let note_inspector = self
            .inspector_open
            .then(|| self.render_note_inspector(cx, clip_id).into_any_element());
        let tools_panel = self
            .tools
            .open
            .then(|| self.render_tools_panel(cx, clip_id).into_any_element());
        // Global Tracks: the project's Chord Track under the ruler, with a
        // matching corner over the keys so the key rows stay on the grid rows.
        let chord_strip = self.tools.show_chord_strip.then(|| {
            div()
                .h(px(tools_panel::CHORD_STRIP_H))
                .w_full()
                .flex_shrink_0()
                .relative()
                .overflow_hidden()
                .bg(Colors::surface_panel())
                .border_b(px(1.0))
                .border_color(Colors::panel_border())
                .children(self.build_chord_strip(cx, view_w))
                .into_any_element()
        });
        let chord_strip_corner = self.tools.show_chord_strip.then(|| {
            div()
                .h(px(tools_panel::CHORD_STRIP_H))
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .px(px(space::TIGHT))
                .bg(Colors::surface_panel())
                .border_b(px(1.0))
                .border_r(px(1.0))
                .border_color(Colors::panel_border())
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child("Chords")
                .into_any_element()
        });

        // ── Single unified controller lane ───────────────────────────────────
        // Exactly one lane is built per frame: velocity OR the active controller.
        // Switching the selector only changes which is built — the hidden lane's
        // data (note velocities / other controller points) is left untouched.
        let unified_lane_visible = self.unified_lane_visible(cx);
        let lane_collapsed = !unified_lane_visible && !self.editing_solfege_track(cx);
        let lane_header: Option<gpui::AnyElement> = if unified_lane_visible {
            Some(self.render_lane_header(cx))
        } else if lane_collapsed {
            Some(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(LANE_COLLAPSED_H))
                    .w_full()
                    .px(px(space::HAIR))
                    .border_t(px(1.0))
                    .border_r(px(1.0))
                    .border_color(Colors::panel_border())
                    .bg(Colors::surface_panel())
                    .child(
                        bar_icon_button(
                            "pr-lane-expand",
                            assets::ICON_CHEVRON_UP_PATH,
                            format!("Show {} lane", self.lane_name()).into(),
                            None,
                            cx.listener(|this, _ev, _w, cx| this.toggle_lane_visible(cx)),
                        )
                        .size(px(crate::theme::size::DENSE)),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };
        let lane_body: Option<gpui::AnyElement> = if !unified_lane_visible {
            lane_collapsed.then(|| {
                div()
                    .flex()
                    .items_center()
                    .h(px(LANE_COLLAPSED_H))
                    .w_full()
                    .px(px(space::BASE))
                    .border_t(px(1.0))
                    .border_color(Colors::panel_border())
                    .bg(Colors::surface_panel())
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_faint())
                    .child(format!("{} lane hidden", self.lane_name()))
                    .into_any_element()
            })
        } else if self.lane_view == PianoLaneView::Articulations {
            Some(
                self.render_articulation_lane(cx, clip_id)
                    .into_any_element(),
            )
        } else if self.lane_view == PianoLaneView::Velocity {
            let vel_grid = self.build_velocity_grid();
            let vel_guides = self.build_velocity_guides();
            let vel_bars = self.build_velocity_bars(cx, clip_id, track_color);
            let velocity_gesture_overlay = self.build_velocity_gesture_overlay();
            let velocity_context_menu = self.build_velocity_context_menu(cx);
            let velocity_bounds = self.cc_bounds.clone();
            let velocity_bounds_canvas = canvas(
                move |bounds, _w, _cx| velocity_bounds.set(Some(bounds)),
                |_bounds, _scene, _w, _cx| {},
            )
            .absolute()
            .inset_0();
            let velocity_empty =
                (note_count_for_clip(cx, &self.timeline, clip_id) == 0).then(|| {
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_faint())
                        .child("No notes — draw notes above to edit velocity")
                });
            let velocity_value_chip = matches!(
                self.drag,
                PianoDrag::Velocity { .. }
                    | PianoDrag::VelocityPaint { .. }
                    | PianoDrag::VelocityLine { .. }
            )
            .then(|| {
                value_chip(
                    self.drag_value_status.as_deref().unwrap_or("Velocity"),
                    8.0,
                    8.0,
                )
            });
            Some(
                div()
                    .id("piano-vel")
                    .h(px(LANE_H))
                    .w_full()
                    .relative()
                    .overflow_hidden()
                    .border_t(px(1.0))
                    .border_color(Colors::panel_border())
                    .bg(Colors::surface_panel_alt())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                            this.begin_velocity_lane_click(ev, window, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                            this.open_velocity_context_menu(ev, window, cx);
                        }),
                    )
                    .child(velocity_bounds_canvas)
                    .children(vel_grid)
                    .children(vel_guides)
                    .children(vel_bars)
                    .children(velocity_gesture_overlay)
                    .children(velocity_empty)
                    .children(velocity_value_chip)
                    .children(velocity_context_menu)
                    .into_any_element(),
            )
        } else {
            Some(self.render_cc_lane(cx, clip_id).into_any_element())
        };
        let grid_cursor = if matches!(self.tool, PianoTool::Draw | PianoTool::Line) {
            gpui::CursorStyle::Crosshair
        } else {
            gpui::CursorStyle::Arrow
        };

        // Capture grid bounds so empty-area clicks can be mapped to beat/pitch.
        let grid_bounds = self.grid_bounds.clone();
        let grid_canvas = canvas(
            move |bounds, _w, _cx| {
                grid_bounds.set(Some(bounds));
            },
            |_b, _r, _w, _cx| {},
        )
        .absolute()
        .inset_0();

        // Capture the key-lane viewport bounds so a window-space cursor can be
        // hit-tested + mapped to a pitch (see `key_lane_pitch_at`). Sits behind
        // the keys and carries no handlers.
        let key_lane_bounds = self.key_lane_bounds.clone();
        let key_lane_canvas = canvas(
            move |bounds, _w, _cx| {
                key_lane_bounds.set(Some(bounds));
            },
            |_b, _r, _w, _cx| {},
        )
        .absolute()
        .inset_0();

        let ruler_bounds = self.ruler_bounds.clone();
        let ruler_bounds_canvas = canvas(
            move |bounds, _w, _cx| {
                ruler_bounds.set(Some(bounds));
            },
            |_b, _r, _w, _cx| {},
        )
        .absolute()
        .inset_0();

        div()
            .flex_1()
            .min_h_0()
            .relative()
            .flex()
            .flex_row()
            // Far left: MIDI Tools.
            .children(tools_panel)
            // Left: piano keys.
            .child(
                div()
                    .w(px(self.key_column_w()))
                    .flex_shrink_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    // Corner over the keys, beside the ruler.
                    .child(
                        div()
                            .h(px(RULER_H))
                            .w_full()
                            .bg(Colors::surface_panel())
                            .border_b(px(1.0))
                            .border_r(px(1.0))
                            .border_color(Colors::panel_border()),
                    )
                    .children(chord_strip_corner)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .overflow_hidden()
                            .bg(Colors::surface_canvas())
                            .border_r(px(1.0))
                            .border_color(Colors::panel_border())
                            .cursor(gpui::CursorStyle::PointingHand)
                            // One press handler for the whole lane, mapped
                            // through the same key geometry the keys are drawn
                            // with. Drag-scrub (move/up) is handled by the
                            // root-level `on_move`/`on_up`.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                                    // The rack's headers are controls, not keys.
                                    if this.rack_active() {
                                        return;
                                    }
                                    let Some(raw) = this.key_lane_pitch_at(ev.position) else {
                                        return;
                                    };
                                    // Drum lanes are sounds of the kit: the
                                    // scale lock has no say over which plays.
                                    let pitch = if this.drum_mode {
                                        raw
                                    } else {
                                        this.pitch_ctx.constrain_pitch(raw)
                                    };
                                    if midi_debug_enabled() {
                                        eprintln!("[PianoKeyPreview] down note={pitch}");
                                    }
                                    this.piano_key_drag_active = true;
                                    this.key_lane_pressed_pitch = Some(pitch);
                                    this.begin_preview_note(pitch, 100, "piano_key_down", cx);
                                    cx.notify();
                                }),
                            )
                            // A drum lane's name: right-click for its row
                            // actions (fill, clear, randomize velocity).
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                    if !this.drum_mode {
                                        return;
                                    }
                                    let row = if this.rack_active() {
                                        this.key_lane_row_at(ev.position)
                                    } else {
                                        this.key_lane_pitch_at(ev.position)
                                            .and_then(|pitch| this.drum_row_of(pitch))
                                    };
                                    let Some(row) = row else {
                                        return;
                                    };
                                    cx.stop_propagation();
                                    this.open_lane_menu_at(row, ev.position, window, cx);
                                }),
                            )
                            .child(key_lane_canvas)
                            .children(keys),
                    )
                    // The controller lane's header: selector, value scale.
                    .children(lane_header),
            )
            // Middle: ruler, grid, controller lane.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    // Ruler — bar/beat labels aligned to the grid below.
                    .child(
                        div()
                            .h(px(RULER_H))
                            .w_full()
                            .relative()
                            .overflow_hidden()
                            .bg(Colors::surface_panel())
                            .border_b(px(1.0))
                            .border_color(Colors::panel_border())
                            .cursor(gpui::CursorStyle::PointingHand)
                            .child(ruler_bounds_canvas)
                            .children(ruler)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    window.focus(&this.focus, cx);
                                    if let Some((lx, _)) = this.ruler_local(ev.position) {
                                        this.drag = PianoDrag::RulerSeek;
                                        this.seek_ruler_at(lx, cx);
                                    }
                                }),
                            ),
                    )
                    .children(chord_strip)
                    // Note grid.
                    .child(
                        div()
                            .id("piano-grid")
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .overflow_hidden()
                            .bg(Colors::surface_base())
                            .cursor(grid_cursor)
                            .child(grid_canvas)
                            .children(grid_lines)
                            .children(drum_hover_grid)
                            .children(clip_bounds)
                            .children(context_notes)
                            .children(loop_overlay)
                            .children(playhead_overlay)
                            .children(grid_empty_hint)
                            .children(notes_geo)
                            .children(stretch_handles)
                            .children(quantize_preview)
                            .when_some(marquee_overlay, |el, overlay| el.child(overlay))
                            .children(draw_preview)
                            .when_some(erase_overlay, |el, overlay| el.child(overlay))
                            .children(scrollbars)
                            .children(note_menu)
                            .children(lane_menu)
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_grid_down))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(Self::on_grid_right_down),
                            ),
                    )
                    // Single unified controller lane (velocity / CC / etc).
                    .children(lane_body),
            )
            .children(note_inspector)
    }

    /// The piano keyboard beside the grid, drawn as a keyboard: white keys
    /// run the full width and meet their neighbours halfway across a black
    /// key's row, black keys cover [`BLACK_KEY_SHARE`] of the lane in their
    /// own row. Every key edge is a grid row edge, so a key and its row can
    /// never disagree. One canvas for the keys; only the names are elements.
    /// The drum lanes inside a view `view_h` tall, by index from the top.
    pub(super) fn visible_drum_rows(&self, view_h: f32) -> std::ops::Range<usize> {
        let row_h = self.note_row_h().max(0.0001);
        let count = self.lane_count();
        let first = ((self.scroll_y / row_h).floor().max(0.0) as usize).min(count);
        let last = (((self.scroll_y + view_h) / row_h).ceil().max(0.0) as usize + 1).min(count);
        first..last.max(first)
    }

    /// The drum lane under the pointer, lit across `scope` (the lane list or
    /// the grid) with the hover state layer. It fades in over
    /// `motion::MICRO_MS` each time the pointer reaches a new lane, keyed by
    /// lane, so moving within one lane does not restart it. Paints nothing
    /// and takes no pointer events: hit-testing is unchanged under it.
    fn drum_hover_band(&self, scope: &'static str) -> Option<gpui::AnyElement> {
        if !self.drum_mode || self.rack_active() {
            return None;
        }
        let pitch = self.drum_hover_lane?;
        // A lane being auditioned already shows as pressed.
        if self.key_lane_pressed_pitch == Some(pitch) {
            return None;
        }
        let index = super::drum_map::drum_row_index(&self.drum_rows, pitch)?;
        let row_h = self.note_row_h();
        let y = index as f32 * row_h - self.scroll_y;
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(y))
                .h(px(row_h))
                .bg(Colors::state_hover())
                .with_animation(
                    gpui::ElementId::NamedInteger(scope.into(), pitch as u64),
                    Animation::new(Duration::from_millis(crate::theme::motion::MICRO_MS))
                        .with_easing(gpui::ease_out_quint()),
                    |band, t| band.opacity(t),
                )
                .into_any_element(),
        )
    }

    /// The drum editor's lane list in place of the keyboard: each lane's
    /// name, and its note quieter beside it. A press auditions the lane.
    fn build_drum_lane(&self) -> Vec<gpui::AnyElement> {
        use crate::theme::{space, typography};
        let row_h = self.note_row_h();
        let (_, view_h) = self.grid_view_size();
        let pressed = self.key_lane_pressed_pitch;
        let hovered = self.drum_hover_lane;
        let band = Colors::with_alpha(Colors::surface_base(), 0.35);
        let press = Colors::with_alpha(Colors::accent_primary(), 0.32);
        let seam = Colors::with_alpha(Colors::text_primary(), 0.06);

        let mut fills: Vec<(f32, Option<gpui::Rgba>)> = Vec::new();
        let mut labels: Vec<gpui::AnyElement> = Vec::new();
        for index in self.visible_drum_rows(view_h) {
            let pitch = self.drum_rows[index];
            let y = index as f32 * row_h - self.scroll_y;
            let is_pressed = pressed == Some(pitch);
            let fill = if is_pressed {
                Some(press)
            } else if index % 2 == 1 {
                Some(band)
            } else {
                None
            };
            fills.push((y, fill));
            if row_h < 10.0 {
                continue;
            }
            let is_hovered = hovered == Some(pitch) && !is_pressed;
            let label = div()
                .absolute()
                .left(px(space::SNUG))
                .right(px(space::SNUG))
                .top(px(y))
                .h(px(row_h))
                .flex()
                .items_center()
                .gap(px(space::TIGHT))
                .overflow_hidden()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(if is_pressed || is_hovered {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(self.drum_lane_name(pitch)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(if is_hovered {
                            Colors::text_secondary()
                        } else {
                            Colors::text_faint()
                        })
                        .child(note_name(pitch as i32)),
                );
            // The lit lane's name eases a hair to the right as it comes up,
            // with the band behind it: the lane answers the pointer.
            labels.push(if is_hovered {
                label
                    .with_animation(
                        gpui::ElementId::NamedInteger("pr-drum-hover-label".into(), pitch as u64),
                        Animation::new(Duration::from_millis(crate::theme::motion::MICRO_MS))
                            .with_easing(gpui::ease_out_quint()),
                        |label, t| label.left(px(space::SNUG + space::HAIR * t)),
                    )
                    .into_any_element()
            } else {
                label.into_any_element()
            });
        }

        let rows = canvas(
            |_bounds, _window, _cx| (),
            move |bounds: gpui::Bounds<gpui::Pixels>, (), window, _cx| {
                let ox: f32 = bounds.origin.x.into();
                let oy: f32 = bounds.origin.y.into();
                let w: f32 = bounds.size.width.into();
                window.paint_layer(bounds, |window| {
                    for (top, fill) in &fills {
                        if let Some(fill) = fill {
                            window.paint_quad(gpui::fill(
                                gpui::Bounds {
                                    origin: gpui::point(px(ox), px(oy + top)),
                                    size: gpui::size(px(w), px(row_h)),
                                },
                                *fill,
                            ));
                        }
                        window.paint_quad(gpui::fill(
                            gpui::Bounds {
                                origin: gpui::point(px(ox), px(oy + top + row_h - 1.0)),
                                size: gpui::size(px(w), px(1.0)),
                            },
                            seam,
                        ));
                    }
                });
            },
        )
        .absolute()
        .inset_0()
        .into_any_element();

        let mut out = Vec::with_capacity(labels.len() + 2);
        out.push(rows);
        out.extend(self.drum_hover_band("pr-drum-hover-lane"));
        out.extend(labels);
        out
    }

    /// The Sampler Rack's row headers in place of the lane names, one per
    /// track: its name (opens its instrument), its sample (loads one, or
    /// takes a dropped file), mute, solo, volume and pan. "Add Sampler" sits
    /// under the last row. Right-click a row for its pattern actions.
    fn build_rack_lane(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        use crate::theme::{space, typography};
        let row_h = self.note_row_h();
        let (_, view_h) = self.grid_view_size();
        let hovered_row = self.step_hover.map(|(row, _)| row);
        let seam = Colors::with_alpha(Colors::text_primary(), 0.06);
        let band = Colors::with_alpha(Colors::surface_base(), 0.35);
        let drop_target = Colors::with_alpha(Colors::accent_primary(), 0.14);
        let mut out: Vec<gpui::AnyElement> = Vec::new();

        let toggle = |id: (&'static str, usize), label: &'static str, on: bool, hue: gpui::Rgba| {
            div()
                .id(id)
                .flex_none()
                .w(px(16.0))
                .h(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(crate::theme::radius::CONTROL_SM))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                // The track header's latch: tinted ground, its hue as text.
                .bg(if on {
                    Colors::latched(Colors::surface_panel_alt(), hue).0
                } else {
                    Colors::button_bg()
                })
                .text_color(if on { hue } else { Colors::text_muted() })
                .cursor(gpui::CursorStyle::PointingHand)
                .child(label)
        };
        // A fader as a thin bar: dragged sideways, read by its fill.
        let bar = |id: (&'static str, usize), width: f32, from: f32, to: f32, fill: gpui::Rgba| {
            div()
                .id(id)
                .flex_none()
                .w(px(width))
                .h(px(12.0))
                .flex()
                .items_center()
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .child(
                    div()
                        .relative()
                        .w_full()
                        .h(px(4.0))
                        .rounded(px(2.0))
                        .bg(Colors::with_alpha(Colors::text_primary(), 0.12))
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(px(width * from.min(to)))
                                .w(px((width * (to - from).abs()).max(1.0)))
                                .rounded(px(2.0))
                                .bg(fill),
                        ),
                )
        };

        for index in self.visible_drum_rows(view_h) {
            let Some(row) = self.rack_rows.get(index) else {
                continue;
            };
            let y = index as f32 * row_h - self.scroll_y;
            let (sample, loads) = self.rack_sample_label(row, cx);
            let track_id = row.track_id.clone();
            let lit = hovered_row == Some(index);
            let volume_fill = Colors::with_alpha(Colors::text_primary(), 0.65);
            let pan = row.pan.clamp(-1.0, 1.0);
            let header = div()
                .id(("pr-rack-row", index))
                .absolute()
                .left_0()
                .right_0()
                .top(px(y))
                .h(px(row_h))
                .flex()
                .items_center()
                .gap(px(space::TIGHT))
                .px(px(space::SNUG))
                .border_b(px(1.0))
                .border_color(seam)
                .when(index % 2 == 1, |b| b.bg(band))
                .when(lit, |b| b.bg(Colors::state_hover()))
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_target))
                .on_drop(cx.listener({
                    let track_id = track_id.clone();
                    move |this, paths: &ExternalPaths, window, cx| {
                        if let Some(path) = paths.paths().first() {
                            this.send_rack_request(
                                RackRequest::LoadSample {
                                    track_id: track_id.clone(),
                                    path: Some(path.clone()),
                                },
                                window,
                                cx,
                            );
                        }
                    }
                }))
                // Which track: its colour, as on its arrangement header.
                .child(
                    div()
                        .flex_none()
                        .w(px(3.0))
                        .h(px((row_h - 6.0).max(4.0)))
                        .rounded(px(1.5))
                        .bg(row.color),
                )
                .child(
                    div()
                        .id(("pr-rack-name", index))
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(if lit || row.solo {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .cursor(gpui::CursorStyle::PointingHand)
                        .tooltip(crate::components::controls::fb_tooltip(format!(
                            "Open {}'s instrument",
                            row.name
                        )))
                        .on_click(cx.listener({
                            let track_id = track_id.clone();
                            move |this, _: &gpui::ClickEvent, window, cx| {
                                this.send_rack_request(
                                    RackRequest::OpenEditor {
                                        track_id: track_id.clone(),
                                    },
                                    window,
                                    cx,
                                );
                            }
                        }))
                        .child(row.name.clone()),
                )
                .child(
                    div()
                        .id(("pr-rack-sample", index))
                        .flex_none()
                        .w(px(84.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .when(loads, |b| {
                            b.cursor(gpui::CursorStyle::PointingHand)
                                .hover(|s| s.text_color(Colors::text_secondary()))
                                .tooltip(crate::components::controls::fb_tooltip(
                                    "Load a sample (or drop an audio file on the row)",
                                ))
                                .on_click(cx.listener({
                                    let track_id = track_id.clone();
                                    move |this, _: &gpui::ClickEvent, window, cx| {
                                        this.send_rack_request(
                                            RackRequest::LoadSample {
                                                track_id: track_id.clone(),
                                                path: None,
                                            },
                                            window,
                                            cx,
                                        );
                                    }
                                }))
                        })
                        .child(sample),
                )
                .child(
                    toggle(
                        ("pr-rack-mute", index),
                        "M",
                        row.muted,
                        Colors::state_mute(),
                    )
                    .tooltip(crate::components::controls::fb_tooltip("Mute"))
                    .on_click(cx.listener({
                        let track_id = track_id.clone();
                        move |this, _: &gpui::ClickEvent, window, cx| {
                            this.toggle_rack_row(&track_id, false, window, cx);
                        }
                    })),
                )
                .child(
                    toggle(("pr-rack-solo", index), "S", row.solo, Colors::state_solo())
                        .tooltip(crate::components::controls::fb_tooltip("Solo"))
                        .on_click(cx.listener({
                            let track_id = track_id.clone();
                            move |this, _: &gpui::ClickEvent, window, cx| {
                                this.toggle_rack_row(&track_id, true, window, cx);
                            }
                        })),
                )
                .child(
                    bar(
                        ("pr-rack-volume", index),
                        36.0,
                        0.0,
                        row.volume,
                        volume_fill,
                    )
                    .tooltip(crate::components::controls::fb_tooltip(format!(
                        "Volume {:+.1} dB (drag sideways)",
                        crate::components::timeline::timeline_state::volume::norm_to_db(row.volume)
                    )))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener({
                            let track_id = track_id.clone();
                            move |this, ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.begin_rack_mixer(
                                    &track_id,
                                    RackControl::Volume,
                                    ev.position.x.into(),
                                    cx,
                                );
                            }
                        }),
                    ),
                )
                .child(
                    bar(
                        ("pr-rack-pan", index),
                        26.0,
                        0.5,
                        0.5 + pan * 0.5,
                        volume_fill,
                    )
                    .tooltip(crate::components::controls::fb_tooltip(
                        "Pan (drag sideways)",
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener({
                            let track_id = track_id.clone();
                            move |this, ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.begin_rack_mixer(
                                    &track_id,
                                    RackControl::Pan,
                                    ev.position.x.into(),
                                    cx,
                                );
                            }
                        }),
                    ),
                );
            out.push(header.into_any_element());
        }

        // A new row: another sampler track in this rack.
        let add_y = self.rack_rows.len() as f32 * row_h - self.scroll_y;
        if add_y < view_h {
            let editing_track = self.editing_clip_id(cx).and_then(|clip_id| {
                self.timeline
                    .read(cx)
                    .state
                    .find_clip(&clip_id)
                    .map(|(track, _)| track.id.clone())
            });
            if let Some(track_id) = editing_track {
                out.push(
                    div()
                        .id("pr-rack-add")
                        .absolute()
                        .left(px(space::SNUG))
                        .top(px(add_y + 2.0))
                        .h(px((row_h - 4.0).max(14.0)))
                        .px(px(space::SNUG))
                        .flex()
                        .items_center()
                        .rounded(px(crate::theme::radius::CONTROL_SM))
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_secondary())
                        .hover(|s| {
                            s.bg(Colors::state_hover())
                                .text_color(Colors::text_primary())
                        })
                        .cursor(gpui::CursorStyle::PointingHand)
                        .tooltip(crate::components::controls::fb_tooltip(
                            "Add a row: a new track with the Quick Sampler, in this rack's group",
                        ))
                        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                            this.send_rack_request(
                                RackRequest::AddRow {
                                    track_id: track_id.clone(),
                                },
                                window,
                                cx,
                            );
                        }))
                        .child("+ Add Sampler")
                        .into_any_element(),
                );
            }
        }

        // A fader drag follows the pointer anywhere in the window.
        if matches!(self.drag, PianoDrag::RackMixer { .. }) {
            let this = cx.weak_entity();
            out.push(
                gpui::canvas(
                    |_, _, _| {},
                    move |_, (), window, _| {
                        let moved = this.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                let _ = moved.update(cx, |this, cx| {
                                    this.on_rack_mixer_move(event, window, cx)
                                });
                            }
                        });
                        let released = this.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                let _ = released.update(cx, |this, cx| this.on_rack_mixer_up(cx));
                            }
                        });
                    },
                )
                .absolute()
                .size_0()
                .into_any_element(),
            );
        }
        out
    }

    pub(super) fn build_key_lane(
        &self,
        first_pitch: i32,
        last_pitch: i32,
    ) -> Vec<gpui::AnyElement> {
        if self.drum_mode {
            return self.build_drum_lane();
        }
        use crate::theme::typography;
        let row_h = self.note_row_h();
        let half = row_h * 0.5;
        let scale = self.pitch_ctx.scale;
        let scale_active = scale.kind != ScaleKind::Chromatic;
        let pressed = self.key_lane_pressed_pitch;

        // Resolved once here; the paint closure only fills quads. Keys out of
        // the scale are shaded toward the canvas; the scale's root and the
        // sounding key take the accent.
        let white = Colors::piano_white_key();
        let black = Colors::piano_black_key();
        let shade = Colors::with_alpha(Colors::surface_canvas(), 0.38);
        let white_out = Colors::composite(white, shade);
        let black_out = Colors::composite(black, Colors::with_alpha(white, 0.10));
        let root_wash = Colors::with_alpha(Colors::accent_primary(), 0.28);
        let press = Colors::with_alpha(Colors::accent_primary(), 0.62);
        let seam = Colors::piano_key_seam();
        let key_label = Colors::piano_key_label();

        let mut whites: Vec<(f32, f32, gpui::Rgba)> = Vec::new();
        let mut blacks: Vec<(f32, f32, gpui::Rgba)> = Vec::new();
        let mut labels: Vec<gpui::AnyElement> = Vec::new();
        for p in first_pitch..=last_pitch {
            let pitch = p as u8;
            let y = self.pitch_to_y(pitch);
            let in_scale = !scale_active || scale.contains_pitch(pitch);
            let is_root = scale_active && pitch % 12 == scale.root.pitch_class();
            let is_pressed = pressed == Some(pitch);
            if is_black(p) {
                let mut fill = if in_scale { black } else { black_out };
                if is_root {
                    fill = Colors::composite(fill, root_wash);
                }
                if is_pressed {
                    fill = Colors::composite(fill, press);
                }
                blacks.push((y, row_h, fill));
                continue;
            }
            // A white key reaches half a row into each black neighbour.
            let top = y - if p < PITCH_CNT - 1 && is_black(p + 1) {
                half
            } else {
                0.0
            };
            let bottom = y + row_h + if p > 0 && is_black(p - 1) { half } else { 0.0 };
            let mut fill = if in_scale { white } else { white_out };
            if is_root {
                fill = Colors::composite(fill, root_wash);
            }
            if is_pressed {
                fill = Colors::composite(fill, press);
            }
            whites.push((top, bottom, fill));

            // Names: every C (the octave), the scale's root, the pressed key,
            // and every white key once rows are tall enough to read them.
            let is_c = p % 12 == 0;
            if is_c || is_root || is_pressed || row_h >= 14.0 {
                // Dark on the white key; the octave and the root are set in
                // full strength and weight, the rest quieter.
                let color = if is_c || is_root || is_pressed {
                    key_label
                } else {
                    Colors::with_alpha(key_label, 0.62)
                };
                let center = (top + bottom) * 0.5;
                labels.push(
                    div()
                        .absolute()
                        .right(px(crate::theme::space::SNUG))
                        .top(px(center - 6.0))
                        .h(px(12.0))
                        .flex()
                        .items_center()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(if is_c || is_root {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .text_color(color)
                        .child(note_name(p))
                        .into_any_element(),
                );
            }
        }

        let keys = canvas(
            |_bounds, _window, _cx| (),
            move |bounds: gpui::Bounds<gpui::Pixels>, (), window, _cx| {
                let ox: f32 = bounds.origin.x.into();
                let oy: f32 = bounds.origin.y.into();
                let w: f32 = bounds.size.width.into();
                let black_w = (w * BLACK_KEY_SHARE).round();
                window.paint_layer(bounds, |window| {
                    for (top, bottom, fill_color) in &whites {
                        let rect = gpui::Bounds {
                            origin: gpui::point(px(ox), px(oy + top)),
                            size: gpui::size(px(w), px((bottom - top).max(0.0))),
                        };
                        window.paint_quad(gpui::fill(rect, *fill_color));
                        // The seam under each white key.
                        let line = gpui::Bounds {
                            origin: gpui::point(px(ox), px(oy + bottom - 1.0)),
                            size: gpui::size(px(w), px(1.0)),
                        };
                        window.paint_quad(gpui::fill(line, seam));
                    }
                    for (top, h, fill_color) in &blacks {
                        let rect = gpui::Bounds {
                            origin: gpui::point(px(ox), px(oy + top)),
                            size: gpui::size(px(black_w), px(*h)),
                        };
                        window.paint_quad(gpui::fill(rect, *fill_color));
                    }
                });
            },
        )
        .absolute()
        .inset_0()
        .into_any_element();

        let mut out = Vec::with_capacity(labels.len() + 1);
        out.push(keys);
        out.extend(labels);
        out
    }

    /// The controller lane's header in the key column: the lane selector and
    /// a hide button on top, the value scale under them.
    fn render_lane_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use crate::theme::{size, space, typography};
        let scale_labels: Vec<gpui::AnyElement> = if self.lane_view == PianoLaneView::Velocity {
            let (_, lane_h) = self.cc_view_size();
            let usable = (lane_h - 8.0).max(1.0);
            [96u8, 64, 32]
                .into_iter()
                .map(|value| {
                    let y = velocity_lane_y(value, usable);
                    div()
                        .absolute()
                        .right(px(space::SNUG))
                        .top(px(y - 6.0))
                        .h(px(12.0))
                        .flex()
                        .items_center()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(value.to_string())
                        .into_any_element()
                })
                .collect()
        } else {
            vec![div()
                .absolute()
                .right(px(space::SNUG))
                .bottom(px(space::TIGHT))
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(self.lane_range())
                .into_any_element()]
        };
        div()
            .relative()
            .flex_shrink_0()
            .h(px(LANE_H))
            .w_full()
            .border_t(px(1.0))
            .border_r(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            // The selector takes the whole width so the lane's name reads.
            .child(
                div()
                    .absolute()
                    .top(px(space::TIGHT))
                    .left(px(space::TIGHT))
                    .right(px(space::TIGHT))
                    .child(self.render_lane_selector(cx)),
            )
            .children(scale_labels)
            // Hide sits at the foot, where the lane folds down to.
            .child(
                div()
                    .absolute()
                    .left(px(space::TIGHT))
                    .bottom(px(space::TIGHT))
                    .child(
                        bar_icon_button(
                            "pr-lane-toggle",
                            assets::ICON_CHEVRON_DOWN_PATH,
                            "Hide lane".into(),
                            None,
                            cx.listener(|this, _ev, _w, cx| this.toggle_lane_visible(cx)),
                        )
                        .size(px(size::DENSE)),
                    ),
            )
            .into_any_element()
    }

    /// The grid's scrollbars, along its right and bottom edges: drag the thumb
    /// or click the track to scroll, the same as the mixer's. They overlay the
    /// grid (whose size every coordinate in the roll is measured against) and
    /// take the pointer before it, so a press on a bar never starts a note.
    fn render_scrollbars(&self, cx: &Context<Self>) -> Vec<gpui::AnyElement> {
        use crate::components::scrollbar::{scrollbar, Axis, THICKNESS};
        let (view_w, view_h) = self.grid_view_size();
        let max_x = self.max_scroll_x(cx);
        let max_y = self.max_scroll_y();
        let has_x = max_x > 0.5;
        let has_y = max_y > 0.5;
        // Where both show, each stops short of the corner the other runs into.
        let corner = |other: bool| if other { THICKNESS } else { 0.0 };
        let entity = cx.entity().downgrade();
        let mut bars = Vec::new();

        if has_y {
            let entity = entity.clone();
            let on_scroll =
                std::sync::Arc::new(move |y: f32, _: &mut Window, cx: &mut gpui::App| {
                    let _ = entity.update(cx, |this, cx| {
                        let y = y.clamp(0.0, this.max_scroll_y());
                        if (this.scroll_y - y).abs() > 0.01 {
                            this.scroll_y = y;
                            cx.notify();
                        }
                    });
                });
            let track_len = (view_h - corner(has_x)).max(1.0);
            if let Some(bar) = scrollbar(
                "piano-roll-vscroll",
                Axis::Vertical,
                self.scroll_y,
                view_h + max_y,
                view_h,
                max_y,
                track_len,
                on_scroll,
            ) {
                bars.push(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .h(px(track_len))
                        .w(px(THICKNESS))
                        .child(bar)
                        .into_any_element(),
                );
            }
        }

        if has_x {
            let on_scroll =
                std::sync::Arc::new(move |x: f32, _: &mut Window, cx: &mut gpui::App| {
                    let _ = entity.update(cx, |this, cx| {
                        let x = x.clamp(0.0, this.max_scroll_x(cx));
                        if (this.scroll_x - x).abs() > 0.01 {
                            let before = this.scroll_x;
                            this.scroll_x = x;
                            this.note_user_scrolled_x(before, cx);
                            cx.notify();
                        }
                    });
                });
            let track_len = (view_w - corner(has_y)).max(1.0);
            if let Some(bar) = scrollbar(
                "piano-roll-hscroll",
                Axis::Horizontal,
                self.scroll_x,
                view_w + max_x,
                view_w,
                max_x,
                track_len,
                on_scroll,
            ) {
                bars.push(
                    div()
                        .absolute()
                        .left_0()
                        .bottom_0()
                        .w(px(track_len))
                        .h(px(THICKNESS))
                        .child(bar)
                        .into_any_element(),
                );
            }
        }

        bars
    }

    /// Articulation palette for the selected notes: one chip per articulation
    /// the instrument can play, plus "None". Applies to the whole selection as
    /// one undo entry.
    pub(super) fn articulation_assign_row(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        fn button_id(articulation: Option<ArticulationId>) -> &'static str {
            match articulation {
                None => "pr-art-assign-none",
                Some(ArticulationId::Sustain) => "pr-art-assign-sustain",
                Some(ArticulationId::Staccato) => "pr-art-assign-staccato",
                Some(ArticulationId::Staccatissimo) => "pr-art-assign-staccatissimo",
                Some(ArticulationId::Legato) => "pr-art-assign-legato",
                Some(ArticulationId::Tenuto) => "pr-art-assign-tenuto",
                Some(ArticulationId::Accent) => "pr-art-assign-accent",
                Some(ArticulationId::Marcato) => "pr-art-assign-marcato",
                Some(ArticulationId::Pizzicato) => "pr-art-assign-pizzicato",
                Some(ArticulationId::Tremolo) => "pr-art-assign-tremolo",
            }
        }
        // Solfege tracks narrow the palette to what the loaded instrument can
        // actually play; everything else keeps the full built-in vocabulary.
        let available = self.available_articulations(cx);
        // `None` while the selection is mixed, so no chip claims to be the
        // current value.
        let current = self.uniform_selection_articulation(cx);
        let mut chips: Vec<gpui::AnyElement> = available
            .iter()
            .map(|articulation| {
                let articulation = *articulation;
                insp_chip(
                    button_id(Some(articulation)),
                    articulation.short_name(),
                    current == Some(Some(articulation)),
                    cx.listener(move |this, _, _w, cx| {
                        this.set_selection_articulation(Some(articulation), cx)
                    }),
                )
                .into_any_element()
            })
            .collect();
        chips.push(
            insp_chip(
                button_id(None),
                "None",
                current == Some(None),
                cx.listener(|this, _, _w, cx| this.set_selection_articulation(None, cx)),
            )
            .into_any_element(),
        );
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(crate::theme::space::TIGHT))
            .children(chips)
            .into_any_element()
    }

    fn render_note_expression_inspector(
        &self,
        note: &MidiNoteState,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        fn range(curve: &sphere_midi_service::ExpressionCurve) -> Option<(f32, f32)> {
            let mut values = curve.points.iter().map(|point| point.value);
            let first = values.next()?;
            let (mut min, mut max) = (first, first);
            for value in values {
                min = min.min(value);
                max = max.max(value);
            }
            Some((min, max))
        }

        fn row(
            label: &str,
            curve: &sphere_midi_service::ExpressionCurve,
            percent: bool,
        ) -> gpui::AnyElement {
            let value = range(curve)
                .map(|(min, max)| {
                    if percent {
                        format!(
                            "{} pts · {:.0}–{:.0}%",
                            curve.points.len(),
                            min * 100.0,
                            max * 100.0
                        )
                    } else {
                        format!("{} pts · {:.2}–{:.2}", curve.points.len(), min, max)
                    }
                })
                .unwrap_or_else(|| "None".to_string());
            insp_row(label, value, None).into_any_element()
        }

        vec![
            insp_section("EXPRESSION").into_any_element(),
            row("Pitch", &note.expression.pitch, false),
            row("Pressure", &note.expression.pressure, true),
            row("Timbre", &note.expression.timbre, true),
            insp_row(
                "Release",
                note.expression
                    .release_velocity
                    .map(|value| format!("{:.0}%", value.clamp(0.0, 1.0) * 100.0))
                    .unwrap_or_else(|| "Default".to_string()),
                None,
            )
            .into_any_element(),
            insp_action_row(vec![
                insp_action(
                    "pr-expression-reset-pitch",
                    "Pitch",
                    false,
                    cx.listener(|this, _, _w, cx| {
                        this.reset_selected_expression(Some(NoteExpressionLane::Pitch), cx)
                    }),
                )
                .into_any_element(),
                insp_action(
                    "pr-expression-reset-pressure",
                    "Pressure",
                    false,
                    cx.listener(|this, _, _w, cx| {
                        this.reset_selected_expression(Some(NoteExpressionLane::Pressure), cx)
                    }),
                )
                .into_any_element(),
                insp_action(
                    "pr-expression-reset-timbre",
                    "Timbre",
                    false,
                    cx.listener(|this, _, _w, cx| {
                        this.reset_selected_expression(Some(NoteExpressionLane::Timbre), cx)
                    }),
                )
                .into_any_element(),
            ])
            .into_any_element(),
            insp_action_row(vec![insp_action(
                "pr-expression-reset-all",
                "Reset All Expression",
                false,
                cx.listener(|this, _, _w, cx| this.reset_selected_expression(None, cx)),
            )
            .into_any_element()])
            .into_any_element(),
        ]
    }

    /// The note inspector: the selection's real values, each with the nudge
    /// that edits it beside it, then its articulation and the commands on it.
    /// With nothing selected it describes the clip instead of saying nothing.
    pub(super) fn render_note_inspector(
        &self,
        cx: &mut Context<Self>,
        clip_id: &str,
    ) -> impl IntoElement {
        use crate::theme::{size, space, typography};
        let snapshot = self.note_inspector_snapshot(cx, clip_id);
        let count = snapshot.count();
        let step = self.grid_res.beats().max(MIN_NOTE_BEATS);
        let fine_step = (step * 0.25).max(MIN_NOTE_BEATS);

        let title = match count {
            0 => "Clip".to_string(),
            1 => "Note".to_string(),
            n => format!("{} Notes", group_thousands(n)),
        };
        let mut content: Vec<gpui::AnyElement> = vec![div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(space::SNUG))
            .h(px(size::DEFAULT))
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(title),
            )
            .when(count > 0, |header| {
                header.child(crate::components::controls::fb_badge(
                    snapshot.pitch_label(),
                    Colors::accent_primary(),
                ))
            })
            .into_any_element()];

        if count == 0 {
            let (notes, lowest, highest) = {
                let tl = self.timeline.read(cx);
                tl.state
                    .midi_clip_notes(clip_id)
                    .map(|notes| {
                        (
                            notes.len(),
                            notes.iter().map(|n| n.pitch).min(),
                            notes.iter().map(|n| n.pitch).max(),
                        )
                    })
                    .unwrap_or((0, None, None))
            };
            let length = self
                .scope
                .editing()
                .map(|span| span.duration_beats)
                .unwrap_or(0.0);
            content.push(insp_row("Notes", group_thousands(notes), None).into_any_element());
            content.push(
                insp_row(
                    "Range",
                    match (lowest, highest) {
                        (Some(lo), Some(hi)) if lo == hi => note_name(lo as i32),
                        (Some(lo), Some(hi)) => {
                            format!("{} – {}", note_name(lo as i32), note_name(hi as i32))
                        }
                        _ => "—".to_string(),
                    },
                    None,
                )
                .into_any_element(),
            );
            content.push(
                insp_row("Length", format!("{} beats", format_beats(length)), None)
                    .into_any_element(),
            );
            content.push(
                div()
                    .pt(px(space::BASE))
                    .text_size(px(typography::DENSE_LABEL))
                    .line_height(px(15.0))
                    .text_color(Colors::text_muted())
                    .child("Select notes to edit their pitch, timing and velocity.")
                    .into_any_element(),
            );
        } else {
            let single = snapshot.selected.first().filter(|_| count == 1).cloned();
            let (start, end_label, end_value) = match &single {
                Some(note) => {
                    let origin = self.edit_origin();
                    let tl = self.timeline.read(cx);
                    (
                        tl.state.format_position(origin + note.start),
                        "End",
                        tl.state
                            .format_position(origin + note.start + note.duration),
                    )
                }
                None => (snapshot.start_label(), "Range", snapshot.end_label()),
            };

            content.push(insp_section("PITCH & TIME").into_any_element());
            content.push(
                insp_row(
                    "Pitch",
                    snapshot.pitch_label(),
                    Some(insp_stepper(
                        "pr-note-pitch",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_pitch(-1, cx)),
                        cx.listener(|this, _, _w, cx| this.nudge_selected_pitch(1, cx)),
                    )),
                )
                .into_any_element(),
            );
            content.push(
                insp_row(
                    "Start",
                    start,
                    Some(insp_stepper(
                        "pr-note-start",
                        cx.listener(move |this, _, _w, cx| {
                            this.nudge_selected_start(false, step, cx)
                        }),
                        cx.listener(move |this, _, _w, cx| {
                            this.nudge_selected_start(true, step, cx)
                        }),
                    )),
                )
                .into_any_element(),
            );
            content.push(
                insp_row(
                    "Length",
                    snapshot.length_label(),
                    Some(insp_stepper(
                        "pr-note-len",
                        cx.listener(move |this, _, _w, cx| {
                            this.nudge_selected_length(false, fine_step, cx)
                        }),
                        cx.listener(move |this, _, _w, cx| {
                            this.nudge_selected_length(true, fine_step, cx)
                        }),
                    )),
                )
                .into_any_element(),
            );
            content.push(insp_row(end_label, end_value, None).into_any_element());

            content.push(insp_section("PERFORMANCE").into_any_element());
            content.push(
                insp_row(
                    "Velocity",
                    snapshot.velocity_label(),
                    Some(insp_stepper(
                        "pr-note-vel",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_velocity(-5, cx)),
                        cx.listener(|this, _, _w, cx| this.nudge_selected_velocity(5, cx)),
                    )),
                )
                .into_any_element(),
            );
            content.push(
                insp_row(
                    "Channel",
                    snapshot.channel_label(),
                    Some(insp_stepper(
                        "pr-note-chan",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_channel(-1, cx)),
                        cx.listener(|this, _, _w, cx| this.nudge_selected_channel(1, cx)),
                    )),
                )
                .into_any_element(),
            );

            content.push(insp_section("ARTICULATION").into_any_element());
            content.push(self.articulation_assign_row(cx));

            // Community keeps expression in the project and shows the inline
            // curve on the note, but the mutating inspector is a
            // Professional-only editing surface.
            if let Some(note) = &single {
                if crate::edition::professional_features_available() {
                    content.extend(self.render_note_expression_inspector(note, cx));
                }
            }

            let muted = match &single {
                Some(note) => note.muted,
                None => self.selection_all_muted(cx),
            };
            content.push(insp_section("ACTIONS").into_any_element());
            content.push(
                insp_action_row(vec![
                    insp_action(
                        "pr-note-mute",
                        if muted { "Unmute" } else { "Mute" },
                        false,
                        cx.listener(|this, _, _w, cx| this.toggle_mute_selection(cx)),
                    )
                    .into_any_element(),
                    insp_action(
                        "pr-notes-duplicate",
                        "Duplicate",
                        false,
                        cx.listener(|this, _, _w, cx| this.duplicate_selection(false, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                insp_action_row(vec![
                    insp_action(
                        "pr-notes-quantize",
                        "Quantize",
                        false,
                        cx.listener(|this, _, _w, cx| this.quantize_selection(cx)),
                    )
                    .into_any_element(),
                    insp_action(
                        "pr-notes-delete",
                        "Delete",
                        true,
                        cx.listener(|this, _, _w, cx| this.delete_selection(cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
        }

        div()
            .id("pr-note-inspector")
            .w(px(INSPECTOR_W))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(space::HAIR))
            .px(px(space::LOOSE))
            .py(px(space::BASE))
            .border_l(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            .children(content)
    }

    pub(super) fn track_color_for_clip(&self, cx: &Context<Self>, clip_id: &str) -> gpui::Rgba {
        let tl = self.timeline.read(cx);
        tl.state
            .tracks
            .iter()
            .find(|t| t.clips.iter().any(|c| c.id == clip_id))
            .map(|t| t.color)
            .unwrap_or_else(Colors::accent_primary)
    }

    /// Compute the visible vertical gridlines with a zoom-aware subdivision
    /// tier. Returns `(x_px, kind)` for each line in `[start_beat, end_beat]`.
    ///
    /// Tiering by `px_per_beat` (`self.ppb`):
    /// - always: bar lines
    /// - `ppb >= 10`: beat lines
    /// - subdivision (snap step) lines only when they're at least ~7 px apart
    ///   and the view is zoomed in enough — keeps far-zoom views uncluttered.
    /// Vertical grid lines for the visible range.
    ///
    /// Geometry lives on [`PianoRollViewport`] so the Solfege Pitch tab draws
    /// the identical grid from the identical transform.
    pub(super) fn visible_grid_lines(
        &self,
        start_beat: f32,
        end_beat: f32,
    ) -> Vec<(f32, GridLineKind)> {
        self.viewport()
            .grid_lines_in(start_beat, end_beat, &self.meter)
    }
    /// The track's clips, drawn where they actually sit on the timeline.
    ///
    /// Everything outside the clip being edited is shaded, and each neighbour
    /// gets its own boundary lines and a name — so the editor reads as a window
    /// onto the song rather than a part with no context. A fill written to land
    /// on the downbeat of the next clip can now be seen to land on it.
    ///
    /// The edited clip is the one left unshaded: it is the only one this pass
    /// accepts edits for, and shading is how that is said without a second
    /// colour meaning something new.
    pub(super) fn build_clip_bounds_overlay(
        &self,
        view_w: f32,
        view_h: f32,
    ) -> Vec<gpui::AnyElement> {
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        let Some(editing) = self.scope.editing() else {
            return out;
        };

        let edit_x0 = self.project_beat_to_x(editing.start_beat);
        let edit_x1 = self.project_beat_to_x(editing.end_beat());

        // Shade everything that is not the edited clip, in two pieces so the
        // clip itself keeps the grid's own background.
        for (x0, x1) in [(0.0_f32, edit_x0), (edit_x1, view_w)] {
            let x0 = x0.max(0.0);
            let x1 = x1.min(view_w);
            if x1 > x0 {
                out.push(
                    div()
                        .absolute()
                        .left(px(x0))
                        .top_0()
                        .w(px(x1 - x0))
                        .h(px(view_h))
                        .bg(Colors::with_alpha(Colors::surface_base(), 0.55))
                        .into_any_element(),
                );
            }
        }

        for span in self.scope.spans() {
            let x0 = self.project_beat_to_x(span.start_beat);
            let x1 = self.project_beat_to_x(span.end_beat());
            if x1 < -2.0 || x0 > view_w + 2.0 {
                continue;
            }
            // The edited clip's edges are stated more strongly than its
            // neighbours', because they are the bounds an edit is bound by.
            let alpha = if span.editable { 0.55 } else { 0.28 };
            for x in [x0, x1] {
                if x >= -1.0 && x <= view_w + 1.0 {
                    out.push(
                        div()
                            .absolute()
                            .left(px(x))
                            .top_0()
                            .w(px(1.0))
                            .h(px(view_h))
                            .bg(Colors::with_alpha(Colors::accent_primary(), alpha))
                            .into_any_element(),
                    );
                }
            }
            // A neighbour says which clip it is. The edited one does not need
            // to — the window title already says, and a label over the notes
            // being edited is in the way.
            if !span.editable && x1 - x0 > 24.0 {
                out.push(
                    div()
                        .absolute()
                        .left(px(x0 + 4.0))
                        .top(px(2.0))
                        .max_w(px((x1 - x0 - 8.0).max(0.0)))
                        .truncate()
                        .text_size(px(9.0))
                        .text_color(Colors::text_faint())
                        .child(span.name.clone())
                        .into_any_element(),
                );
            }
        }
        out
    }

    /// Loop region band + edge lines over the note grid (project beats).
    /// Returns empty when looping is off or the region is fully off-screen.
    pub(super) fn build_loop_overlay(
        &self,
        loop_region: Option<(f32, f32)>,
        view_w: f32,
        view_h: f32,
    ) -> Vec<gpui::AnyElement> {
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        let Some((lo, hi)) = loop_region else {
            return out;
        };
        let band_x0 = self.project_beat_to_x(lo).max(0.0);
        let band_x1 = self.project_beat_to_x(hi).min(view_w);
        if band_x1 <= 0.0 || band_x0 >= view_w || band_x1 <= band_x0 {
            return out;
        }
        let accent = Colors::accent_primary();
        out.push(
            div()
                .absolute()
                .left(px(band_x0))
                .top_0()
                .w(px(band_x1 - band_x0))
                .h(px(view_h))
                .bg(Colors::with_alpha(accent, 0.06))
                .into_any_element(),
        );
        // Edge lines, drawn only when their exact beat is on-screen.
        for edge in [lo, hi] {
            let ex = self.project_beat_to_x(edge);
            if ex >= 0.0 && ex <= view_w {
                out.push(
                    div()
                        .absolute()
                        .left(px(ex))
                        .top_0()
                        .w(px(1.0))
                        .h(px(view_h))
                        .bg(Colors::with_alpha(accent, 0.5))
                        .into_any_element(),
                );
            }
        }
        out
    }

    /// Loop region band in the ruler header (clip-local beats).
    pub(super) fn build_loop_ruler_markers(
        &self,
        loop_region: Option<(f32, f32)>,
    ) -> Vec<gpui::AnyElement> {
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        let Some((lo, hi)) = loop_region else {
            return out;
        };
        let left = self.project_beat_to_x(lo).max(0.0);
        let right = self.project_beat_to_x(hi);
        if right <= left {
            return out;
        }
        out.push(
            div()
                .absolute()
                .top_0()
                .left(px(left))
                .w(px(right - left))
                .h(px(3.0))
                .bg(Colors::with_alpha(Colors::accent_primary(), 0.6))
                .into_any_element(),
        );
        out
    }

    pub(super) fn build_grid_lines(
        &self,
        start_beat: f32,
        end_beat: f32,
        view_w: f32,
        first_pitch: i32,
        last_pitch: i32,
        clip_len: f32,
    ) -> Vec<gpui::AnyElement> {
        let row_h = self.note_row_h();
        let scale = self.pitch_ctx.scale;
        let scale_active = scale.kind != ScaleKind::Chromatic;

        // ── Pitch row backgrounds: shade black-key rows, highlight C / scale ──
        let out_of_scale = Colors::with_alpha(Colors::surface_canvas(), 0.55);
        let root_row = Colors::with_alpha(Colors::accent_primary(), 0.08);
        let black_row = Colors::with_alpha(Colors::surface_base(), 0.45);
        let c_row = Colors::with_alpha(Colors::text_primary(), 0.03);
        let mut rows = Vec::new();
        let mut row_lines = Vec::new();
        if self.drum_mode {
            // Lanes alternate so a hit reads along its row across the bar; a
            // hairline under each. No key colours or scale: lanes are sounds.
            let band = Colors::with_alpha(Colors::surface_base(), 0.35);
            let line = Colors::with_alpha(Colors::text_primary(), 0.06);
            let (_, view_h) = self.grid_view_size();
            for index in self.visible_drum_rows(view_h) {
                let y = index as f32 * row_h - self.scroll_y;
                if index % 2 == 1 {
                    rows.push((y, y + row_h, band));
                }
                row_lines.push((y + row_h, line));
            }
        }
        // Drum mode draws its rows above; no pitch rows then.
        let pitch_range = (!self.drum_mode).then_some(first_pitch..=last_pitch);
        for p in pitch_range.into_iter().flatten() {
            let pitch = p as u8;
            let y = self.pitch_to_y(pitch);
            let in_scale = !scale_active || scale.contains_pitch(pitch);
            let is_root = scale_active && pitch % 12 == scale.root.pitch_class();
            let fill = if !in_scale {
                Some(out_of_scale)
            } else if is_root {
                Some(root_row)
            } else if is_black(p) {
                Some(black_row)
            } else if p % 12 == 0 {
                // C row — a touch brighter so octaves are easy to scan.
                Some(c_row)
            } else {
                None
            };
            if let Some(color) = fill {
                rows.push((y, y + row_h, color));
            }
            // Row separators along each row's bottom edge, where the key lane
            // draws its key borders: C strongest (B|C, the octave boundary),
            // F medium (E|F, the other white/white seam), every other a
            // hairline. On the top edge the "octave" line fell between C and
            // C♯, one row off from the keys beside it.
            let alpha = match p.rem_euclid(12) {
                0 => 0.14,
                5 => 0.07,
                _ => 0.035,
            };
            row_lines.push((y + row_h, Colors::with_alpha(Colors::text_primary(), alpha)));
        }

        // ── Vertical timing lines (zoom- and meter-aware hierarchy) ──
        // Across the whole visible range: the editor shows every clip on the
        // track, and the neighbours need the same bars under them.
        let mut columns: Vec<(f32, gpui::Rgba)> = self
            .visible_grid_lines(start_beat, end_beat)
            .into_iter()
            .filter(|(x, _)| *x >= -1.0 && *x <= view_w + 1.0)
            .map(|(x, kind)| (x, kind.color()))
            .collect();

        // Clip end marker. `clip_len` is the edited clip's own length, so it
        // converts through the clip frame.
        let end_x = self.clip_beat_to_x(clip_len);
        if end_x >= 0.0 && end_x <= view_w {
            columns.push((end_x, Colors::with_alpha(Colors::accent_primary(), 0.4)));
        }

        vec![grid_render::render_note_grid(
            grid_render::NoteGridSnapshot {
                scale: self.window_scale,
                rows,
                row_lines,
                columns,
            },
        )]
    }

    /// Bar/beat ruler header labels, aligned to the note grid via `beat_to_x`.
    /// The ruler's labels and ticks.
    ///
    /// Mark positions come from [`PianoRollViewport::ruler_marks`], the single
    /// source of ruler geometry in the editor; only the styling is local.
    pub(super) fn build_ruler(&self, start_beat: f32, end_beat: f32) -> Vec<gpui::AnyElement> {
        use crate::theme::{space, typography};
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        let scale = self.window_scale;
        let tick_w = grid_render::hairline(scale);
        for mark in self
            .viewport()
            .ruler_marks_in(start_beat, end_beat, &self.meter)
        {
            let x = grid_render::snap(mark.x, scale);
            out.push(
                div()
                    .absolute()
                    .top(px(space::HAIR))
                    .left(px(x + space::TIGHT))
                    .text_size(px(if mark.on_bar {
                        typography::DENSE_LABEL
                    } else {
                        typography::DENSE_CAPTION
                    }))
                    .font_weight(if mark.on_bar {
                        gpui::FontWeight::MEDIUM
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if mark.on_bar {
                        Colors::text_secondary()
                    } else {
                        Colors::text_faint()
                    })
                    .child(mark.label)
                    .into_any_element(),
            );
            // A bar reads as a full-height rule, a beat as a short tick.
            out.push(
                div()
                    .absolute()
                    .left(px(x))
                    .bottom_0()
                    .w(px(tick_w))
                    .h(px(if mark.on_bar { RULER_H } else { 5.0 }))
                    .bg(if mark.on_bar {
                        GridLineKind::Bar.color()
                    } else {
                        GridLineKind::Beat.color()
                    })
                    .into_any_element(),
            );
        }
        out
    }

    /// The clips on the track as bands along the ruler's foot: the edited one
    /// in its track's colour, its neighbours quiet — where the part is, at a
    /// glance, even when its notes are scrolled out of view.
    pub(super) fn build_ruler_clip_band(
        &self,
        track_color: gpui::Rgba,
        view_w: f32,
    ) -> Vec<gpui::AnyElement> {
        const BAND_H: f32 = 3.0;
        self.scope
            .spans()
            .iter()
            .filter_map(|span| {
                let x0 = self.project_beat_to_x(span.start_beat).max(0.0);
                let x1 = self.project_beat_to_x(span.end_beat()).min(view_w);
                if x1 <= x0 {
                    return None;
                }
                Some(
                    div()
                        .absolute()
                        .left(px(x0))
                        .bottom_0()
                        .w(px(x1 - x0))
                        .h(px(BAND_H))
                        .bg(if span.editable {
                            track_color
                        } else {
                            Colors::with_alpha(Colors::text_faint(), 0.35)
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// Bar/beat vertical lines through the lane under the grid (velocity,
    /// controller, articulation), from the same lines as the grid itself —
    /// project beats across the whole visible width, so a lane's bar line
    /// continues the grid's. Subdivisions are left out to keep a lane quiet.
    pub(super) fn build_velocity_grid(&self) -> Vec<gpui::AnyElement> {
        let (view_w, _) = self.grid_view_size();
        let start = self.x_to_project_beat(0.0);
        let end = self.x_to_project_beat(view_w);
        let columns = self
            .visible_grid_lines(start, end)
            .into_iter()
            .filter(|(x, kind)| {
                *kind != GridLineKind::Subdivision && *x >= -1.0 && *x <= view_w + 1.0
            })
            .map(|(x, kind)| (x, kind.color()))
            .collect();
        vec![grid_render::render_lane_grid(self.window_scale, columns)]
    }

    /// Ghost outlines showing where the affected notes would land after a
    /// quantize. Empty unless the Quantize button is hovered. Mirrors
    /// [`Self::quantize_selection`]'s target set: the selection, or every note
    /// when nothing is selected. Notes already on the grid are skipped.
    pub(super) fn build_quantize_preview(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
    ) -> Vec<gpui::AnyElement> {
        if !self.quantize_preview {
            return Vec::new();
        }
        let (view_w, view_h) = self.grid_view_size();
        let accent = Colors::accent_primary();
        let row_h = self.note_row_h();
        // The preview runs the very transform the click will: same targets,
        // same grid, strength and swing.
        let ids: HashSet<u64> = self.tool_target_ids(cx).into_iter().collect();
        let mut notes: Vec<MidiNoteState> = {
            let tl = self.timeline.read(cx);
            let Some(notes) = tl.state.midi_clip_notes(clip_id) else {
                return Vec::new();
            };
            notes
                .iter()
                .filter(|n| ids.contains(&n.id))
                .cloned()
                .collect()
        };
        let before: Vec<f32> = notes.iter().map(|n| n.start).collect();
        super::tools_panel::quantize_in(
            &mut notes,
            &ids,
            self.quantize_spec(),
            crate::components::timeline::timeline_state::QuantizeTarget::Start,
            self.quantize_bars(),
        );
        notes
            .iter()
            .zip(before)
            .filter_map(|(n, old_start)| {
                let q_start = n.start;
                if (q_start - old_start).abs() < 1.0e-4 {
                    return None;
                }
                let x = self.clip_beat_to_x(q_start);
                let w = (n.duration * self.ppb).max(3.0);
                let y = self.pitch_to_y(n.pitch);
                if x + w < 0.0 || x > view_w || y + row_h < 0.0 || y > view_h {
                    return None;
                }
                Some(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y + 1.0))
                        .w(px(w))
                        .h(px(row_h - 2.0))
                        .rounded(px(crate::theme::radius::MICRO))
                        .border(px(1.0))
                        .border_color(Colors::with_alpha(accent, 0.9))
                        .bg(Colors::with_alpha(accent, 0.12))
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// The neighbouring clips' notes, painted flat.
    ///
    /// One canvas for all of them rather than an element per note: they carry
    /// no listeners, no selection and no drag state, so there is nothing an
    /// element would give them. A busy track's neighbours can be thousands of
    /// notes, and this is the difference between showing the song and paying
    /// for it every frame.
    ///
    /// Deliberately not editable. Routing a drag to whichever clip is under the
    /// pointer is the next step, and drawing them as if they could be dragged
    /// before that is true would be the lie.
    pub(super) fn build_context_notes(
        &self,
        cx: &Context<Self>,
        view_w: f32,
        view_h: f32,
    ) -> Option<gpui::AnyElement> {
        let row_h = self.note_row_h();
        if row_h <= 0.0 || view_w <= 0.0 || view_h <= 0.0 {
            return None;
        }

        // Resolve to plain geometry here, while the timeline is open, so the
        // paint closure owns everything it needs and borrows nothing.
        let mut quads: Vec<(f32, f32, f32)> = Vec::new();
        {
            let tl = self.timeline.read(cx);
            for span in self.scope.spans() {
                if span.editable {
                    continue;
                }
                let Some(notes) = tl.state.midi_clip_notes(&span.clip_id) else {
                    continue;
                };
                for note in notes {
                    // A note is stored against its own clip, so it is placed by
                    // that clip's origin — not the edited one's.
                    let x0 = self.project_beat_to_x(span.to_project(note.start));
                    let x1 = self
                        .project_beat_to_x(span.to_project(note.start + note.duration.max(0.0)));
                    if x1 < 0.0 || x0 > view_w {
                        continue;
                    }
                    let y = self.pitch_to_y(note.pitch);
                    if y + row_h < 0.0 || y > view_h {
                        continue;
                    }
                    quads.push((x0.max(0.0), (x1 - x0).max(1.0), y));
                }
            }
        }
        if quads.is_empty() {
            return None;
        }

        // Dim enough to read as background, solid enough to see the shape of
        // the part. The track colour would compete with the edited notes; this
        // is deliberately colourless.
        let fill_color = Colors::with_alpha(Colors::text_muted(), 0.30);
        let height = (row_h - 1.0).max(1.0);
        Some(
            gpui::canvas(
                |_bounds, _window, _cx| (),
                move |bounds: gpui::Bounds<gpui::Pixels>, (), window, _cx| {
                    let ox: f32 = bounds.origin.x.into();
                    let oy: f32 = bounds.origin.y.into();
                    window.paint_layer(bounds, |window| {
                        for (x, w, y) in &quads {
                            let rect = gpui::Bounds {
                                origin: gpui::point(px(ox + x), px(oy + y)),
                                size: gpui::size(px(*w), px(height)),
                            };
                            window.paint_quad(gpui::fill(rect, fill_color));
                        }
                    });
                },
            )
            .absolute()
            .inset_0()
            .into_any_element(),
        )
    }

    /// One drum hit: a diamond centred on its start in its lane, filled by
    /// velocity like a note, with the same selection, erase and mute states.
    /// No length, label or resize edge — a hit is a moment.
    fn drum_hit_element(
        cx: &mut Context<Self>,
        hit: DrumHit,
        track_color: gpui::Rgba,
    ) -> gpui::AnyElement {
        let DrumHit {
            id,
            pitch,
            start,
            x,
            y,
            row_h,
            velocity,
            selected,
            erase_target,
            muted,
            name,
        } = hit;
        let mut fill = track_color;
        fill.a = if erase_target {
            0.45
        } else if muted {
            0.14
        } else if selected {
            1.0
        } else {
            0.42 + 0.5 * (velocity as f32 / 127.0)
        };
        let border = if erase_target {
            Colors::status_error()
        } else if selected {
            Colors::accent_primary()
        } else if muted {
            Colors::with_alpha(Colors::text_muted(), 0.6)
        } else {
            Colors::composite(
                track_color,
                Colors::with_alpha(Colors::surface_canvas(), 0.45),
            )
        };
        let border_w = if selected { 1.5 } else { 1.0 };
        let size = (row_h - 4.0).clamp(6.0, 18.0);
        let left = x - size * 0.5;
        let top = y + (row_h - size) * 0.5;
        div()
            .id(("pr-note", id as usize))
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(size))
            .h(px(size))
            .cursor(gpui::CursorStyle::PointingHand)
            .child(
                canvas(
                    |_bounds, _window, _cx| (),
                    move |bounds: gpui::Bounds<gpui::Pixels>, (), window, _cx| {
                        let ox: f32 = bounds.origin.x.into();
                        let oy: f32 = bounds.origin.y.into();
                        let s: f32 = bounds.size.width.into();
                        let half = s * 0.5;
                        let diamond = |inset: f32| {
                            [
                                point(px(ox + half), px(oy + inset)),
                                point(px(ox + s - inset), px(oy + half)),
                                point(px(ox + half), px(oy + s - inset)),
                                point(px(ox + inset), px(oy + half)),
                            ]
                        };
                        // The edge first, the fill inset by its width over it.
                        let mut edge = PathBuilder::fill();
                        let outer = diamond(0.0);
                        edge.move_to(outer[0]);
                        for corner in &outer[1..] {
                            edge.line_to(*corner);
                        }
                        edge.close();
                        if let Ok(path) = edge.build() {
                            window.paint_path(path, border);
                        }
                        let mut body = PathBuilder::fill();
                        let inner = diamond(border_w * std::f32::consts::SQRT_2);
                        body.move_to(inner[0]);
                        for corner in &inner[1..] {
                            body.line_to(*corner);
                        }
                        body.close();
                        if let Ok(path) = body.build() {
                            // Over the canvas, so a quiet hit still reads hollow
                            // rather than taking the edge's colour through it.
                            window.paint_path(
                                path,
                                Colors::composite(Colors::surface_canvas(), fill),
                            );
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                this.hover_note_status = hovered.then(|| {
                    format!(
                        "{} ({}) · at {:.2} · vel {}{}",
                        name,
                        note_name(pitch as i32),
                        start,
                        velocity,
                        if muted { " · muted" } else { "" }
                    )
                });
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.note_mouse_down(id, ev, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let (lx, ly) = this.grid_local(ev.position).unwrap_or((0.0, 0.0));
                    this.note_right_down(id, lx, ly, window, cx);
                }),
            )
            .into_any_element()
    }

    /// The step view's grid: every step of every visible lane as a cell,
    /// filled where the clip plays one. Painted in one canvas — a 64-step
    /// pattern over a kit is a few thousand cells, too many for an element
    /// each. Presses are handled by the grid (`begin_step_gesture`).
    ///
    /// What it shows, and why:
    /// - every other beat's columns sit on a faint band, so beats 1–4 read at
    ///   a glance without counting lines;
    /// - a single hit fills its cell, brighter the louder it is;
    /// - a ratchet shows one bar per hit, each as tall as that hit is loud, so
    ///   a roll's shape is visible in the cell;
    /// - a nudged step is drawn where it plays, shifted inside its column;
    /// - an uneven roll is amber, the one thing "Snap to grid" fixes;
    /// - the step under the pointer is outlined, and its lane is lit across
    ///   the grid by the drum hover band beneath.
    pub(super) fn build_step_cells(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
        track_color: gpui::Rgba,
        clip_len: f32,
    ) -> Vec<gpui::AnyElement> {
        let (view_w, view_h) = self.grid_view_size();
        let row_h = self.note_row_h();
        let step = self.step_view_beats();
        let steps = step_grid::step_count(clip_len, step);
        let step_w = (self.clip_beat_to_x(step) - self.clip_beat_to_x(0.0)).max(0.0);
        if steps == 0 || step_w < 1.0 {
            return Vec::new();
        }
        let px_per_beat = step_w / step;
        // A cell is inset so neighbours read as separate pads.
        let gap = (step_w * 0.12).clamp(1.0, 3.0);
        let notes: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(clip_id)
            .map(|notes| notes.to_vec())
            .unwrap_or_default();
        let first_step = ((self.x_to_clip_beat(0.0) / step).floor().max(0.0)) as usize;
        let last_step =
            (((self.x_to_clip_beat(view_w) / step).ceil().max(0.0)) as usize + 1).min(steps);
        let rows = self.visible_drum_rows(view_h);
        let rows_top = rows.start as f32 * row_h - self.scroll_y;
        let rows_bottom = rows.end as f32 * row_h - self.scroll_y;

        // Resolved here so the paint closure only walks a list (DESIGN.md:
        // no colour work in a per-frame paint loop).
        let empty = Colors::with_alpha(Colors::text_primary(), 0.05);
        let empty_hover = Colors::with_alpha(Colors::text_primary(), 0.14);
        let beat_band = Colors::with_alpha(Colors::text_primary(), 0.025);
        let outline = Colors::with_alpha(Colors::text_primary(), 0.7);
        let selected = Colors::accent_primary();
        let uneven = Colors::status_warning();
        let mut slab = track_color;
        slab.a = 0.22;
        let mut quads: Vec<StepQuad> = Vec::new();

        // Beat shading: the odd beats' columns, behind every lane.
        for index in first_step..last_step {
            let beat = (index as f32 * step + 1e-4).floor() as i64;
            if beat % 2 == 1 {
                let x = self.clip_beat_to_x(index as f32 * step);
                quads.push(StepQuad::fill(
                    x,
                    rows_top,
                    step_w,
                    rows_bottom - rows_top,
                    beat_band,
                    0.0,
                ));
            }
        }

        let targets = self.step_rows(cx);
        let rack = self.rack_active();
        for row in rows {
            let Some(target) = targets.get(row) else {
                continue;
            };
            let pitch = target.pitch;
            // In the rack every row is its own track: its own clip, drawn in
            // its own colour.
            let (track_color, slab) = match self.rack_rows.get(row).filter(|_| rack) {
                Some(rack_row) => {
                    let mut faint = rack_row.color;
                    faint.a = 0.22;
                    (rack_row.color, faint)
                }
                None => (track_color, slab),
            };
            // A muted rack row plays nothing: its steps read like muted notes.
            let row_muted = rack && self.rack_rows.get(row).is_some_and(|r| r.muted);
            let y = row as f32 * row_h - self.scroll_y + gap;
            let h = (row_h - 2.0 * gap).max(1.0);
            let cells = if rack {
                target
                    .clip_id
                    .as_deref()
                    .map(|id| step_grid::lane_cells(&self.clip_notes(id, cx), pitch, step))
                    .unwrap_or_default()
            } else {
                step_grid::lane_cells(&notes, pitch, step)
            };
            for index in first_step..last_step {
                let line_x = self.clip_beat_to_x(index as f32 * step) + gap * 0.5;
                let w = (step_w - gap).max(1.0);
                let hovered = self.step_hover == Some((row, index));
                let Some(cell) = cells.iter().find(|cell| cell.index == index) else {
                    quads.push(StepQuad::fill(
                        line_x,
                        y,
                        w,
                        h,
                        if hovered { empty_hover } else { empty },
                        2.0,
                    ));
                    continue;
                };
                let muted = cell.muted || row_muted;
                // Where the step plays: its line plus its nudge.
                let x = line_x + cell.offset * px_per_beat;
                if !cell.even {
                    let mut color = uneven;
                    color.a = if muted { 0.25 } else { 0.85 };
                    quads.push(StepQuad::fill(x, y, w, h, color, 2.0));
                } else if cell.repeat > 1 {
                    // One bar per hit, its height that hit's velocity, on a
                    // faint slab the size of the step.
                    quads.push(StepQuad::fill(x, y, w, h, slab, 2.0));
                    let slice = w / f32::from(cell.repeat);
                    let mut bar = track_color;
                    bar.a = if muted { 0.3 } else { 0.95 };
                    for (i, velocity) in cell.velocities.iter().enumerate() {
                        let bar_h = (h * f32::from(*velocity) / 127.0).max(2.0);
                        quads.push(StepQuad::fill(
                            x + slice * i as f32 + 0.5,
                            y + h - bar_h,
                            (slice - 1.0).max(1.0),
                            bar_h,
                            bar,
                            1.0,
                        ));
                    }
                } else {
                    // Velocity is the fill's strength; a muted step is faint.
                    let mut color = track_color;
                    color.a = if muted {
                        0.2
                    } else {
                        0.45 + 0.55 * (f32::from(cell.velocity) / 127.0)
                    };
                    quads.push(StepQuad::fill(x, y, w, h, color, 2.0));
                }
                // Selected (what the toolbar's count and curve buttons act
                // on) wins over hover: the accent is for selection alone.
                if cell.ids.iter().any(|id| self.selection.contains(id)) {
                    quads.push(StepQuad::outline(x, y, w, h, selected));
                } else if hovered {
                    quads.push(StepQuad::outline(x, y, w, h, outline));
                }
            }
        }
        let scale = self.window_scale;
        let this = cx.weak_entity();
        vec![gpui::canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                // Window-wide, not hover-scoped: see `on_step_pointer_move`.
                let moved = this.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        let _ = moved.update(cx, |this, cx| this.on_step_pointer_move(event, cx));
                    }
                });
                let released = this.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        let _ = released.update(cx, |this, cx| this.on_step_pointer_up(cx));
                    }
                });
                let origin = bounds.origin;
                for quad in &quads {
                    let rect = gpui::Bounds::new(
                        origin
                            + gpui::point(
                                px(grid_render::snap(quad.x, scale)),
                                px(grid_render::snap(quad.y, scale)),
                            ),
                        gpui::size(px(quad.w), px(quad.h)),
                    );
                    let paint = gpui::fill(rect, quad.fill).corner_radii(px(quad.radius));
                    window.paint_quad(match quad.border {
                        Some(border) => paint.border_widths(px(1.0)).border_color(border),
                        None => paint,
                    });
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()]
    }

    pub(super) fn build_note_elements(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
        track_color: gpui::Rgba,
    ) -> Vec<gpui::AnyElement> {
        let (view_w, view_h) = self.grid_view_size();
        let row_h = self.note_row_h();
        // Collect owned geometry first so the timeline read borrow is released
        // before we build per-note listeners (which borrow `cx` mutably).
        #[allow(clippy::type_complexity)]
        let geos: Vec<(
            u64,
            u8,
            f32,
            f32,
            f32,
            f32,
            f32,
            u8,
            bool,
            bool,
            bool,
            Option<&'static str>,
            Option<Vec<(f32, f32)>>,
        )> = {
            let tl = self.timeline.read(cx);
            let Some(notes) = tl.state.midi_clip_notes(clip_id) else {
                return Vec::new();
            };
            notes
                .iter()
                .filter(|n| self.channel_visible(n.channel))
                .filter_map(|n| {
                    let d = self.display_note(n);
                    let x = self.clip_beat_to_x(d.start);
                    let w = (d.duration * self.ppb).max(NOTE_MIN_W);
                    let y = self.pitch_to_y(d.pitch);
                    // Cull off-screen notes.
                    if x + w < 0.0 || x > view_w || y + row_h < 0.0 || y > view_h {
                        return None;
                    }
                    Some((
                        d.id,
                        d.pitch,
                        d.start,
                        d.duration,
                        x,
                        y,
                        w,
                        d.velocity,
                        self.selection.contains(&d.id),
                        self.erase_preview_ids.contains(&d.id),
                        n.muted,
                        n.articulation.map(|a| a.short_name()),
                        if self.selection.contains(&n.id) && n.expression.pitch.points.len() >= 2 {
                            Some(
                                n.expression
                                    .pitch
                                    .points
                                    .iter()
                                    .map(|point| {
                                        let x =
                                            (point.position.max(0.0) * self.ppb).min(w.max(0.0));
                                        let normalized = (point.value.clamp(-1.0, 1.0) + 1.0) * 0.5;
                                        let y = (row_h - 2.0) * (1.0 - normalized);
                                        (x, y)
                                    })
                                    .collect(),
                            )
                        } else {
                            None
                        },
                    ))
                })
                .collect()
        };

        let drum = self.drum_mode;
        let drum_names: HashMap<u8, SharedString> = if drum {
            self.drum_rows
                .iter()
                .map(|pitch| (*pitch, self.drum_lane_name(*pitch)))
                .collect()
        } else {
            HashMap::new()
        };

        geos.into_iter()
            .map(
                |(
                    id,
                    pitch,
                    start,
                    duration,
                    x,
                    y,
                    w,
                    velocity,
                    selected,
                    erase_target,
                    muted,
                    articulation,
                    inline_pitch,
                )| {
                    if drum {
                        let name = drum_names.get(&pitch).cloned().unwrap_or_default();
                        return Self::drum_hit_element(
                            cx,
                            DrumHit {
                                id,
                                pitch,
                                start,
                                x,
                                y,
                                row_h,
                                velocity,
                                selected,
                                erase_target,
                                muted,
                                name,
                            },
                            track_color,
                        );
                    }
                    // Velocity is the fill's strength: a quiet note reads
                    // quiet before its bar in the lane is found. Selection is
                    // said twice — full strength and the accent edge — and
                    // muted notes go hollow.
                    let mut fill = track_color;
                    fill.a = if erase_target {
                        0.45
                    } else if muted {
                        0.14
                    } else if selected {
                        1.0
                    } else {
                        0.42 + 0.5 * (velocity as f32 / 127.0)
                    };
                    let border = if erase_target {
                        Colors::status_error()
                    } else if selected {
                        Colors::accent_primary()
                    } else if muted {
                        Colors::with_alpha(Colors::text_muted(), 0.6)
                    } else {
                        Colors::composite(
                            track_color,
                            Colors::with_alpha(Colors::surface_canvas(), 0.45),
                        )
                    };
                    let note_h = row_h - 2.0;
                    let mut note = div()
                        .id(("pr-note", id as usize))
                        .absolute()
                        .left(px(x))
                        .top(px(y + 1.0))
                        .w(px(w))
                        .h(px(note_h))
                        // Data-sized: square below the radius' minimum side.
                        .rounded(px(crate::theme::radius::clamped(
                            crate::theme::radius::MICRO,
                            w,
                            note_h,
                        )))
                        .bg(fill)
                        .border(px(if selected { 1.5 } else { 1.0 }))
                        .border_color(border)
                        .cursor(gpui::CursorStyle::PointingHand)
                        .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                            this.hover_note_status = hovered.then(|| {
                                format!(
                                    "{} · start {:.2} · len {:.2} · vel {}{}",
                                    note_name(pitch as i32),
                                    start,
                                    duration,
                                    velocity,
                                    if muted { " · muted" } else { "" }
                                )
                            });
                            cx.notify();
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.note_mouse_down(id, ev, window, cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                let (lx, ly) = this.grid_local(ev.position).unwrap_or((0.0, 0.0));
                                this.note_right_down(id, lx, ly, window, cx);
                            }),
                        );
                    if let Some(points) = inline_pitch {
                        note = note.child(
                            canvas(
                                |_bounds, _window, _cx| {},
                                move |bounds, _scene, window, _cx| {
                                    if points.len() < 2 {
                                        return;
                                    }
                                    let origin = bounds.origin;
                                    let mut path =
                                        PathBuilder::stroke(px(1.1)).with_style(PathStyle::Stroke(
                                            StrokeOptions::default().with_miter_limit(2.0),
                                        ));
                                    path.move_to(origin + point(px(points[0].0), px(points[0].1)));
                                    for (x, y) in points.iter().copied().skip(1) {
                                        path.line_to(origin + point(px(x), px(y)));
                                    }
                                    if let Ok(path) = path.build() {
                                        window.paint_path(
                                            path,
                                            Colors::with_alpha(Colors::text_primary(), 0.82),
                                        );
                                    }
                                },
                            )
                            .absolute()
                            .inset_0(),
                        );
                    }
                    // Note-name label, shown only when the block is large enough to
                    // read so dense clips stay clean.
                    if w >= 24.0 && row_h >= 11.0 {
                        let label_color = if muted {
                            Colors::with_alpha(Colors::text_muted(), 0.8)
                        } else if selected {
                            Colors::text_primary()
                        } else {
                            Colors::with_alpha(Colors::text_primary(), 0.82)
                        };
                        note = note.child(
                            div()
                                .absolute()
                                .left(px(crate::theme::space::TIGHT))
                                .top_0()
                                .bottom_0()
                                .flex()
                                .items_center()
                                .text_size(px(if row_h >= 16.0 {
                                    crate::theme::typography::DENSE_CAPTION
                                } else {
                                    8.5
                                }))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(label_color)
                                .child(note_name(pitch as i32)),
                        );
                    }
                    // Per-note articulation badge, right-aligned on the block
                    // (clear of the left note-name label), only when wide
                    // enough to stay readable in dense clips.
                    if let Some(short) = articulation {
                        if w >= 46.0 && row_h >= 11.0 {
                            note = note.child(
                                div()
                                    .absolute()
                                    .right(px(RESIZE_ZONE + 2.0))
                                    .top_0()
                                    .bottom_0()
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .px(px(2.0))
                                            .rounded(px(crate::theme::radius::MICRO))
                                            .bg(Colors::with_alpha(Colors::accent_primary(), 0.85))
                                            .text_size(px(7.0))
                                            .text_color(Colors::text_primary())
                                            .child(short),
                                    ),
                            );
                        }
                    }
                    // Right-edge resize handle (only when the note is wide enough to
                    // leave room for a separate move/resize zone).
                    if w >= NOTE_RESIZE_MIN_W {
                        note = note.child(
                            div()
                                .id(("pr-note-edge", id as usize))
                                .absolute()
                                .right_0()
                                .top_0()
                                .w(px(RESIZE_ZONE))
                                .h_full()
                                .cursor(gpui::CursorStyle::ResizeLeftRight)
                                .hover(|s| s.bg(Colors::with_alpha(Colors::text_primary(), 0.16)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.begin_resize_drag(id, ev, window, cx);
                                    }),
                                ),
                        );
                    }
                    note.into_any_element()
                },
            )
            .collect()
    }

    /// Quiet level guides across the velocity lane, at the values the lane
    /// header labels.
    pub(super) fn build_velocity_guides(&self) -> Vec<gpui::AnyElement> {
        let (_, lane_h) = self.cc_view_size();
        let usable = (lane_h - 8.0).max(1.0);
        [96u8, 64, 32]
            .into_iter()
            .map(|value| {
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(velocity_lane_y(value, usable).round()))
                    .h(px(1.0))
                    .bg(Colors::with_alpha(
                        Colors::text_primary(),
                        if value == 64 { 0.06 } else { 0.035 },
                    ))
                    .into_any_element()
            })
            .collect()
    }

    /// One stem per note, from the lane's floor to its velocity, with a head
    /// on top. The head sits exactly where a press sets that value
    /// (`velocity_from_local_y`), so what is drawn is what a drag grabs.
    pub(super) fn build_velocity_bars(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
        track_color: gpui::Rgba,
    ) -> Vec<gpui::AnyElement> {
        const HIT_W: f32 = 8.0;
        const STEM_W: f32 = 2.0;
        const HEAD: f32 = 6.0;
        let (view_w, lane_h) = self.cc_view_size();
        let usable = (lane_h - 8.0).max(1.0);
        let floor = velocity_lane_y(1, usable);
        let geos: Vec<(u64, u8, f32, bool, bool)> = {
            let tl = self.timeline.read(cx);
            let Some(notes) = tl.state.midi_clip_notes(clip_id) else {
                return Vec::new();
            };
            notes
                .iter()
                .filter(|n| self.channel_visible(n.channel))
                .filter_map(|n| {
                    let d = self.display_note(n);
                    let x = self.clip_beat_to_x(d.start);
                    if x < -HIT_W || x > view_w {
                        return None;
                    }
                    Some((d.id, d.velocity, x, self.selection.contains(&d.id), n.muted))
                })
                .collect()
        };
        let stem_rest = Colors::with_alpha(track_color, 0.55);
        let muted_color = Colors::with_alpha(Colors::text_muted(), 0.45);
        let selected_color = Colors::accent_primary();
        // The step under the pointer in the step view: its stalks are lit and
        // drawn last, so the one that belongs to it is found among the other
        // drums hitting at the same time.
        let hovered_ids = self.hovered_step_ids(cx);
        let mut geos = geos;
        geos.sort_by_key(|(id, ..)| hovered_ids.contains(id));

        geos.into_iter()
            .map(|(id, vel, x, selected, muted)| {
                let head_y = velocity_lane_y(vel, usable);
                let hovered = hovered_ids.contains(&id);
                let (stem, head) = if selected {
                    (selected_color, selected_color)
                } else if muted {
                    (muted_color, muted_color)
                } else if hovered {
                    (Colors::text_primary(), track_color)
                } else {
                    (stem_rest, track_color)
                };
                // A full-height transparent column, so even a low velocity is
                // easy to grab.
                div()
                    .id(("pr-vel", id as usize))
                    .absolute()
                    .left(px(x - (HIT_W - STEM_W) * 0.5))
                    .top_0()
                    .bottom_0()
                    .w(px(HIT_W))
                    .cursor(gpui::CursorStyle::ResizeUpDown)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.begin_velocity_drag(id, vel, ev, window, cx);
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px((HIT_W - STEM_W) * 0.5))
                            .top(px(head_y))
                            .w(px(STEM_W))
                            .h(px((floor - head_y).max(1.0)))
                            .bg(stem),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px((HIT_W - HEAD) * 0.5))
                            .top(px(head_y - HEAD * 0.5))
                            .size(px(HEAD))
                            .rounded(px(crate::theme::radius::PILL))
                            .bg(head)
                            .when(selected || hovered, |dot| {
                                dot.border(px(1.0)).border_color(Colors::text_primary())
                            }),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

/// What [PianoRoll::drum_hit_element] draws one hit from.
struct DrumHit {
    id: u64,
    pitch: u8,
    start: f32,
    x: f32,
    y: f32,
    row_h: f32,
    velocity: u8,
    selected: bool,
    erase_target: bool,
    muted: bool,
    name: SharedString,
}

/// One rectangle of the step grid, in grid-local logical pixels.
struct StepQuad {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fill: gpui::Rgba,
    border: Option<gpui::Rgba>,
    radius: f32,
}

impl StepQuad {
    fn fill(x: f32, y: f32, w: f32, h: f32, fill: gpui::Rgba, radius: f32) -> Self {
        Self {
            x,
            y,
            w,
            h,
            fill,
            border: None,
            radius,
        }
    }

    /// The hovered step's outline: a ring, so the fill under it still reads.
    fn outline(x: f32, y: f32, w: f32, h: f32, border: gpui::Rgba) -> Self {
        Self {
            x,
            y,
            w,
            h,
            fill: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
            border: Some(border),
            radius: 2.0,
        }
    }
}
