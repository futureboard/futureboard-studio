//! Video Player — the preview/reference window for the arrangement Video track.
//!
//! What this is: a picture monitor. It shows the frame of the Video track's
//! reference video that lines up with the transport playhead, so audio can be
//! written against picture. What it is **not**: a playback engine. It decodes no
//! audio, owns no clock, and never drives the transport — the transport drives
//! it. `sphere_video_player` does the decoding on its own worker thread using
//! the OS media framework (no FFmpeg); see that crate's docs.
//!
//! Like [`crate::components::MixerWindow`] and the routing matrix, this view
//! renders from a pushed snapshot and never reads `StudioLayout` during
//! `Render`, so it cannot re-enter a GPUI entity the studio is already updating.
//!
//! Frame flow, once per studio transport tick:
//!
//! 1. the studio pushes [`VideoPlayerSnapshot`] (source path + playhead seconds);
//! 2. this window forwards the position to the decoder worker and returns —
//!    nothing here blocks on decode;
//! 3. a low-rate poll task notices the worker published a newer frame, uploads
//!    it once as a GPUI image, and notifies.
//!
//! Step 3 is what keeps decode cost off the render thread: `Render` only ever
//! draws an image that is already resident.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, px, size, App, AppContext, Bounds, Context, FocusHandle, ImageSource,
    InteractiveElement, IntoElement, ObjectFit, ParentElement, Render, RenderImage, Styled,
    StyledImage, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};
use sphere_video_player::{PreviewStatus, VideoInfo, VideoPreview, VideoPreviewFrame};

use crate::components::title_bar::external_window_titlebar;
use crate::theme::Colors;
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const VIDEO_PLAYER_WINDOW_WIDTH: f32 = 640.0;
pub const VIDEO_PLAYER_WINDOW_HEIGHT: f32 = 420.0;
pub const VIDEO_PLAYER_WINDOW_MIN_WIDTH: f32 = 320.0;
pub const VIDEO_PLAYER_WINDOW_MIN_HEIGHT: f32 = 240.0;

/// How often the window checks whether the decoder published a newer frame.
/// This is an atomic load, not a decode — 30 Hz costs nothing and is finer than
/// any reference video's frame rate needs for scrubbing feedback.
const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(33);

const STATUS_BAR_H: f32 = 26.0;

/// What the studio pushes each time the transport moves or the Video track's
/// media changes.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoPlayerSnapshot {
    /// Absolute path of the video under the playhead. `None` when the Video
    /// track is empty, or the playhead sits outside every video clip.
    pub source_path: Option<PathBuf>,
    /// Position **within the source media**, in seconds. The studio resolves
    /// this from the clip's timeline placement and offset, so trimming or moving
    /// the clip moves the picture with it.
    pub source_seconds: f64,
    /// Playhead position on the arrangement, for the timecode readout.
    pub timeline_seconds: f64,
}

impl Default for VideoPlayerSnapshot {
    fn default() -> Self {
        Self {
            source_path: None,
            source_seconds: 0.0,
            timeline_seconds: 0.0,
        }
    }
}

pub struct VideoPlayerWindow {
    snapshot: VideoPlayerSnapshot,
    /// Decoder worker for [`VideoPlayerSnapshot::source_path`]. Replaced (which
    /// stops the old worker) whenever the path changes; `None` when there is no
    /// video under the playhead.
    preview: Option<VideoPreview>,
    /// Most recently uploaded frame, and the decoder revision it came from.
    /// Comparing revisions is what stops a re-upload per tick.
    image: Option<Arc<RenderImage>>,
    image_revision: u64,
    /// Frames replaced since the last render pass. Each one still owns a sprite
    /// atlas tile — 33 MB apiece for a 4K source — and GPUI only frees a tile on
    /// an explicit `drop_image`, which needs a `Window`. Uploading without this
    /// leaks every frame the player ever showed.
    stale_images: Vec<Arc<RenderImage>>,
    frame_size: Option<(u32, u32)>,
    info: Option<VideoInfo>,
    status: PreviewStatus,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    focus_handle: FocusHandle,
    _poll_task: gpui::Task<()>,
}

