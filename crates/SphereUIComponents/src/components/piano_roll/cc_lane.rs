//! Split out of `piano_roll.rs` (god-file decomposition). These are
//! `impl PianoRoll` extension blocks; `use super::*` pulls in the shared
//! piano-roll vocabulary (struct fields via the type, consts, free fns).

use super::*;

use super::cc_lane_render::{self, CcHandle, CcLaneSnapshot};

/// Spacing, in lane pixels, between samples written while freehand-drawing a CC
/// stroke. Small enough that the written curve matches the pointer path at any
/// drag speed, large enough that free (unsnapped) drawing does not mint a point
/// per pixel.
const CC_PAINT_SAMPLE_PX: f32 = 2.0;
const CC_POINT_MERGE_EPS: f32 = 1.0e-3;

/// Radius of a CC point handle, in lane pixels. Kept under [`CC_HIT_R`] so a
/// handle is never larger than the area that grabs it.
const HANDLE_R: f32 = 4.0;

/// Grab radius around a CC point, in lane pixels. The 8 px handle sits inside
/// a 16 px target — the same "draw small, hit big" split as `size::hit_target`.
const CC_HIT_R: f32 = 8.0;

/// Inset of the value axis from the lane's top and bottom edge, so the 0 and
/// 127 handles are drawn whole instead of half-clipped. The inverse mapping
/// clamps inside the inset, which turns both edges into a few pixels of
/// "exactly min / max" rather than a one-pixel target.
const CC_VALUE_PAD: f32 = 5.0;

