//! Articulation lane for the piano roll's unified bottom lane.
//!
//! Shows the clip's **direction** articulation events as regions along the
//! timeline (each event runs until the next one) and lets the user insert,
//! select, move, and delete events. Per-note articulation badges render on the
//! note blocks themselves (see `render.rs`). All edits commit through the
//! `SetMidiArticulations` undo command as full prev/next lane snapshots,
//! mirroring the CC lane's gesture model. Live drags mutate silently (no
//! engine dirty per mouse-move); the commit on release marks the project
//! dirty exactly once.

use super::*;

/// Marker hit radius around an event boundary, in px.
const ART_MARKER_HIT_PX: f32 = 6.0;
/// Height of the insert-palette chip row at the top of the lane.
const ART_PALETTE_H: f32 = 22.0;

impl PianoRoll {
    /// Insert the palette articulation as a direction event at `beat`
    /// (clip-local, already snapped by the caller) — one undoable command.
    pub(super) fn insert_articulation_at(
        &mut self,
        clip_id: &str,
        beat: f32,
        cx: &mut Context<Self>,
    ) {
        let articulation = self.insert_articulation;
        let prev = self.timeline.read(cx).state.articulations_snapshot(clip_id);
        let new_id = self.timeline.update(cx, |tl, tcx| {
            let id = tl.state.add_midi_articulation(clip_id, beat, articulation);
            tcx.notify();
            id
        });
        let next = self.timeline.read(cx).state.articulations_snapshot(clip_id);
        if prev == next {
            return;
        }
        // Record the already-applied edit; execute() is a no-op re-apply.
        self.run_edit_command(
            EditCommand::SetMidiArticulations {
                clip_id: clip_id.to_string(),
                prev,
                next,
            },
            cx,
        );
        self.selected_articulation = new_id;
        cx.notify();
    }

    /// The direction event whose boundary marker sits within
    /// [`ART_MARKER_HIT_PX`] of lane-local `lx`. Closest wins on overlap.
    pub(super) fn articulation_at(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
        lx: f32,
    ) -> Option<u64> {
        let tl = self.timeline.read(cx);
        let events = tl.state.midi_clip_articulations(clip_id)?;
        events
            .iter()
            .filter_map(|e| {
                let dx = (self.clip_beat_to_x(e.beat) - lx).abs();
                (dx <= ART_MARKER_HIT_PX).then_some((e.id, dx))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }

    /// Begin dragging an event boundary: select it and snapshot the lane for
    /// the one undo entry recorded on release.
    pub(super) fn begin_articulation_move(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        self.art_edit_prev = Some(
            self.timeline
                .read(cx)
                .state
                .articulations_snapshot(&clip_id),
        );
        self.selected_articulation = Some(id);
        self.drag = PianoDrag::ArtMove { id };
        cx.notify();
    }

    /// Live-move the dragged event to the cursor beat (snapped). Silent —
    /// repaints without dirtying the engine; the release commit marks dirty.
    pub(super) fn articulation_move_to(&mut self, id: u64, lx: f32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let beat = self.snap_beats(self.x_to_clip_beat(lx)).max(0.0);
        let label = self
            .timeline
            .read(cx)
            .state
            .midi_clip_articulations(&clip_id)
            .and_then(|events| events.iter().find(|e| e.id == id))
            .map(|e| e.articulation.name())
            .unwrap_or("Articulation");
        self.drag_value_status = Some(format!("{label} @ {beat:.2}"));
        self.with_timeline_silent(cx, |tl, _| {
            tl.state.move_midi_articulation(&clip_id, id, beat);
        });
        cx.notify();
    }

    /// Commit a finished articulation gesture as one undoable command
    /// (skips no-ops).
    pub(super) fn commit_articulation_edit(&mut self, cx: &mut Context<Self>) {
        let Some(prev) = self.art_edit_prev.take() else {
            return;
        };
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let next = self
            .timeline
            .read(cx)
            .state
            .articulations_snapshot(&clip_id);
        if prev == next {
            return;
        }
        self.run_edit_command(
            EditCommand::SetMidiArticulations {
                clip_id,
                prev,
                next,
            },
            cx,
        );
    }

    /// Delete one direction event by id (lane right-click) — one undoable
    /// command.
    pub(super) fn delete_articulation(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.selected_articulation == Some(id) {
            self.selected_articulation = None;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let prev = self
            .timeline
            .read(cx)
            .state
            .articulations_snapshot(&clip_id);
        let next: Vec<MidiArticulationEvent> =
            prev.iter().filter(|e| e.id != id).cloned().collect();
        if prev.len() == next.len() {
            return;
        }
        self.run_edit_command(
            EditCommand::SetMidiArticulations {
                clip_id,
                prev,
                next,
            },
            cx,
        );
        cx.notify();
    }

    /// The articulation lane body: direction regions with labels + boundary
    /// markers, an insert palette, and mouse handlers. Uses the shared
    /// `cc_bounds` capture so lane-local coordinates and the grid's beat
    /// mapping stay identical to the other lanes.
    pub(super) fn render_articulation_lane(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
        start_beat: f32,
        end_beat: f32,
        bpb: f32,
    ) -> impl IntoElement {
        let (view_w, _) = self.cc_view_size();
        let grid = self.build_velocity_grid(start_beat, end_beat, bpb);
        let (_, clip_len) = self.clip_meta(cx, clip_id);

        // Owned copy of the visible events (culled below) so the timeline read
        // borrow is released before building listeners.
        let events: Vec<MidiArticulationEvent> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_articulations(clip_id)
            .cloned()
            .unwrap_or_default();
        let selected = self.selected_articulation;

        let mut children: Vec<gpui::AnyElement> = Vec::new();
        for (i, event) in events.iter().enumerate() {
            let x = self.clip_beat_to_x(event.beat);
            let end = events
                .get(i + 1)
                .map(|next| self.clip_beat_to_x(next.beat))
                .unwrap_or_else(|| self.clip_beat_to_x(clip_len));
            // Cull regions fully outside the visible lane.
            if end < 0.0 || x > view_w {
                continue;
            }
            let is_selected = selected == Some(event.id);
            let region_w = (end - x).max(2.0);
            let accent = Colors::accent_primary();
            // Region body: tinted span from this event to the next.
            children.push(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(ART_PALETTE_H))
                    .bottom_0()
                    .w(px(region_w))
                    .bg(Colors::with_alpha(
                        accent,
                        if is_selected { 0.16 } else { 0.08 },
                    ))
                    .border_l(px(2.0))
                    .border_color(if is_selected {
                        accent
                    } else {
                        Colors::with_alpha(accent, 0.55)
                    })
                    .into_any_element(),
            );
            // Label flag at the region start (clamped inside the region).
            if region_w >= 24.0 {
                children.push(
                    div()
                        .absolute()
                        .left(px(x + 4.0))
                        .top(px(ART_PALETTE_H + 4.0))
                        .px(px(4.0))
                        .h(px(14.0))
                        .max_w(px(region_w - 8.0))
                        .flex()
                        .items_center()
                        .rounded(px(crate::theme::radius::MICRO))
                        .bg(Colors::with_alpha(
                            accent,
                            if is_selected { 0.9 } else { 0.55 },
                        ))
                        .text_size(px(9.0))
                        .text_color(Colors::text_primary())
                        .overflow_hidden()
                        .child(event.articulation.short_name())
                        .into_any_element(),
                );
            }
        }

        // Insert palette: one compact chip per built-in articulation; the
        // active chip is what a lane click inserts.
        let palette = div()
            .absolute()
            .left_0()
            .top_0()
            .right_0()
            .h(px(ART_PALETTE_H))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .px(px(4.0))
            .border_b(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            .child(
                div()
                    .text_size(px(8.0))
                    .text_color(Colors::text_faint())
                    .child("Insert:"),
            )
            // Capability-filtered like the inspector row and the note menu: a
            // Wind instrument must not be able to insert Marcato here when no
            // other surface will show or clear it.
            .children(
                self.available_articulations(cx)
                    .into_iter()
                    .map(|articulation| {
                        let active = self.insert_articulation == articulation;
                        div()
                            .id(("pr-art-palette", articulation.to_tag() as usize))
                            .h(px(16.0))
                            .px(px(5.0))
                            .flex()
                            .items_center()
                            .rounded(px(crate::theme::radius::MICRO))
                            .text_size(px(9.0))
                            .text_color(if active {
                                Colors::text_primary()
                            } else {
                                Colors::text_secondary()
                            })
                            .bg(if active {
                                Colors::with_alpha(Colors::accent_primary(), 0.35)
                            } else {
                                Colors::with_alpha(Colors::text_primary(), 0.0)
                            })
                            .border(px(1.0))
                            .border_color(if active {
                                Colors::accent_primary()
                            } else {
                                Colors::border_subtle()
                            })
                            .hover(|s| s.bg(Colors::surface_hover()))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_mouse_down(MouseButton::Left, |_, _w, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                cx.stop_propagation();
                                this.insert_articulation = articulation;
                                cx.notify();
                            }))
                            .child(articulation.short_name())
                            .into_any_element()
                    }),
            );

        let empty_state = events.is_empty().then(|| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(ART_PALETTE_H))
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .text_color(Colors::text_faint())
                .child("Click to insert a direction articulation · drag markers to move · right-click to delete")
        });
        let value_chip_el = matches!(self.drag, PianoDrag::ArtMove { .. }).then(|| {
            value_chip(
                self.drag_value_status.as_deref().unwrap_or("Articulation"),
                8.0,
                ART_PALETTE_H + 6.0,
            )
        });

