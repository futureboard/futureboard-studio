//! MKFF Player engine: threaded, A/V-synchronised playback of any file `mkff-codecs` opens.
//!
//! The engine has no UI dependency; the `mkff-player` binary puts an egui / wgpu front end on it,
//! and the integration tests drive it headless.
//!
//! ```text
//!              ┌── video thread ── MediaFile::video_frame ──► Picture queue ──┐
//! MediaFile ───┤                                                             ├─► Player::update (UI thread)
//!              └── audio thread ── MediaFile::audio ──► SPSC ring ──► device callback
//!                                                                  │
//!                                                     Clock ◄──────┘ (audio master)
//! ```
//!
//! - **Clock**: the audio device is the master clock when there is audio; otherwise (no audio
//!   stream, no device, `--no-audio`) a monotonic system clock. Seeks and speed changes start a
//!   new *epoch*: the device callback drops stale samples, the clock restarts at the target.
//! - **Video**: decoded ahead into a bounded queue; [`Player::update`] presents the latest picture
//!   due at the clock time and drops the ones it skipped. When decoding falls behind, the video
//!   thread jumps forward to the clock instead of drifting.
//! - **Audio**: resampled to the device rate by `mkff-codecs`, mapped to the device's channels,
//!   played through a lock-free ring buffer with click-free volume ramps.

pub mod audio;
pub mod clock;
pub mod color;
pub mod picture;
pub mod player;
pub mod time;
mod video;

pub use audio::AudioOutput;
pub use color::yuv_to_rgb_matrix;
pub use mkff_codecs::{MediaFile, MediaInfo};
pub use picture::{Picture, PictureData};
pub use player::{PlaybackState, Player, PlayerOptions, Stats};

/// Playback speeds the player offers (audio is resampled, so pitch follows speed).
pub const SPEEDS: [f64; 8] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// Open a media file (index only; samples are read on demand). This can take a moment for large
/// files, so front ends call it off the UI thread and hand the result to [`Player::new`].
pub fn open_media(path: &std::path::Path) -> Result<MediaFile, String> {
    let media = MediaFile::open(path).map_err(|e| e.to_string())?;
    let info = media.info();
    if info.video.is_none() && info.audio.is_empty() {
        return Err(format!("{}: no audio or video stream", path.display()));
    }
    Ok(media)
}
