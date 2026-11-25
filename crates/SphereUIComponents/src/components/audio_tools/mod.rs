//! Floating audio-editor tool windows.

mod canvas;
mod graph;
mod module_list;
mod preview;
mod repair;
mod viz;
mod window;
mod workspace;

#[cfg(feature = "ui-preview")]
pub(crate) use window::open_preview_audio_tool_window;

pub use preview::{apply_preview_to_clip, apply_previews_to_snapshot, ClipPreviewOverride};
pub use window::{
    open_audio_tool_window, AudioToolCommand, AudioToolWindow, AudioToolWindowCallbacks,
    AudioToolWindowManager, AUDIO_TOOL_WINDOW_MIN_HEIGHT, AUDIO_TOOL_WINDOW_MIN_WIDTH,
};
