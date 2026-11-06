//! Time stretch in the MIDI editor.
//!
//! With two or more notes selected, a faint frame surrounds them on the grid
//! with a stretch handle on each side (and the ruler marks the span they
//! cover). Dragging a handle stretches the selection about the other end: every start moves proportionally, every length scales with it,
//! and so do the notes' own pitch and expression curves. The grid snaps the
//! dragged edge unless Shift is held, as for a note's end. Alt-dragging the
//! end of a note in the selection does the same from the right.
//!
//! The drag only changes what is drawn (`PianoDrag::Stretch` maps every note
//! through a [`StretchSpec`] in `display_note`); release writes the notes once,
//! as one "Time Stretch" undo step that also grows the clip to fit. The math
//! is `timeline::state::midi_transform::stretch_notes`, the same one the tools
//! column's ×2 / ×½ buttons run.

use super::*;
use crate::components::timeline::timeline_state::{
    note_span, stretch_notes, MAX_STRETCH_FACTOR, MIN_STRETCH_FACTOR,
};

/// A stretch handle beside the selected notes: a small accent tab with a
/// double arrow, wide enough to grab at any zoom.
const HANDLE_W: f32 = 18.0;
const HANDLE_H: f32 = 14.0;
/// Gap between the selection's frame and a handle, so the handle never sits
/// over the end of a note (that zone resizes the note).
const HANDLE_GAP: f32 = 4.0;
const ICON_SIZE: f32 = 12.0;
/// The band between the handles, just above the clip band on the ruler's
/// bottom edge, so the two read as different things.
const BAND_H: f32 = 2.0;
const BAND_BOTTOM: f32 = 3.0;

/// The stretch factor a drag of `dx_px` asks for. The dragged edge snaps to
/// `step` when given; a press that has not moved past the click slop is no
/// stretch, so grabbing an off-grid handle does not snap the notes.
pub(super) fn drag_factor(spec: &StretchSpec, dx_px: f32, ppb: f32, step: Option<f32>) -> f32 {
    let span = spec.last - spec.first;
    if span < MIN_NOTE_BEATS || dx_px.abs() < NOTE_CLICK_SLOP_PX {
        return 1.0;
    }
    let dx_beats = dx_px / ppb.max(0.0001);
    let snap = |beat: f32| match step {
        Some(step) => snap_beat_to_step(beat, step),
        None => beat.max(0.0),
    };
    let factor = match spec.anchor {
        StretchAnchor::Start => {
            let end = snap(spec.last + dx_beats).max(spec.first + MIN_NOTE_BEATS);
            (end - spec.first) / span
        }
        StretchAnchor::End => {
            let start = snap(spec.first + dx_beats).clamp(0.0, spec.last - MIN_NOTE_BEATS);
            (spec.last - start) / span
        }
    };
    factor.clamp(MIN_STRETCH_FACTOR, MAX_STRETCH_FACTOR)
}

/// The footer's readout while a stretch drags.
pub(super) fn stretch_status(spec: &StretchSpec, notes: usize) -> String {
    let (first, last) = spec.stretched_span();
    format!(
        "Stretch ×{:.2} · {:.2} → {:.2} beats · {} note{}",
        spec.factor,
        spec.last - spec.first,
        last - first,
        notes,
        plural(notes)
    )
}

impl PianoRoll {
    /// Start stretching the selection; `anchor` is the edge that stays put.
    pub(super) fn begin_stretch_drag(
        &mut self,
        anchor: StretchAnchor,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.accept_active_tool(cx);
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.selection.clone();
        let span = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .and_then(|notes| note_span(notes, &ids));
        let Some((first, last)) = span else {
            return;
        };
        if last - first < MIN_NOTE_BEATS {
            return;
        }
        self.drag = PianoDrag::Stretch {
            ids,
            start_x: event.position.x.into(),
            spec: StretchSpec {
                first,
                last,
                factor: 1.0,
                anchor,
            },
            unsnap: event.modifiers.shift,
        };
        cx.notify();
    }

    /// Write a finished stretch drag as one undo step.
    pub(super) fn commit_stretch(
        &mut self,
        ids: HashSet<u64>,
        spec: StretchSpec,
        cx: &mut Context<Self>,
    ) {
        if (spec.factor - 1.0).abs() < 1.0e-4 {
            return;
        }
        let ids: Vec<u64> = ids.into_iter().collect();
        self.run_note_tool_on("Time Stretch", ids, cx, move |notes, ids| {
            stretch_notes(notes, ids, spec.factor, spec.anchor)
        });
    }

