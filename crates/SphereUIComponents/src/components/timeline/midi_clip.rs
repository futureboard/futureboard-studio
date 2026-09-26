use crate::components::timeline::render::clip_geometry::{
    controller_preview_band_h, controller_preview_cached, midi_controller_default_value,
    midi_controller_kind_label, note_preview_cached, visible_clip_px_range, ControllerPreview,
    NotePreview,
};
use crate::components::timeline::timeline_state::{
    midi_debug_enabled, ClipDragItem, ClipEdge, ClipResizeDrag, ClipState, ClipType,
    MidiControllerKind, TimelineState,
};
use crate::theme::Colors;
use gpui::{
    canvas, div, fill, point, px, size, AppContext, Bounds, InteractiveElement, IntoElement,
    ParentElement, Pixels, StatefulInteractiveElement, Styled,
};

/// Height of the clip's name bar.
const LABEL_H: f32 = 14.0;

/// Narrowest clip that still gets a name bar. Under this the bar shows a
/// character or two of a name nobody can read, at the cost of a measured text
/// node on every clip in the arrangement.
const MIDI_LABEL_MIN_W: f32 = 40.0;

/// Narrowest clip that gets edge resize handles — below it the two 6 px
/// handles would cover the body they sit on.
const MIDI_RESIZE_HANDLE_MIN_W: f32 = 20.0;

