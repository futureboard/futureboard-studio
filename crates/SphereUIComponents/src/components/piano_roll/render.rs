//! Split out of `piano_roll.rs` (god-file decomposition). These are
//! `impl PianoRoll` extension blocks; `use super::*` pulls in the shared
//! piano-roll vocabulary (struct fields via the type, consts, free fns).

use super::*;

impl PianoRoll {
    pub(super) fn display_note(&self, n: &MidiNoteState) -> DisplayNote {
        let mut start = n.start;
        let mut pitch = n.pitch;
        let mut duration = n.duration;
        match &self.drag {
            PianoDrag::Move {
                prev,
                dx_beats,
                dpitch,
                anchor_start,
                unsnap,
                ..
            } => {
                if prev.iter().any(|(id, _, _)| *id == n.id) {
                    // Snap only the grabbed anchor, then apply its delta to every
                    // peer so an off-grid multi-selection keeps internal spacing.
                    let snapped_anchor = self.snap_beats_live(*anchor_start + *dx_beats, *unsnap);
                    start = (n.start + snapped_anchor - *anchor_start).max(0.0);
                    let raw_pitch = (n.pitch as i32 + dpitch).clamp(0, 127) as u8;
                    pitch = self.pitch_ctx.constrain_pitch(raw_pitch);
                }
            }
            PianoDrag::Resize {
                prev_durs,
                delta_dur,
                ..
            } => {
                if let Some((_, prev_duration)) =
                    prev_durs.iter().find(|(note_id, _)| *note_id == n.id)
                {
                    duration = (*prev_duration + *delta_dur).max(MIN_NOTE_BEATS);
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
        let x = self.beat_to_x(note.start);
        let w = (note.duration * self.ppb).max(NOTE_MIN_W);
        let y = self.pitch_to_y(note.pitch) + 1.0;
        let h = self.note_row_h() - 2.0;
        (x, y, x + w, y + h)
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

    pub(super) fn build_draw_note_preview(&self) -> Option<gpui::AnyElement> {
        let PianoDrag::DrawNote {
            pitch,
            start_beat,
            end_beat,
            unsnap,
            ..
        } = &self.drag
        else {
            return None;
        };
        let (lo, hi) = normalize_range(*start_beat, *end_beat);
        let minimum = if self.snap_on && !self.grid_res.is_free() && !*unsnap {
            self.step_beats().max(MIN_NOTE_BEATS)
        } else {
            MIN_NOTE_BEATS
        };
        let duration = (hi - lo).max(minimum);
        let x = self.beat_to_x(lo);
        let w = (duration * self.ppb).max(3.0);
        let y = self.pitch_to_y(*pitch);
        let h = self.note_row_h() - 2.0;
        Some(
            div()
                .absolute()
                .left(px(x))
                .top(px(y + 1.0))
                .w(px(w))
                .h(px(h))
                .rounded(px(crate::theme::radius::MICRO))
                .bg(Colors::with_alpha(Colors::accent_primary(), 0.35))
                .border(px(1.0))
                .border_color(Colors::with_alpha(Colors::accent_primary(), 0.85))
                .into_any_element(),
        )
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
                let x0 = self.beat_to_x(*anchor_beat);
                let x1 = self.beat_to_x(*current_beat);
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

        let clip_id = self.editing_clip_id(cx);
        if clip_id != self.last_editing_clip && !matches!(self.drag, PianoDrag::None) {
            self.cancel_active_gesture(cx);
        }
        self.prune_transient_state(cx, clip_id.as_deref());

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
        }

        if let Some(cid) = clip_id.as_deref() {
            if self.fitted_clip_id.as_deref() != Some(cid) {
                self.fit_piano_roll_to_notes(cx, cid);
                self.fitted_clip_id = Some(cid.to_string());
            }
        }

        // Toolbar is always shown; the body shows a hint when no MIDI clip is
        // selected.
        let toolbar = self.render_toolbar(cx, clip_id.as_deref());

        let body: gpui::AnyElement = match clip_id {
            Some(cid) => self.render_body(cx, &cid).into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.0))
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
            .on_mouse_move(cx.listener(Self::on_move))
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
    }
}

impl PianoRoll {
    fn render_select_menu(
        &self,
        menu: PianoSelectMenu,
        id: &'static str,
        label: String,
        options: Vec<(String, bool, gpui::AnyElement)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.open_select_menu == Some(menu);
        let mut dropdown: Option<gpui::AnyElement> = None;
        if open {
            let mut panel = div()
                .absolute()
                .top(px(26.0))
                .left_0()
                .w(px(
                    if menu == PianoSelectMenu::Grid || menu == PianoSelectMenu::Channel {
                        160.0
                    } else {
                        148.0
                    },
                ))
                .max_h(px(280.0))
                .id(("pr-select-menu-scroll", menu as u32))
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
                .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation());

            for (i, (_text, selected, action)) in options.into_iter().enumerate() {
                panel = panel.child(
                    div()
                        .id((id, i))
                        .flex()
                        .items_center()
                        .h(px(20.0))
                        .px(px(7.0))
                        .rounded(px(crate::theme::radius::CONTROL_SM))
                        .text_size(px(10.0))
                        .text_color(if selected {
                            Colors::accent_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .hover(|s| s.bg(Colors::surface_hover()))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .child(action),
                );
            }
            dropdown = Some(
                deferred(
                    panel
                        .with_animation(
                            "pr-select-menu-open",
                            Animation::new(Duration::from_millis(90))
                                .with_easing(pulsating_between(0.9, 1.0)),
                            |this, delta| this.opacity(delta),
                        )
                        .into_any_element(),
                )
                .with_priority(PIANO_ROLL_MENU_PRIORITY)
                .into_any_element(),
            );
        }

        div()
            .relative()
            .flex()
            .items_center()
            .occlude()
            .child(
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .h(px(22.0))
                    .min_w(px(72.0))
                    .pl(px(7.0))
                    .pr(px(5.0))
                    .gap(px(6.0))
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .text_size(px(10.0))
                    .text_color(if open {
                        Colors::text_primary()
                    } else {
                        Colors::text_secondary()
                    })
                    .bg(if open {
                        Colors::surface_hover()
                    } else {
                        Colors::with_alpha(Colors::text_primary(), 0.0)
                    })
                    .border(px(1.0))
                    .border_color(if open {
                        Colors::border_subtle()
                    } else {
                        Colors::with_alpha(Colors::text_primary(), 0.0)
                    })
                    .hover(|s| s.bg(Colors::surface_hover()))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.open_select_menu = if this.open_select_menu == Some(menu) {
                            None
                        } else {
                            Some(menu)
                        };
                        cx.notify();
                    }))
                    .child(div().flex_1().truncate().child(label))
                    .child(
                        svg()
                            .path(assets::ICON_CHEVRON_DOWN_PATH)
                            .w(px(10.0))
                            .h(px(10.0))
                            .flex_shrink_0()
                            .text_color(if open {
                                Colors::text_secondary()
                            } else {
                                Colors::text_faint()
                            }),
                    ),
            )
            .when_some(dropdown, |root, panel| root.child(panel))
    }

