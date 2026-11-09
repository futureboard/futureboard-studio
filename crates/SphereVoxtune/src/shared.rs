//! Engine sharing between a UI thread and a native audio callback.
//!
//! The audio callback never blocks: it `try_lock`s the engine for a bounded
//! number of spins and renders silence (counted in `lock_misses`) if the UI
//! thread holds it. Playhead, level and play state are published through
//! atomics after every block so the UI reads them without taking the lock.
//! A playback fork copies its peak into the source shared when that fork is
//! the one the editor is showing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use crate::engine::CantoEngine;

pub struct Shared {
    engine: Mutex<CantoEngine>,
    sample_rate: f32,
    playhead_bits: AtomicU64,
    /// This shared's own meter. For the edited engine it is `editor_meter`;
    /// a fork has a private word so a region the editor is not showing does
    /// not move the editor meter.
    level_bits: Arc<AtomicU32>,
    /// The edited engine's meter. Forks that report write it.
    editor_meter: Arc<AtomicU32>,
    /// Loaded on the audio thread. The editor sets it when the shown region
    /// changes; forks start out reporting.
    reports_meter: Arc<AtomicBool>,
    playing: AtomicBool,
    auditioning: AtomicBool,
    lock_misses: AtomicU32,
}

impl Shared {
    pub fn new(engine: CantoEngine) -> Self {
        let sample_rate = engine.sample_rate();
        let editor_meter = engine.editor_meter();
        let reports_meter = engine.reports_meter_handle();
        Self {
            engine: Mutex::new(engine),
            sample_rate,
            playhead_bits: AtomicU64::new(0f64.to_bits()),
            level_bits: Arc::clone(&editor_meter),
            editor_meter,
            reports_meter,
            playing: AtomicBool::new(false),
            auditioning: AtomicBool::new(false),
            lock_misses: AtomicU32::new(0),
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Blocking lock for the UI thread. A panic inside the engine poisons the
    /// mutex; the engine state is still usable, so recover it.
    pub fn lock(&self) -> MutexGuard<'_, CantoEngine> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn try_lock(&self) -> Option<MutexGuard<'_, CantoEngine>> {
        // UI-side critical sections are a few microseconds (single-note
        // syncs during drags); spinning briefly avoids a silent block.
        for _ in 0..1024 {
            match self.engine.try_lock() {
                Ok(guard) => return Some(guard),
                Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
                Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
            }
        }
        None
    }

    pub fn publish(&self, engine: &CantoEngine) {
        self.playhead_bits
            .store(engine.playhead_sec.to_bits(), Ordering::Relaxed);
        let bits = engine.meter_level().to_bits();
        self.level_bits.store(bits, Ordering::Relaxed);
        // The edited engine's level word is the editor meter. A fork keeps
        // its own word and copies the peak across only while it is the
        // region the editor is showing.
        if !Arc::ptr_eq(&self.level_bits, &self.editor_meter)
            && self.reports_meter.load(Ordering::Relaxed)
        {
            self.editor_meter.store(bits, Ordering::Relaxed);
        }
        self.playing.store(engine.playing, Ordering::Relaxed);
        self.auditioning
            .store(engine.is_auditioning(), Ordering::Relaxed);
    }

    /// The editor shows this fork's level, or not. An atomic store; the
    /// audio thread only loads it.
    pub fn set_reports_meter(&self, reports: bool) {
        self.reports_meter.store(reports, Ordering::Relaxed);
    }

    pub fn playhead(&self) -> f64 {
        f64::from_bits(self.playhead_bits.load(Ordering::Relaxed))
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn is_auditioning(&self) -> bool {
        self.auditioning.load(Ordering::Relaxed)
    }

    pub fn lock_misses(&self) -> u32 {
        self.lock_misses.load(Ordering::Relaxed)
    }

    /// Audio-thread render that follows a host transport (a plug-in): the
    /// take plays from `take_sec` while `playing`, re-seeking only when the
    /// host jumps. Frames before the take's start stay silent, so a take
    /// placed mid-block starts on its sample. Never blocks, never allocates;
    /// returns false when the engine was busy and the block is silent.
    ///
    /// While stopped the engine still renders, so a note preview pressed in
    /// the editor is heard.
    pub fn render_following(&self, take_sec: f64, playing: bool, out: &mut [f32]) -> bool {
        let Some(mut engine) = self.try_lock() else {
            out.fill(0.0);
            self.lock_misses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        engine.loop_enabled = false;
        let sr = self.sample_rate as f64;
        let frames = out.len() / 2;
        // Skip the part of the block before the take starts.
        let lead = if playing && take_sec < 0.0 {
            ((-take_sec * sr).round() as usize).min(frames)
        } else {
            0
        };
        out[..lead * 2].fill(0.0);
        let start = take_sec.max(0.0);
        if playing && lead < frames {
            // A block's worth of slack: the host's position and ours agree
            // to within rounding when nothing jumped.
            let slack = 2.0 * frames as f64 / sr + 0.002;
            if !engine.playing || (engine.playhead_sec - start).abs() > slack {
                if start < engine.duration_sec {
                    engine.seek(start);
                    engine.set_playing(true);
                }
            }
        } else if !playing && engine.playing {
            engine.set_playing(false);
        }
        engine.render_stereo_f32(&mut out[lead * 2..]);
        self.publish(&engine);
        true
    }

    /// Like [`Self::render_following`], at a host rate that may differ from
    /// the engine's. The engine stays at the rate it was built; this block
    /// is resampled so a project-rate change or a take recorded at another
    /// rate still plays.
    pub fn render_following_host(
        &self,
        take_sec: f64,
        playing: bool,
        host_rate: f32,
        out: &mut [f32],
    ) -> bool {
        if !(host_rate.is_finite() && host_rate > 0.0)
            || (self.sample_rate - host_rate).abs() <= 0.5
        {
            return self.render_following(take_sec, playing, out);
        }
        let host_frames = out.len() / 2;
        if host_frames == 0 {
            return true;
        }
        let Some(mut engine) = self.try_lock() else {
            out.fill(0.0);
            self.lock_misses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        engine.loop_enabled = false;
        let er = self.sample_rate as f64;
        let hr = host_rate as f64;
        let lead = if playing && take_sec < 0.0 {
            ((-take_sec * hr).round() as usize).min(host_frames)
        } else {
            0
        };
        out[..lead * 2].fill(0.0);
        let audible = host_frames - lead;
        if audible == 0 {
            self.publish(&engine);
            return true;
        }
        let start = take_sec.max(0.0);
        if playing && lead < host_frames {
            let slack = 2.0 * host_frames as f64 / hr + 0.002;
            if !engine.playing || (engine.playhead_sec - start).abs() > slack {
                if start < engine.duration_sec {
                    engine.seek(start);
                    engine.set_playing(true);
                }
            }
        } else if !playing && engine.playing {
            engine.set_playing(false);
        }
        let engine_frames = if playing && engine.playing {
            let block_end = start + audible as f64 / hr;
            ((block_end - engine.playhead_sec).max(0.0) * er).round() as usize
        } else {
            ((audible as f64) * er / hr).round().max(1.0) as usize
        };
        let dest = &mut out[lead * 2..];
        if engine_frames == 0 {
            dest.fill(0.0);
        } else {
            render_resampled(&mut engine, engine_frames, dest);
        }
        self.publish(&engine);
        true
    }

    /// Playback state of its own, sharing the take and the live edits.
    /// The fork reports its level to this shared (the editor meter).
    pub fn fork_playback(&self) -> Self {
        self.fork_playback_with(Arc::new(AtomicBool::new(true)))
    }

    /// Like [`Self::fork_playback`], publishing the meter through `reports`.
    /// Regions of one modification share that flag with the fork, so the
    /// editor can retarget it without taking the engine lock.
    pub fn fork_playback_with(&self, reports: Arc<AtomicBool>) -> Self {
        let mut engine = self.lock().fork_playback();
        engine.use_meter_flag(reports);
        let mut fork = Self::new(engine);
        fork.level_bits = Arc::new(AtomicU32::new(0));
        fork
    }

    /// Audio-thread render: never blocks, never allocates.
    pub fn render_realtime(&self, out: &mut [f32]) {
        match self.try_lock() {
            Some(mut engine) => {
                engine.render_stereo_f32(out);
                self.publish(&engine);
            }
            None => {
                out.fill(0.0);
                self.lock_misses.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

const RESAMPLE_CHUNK: usize = 64;

/// Renders `engine_frames` at the engine rate and writes `dest` (stereo
/// interleaved) at the host rate. `dest.len() / 2` is the host frame count.
fn render_resampled(engine: &mut CantoEngine, engine_frames: usize, dest: &mut [f32]) {
    let host_frames = dest.len() / 2;
    if engine_frames == 0 || host_frames == 0 {
        dest.fill(0.0);
        return;
    }
    let step = engine_frames as f64 / host_frames as f64;
    let mut chunk = [0.0f32; RESAMPLE_CHUNK * 2];
    let mut produced = 0usize;
    let mut host_i = 0usize;
    let mut prev_l = 0.0f32;
    let mut prev_r = 0.0f32;
    while host_i < host_frames && produced < engine_frames {
        let n = (engine_frames - produced).min(RESAMPLE_CHUNK);
        engine.render_stereo_f32(&mut chunk[..n * 2]);
        let chunk_end = produced + n;
        while host_i < host_frames {
            let pos = host_i as f64 * step;
            let idx = pos.floor() as usize;
            let nxt = idx + 1;
            if nxt >= chunk_end && chunk_end < engine_frames {
                break;
            }
            let frac = (pos - idx as f64) as f32;
            let sample_at = |frame: usize| -> (f32, f32) {
                if frame + 1 == produced {
                    (prev_l, prev_r)
                } else if frame >= produced && frame < chunk_end {
                    let k = frame - produced;
                    (chunk[k * 2], chunk[k * 2 + 1])
                } else {
                    (0.0, 0.0)
                }
            };
            let (l0, r0) = sample_at(idx.min(engine_frames.saturating_sub(1)));
            let (l1, r1) = if nxt >= engine_frames {
                (l0, r0)
            } else {
                sample_at(nxt)
            };
            dest[host_i * 2] = l0 + (l1 - l0) * frac;
            dest[host_i * 2 + 1] = r0 + (r1 - r0) * frac;
            host_i += 1;
        }
        prev_l = chunk[(n - 1) * 2];
        prev_r = chunk[(n - 1) * 2 + 1];
        produced = chunk_end;
    }
    if host_i < host_frames {
        dest[host_i * 2..].fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_render_never_blocks_on_a_held_lock() {
        let shared = Shared::new(CantoEngine::new(48_000.0));
        let mut out = vec![1.0f32; 256];

        // Unlocked: renders (silence, nothing loaded) and publishes state.
        shared.render_realtime(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(shared.lock_misses(), 0);

        // UI thread holds the engine: the callback must return immediately
        // with silence and count the miss instead of waiting.
        out.fill(1.0);
        let guard = shared.lock();
        let started = std::time::Instant::now();
        shared.render_realtime(&mut out);
        assert!(started.elapsed() < std::time::Duration::from_millis(5));
        drop(guard);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(shared.lock_misses(), 1);
    }

    #[test]
    fn following_a_host_starts_on_the_takes_first_sample_and_seeks_on_jumps() {
        let sr = 48_000.0f32;
        let shared = Shared::new(CantoEngine::new(sr));
        let tone: Vec<f32> = (0..sr as usize)
            .map(|i| 0.5 * (std::f32::consts::TAU * 220.0 * i as f32 / sr).sin())
            .collect();
        shared.lock().load_mono_samples(tone, sr as u32).unwrap();

        // The take starts 100 frames into this block.
        let mut out = vec![1.0f32; 512 * 2];
        assert!(shared.render_following(-100.0 / sr as f64, true, &mut out));
        assert!(out[..200].iter().all(|&s| s == 0.0));
        assert!(shared.is_playing());

        // A host jump re-seeks; continuing does not.
        let mut out = vec![0.0f32; 512 * 2];
        shared.render_following(0.5, true, &mut out);
        let at = shared.playhead();
        assert!((at - (0.5 + 512.0 / sr as f64)).abs() < 1e-3, "{at}");
        shared.render_following(at, true, &mut out);
        assert!((shared.playhead() - (at + 512.0 / sr as f64)).abs() < 1e-3);

        // Stopping the host stops the take.
        shared.render_following(0.6, false, &mut out);
        assert!(!shared.is_playing());
    }

    #[test]
    fn published_transport_matches_engine() {
        let shared = Shared::new(CantoEngine::new(48_000.0));
        {
            let mut engine = shared.lock();
            engine.playing = true;
            engine.playhead_sec = 1.25;
            shared.publish(&engine);
        }
        assert!(shared.is_playing());
        assert_eq!(shared.playhead(), 1.25);
    }

    fn sine_take() -> Shared {
        use crate::audio_io::DecodedAudio;
        use crate::engine::{Analysis, PitchTrack};
        use crate::scale::Scale;

        let sr = 48_000u32;
        let samples: Vec<f32> = (0..sr as usize)
            .map(|i| 0.5 * (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin())
            .collect();
        let shared = Shared::new(CantoEngine::new(sr as f32));
        shared.lock().install_analyzed(
            DecodedAudio {
                samples,
                sample_rate: sr,
                source_sample_rate: sr,
                source_channels: 1,
                duration_sec: 1.0,
                waveform_peaks: Vec::new(),
            },
            Analysis {
                key: 0,
                scale: Scale::Major,
                bpm: 120.0,
                notes: Vec::new(),
                pitch: PitchTrack::default(),
            },
        );
        shared.lock().set_bypass(true);
        shared
    }

    #[test]
    fn a_different_host_rate_still_plays_the_take() {
        let shared = sine_take();
        let mut out = vec![0.0f32; 256 * 2];
        assert!(shared.render_following_host(0.05, true, 24_000.0, &mut out));
        let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak > 0.2, "silent at half rate ({peak})");
    }

    #[test]
    fn playback_forks_keep_independent_playheads() {
        let shared = sine_take();
        let a = shared.fork_playback();
        let b = shared.fork_playback();
        let mut out = vec![0.0f32; 512 * 2];
        assert!(a.render_following(0.2, true, &mut out));
        assert!(b.render_following(0.2, true, &mut out));
        let step = 512.0 / 48_000.0;
        let pa = a.playhead();
        let pb = b.playhead();
        assert!((pa - (0.2 + step)).abs() < 1e-3, "{pa}");
        assert!((pb - (0.2 + step)).abs() < 1e-3, "{pb}");
    }

    fn peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    fn muted_cover() -> crate::engine::CantoNote {
        use crate::engine::{CONTOUR_POINTS, CantoNote};
        CantoNote {
            id: "m".into(),
            lyric: String::new(),
            unpitched: true,
            detected: 57.0,
            pitch: 57.0,
            detected_start: 0.0,
            start: 0.0,
            detected_dur: 1.0,
            dur: 1.0,
            modulation: 1.0,
            formant: 0.0,
            gain_db: 0.0,
            detected_gain: 0.0,
            pinned: false,
            muted: true,
            orn_style: 0,
            orn_depth: 0.0,
            orn_speed: 0.0,
            accent: 0.0,
            scoop: 0.0,
            drift: 0.0,
            vib: 0.0,
            vib_cycles: 0.0,
            vib_phase: 0.0,
            contour: vec![0.0; CONTOUR_POINTS],
            envelope: vec![1.0; CONTOUR_POINTS],
        }
    }

    #[test]
    fn a_fork_hears_bypass_toggled_on_the_edited_engine() {
        let shared = sine_take();
        {
            let mut engine = shared.lock();
            engine.set_notes(vec![muted_cover()]);
            engine.set_bypass(false);
        }
        let fork = shared.fork_playback();
        let mut out = vec![0.0f32; 2048 * 2];
        assert!(fork.render_following(0.05, true, &mut out));
        let muted = peak(&out);
        assert!(muted < 1.0e-3, "muted take is silent ({muted})");

        shared.lock().set_bypass(true);
        assert!(fork.render_following(0.05, true, &mut out));
        let dry = peak(&out);
        assert!(
            dry > 0.2,
            "bypass toggled on the edited engine is heard by the fork ({dry})"
        );
    }

    #[test]
    fn the_editor_meter_follows_the_fork_that_reports() {
        let shared = sine_take();
        let quiet = shared.fork_playback();
        quiet.set_reports_meter(false);
        let heard = shared.fork_playback();
        let mut out = vec![0.0f32; 1024 * 2];
        assert!(quiet.render_following(0.0, true, &mut out));
        assert!(
            quiet.level() > 0.15,
            "the fork meters itself ({})",
            quiet.level()
        );
        assert_eq!(
            shared.level(),
            0.0,
            "a fork the editor is not showing stays off the meter"
        );
        assert!(heard.render_following(0.0, true, &mut out));
        assert!(
            shared.level() > 0.15,
            "the reporting fork drives the editor meter ({})",
            shared.level()
        );
        assert_eq!(shared.level().to_bits(), heard.level().to_bits());
    }
}
