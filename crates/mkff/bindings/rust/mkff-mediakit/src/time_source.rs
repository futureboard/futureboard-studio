//! Time sources (BeOS `BTimeSource`).
//!
//! Every node schedules its work in *performance time* (microseconds) of the time source it is
//! slaved to. A time source maps performance time to real time with a published triple
//! `(performance, real, drift)`: `perf(real) = performance + (real − real₀) · drift`. The system
//! time source runs at drift 1 from the moment it starts; a sound-card node publishes new triples
//! from its interrupt (here: its device callback) so everything downstream follows the audio
//! clock — the arrangement BeOS used for A/V sync.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
struct Snapshot {
    perf: i64,
    real: Instant,
    drift: f64,
    running: bool,
}

/// A clock nodes are slaved to. Cheap to clone (shared).
#[derive(Clone, Debug)]
pub struct TimeSource {
    name: Arc<str>,
    snap: Arc<Mutex<Snapshot>>,
}

impl TimeSource {
    /// A time source that starts running now at performance time 0 and drift 1.
    pub fn system() -> Self {
        Self::new("system clock", true)
    }

    /// A stopped time source (performance time frozen at 0 until [`Self::start`] or
    /// [`Self::publish`]).
    pub fn new(name: &str, running: bool) -> Self {
        Self {
            name: name.into(),
            snap: Arc::new(Mutex::new(Snapshot {
                perf: 0,
                real: Instant::now(),
                drift: 1.0,
                running,
            })),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn get(&self) -> Snapshot {
        *self.snap.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Current performance time in microseconds (`BTimeSource::Now`).
    pub fn now(&self) -> i64 {
        self.performance_time_for(Instant::now())
    }

    pub fn is_running(&self) -> bool {
        self.get().running
    }

    /// Performance time at real instant `real` (`PerformanceTimeFor`).
    pub fn performance_time_for(&self, real: Instant) -> i64 {
        let s = self.get();
        if !s.running {
            return s.perf;
        }
        let d = if real >= s.real {
            real.duration_since(s.real).as_secs_f64()
        } else {
            -s.real.duration_since(real).as_secs_f64()
        };
        s.perf + (d * s.drift * 1e6).round() as i64
    }

    /// Real instant at which performance time `perf` happens, minus `latency`
    /// (`RealTimeFor(performance, latency)`). A stopped source answers "far in the future".
    pub fn real_time_for(&self, perf: i64, latency: Duration) -> Instant {
        let s = self.get();
        let now = Instant::now();
        if !s.running || s.drift <= 0.0 {
            return now + Duration::from_secs(3600);
        }
        let secs = (perf - s.perf) as f64 / 1e6 / s.drift;
        let t = if secs >= 0.0 {
            s.real + Duration::from_secs_f64(secs)
        } else {
            s.real
                .checked_sub(Duration::from_secs_f64(-secs))
                .unwrap_or(now)
        };
        t.checked_sub(latency).unwrap_or(t)
    }

    /// Publish a new mapping (`PublishTime`): performance time `perf` happened at `real`, and
    /// the clock now advances `drift` performance seconds per real second.
    pub fn publish(&self, perf: i64, real: Instant, drift: f64) {
        let mut s = self.snap.lock().unwrap_or_else(|e| e.into_inner());
        *s = Snapshot {
            perf,
            real,
            drift: if drift.is_finite() && drift > 0.0 {
                drift
            } else {
                1.0
            },
            running: true,
        };
    }

    /// Start running from the current performance time.
    pub fn start(&self) {
        let mut s = self.snap.lock().unwrap_or_else(|e| e.into_inner());
        if !s.running {
            s.real = Instant::now();
            s.running = true;
        }
    }

    /// Freeze at the current performance time.
    pub fn stop(&self) {
        let now = self.now();
        let mut s = self.snap.lock().unwrap_or_else(|e| e.into_inner());
        *s = Snapshot {
            perf: now,
            real: Instant::now(),
            drift: s.drift,
            running: false,
        };
    }

    /// Jump to performance time `perf` now.
    pub fn seek(&self, perf: i64) {
        let mut s = self.snap.lock().unwrap_or_else(|e| e.into_inner());
        s.perf = perf;
        s.real = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_roundtrip() {
        let ts = TimeSource::new("t", false);
        assert_eq!(ts.now(), 0);
        let t0 = Instant::now();
        ts.publish(1_000_000, t0, 2.0);
        let real = ts.real_time_for(1_500_000, Duration::ZERO);
        assert!((real.duration_since(t0).as_secs_f64() - 0.25).abs() < 1e-6);
        assert!((ts.performance_time_for(t0 + Duration::from_millis(100)) - 1_200_000).abs() <= 1);
        ts.stop();
        let a = ts.now();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(ts.now(), a);
    }
}