    /// Compact selector for the single controller lane: a button showing the
    /// active lane that opens a dropdown of choices (Velocity / common CCs /
    /// pitch-bend / pressure / custom CC), plus a collapse toggle. Alt+wheel on
    /// the button cycles lanes. Replaces the old "Lane / +Lane / −Lane" trio —
    /// switching here only changes what the one lane shows, never the data.
    pub(super) fn render_lane_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current_lane();
        let label = format!("Lane: {}", self.lane_name());
        let open = self.open_select_menu == Some(PianoSelectMenu::Lane);
        let visible = self.unified_lane_visible(cx);
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

        let mut dropdown: Option<gpui::AnyElement> = None;
        if open {
            let mut panel = div()
                .absolute()
                .top(px(26.0))
                .left_0()
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
                .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation());
            for (i, kind) in LANE_CYCLE.iter().enumerate() {
                let kind = *kind;
                let selected = kind == current;
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
                panel = panel.child(
                    div()
                        .id(("pr-lane-opt", i))
                        .flex()
                        .items_center()
                        .justify_between()
                        .h(px(20.0))
                        .px(px(7.0))
                        .rounded(px(crate::theme::radius::CONTROL_SM))
                        .text_size(px(10.0))
                        .text_color(if selected {
                            Colors::accent_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .hover(|s| s.bg(Colors::surface_hover()))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            cx.stop_propagation();
                            this.set_lane(kind, cx);
                        }))
                        .child(text)
                        .when(lane_has_data, |row| {
                            row.child(
                                div()
                                    .size(px(4.0))
                                    .rounded(px(crate::theme::radius::MICRO))
                                    .bg(Colors::accent_primary())
                                    .flex_shrink_0(),
                            )
                        }),
                );
            }
            if !extra_kinds.is_empty() {
                panel = panel.child(
                    div()
                        .h(px(1.0))
                        .mt(px(2.0))
                        .mb(px(2.0))
                        .bg(Colors::divider()),
                );
                panel = panel.child(
                    div()
                        .px(px(7.0))
                        .text_size(px(9.0))
                        .text_color(Colors::text_faint())
                        .child("In This Clip"),
                );
                for (i, kind) in extra_kinds.iter().enumerate() {
                    let kind = *kind;
                    let lane_kind = ControllerLaneKind::Controller(kind);
                    let selected = lane_kind == current;
                    panel = panel.child(
                        div()
                            .id(("pr-lane-extra", i))
                            .flex()
                            .items_center()
                            .justify_between()
                            .h(px(20.0))
                            .px(px(7.0))
                            .rounded(px(crate::theme::radius::CONTROL_SM))
                            .text_size(px(10.0))
                            .text_color(if selected {
                                Colors::accent_primary()
                            } else {
                                Colors::text_secondary()
                            })
                            .hover(|s| s.bg(Colors::surface_hover()))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                cx.stop_propagation();
                                this.set_lane(lane_kind, cx);
                            }))
                            .child(cc_kind_label(kind))
                            .child(
                                div()
                                    .size(px(4.0))
                                    .rounded(px(crate::theme::radius::MICRO))
                                    .bg(Colors::accent_primary())
                                    .flex_shrink_0(),
                            ),
                    );
                }
            }
            // Custom CC row: − / CCnn (select) / + . Steppers keep the menu open.
            panel = panel.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(22.0))
                    .px(px(4.0))
                    .mt(px(2.0))
                    .border_t(px(1.0))
                    .border_color(Colors::divider())
                    .child(
                        div()
                            .id(("pr-lane-custom", 0usize))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.0))
                            .rounded(px(crate::theme::radius::MICRO))
                            .text_size(px(11.0))
                            .text_color(Colors::text_secondary())
                            .hover(|s| s.bg(Colors::surface_hover()))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                cx.stop_propagation();
                                this.custom_cc = this.custom_cc.saturating_sub(1);
                                cx.notify();
                            }))
                            .child("−"),
                    )
                    .child(
                        div()
                            .id(("pr-lane-custom", 1usize))
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(18.0))
                            .rounded(px(crate::theme::radius::MICRO))
                            .text_size(px(10.0))
                            .text_color(
                                if current
                                    == ControllerLaneKind::Controller(MidiControllerKind::CC(
                                        custom,
                                    ))
                                {
                                    Colors::accent_primary()
                                } else {
                                    Colors::text_primary()
                                },
                            )
                            .hover(|s| s.bg(Colors::surface_hover()))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                cx.stop_propagation();
                                this.set_lane(
                                    ControllerLaneKind::Controller(MidiControllerKind::CC(
                                        this.custom_cc,
                                    )),
                                    cx,
                                )
                            }))
                            .child(format!("Custom CC{custom}")),
                    )
                    .child(
                        div()
                            .id(("pr-lane-custom", 2usize))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.0))
                            .rounded(px(crate::theme::radius::MICRO))
                            .text_size(px(11.0))
                            .text_color(Colors::text_secondary())
                            .hover(|s| s.bg(Colors::surface_hover()))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                cx.stop_propagation();
                                this.custom_cc = (this.custom_cc + 1).min(127);
                                cx.notify();
                            }))
                            .child("+"),
                    ),
            );
            dropdown = Some(
                deferred(
                    panel
                        .with_animation(
                            "pr-lane-menu-open",
                            Animation::new(Duration::from_millis(90))
                                .with_easing(pulsating_between(0.92, 1.0)),
                            |this, delta| this.opacity(delta),
                        )
                        .into_any_element(),
                )
                .with_priority(PIANO_ROLL_MENU_PRIORITY)
                .into_any_element(),
            );
        }

        div()
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .occlude()
            .child(
                div()
                    .id("pr-lane-select")
                    .flex()
                    .items_center()
                    .h(px(22.0))
                    .min_w(px(122.0))
                    .pl(px(7.0))
                    .pr(px(5.0))
                    .gap(px(6.0))
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .text_size(px(10.0))
                    .text_color(if open {
                        Colors::text_primary()
                    } else {
                        Colors::text_secondary()
                    })
                    .bg(if open {
                        Colors::surface_hover()
                    } else {
                        Colors::with_alpha(Colors::text_primary(), 0.0)
                    })
                    .border(px(1.0))
                    .border_color(if open {
                        Colors::border_subtle()
                    } else {
                        Colors::with_alpha(Colors::text_primary(), 0.0)
                    })
                    .hover(|s| s.bg(Colors::surface_hover()))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.open_select_menu =
                            if this.open_select_menu == Some(PianoSelectMenu::Lane) {
                                None
                            } else {
                                Some(PianoSelectMenu::Lane)
                            };
                        cx.notify();
                    }))
                    // Alt + mouse wheel cycles lanes (Part 7 optional shortcut).
                    .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                        if !ev.modifiers.alt {
                            return;
                        }
                        let dy = match ev.delta {
                            gpui::ScrollDelta::Pixels(p) => f32::from(p.y),
                            gpui::ScrollDelta::Lines(p) => p.y,
                        };
                        if dy != 0.0 {
                            this.cycle_lane(if dy < 0.0 { 1 } else { -1 }, cx);
                        }
                    }))
                    .child(div().flex_1().truncate().child(label))
                    .child(
                        svg()
                            .path(assets::ICON_CHEVRON_DOWN_PATH)
                            .w(px(10.0))
                            .h(px(10.0))
                            .flex_shrink_0()
                            .text_color(if open {
                                Colors::text_secondary()
                            } else {
                                Colors::text_faint()
                            }),
                    ),
            )
            // Collapse / expand the whole lane.
            .child(tool_btn(
                "pr-lane-toggle",
                if visible { "▾" } else { "▸" },
                !visible,
                cx.listener(|this, _ev, _w, cx| this.toggle_lane_visible(cx)),
            ))
            .when_some(dropdown, |root, panel| root.child(panel))
    }

    pub(super) fn render_toolbar(
        &self,
        cx: &mut Context<Self>,
        clip_id: Option<&str>,
    ) -> impl IntoElement {
        let note_count = clip_id
            .and_then(|cid| {
                self.timeline
                    .read(cx)
                    .state
                    .midi_clip_notes(cid)
                    .map(|n| n.len())
            })
            .unwrap_or(0);
        let sel_count = self.selection.len();
        let tool = self.tool;
        let snap_on = self.snap_on;
        let grid_label = format!("Grid: {}", self.grid_res.label());
        let status = self.toolbar_status(note_count, sel_count);
        let grid_options = GridRes::ALL
            .iter()
            .enumerate()
            .map(|(idx, res)| {
                let res = *res;
                (
                    res.label().to_string(),
                    res == self.grid_res,
                    div()
                        .id(("pr-grid-choice", idx))
                        .size_full()
                        .flex()
                        .items_center()
                        .child(res.label().to_string())
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            cx.stop_propagation();
                            this.grid_res = res;
                            // Free mode turns snapping off; other modes re-enable it.
                            this.snap_on = !res.is_free();
                            this.open_select_menu = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            })
            .collect();
        let channel_options = {
            let mut opts = Vec::with_capacity(17);
            opts.push((
                "All Channels".to_string(),
                self.channel_view.is_all(),
                div()
                    .id("pr-channel-choice-all")
                    .size_full()
                    .flex()
                    .items_center()
                    .child("All Channels")
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.set_channel_view(MidiChannelMask::ALL, cx);
                    }))
                    .into_any_element(),
            ));
            for (idx, ch) in MidiChannel::all().enumerate() {
                let selected = self.channel_view == MidiChannelMask::single(ch);
                let label = format!("Channel {}", ch.ui());
                opts.push((
                    label.clone(),
                    selected,
                    div()
                        .id(("pr-channel-choice", idx))
                        .size_full()
                        .flex()
                        .items_center()
                        .child(label)
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            cx.stop_propagation();
                            this.set_channel_view(MidiChannelMask::single(ch), cx);
                        }))
                        .into_any_element(),
                ));
            }
            opts
        };
        let root_options = ScaleRoot::ALL
            .iter()
            .enumerate()
            .map(|(idx, root)| {
                let root = *root;
                (
                    root.label().to_string(),
                    root == self.pitch_ctx.scale.root,
                    div()
                        .id(("pr-root-choice", idx))
                        .size_full()
                        .flex()
                        .items_center()
                        .child(root.label())
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            cx.stop_propagation();
                            this.pitch_ctx.scale.root = root;
                            this.open_select_menu = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            })
            .collect();
        let scale_options = ScaleKind::ALL
            .iter()
            .enumerate()
            .map(|(idx, kind)| {
                let kind = *kind;
                (
                    kind.label().to_string(),
                    kind == self.pitch_ctx.scale.kind,
                    div()
                        .id(("pr-scale-choice", idx))
                        .size_full()
                        .flex()
                        .items_center()
                        .child(kind.label())
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            cx.stop_propagation();
                            this.pitch_ctx.scale.kind = kind;
                            this.open_select_menu = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            })
            .collect();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(34.0))
            .px(px(8.0))
            .border_b(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            .child(
                toolbar_group("Tools")
                    .child(tool_btn(
                        "pr-select",
                        "Select",
                        tool == PianoTool::Select,
                        cx.listener(|this, _, _w, cx| {
                            this.cancel_active_gesture(cx);
                            this.tool = PianoTool::Select;
                            cx.notify();
                        }),
                    ))
                    .child(tool_btn(
                        "pr-draw",
                        "Draw",
                        tool == PianoTool::Draw,
                        cx.listener(|this, _, _w, cx| {
                            this.cancel_active_gesture(cx);
                            this.tool = PianoTool::Draw;
                            cx.notify();
                        }),
                    ))
                    .child(tool_btn(
                        "pr-line",
                        "Line",
                        tool == PianoTool::Line,
                        cx.listener(|this, _, _w, cx| {
                            this.cancel_active_gesture(cx);
                            this.tool = PianoTool::Line;
                            cx.notify();
                        }),
                    ))
                    .child(tool_btn(
                        "pr-erase",
                        "Erase",
                        tool == PianoTool::Erase,
                        cx.listener(|this, _, _w, cx| {
                            this.cancel_active_gesture(cx);
                            this.tool = PianoTool::Erase;
                            cx.notify();
                        }),
                    ))
                    .child(tool_btn(
                        "pr-split",
                        "Split",
                        tool == PianoTool::Split,
                        cx.listener(|this, _, _w, cx| {
                            this.cancel_active_gesture(cx);
                            this.tool = PianoTool::Split;
                            cx.notify();
                        }),
                    ))
                    .child(tool_btn(
                        "pr-mute-tool",
                        "Mute",
                        tool == PianoTool::Mute,
                        cx.listener(|this, _, _w, cx| {
                            this.cancel_active_gesture(cx);
                            this.tool = PianoTool::Mute;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                toolbar_group("Snap")
                    .child(tool_btn(
                        "pr-snap",
                        "Snap",
                        snap_on,
                        cx.listener(|this, _, _w, cx| {
                            this.snap_on = !this.snap_on;
                            cx.notify();
                        }),
                    ))
                    .child(self.render_select_menu(
                        PianoSelectMenu::Grid,
                        "pr-grid-select",
                        grid_label,
                        grid_options,
                        cx,
                    )),
            )
            .child(
                toolbar_group("Scale")
                    .child(self.render_select_menu(
                        PianoSelectMenu::ScaleRoot,
                        "pr-scale-root",
                        self.pitch_ctx.scale.root.label().to_string(),
                        root_options,
                        cx,
                    ))
                    .child(self.render_select_menu(
                        PianoSelectMenu::ScaleKind,
                        "pr-scale-kind",
                        self.pitch_ctx.scale.kind.label().to_string(),
                        scale_options,
                        cx,
                    ))
                    .child(tool_btn(
                        "pr-scale-constrain",
                        "Lock",
                        self.pitch_ctx.constrain,
                        cx.listener(|this, _, _w, cx| {
                            this.pitch_ctx.constrain = !this.pitch_ctx.constrain;
                            this.open_select_menu = None;
                            cx.notify();
                        }),
                    ))
                    .child(tool_btn(
                        "pr-scale-snap-selection",
                        "To Scale",
                        false,
                        cx.listener(|this, _, _w, cx| this.snap_selection_to_scale(cx)),
                    )),
            )
            .child(
                toolbar_group("Channel")
                    .child(self.render_select_menu(
                        PianoSelectMenu::Channel,
                        "pr-channel-view",
                        self.channel_view_label(),
                        channel_options,
                        cx,
                    ))
                    .child(tool_btn(
                        "pr-channel-apply",
                        "Set Sel",
                        false,
                        cx.listener(|this, _, _w, cx| {
                            let channel = this.active_note_channel(cx);
                            this.set_selected_notes_channel(channel, cx);
                        }),
                    ))
                    .child(tool_btn(
                        "pr-channel-output-mode",
                        "Per-Note Out",
                        self.track_output_per_note(cx),
                        cx.listener(|this, _, _w, cx| this.toggle_track_output_per_note(cx)),
                    )),
            )
            .child(
                toolbar_group("Edit")
                    .child(
                        div()
                            .id("pr-quantize")
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(22.0))
                            .min_w(px(24.0))
                            .px(px(7.0))
                            .rounded(px(crate::theme::radius::CONTROL_SM))
                            .text_size(px(10.0))
                            .text_color(if self.quantize_preview {
                                Colors::text_primary()
                            } else {
                                Colors::text_secondary()
                            })
                            .bg(if self.quantize_preview {
                                Colors::surface_hover()
                            } else {
                                Colors::with_alpha(Colors::text_primary(), 0.0)
                            })
                            .border(px(1.0))
                            .border_color(if self.quantize_preview {
                                Colors::border_subtle()
                            } else {
                                Colors::with_alpha(Colors::text_primary(), 0.0)
                            })
                            .hover(|s| s.bg(Colors::surface_hover()))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
                                this.quantize_preview = *hovered;
                                cx.notify();
                            }))
                            .on_click(cx.listener(|this, _, _w, cx| this.quantize_selection(cx)))
                            .child("Quantize"),
                    )
                    .child(tool_btn(
                        "pr-delete",
                        "Del",
                        false,
                        cx.listener(|this, _, _w, cx| this.delete_selection(cx)),
                    ))
                    .child(tool_btn(
                        "pr-dup",
                        "Dup",
                        false,
                        cx.listener(|this, _, _w, cx| this.duplicate_selection(false, cx)),
                    )),
            )
            .child(toolbar_group("Controller").child(self.render_lane_selector(cx)))
            .child(
                toolbar_group("View")
                    .child(tool_btn(
                        "pr-fit",
                        "Fit",
                        false,
                        cx.listener(|this, _, _w, cx| {
                            if let Some(cid) = this.editing_clip_id(cx) {
                                this.fit_piano_roll_to_notes(cx, &cid);
                                cx.notify();
                            }
                        }),
                    ))
                    .child(tool_btn(
                        "pr-zoom-out",
                        "−",
                        false,
                        cx.listener(|this, _, _w, cx| this.zoom_by(0.5, cx)),
                    ))
                    .child(tool_btn(
                        "pr-zoom-in",
                        "+",
                        false,
                        cx.listener(|this, _, _w, cx| this.zoom_by(2.0, cx)),
                    ))
                    .child(tool_btn(
                        "pr-c4",
                        "C4",
                        false,
                        cx.listener(|this, _, _w, cx| {
                            this.scroll_to_pitch(60);
                            cx.notify();
                        }),
                    )),
            )
            .child(div().flex_1())
            .child(
                div()
                    .min_w(px(132.0))
                    .text_size(px(9.0))
                    .text_color(Colors::text_muted())
                    .truncate()
                    .child(status),
            )
            .when_some(self.on_pop_out.clone(), |row, pop_out| {
                row.child(
                    div()
                        .id("pr-pop-out")
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .text_size(px(9.0))
                        .text_color(Colors::text_secondary())
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|s| s.bg(Colors::surface_hover()))
                        .on_click(move |_, window, cx| pop_out(window, cx))
                        .child("Pop out"),
                )
            })
    }

    pub(super) fn render_body(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
    ) -> impl IntoElement {
        let (view_w, view_h) = self.grid_view_size();
        let track_color = self.track_color_for_clip(cx, clip_id);
        let (bpb, clip_len, show_playhead, playing, playhead_rel, loop_region) = {
            let tl = self.timeline.read(cx);
            let bpb = tl.state.beats_per_bar().max(1.0);
            let (clip_start, clip_len) = self.clip_meta(cx, clip_id);
            let t = &tl.state.transport;
            let playhead_rel = t.playhead_beats - clip_start;
            // Playhead is visible whenever it sits within the clip — playing or
            // paused — so the user always sees the current position.
            let show_playhead = playhead_rel >= 0.0 && playhead_rel <= clip_len;
            // Loop region in clip-local beats (transport stores project-global).
            let loop_region = if t.loop_enabled && t.loop_end_beats > t.loop_start_beats {
                Some((
                    t.loop_start_beats - clip_start,
                    t.loop_end_beats - clip_start,
                ))
            } else {
                None
            };
            (
                bpb,
                clip_len,
                show_playhead,
                t.playing,
                playhead_rel,
                loop_region,
            )
        };

        // Visible ranges (only build geometry for what's on screen).
        let first_pitch = (self.y_to_pitch(view_h) as i32 - 1).max(0);
        let last_pitch = (self.y_to_pitch(0.0) as i32 + 1).min(PITCH_CNT - 1);
        let start_beat = self.x_to_beat(0.0);
        let end_beat = self.x_to_beat(view_w);

        // Piano key lane.
        // Label policy: show every note name when each row has enough vertical
        // room (>= 14 px), otherwise fall back to C-only labels so the lane
        // stays readable.
        let row_h = self.note_row_h();
        let show_all_labels = row_h >= 14.0;
        let pressed_pitch = self.key_lane_pressed_pitch;
        let keys: Vec<_> = (first_pitch..=last_pitch)
            .map(|p| {
                let y = self.pitch_to_y(p as u8);
                let black = is_black(p);
                let is_c = p.rem_euclid(12) == 0;
                let pressed = pressed_pitch == Some(p as u8);
                let label_color = if is_c {
                    Colors::text_primary()
                } else if black {
                    Colors::text_muted()
                } else {
                    Colors::text_secondary()
                };
                let show_label = is_c || show_all_labels;
                div()
                    .absolute()
                    .top(px(y))
                    .left_0()
                    .w_full()
                    .h(px(row_h))
                    // Pressed/auditioned key reads with the accent fill (both black
                    // and white keys); otherwise the usual black/white surface.
                    .bg(if pressed {
                        Colors::accent_primary()
                    } else if black {
                        Colors::surface_base()
                    } else {
                        Colors::surface_raised()
                    })
                    .border_b(px(1.0))
                    .border_color(Colors::border_subtle())
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr(px(5.0))
                    .cursor(gpui::CursorStyle::PointingHand)
                    // Mouse-down starts a key-lane scrub. The matching note-off /
                    // state reset happens centrally in `on_up` (wired to both
                    // mouse-up and mouse-up-out on the root), and drag tracking
                    // happens in `on_move` — so no per-key up/move handlers here.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            if midi_debug_enabled() {
                                eprintln!("[PianoKeyPreview] down note={p}");
                            }
                            this.piano_key_drag_active = true;
                            this.key_lane_pressed_pitch = Some(p as u8);
                            this.begin_preview_note(p as u8, 100, "piano_key_down", cx);
                            cx.notify();
                        }),
                    )
                    .when(show_label, |this| {
                        this.child(
                            div()
                                .text_size(px(8.0))
                                .text_color(label_color)
                                .child(note_name(p)),
                        )
                    })
            })
            .collect();

        let grid_lines = self.build_grid_lines(
            start_beat,
            end_beat,
            view_w,
            view_h,
            first_pitch,
            last_pitch,
            bpb,
            clip_len,
        );
        let clip_bounds = self.build_clip_bounds_overlay(clip_len, view_w, view_h);
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
                x: self.beat_to_x(playhead_rel),
                visible: show_playhead,
                playing,
            },
        );
        let playhead_overlay = self.playhead_overlay.clone();
        let mut ruler = self.build_ruler(start_beat, end_beat, bpb);
        ruler.extend(self.build_loop_ruler_markers(loop_region));
        let notes_geo = self.build_note_elements(cx, clip_id, track_color);
        let quantize_preview = self.build_quantize_preview(cx, clip_id);
        let marquee_overlay = self.build_marquee_overlay();
        let draw_preview = self.build_draw_note_preview();
        let erase_overlay = self.build_erase_overlay();
        let note_menu = self.build_note_context_menu(cx);
        // Empty clip: the musical canvas (ruler, keys, grid) stays, with one
        // quiet hint naming the gesture that actually creates a note. Nothing
        // here is a status readout — the note/selection counts live in the
        // toolbar.
        let grid_empty_hint = (note_count_for_clip(cx, &self.timeline, clip_id) == 0).then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.0))
                .text_color(Colors::text_muted())
                .child("Draw notes with the Draw tool, or record onto this clip")
                .into_any_element()
        });
        let note_inspector = self.render_note_inspector(cx, clip_id);

        // ── Single unified controller lane ───────────────────────────────────
        // Exactly one lane is built per frame: velocity OR the active controller.
        // Switching the selector only changes which is built — the hidden lane's
        // data (note velocities / other controller points) is left untouched.
        let unified_lane_visible = self.unified_lane_visible(cx);
        let lane_header: Option<gpui::AnyElement> = unified_lane_visible.then(|| {
            div()
                .h(px(LANE_H))
                .w_full()
                .border_t(px(1.0))
                .border_color(Colors::panel_border())
                .bg(Colors::surface_panel())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(2.0))
                .text_size(px(9.0))
                .text_color(Colors::text_secondary())
                .child(self.lane_name())
                .child(
                    div()
                        .text_size(px(7.0))
                        .text_color(Colors::text_faint())
                        .child(self.lane_range()),
                )
                .into_any_element()
        });
        let lane_body: Option<gpui::AnyElement> = if !unified_lane_visible {
            None
        } else if self.lane_view == PianoLaneView::Articulations {
            Some(
                self.render_articulation_lane(cx, clip_id, start_beat, end_beat, bpb)
                    .into_any_element(),
            )
        } else if self.lane_view == PianoLaneView::Velocity {
            let vel_grid = self.build_velocity_grid(start_beat, end_beat, bpb);
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
                        .text_size(px(9.0))
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
                    .children(vel_bars)
                    .children(velocity_gesture_overlay)
                    .children(velocity_empty)
                    .children(velocity_value_chip)
                    .children(velocity_context_menu)
                    .into_any_element(),
            )
        } else {
            Some(
                self.render_cc_lane(cx, clip_id, start_beat, end_beat, bpb)
                    .into_any_element(),
            )
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
        // hit-tested + mapped to a pitch during drag-scrub (see `on_move`). Sits
        // behind the keys and carries no handlers, so it never intercepts the
        // per-key mouse-down.
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
            // Left: piano keys.
            .child(
                div()
                    .w(px(key_lane_width()))
                    .h_full()
                    .flex()
                    .flex_col()
                    // Corner spacer so the keys line up with the grid (below the
                    // ruler row on the right).
                    .child(
                        div()
                            .h(px(RULER_H))
                            .w_full()
                            .bg(Colors::surface_panel())
                            .border_b(px(1.0))
                            .border_r(px(1.0))
                            .border_color(Colors::panel_border()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .overflow_hidden()
                            .bg(Colors::surface_panel())
                            .border_r(px(1.0))
                            .border_color(Colors::panel_border())
                            // Drag-scrub (move/up) is handled by the root-level
                            // `on_move`/`on_up` using `key_lane_bounds` captured
                            // here — never read raw window coords as if they were
                            // lane-local (the old bug).
                            .child(key_lane_canvas)
                            .children(keys),
                    )
                    // Single unified controller-lane header (name + range).
                    .children(lane_header),
            )
            // Right: grid + single controller lane.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    // Ruler header — bar/beat labels aligned to the grid below.
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
                            .children(clip_bounds)
                            .children(loop_overlay)
                            .children(playhead_overlay)
                            .children(grid_empty_hint)
                            .children(notes_geo)
                            .children(quantize_preview)
                            .when_some(marquee_overlay, |el, overlay| el.child(overlay))
                            .when_some(draw_preview, |el, overlay| el.child(overlay))
                            .when_some(erase_overlay, |el, overlay| el.child(overlay))
                            .children(note_menu)
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_grid_down))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(Self::on_grid_right_down),
                            ),
                    )
                    // Single unified controller lane (velocity / CC / etc).
                    .children(lane_body),
            )
            .children(self.render_scrollbars(cx, clip_id))
            .child(note_inspector)
    }

    fn render_scrollbars(&self, cx: &Context<Self>, clip_id: &str) -> Vec<gpui::AnyElement> {
        let (view_w, view_h) = self.grid_view_size();
        let (_, clip_len) = self.clip_meta(cx, clip_id);
        let max_x = (clip_len * self.ppb - view_w).max(0.0);
        let max_y = self.max_scroll_y();
        let mut bars = Vec::new();

        if max_y > 0.5 {
            let track_h = view_h.max(1.0);
            // A just-created/floating editor can have a viewport shorter than
            // the preferred scrollbar thumb. `clamp` panics when its minimum
            // exceeds its maximum, so cap the preferred minimum to the track
            // size before clamping.
            let min_thumb_h = 24.0_f32.min(track_h);
            let thumb_h = (track_h * (view_h / (view_h + max_y))).clamp(min_thumb_h, track_h);
            let thumb_y = ((self.scroll_y / max_y) * (track_h - thumb_h)).clamp(0.0, track_h);
            bars.push(
                div()
                    .absolute()
                    .right(px(3.0))
                    .top(px(RULER_H + thumb_y))
                    .w(px(5.0))
                    .h(px(thumb_h))
                    .rounded(px(crate::theme::radius::MICRO))
                    .bg(Colors::with_alpha(Colors::text_faint(), 0.42))
                    .into_any_element(),
            );
        }

        if max_x > 0.5 {
            let track_w = view_w.max(1.0);
            let min_thumb_w = 32.0_f32.min(track_w);
            let thumb_w = (track_w * (view_w / (view_w + max_x))).clamp(min_thumb_w, track_w);
            let thumb_x = ((self.scroll_x / max_x) * (track_w - thumb_w)).clamp(0.0, track_w);
            bars.push(
                div()
                    .absolute()
                    .left(px(key_lane_width() + thumb_x))
                    .bottom(px(if self.unified_lane_visible(cx) {
                        LANE_H + 3.0
                    } else {
                        3.0
                    }))
                    .w(px(thumb_w))
                    .h(px(5.0))
                    .rounded(px(crate::theme::radius::MICRO))
                    .bg(Colors::with_alpha(Colors::text_faint(), 0.42))
                    .into_any_element(),
            );
        }

        bars
    }

    /// Articulation assignment buttons for the selected notes: one compact
    /// button per built-in articulation plus "None". Wraps across rows via
    /// `note_button_row`. Applies to the whole selection as one undo entry.
    fn articulation_assign_row(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
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
        // `None` while the selection is mixed, so no button claims to be the
        // current value.
        let current = self.uniform_selection_articulation(cx);
        let mut buttons: Vec<gpui::AnyElement> = available
            .iter()
            .map(|articulation| {
                let articulation = *articulation;
                note_toggle_button(
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
        buttons.push(
            note_toggle_button(
                button_id(None),
                "None",
                current == Some(None),
                cx.listener(|this, _, _w, cx| this.set_selection_articulation(None, cx)),
            )
            .into_any_element(),
        );
        note_button_row(buttons).into_any_element()
    }

    pub(super) fn render_note_inspector(
        &self,
        cx: &mut Context<Self>,
        clip_id: &str,
    ) -> impl IntoElement {
        let snapshot = self.note_inspector_snapshot(cx, clip_id);
        let count = snapshot.count();
        let step = self.grid_res.beats().max(MIN_NOTE_BEATS);
        let fine_step = (step * 0.25).max(MIN_NOTE_BEATS);

        let mut content: Vec<gpui::AnyElement> = Vec::new();
        content.push(note_inspector_label("NOTE INSPECTOR").into_any_element());

        if count == 0 {
            content.push(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_muted())
                    .line_height(px(15.0))
                    .child("Select notes in the piano roll to edit pitch, timing, and velocity.")
                    .into_any_element(),
            );
        } else if count == 1 {
            let note = &snapshot.selected[0];
            content.push(note_value_row("Pitch", snapshot.pitch_label()).into_any_element());
            content.push(note_value_row("Start", format_beats(note.start)).into_any_element());
            content.push(note_value_row("Length", format_beats(note.duration)).into_any_element());
            content.push(
                note_value_row("End", format_beats(note.start + note.duration)).into_any_element(),
            );
            content.push(note_value_row("Velocity", note.velocity.to_string()).into_any_element());
            content.push(note_value_row("Channel", note.channel.label()).into_any_element());
            content
                .push(note_value_row("Artic.", snapshot.articulation_label()).into_any_element());
            content.push(self.articulation_assign_row(cx));
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-note-chan-down",
                        "Ch -1",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_channel(-1, cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-note-chan-up",
                        "Ch +1",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_channel(1, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-note-pitch-down",
                        "-1 st",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_pitch(-1, cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-note-pitch-up",
                        "+1 st",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_pitch(1, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-note-start-down",
                        "-Start",
                        cx.listener(move |this, _, _w, cx| this.nudge_selected_start(-step, cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-note-start-up",
                        "+Start",
                        cx.listener(move |this, _, _w, cx| this.nudge_selected_start(step, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-note-len-down",
                        "-Len",
                        cx.listener(move |this, _, _w, cx| {
                            this.nudge_selected_length(-fine_step, cx)
                        }),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-note-len-up",
                        "+Len",
                        cx.listener(move |this, _, _w, cx| {
                            this.nudge_selected_length(fine_step, cx)
                        }),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-note-vel-down",
                        "Vel -5",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_velocity(-5, cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-note-vel-up",
                        "Vel +5",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_velocity(5, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            let mute_label = if note.muted { "Unmute" } else { "Mute" };
            content.push(
                note_button_row(vec![note_action_button(
                    "pr-note-mute",
                    mute_label,
                    cx.listener(|this, _, _w, cx| this.toggle_mute_selection(cx)),
                )
                .into_any_element()])
                .into_any_element(),
            );
        } else {
            content.push(note_value_row("Selected", count.to_string()).into_any_element());
            content.push(note_value_row("Pitch", snapshot.pitch_label()).into_any_element());
            content.push(note_value_row("Range", snapshot.end_label()).into_any_element());
            content.push(note_value_row("Start", snapshot.start_label()).into_any_element());
            content.push(note_value_row("Length", snapshot.length_label()).into_any_element());
            content.push(note_value_row("Velocity", snapshot.velocity_label()).into_any_element());
            content.push(note_value_row("Channel", snapshot.channel_label()).into_any_element());
            content
                .push(note_value_row("Artic.", snapshot.articulation_label()).into_any_element());
            content.push(self.articulation_assign_row(cx));
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-notes-chan-down",
                        "Ch -1",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_channel(-1, cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-notes-chan-up",
                        "Ch +1",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_channel(1, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-notes-trans-down",
                        "-1 st",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_pitch(-1, cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-notes-trans-up",
                        "+1 st",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_pitch(1, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-notes-vel-down",
                        "Vel -5",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_velocity(-5, cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-notes-vel-up",
                        "Vel +5",
                        cx.listener(|this, _, _w, cx| this.nudge_selected_velocity(5, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-notes-quantize",
                        "Quantize",
                        cx.listener(|this, _, _w, cx| this.quantize_selection(cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-notes-delete",
                        "Delete",
                        cx.listener(|this, _, _w, cx| this.delete_selection(cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
            content.push(
                note_button_row(vec![
                    note_action_button(
                        "pr-notes-mute",
                        "Mute",
                        cx.listener(|this, _, _w, cx| this.toggle_mute_selection(cx)),
                    )
                    .into_any_element(),
                    note_action_button(
                        "pr-notes-duplicate",
                        "Duplicate",
                        cx.listener(|this, _, _w, cx| this.duplicate_selection(false, cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
        }

        div()
            .w(px(216.0))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .p(px(8.0))
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
        bpb: f32,
    ) -> Vec<(f32, GridLineKind)> {
        self.viewport().grid_lines(start_beat, end_beat, bpb)
    }

    pub(super) fn build_clip_bounds_overlay(
        &self,
        clip_len: f32,
        view_w: f32,
        view_h: f32,
    ) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        let end_x = self.beat_to_x(clip_len);
        if end_x < view_w {
            out.push(
                div()
                    .absolute()
                    .left(px(end_x))
                    .top_0()
                    .w(px((view_w - end_x).max(0.0)))
                    .h(px(view_h))
                    .bg(Colors::with_alpha(Colors::surface_base(), 0.55))
                    .into_any_element(),
            );
        }
        out.push(
            div()
                .absolute()
                .left(px(0.0))
                .top_0()
                .w(px(1.0))
                .h(px(view_h))
                .bg(Colors::with_alpha(Colors::accent_primary(), 0.35))
                .into_any_element(),
        );
        if end_x > 0.0 && end_x <= view_w + 2.0 {
            out.push(
                div()
                    .absolute()
                    .left(px(end_x))
                    .top_0()
                    .w(px(1.0))
                    .h(px(view_h))
                    .bg(Colors::with_alpha(Colors::accent_primary(), 0.55))
                    .into_any_element(),
            );
        }
        out
    }

    /// Loop region band + edge lines over the note grid (clip-local beats).
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
        let band_x0 = self.beat_to_x(lo).max(0.0);
        let band_x1 = self.beat_to_x(hi).min(view_w);
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
            let ex = self.beat_to_x(edge);
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
        let left = self.beat_to_x(lo).max(0.0);
        let right = self.beat_to_x(hi);
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
        _view_h: f32,
        first_pitch: i32,
        last_pitch: i32,
        bpb: f32,
        clip_len: f32,
    ) -> Vec<gpui::AnyElement> {
        let mut out: Vec<gpui::AnyElement> = Vec::new();

        let row_h = self.note_row_h();
        // ── Pitch row backgrounds: shade black-key rows, highlight C rows ──
        for p in first_pitch..=last_pitch {
            let y = self.pitch_to_y(p as u8);
            if is_black(p) {
                out.push(
                    div()
                        .absolute()
                        .top(px(y))
                        .left_0()
                        .w(px(view_w))
                        .h(px(row_h))
                        .bg(Colors::with_alpha(Colors::surface_base(), 0.45))
                        .into_any_element(),
                );
            } else if p % 12 == 0 {
                // C row — a touch brighter so octaves are easy to scan.
                out.push(
                    div()
                        .absolute()
                        .top(px(y))
                        .left_0()
                        .w(px(view_w))
                        .h(px(row_h))
                        .bg(Colors::with_alpha(Colors::text_primary(), 0.03))
                        .into_any_element(),
                );
            }
        }

        // Clip end marker inside the visible beat range.
        let end_x = self.beat_to_x(clip_len);
        if end_x >= 0.0 && end_x <= view_w {
            out.push(
                div()
                    .absolute()
                    .left(px((end_x - 0.5).max(0.0)))
                    .top_0()
                    .w(px(1.0))
                    .h_full()
                    .bg(Colors::with_alpha(Colors::accent_primary(), 0.4))
                    .into_any_element(),
            );
        }

        // ── Vertical timing gridlines (zoom-aware hierarchy) ──
        for (x, kind) in self.visible_grid_lines(start_beat, end_beat.min(clip_len + bpb), bpb) {
            let (alpha, w) = match kind {
                GridLineKind::Bar => (0.26, 1.0),
                GridLineKind::Beat => (0.13, 1.0),
                GridLineKind::Subdivision => (0.06, 1.0),
            };
            out.push(
                div()
                    .absolute()
                    .top_0()
                    .left(px(x))
                    .w(px(w))
                    .h_full()
                    .bg(Colors::with_alpha(Colors::text_primary(), alpha))
                    .into_any_element(),
            );
        }

        // ── Horizontal pitch row lines ──
        // Draw a line for every visible semitone row so editing reads like a
        // real piano roll. C gets the strongest line (octave boundary), F gets
        // a medium line (the other white-white separator on a piano), and every
        // other row gets a faint hairline.
        for p in first_pitch..=last_pitch {
            let m = p.rem_euclid(12);
            let alpha = match m {
                0 => 0.14,  // C: octave boundary
                5 => 0.07,  // F: white/white separator
                _ => 0.035, // every other semitone row
            };
            let y = self.pitch_to_y(p as u8);
            out.push(
                div()
                    .absolute()
                    .top(px(y))
                    .left_0()
                    .w(px(view_w))
                    .h(px(1.0))
                    .bg(Colors::with_alpha(Colors::text_primary(), alpha))
                    .into_any_element(),
            );
        }

        out
    }

    /// Bar/beat ruler header labels, aligned to the note grid via `beat_to_x`.
    /// The ruler's labels and ticks.
    ///
    /// Mark positions come from [`PianoRollViewport::ruler_marks`], the single
    /// source of ruler geometry in the editor; only the styling is local.
    pub(super) fn build_ruler(
        &self,
        start_beat: f32,
        end_beat: f32,
        bpb: f32,
    ) -> Vec<gpui::AnyElement> {
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        for mark in self.viewport().ruler_marks(start_beat, end_beat, bpb) {
            out.push(
                div()
                    .absolute()
                    .top_0()
                    .left(px(mark.x + 2.0))
                    .text_size(px(8.5))
                    .text_color(if mark.on_bar {
                        Colors::text_secondary()
                    } else {
                        Colors::text_muted()
                    })
                    .child(mark.label)
                    .into_any_element(),
            );
            // Tick mark at the bottom of the ruler.
            out.push(
                div()
                    .absolute()
                    .left(px(mark.x))
                    .bottom_0()
                    .w(px(1.0))
                    .h(px(if mark.on_bar { 6.0 } else { 4.0 }))
                    .bg(Colors::with_alpha(
                        Colors::text_primary(),
                        if mark.on_bar { 0.26 } else { 0.13 },
                    ))
                    .into_any_element(),
            );
        }
        out
    }

    /// Bar/beat vertical lines through the velocity lane (aligned with the grid;
    /// subdivisions omitted to keep the lane uncluttered).
    pub(super) fn build_velocity_grid(
        &self,
        start_beat: f32,
        end_beat: f32,
        bpb: f32,
    ) -> Vec<gpui::AnyElement> {
        self.visible_grid_lines(start_beat, end_beat, bpb)
            .into_iter()
            .filter(|(_, kind)| *kind != GridLineKind::Subdivision)
            .map(|(x, kind)| {
                let alpha = if kind == GridLineKind::Bar {
                    0.20
                } else {
                    0.10
                };
                div()
                    .absolute()
                    .top_0()
                    .left(px(x))
                    .w(px(1.0))
                    .h_full()
                    .bg(Colors::with_alpha(Colors::text_primary(), alpha))
                    .into_any_element()
            })
            .collect()
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
        let step = self.quantize_res.beats().max(MIN_NOTE_BEATS);
        let only_selected = !self.selection.is_empty();
        let accent = Colors::accent_primary();
        let row_h = self.note_row_h();
        let tl = self.timeline.read(cx);
        let Some(notes) = tl.state.midi_clip_notes(clip_id) else {
            return Vec::new();
        };
        notes
            .iter()
            .filter(|n| !only_selected || self.selection.contains(&n.id))
            .filter_map(|n| {
                let q_start = (n.start / step).round() * step;
                if (q_start - n.start).abs() < 1.0e-4 {
                    return None;
                }
                let x = self.beat_to_x(q_start);
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
                    let x = self.beat_to_x(d.start);
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
                    ))
                })
                .collect()
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
                )| {
                    let mut fill = track_color;
                    fill.a = if erase_target {
                        0.45
                    } else if muted {
                        // Muted notes read as hollow/dim so they stand apart from
                        // active notes without leaving the grid.
                        0.18
                    } else if selected {
                        1.0
                    } else {
                        0.78
                    };
                    let border = if erase_target {
                        Colors::status_error()
                    } else if selected {
                        Colors::accent_primary()
                    } else if muted {
                        Colors::with_alpha(Colors::text_muted(), 0.7)
                    } else {
                        Colors::with_alpha(track_color, 0.55)
                    };
                    let mut note = div()
                        .id(("pr-note", id as usize))
                        .absolute()
                        .left(px(x))
                        .top(px(y + 1.0))
                        .w(px(w))
                        .h(px(row_h - 2.0))
                        .rounded(px(crate::theme::radius::MICRO))
                        .bg(fill)
                        .border(px(1.0))
                        .border_color(border)
                        .shadow(if selected {
                            vec![gpui::BoxShadow {
                                color: Colors::with_alpha(Colors::accent_primary(), 0.35).into(),
                                offset: gpui::point(px(0.0), px(0.0)),
                                blur_radius: px(8.0),
                                spread_radius: px(0.0),
                                inset: false,
                            }]
                        } else {
                            Vec::new()
                        })
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
                    // Note-name label, shown only when the block is large enough to
                    // read so dense clips stay clean.
                    if w >= 22.0 && row_h >= 11.0 {
                        let label_color = if muted {
                            Colors::with_alpha(Colors::text_muted(), 0.8)
                        } else if selected {
                            Colors::text_primary()
                        } else {
                            Colors::with_alpha(Colors::text_primary(), 0.85)
                        };
                        note = note.child(
                            div()
                                .absolute()
                                .left(px(3.0))
                                .top_0()
                                .bottom_0()
                                .flex()
                                .items_center()
                                .text_size(px(8.0))
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
                                .hover(|s| s.bg(Colors::with_alpha(Colors::text_primary(), 0.35)))
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

    pub(super) fn build_velocity_bars(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
        track_color: gpui::Rgba,
    ) -> Vec<gpui::AnyElement> {
        let (view_w, _) = self.grid_view_size();
        let geos: Vec<(u64, u8, f32, bool)> = {
            let tl = self.timeline.read(cx);
            let Some(notes) = tl.state.midi_clip_notes(clip_id) else {
                return Vec::new();
            };
            notes
                .iter()
                .filter(|n| self.channel_visible(n.channel))
                .filter_map(|n| {
                    let d = self.display_note(n);
                    let x = self.beat_to_x(d.start);
                    if x < -8.0 || x > view_w {
                        return None;
                    }
                    Some((d.id, d.velocity, x, self.selection.contains(&d.id)))
                })
                .collect()
        };

        geos.into_iter()
            .map(|(id, vel, x, selected)| {
                let bar_h = (((vel as f32 - 1.0) / 126.0) * (LANE_H - 8.0)).max(1.0);
                let mut fill = track_color;
                fill.a = if selected { 1.0 } else { 0.5 };
                // Full-height invisible hit column so even low-velocity bars are
                // easy to grab; the colored bar sits inside it at the bottom.
                div()
                    .id(("pr-vel", id as usize))
                    .absolute()
                    .left(px(x))
                    .top_0()
                    .bottom_0()
                    .w(px(8.0))
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
                            .left_0()
                            .bottom(px(2.0))
                            .w(px(6.0))
                            .h(px(bar_h))
                            .rounded_t(px(crate::theme::radius::MICRO))
                            .bg(fill),
                    )
                    .into_any_element()
            })
            .collect()
    }
}
