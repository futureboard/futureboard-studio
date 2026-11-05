//! A backing track: the instrumental, played under the vocal while tuning so
//! notes can be judged against the song.
//!
//! The decoded audio is shared (`Arc`) by every take's engine, so a project
//! with several takes holds it in memory once. Each engine plays it through
//! its own [`Backing`] (gain, and where the take sits in the song).

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::audio_io::decode_audio_file_to_stereo;

const I16_SCALE: f32 = 1.0 / i16::MAX as f32;

/// Stereo backing audio at the engine rate, stored 16-bit: a reference
/// track gains nothing from more, and a song is half the size.
pub struct BackingAudio {
    frames: Box<[[i16; 2]]>,
    sample_rate: u32,
    pub duration_sec: f64,
    /// Downsampled peak envelope (`0.0..=1.0`) for the UI.
    pub waveform_peaks: Vec<f32>,
}

impl fmt::Debug for BackingAudio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackingAudio")
            .field("frames", &self.frames.len())
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

impl BackingAudio {
    /// Decodes `path` at `sample_rate` (the engine's).
    pub fn decode(path: &std::path::Path, sample_rate: u32) -> Result<Self, String> {
        let decoded = decode_audio_file_to_stereo(path, sample_rate)?;
        Ok(Self::from_interleaved(
            &decoded.samples,
            decoded.sample_rate,
            decoded.waveform_peaks,
        ))
    }

    /// Interleaved L/R `f32` frames at `sample_rate`.
    pub fn from_interleaved(samples: &[f32], sample_rate: u32, waveform_peaks: Vec<f32>) -> Self {
        let to_i16 = |s: f32| (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        let frames: Box<[[i16; 2]]> = samples
            .chunks_exact(2)
            .map(|f| [to_i16(f[0]), to_i16(f[1])])
            .collect();
        Self {
            duration_sec: frames.len() as f64 / sample_rate.max(1) as f64,
            frames,
            sample_rate,
            waveform_peaks,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The frame at `index`; silence before the start and past the end.
    #[inline]
    pub fn frame(&self, index: i64) -> (f32, f32) {
        if index < 0 {
            return (0.0, 0.0);
        }
        match self.frames.get(index as usize) {
            Some(&[l, r]) => (l as f32 * I16_SCALE, r as f32 * I16_SCALE),
            None => (0.0, 0.0),
        }
    }
}

/// How one take plays the backing track.
#[derive(Debug, Clone)]
pub struct Backing {
    pub audio: Arc<BackingAudio>,
    /// Linear gain; 0 is muted.
    pub gain: f32,
    /// Where the take starts in the backing track, in seconds: 0 for a vocal
    /// stem exported from the start of the song, 60 for a take that starts a
    /// minute in. Negative when the take starts before the track.
    pub offset_sec: f64,
}

impl Backing {
    /// The backing frame heard at `take_sec` of the take.
    #[inline]
    pub fn at(&self, take_sec: f64) -> (f32, f32) {
        if self.gain <= 0.0 {
            return (0.0, 0.0);
        }
        let index = ((take_sec + self.offset_sec) * self.audio.sample_rate as f64).round() as i64;
        let (l, r) = self.audio.frame(index);
        (l * self.gain, r * self.gain)
    }
}

#[derive(Default)]
struct LoadState {
    done: AtomicBool,
    result: Mutex<Option<Result<Arc<BackingAudio>, String>>>,
}

/// Decoding a backing track on a worker thread, so the UI keeps running.
/// Dropping it early lets the worker finish and discard its result.
pub struct BackingLoad {
    state: Arc<LoadState>,
}

impl BackingLoad {
    pub fn start(path: PathBuf, sample_rate: u32) -> Self {
        let state = Arc::new(LoadState::default());
        let worker = Arc::clone(&state);
        let finish = |state: &LoadState, res: Result<Arc<BackingAudio>, String>| {
            if let Ok(mut slot) = state.result.lock() {
                *slot = Some(res);
            }
            state.done.store(true, Ordering::Release);
        };
        let spawned = thread::Builder::new()
            .name("canto-backing".into())
            .spawn(move || {
                let res = BackingAudio::decode(&path, sample_rate).map(Arc::new);
                finish(&worker, res);
            });
        if let Err(e) = spawned {
            finish(&state, Err(format!("Could not start loading: {e}")));
        }
        Self { state }
    }

    pub fn is_done(&self) -> bool {
        self.state.done.load(Ordering::Acquire)
    }

    /// The outcome, once; None while running or after it was taken.
    pub fn take_result(&self) -> Option<Result<Arc<BackingAudio>, String>> {
        if !self.is_done() {
            return None;
        }
        self.state.result.lock().ok()?.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(frames: usize) -> Arc<BackingAudio> {
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let v = i as f32 / frames as f32;
                [v, -v]
            })
            .collect();
        Arc::new(BackingAudio::from_interleaved(&samples, 1_000, Vec::new()))
    }

    #[test]
    fn frames_outside_the_track_are_silent() {
        let audio = ramp(100);
        assert_eq!(audio.frame(-1), (0.0, 0.0));
        assert_eq!(audio.frame(100), (0.0, 0.0));
        let (l, r) = audio.frame(50);
        assert!((l - 0.5).abs() < 1e-3 && (r + 0.5).abs() < 1e-3);
        assert!((audio.duration_sec - 0.1).abs() < 1e-9);
    }

    #[test]
    fn the_offset_places_the_take_in_the_song_and_gain_scales_it() {
        let backing = Backing {
            audio: ramp(1_000),
            gain: 0.5,
            offset_sec: 0.25,
        };
        // Take time 0 is 0.25 s into the track.
        let (l, _) = backing.at(0.0);
        assert!((l - 0.125).abs() < 1e-3, "{l}");
        let muted = Backing {
            gain: 0.0,
            ..backing
        };
        assert_eq!(muted.at(0.0), (0.0, 0.0));
    }

    #[test]
    fn loads_a_file_off_thread() {
        let sr = 48_000u32;
        let mono: Vec<f32> = (0..sr / 2)
            .map(|i| 0.5 * (std::f32::consts::TAU * 220.0 * i as f32 / sr as f32).sin())
            .collect();
        let path = std::env::temp_dir().join(format!("canto_backing_{}.wav", std::process::id()));
        std::fs::write(&path, crate::audio_io::encode_mono_wav_i16(&mono, sr)).unwrap();

        let load = BackingLoad::start(path.clone(), sr);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let audio = loop {
            if let Some(res) = load.take_result() {
                break res.expect("decode");
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(std::time::Duration::from_millis(2));
        };
        let _ = std::fs::remove_file(path);
        assert!((audio.duration_sec - 0.5).abs() < 1e-3);
        // A mono file plays on both sides.
        let (l, r) = audio.frame(1_000);
        assert!((l - r).abs() < 1e-4);
        assert!(load.take_result().is_none(), "the result is returned once");
    }
}