        let cc_bounds = self.cc_bounds.clone();
        let bounds_canvas = canvas(
            move |bounds, _w, _cx| cc_bounds.set(Some(bounds)),
            |_b, _r, _w, _cx| {},
        )
        .absolute()
        .inset_0();

        div()
            .id("piano-articulations")
            .h(px(LANE_H))
            .w_full()
            .relative()
            .overflow_hidden()
            .border_t(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel_alt())
            .cursor(gpui::CursorStyle::PointingHand)
            .child(bounds_canvas)
            .children(grid)
            .children(children)
            .child(palette)
            .children(empty_state)
            .children(value_chip_el)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some((lx, ly)) = this.cc_local(ev.position) else {
                        return;
                    };
                    if ly <= ART_PALETTE_H {
                        return; // palette row handles its own clicks
                    }
                    // The lane spans the track, so a press in a neighbouring
                    // clip moves the editor there before anything is written.
                    if this.retarget_to_clip_under(lx, cx) {
                        return;
                    }
                    let Some(clip_id) = this.editing_clip_id(cx) else {
                        return;
                    };
                    if let Some(id) = this.articulation_at(cx, &clip_id, lx) {
                        this.begin_articulation_move(id, window, cx);
                        return;
                    }
                    window.focus(&this.focus, cx);
                    let beat = this.snap_beats(this.x_to_clip_beat(lx)).max(0.0);
                    this.insert_articulation_at(&clip_id, beat, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    let Some((lx, ly)) = this.cc_local(ev.position) else {
                        return;
                    };
                    if ly <= ART_PALETTE_H {
                        return;
                    }
                    let Some(clip_id) = this.editing_clip_id(cx) else {
                        return;
                    };
                    if let Some(id) = this.articulation_at(cx, &clip_id, lx) {
                        this.delete_articulation(id, cx);
                    }
                }),
            )
    }
}
