//! Audio output: a decode thread feeding a lock-free ring buffer that the device callback drains.
//!
//! Seeks never block the real-time callback. Every seek / speed change starts an *epoch*:
//!
//! 1. the controller bumps `epoch_req`: the callback goes silent at once;
//! 2. the decode thread gets the command, notes how many samples it has pushed so far (the
//!    epoch's `boundary`) and publishes the epoch as ready;
//! 3. the callback discards everything before `boundary`, resets its frame counter and plays the
//!    new epoch. The clock follows the callback's frame counter (see [`crate::clock`]).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mkff_codecs::MediaFile;

/// Where audio goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AudioOutput {
    /// The system's default output device (falls back to `Null`-less video-only playback with a
    /// system clock when there is none).
    #[default]
    Device,
    /// A simulated device that consumes samples in real time and discards them: the full audio
    /// path and audio clock, without sound hardware (headless runs, tests).
    Null,
    /// No audio: video plays against the system clock.
    Disabled,
}

/// Frames decoded per request to the media file.
const CHUNK_FRAMES: usize = 2048;
/// Ring buffer length.
const RING_SECONDS: f64 = 0.5;
/// Volume changes ramp over this long (no clicks).
const RAMP_SECONDS: f32 = 0.015;
/// The simulated device's period.
const NULL_PERIOD: Duration = Duration::from_millis(10);

/// What the callback last did, for the clock.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Timing {
    pub epoch: u64,
    /// Frames of this epoch played before the last callback.
    pub before: u64,
    /// Frames of this epoch the last callback wrote.
    pub frames: u64,
    /// When the last callback ran, seconds since [`AudioShared::origin`].
    pub callback_secs: f64,
    /// Output latency the device reported for it.
    pub latency_secs: f64,
}

/// State shared between the controller, the decode thread and the device callback.
pub(crate) struct AudioShared {
    pub rate: u32,
    pub channels: u16,
    pub device_name: String,
    origin: Instant,
    /// Epoch the controller wants (the callback is silent until it plays it).
    epoch_req: AtomicU64,
    /// Epoch the decode thread is producing, and its first sample's position in the ring stream.
    epoch_ready: AtomicU64,
    boundary: AtomicU64,
    paused: AtomicBool,
    /// Target gain (f32 bits).
    gain: AtomicU32,
    /// Seqlock over the `t_*` timing fields.
    seq: AtomicU64,
    t_epoch: AtomicU64,
    t_before: AtomicU64,
    t_frames: AtomicU64,
    t_callback_ns: AtomicU64,
    t_latency_ns: AtomicU64,
    /// The last epoch the decode thread produced to the end of the media.
    eos_epoch: AtomicU64,
    /// The last epoch the callback played to the end (ran dry after `eos_epoch`).
    drained_epoch: AtomicU64,
    pub underruns: AtomicU64,
    pub decode_errors: AtomicU64,
    pub device_failed: AtomicBool,
}

impl AudioShared {
    fn new(rate: u32, channels: u16, device_name: String) -> AudioShared {
        AudioShared {
            rate,
            channels,
            device_name,
            origin: Instant::now(),
            epoch_req: AtomicU64::new(0),
            epoch_ready: AtomicU64::new(0),
            boundary: AtomicU64::new(0),
            paused: AtomicBool::new(true),
            gain: AtomicU32::new(1.0f32.to_bits()),
            seq: AtomicU64::new(0),
            t_epoch: AtomicU64::new(0),
            t_before: AtomicU64::new(0),
            t_frames: AtomicU64::new(0),
            t_callback_ns: AtomicU64::new(0),
            t_latency_ns: AtomicU64::new(0),
            eos_epoch: AtomicU64::new(0),
            drained_epoch: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            device_failed: AtomicBool::new(false),
        }
    }

