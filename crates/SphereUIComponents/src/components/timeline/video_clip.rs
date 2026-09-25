//! Arrangement rendering for a reference video clip.
//!
//! Deliberately the thinnest of the three clip renderers. An audio clip draws a
//! waveform and a MIDI clip draws its notes because those are the clip's
//! content; a video clip's content is a picture that belongs in the Video Player
//! window, not smeared across a 60-pixel lane. The lane shows placement — where
//! the reference starts, how long it runs, whether it is selected — and the
//! window shows the frame.
//!
//! Geometry, drag payloads, and resize handles match [`super::midi_clip`] so a
//! video clip moves and trims with exactly the same gestures as every other
//! clip.

use crate::components::timeline::timeline_state::{
    ClipDragItem, ClipEdge, ClipResizeDrag, ClipState, ClipType, TimelineState,
};
use crate::{assets, theme::Colors};
use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, AppContext, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled,
};

/// Width of the invisible left/right edge-resize strips, in pixels. Matches the
/// audio and MIDI clips so trim feels identical across clip types.
const RESIZE_HANDLE_W: f32 = 6.0;

/// Below this width the film icon is dropped so the name keeps the whole bar.
const ICON_MIN_CLIP_W: f32 = 56.0;

pub fn video_clip(
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
    let context_clip_id = clip.id.clone();
    let clip_for_erase = clip.id.clone();
    let drag_clip_id = clip.id.clone();
    let drag_track_id = track_id.to_string();
    let drag_name = clip.name.clone();
    let drag_start_beat = clip.start_beat;

    let selected = state.selection.selected_clip_ids.contains(&clip.id);
    let unresolved = matches!(
        &clip.clip_type,
        ClipType::Video {
            source_path: None,
            ..
        }
    );

    // The shared clip rectangle, the one the marquee hit-tests against.
    let lane_rect = state.clip_lane_rect(clip, row_height);
    let left = lane_rect.left;
    let width = lane_rect.width;

    let pad = lane_rect.top;
    let clip_h = lane_rect.height;

    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        clip.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    let on_select = on_select_clip.clone();
    let ctx_cb = on_context_menu.clone();
    let erase_cb = on_erase_clip.clone();

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

    let label_color = if unresolved {
        Colors::text_faint()
    } else if selected {
        Colors::text_primary()
    } else {
        Colors::text_secondary()
    };

    div()
        .absolute()
        .left(px(left))
        .top(px(pad))
        .w(px(width))
        .h(px(clip_h))
        .rounded(px(crate::theme::radius::CONTROL))
        .overflow_hidden()
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
        .id(("video-clip", id_num))
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                // Keep the lane handler from re-selecting the track and clearing
                // this selection — the Video Player follows the selected clip.
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
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(6.0))
        .when(width >= ICON_MIN_CLIP_W, |this| {
            this.child(
                svg()
                    .path(assets::ICON_FILM_PATH)
                    .size(px(11.0))
                    .flex_none()
                    .text_color(Colors::with_alpha(track_color, 0.85)),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(label_color)
                // An unresolved clip says so rather than showing a name that
                // implies a file the player cannot open.
                .child(if unresolved {
                    format!("{} (missing)", clip.name)
                } else {
                    clip.name.clone()
                }),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(px(RESIZE_HANDLE_W))
                .cursor(gpui::CursorStyle::ResizeLeft)
                .id(("video-clip-resize-l", id_num))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(resize_left, |drag, _offset, _window, cx| {
                    cx.new(|_| drag.clone())
                }),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(RESIZE_HANDLE_W))
                .cursor(gpui::CursorStyle::ResizeRight)
                .id(("video-clip-resize-r", id_num))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(resize_right, |drag, _offset, _window, cx| {
                    cx.new(|_| drag.clone())
                }),
        )
}
