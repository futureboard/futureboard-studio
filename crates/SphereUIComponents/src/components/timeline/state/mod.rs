//! Timeline arrangement state, split by domain.
//!
//! Extracted from the former monolithic `timeline_state.rs`. Public items are
//! re-exported flat so existing `timeline_state::*` imports keep working through
//! the shim at `super::timeline_state`.

mod accent;
mod articulation;
mod audio;
mod automation;
mod chord_track;
mod clip;
mod core;
mod debug;
mod demo;
mod drag;
mod geometry;
mod global_lanes;
mod grid;
mod hit_test;
mod ids;
mod marker;
mod midi;
mod midi_channel;
mod midi_controller;
mod midi_scale;
mod mixer;
mod mixer_tree_state;
mod musical_snap;
mod pitch_curve;
mod pitch_trajectory;
mod plugin_chain;
mod recording;
mod routing;
mod selection;
mod song_text;
mod stretch;
mod sysex;
mod take;
mod tempo;
mod time_display;
mod time_signature;
mod track;
mod track_row_layout;
mod video;
mod viewport;

#[cfg(test)]
mod pitch_bench;
#[cfg(test)]
mod pitch_stroke_bench;
#[cfg(test)]
mod tests;

pub use accent::*;
pub use articulation::*;
pub use automation::*;
pub use chord_track::*;
pub use clip::*;
pub use core::*;
pub use debug::*;
pub use drag::*;
pub use geometry::*;
pub use global_lanes::*;
pub use grid::*;
pub use hit_test::*;
pub use ids::*;
pub use marker::*;
pub use midi::*;
pub use midi_channel::*;
pub use midi_controller::*;
pub use midi_scale::*;
pub use mixer::*;
pub use mixer_tree_state::*;
pub use musical_snap::{
    beats_to_ticks, multi_select_move_delta, snap_beat_with_grab_offset, snap_relative_delta,
    snap_resize_edge, ticks_to_beats, MusicalSnap, SnapShape, TICKS_PER_QUARTER,
};
pub use pitch_curve::*;
pub use pitch_trajectory::*;
pub use plugin_chain::*;
pub use routing::*;
pub use selection::*;
pub use song_text::*;
pub use stretch::*;
pub use take::*;
pub use tempo::*;
pub use time_display::*;
pub use time_signature::*;
pub use track::*;
pub use track_row_layout::*;
pub use viewport::*;

// `audio`, `recording`, `video`, and `demo` only contribute `impl TimelineState`
// methods (no nameable items), so they need no `pub use` re-export.
