//! The playback clock: audio master when an audio device plays, a system clock otherwise.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::audio::AudioShared;

/// Media time, in seconds, of what is being heard / should be seen right now.
///
/// Shared (`Arc`) between the UI thread, which presents video, and the video decode thread,
/// which uses it to skip ahead when decoding falls behind.
pub struct Clock {
    state: Mutex<State>,
    audio: Option<Arc<AudioShared>>,
}

struct State {
    playing: bool,
    speed: f64,
    /// System clock: position `anchor_pos` at `anchor_at`.
    anchor_pos: f64,
    anchor_at: Instant,
    /// Audio clock: the epoch the audio callback must be in for its timing to count, where that
    /// epoch starts in media time, and the source sample rate of the epoch.
    epoch: u64,
    epoch_start: f64,
    epoch_rate: f64,
    /// Highest audio-clock position reported this epoch (the clock never runs backwards).
    floor: f64,
    use_audio: bool,
}

impl Clock {
    pub(crate) fn new(audio: Option<Arc<AudioShared>>) -> Clock {
        Clock {
            state: Mutex::new(State {
                playing: false,
                speed: 1.0,
                anchor_pos: 0.0,
                anchor_at: Instant::now(),
                epoch: 0,
                epoch_start: 0.0,
                epoch_rate: 48_000.0,
                floor: 0.0,
                use_audio: audio.is_some(),
            }),
            audio,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // a panic while holding the lock leaves plain numbers behind: keep going
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The current media time in seconds.
    pub fn now(&self) -> f64 {
        let mut st = self.lock();
        self.position(&mut st)
    }

    fn position(&self, st: &mut State) -> f64 {
        if !st.playing {
            return st.anchor_pos;
        }
        match (&self.audio, st.use_audio) {
            (Some(audio), true) => {
                let snap = audio.timing();
                if snap.epoch != st.epoch {
                    // the device is still dropping the previous epoch's samples
                    return st.floor;
                }
                let since = audio.since_origin_secs() - snap.callback_secs - snap.latency_secs;
                let rate = f64::from(audio.rate);
                let played = (since * rate).clamp(-(snap.before as f64), snap.frames as f64);
                let pos = st.epoch_start + (snap.before as f64 + played) / st.epoch_rate;
                st.floor = st.floor.max(pos);
                st.floor
            }
            _ => st.anchor_pos + st.anchor_at.elapsed().as_secs_f64() * st.speed,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.lock().playing
    }

    pub fn speed(&self) -> f64 {
        self.lock().speed
    }

    /// Whether the audio device drives the clock.
    pub fn audio_master(&self) -> bool {
        self.audio.is_some() && self.lock().use_audio
    }

    pub(crate) fn set_playing(&self, playing: bool) {
        let mut st = self.lock();
        if st.playing == playing {
            return;
        }
        let pos = self.position(&mut st);
        st.anchor_pos = pos;
        st.anchor_at = Instant::now();
        st.floor = st.floor.max(pos);
        st.playing = playing;
    }

    /// Start a new epoch at `pos` (a seek, a speed change or a loop). `rate`: the source sample
    /// rate the audio thread reads the epoch at.
    pub(crate) fn restart(&self, pos: f64, speed: f64, epoch: u64, rate: f64) {
        let mut st = self.lock();
        st.anchor_pos = pos;
        st.anchor_at = Instant::now();
        st.speed = speed;
        st.epoch = epoch;
        st.epoch_start = pos;
        st.epoch_rate = rate;
        st.floor = pos;
    }

    /// Stop following the audio device (it failed); continue from the current position.
    pub(crate) fn detach_audio(&self) {
        let mut st = self.lock();
        let pos = self.position(&mut st);
        st.anchor_pos = pos;
        st.anchor_at = Instant::now();
        st.use_audio = false;
    }
}
