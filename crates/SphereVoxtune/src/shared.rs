//! Engine sharing between a UI thread and a native audio callback.
//!
//! The audio callback never blocks: it `try_lock`s the engine for a bounded
//! number of spins and renders silence (counted in `lock_misses`) if the UI
//! thread holds it. Playhead, level and play state are published through
//! atomics after every block so the UI reads them without taking the lock.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

use crate::engine::CantoEngine;

pub struct Shared {
    engine: Mutex<CantoEngine>,
    sample_rate: f32,
    playhead_bits: AtomicU64,
    level_bits: AtomicU32,
    playing: AtomicBool,
    auditioning: AtomicBool,
    lock_misses: AtomicU32,
}

impl Shared {
    pub fn new(engine: CantoEngine) -> Self {
        let sample_rate = engine.sample_rate();
        Self {
            engine: Mutex::new(engine),
            sample_rate,
            playhead_bits: AtomicU64::new(0f64.to_bits()),
            level_bits: AtomicU32::new(0f32.to_bits()),
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
        self.level_bits
            .store(engine.meter_level().to_bits(), Ordering::Relaxed);
        self.playing.store(engine.playing, Ordering::Relaxed);
        self.auditioning
            .store(engine.is_auditioning(), Ordering::Relaxed);
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
}