    /// The tools column's fixed stretches, from the first note's start.
    pub(super) fn tool_stretch(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.run_note_tool("Time Stretch", cx, move |notes, ids| {
            stretch_notes(notes, ids, factor, StretchAnchor::Start)
        });
    }

    /// The span the selection covers, as drawn: stretched while a stretch
    /// drags, and its lowest and highest pitch. `None` for fewer than two
    /// notes: one note's length is its own end handle.
    fn stretch_frame(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
    ) -> Option<((f32, f32), (u8, u8), Option<StretchAnchor>)> {
        let notes = self.timeline.read(cx).state.midi_clip_notes(clip_id)?;
        let (ids, span, dragging) = match &self.drag {
            PianoDrag::Stretch { ids, spec, .. } => (ids, spec.stretched_span(), Some(spec.anchor)),
            _ if self.selection.len() >= 2 => {
                (&self.selection, note_span(notes, &self.selection)?, None)
            }
            _ => return None,
        };
        let pitches = notes.iter().filter(|n| ids.contains(&n.id)).fold(
            None,
            |range: Option<(u8, u8)>, n| {
                Some(match range {
                    None => (n.pitch, n.pitch),
                    Some((lo, hi)) => (lo.min(n.pitch), hi.max(n.pitch)),
                })
            },
        )?;
        Some((span, pitches, dragging))
    }

    /// The selection's span along the ruler's bottom edge, so where a stretch
    /// starts and ends reads against the bar numbers.
    pub(super) fn build_stretch_band(
        &self,
        cx: &mut Context<Self>,
        clip_id: &str,
    ) -> Vec<gpui::AnyElement> {
        let Some(((first, last), _, _)) = self.stretch_frame(cx, clip_id) else {
            return Vec::new();
        };
        let (view_w, _) = self.grid_view_size();
        let x0 = self.clip_beat_to_x(first).max(0.0);
        let x1 = self.clip_beat_to_x(last).min(view_w);
        if x1 <= x0 {
            return Vec::new();
        }
        vec![div()
            .absolute()
            .bottom(px(BAND_BOTTOM))
            .left(px(x0))
            .w(px(x1 - x0))
            .h(px(BAND_H))
            .bg(Colors::with_alpha(Colors::accent_primary(), 0.9))
            .into_any_element()]
    }