pub fn midi_clip(
    clip: &ClipState,
    track_id: &str,
    track_color: gpui::Rgba,
    state: &TimelineState,
    row_height: f32,
    on_select_clip: std::sync::Arc<
        dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_context_menu: Option<
        std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_open_editor: Option<std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>>,
    on_erase_clip: Option<
        std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    erase_target: bool,
) -> impl IntoElement {
    let clip_id = clip.id.clone();
    let drag_clip_id = clip.id.clone();
    let drag_track_id = track_id.to_string();
    let drag_name = clip.name.clone();
    let drag_start_beat = clip.start_beat;
    let selected = state
        .display_selection()
        .selected_clip_ids
        .contains(&clip.id);
    // The shared clip rectangle, the one the marquee hit-tests against.
    let lane_rect = state.clip_lane_rect(clip, row_height);
    let left = lane_rect.left;
    let width = lane_rect.width;

    // Detail by width, for the same reason as an audio clip: the label bar is a
    // measured text node per clip, and a zoomed-out arrangement has the most
    // clips and the least room on each. See `audio_clip::CLIP_STRIP_MIN_W`.
    let show_label = width >= MIDI_LABEL_MIN_W;
    let show_resize_handles = width >= MIDI_RESIZE_HANDLE_MIN_W;
    let label_h = if show_label { LABEL_H } else { 0.0 };

    let pad = lane_rect.top;
    let clip_h = lane_rect.height;
    let note_h = clip_h - label_h; // height for notes preview

    // Draw notes and controller previews with canvases instead of one GPUI element
    // per note. Dense imported MIDI can contain thousands of notes per clip; the
    // canvas path keeps render cost proportional to visible pixels, not event count.
    //
    // The preview geometry is resolved here, during element build, and the canvas
    // closures own only that resolved geometry. Handing them the note/controller
    // vectors instead meant a full clone of every note in the clip on every
    // repaint — and the arrangement repaints on each playback tick.
    let mut note_elements: Vec<gpui::AnyElement> = Vec::new();
    let clip_len = clip.duration_beats;
    if let ClipType::Midi {
        notes,
        controller_lanes,
        ..
    } = &clip.clip_type
    {
        // Notes follow the arrangement's tempo warp, relative to the clip.
        let axis = crate::components::timeline::render::clip_geometry::ClipAxis::new(
            state.viewport.time_warp.clone(),
            clip.start_beat,
            state.viewport.pixels_per_second,
            state.viewport.pixels_per_beat,
        );
        // Only the on-screen slice of the clip needs quads. A clip that spans the
        // whole arrangement is mostly scrolled out of view, and the lane already
        // clips it — building spans for the hidden part was pure waste.
        let visible_px = visible_clip_px_range(left, width, state.viewport.viewport_width);
        // Cached: this is a pass over every note in the clip, and the lane
        // rebuilds on every scroll, zoom and selection. See
        // `clip_geometry::note_preview_cached`.
        let preview = visible_px.and_then(|(px_start, px_end)| {
            note_preview_cached(&clip.id, notes, clip_len, &axis, px_start, px_end)
        });
        let preview_count = preview.as_ref().map(|p| p.note_count).unwrap_or(0);
        if let Some(preview) = preview {
            note_elements.push(
                midi_note_preview_canvas(preview, track_color)
                    .absolute()
                    .inset_0()
                    .into_any_element(),
            );
        }

        let controller_preview =
            controller_preview_cached(&clip.id, controller_lanes, clip_len, &axis, width);
        if let Some(controller_preview) = controller_preview {
            let lane_kinds = controller_preview.lane_kinds.clone();
            let lane_count = lane_kinds.len();
            note_elements.push(
                midi_controller_preview_canvas(controller_preview)
                    .absolute()
                    .inset_0()
                    .into_any_element(),
            );
            if width >= 44.0 && note_h >= 18.0 {
                let band_h = controller_preview_band_h(note_h, lane_count);
                let row_h = (band_h / lane_count as f32).max(4.0);
                let band_top = (note_h - band_h - 1.0).max(1.0);
                for (idx, kind) in lane_kinds.iter().enumerate() {
                    note_elements.push(
                        div()
                            .absolute()
                            .left(px(3.0))
                            .top(px(band_top + idx as f32 * row_h))
                            .text_size(px(7.0))
                            .text_color(Colors::text_faint())
                            .child(midi_controller_kind_label(*kind))
                            .into_any_element(),
                    );
                }
            }
        }

        if midi_debug_enabled() {
            eprintln!(
                "[midi] preview clip={} notes={}/{} len={:.2}",
                clip.id,
                preview_count,
                notes.len(),
                clip_len
            );
        }
    }

    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        clip.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    let on_select = on_select_clip.clone();
    let context_clip_id = clip.id.clone();
    let erase_cb = on_erase_clip.clone();
    let ctx_cb = on_context_menu.clone();
    let clip_for_erase = clip.id.clone();

    // Edge-resize drag payloads. The opposite edge stays fixed; the timeline
    // root resolves the new length from the live cursor (see `resize_clip`).
    let resize_left = ClipResizeDrag {
        clip_id: clip.id.clone(),
        edge: ClipEdge::Left,
        start_beat: clip.start_beat,
        duration_beats: clip.duration_beats,
    };
    let resize_right = ClipResizeDrag {
        clip_id: clip.id.clone(),
        edge: ClipEdge::Right,
        start_beat: clip.start_beat,
        duration_beats: clip.duration_beats,
    };
    let resize_handle_w = crate::components::timeline::audio_clip::clip_resize_handle_w(width);

    div()
        .absolute()
        .left(px(left))
        .top(px(pad))
        .w(px(width))
        .h(px(clip_h))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg({
            let mut c = track_color;
            c.a = 0.12;
            c
        })
        .border(px(1.0))
        .border_color(if erase_target {
            Colors::status_error()
        } else if selected {
            Colors::text_primary()
        } else {
            let mut c = track_color;
            c.a = 0.4;
            c
        })
        .cursor(gpui::CursorStyle::OpenHand)
        .id(("midi-clip", id_num))
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                // Stop the parent lane handler from re-selecting the track and
                // clearing this clip selection — the piano roll edits the
                // selected clip, so selection must survive the click.
                cx.stop_propagation();
                let additive = event.modifiers.control || event.modifiers.platform;
                on_select(
                    &(clip_id.clone(), additive, event.modifiers.alt),
                    window,
                    cx,
                );
                if event.click_count >= 2 {
                    if let Some(open) = on_open_editor.as_ref() {
                        open(window, cx);
                    }
                }
            },
        )
        .on_mouse_down(
            gpui::MouseButton::Right,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if let Some(cb) = ctx_cb.as_ref() {
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    cb(&(context_clip_id.clone(), x, y), window, cx);
                } else if let Some(erase) = erase_cb.as_ref() {
                    erase(&clip_for_erase, window, cx);
                }
            },
        )
        .on_drag(
            ClipDragItem {
                clip_id: drag_clip_id,
                source_track_id: drag_track_id,
                start_beat: drag_start_beat,
            },
            move |_drag, _offset, _window, cx| {
                cx.new(
                    |_| crate::components::timeline::audio_clip::ClipDragPreview {
                        name: drag_name.clone(),
                        color: track_color,
                    },
                )
            },
        )
        .flex()
        .flex_col()
        .justify_between()
        // Notes preview area
        .child(div().flex_1().min_h_0().relative().children(note_elements))
        // Bottom Clip Label bar. Absent on a clip too narrow to read it — see
        // [`MIDI_LABEL_MIN_W`].
        .children(show_label.then(|| {
            div()
                .h(px(LABEL_H))
                .bg(Colors::surface_panel_alt()) // dark bar
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .px(px(6.0))
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(9.0))
                        .min_w(px(0.0))
                        .flex_1()
                        .truncate()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if selected {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(clip.name.clone()),
                )
            // Clip length text intentionally not rendered on the clip body — the
            // name (flex_1) fills the bar, so no gap remains. Duration stays in the
            // model and the inspector; resize/trim handles are unaffected.
        }))
        // Left/right edge resize handles (absolute, on top). Each starts a
        // typed `ClipResizeDrag`; `stop_propagation` keeps the body move-drag
        // and track re-select from also firing on an edge grab.
        .children(show_resize_handles.then(|| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(px(resize_handle_w))
                .cursor(gpui::CursorStyle::ResizeLeft)
                .id(("midi-clip-resize-l", id_num))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(resize_left, |drag, _offset, _window, cx| {
                    cx.new(|_| drag.clone())
                })
        }))
        .children(show_resize_handles.then(|| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(resize_handle_w))
                .cursor(gpui::CursorStyle::ResizeRight)
                .id(("midi-clip-resize-r", id_num))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(resize_right, |drag, _offset, _window, cx| {
                    cx.new(|_| drag.clone())
                })
        }))
}

