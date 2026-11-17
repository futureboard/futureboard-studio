//! The video decode thread: decodes ahead into a bounded queue of [`Picture`]s.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mkff_codecs::MediaFile;

use crate::clock::Clock;
use crate::picture::Picture;

/// Pictures decoded ahead of the clock.
const QUEUE_LEN: usize = 6;
/// Consecutive failed frames before video is given up on.
const MAX_CONSECUTIVE_ERRORS: u32 = 25;

pub(crate) type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct State {
    quit: bool,
    /// Pending seek: (epoch, frame index).
    seek: Option<(u64, u64)>,
    /// Epoch of the frames that may enter the queue.
    epoch: u64,
    queue: VecDeque<Arc<Picture>>,
    /// The decoder passed the last frame (this epoch).
    eos: bool,
    /// Video stopped for good.
    fatal: Option<String>,
    /// Non-fatal errors since the last [`VideoWorker::take_errors`].
    errors: Vec<String>,
    stats: VideoStats,
}

/// Decode-side counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoStats {
    pub decoded: u64,
    /// Frames the decoder skipped to catch up with the clock.
    pub skipped: u64,
    pub errors: u64,
    /// Smoothed decode time per frame, milliseconds.
    pub decode_ms: f64,
}

struct Shared {
    state: Mutex<State>,
    cond: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub(crate) struct VideoWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    fps: f64,
    frame_count: u64,
}

impl VideoWorker {
    pub fn start(
        media: MediaFile,
        fps: f64,
        frame_count: u64,
        clock: Arc<Clock>,
        waker: Option<Waker>,
    ) -> Result<VideoWorker, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cond: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("mkff-video-decode".into())
            .spawn(move || run(media, fps, frame_count, clock, thread_shared, waker))
            .map_err(|e| format!("video decode thread: {e}"))?;
        Ok(VideoWorker {
            shared,
            thread: Some(thread),
            fps,
            frame_count,
        })
    }

    /// The frame index shown at `pos` seconds.
    pub fn index_at(&self, pos: f64) -> u64 {
        let i = (pos.max(0.0) * self.fps + 1e-6).floor() as u64;
        i.min(self.frame_count.saturating_sub(1))
    }

    /// Restart decoding at frame `index`; queued pictures are discarded.
    pub fn seek(&self, index: u64) {
        let mut st = self.shared.lock();
        st.epoch += 1;
        let epoch = st.epoch;
        st.seek = Some((epoch, index));
        st.queue.clear();
        st.eos = false;
        drop(st);
        self.shared.cond.notify_all();
    }

    /// The newest queued picture due at `t` (older due ones are dropped: returned as the count).
    pub fn take_due(&self, t: f64) -> (Option<Arc<Picture>>, u64) {
        let mut st = self.shared.lock();
        let mut due = None;
        let mut dropped = 0;
        while st.queue.front().is_some_and(|p| p.pts <= t + 1e-6) {
            if due.is_some() {
                dropped += 1;
            }
            due = st.queue.pop_front();
        }
        drop(st);
        if due.is_some() {
            self.shared.cond.notify_all();
        }
        (due, dropped)
    }

    /// The next queued picture whatever its time (frame stepping), waiting up to `timeout`.
    pub fn take_next(&self, timeout: Duration) -> Option<Arc<Picture>> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if let Some(p) = st.queue.pop_front() {
                drop(st);
                self.shared.cond.notify_all();
                return Some(p);
            }
            if st.eos || st.fatal.is_some() {
                return None;
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            st = self
                .shared
                .cond
                .wait_timeout(st, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Presentation time of the next queued picture.
    pub fn next_pts(&self) -> Option<f64> {
        self.shared.lock().queue.front().map(|p| p.pts)
    }

    /// Every frame of this epoch has been decoded and taken.
    pub fn finished(&self) -> bool {
        let st = self.shared.lock();
        (st.eos || st.fatal.is_some()) && st.queue.is_empty() && st.seek.is_none()
    }

    pub fn queued(&self) -> usize {
        self.shared.lock().queue.len()
    }

    pub fn stats(&self) -> VideoStats {
        self.shared.lock().stats
    }

    pub fn fatal(&self) -> Option<String> {
        self.shared.lock().fatal.clone()
    }

    pub fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.lock().errors)
    }
}

impl Drop for VideoWorker {
    fn drop(&mut self) {
        self.shared.lock().quit = true;
        self.shared.cond.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    media: MediaFile,
    fps: f64,
    frame_count: u64,
    clock: Arc<Clock>,
    shared: Arc<Shared>,
    waker: Option<Waker>,
) {
    let wake = || {
        if let Some(w) = &waker {
            w();
        }
    };
    // Fall this many frames behind the clock and the decoder jumps to it.
    let late_frames = ((fps * 0.25).ceil() as u64).max(3);
    let mut epoch = 0u64;
    let mut next = 0u64;
    let mut consecutive_errors = 0u32;
    loop {
        {
            let mut st = shared.lock();
            loop {
                if st.quit {
                    return;
                }
                if let Some((e, index)) = st.seek.take() {
                    epoch = e;
                    next = index;
                    consecutive_errors = 0;
                    break;
                }
                if st.epoch == epoch && !st.eos && st.fatal.is_none() && st.queue.len() < QUEUE_LEN
                {
                    break;
                }
                st = shared.cond.wait(st).unwrap_or_else(|p| p.into_inner());
            }
        }

        if clock.is_playing() {
            let due = (clock.now() * fps).floor().max(0.0) as u64;
            if due > next + late_frames {
                let target = due.min(frame_count.saturating_sub(1));
                log::debug!("video behind the clock: skipping {next} → {target}");
                shared.lock().stats.skipped += target.saturating_sub(next);
                next = target;
            }
        }
        if next >= frame_count {
            let mut st = shared.lock();
            if st.epoch == epoch {
                st.eos = true;
            }
            drop(st);
            wake();
            continue;
        }

        let t0 = Instant::now();
        let index = next;
        let result = catch_unwind(AssertUnwindSafe(|| {
            media
                .video_frame(index)
                .map(|f| Picture::from_frame(&f, index, index as f64 / fps))
        }));
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        next += 1;

        let mut st = shared.lock();
        // While playing, the UI schedules its own repaints from the queue's timestamps; it
        // only needs waking when it may be waiting on an empty queue (a seek, a stall) or for
        // an error.
        let mut notify_ui = st.queue.is_empty();
        match result {
            Ok(Ok(pic)) => {
                consecutive_errors = 0;
                st.stats.decoded += 1;
                st.stats.decode_ms = if st.stats.decoded == 1 {
                    elapsed_ms
                } else {
                    st.stats.decode_ms * 0.9 + elapsed_ms * 0.1
                };
                if st.epoch == epoch && st.seek.is_none() {
                    st.queue.push_back(Arc::new(pic));
                }
            }
            failure => {
                let msg = match failure {
                    Ok(Err(e)) => format!("video frame {index}: {e}"),
                    _ => format!("video frame {index}: decoder panicked"),
                };
                log::warn!("{msg}");
                notify_ui = true;
                st.stats.errors += 1;
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    st.fatal = Some(msg);
                } else if st.errors.len() < 16 {
                    st.errors.push(msg);
                }
            }
        }
        drop(st);
        shared.cond.notify_all();
        if notify_ui {
            wake();
        }
    }
}
