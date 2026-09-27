//! What a collapsed folder's lane shows: the clips of the tracks folded inside
//! it, so the arrangement still reads while its rows are hidden.
//!
//! One band per member track that has clips, in list order and in that
//! track's colour, each clip at the x its own row would draw it. When the
//! members are too many for readable bands they share one band. The summary
//! is a picture only: presses fall through to the folder lane, which selects
//! the folder like any lane.

use gpui::prelude::FluentBuilder;
use gpui::{div, px, IntoElement, ParentElement, Styled};

use crate::components::timeline::timeline_state::{
    is_arrangement_hidden_track, TimelineState, TrackType,
};
use crate::theme::{radius, space, Colors};

/// Thinnest band that still reads as its own row of clips; below it every
/// member shares one band.
const MIN_BAND_H: f32 = 3.0;
/// Space between two members' bands.
const BAND_GAP: f32 = 1.0;
/// Upper bound on the blocks one summary draws, so a folder of very long
/// tracks cannot turn one lane into thousands of elements.
const MAX_BLOCKS: usize = 1_500;

/// The summary for the folder at `folder_index`, or `None` when it is not a
/// collapsed folder or nothing inside it has clips.
pub fn collapsed_folder_summary(
    folder_index: usize,
    state: &TimelineState,
    row_height: f32,
) -> Option<gpui::AnyElement> {
    let folder = state.tracks.get(folder_index)?;
    if folder.track_type != TrackType::Group || !folder.group_collapsed {
        return None;
    }
    let tree = state.group_tree();
    let members: Vec<_> = (folder_index + 1..tree.block_end(folder_index))
        .map(|index| &state.tracks[index])
        .filter(|track| {
            track.track_type != TrackType::Group
                && !is_arrangement_hidden_track(track)
                && !track.clips.is_empty()
        })
        .collect();
    if members.is_empty() {
        return None;
    }

    let inner_h = (row_height - 2.0 * space::TIGHT).max(MIN_BAND_H);
    let bands = members.len() as f32;
    let per_member = (inner_h - BAND_GAP * (bands - 1.0)) / bands;
    let shared = per_member < MIN_BAND_H;
    let band_h = if shared { inner_h } else { per_member };
    let viewport_w = state.viewport.viewport_width.max(1.0);

    let mut blocks = Vec::new();
    'members: for (band, member) in members.iter().enumerate() {
        let top = space::TIGHT
            + if shared {
                0.0
            } else {
                band as f32 * (band_h + BAND_GAP)
            };
        for clip in &member.clips {
            let (left, width) = state.clip_lane_x_span(clip);
            if left + width < 0.0 || left > viewport_w {
                continue;
            }
            if blocks.len() >= MAX_BLOCKS {
                break 'members;
            }
            // A muted clip (an inactive take among them) is drawn faint, the
            // way its own row draws it.
            let alpha = if clip.muted || member.muted {
                0.22
            } else {
                0.62
            };
            // The lane clips what runs past its edges.
            blocks.push(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(width))
                    .h(px(band_h))
                    .rounded(px(radius::clamped(radius::MICRO, width, band_h)))
                    .bg(Colors::with_alpha(member.color, alpha))
                    // A top edge in the member's full colour, on a band tall
                    // enough to hold one, keeps its neighbouring clips apart.
                    .when(band_h >= 6.0, |block| {
                        block.border_t(px(1.0)).border_color(member.color)
                    }),
            );
        }
    }
    Some(
        div()
            .absolute()
            .inset_0()
            .children(blocks)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{CreateTrackOptions, InputMonitorMode};

    fn add(state: &mut TimelineState, kind: TrackType) -> String {
        state.create_track(CreateTrackOptions {
            track_type: kind,
            name: "t".to_string(),
            color: gpui::rgba(0x3366_99ff),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    #[test]
    fn only_a_collapsed_folder_with_clips_inside_shows_a_summary() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.viewport.viewport_width = 800.0;
        let folder = add(&mut state, TrackType::Group);
        let inner = add(&mut state, TrackType::Group);
        let keys = add(&mut state, TrackType::Midi);
        assert!(state.assign_track_to_group(&keys, &inner));
        assert!(state.assign_track_to_group(&inner, &folder));

        // Open: its members show themselves.
        assert!(collapsed_folder_summary(0, &state, 72.0).is_none());
        state.toggle_group_collapsed(&folder);
        // Collapsed, but nothing inside has clips yet.
        assert!(collapsed_folder_summary(0, &state, 72.0).is_none());
        // A clip two folders down still shows on the outer folder.
        state.create_midi_clip(&keys, 0.0, 4.0).unwrap();
        assert!(collapsed_folder_summary(0, &state, 72.0).is_some());
        // Not on a track that is no folder.
        assert!(collapsed_folder_summary(2, &state, 72.0).is_none());
    }
}
