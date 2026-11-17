//! The playback controller the front end drives.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mkff_codecs::{MediaFile, MediaInfo};

use crate::audio::{AudioEngine, AudioOutput};
use crate::clock::Clock;
use crate::picture::Picture;
use crate::video::{VideoWorker, Waker};

/// Longest the clock waits for the first picture after a seek.
const PREROLL_TIMEOUT: Duration = Duration::from_secs(2);

/// How a [`Player`] starts.
#[derive(Clone)]
pub struct PlayerOptions {
    pub audio: AudioOutput,
    /// Audio stream to play (index into [`MediaInfo::audio`]).
    pub audio_stream: usize,
    /// 0..=1 (perceptual: the gain is its square).
    pub volume: f32,
    pub muted: bool,
    pub speed: f64,
    pub looping: bool,
    /// Start position in seconds.
    pub start: f64,
    /// Start playing at once (otherwise paused on the first frame).
    pub autoplay: bool,
    /// Called from the decode threads when a new picture is ready (e.g. to request a repaint).
    pub waker: Option<Waker>,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        PlayerOptions {
            audio: AudioOutput::Device,
            audio_stream: 0,
            volume: 1.0,
            muted: false,
            speed: 1.0,
            looping: false,
            start: 0.0,
            autoplay: true,
            waker: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    /// Reached the end (not looping). `play` starts over.
    Ended,
}

/// Playback counters for a statistics overlay.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub decoded: u64,
    pub presented: u64,
    /// Decoded pictures that were never shown (late).
    pub dropped: u64,
    /// Frames the decoder skipped to catch up.
    pub skipped: u64,
    pub video_errors: u64,
    /// Smoothed video decode time per frame (ms).
    pub decode_ms: f64,
    pub queued: usize,
    /// Clock minus the shown picture's time (seconds): positive when video lags.
    pub av_offset: f64,
    pub audio_master: bool,
    pub audio_device: Option<String>,
    pub audio_rate: u32,
    pub audio_channels: u16,
    pub audio_underruns: u64,
    pub audio_errors: u64,
}

/// One open media file, playing.
///
/// Call [`Player::update`] once per UI frame: it advances the presented picture to the clock and
/// handles the end of the media. Everything else is a command. Decoding runs on two background
/// threads that stop when the `Player` is dropped.
pub struct Player {
    info: MediaInfo,
    duration: f64,
    fps: f64,
    frame_count: u64,
    clock: Arc<Clock>,
    video: Option<VideoWorker>,
    audio: Option<AudioEngine>,
    current: Option<Arc<Picture>>,
    state: PlaybackState,
    volume: f32,
    muted: bool,
    speed: f64,
    looping: bool,
    presented: u64,
    dropped: u64,
    events: Vec<String>,
    video_failed_reported: bool,
    /// Since when the clock waits for the first picture of a seek.
    preroll: Option<Instant>,
    present_ahead: f64,
}

