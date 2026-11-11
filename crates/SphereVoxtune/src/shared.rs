//! Engine sharing between a UI thread and a native audio callback.
//!
//! The audio callback never blocks: it `try_lock`s the engine for a bounded
//! number of spins and renders silence (counted in `lock_misses`) if the UI
//! thread holds it. Playhead, level and play state are published through
//! atomics after every block so the UI reads them without taking the lock.
//! A playback fork copies its peak into the source shared when that fork is
//! the one the editor is showing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};

use crate::engine::CantoEngine;

pub struct Shared {
    engine: Mutex<CantoEngine>,
    /// Converts the engine's output to a host rate that differs from it.
    /// Only the audio thread uses it.
    host_resampler: Mutex<HostResampler>,
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
        // Built here, off the audio thread, so rendering never allocates it.
        kernel_table();
        Self {
            engine: Mutex::new(engine),
            host_resampler: Mutex::new(HostResampler::new()),
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
            if (!engine.playing || out_of_line(engine.playhead_sec, start, sr))
                && start < engine.duration_sec
            {
                engine.seek(start);
                engine.set_playing(true);
            }
        } else if !playing && engine.playing {
            engine.set_playing(false);
        }
        engine.render_stereo_f32(&mut out[lead * 2..]);
        self.publish(&engine);
        true
    }

    /// Like [`Self::render_following`], at a host rate that may differ from
    /// the engine's. The engine stays at the rate it was built (the take's);
    /// its output is converted to the host rate with a band-limited
    /// interpolator whose position and history carry over from block to
    /// block, so a take recorded at another rate keeps its top end and the
    /// result does not depend on how the host splits blocks. Never blocks,
    /// never allocates.
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
        if out.len() / 2 == 0 {
            return true;
        }
        let Some(mut engine) = self.try_lock() else {
            out.fill(0.0);
            self.lock_misses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        // Only this (audio) thread takes it, so this does not wait.
        let mut resampler = match self.host_resampler.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                out.fill(0.0);
                self.lock_misses.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        engine.loop_enabled = false;
        let er = self.sample_rate as f64;
        let hr = host_rate as f64;
        let bandwidth = resampler_bandwidth(er, hr);
        let reach = HALF_TAPS as f64 / bandwidth;
        if playing {
            if !resampler.following
                || resampler.host_rate != hr
                || out_of_line(resampler.next_time(er), take_sec, hr)
            {
                // Host frame 0 is take time `take_sec`, which may be before
                // the take: that part is silence, and the take starts on its
                // own sample.
                let from = resampler.restart(take_sec, er, hr, reach);
                resampler.following = true;
                if from < engine.duration_sec {
                    engine.seek(from);
                    engine.set_playing(true);
                } else {
                    engine.set_playing(false);
                }
            }
        } else {
            if engine.playing {
                engine.set_playing(false);
            }
            // Stopped, the engine still renders a note preview. Keep
            // converting it; there is no transport to line it up with.
            if resampler.following || resampler.host_rate != hr {
                resampler.restart(0.0, er, hr, reach);
                resampler.following = false;
            }
        }
        resampler.render(&mut engine, er / hr, bandwidth, out);
        self.publish(&engine);
        if resampler.following {
            // The engine runs ahead by the interpolator's reach; the playhead
            // is the time being heard.
            let heard = resampler
                .next_time(er)
                .clamp(0.0, engine.duration_sec.max(0.0));
            self.playhead_bits.store(heard.to_bits(), Ordering::Relaxed);
        }
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

/// How far, in frames, the take's position may sit from the host's before
/// the take is put back in line with it. While every block is rendered only
/// rounding separates them (a take starting mid-block starts on a whole
/// frame, half a frame at most). A block the take did not render — the
/// engine lock was busy, or the plug-in glue skipped the call — leaves it at
/// least one frame behind, and it re-aligns on the next block it renders.
const ALIGN_TOLERANCE_FRAMES: f64 = 0.75;

/// The take's position `ours` and the host's `host` (seconds) disagree by
/// more than rounding at `rate`: the host jumped, or blocks were missed.
fn out_of_line(ours: f64, host: f64, rate: f64) -> bool {
    (ours - host).abs() * rate > ALIGN_TOLERANCE_FRAMES
}

/// Half the interpolation kernel, in engine frames at full bandwidth.
const HALF_TAPS: usize = 24;
/// Kernel table points per engine frame.
const KERNEL_STEPS: usize = 128;
/// Kaiser window shape: about 80 dB of stopband rejection.
const KAISER_BETA: f64 = 8.0;
/// Passband edge as a fraction of the lower rate's Nyquist frequency. With
/// 48 taps the response is flat to about 18 kHz for a 44.1 kHz take.
const CUTOFF: f64 = 0.92;
/// The narrowest band the kernel is stretched to (an engine rate up to 16x
/// the host's); beyond that a little aliasing is accepted.
const MIN_BANDWIDTH: f64 = 1.0 / 16.0;
/// Engine frames kept around the read position: the widest kernel
/// (`2 * HALF_TAPS / (MIN_BANDWIDTH * CUTOFF)` + 2) plus one render chunk.
const RING_FRAMES: usize = 1024;
/// Engine frames rendered per call into the engine.
const RENDER_CHUNK: usize = 64;

/// The kernel's bandwidth, as a fraction of the engine's Nyquist frequency:
/// the whole band when upsampling, the host's band when downsampling.
fn resampler_bandwidth(engine_rate: f64, host_rate: f64) -> f64 {
    (host_rate / engine_rate).clamp(MIN_BANDWIDTH, 1.0) * CUTOFF
}

/// A Kaiser-windowed sinc over `0..=HALF_TAPS`, `KERNEL_STEPS` points per
/// unit. Built once per process.
fn kernel_table() -> &'static [f32] {
    static TABLE: OnceLock<Box<[f32]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let i0_beta = bessel_i0(KAISER_BETA);
        (0..=HALF_TAPS * KERNEL_STEPS + 1)
            .map(|i| {
                let x = i as f64 / KERNEL_STEPS as f64;
                if x >= HALF_TAPS as f64 {
                    return 0.0;
                }
                let sinc = if i == 0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let t = x / HALF_TAPS as f64;
                let window = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
                (sinc * window) as f32
            })
            .collect()
    })
}