    /// On the note grid, the way FL Studio and Ableton show it: a faint frame
    /// around the selected notes and a stretch handle on each side of it,
    /// halfway up. The left handle stretches about the frame's end, the right
    /// one about its start.
    pub(super) fn build_stretch_handles(
        &self,
        cx: &mut Context<Self>,
        clip_id: &str,
    ) -> Vec<gpui::AnyElement> {
        let Some(((first, last), (low, high), dragging)) = self.stretch_frame(cx, clip_id) else {
            return Vec::new();
        };
        let (view_w, view_h) = self.grid_view_size();
        let x0 = self.clip_beat_to_x(first);
        let x1 = self.clip_beat_to_x(last);
        let top = self.pitch_to_y(high);
        let bottom = self.pitch_to_y(low) + self.note_row_h();
        if x1 < -HANDLE_W - HANDLE_GAP
            || x0 > view_w + HANDLE_W + HANDLE_GAP
            || bottom < 0.0
            || top > view_h
        {
            return Vec::new();
        }
        let accent = Colors::accent_primary();
        let mut out = vec![div()
            .absolute()
            .left(px(x0))
            .top(px(top))
            .w(px((x1 - x0).max(1.0)))
            .h(px((bottom - top).max(1.0)))
            .border_1()
            .border_color(Colors::with_alpha(
                accent,
                if dragging.is_some() { 0.7 } else { 0.35 },
            ))
            .bg(Colors::with_alpha(accent, 0.04))
            .into_any_element()];
        // Halfway up the frame, held inside the grid so a tall selection
        // that runs off screen keeps its handles in reach.
        let mid = ((top + bottom) * 0.5).clamp(HANDLE_H * 0.5, (view_h - HANDLE_H * 0.5).max(0.0));
        let max_left = (view_w - HANDLE_W).max(0.0);
        for (index, left, anchor) in [
            (
                0usize,
                (x0 - HANDLE_GAP - HANDLE_W).clamp(0.0, max_left),
                StretchAnchor::End,
            ),
            (
                1,
                (x1 + HANDLE_GAP).clamp(0.0, max_left),
                StretchAnchor::Start,
            ),
        ] {
            let rest = if dragging == Some(anchor) {
                Colors::accent_pressed()
            } else {
                accent
            };
            let hover = Colors::accent_primary_hover();
            out.push(
                div()
                    .id(("pr-stretch-handle", index))
                    .absolute()
                    .left(px(left))
                    .top(px(mid - HANDLE_H * 0.5))
                    .w(px(HANDLE_W))
                    .h(px(HANDLE_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .bg(rest)
                    .shadow(crate::theme::elevation::shadow(
                        crate::theme::elevation::OVERLAY,
                    ))
                    .cursor(gpui::CursorStyle::ResizeLeftRight)
                    .hover(move |s| s.bg(hover))
                    .tooltip(crate::components::controls::fb_tooltip(
                        "Drag to time-stretch the selection · Shift: off the grid",
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.begin_stretch_drag(anchor, ev, window, cx);
                        }),
                    )
                    .child(
                        gpui::svg()
                            .path(assets::ICON_MOVE_HORIZONTAL_PATH)
                            .size(px(ICON_SIZE))
                            .text_color(Colors::on_accent()),
                    )
                    .into_any_element(),
            );
        }
        out
    }
}

#[cfg(feature = "ui-preview")]
impl PianoRoll {
    /// Preview scenes: the first `count` notes selected and caught mid-drag,
    /// stretched by `factor` from the right-hand handle.
    pub fn preview_stretch(&mut self, count: usize, factor: f32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let mut notes: Vec<(f32, u64)> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| notes.iter().map(|n| (n.start, n.id)).collect())
            .unwrap_or_default();
        notes.sort_by(|a, b| a.0.total_cmp(&b.0));
        let ids: HashSet<u64> = notes.into_iter().take(count).map(|(_, id)| id).collect();
        self.selection = ids.clone();
        let span = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .and_then(|notes| note_span(notes, &ids));
        // The first render cancels a gesture begun on another clip; this one
        // was begun on the clip it will render.
        self.last_editing_clip = Some(clip_id.clone());
        if let Some((first, last)) = span {
            self.drag = PianoDrag::Stretch {
                ids,
                start_x: 0.0,
                spec: StretchSpec {
                    first,
                    last,
                    factor,
                    anchor: StretchAnchor::Start,
                },
                unsnap: false,
            };
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(anchor: StretchAnchor) -> StretchSpec {
        StretchSpec {
            first: 2.0,
            last: 6.0,
            factor: 1.0,
            anchor,
        }
    }

    /// At 40 px a beat, dragging the right handle 160 px adds four beats to a
    /// four-beat span: twice as long. The left handle stretches the other way.
    #[test]
    fn a_handle_drag_scales_the_span_about_the_other_end() {
        let factor = drag_factor(&spec(StretchAnchor::Start), 160.0, 40.0, None);
        assert!((factor - 2.0).abs() < 1e-5);
        let factor = drag_factor(&spec(StretchAnchor::End), 40.0, 40.0, None);
        assert!((factor - 0.75).abs() < 1e-5);
    }

    /// The dragged edge lands on the grid, and a press without a move is no
    /// stretch even when that edge is off the grid.
    #[test]
    fn the_dragged_edge_snaps_and_a_click_does_nothing() {
        let off_grid = StretchSpec {
            last: 5.9,
            ..spec(StretchAnchor::Start)
        };
        assert_eq!(drag_factor(&off_grid, 1.0, 40.0, Some(1.0)), 1.0);
        let factor = drag_factor(&off_grid, 50.0, 40.0, Some(1.0));
        assert!((factor - 5.0 / 3.9).abs() < 1e-4, "{factor}");
    }

    /// A handle dragged past the other end stops at the shortest note, and
    /// the left one stops at the clip's start.
    #[test]
    fn a_stretch_cannot_fold_over_or_leave_the_clip() {
        let factor = drag_factor(&spec(StretchAnchor::Start), -1000.0, 40.0, None);
        assert!((factor - (MIN_NOTE_BEATS / 4.0).max(MIN_STRETCH_FACTOR)).abs() < 1e-4);
        let factor = drag_factor(&spec(StretchAnchor::End), -1000.0, 40.0, None);
        assert!((factor - 1.5).abs() < 1e-5);
    }
}
