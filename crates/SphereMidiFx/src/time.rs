//! The musical clock a device sees for one block.

/// Transport and timing for one block, read by the devices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Context {
    pub sample_rate: f64,
    /// Frames in this block.
    pub frames: u32,
    /// Quarter notes per minute.
    pub tempo_bpm: f64,
    /// Project position of the block start in quarter notes.
    pub ppq: f64,
    pub playing: bool,
}

impl Context {
    /// Samples per quarter note at this block's tempo.
    pub fn samples_per_beat(&self) -> f64 {
        self.sample_rate.max(1.0) * 60.0 / self.tempo_bpm.max(1.0)
    }

    pub fn samples_for_ms(&self, ms: f32) -> u64 {
        (f64::from(ms.max(0.0)) * self.sample_rate / 1000.0).round() as u64
    }
}

/// Where a device's grid stands: the project position while playing, its own
/// free-running position while stopped (so an arpeggio still runs on live
/// input with the transport stopped), and its own sample clock for anything
/// measured in samples.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clock {
    /// Device sample clock at the start of the current block.
    pub samples: u64,
    /// Free-running position in quarter notes, used while stopped.
    free_ppq: f64,
    was_playing: bool,
}

impl Clock {
    /// Position of this block's start on the device's grid.
    pub fn block_ppq(&mut self, ctx: &Context) -> f64 {
        if ctx.playing {
            self.was_playing = true;
            ctx.ppq
        } else {
            if self.was_playing {
                // Stopping restarts the free grid where the transport stood.
                self.was_playing = false;
                self.free_ppq = ctx.ppq;
            }
            self.free_ppq
        }
    }

    /// Advance past the block just processed.
    pub fn advance(&mut self, ctx: &Context) {
        self.samples += u64::from(ctx.frames);
        if !ctx.playing {
            self.free_ppq += f64::from(ctx.frames) / ctx.samples_per_beat();
        }
    }

    /// Restart the free grid at the next block (a note arriving while
    /// stopped starts its pattern on that note, not mid-step).
    pub fn restart_free(&mut self, ppq: f64) {
        self.free_ppq = ppq;
    }
}

/// Grid points `k * step` (in quarter notes) falling in this block, from
/// `start_ppq`, as `(k, sample offset)`.
pub struct GridSteps {
    next: f64,
    step: f64,
    start_ppq: f64,
    spb: f64,
    frames: u32,
}

impl GridSteps {
    pub fn new(start_ppq: f64, step: f64, spb: f64, frames: u32) -> Self {
        let step = step.max(1.0e-6);
        // A step belongs to the block its sample rounds into: one up to half
        // a sample before this block's start rounded to this block's offset
        // 0 (the previous block rounded it to its own end and skipped it),
        // so every step lands in exactly one block.
        let k = ((start_ppq - 0.5 / spb.max(1.0)) / step).ceil();
        Self {
            next: k,
            step,
            start_ppq,
            spb,
            frames,
        }
    }
}

impl Iterator for GridSteps {
    type Item = (i64, u32);

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.next * self.step;
        let offset = ((at - self.start_ppq).max(0.0) * self.spb).round();
        if offset >= f64::from(self.frames) {
            return None;
        }
        let k = self.next as i64;
        self.next += 1.0;
        Some((k, offset as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(ppq: f64, playing: bool) -> Context {
        Context {
            sample_rate: 48_000.0,
            frames: 512,
            tempo_bpm: 120.0,
            ppq,
            playing,
        }
    }

    #[test]
    fn grid_steps_land_on_exact_sample_offsets() {
        let c = ctx(0.0, true);
        // 1/16 at 120 BPM / 48 kHz = 6000 samples; one block of 512 holds k=0.
        let steps: Vec<_> = GridSteps::new(0.0, 0.25, c.samples_per_beat(), 512).collect();
        assert_eq!(steps, vec![(0, 0)]);
        // A block starting just before step 1 (at 0.25 ppq).
        let start = 0.25 - 100.0 / c.samples_per_beat();
        let steps: Vec<_> = GridSteps::new(start, 0.25, c.samples_per_beat(), 512).collect();
        assert_eq!(steps, vec![(1, 100)]);
    }

    #[test]
    fn free_clock_runs_while_stopped_and_resumes_at_transport() {
        let mut clock = Clock::default();
        let playing = ctx(8.0, true);
        assert_eq!(clock.block_ppq(&playing), 8.0);
        clock.advance(&playing);
        let stopped = ctx(8.5, false);
        assert_eq!(clock.block_ppq(&stopped), 8.5);
        clock.advance(&stopped);
        let next = clock.block_ppq(&stopped);
        assert!((next - (8.5 + 512.0 / 24_000.0)).abs() < 1e-9);
        assert_eq!(clock.samples, 1024);
    }
}