/// Modified Bessel function of the first kind, order 0 (Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let quarter_sq = x * x / 4.0;
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..64 {
        term *= quarter_sq / (k * k) as f64;
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

#[inline]
fn kernel_at(table: &[f32], x: f64) -> f32 {
    let u = x.abs() * KERNEL_STEPS as f64;
    let i = u as usize;
    if i + 1 >= table.len() {
        return 0.0;
    }
    let frac = (u - i as f64) as f32;
    table[i] + (table[i + 1] - table[i]) * frac
}

/// Converts the engine's stream to the host rate. The engine renders ahead
/// of the read position by the kernel's reach into a ring; each host frame is
/// a windowed-sinc sum around its fractional position. Position and ring
/// carry over between blocks. All storage is allocated up front.
struct HostResampler {
    /// Engine frames, stereo interleaved, at `frame % RING_FRAMES`.
    ring: Box<[f32]>,
    /// Engine frames written since the last restart.
    written: u64,
    /// Silent frames still to write before the engine's own: take time
    /// before the take starts.
    silence: u64,
    /// Position of the next host frame, in engine frames since the restart.
    pos: f64,
    /// Take time (seconds) of engine frame 0 since the restart.
    origin: f64,
    /// Host rate the position was set up for; 0 before the first block.
    host_rate: f64,
    /// Following a playing host transport.
    following: bool,
}

impl HostResampler {
    fn new() -> Self {
        Self {
            ring: vec![0.0; RING_FRAMES * 2].into_boxed_slice(),
            written: 0,
            silence: 0,
            pos: 0.0,
            origin: 0.0,
            host_rate: 0.0,
            following: false,
        }
    }

    /// Starts over with the next host frame at take time `at`. Returns the
    /// take time the engine renders from; frames before the take are silence.
    fn restart(&mut self, at: f64, engine_rate: f64, host_rate: f64, reach: f64) -> f64 {
        // Enough history before the first host frame for the whole kernel.
        let pre = reach.ceil() + 1.0;
        // The engine starts on one of the take's own samples and the
        // fraction goes into the read position: the engine reads between
        // samples with two points, which would dull the take again whenever
        // the host's frame falls between two of the take's.
        let at_frame = at * engine_rate;
        let first = at_frame.floor() - pre;
        self.written = 0;
        self.pos = at_frame - first;
        self.origin = first / engine_rate;
        self.host_rate = host_rate;
        self.silence = if first < 0.0 { -first as u64 } else { 0 };
        self.origin.max(0.0)
    }

    /// Take time of the next host frame.
    fn next_time(&self, engine_rate: f64) -> f64 {
        self.origin + self.pos / engine_rate
    }

    /// Writes engine frames into the ring until `frames` have been written.
    fn fill_to(&mut self, engine: &mut CantoEngine, frames: u64) {
        let mut chunk = [0.0f32; RENDER_CHUNK * 2];
        while self.written < frames {
            let at = (self.written % RING_FRAMES as u64) as usize;
            let mut n = ((frames - self.written) as usize)
                .min(RENDER_CHUNK)
                .min(RING_FRAMES - at);
            if self.silence > 0 {
                n = n.min(self.silence as usize);
                self.ring[at * 2..(at + n) * 2].fill(0.0);
                self.silence -= n as u64;
            } else {
                engine.render_stereo_f32(&mut chunk[..n * 2]);
                self.ring[at * 2..(at + n) * 2].copy_from_slice(&chunk[..n * 2]);
            }
            self.written += n as u64;
        }
    }

    /// Fills `out` (stereo interleaved, host rate). `step` is engine frames
    /// per host frame, `bandwidth` the kernel's (see [`resampler_bandwidth`]).
    fn render(&mut self, engine: &mut CantoEngine, step: f64, bandwidth: f64, out: &mut [f32]) {
        let table = kernel_table();
        let reach = HALF_TAPS as f64 / bandwidth;
        let gain = bandwidth as f32;
        for frame in out.as_chunks_mut::<2>().0 {
            let p = self.pos;
            let lo = (p - reach).ceil().max(0.0) as u64;
            let hi = (p + reach).floor() as u64;
            if hi >= self.written {
                // Render ahead a chunk at a time, never over frames still in
                // the kernel's window.
                let room = lo + RING_FRAMES as u64;
                let target = (self.written + RENDER_CHUNK as u64).max(hi + 1).min(room);
                self.fill_to(engine, target);
            }
            let (mut left, mut right) = (0.0f32, 0.0f32);
            for k in lo..=hi {
                let w = kernel_at(table, (p - k as f64) * bandwidth);
                let at = (k % RING_FRAMES as u64) as usize * 2;
                left += w * self.ring[at];
                right += w * self.ring[at + 1];
            }
            frame[0] = left * gain;
            frame[1] = right * gain;
            self.pos += step;
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

    /// A bypassed take of `hz` (plus 220 Hz) recorded at `sr`.
    fn tone_take(sr: u32, hz: f32) -> Shared {
        use crate::audio_io::DecodedAudio;
        use crate::engine::{Analysis, PitchTrack};
        use crate::scale::Scale;

        let samples: Vec<f32> = (0..sr as usize * 2)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.3 * (std::f32::consts::TAU * hz * t).sin()
                    + 0.2 * (std::f32::consts::TAU * 220.0 * t).sin()
            })
            .collect();
        let shared = Shared::new(CantoEngine::new(sr as f32));
        shared.lock().install_analyzed(
            DecodedAudio {
                samples,
                sample_rate: sr,
                source_sample_rate: sr,
                source_channels: 1,
                duration_sec: 2.0,
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

    /// Renders `seconds` of the take from 0.25 s at `host_rate` in blocks of
    /// `block` frames, as a host does. Left channel only.
    fn render_at(shared: &Shared, host_rate: f32, block: usize, seconds: f64) -> Vec<f32> {
        render_from(shared, 0.25, host_rate, block, seconds)
    }

    /// [`render_at`] from take time `from`.
    fn render_from(
        shared: &Shared,
        from: f64,
        host_rate: f32,
        block: usize,
        seconds: f64,
    ) -> Vec<f32> {
        let total = (seconds * host_rate as f64) as usize;
        let mut left = Vec::with_capacity(total);
        let mut out = vec![0.0f32; block * 2];
        while left.len() < total {
            let take_sec = from + left.len() as f64 / host_rate as f64;
            assert!(shared.render_following_host(take_sec, true, host_rate, &mut out));
            left.extend(out.as_chunks::<2>().0.iter().map(|frame| frame[0]));
        }
        left.truncate(total);
        left
    }

    /// Amplitude of `hz` in `samples` at `rate` (one DFT bin).
    fn amplitude(samples: &[f32], rate: f32, hz: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, s) in samples.iter().enumerate() {
            let phase = std::f64::consts::TAU * hz as f64 * i as f64 / rate as f64;
            re += *s as f64 * phase.cos();
            im += *s as f64 * phase.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / samples.len() as f64) as f32
    }

    /// #357: a 44.1 kHz take in a 48 kHz project keeps its top end. Two-point
    /// interpolation lost about 4 dB at 16 kHz.
    #[test]
    fn a_take_at_another_rate_keeps_its_high_frequencies() {
        for hz in [10_000.0f32, 16_000.0] {
            let native = render_at(&tone_take(44_100, hz).fork_playback(), 44_100.0, 441, 1.0);
            let converted = render_at(&tone_take(44_100, hz).fork_playback(), 48_000.0, 480, 1.0);
            let reference = amplitude(&native, 44_100.0, hz);
            let heard = amplitude(&converted, 48_000.0, hz);
            assert!(
                reference > 0.25,
                "the take plays at its own rate ({reference})"
            );
            let db = 20.0 * (heard / reference).log10();
            assert!(db.abs() < 0.2, "{hz} Hz is {db:.2} dB off at 48 kHz");
            // Nothing new appears where the interpolation images would land.
            let image = amplitude(&converted, 48_000.0, 44_100.0 - hz);
            assert!(image < reference * 0.01, "{hz} Hz images at {image}");
        }
    }

    /// Playback started (or re-aligned) where the host's frame falls between
    /// two of the take's samples keeps the top end too: the band-limited
    /// converter takes the fraction, not the engine's two-point read.
    #[test]
    fn a_take_at_another_rate_keeps_its_high_frequencies_from_any_start() {
        // Six 48 kHz frames past 0.25 s is 5.51 frames of a 44.1 kHz take.
        let from = 0.25 + 6.0 / 48_000.0;
        for hz in [10_000.0f32, 16_000.0] {
            let native = render_at(&tone_take(44_100, hz).fork_playback(), 44_100.0, 441, 1.0);
            let converted = render_from(
                &tone_take(44_100, hz).fork_playback(),
                from,
                48_000.0,
                480,
                1.0,
            );
            let reference = amplitude(&native, 44_100.0, hz);
            let heard = amplitude(&converted, 48_000.0, hz);
            let db = 20.0 * (heard / reference).log10();
            assert!(
                db.abs() < 0.2,
                "{hz} Hz is {db:.2} dB off from an off-grid start"
            );
        }
    }

    /// How the host splits its blocks does not change what it hears.
    #[test]
    fn the_converted_take_does_not_depend_on_the_host_block_size() {
        let whole = render_at(
            &tone_take(44_100, 3_000.0).fork_playback(),
            48_000.0,
            4_800,
            0.1,
        );
        let split = render_at(
            &tone_take(44_100, 3_000.0).fork_playback(),
            48_000.0,
            37,
            0.1,
        );
        let worst = whole
            .iter()
            .zip(&split)
            .fold(0.0f32, |worst, (a, b)| worst.max((a - b).abs()));
        assert!(worst < 1.0e-4, "block size changed the output by {worst}");
    }

    /// How a block goes wrong in [`render_missing_one`].
    #[derive(Clone, Copy, Debug)]
    enum Miss {
        /// Nothing goes wrong: the reference.
        None,
        /// The UI thread holds the engine lock for the whole callback.
        EngineLock,
        /// The plug-in glue never calls the player (its region list or the
        /// region's player slot was busy), so the engine is not told.
        Skipped,
    }

    /// Renders `blocks` host blocks of `block` frames from take time 0.25 s
    /// at `host_rate`, the host advancing every block whatever happens to
    /// block `missed`. Returns the left channel of every block and the
    /// published playhead after each.
    fn render_missing_one(
        shared: &Shared,
        host_rate: f32,
        block: usize,
        blocks: usize,
        missed: usize,
        miss: Miss,
    ) -> (Vec<Vec<f32>>, Vec<f64>) {
        let mut out = vec![0.0f32; block * 2];
        let mut heard = Vec::new();
        let mut playheads = Vec::new();
        for b in 0..blocks {
            let take_sec = 0.25 + (b * block) as f64 / host_rate as f64;
            out.fill(0.0);
            match (b == missed, miss) {
                (true, Miss::EngineLock) => {
                    let guard = shared.lock();
                    assert!(!shared.render_following_host(take_sec, true, host_rate, &mut out));
                    drop(guard);
                }
                (true, Miss::Skipped) => {}
                _ => assert!(shared.render_following_host(take_sec, true, host_rate, &mut out)),
            }
            heard.push(
                out.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|frame| frame[0])
                    .collect(),
            );
            playheads.push(shared.playhead());
        }
        (heard, playheads)
    }

    /// Audit #408 (follow-up to #357): after one missed callback the take
    /// lines up with the host again on the very next block, at the take's
    /// own rate and converted to another host rate, whether the engine lock
    /// was busy or the glue skipped the call.
    #[test]
    fn one_missed_block_does_not_leave_the_take_late() {
        let block = 512;
        let (blocks, missed) = (8, 3);
        let mut late_blocks = Vec::new();
        for take_rate in [48_000u32, 44_100] {
            for miss in [Miss::EngineLock, Miss::Skipped] {
                let host = 48_000.0f32;
                let (reference, _) = render_missing_one(
                    &tone_take(take_rate, 3_000.0).fork_playback(),
                    host,
                    block,
                    blocks,
                    missed,
                    Miss::None,
                );
                let (heard, playheads) = render_missing_one(
                    &tone_take(take_rate, 3_000.0).fork_playback(),
                    host,
                    block,
                    blocks,
                    missed,
                    miss,
                );
                for b in missed + 1..blocks {
                    let expected = 0.25 + ((b + 1) * block) as f64 / host as f64;
                    let late = (expected - playheads[b]) * host as f64;
                    let worst = reference[b]
                        .iter()
                        .zip(&heard[b])
                        .fold(0.0f32, |worst, (a, h)| worst.max((a - h).abs()));
                    if late.abs() >= 1.0 || worst >= 1.0e-3 {
                        late_blocks.push(format!(
                            "{take_rate} Hz take, {miss:?}: block {b} is {late:.1} frames \
                             late, output off by {worst:.3}"
                        ));
                    }
                }
            }
        }
        assert!(late_blocks.is_empty(), "{}", late_blocks.join("\n"));
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