fn midi_note_preview_canvas(
    preview: std::sync::Arc<NotePreview>,
    track_color: gpui::Rgba,
) -> gpui::Canvas<()> {
    canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let width: f32 = bounds.size.width.into();
            let height: f32 = bounds.size.height.into();
            if width <= 1.0 || height <= 16.0 {
                return;
            }

            let note_area_h = (height - 14.0).max(1.0);
            let span = (note_area_h - 4.0).max(1.0);
            let note_h = if note_area_h < 30.0 { 1.4 } else { 2.0 };
            let y_of = |norm: f32| (1.0 - norm) * span + 1.0;

            let mut note_color = track_color;
            note_color.a = 0.86;

            for column in &preview.columns {
                // `highest_norm` is the top of the span, so it yields the smaller y.
                let top = y_of(column.highest_norm);
                let bottom = y_of(column.lowest_norm) + note_h;
                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(column.x), px(top)),
                        size(px(1.0), px((bottom - top).max(1.0))),
                    ),
                    note_color,
                ));
            }

            for quad in &preview.quads {
                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(quad.x), px(y_of(quad.norm_pitch))),
                        size(px(quad.width), px(note_h)),
                    ),
                    note_color,
                ));
            }
        },
    )
}

fn midi_controller_preview_canvas(preview: std::sync::Arc<ControllerPreview>) -> gpui::Canvas<()> {
    canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let width: f32 = bounds.size.width.into();
            let height: f32 = bounds.size.height.into();
            if width <= 1.0 || height <= 6.0 {
                return;
            }

            let lane_count = preview.lane_kinds.len();
            let band_h = controller_preview_band_h(height, lane_count);
            let row_h = (band_h / lane_count as f32).max(4.0);
            let band_top = (height - band_h - 1.0).max(1.0);
            let usable = (row_h - 2.0).max(1.0);

            for (lane_idx, kind) in preview.lane_kinds.iter().enumerate() {
                let row_top = band_top + lane_idx as f32 * row_h;
                let default_value = midi_controller_default_value(*kind);
                let baseline_y = row_top + (1.0 - default_value) * usable + 1.0;
                let mut line_color = match kind {
                    MidiControllerKind::PitchBend => Colors::accent_purple(),
                    MidiControllerKind::CC(_) => Colors::automation_curve(),
                    MidiControllerKind::ChannelPressure | MidiControllerKind::PolyPressure => {
                        Colors::accent_warning()
                    }
                };
                line_color.a = (0.82 - lane_idx as f32 * 0.14).clamp(0.42, 0.82);
                let mut baseline_color = Colors::text_primary();
                baseline_color.a = 0.12;

                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(0.0), px(baseline_y)),
                        size(px(width), px(1.0)),
                    ),
                    baseline_color,
                ));

                let values = &preview.lane_values[lane_idx];
                let mut prev_y: Option<f32> = None;
                for col in 0..=preview.columns {
                    let x = (col as f32 * preview.step_px).min(preview.width);
                    let y = row_top + (1.0 - values[col]) * usable + 1.0;
                    if let Some(prev) = prev_y {
                        let top = prev.min(y);
                        let h = (prev - y).abs().max(1.4);
                        window.paint_quad(fill(
                            Bounds::new(
                                bounds.origin + point(px(x), px(top)),
                                size(px(preview.step_px), px(h)),
                            ),
                            line_color,
                        ));
                    }
                    prev_y = Some(y);
                }
            }
        },
    )
}