impl VideoPlayerWindow {
    pub fn new(
        snapshot: VideoPlayerSnapshot,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME_POLL_INTERVAL).await;
                let updated = this.update(cx, |this, cx| {
                    if this.adopt_new_frame() {
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    // The window closed; stop polling.
                    return;
                }
            }
        });

        let mut window = Self {
            snapshot: VideoPlayerSnapshot::default(),
            preview: None,
            image: None,
            image_revision: 0,
            stale_images: Vec::new(),
            frame_size: None,
            info: None,
            status: PreviewStatus::Ready,
            on_close,
            focus_handle: cx.focus_handle(),
            _poll_task: poll_task,
        };
        window.set_snapshot(snapshot);
        window
    }

    /// Applies a pushed snapshot: opens/closes the decoder when the media
    /// changes, then forwards the requested position. Never blocks.
    ///
    /// Returns `true` when the snapshot actually differed, so a caller pushing
    /// on every transport tick can skip the repaint when nothing moved.
    pub fn set_snapshot(&mut self, snapshot: VideoPlayerSnapshot) -> bool {
        // The studio pushes on every transport tick; an unchanged position means
        // there is nothing to request and nothing to repaint.
        if snapshot == self.snapshot {
            return false;
        }
        let media_changed =
            snapshot.source_path.as_deref() != self.preview.as_ref().map(|preview| preview.path());

        if media_changed {
            // Dropping the old preview stops its worker and closes the file.
            self.preview = None;
            // The outgoing picture still holds its atlas tile until a render
            // pass can free it.
            if let Some(previous) = self.image.take() {
                self.stale_images.push(previous);
            }
            self.image_revision = 0;
            self.frame_size = None;
            self.info = None;
            self.status = PreviewStatus::Ready;

            if let Some(path) = snapshot.source_path.clone() {
                let preview = VideoPreview::open(path);
                preview.request_position(snapshot.source_seconds);
                self.preview = Some(preview);
                self.status = PreviewStatus::Opening;
            }
        } else if let Some(preview) = &self.preview {
            preview.request_position(snapshot.source_seconds);
        }

        self.snapshot = snapshot;
        true
    }

    /// Uploads the decoder's newest frame if it is newer than the one on
    /// screen. Returns `true` when anything changed and the view needs a
    /// repaint. Called from the poll task only.
    fn adopt_new_frame(&mut self) -> bool {
        let Some(preview) = &self.preview else {
            return false;
        };

        let mut changed = false;

        let status = preview.status();
        if status != self.status {
            self.status = status;
            changed = true;
        }
        if self.info.is_none() {
            if let Some(info) = preview.info() {
                self.info = Some(info);
                changed = true;
            }
        }

        if let Some((revision, frame)) = preview.frame_if_newer(self.image_revision) {
            if let Some(image) = render_image_from_frame(&frame) {
                self.frame_size = Some((frame.width, frame.height));
                if let Some(previous) = self.image.replace(Arc::new(image)) {
                    self.stale_images.push(previous);
                }
                self.image_revision = revision;
                changed = true;
            }
        }

        changed
    }

    /// One-line description of what the player is currently doing, shown in the
    /// status bar. Never invents a state — an unsupported build and a failed
    /// open read differently.
    fn status_text(&self) -> String {
        if self.snapshot.source_path.is_none() {
            return "No video under the playhead".to_string();
        }
        match &self.status {
            PreviewStatus::Opening => "Opening video…".to_string(),
            PreviewStatus::Unsupported => format!(
                "Video preview is not available on this platform ({})",
                sphere_video_player::backend_name()
            ),
            PreviewStatus::Failed(message) => message.clone(),
            PreviewStatus::Ready => match (self.info, self.frame_size) {
                (Some(info), Some((width, height))) => {
                    if info.frame_rate > 0.0 {
                        format!(
                            "{width}×{height} · {:.2} fps · {}",
                            info.frame_rate, info.decoder
                        )
                    } else {
                        format!("{width}×{height} · {}", info.decoder)
                    }
                }
                _ => "Decoding…".to_string(),
            },
        }
    }
}