    pub fn since_origin_secs(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    /// A consistent copy of the callback's timing.
    pub fn timing(&self) -> Timing {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let t = Timing {
                epoch: self.t_epoch.load(Ordering::Relaxed),
                before: self.t_before.load(Ordering::Relaxed),
                frames: self.t_frames.load(Ordering::Relaxed),
                callback_secs: self.t_callback_ns.load(Ordering::Relaxed) as f64 * 1e-9,
                latency_secs: self.t_latency_ns.load(Ordering::Relaxed) as f64 * 1e-9,
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return t;
            }
        }
    }

    fn publish(&self, epoch: u64, before: u64, frames: u64, latency: Duration) {
        let now = self.origin.elapsed().as_nanos() as u64;
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.t_epoch.store(epoch, Ordering::Relaxed);
        self.t_before.store(before, Ordering::Relaxed);
        self.t_frames.store(frames, Ordering::Relaxed);
        self.t_callback_ns.store(now, Ordering::Relaxed);
        self.t_latency_ns
            .store(latency.as_nanos() as u64, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// Seconds since the device last asked for samples.
    pub fn callback_age(&self) -> f64 {
        self.since_origin_secs() - self.timing().callback_secs
    }

    /// The device played everything up to the end of the media (this epoch).
    pub fn drained(&self) -> bool {
        let req = self.epoch_req.load(Ordering::Acquire);
        req != 0 && self.drained_epoch.load(Ordering::Acquire) == req
    }
}

/// The device side: fills output buffers from the ring.
struct Renderer {
    consumer: rtrb::Consumer<f32>,
    shared: Arc<AudioShared>,
    channels: usize,
    epoch: u64,
    /// Samples popped from the ring, ever (compared against epoch boundaries).
    popped: u64,
    /// Frames of the current epoch played.
    played: u64,
    /// A new epoch waits for one chunk of samples before it starts (no stutter after seeks).
    priming: bool,
    /// Samples a new epoch waits for.
    prime: usize,
    gain: f32,
    ramp_step: f32,
    /// Scratch for integer sample formats.
    scratch: Vec<f32>,
}

impl Renderer {
    fn render(&mut self, out: &mut [f32], latency: Duration) {
        let sh = Arc::clone(&self.shared);
        let ch = self.channels.max(1);
        let ready = sh.epoch_ready.load(Ordering::Acquire);
        if ready != self.epoch {
            // new epoch: drop the old one's samples
            let boundary = sh.boundary.load(Ordering::Acquire);
            let stale = boundary.saturating_sub(self.popped) as usize;
            let n = stale.min(self.consumer.slots());
            if n > 0 {
                if let Ok(chunk) = self.consumer.read_chunk(n) {
                    chunk.commit_all();
                }
                self.popped += n as u64;
            }
            if self.popped >= boundary {
                self.epoch = ready;
                self.played = 0;
                self.priming = true;
                sh.publish(self.epoch, 0, 0, latency);
            }
        }
        let wanted = sh.epoch_req.load(Ordering::Acquire);
        if self.priming
            && (self.consumer.slots() >= self.prime
                || sh.eos_epoch.load(Ordering::Acquire) == self.epoch)
        {
            self.priming = false;
        }
        if self.epoch != ready
            || self.epoch != wanted
            || self.priming
            || sh.paused.load(Ordering::Acquire)
        {
            out.fill(0.0);
            sh.publish(self.epoch, self.played, 0, latency);
            return;
        }

        let frames_out = out.len() / ch;
        let avail = self.consumer.slots() / ch;
        let frames = frames_out.min(avail);
        let target = f32::from_bits(sh.gain.load(Ordering::Relaxed));
        let ramp_len = (RAMP_SECONDS * sh.rate as f32).max(1.0);
        if (target - self.gain).abs() > 1e-6 {
            self.ramp_step = (target - self.gain) / ramp_len;
        }
        if frames > 0 {
            if let Ok(chunk) = self.consumer.read_chunk(frames * ch) {
                let (a, b) = chunk.as_slices();
                for (i, frame) in out[..frames * ch].chunks_exact_mut(ch).enumerate() {
                    if self.ramp_step != 0.0 {
                        self.gain += self.ramp_step;
                        if (self.ramp_step > 0.0 && self.gain >= target)
                            || (self.ramp_step < 0.0 && self.gain <= target)
                        {
                            self.gain = target;
                            self.ramp_step = 0.0;
                        }
                    }
                    for (c, s) in frame.iter_mut().enumerate() {
                        let k = i * ch + c;
                        let v = if k < a.len() { a[k] } else { b[k - a.len()] };
                        // a downmix can exceed full scale: clip rather than wrap
                        *s = (v * self.gain).clamp(-1.0, 1.0);
                    }
                }
                chunk.commit_all();
                self.popped += (frames * ch) as u64;
            }
        }
        out[frames * ch..].fill(0.0);
        if frames < frames_out {
            if sh.eos_epoch.load(Ordering::Acquire) == self.epoch {
                sh.drained_epoch.store(self.epoch, Ordering::Release);
            } else {
                sh.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        let before = self.played;
        self.played += frames as u64;
        sh.publish(self.epoch, before, frames as u64, latency);
    }

    /// Render into a device buffer of another sample type.
    fn render_as<T: cpal::SizedSample + cpal::FromSample<f32>>(
        &mut self,
        out: &mut [T],
        latency: Duration,
    ) {
        let mut scratch = std::mem::take(&mut self.scratch);
        if scratch.len() < out.len() {
            scratch.resize(out.len(), 0.0);
        }
        self.render(&mut scratch[..out.len()], latency);
        for (o, s) in out.iter_mut().zip(&scratch) {
            *o = T::from_sample(*s);
        }
        self.scratch = scratch;
    }
}

enum Command {
    /// Produce from `position` (seconds) at `speed` for `epoch`.
    Start {
        epoch: u64,
        position: f64,
        speed: f64,
    },
    Quit,
}

/// The decode side: reads the media and fills the ring.
struct Producer {
    media: MediaFile,
    stream: usize,
    ring: rtrb::Producer<f32>,
    shared: Arc<AudioShared>,
    rx: Receiver<Command>,
    /// Samples pushed, ever.
    pushed: u64,
    /// Source → device channel gains (rebuilt when the source channel count changes).
    mix: Vec<Vec<f32>>,
    duration: f64,
}

struct EpochState {
    epoch: u64,
    rate: u32,
    next: i64,
    end: i64,
}

impl Producer {
    fn run(mut self) {
        let mut cur: Option<EpochState> = None;
        let mut idle = true;
        loop {
            let cmd = if idle {
                match self.rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                self.rx.try_recv().ok()
            };
            // coalesce: only the newest command matters
            let mut latest = cmd;
            while let Ok(c) = self.rx.try_recv() {
                latest = Some(c);
            }
            match latest {
                Some(Command::Quit) => return,
                Some(Command::Start {
                    epoch,
                    position,
                    speed,
                }) => {
                    let rate = source_rate(self.shared.rate, speed);
                    let end = (self.duration * f64::from(rate)).ceil() as i64;
                    let next = ((position * f64::from(rate)).round() as i64).min(end);
                    cur = Some(EpochState {
                        epoch,
                        rate,
                        next,
                        end,
                    });
                    self.shared.boundary.store(self.pushed, Ordering::Release);
                    self.shared.epoch_ready.store(epoch, Ordering::Release);
                }
                None => {}
            }

            let Some(st) = cur.as_mut() else {
                idle = true;
                continue;
            };
            if st.next >= st.end {
                self.shared.eos_epoch.store(st.epoch, Ordering::Release);
                idle = true;
                continue;
            }
            let ch = usize::from(self.shared.channels.max(1));
            let free = self.ring.slots() / ch;
            if free < CHUNK_FRAMES {
                // ring full: wait for the device (or a command)
                idle = false;
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
            idle = false;
            let n = CHUNK_FRAMES.min((st.end - st.next) as usize);
            let decoded = catch_unwind(AssertUnwindSafe(|| {
                self.media.audio_stream(self.stream, st.next, n, st.rate)
            }));
            let buf = match decoded {
                Ok(Ok(b)) => Some(b),
                Ok(Err(e)) => {
                    log::warn!("audio decode at sample {}: {e}", st.next);
                    self.shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                    None
                }
                Err(_) => {
                    log::error!("audio decoder panicked at sample {}", st.next);
                    self.shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
            if self.shared.epoch_req.load(Ordering::Acquire) != st.epoch {
                // a newer epoch is on its way: don't bother pushing this one
                continue;
            }
            let Ok(mut chunk) = self.ring.write_chunk_uninit(n * ch) else {
                continue;
            };
            let (a, b) = chunk.as_mut_slices();
            let mut k = 0usize;
            let src = buf.as_ref().map(|b| &b.channels[..]).unwrap_or(&[]);
            if self.mix.first().map(Vec::len) != Some(src.len()) {
                self.mix = mix_matrix(src.len(), ch);
            }
            for i in 0..n {
                for row in &self.mix {
                    let v: f32 = row
                        .iter()
                        .zip(src)
                        .filter(|(g, _)| **g != 0.0)
                        .map(|(g, s)| g * s.get(i).copied().unwrap_or(0.0))
                        .sum();
                    if k < a.len() {
                        a[k].write(v);
                    } else {
                        b[k - a.len()].write(v);
                    }
                    k += 1;
                }
            }
            // SAFETY: every slot of the chunk was written above.
            unsafe { chunk.commit_all() };
            self.pushed += (n * ch) as u64;
            st.next += n as i64;
        }
    }
}

/// The source rate that plays back at `speed` on a device running at `device_rate`.
pub(crate) fn source_rate(device_rate: u32, speed: f64) -> u32 {
    let speed = if speed.is_finite() && speed > 0.0 {
        speed
    } else {
        1.0
    };
    ((f64::from(device_rate) / speed).round() as u32).max(1)
}

/// −3 dB, the ITU-R BS.775 downmix coefficient.
const K: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Gains `m[out][in]` from `src` source channels (WAV / SMPTE order: L R C LFE Ls Rs …, as
/// mkff-codecs delivers them) to `dev` device channels.
///
/// - A device with at least as many channels plays the source as is (mono: on the front pair).
/// - Fewer: the ITU-R BS.775 stereo downmix (centre and surrounds at −3 dB, LFE dropped) on the
///   front pair; a mono device gets its average.
pub(crate) fn mix_matrix(src: usize, dev: usize) -> Vec<Vec<f32>> {
    let mut m = vec![vec![0.0f32; src]; dev];
    if src == 0 || dev == 0 {
        return m;
    }
    if src == 1 {
        for row in m.iter_mut().take(2) {
            row[0] = 1.0;
        }
        return m;
    }
    if dev >= src {
        for (i, row) in m.iter_mut().enumerate().take(src) {
            row[i] = 1.0;
        }
        return m;
    }
    // stereo downmix: (left gain, right gain) per source channel
    let lr: Vec<(f32, f32)> = (0..src)
        .map(|c| match (src, c) {
            (_, 0) => (1.0, 0.0),
            (_, 1) => (0.0, 1.0),
            // quad: L R Ls Rs
            (4, 2) => (K, 0.0),
            (4, 3) => (0.0, K),
            // centre
            (_, 2) => (K, K),
            // L R C Ls Rs
            (5, 3) => (K, 0.0),
            (5, 4) => (0.0, K),
            // LFE
            (6..=8, 3) => (0.0, 0.0),
            // 6.1: L R C LFE Cs Ls Rs
            (7, 4) => (K * K, K * K),
            (7, 5) => (K, 0.0),
            (7, 6) => (0.0, K),
            // 5.1 / 7.1 surrounds and backs: left / right alternate
            (6 | 8, c) if c >= 4 => {
                if c % 2 == 0 {
                    (K, 0.0)
                } else {
                    (0.0, K)
                }
            }
            _ => (0.0, 0.0),
        })
        .collect();
    if dev == 1 {
        for (c, (l, r)) in lr.iter().enumerate() {
            m[0][c] = (l + r) * 0.5;
        }
    } else {
        for (c, (l, r)) in lr.iter().enumerate() {
            m[0][c] = *l;
            m[1][c] = *r;
        }
    }
    m
}

/// The running audio output of one [`crate::Player`].
pub(crate) struct AudioEngine {
    pub shared: Arc<AudioShared>,
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
    /// Keeps the device stream (or the simulated device thread) alive.
    _sink: Sink,
    epoch: u64,
}

enum Sink {
    Device(#[allow(dead_code)] cpal::Stream),
    Null {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    },
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Sink::Null { stop, thread } = self {
            stop.store(true, Ordering::Release);
            if let Some(t) = thread.take() {
                let _ = t.join();
            }
        }
    }
}

impl AudioEngine {
    /// Start audio for stream `stream` of `media`.
    pub fn start(
        media: MediaFile,
        stream: usize,
        output: AudioOutput,
        duration: f64,
    ) -> Result<AudioEngine, String> {
        let (rate, channels, device) = match output {
            AudioOutput::Disabled => return Err("audio disabled".into()),
            AudioOutput::Null => (48_000, 2, None),
            AudioOutput::Device => {
                let (dev, cfg) = default_device()?;
                (cfg.sample_rate(), cfg.channels(), Some((dev, cfg)))
            }
        };
        let name = match &device {
            Some((dev, _)) => {
                use cpal::traits::DeviceTrait;
                dev.description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_else(|_| "default output".into())
            }
            None => "null (simulated)".into(),
        };
        let shared = Arc::new(AudioShared::new(rate, channels, name));
        let ring_len = ((f64::from(rate) * RING_SECONDS) as usize).max(CHUNK_FRAMES * 2)
            * usize::from(channels.max(1));
        let (ring_tx, ring_rx) = rtrb::RingBuffer::new(ring_len);
        let mut renderer = Renderer {
            consumer: ring_rx,
            shared: Arc::clone(&shared),
            channels: usize::from(channels.max(1)),
            epoch: 0,
            popped: 0,
            played: 0,
            priming: false,
            prime: CHUNK_FRAMES * usize::from(channels.max(1)),
            gain: 1.0,
            ramp_step: 0.0,
            scratch: Vec::with_capacity(16_384),
        };

        let sink = match device {
            Some((dev, cfg)) => Sink::Device(build_stream(dev, cfg, renderer, &shared)?),
            None => {
                let stop = Arc::new(AtomicBool::new(false));
                let stop2 = Arc::clone(&stop);
                let frames = (rate as usize * NULL_PERIOD.as_millis() as usize) / 1000;
                let thread = std::thread::Builder::new()
                    .name("mkff-audio-null".into())
                    .spawn(move || {
                        let mut buf = vec![0f32; frames * usize::from(channels)];
                        // Like hardware: one period of buffered latency, and periods missed
                        // while this thread slept late are rendered at once instead of lost
                        // (up to 10 of them, e.g. after a suspend).
                        let mut next = Instant::now();
                        while !stop2.load(Ordering::Acquire) {
                            let mut periods = 0;
                            while next <= Instant::now() && periods < 10 {
                                renderer.render(&mut buf, NULL_PERIOD);
                                next += NULL_PERIOD;
                                periods += 1;
                            }
                            if periods == 10 {
                                next = Instant::now();
                            }
                            if let Some(d) = next.checked_duration_since(Instant::now()) {
                                std::thread::sleep(d);
                            }
                        }
                    })
                    .map_err(|e| format!("audio thread: {e}"))?;
                Sink::Null {
                    stop,
                    thread: Some(thread),
                }
            }
        };

        let (tx, rx) = mpsc::channel();
        let producer = Producer {
            media,
            stream,
            ring: ring_tx,
            shared: Arc::clone(&shared),
            rx,
            pushed: 0,
            mix: Vec::new(),
            duration,
        };
        let thread = std::thread::Builder::new()
            .name("mkff-audio-decode".into())
            .spawn(move || producer.run())
            .map_err(|e| format!("audio decode thread: {e}"))?;
        Ok(AudioEngine {
            shared,
            tx,
            thread: Some(thread),
            _sink: sink,
            epoch: 0,
        })
    }

    /// Start a new epoch at `position`; returns (epoch, source sample rate) for the clock.
    pub fn restart(&mut self, position: f64, speed: f64) -> (u64, u32) {
        self.epoch += 1;
        let epoch = self.epoch;
        self.shared.epoch_req.store(epoch, Ordering::Release);
        let _ = self.tx.send(Command::Start {
            epoch,
            position,
            speed,
        });
        (epoch, source_rate(self.shared.rate, speed))
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    pub fn set_gain(&self, gain: f32) {
        self.shared
            .gain
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn default_device() -> Result<(cpal::Device, cpal::SupportedStreamConfig), String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no audio output device".to_string())?;
    let config = device
        .default_output_config()
        .map_err(|e| format!("audio device config: {e}"))?;
    Ok((device, config))
}

fn build_stream(
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    mut renderer: Renderer,
    shared: &Arc<AudioShared>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use cpal::SampleFormat as F;

    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let err_shared = Arc::clone(shared);
    let on_error = move |e: cpal::Error| {
        log::error!("audio device: {e}");
        err_shared.device_failed.store(true, Ordering::Release);
    };
    let latency = |info: &cpal::OutputCallbackInfo| {
        let ts = info.timestamp();
        ts.playback
            .checked_duration_since(ts.callback)
            .unwrap_or_default()
    };
    macro_rules! build {
        ($t:ty) => {
            device.build_output_stream(
                stream_config,
                move |out: &mut [$t], info: &cpal::OutputCallbackInfo| {
                    renderer.render_as(out, latency(info))
                },
                on_error,
                None,
            )
        };
    }
    let stream = match config.sample_format() {
        F::F32 => device.build_output_stream(
            stream_config,
            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                renderer.render(out, latency(info))
            },
            on_error,
            None,
        ),
        F::F64 => build!(f64),
        F::I16 => build!(i16),
        F::I32 => build!(i32),
        F::U16 => build!(u16),
        F::U32 => build!(u32),
        F::I8 => build!(i8),
        F::U8 => build!(u8),
        other => return Err(format!("unsupported device sample format {other}")),
    }
    .map_err(|e| format!("open audio stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("start audio stream: {e}"))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_mixing() {
        // stereo plays as is, on a stereo or a 5.1 device
        assert_eq!(mix_matrix(2, 2), vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert_eq!(mix_matrix(2, 6)[0], vec![1.0, 0.0]);
        assert_eq!(mix_matrix(2, 6)[2], vec![0.0, 0.0]);
        // mono on both front speakers
        assert_eq!(mix_matrix(1, 2), vec![vec![1.0], vec![1.0]]);
        // 5.1 → stereo: BS.775, LFE dropped
        let m = mix_matrix(6, 2);
        assert_eq!(m[0], vec![1.0, 0.0, K, 0.0, K, 0.0]);
        assert_eq!(m[1], vec![0.0, 1.0, K, 0.0, 0.0, K]);
        // 7.1 → stereo: backs and sides on their side
        let m = mix_matrix(8, 2);
        assert_eq!(m[0], vec![1.0, 0.0, K, 0.0, K, 0.0, K, 0.0]);
        assert_eq!(m[1], vec![0.0, 1.0, K, 0.0, 0.0, K, 0.0, K]);
        // 5.1 → mono: the average of the stereo downmix
        let m = mix_matrix(6, 1);
        assert_eq!(m[0][0], 0.5);
        assert_eq!(m[0][2], K);
        // 5.1 on a 5.1 device: discrete
        assert_eq!(mix_matrix(6, 6)[4], vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        assert!(mix_matrix(0, 2).iter().all(Vec::is_empty));
    }

    #[test]
    fn speed_rates() {
        assert_eq!(source_rate(48_000, 1.0), 48_000);
        assert_eq!(source_rate(48_000, 2.0), 24_000);
        assert_eq!(source_rate(48_000, 0.5), 96_000);
        assert_eq!(source_rate(44_100, f64::NAN), 44_100);
    }

    fn renderer(ring: usize) -> (rtrb::Producer<f32>, Renderer, Arc<AudioShared>) {
        let shared = Arc::new(AudioShared::new(1000, 1, "test".into()));
        let (p, c) = rtrb::RingBuffer::new(ring);
        let r = Renderer {
            consumer: c,
            shared: Arc::clone(&shared),
            channels: 1,
            epoch: 0,
            popped: 0,
            played: 0,
            priming: false,
            prime: 1,
            gain: 1.0,
            ramp_step: 0.0,
            scratch: Vec::new(),
        };
        (p, r, shared)
    }

    #[test]
    fn epochs_drop_stale_samples() {
        let (mut p, mut r, sh) = renderer(64);
        sh.paused.store(false, Ordering::Release);
        // epoch 1: samples 0.25
        sh.epoch_req.store(1, Ordering::Release);
        sh.boundary.store(0, Ordering::Release);
        sh.epoch_ready.store(1, Ordering::Release);
        for _ in 0..8 {
            p.push(0.25).unwrap();
        }
        let mut out = [9.0f32; 4];
        r.render(&mut out, Duration::ZERO);
        assert_eq!(out, [0.25; 4]);
        assert_eq!(sh.timing().before, 0);
        assert_eq!(sh.timing().frames, 4);

        // seek requested: silent until the producer publishes the epoch
        sh.epoch_req.store(2, Ordering::Release);
        r.render(&mut out, Duration::ZERO);
        assert_eq!(out, [0.0; 4]);

        // producer: the 4 remaining 0.25 samples are stale, then epoch 2 data
        sh.boundary.store(8, Ordering::Release);
        sh.epoch_ready.store(2, Ordering::Release);
        for _ in 0..4 {
            p.push(0.5).unwrap();
        }
        r.render(&mut out, Duration::ZERO);
        assert_eq!(out, [0.5; 4]);
        let t = sh.timing();
        assert_eq!((t.epoch, t.before, t.frames), (2, 0, 4));

        // underrun: silence, no frames counted beyond what was there
        r.render(&mut out, Duration::ZERO);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(sh.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(sh.timing().before, 4);
        assert_eq!(sh.timing().frames, 0);
    }

    #[test]
    fn paused_consumes_nothing_and_volume_ramps() {
        let (mut p, mut r, sh) = renderer(4096);
        sh.epoch_req.store(1, Ordering::Release);
        sh.epoch_ready.store(1, Ordering::Release);
        for _ in 0..2000 {
            p.push(1.0).unwrap();
        }
        let mut out = vec![0.0f32; 100];
        r.render(&mut out, Duration::ZERO);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(r.consumer.slots(), 2000);

        sh.paused.store(false, Ordering::Release);
        sh.gain.store(0.0f32.to_bits(), Ordering::Relaxed);
        r.render(&mut out, Duration::ZERO);
        // 15 ms at 1 kHz: ramps to zero over 15 frames, monotonically
        assert!(out[0] > 0.9);
        assert!(out.windows(2).all(|w| w[1] <= w[0]));
        assert_eq!(out[20], 0.0);
    }
}