impl Player {
    /// Start playback threads for `media`. Never fails for lack of audio: without a usable
    /// device, video plays against the system clock (and [`Player::take_events`] says why).
    pub fn new(media: MediaFile, opts: PlayerOptions) -> Player {
        let info = media.info().clone();
        let (fps, frame_count) = match &info.video {
            Some(v) if v.frame_rate.0 > 0 && v.frame_rate.1 > 0 => (
                v.frame_rate.0 as f64 / v.frame_rate.1 as f64,
                media.frame_count(),
            ),
            Some(_) => (25.0, media.frame_count()),
            None => (25.0, 0),
        };
        let duration = if info.duration_seconds.is_finite() && info.duration_seconds > 0.0 {
            info.duration_seconds
        } else {
            frame_count as f64 / fps
        };

        let mut events = Vec::new();
        let audio = if info.audio.is_empty() || opts.audio == AudioOutput::Disabled {
            None
        } else {
            let stream = opts.audio_stream.min(info.audio.len() - 1);
            match AudioEngine::start(media.clone(), stream, opts.audio, duration) {
                Ok(a) => Some(a),
                Err(e) => {
                    log::warn!("audio unavailable: {e}");
                    events.push(format!("No audio: {e}"));
                    None
                }
            }
        };
        let clock = Arc::new(Clock::new(audio.as_ref().map(|a| Arc::clone(&a.shared))));
        let video = if frame_count > 0 {
            match VideoWorker::start(media, fps, frame_count, Arc::clone(&clock), opts.waker) {
                Ok(v) => Some(v),
                Err(e) => {
                    events.push(format!("No video: {e}"));
                    None
                }
            }
        } else {
            None
        };

        let mut player = Player {
            info,
            duration,
            fps,
            frame_count,
            clock,
            video,
            audio,
            current: None,
            state: PlaybackState::Paused,
            volume: opts.volume.clamp(0.0, 1.0),
            muted: opts.muted,
            speed: sanitize_speed(opts.speed),
            looping: opts.looping,
            presented: 0,
            dropped: 0,
            events,
            video_failed_reported: false,
            preroll: None,
            present_ahead: 0.0,
        };
        player.apply_gain();
        player.restart_at(opts.start.clamp(0.0, duration));
        if opts.autoplay {
            player.play();
        }
        player
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// Video frame rate (25 when there is no video).
    pub fn frame_rate(&self) -> f64 {
        self.fps
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn is_playing(&self) -> bool {
        self.state == PlaybackState::Playing
    }

    /// Current media time in seconds, within `0..=duration`.
    pub fn position(&self) -> f64 {
        if self.state == PlaybackState::Ended {
            return self.duration;
        }
        self.clock.now().clamp(0.0, self.duration)
    }

    /// The picture to show now.
    pub fn current_picture(&self) -> Option<&Arc<Picture>> {
        self.current.as_ref()
    }

    pub fn play(&mut self) {
        if self.state == PlaybackState::Ended {
            self.restart_at(0.0);
        }
        self.state = PlaybackState::Playing;
        self.apply_running();
    }

    pub fn pause(&mut self) {
        if self.state == PlaybackState::Playing {
            self.state = PlaybackState::Paused;
        }
        self.apply_running();
    }

    /// Whether playback waits for the first picture after a seek.
    pub fn is_buffering(&self) -> bool {
        self.preroll.is_some()
    }

    /// Run the clock and audio when playing and not prerolling.
    fn apply_running(&self) {
        let run = self.state == PlaybackState::Playing && self.preroll.is_none();
        self.clock.set_playing(run);
        if let Some(a) = &self.audio {
            a.set_paused(!run);
        }
    }

    pub fn toggle_play(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Seek to `pos` seconds (clamped to the media). Shows the frame there even when paused.
    pub fn seek(&mut self, pos: f64) {
        let pos = if pos.is_finite() { pos } else { 0.0 };
        if self.state == PlaybackState::Ended {
            self.state = PlaybackState::Paused;
        }
        self.restart_at(pos.clamp(0.0, self.duration));
    }

    pub fn seek_relative(&mut self, delta: f64) {
        let pos = self.position();
        self.seek(pos + delta);
    }

    /// Show the next (`forward`) or previous frame and pause there.
    pub fn step(&mut self, forward: bool) {
        self.pause();
        let Some(video) = &self.video else {
            self.seek_relative(if forward { 0.1 } else { -0.1 });
            return;
        };
        let cur = self
            .current
            .as_ref()
            .map(|p| p.index)
            .unwrap_or_else(|| video.index_at(self.position()));
        if forward {
            if cur + 1 >= self.frame_count {
                return;
            }
            let pic = video
                .take_next(Duration::from_millis(500))
                .filter(|p| p.index > cur);
            let pos = match pic {
                Some(p) => {
                    let pos = p.pts;
                    self.current = Some(p);
                    self.presented += 1;
                    pos
                }
                None => (cur + 1) as f64 / self.fps,
            };
            // keep audio and the clock in step with the shown frame (the queue continues from it)
            let reseek = self.current.as_ref().is_none_or(|p| p.index != cur + 1);
            if reseek {
                video.seek(cur + 1);
            }
            self.restart_clock(pos);
        } else if cur > 0 {
            self.restart_at((cur - 1) as f64 / self.fps);
        }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.apply_gain();
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.apply_gain();
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// Playback speed (audio is resampled: its pitch follows).
    pub fn set_speed(&mut self, speed: f64) {
        let speed = sanitize_speed(speed);
        if (speed - self.speed).abs() < 1e-9 {
            return;
        }
        self.speed = speed;
        let pos = self.position();
        self.restart_clock(pos);
    }

    pub fn looping(&self) -> bool {
        self.looping
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    /// Messages for the user (audio fell back, decode errors, …) since the last call.
    pub fn take_events(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.events);
        if let Some(v) = &self.video {
            out.extend(v.take_errors());
            if let Some(f) = v.fatal() {
                if !self.video_failed_reported {
                    self.video_failed_reported = true;
                    out.push(format!("Video stopped: {f}"));
                }
            }
        }
        out
    }

    /// Advance to the clock. Returns whether the picture to show changed.
    pub fn update(&mut self) -> bool {
        self.check_audio();
        if let (Some(since), Some(v)) = (self.preroll, &self.video) {
            if v.next_pts().is_some() || v.finished() || since.elapsed() > PREROLL_TIMEOUT {
                self.preroll = None;
                self.apply_running();
            }
        }
        let t = self.clock.now();
        // a frame due before the next display refresh is shown now (while the clock runs)
        let ahead = if self.clock.is_playing() {
            self.present_ahead * self.speed
        } else {
            0.0
        };
        let mut changed = false;
        if let Some(video) = &self.video {
            let (pic, dropped) = video.take_due(t + ahead);
            if dropped > 0 {
                log::debug!(
                    "dropped {dropped} late picture(s) at {t:.3} s (showing {:?})",
                    pic.as_ref().map(|p| p.index)
                );
            }
            self.dropped += dropped;
            if let Some(p) = pic {
                self.presented += 1;
                self.current = Some(p);
                changed = true;
            }
        }
        if self.state == PlaybackState::Playing && self.at_end(t) {
            if self.looping {
                self.restart_at(0.0);
            } else {
                self.state = PlaybackState::Ended;
                self.apply_running();
            }
        }
        changed
    }

    /// Show pictures up to `seconds` (wall time) before they are due: about half the display's
    /// refresh interval, so a repaint landing just before a frame's time shows it instead of
    /// needing another repaint right after. 0 (the default) presents exactly on time.
    pub fn set_present_ahead(&mut self, seconds: f64) {
        self.present_ahead = if seconds.is_finite() {
            seconds.clamp(0.0, 0.05)
        } else {
            0.0
        };
    }

    /// How long until the next picture is due (for scheduling a repaint).
    pub fn time_to_next_frame(&self) -> Option<Duration> {
        if self.state != PlaybackState::Playing {
            return None;
        }
        let next = self.video.as_ref()?.next_pts()?;
        let dt = (next - self.clock.now()) / self.speed - self.present_ahead;
        Some(Duration::from_secs_f64(dt.clamp(0.0, 1.0)))
    }

    pub fn stats(&self) -> Stats {
        let v = self.video.as_ref().map(|v| v.stats()).unwrap_or_default();
        let mut s = Stats {
            decoded: v.decoded,
            presented: self.presented,
            dropped: self.dropped,
            skipped: v.skipped,
            video_errors: v.errors,
            decode_ms: v.decode_ms,
            queued: self.video.as_ref().map_or(0, |v| v.queued()),
            av_offset: self
                .current
                .as_ref()
                .map_or(0.0, |p| self.position() - p.pts),
            audio_master: self.clock.audio_master(),
            ..Stats::default()
        };
        if let Some(a) = &self.audio {
            s.audio_device = Some(a.shared.device_name.clone());
            s.audio_rate = a.shared.rate;
            s.audio_channels = a.shared.channels;
            s.audio_underruns = a.shared.underruns.load(Ordering::Relaxed);
            s.audio_errors = a.shared.decode_errors.load(Ordering::Relaxed);
        }
        s
    }

    fn at_end(&self, t: f64) -> bool {
        let video_done = self.video.as_ref().is_none_or(|v| v.finished());
        let audio_done = self.audio.as_ref().is_none_or(|a| a.shared.drained());
        (video_done && t >= self.duration - 1e-3)
            || (video_done && audio_done && self.audio.is_some())
    }

    /// Fall back to the system clock when the audio device dies or stalls.
    fn check_audio(&mut self) {
        let Some(a) = &self.audio else { return };
        let failed = a.shared.device_failed.load(Ordering::Acquire);
        let stalled = self.state == PlaybackState::Playing
            && self.clock.audio_master()
            && a.shared.callback_age() > 1.5;
        if failed || stalled {
            log::warn!("audio device stopped (failed: {failed}, stalled: {stalled})");
            self.events
                .push("Audio device stopped — continuing without sound".into());
            self.clock.detach_audio();
            self.audio = None;
        }
    }

    fn apply_gain(&self) {
        if let Some(a) = &self.audio {
            let g = if self.muted {
                0.0
            } else {
                self.volume * self.volume
            };
            a.set_gain(g);
        }
    }

    /// Restart audio and the clock at `pos` (video continues from its queue).
    fn restart_clock(&mut self, pos: f64) {
        let (epoch, rate) = match &mut self.audio {
            Some(a) => a.restart(pos, self.speed),
            None => (0, 48_000),
        };
        self.clock.restart(pos, self.speed, epoch, f64::from(rate));
    }

    /// Restart everything at `pos`.
    fn restart_at(&mut self, pos: f64) {
        self.restart_clock(pos);
        if let Some(v) = &self.video {
            v.seek(v.index_at(pos));
            // hold the clock until the picture there is decoded
            self.preroll = Some(Instant::now());
        }
        self.apply_running();
    }
}

fn sanitize_speed(speed: f64) -> f64 {
    if speed.is_finite() {
        speed.clamp(0.1, 4.0)
    } else {
        1.0
    }
}
