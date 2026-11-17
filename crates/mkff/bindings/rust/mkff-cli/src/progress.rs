//! The `frame= … size= … time= … speed=` status line on stderr.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::args::{Global, Level};
use crate::units::{fmt_kb, fmt_time};

pub struct Progress {
    enabled: bool,
    path: String,
    started: Instant,
    last: Instant,
}

impl Progress {
    pub fn new(g: &Global, path: &str) -> Self {
        let now = Instant::now();
        Progress {
            enabled: g.stats && g.level >= Level::Info,
            path: path.to_owned(),
            started: now,
            last: now,
        }
    }

    fn line(&self, frames: Option<u64>, secs: f64) -> String {
        let el = self.started.elapsed().as_secs_f64().max(1e-6);
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let bitrate = if secs > 0.0 {
            format!("{:.1}kbits/s", size as f64 * 8.0 / secs / 1000.0)
        } else {
            "N/A".into()
        };
        let frame = frames
            .map(|f| format!("frame={f:>5} fps={:>4.0} ", f as f64 / el))
            .unwrap_or_default();
        format!(
            "{frame}size={:>8} time={} bitrate={bitrate} speed={:.3}x",
            fmt_kb(size),
            fmt_time(secs),
            secs / el
        )
    }

    pub fn update(&mut self, frames: Option<u64>, secs: f64) {
        if self.enabled && self.last.elapsed() >= Duration::from_millis(500) {
            self.last = Instant::now();
            eprint!("{}    \r", self.line(frames, secs));
            let _ = std::io::stderr().flush();
        }
    }

    pub fn finish(&mut self, frames: Option<u64>, secs: f64) {
        if self.enabled {
            eprintln!("{}    ", self.line(frames, secs));
        }
    }
}