/// Keep a controller lane single-valued at each beat. Freehand painting can
/// revisit the same x range many times, and retaining near-identical points
/// makes the renderer draw narrow vertical spikes when the lane is sorted.
fn compact_cc_points(mut points: Vec<MidiControllerPoint>) -> Vec<MidiControllerPoint> {
    points.sort_by(|a, b| {
        a.beat
            .partial_cmp(&b.beat)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut compacted: Vec<MidiControllerPoint> = Vec::with_capacity(points.len());
    for point in points {
        if let Some(previous) = compacted.last_mut() {
            if (previous.beat - point.beat).abs() <= CC_POINT_MERGE_EPS {
                // `point` is newer in the paint/replace path, so its value is
                // the one that should remain at this beat.
                *previous = point;
                continue;
            }
        }
        compacted.push(point);
    }
    compacted
}

/// Replace the horizontal segment covered by one paint event. This is the
/// important distinction between a CC brush and an append-only point list:
/// dragging back over an earlier segment paints over it instead of building a
/// second envelope that later turns into a comb of almost-vertical lines.
fn replace_cc_paint_segment(
    existing: Vec<MidiControllerPoint>,
    edits: &[(f32, f32)],
    erase: bool,
    epsilon: f32,
) -> Vec<MidiControllerPoint> {
    let Some(first) = edits.first() else {
        return existing;
    };
    let last = edits.last().unwrap_or(first);
    let lo = first.0.min(last.0);
    let hi = first.0.max(last.0);
    let epsilon = epsilon.max(CC_POINT_MERGE_EPS);

    let mut points: Vec<MidiControllerPoint> = existing
        .into_iter()
        .filter(|point| point.beat < lo - epsilon || point.beat > hi + epsilon)
        .collect();
    if !erase {
        points.extend(
            edits
                .iter()
                .map(|(beat, value)| MidiControllerPoint::new(*beat, *value)),
        );
    }
    compact_cc_points(points)
}

impl PianoRoll {
    pub(super) fn cc_view_size(&self) -> (f32, f32) {
        match self.cc_bounds.get() {
            Some(b) => (
                f32::from(b.size.width).max(1.0),
                f32::from(b.size.height).max(1.0),
            ),
            None => (600.0, LANE_H),
        }
    }

    pub(super) fn cc_local(&self, window_pos: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let b = self.cc_bounds.get()?;
        let ox: f32 = b.origin.x.into();
        let oy: f32 = b.origin.y.into();
        let x: f32 = window_pos.x.into();
        let y: f32 = window_pos.y.into();
        Some((x - ox, y - oy))
    }

    /// Beat range a CC edit may write into: the edited clip's own span. A
    /// stroke that runs past the clip's end (or into the gap before a
    /// neighbour) would otherwise leave points no playback ever reaches.
    fn cc_beat_range(&self) -> (f32, f32) {
        let end = self
            .scope
            .editing()
            .map(|span| span.duration_beats)
            .filter(|len| *len > 0.0)
            .unwrap_or(f32::INFINITY);
        (0.0, end)
    }

    fn clamp_cc_beat(&self, beat: f32) -> f32 {
        let (lo, hi) = self.cc_beat_range();
        beat.clamp(lo, hi)
    }

    /// Delete one controller point, as its own undo step.
    ///
    /// The right-click gesture: it acts on the point under the cursor, so it
    /// borrows the paint gesture's undo bookkeeping rather than the selection's
    /// — the point being deleted need not be selected, and deleting it must not
    /// take the selection with it.
    pub(super) fn delete_cc_point(&mut self, point_id: u64, cx: &mut Context<Self>) -> bool {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return false;
        };
        let kind = self.active_cc;
        self.cc_edit_prev = Some(
            self.timeline
                .read(cx)
                .state
                .controller_points_snapshot(&clip_id, kind),
        );
        self.cc_edit_target = Some((clip_id.clone(), kind));
        let removed = self.timeline.update(cx, |tl, _| {
            tl.state.delete_controller_point(&clip_id, kind, point_id)
        });
        if !removed {
            self.cc_edit_prev = None;
            self.cc_edit_target = None;
            return false;
        }
        self.cc_selection.remove(&point_id);
        // `commit_cc_edit` no-ops when the points are unchanged, so this is the
        // whole edit: mutate, then record what actually changed.
        self.commit_cc_edit(cx);
        cx.notify();
        true
    }

    /// Begin a CC paint (`erase = false`) or erase (`erase = true`) gesture:
    /// ensure the active lane, snapshot its points for undo, and apply the first
    /// edit at the cursor.
    pub(super) fn begin_cc_paint(
        &mut self,
        erase: bool,
        unsnap: bool,
        lx: f32,
        ly: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        // The lane spans the whole track like the grid above it, so a press
        // in a neighbouring clip moves the editor there first — otherwise the
        // point would be written into the clip that happens to be selected,
        // at a beat that belongs to a different one.
        if self.retarget_to_clip_under(lx, cx) {
            return;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let kind = self.active_cc;
        self.timeline.update(cx, |tl, _| {
            tl.state.ensure_controller_lane(&clip_id, kind);
        });
        self.cc_edit_prev = Some(
            self.timeline
                .read(cx)
                .state
                .controller_points_snapshot(&clip_id, kind),
        );
        self.cc_edit_target = Some((clip_id.clone(), kind));
        self.drag = PianoDrag::CcPaint {
            erase,
            last: None,
            unsnap,
        };
        self.cc_paint_stroke_to(lx, ly, erase, cx);
        cx.notify();
    }

    /// Continue the freehand stroke to `(lx, ly)`, writing every sample between
    /// the previous cursor position and this one.
    ///
    /// A mouse move can cover tens of pixels, so sampling only the event
    /// position leaves holes: the drawn line jumps from dot to dot and reads as
    /// stepped rather than drawn. Walking the segment at [`CC_PAINT_SAMPLE_PX`]
    /// intervals produces a continuous curve at any drag speed, and the whole
    /// segment is written inside **one** timeline update so a fast drag costs one
    /// notify per mouse event rather than one per sample.
    pub(super) fn cc_paint_stroke_to(
        &mut self,
        lx: f32,
        ly: f32,
        erase: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let kind = self.active_cc;
        let unsnap = match &self.drag {
            PianoDrag::CcPaint { unsnap, .. } => *unsnap,
            _ => false,
        };
        let last = match &self.drag {
            PianoDrag::CcPaint { last, .. } => *last,
            _ => None,
        };
        let (from_x, from_y) = last.unwrap_or((lx, ly));

        let (_, cc_h) = self.cc_view_size();
        // The inverse of the transform the curve is drawn with, so the value
        // written is the one under the pointer.
        let value_at = |y: f32| Self::controller_value_for_y(y, cc_h);

        // One sample per CC_PAINT_SAMPLE_PX of travel, always including both
        // endpoints so the stroke starts and ends exactly under the cursor.
        let dx = lx - from_x;
        let dy = ly - from_y;
        let distance = (dx * dx + dy * dy).sqrt();
        let steps = (distance / CC_PAINT_SAMPLE_PX).ceil().max(1.0) as i32;

        let step_beats = self.step_beats();
        let tol = (step_beats * 0.5).max(1.0e-3);
        // Free drawing has no grid to collapse samples onto, so thin by beat
        // distance instead — otherwise a wide drag would mint one point per
        // sample and bloat the lane far past any useful CC resolution.
        let min_gap = if unsnap || step_beats <= 0.0 {
            (self.x_to_clip_beat(CC_PAINT_SAMPLE_PX) - self.x_to_clip_beat(0.0)).abs()
        } else {
            0.0
        }
        .max(CC_POINT_MERGE_EPS);

        let mut edits: Vec<(f32, f32)> = Vec::with_capacity(steps as usize + 1);
        let mut last_beat: Option<f32> = None;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = from_x + dx * t;
            let y = from_y + dy * t;
            let beat = self.clamp_cc_beat(self.snap_beats_live(self.x_to_clip_beat(x), unsnap));
            // Collapse samples that resolve to the same target (snapped strokes
            // land many pixels on one grid line).
            if let Some(previous) = last_beat {
                if (beat - previous).abs() <= min_gap {
                    // Still let the newest value win at that position.
                    if let Some(entry) = edits.last_mut() {
                        entry.1 = value_at(y);
                    }
                    continue;
                }
            }
            last_beat = Some(beat);
            edits.push((beat, value_at(y)));
        }

        let cursor_value = value_at(ly);
        self.drag_value_status = Some(format!(
            "{}: {}{}",
            cc_kind_label(kind),
            controller_display_value(kind, cursor_value),
            if unsnap { " · free" } else { "" }
        ));

        let existing = self
            .timeline
            .read(cx)
            .state
            .controller_points_snapshot(&clip_id, kind);
        // Snapped strokes should clear the whole grid interval they traverse;
        // freehand strokes use their screen-space sample spacing so a vertical
        // brush still replaces the point directly under the cursor.
        let replace_epsilon = if unsnap { min_gap } else { tol };
        let points = replace_cc_paint_segment(existing, &edits, erase, replace_epsilon);

        self.timeline.update(cx, |tl, tcx| {
            tl.state.set_controller_lane_points(&clip_id, kind, points);
            tcx.notify();
        });

        if let PianoDrag::CcPaint { last, .. } = &mut self.drag {
            *last = Some((lx, ly));
        }
    }

    /// Hit-test the active lane's points: the id of the point **nearest** the
    /// local strip coordinate, within [`CC_HIT_R`].
    ///
    /// Nearest, not first: a drawn curve puts a point on every grid step, so
    /// several handles overlap one hit radius, and taking the leftmost one
    /// grabbed a neighbour of the handle actually under the cursor.
    pub(super) fn cc_point_at(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
        lx: f32,
        ly: f32,
    ) -> Option<u64> {
        let (_, cc_h) = self.cc_view_size();
        let kind = self.active_cc;
        let tl = self.timeline.read(cx);
        let points = tl.state.controller_lane_points(clip_id, kind)?;
        let mut best: Option<(u64, f32)> = None;
        for p in points {
            let dx = lx - self.clip_beat_to_x(p.beat);
            if dx.abs() > CC_HIT_R {
                continue;
            }
            let dy = ly - Self::controller_y_for_value(p.value, cc_h);
            let d2 = dx * dx + dy * dy;
            if d2 <= CC_HIT_R * CC_HIT_R && best.is_none_or(|(_, bd)| d2 < bd) {
                best = Some((p.id, d2));
            }
        }
        best.map(|(id, _)| id)
    }

    /// Begin dragging an existing CC point (and any multi-selection that
    /// contains it). Ctrl/Cmd+click toggles selection without starting a drag
    /// when handled by the lane mouse-down path before this is called.
    pub(super) fn begin_cc_move(
        &mut self,
        id: u64,
        lx: f32,
        ly: f32,
        unsnap: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let kind = self.active_cc;
        if !self.cc_selection.contains(&id) {
            self.cc_selection = HashSet::from([id]);
        }
        let selected = self.cc_selection.clone();
        let prev: Vec<(u64, f32, f32)> = self
            .timeline
            .read(cx)
            .state
            .controller_lane_points(&clip_id, kind)
            .map(|pts| {
                pts.iter()
                    .filter(|p| selected.contains(&p.id))
                    .map(|p| (p.id, p.beat, p.value))
                    .collect()
            })
            .unwrap_or_default();
        // Anchor at the press, not at the point: the handle may be grabbed a
        // few pixels off-centre, and measuring from its centre made it jump
        // to the cursor on the first move.
        let (_, cc_h) = self.cc_view_size();
        let anchor_beat = self.x_to_clip_beat(lx);
        let anchor_value = Self::controller_value_for_y(ly, cc_h);
        self.cc_edit_prev = Some(
            self.timeline
                .read(cx)
                .state
                .controller_points_snapshot(&clip_id, kind),
        );
        self.cc_edit_target = Some((clip_id.clone(), kind));
        let ids: Vec<u64> = prev.iter().map(|(pid, _, _)| *pid).collect();
        self.drag = PianoDrag::CcMove {
            ids,
            prev,
            anchor_beat,
            anchor_value,
            unsnap,
        };
        cx.notify();
    }

    /// Move every selected CC point by the cursor's Δbeat/Δvalue since the
    /// press. Each point's beat snaps on its own (so a point on the grid stays
    /// there until the cursor has travelled half a step) unless Shift/Alt
    /// (`unsnap`) is held.
    pub(super) fn cc_move_selection_to(&mut self, lx: f32, ly: f32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let PianoDrag::CcMove {
            prev,
            anchor_beat,
            anchor_value,
            unsnap,
            ..
        } = &self.drag
        else {
            return;
        };
        let prev = prev.clone();
        let anchor_beat = *anchor_beat;
        let anchor_value = *anchor_value;
        let unsnap = *unsnap;
        let kind = self.active_cc;
        let (_, cc_h) = self.cc_view_size();
        let d_beat = self.x_to_clip_beat(lx) - anchor_beat;
        let d_value = Self::controller_value_for_y(ly, cc_h) - anchor_value;
        let (lo_beat, hi_beat) = self.cc_beat_range();
        self.drag_value_status = Some(if prev.len() == 1 {
            format!(
                "{}: {}",
                cc_kind_label(kind),
                controller_display_value(kind, (prev[0].2 + d_value).clamp(0.0, 1.0))
            )
        } else {
            format!(
                "{} Δbeat {:+.2} · {} pts",
                cc_kind_label(kind),
                d_beat,
                prev.len()
            )
        });
        let moved: Vec<(u64, f32, f32)> = prev
            .iter()
            .map(|(id, beat, value)| {
                let next_beat = self
                    .snap_beats_live(*beat + d_beat, unsnap)
                    .clamp(lo_beat, hi_beat);
                (*id, next_beat, (*value + d_value).clamp(0.0, 1.0))
            })
            .collect();
        self.timeline.update(cx, |tl, tcx| {
            for (id, beat, value) in moved {
                tl.state
                    .set_controller_point(&clip_id, kind, id, beat, value);
            }
            tcx.notify();
        });
    }

    pub(super) fn begin_cc_select(
        &mut self,
        clip_id: String,
        kind: MidiControllerKind,
        lx: f32,
        ly: f32,
        mode: MarqueeSelectionMode,
        cx: &mut Context<Self>,
    ) {
        // The lane spans the whole track like the grid above it, so a press
        // in a neighbouring clip moves the editor there first — otherwise the
        // point would be written into the clip that happens to be selected,
        // at a beat that belongs to a different one.
        if self.retarget_to_clip_under(lx, cx) {
            return;
        }
        self.cc_selection_before_marquee = self.cc_selection.clone();
        self.drag = PianoDrag::CcSelect {
            clip_id,
            kind,
            start_x: lx,
            start_y: ly,
            current_x: lx,
            current_y: ly,
            mode,
            dragging: false,
        };
        cx.notify();
    }

    pub(super) fn update_cc_select(&mut self, lx: f32, ly: f32, cx: &mut Context<Self>) {
        let (clip_id, kind, start_x, start_y, mode, was_dragging) = match &self.drag {
            PianoDrag::CcSelect {
                clip_id,
                kind,
                start_x,
                start_y,
                mode,
                dragging,
                ..
            } => (clip_id.clone(), *kind, *start_x, *start_y, *mode, *dragging),
            _ => return,
        };
        let dx = lx - start_x;
        let dy = ly - start_y;
        let dragging = was_dragging || (dx * dx + dy * dy).sqrt() >= MARQUEE_DRAG_THRESHOLD;
        if let PianoDrag::CcSelect {
            current_x,
            current_y,
            dragging: state_dragging,
            ..
        } = &mut self.drag
        {
            *current_x = lx;
            *current_y = ly;
            *state_dragging = dragging;
        }
        if !dragging {
            return;
        }
        let (view_w, view_h) = self.cc_view_size();
        let rect = Self::normalized_marquee_rect(start_x, start_y, lx, ly, view_w, view_h);
        let hits: HashSet<u64> = self
            .timeline
            .read(cx)
            .state
            .controller_lane_points(&clip_id, kind)
            .map(|points| {
                points
                    .iter()
                    .filter(|point| {
                        let x = self.clip_beat_to_x(point.beat);
                        let y = Self::controller_y_for_value(point.value, view_h);
                        x >= rect.0 && x <= rect.2 && y >= rect.1 && y <= rect.3
                    })
                    .map(|point| point.id)
                    .collect()
            })
            .unwrap_or_default();
        self.cc_selection =
            Self::apply_marquee_mode(&self.cc_selection_before_marquee, &hits, mode);
        cx.notify();
    }

    pub(super) fn delete_selected_cc_points(&mut self, cx: &mut Context<Self>) {
        if self.cc_selection.is_empty() {
            return;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let kind = self.active_cc;
        let prev = self
            .timeline
            .read(cx)
            .state
            .controller_points_snapshot(&clip_id, kind);
        let selected = self.cc_selection.clone();
        let next: Vec<MidiControllerPoint> = prev
            .iter()
            .filter(|point| !selected.contains(&point.id))
            .cloned()
            .collect();
        self.cc_edit_prev = Some(prev);
        self.cc_edit_target = Some((clip_id.clone(), kind));
        self.timeline.update(cx, |timeline, tcx| {
            timeline
                .state
                .set_controller_lane_points(&clip_id, kind, next);
            tcx.notify();
        });
        self.cc_selection.clear();
        self.commit_cc_edit(cx);
        cx.notify();
    }

    pub(super) fn duplicate_selected_cc_points(&mut self, cx: &mut Context<Self>) {
        if self.cc_selection.is_empty() {
            return;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let kind = self.active_cc;
        let prev = self
            .timeline
            .read(cx)
            .state
            .controller_points_snapshot(&clip_id, kind);
        let offset = self.step_beats().max(1.0e-3);
        let mut next = prev.clone();
        let mut new_ids = HashSet::new();
        for point in prev
            .iter()
            .filter(|point| self.cc_selection.contains(&point.id))
        {
            let duplicate = MidiControllerPoint::new(point.beat + offset, point.value);
            new_ids.insert(duplicate.id);
            next.push(duplicate);
        }
        next.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.cc_edit_prev = Some(prev);
        self.cc_edit_target = Some((clip_id.clone(), kind));
        self.timeline.update(cx, |timeline, tcx| {
            timeline
                .state
                .set_controller_lane_points(&clip_id, kind, next);
            tcx.notify();
        });
        self.cc_selection = new_ids;
        self.commit_cc_edit(cx);
        cx.notify();
    }

    /// Generate a shaped CC curve over the selected points' beat span (or one
    /// bar from the click beat when nothing is selected). Replaces points in
    /// that span; commits as one `SetControllerPoints` undo entry.
    pub(super) fn apply_cc_curve(
        &mut self,
        kind: CcCurveKind,
        click_beat: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let controller = self.active_cc;
        let step = self.step_beats().max(1.0e-3);
        let existing = self
            .timeline
            .read(cx)
            .state
            .controller_points_snapshot(&clip_id, controller);
        let selected: Vec<&MidiControllerPoint> = existing
            .iter()
            .filter(|p| self.cc_selection.contains(&p.id))
            .collect();
        let (lo_beat, hi_beat, from, to) = if selected.len() >= 2 {
            let lo = selected
                .iter()
                .map(|p| p.beat)
                .fold(f32::INFINITY, f32::min);
            let hi = selected.iter().map(|p| p.beat).fold(0.0_f32, f32::max);
            let from = selected
                .iter()
                .min_by(|a, b| {
                    a.beat
                        .partial_cmp(&b.beat)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|p| p.value)
                .unwrap_or(0.0);
            let to = selected
                .iter()
                .max_by(|a, b| {
                    a.beat
                        .partial_cmp(&b.beat)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|p| p.value)
                .unwrap_or(1.0);
            (lo, hi.max(lo + step), from, to)
        } else if selected.len() == 1 {
            let p = selected[0];
            (p.beat, p.beat + 4.0, p.value, 1.0 - p.value)
        } else {
            let start = self.snap_beats(click_beat.max(0.0));
            (start, start + 4.0, 0.0, 1.0)
        };

        // Humanize: jitter existing points in-span rather than regenerating.
        let prev = existing.clone();
        let mut points: Vec<MidiControllerPoint> = existing
            .into_iter()
            .filter(|p| p.beat < lo_beat - 1.0e-4 || p.beat > hi_beat + 1.0e-4)
            .collect();
        let mut generated_ids = HashSet::new();

        if kind == CcCurveKind::Humanize {
            for p in prev
                .iter()
                .filter(|p| p.beat >= lo_beat - 1.0e-4 && p.beat <= hi_beat + 1.0e-4)
            {
                let jitter = (CcCurveKind::Humanize.sample(p.beat.fract(), 0.0, 1.0) - 0.5) * 0.12;
                let point = MidiControllerPoint::new(p.beat, (p.value + jitter).clamp(0.0, 1.0));
                generated_ids.insert(point.id);
                points.push(point);
            }
        } else {
            let span = (hi_beat - lo_beat).max(step);
            let count = (span / step).round().max(1.0) as i32;
            for i in 0..=count {
                let beat = (lo_beat + step * i as f32).min(hi_beat);
                let t = if span <= 1.0e-6 {
                    0.0
                } else {
                    (beat - lo_beat) / span
                };
                let value = kind.sample(t, from, to);
                let point = MidiControllerPoint::new(beat, value);
                generated_ids.insert(point.id);
                points.push(point);
            }
        }

        self.cc_edit_prev = Some(prev);
        self.cc_edit_target = Some((clip_id.clone(), controller));
        self.timeline.update(cx, |tl, tcx| {
            tl.state
                .set_controller_lane_points(&clip_id, controller, points);
            tcx.notify();
        });
        self.commit_cc_edit(cx);
        self.cc_selection = generated_ids;
        self.open_cc_curve_menu = None;
        cx.notify();
    }

    /// Begin a Shift+drag ramp: snapshot the lane for undo and anchor the line
    /// at the cursor. The line is rebuilt on every move from the pre-drag points.
    pub(super) fn begin_cc_line(
        &mut self,
        lx: f32,
        ly: f32,
        unsnap: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        // The lane spans the whole track like the grid above it, so a press
        // in a neighbouring clip moves the editor there first — otherwise the
        // point would be written into the clip that happens to be selected,
        // at a beat that belongs to a different one.
        if self.retarget_to_clip_under(lx, cx) {
            return;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let kind = self.active_cc;
        self.timeline.update(cx, |tl, _| {
            tl.state.ensure_controller_lane(&clip_id, kind);
        });
        self.cc_edit_prev = Some(
            self.timeline
                .read(cx)
                .state
                .controller_points_snapshot(&clip_id, kind),
        );
        self.cc_edit_target = Some((clip_id.clone(), kind));
        let anchor_beat = self.clamp_cc_beat(self.snap_beats_live(self.x_to_clip_beat(lx), unsnap));
        let (_, cc_h) = self.cc_view_size();
        let anchor_value = Self::controller_value_for_y(ly, cc_h);
        self.drag = PianoDrag::CcLine {
            anchor_beat,
            anchor_value,
            unsnap,
        };
        self.cc_line_to(anchor_beat, anchor_value, lx, ly, cx);
        cx.notify();
    }

    /// Rebuild the ramp from `anchor` to the cursor: keep pre-drag points outside
    /// the spanned beat range, then lay evenly-spaced points (one per grid step)
    /// along the straight line between the two endpoints.
    pub(super) fn cc_line_to(
        &mut self,
        anchor_beat: f32,
        anchor_value: f32,
        lx: f32,
        ly: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let Some(base) = self.cc_edit_prev.clone() else {
            return;
        };
        let kind = self.active_cc;
        let unsnap = match &self.drag {
            PianoDrag::CcLine { unsnap, .. } => *unsnap,
            _ => false,
        };
        let cur_beat = self.clamp_cc_beat(self.snap_beats_live(self.x_to_clip_beat(lx), unsnap));
        let (_, cc_h) = self.cc_view_size();
        let cur_value = Self::controller_value_for_y(ly, cc_h);
        self.drag_value_status = Some(format!(
            "{} line: {}→{}",
            cc_kind_label(kind),
            controller_display_value(kind, anchor_value),
            controller_display_value(kind, cur_value)
        ));

        // Orient the span left-to-right and pair values with the same orientation.
        let (lo_beat, hi_beat, lo_val, hi_val) = if anchor_beat <= cur_beat {
            (anchor_beat, cur_beat, anchor_value, cur_value)
        } else {
            (cur_beat, anchor_beat, cur_value, anchor_value)
        };
        const EPS: f32 = 1.0e-4;
        let mut points: Vec<MidiControllerPoint> = base
            .into_iter()
            .filter(|p| p.beat < lo_beat - EPS || p.beat > hi_beat + EPS)
            .collect();

        let step = self.step_beats().max(1.0e-3);
        let span = (hi_beat - lo_beat).max(0.0);
        let count = (span / step).round().max(0.0) as i32;
        for i in 0..=count {
            let beat = (lo_beat + step * i as f32).min(hi_beat);
            let t = if span <= 1.0e-6 {
                0.0
            } else {
                (beat - lo_beat) / span
            };
            let value = (lo_val + (hi_val - lo_val) * t).clamp(0.0, 1.0);
            points.push(MidiControllerPoint::new(beat, value));
        }

        self.timeline.update(cx, |tl, tcx| {
            tl.state.set_controller_lane_points(&clip_id, kind, points);
            tcx.notify();
        });
    }

    /// Commit a finished CC gesture as one undoable command (skips no-ops).
    pub(super) fn commit_cc_edit(&mut self, cx: &mut Context<Self>) {
        let Some(prev) = self.cc_edit_prev.take() else {
            self.cc_edit_target = None;
            return;
        };
        let Some((clip_id, kind)) = self.cc_edit_target.take() else {
            return;
        };
        let next = self
            .timeline
            .read(cx)
            .state
            .controller_points_snapshot(&clip_id, kind);
        if prev == next {
            return;
        }
        self.timeline.update(cx, |tl, tcx| {
            tl.record_executed_command(
                EditCommand::SetControllerPoints {
                    clip_id,
                    kind,
                    prev,
                    next,
                },
                tcx,
            );
        });
        if self.midi_editor_sink {
            crate::components::midi_editor_window::midi_editor_debug("edit command committed");
        }
    }

    pub(super) fn controller_y_for_value(value: f32, lane_h: f32) -> f32 {
        let span = (lane_h - 2.0 * CC_VALUE_PAD).max(1.0);
        (1.0 - value.clamp(0.0, 1.0)) * span + CC_VALUE_PAD
    }

    /// Inverse of [`Self::controller_y_for_value`]. Every gesture that turns a
    /// pointer into a value goes through this, so the curve lands under the
    /// cursor instead of up to `CC_VALUE_PAD` pixels away from it.
    pub(super) fn controller_value_for_y(y: f32, lane_h: f32) -> f32 {
        let span = (lane_h - 2.0 * CC_VALUE_PAD).max(1.0);
        (1.0 - (y - CC_VALUE_PAD) / span).clamp(0.0, 1.0)
    }

    /// Follow a CC-lane pointer move from anywhere in the window.
    ///
    /// Registered window-wide by the lane (see `render_cc_lane`) instead of
    /// relying on the editor root's hover-scoped `on_mouse_move`: the lane
    /// sits on the editor's bottom edge, so overshooting it to slam a value
    /// to 0 left the root and froze the stroke a few pixels short. Outside
    /// the lane the value clamps, which is what an overshoot means.
    pub(super) fn on_cc_pointer_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let cc_drag = matches!(
            self.drag,
            PianoDrag::CcPaint { .. }
                | PianoDrag::CcMove { .. }
                | PianoDrag::CcLine { .. }
                | PianoDrag::CcSelect { .. }
        );
        if cc_drag && event.pressed_button.is_none() {
            // The release happened where no up listener saw it; finish the
            // gesture rather than keep drawing with no button held.
            self.end_cc_drag(cx);
            return;
        }
        let Some((lx, ly)) = self.cc_local(event.position) else {
            return;
        };
        match self.drag {
            PianoDrag::CcSelect { .. } => self.update_cc_select(lx, ly, cx),
            PianoDrag::CcPaint { erase, .. } => {
                // Alt is the live "free" modifier: releasing the grid
                // mid-stroke takes effect on the next segment.
                if let PianoDrag::CcPaint { unsnap, .. } = &mut self.drag {
                    *unsnap = event.modifiers.alt;
                }
                self.cc_paint_stroke_to(lx, ly, erase, cx);
            }
            PianoDrag::CcMove { .. } => {
                if let PianoDrag::CcMove { unsnap, .. } = &mut self.drag {
                    *unsnap = event.modifiers.alt || event.modifiers.shift;
                }
                self.cc_move_selection_to(lx, ly, cx);
            }
            PianoDrag::CcLine {
                anchor_beat,
                anchor_value,
                ..
            } => {
                if let PianoDrag::CcLine { unsnap, .. } = &mut self.drag {
                    *unsnap = event.modifiers.alt
                        || (self.tool == PianoTool::Line && event.modifiers.shift);
                }
                self.cc_line_to(anchor_beat, anchor_value, lx, ly, cx);
            }
            PianoDrag::None => {
                // Only the Select tool grabs points with a plain press, so
                // only it previews the grab.
                let (w, h) = self.cc_view_size();
                let inside = lx >= 0.0 && lx <= w && ly >= 0.0 && ly <= h;
                let hover = if inside && self.tool == PianoTool::Select {
                    self.editing_clip_id(cx)
                        .and_then(|cid| self.cc_point_at(cx, &cid, lx, ly))
                } else {
                    None
                };
                if hover != self.cc_hover_point {
                    self.cc_hover_point = hover;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// Finish whichever CC gesture is active. Returns `false` when none was.
    pub(super) fn end_cc_drag(&mut self, cx: &mut Context<Self>) -> bool {
        match std::mem::replace(&mut self.drag, PianoDrag::None) {
            PianoDrag::CcSelect { mode, dragging, .. } => {
                if !dragging && mode == MarqueeSelectionMode::Replace {
                    self.cc_selection.clear();
                }
                self.cc_selection_before_marquee.clear();
                cx.notify();
                true
            }
            PianoDrag::CcPaint { .. } | PianoDrag::CcMove { .. } | PianoDrag::CcLine { .. } => {
                self.drag_value_status = None;
                self.commit_cc_edit(cx);
                cx.notify();
                true
            }
            other => {
                self.drag = other;
                false
            }
        }
    }

    /// The controller curve **and** its point handles.
    ///
    /// The lane samples the envelope once per visible column in a single merged
    /// walk over columns and points (`O(width + points)`, both ordered by beat)
    /// and culls handles outside the strip, then hands that draw-only snapshot
    /// to [`cc_lane_render`]: the GPU painter when the Renderer setting asks
    /// for it and a device is available, the batched GPUI canvas otherwise.
    /// The handles never had click handlers — the lane's own `on_mouse_down`
    /// hit-tests them geometrically via [`Self::cc_point_at`] — so they are
    /// purely visual on both paths.
    pub(super) fn build_cc_curve(&self, cx: &mut Context<Self>, clip_id: &str) -> gpui::AnyElement {
        let (view_w, cc_h) = self.cc_view_size();
        let kind = self.active_cc;
        let default_value = controller_default_value(kind);
        let baseline_y = Self::controller_y_for_value(default_value, cc_h);
        let num_cols = view_w.ceil().max(1.0) as usize;

        let mut samples = Vec::with_capacity(num_cols + 1);
        // Visible handles only — everything scrolled out of the strip is culled
        // before it reaches the painter.
        let mut handles: Vec<CcHandle> = Vec::new();

        {
            let timeline = self.timeline.read(cx);
            let points = timeline
                .state
                .controller_lane_points(clip_id, kind)
                .map(|points| compact_cc_points(points.clone()))
                .unwrap_or_default();

            // Columns advance left-to-right and points are kept sorted by beat,
            // so one cursor over the points serves every column.
            let mut cursor = 0usize;
            for col in 0..=num_cols {
                let beat = self.x_to_clip_beat(col as f32).max(0.0);
                let value = if points.is_empty() {
                    default_value.clamp(0.0, 1.0)
                } else {
                    while cursor + 1 < points.len() && points[cursor + 1].beat <= beat {
                        cursor += 1;
                    }
                    let a = &points[cursor];
                    if beat <= a.beat {
                        // Before the first point: hold its value.
                        a.value
                    } else if cursor + 1 >= points.len() {
                        // After the last point: hold its value.
                        a.value
                    } else {
                        let b = &points[cursor + 1];
                        let span = (b.beat - a.beat).max(1.0e-6);
                        let t = ((beat - a.beat) / span).clamp(0.0, 1.0);
                        (a.value + (b.value - a.value) * t).clamp(0.0, 1.0)
                    }
                };
                samples.push(Self::controller_y_for_value(value, cc_h));
            }

            for p in &points {
                let x = self.clip_beat_to_x(p.beat);
                if x < -HANDLE_R || x > view_w + HANDLE_R {
                    continue;
                }
                handles.push(CcHandle {
                    x,
                    y: Self::controller_y_for_value(p.value, cc_h),
                    selected: self.cc_selection.contains(&p.id),
                });
            }
        }

        let snapshot = CcLaneSnapshot {
            width: view_w,
            height: cc_h,
            scale: self.window_scale,
            samples,
            handles,
            baseline_y,
        };
        if cc_lane_render::wgpu_selected() {
            if let Some(element) = cc_lane_render::render_wgpu(&snapshot, cx) {
                return element;
            }
        }
        cc_lane_render::render_gpui(&snapshot)
    }

    /// Ring around the point a Select-tool press would grab. An overlay on top
    /// of the curve rather than a handle state, so both curve painters show it
    /// without either carrying a hover flag.
    fn cc_hover_ring(&self, cx: &Context<Self>, clip_id: &str) -> Option<gpui::AnyElement> {
        if self.tool != PianoTool::Select || !matches!(self.drag, PianoDrag::None) {
            return None;
        }
        let id = self.cc_hover_point?;
        let (_, cc_h) = self.cc_view_size();
        let point = self
            .timeline
            .read(cx)
            .state
            .controller_lane_points(clip_id, self.active_cc)?
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.beat, p.value))?;
        let x = self.clip_beat_to_x(point.0);
        let y = Self::controller_y_for_value(point.1, cc_h);
        let r = HANDLE_R + 3.0;
        Some(
            div()
                .absolute()
                .left(px(x - r))
                .top(px(y - r))
                .size(px(r * 2.0))
                .rounded_full()
                .border(px(1.5))
                .border_color(Colors::accent_primary())
                .into_any_element(),
        )
    }

    fn build_cc_selection_overlay(&self) -> Option<gpui::AnyElement> {
        let PianoDrag::CcSelect {
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

    /// The CC strip (right column) plus its captured bounds + interaction.
    pub(super) fn render_cc_lane(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
    ) -> impl IntoElement {
        let grid = self.build_velocity_grid();
        let is_empty = self
            .timeline
            .read(cx)
            .state
            .controller_lane_points(clip_id, self.active_cc)
            .is_none_or(|points| points.is_empty());
        let curve = self.build_cc_curve(cx, clip_id);
        // Where the clips divide. The lane spans the track and retargets the
        // editor on a press, so the division has to be visible or the jump has
        // no cause the user can see.
        let (cc_view_w, cc_lane_h) = self.cc_view_size();
        let clip_divisions = crate::components::piano_roll::scope::clip_division_lines(
            &self.scope,
            |beat| self.project_beat_to_x(beat),
            cc_view_w,
            cc_lane_h,
        );
        let value_chip_el = matches!(
            self.drag,
            PianoDrag::CcPaint { .. } | PianoDrag::CcMove { .. } | PianoDrag::CcLine { .. }
        )
        .then(|| value_chip(self.drag_value_status.as_deref().unwrap_or("CC"), 8.0, 8.0));
        let empty_state = is_empty.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .text_color(Colors::text_faint())
                .child(match self.tool {
                    PianoTool::Select => {
                        "Drag a point to move · drag empty space to select · Alt+drag draws free"
                    }
                    PianoTool::Line => "Drag a ramp · Alt or Shift releases the grid",
                    PianoTool::Erase => "Drag across points to erase",
                    _ => {
                        "Drag to draw · Alt+drag draws free (no snap) · Shift-drag line · Right-click a point to delete"
                    }
                })
        });
        let hover_ring = self.cc_hover_ring(cx, clip_id);
        let curve_menu = self.build_cc_curve_menu(cx);
        let selection_overlay = self.build_cc_selection_overlay();
        let cc_bounds = self.cc_bounds.clone();
        let this = cx.weak_entity();
        let canvas = canvas(
            move |bounds, _w, _cx| cc_bounds.set(Some(bounds)),
            move |_b, _r, window, _cx| {
                // Window-wide, not hover-scoped: see `on_cc_pointer_move`.
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    let _ = this.update(cx, |this, cx| this.on_cc_pointer_move(event, cx));
                });
            },
        )
        .absolute()
        .inset_0();
        div()
            .id("piano-cc")
            .h(px(LANE_H))
            .w_full()
            .relative()
            .overflow_hidden()
            .border_t(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel_alt())
            .cursor(gpui::CursorStyle::Crosshair)
            .child(canvas)
            .children(grid)
            .children(clip_divisions)
            .child(curve)
            .children(hover_ring)
            .children(selection_overlay)
            .children(empty_state)
            .children(value_chip_el)
            .children(curve_menu)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_cc_curve_menu = None;
                    this.cc_hover_point = None;
                    let Some((lx, ly)) = this.cc_local(ev.position) else {
                        return;
                    };
                    // Alt is the lane-wide "free" modifier: it releases the
                    // grid for whichever gesture the click starts.
                    let free = ev.modifiers.alt;
                    let toggle = ev.modifiers.control || ev.modifiers.platform;

                    // The Line tool draws a ramp; Shift is retained as the
                    // established temporary line gesture from the Draw tool.
                    if this.tool == PianoTool::Line
                        || (ev.modifiers.shift
                            && !toggle
                            && !matches!(this.tool, PianoTool::Select | PianoTool::Erase))
                    {
                        let unsnap = free || (this.tool == PianoTool::Line && ev.modifiers.shift);
                        this.begin_cc_line(lx, ly, unsnap, window, cx);
                        return;
                    }
                    if this.tool == PianoTool::Erase {
                        this.begin_cc_paint(true, free, lx, ly, window, cx);
                        return;
                    }

                    // Only the Select tool (or Ctrl/Cmd from any tool) grabs
                    // points. The Draw tool used to grab them too, and a drawn
                    // curve has a handle on every grid step — so re-drawing
                    // over it caught a handle on nearly every press and
                    // dragged that one point around instead of painting.
                    let grabs_points = this.tool == PianoTool::Select || toggle;
                    let hit = if grabs_points {
                        this.editing_clip_id(cx)
                            .and_then(|cid| this.cc_point_at(cx, &cid, lx, ly))
                    } else {
                        None
                    };
                    if let Some(id) = hit {
                        if toggle {
                            // Toggle, then fall through into the move so
                            // Ctrl+drag still drags. A Ctrl+click that does
                            // not move commits nothing — `commit_cc_edit`
                            // no-ops when the points are unchanged.
                            if this.cc_selection.remove(&id) {
                                // Nothing left under the cursor to drag.
                                cx.notify();
                                return;
                            }
                            this.cc_selection.insert(id);
                            this.begin_cc_move(id, lx, ly, free, window, cx);
                            return;
                        }
                        if ev.modifiers.shift {
                            this.cc_selection.insert(id);
                            cx.notify();
                            return;
                        }
                        this.begin_cc_move(id, lx, ly, free, window, cx);
                        return;
                    }

                    // Empty space: the Select tool marquees, except that Alt
                    // is free draw in every tool (the gesture the hint
                    // advertises); every other tool paints.
                    if this.tool == PianoTool::Select && !free {
                        let mode = MarqueeSelectionMode::from_modifiers(&ev.modifiers);
                        if let Some(clip_id) = this.editing_clip_id(cx) {
                            this.begin_cc_select(clip_id, this.active_cc, lx, ly, mode, cx);
                        }
                        return;
                    }
                    this.cc_selection.clear();
                    this.begin_cc_paint(false, free, lx, ly, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus, cx);
                    // Right-click on a point deletes it, which is the gesture
                    // this lane was missing: erasing one point otherwise meant
                    // holding Alt and sweeping, and a sweep is a poor way to
                    // remove exactly one. Empty space keeps the curve menu.
                    if !ev.modifiers.alt {
                        if let Some((lx, ly)) = this.cc_local(ev.position) {
                            if let Some(cid) = this.editing_clip_id(cx) {
                                if let Some(id) = this.cc_point_at(cx, &cid, lx, ly) {
                                    this.delete_cc_point(id, cx);
                                    return;
                                }
                            }
                        }
                    }
                    // Alt+right keeps the erase paint sweep; plain right-click on
                    // empty lane space opens the CC curve context menu.
                    if ev.modifiers.alt {
                        if let Some((lx, ly)) = this.cc_local(ev.position) {
                            // Alt already means "free" on this lane, so the
                            // erase stroke follows the cursor rather than the
                            // grid.
                            this.begin_cc_paint(true, true, lx, ly, window, cx);
                        }
                        return;
                    }
                    if let Some((lx, ly)) = this.cc_local(ev.position) {
                        this.open_cc_curve_menu = Some((lx, ly));
                        cx.notify();
                    }
                }),
            )
    }

    fn build_cc_curve_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (lx, ly) = self.open_cc_curve_menu?;
        let click_beat = self.x_to_clip_beat(lx);
        let mut panel = div()
            .absolute()
            .left(px(lx.clamp(4.0, 240.0)))
            .top(px(ly.clamp(4.0, 40.0)))
            .w(px(132.0))
            .max_h(px(LANE_H - 8.0))
            .id("pr-cc-curve-menu")
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
                    .child("Generate Curve"),
            );
        for (i, kind) in CcCurveKind::ALL.iter().enumerate() {
            let kind = *kind;
            panel = panel.child(
                div()
                    .id(("pr-cc-curve", i))
                    .flex()
                    .items_center()
                    .h(px(18.0))
                    .px(px(7.0))
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .text_size(px(10.0))
                    .text_color(Colors::text_secondary())
                    .hover(|s| s.bg(Colors::surface_hover()))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(kind.label())
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.apply_cc_curve(kind, click_beat, cx);
                    })),
            );
        }
        Some(
            deferred(panel.into_any_element())
                .with_priority(PIANO_ROLL_MENU_PRIORITY)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_paint_replaces_the_previous_horizontal_segment() {
        let existing = vec![
            MidiControllerPoint::new(0.0, 0.1),
            MidiControllerPoint::new(0.25, 0.2),
            MidiControllerPoint::new(0.5, 0.3),
            MidiControllerPoint::new(0.75, 0.4),
        ];
        let points = replace_cc_paint_segment(
            existing,
            &[(0.75, 0.9), (0.5, 0.8)],
            false,
            CC_POINT_MERGE_EPS,
        );

        assert_eq!(points.len(), 4);
        assert_eq!(points[2].beat, 0.5);
        assert_eq!(points[2].value, 0.8);
        assert_eq!(points[3].beat, 0.75);
        assert_eq!(points[3].value, 0.9);
        assert!(points
            .windows(2)
            .all(|pair| (pair[1].beat - pair[0].beat) > CC_POINT_MERGE_EPS));
    }

    #[test]
    fn value_for_y_inverts_the_drawn_transform() {
        for lane_h in [60.0_f32, 140.0] {
            for value in [0.0_f32, 0.25, 0.5, 0.9, 1.0] {
                let y = PianoRoll::controller_y_for_value(value, lane_h);
                let back = PianoRoll::controller_value_for_y(y, lane_h);
                assert!((back - value).abs() < 1.0e-5, "{value} -> {y} -> {back}");
            }
            // The inset edges clamp to the extremes, so min and max are
            // reachable without pixel-exact aim (or by overshooting).
            assert_eq!(PianoRoll::controller_value_for_y(0.0, lane_h), 1.0);
            assert_eq!(PianoRoll::controller_value_for_y(-40.0, lane_h), 1.0);
            assert_eq!(PianoRoll::controller_value_for_y(lane_h, lane_h), 0.0);
            assert_eq!(
                PianoRoll::controller_value_for_y(lane_h + 40.0, lane_h),
                0.0
            );
        }
    }

    #[test]
    fn erase_paint_removes_the_whole_reversed_segment() {
        let existing = vec![
            MidiControllerPoint::new(0.0, 0.1),
            MidiControllerPoint::new(0.25, 0.2),
            MidiControllerPoint::new(0.5, 0.3),
            MidiControllerPoint::new(0.75, 0.4),
        ];
        let points = replace_cc_paint_segment(
            existing,
            &[(0.75, 0.0), (0.5, 0.0)],
            true,
            CC_POINT_MERGE_EPS,
        );

        assert_eq!(points.len(), 2);
        assert_eq!(points[0].beat, 0.0);
        assert_eq!(points[1].beat, 0.25);
    }
}
