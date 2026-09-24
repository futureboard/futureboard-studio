#[cfg(test)]
mod arrangement_bench;
pub mod audio_clip;
pub mod audio_import;
pub mod automation_control_lane;
pub mod automation_lane;
pub mod automation_target_picker;
pub mod chord_track;
pub mod floating_tools_bar;
pub mod global_lane_header;
pub mod marker_flag;
pub mod marker_track;
pub mod midi_clip;
pub mod midi_export;
pub mod midi_import;
pub mod playhead;
pub mod region_track;
pub mod render;
pub mod song_text_track;
pub mod state;
pub mod tempo_track;
pub mod time_signature_track;
pub mod timeline;
pub mod timeline_grid;
pub mod timeline_ruler;
pub mod timeline_state;
pub mod timeline_surface;
pub mod track_header;
pub mod track_lane;
pub mod track_lane_view;
pub mod track_list;
pub mod track_resize;
pub mod video_clip;
pub mod vu_meter;
pub mod waveform_cache;
pub mod waveform_canvas;
pub mod waveform_detail;
pub mod waveform_peak_file;
pub mod waveform_samples;

pub use render::{
    TimelineRenderSnapshot, TimelineRenderer, TimelineRendererBackend, TimelineViewport,
};
pub use timeline::{
    resolve_plugin_drop, NewTrackKind, PluginDropTarget, Timeline, TimelineChromeMetrics,
};