/// Converts a decoded BGRA frame into a GPUI image.
///
/// `RenderImage` consumes BGRA, which is exactly what the decoder produces, so
/// this is a single move of the frame's bytes with no per-pixel conversion.
fn render_image_from_frame(frame: &VideoPreviewFrame) -> Option<RenderImage> {
    let expected = frame.width as usize * frame.height as usize * 4;
    if frame.width == 0 || frame.height == 0 || frame.bgra.len() < expected {
        return None;
    }
    let buffer = image::ImageBuffer::from_raw(frame.width, frame.height, frame.bgra.clone())?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}

/// `HH:MM:SS.mmm` — the form that reads against a video reference.
fn format_timecode(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let total_millis = (seconds * 1000.0).round() as u64;
    let millis = total_millis % 1000;
    let total_seconds = total_millis / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        total_seconds / 3600,
        (total_seconds / 60) % 60,
        total_seconds % 60,
        millis
    )
}

impl Render for VideoPlayerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Superseded frames still hold a sprite atlas tile; this is the first
        // point in the frame where a `Window` exists to free them. Without it a
        // 4K reference video grows the atlas by 33 MB per decoded frame.
        for image in self.stale_images.drain(..) {
            cx.drop_image(image, Some(window));
        }

        if !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window, cx);
        }

        let on_close = self.on_close.clone();

        // Letterboxed picture area. `ObjectFit::Contain` on a black field is the
        // correct presentation for a reference monitor: it never crops and never
        // distorts, so what is on screen is what is in the file.
        let picture = div()
            .flex_1()
            .min_h_0()
            .bg(gpui::rgb(0x000000))
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .children(self.image.clone().map(|image| {
                img(ImageSource::Render(image))
                    .object_fit(ObjectFit::Contain)
                    .size_full()
            }))
            .children((self.image.is_none()).then(|| {
                div()
                    .text_size(px(11.0))
                    .text_color(Colors::text_faint())
                    .child(self.status_text())
            }));

        let status_bar = div()
            .h(px(STATUS_BAR_H))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .bg(Colors::surface_panel())
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(Colors::text_muted())
                    .child(self.status_text()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(Colors::text_secondary())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(format_timecode(self.snapshot.timeline_seconds)),
            );

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .child(external_window_titlebar(
                "Video Player",
                "video-player-close",
                move |window, app| on_close(window, app),
            ))
            .child(picture)
            .child(status_bar)
    }
}

pub fn open_video_player_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    snapshot: VideoPlayerSnapshot,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<VideoPlayerWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(
            px(VIDEO_PLAYER_WINDOW_WIDTH),
            px(VIDEO_PLAYER_WINDOW_HEIGHT),
        ),
        cx,
    );
    // A picture monitor is watched *while* working the arrangement, so it is an
    // independent top-level window (taskbar entry, minimize, resize), not a
    // dialog owned by the studio. `Floating` additionally keeps it above the
    // studio window, so it cannot be buried by the surface being scored to.
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(VIDEO_PLAYER_WINDOW_MIN_WIDTH),
        px(VIDEO_PLAYER_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| VideoPlayerWindow::new(snapshot, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_reads_as_hours_minutes_seconds_millis() {
        assert_eq!(format_timecode(0.0), "00:00:00.000");
        assert_eq!(format_timecode(1.5), "00:00:01.500");
        assert_eq!(format_timecode(61.25), "00:01:01.250");
        assert_eq!(format_timecode(3661.0), "01:01:01.000");
    }

    #[test]
    fn negative_positions_clamp_rather_than_wrapping() {
        assert_eq!(format_timecode(-5.0), "00:00:00.000");
    }

    #[test]
    fn a_short_frame_buffer_is_rejected_instead_of_reading_past_it() {
        let frame = Arc::new(sphere_video_player::VideoFrame {
            width: 4,
            height: 4,
            timestamp_seconds: 0.0,
            bgra: vec![0u8; 4 * 4 * 4 - 1],
        });
        assert!(render_image_from_frame(&frame).is_none());
    }

    #[test]
    fn a_well_formed_frame_converts() {
        let frame = Arc::new(sphere_video_player::VideoFrame {
            width: 2,
            height: 2,
            timestamp_seconds: 0.0,
            bgra: vec![0u8; 2 * 2 * 4],
        });
        assert!(render_image_from_frame(&frame).is_some());
    }
}
