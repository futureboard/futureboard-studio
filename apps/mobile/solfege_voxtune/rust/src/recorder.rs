//! Live vocal recording from the device microphone.
//!
//! The AAudio input callback only copies samples into a lock-free SPSC ring
//! (no allocation, no locks). The UI thread drains it on every frame
//! (`Recorder::poll`), appending to the take and updating the level meter
//! and a live pitch reading. Takes are saved as 48 kHz mono WAV.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::audio_io::{encode_mono_wav_i16, resample_mono_cubic};
use crate::detect::Detector;

/// Longest take; recording stops by itself at this length.
pub const MAX_SECONDS: f64 = 600.0;
/// Ring capacity: how long the UI thread may stall without losing audio.
const RING_SECONDS: usize = 8;

/// Single-producer / single-consumer sample ring. The audio thread pushes,
/// the UI thread pops; both sides only touch atomics.
pub struct Ring {
    data: Box<[AtomicU32]>,
    write: AtomicUsize,
    read: AtomicUsize,
    dropped: AtomicU64,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: (0..capacity.max(1)).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Producer side (audio thread). Samples that don't fit are counted, not
    /// waited for.
    pub fn push(&self, samples: &[f32]) {
        let cap = self.data.len();
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        let n = samples.len().min(cap - (w - r));
        for (i, s) in samples[..n].iter().enumerate() {
            self.data[(w + i) % cap].store(s.to_bits(), Ordering::Relaxed);
        }
        self.write.store(w + n, Ordering::Release);
        if n < samples.len() {
            self.dropped
                .fetch_add((samples.len() - n) as u64, Ordering::Relaxed);
        }
    }

    /// Consumer side (UI thread): moves everything available into `out`.
    pub fn pop_into(&self, out: &mut Vec<f32>) -> usize {
        let cap = self.data.len();
        let r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        out.extend((r..w).map(|i| f32::from_bits(self.data[i % cap].load(Ordering::Relaxed))));
        self.read.store(w, Ordering::Release);
        w - r
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Snapshot for the recording screen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Status {
    pub recording: bool,
    pub seconds: f64,
    /// Peak level with a short decay, 0..1.
    pub level: f32,
    /// Sung pitch in Hz, 0 when nothing voiced is detected.
    pub pitch_hz: f32,
    /// A sample at or above full scale arrived since the last poll.
    pub clipped: bool,
}

pub struct Recorder {
    ring: Arc<Ring>,
    sample_rate: u32,
    take: Vec<f32>,
    scratch: Vec<f32>,
    detector: Detector,
    level: f32,
    pitch_hz: f32,
    recording: bool,
    #[cfg(target_os = "android")]
    input: Option<android::Input>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            ring: Arc::new(Ring::new(48_000 * RING_SECONDS)),
            sample_rate: 48_000,
            take: Vec::new(),
            scratch: Vec::new(),
            detector: Self::detector(48_000),
            level: 0.0,
            pitch_hz: 0.0,
            recording: false,
            #[cfg(target_os = "android")]
            input: None,
        }
    }

    fn detector(sample_rate: u32) -> Detector {
        let mut d = Detector::new(sample_rate as f32, 55.0);
        d.set_range(65.0, 1_200.0);
        d
    }

    /// Opens the microphone and starts a new take.
    pub fn start(&mut self) -> Result<(), String> {
        self.stop();
        #[cfg(target_os = "android")]
        {
            // A fresh ring per take, so a previous stream can never write
            // into this one. Sized for 8 s even at 96 kHz device rates.
            self.ring = Arc::new(Ring::new(48_000 * RING_SECONDS * 2));
            let input = android::Input::open(self.ring.clone())?;
            self.begin(input.sample_rate);
            if !input.start() {
                self.recording = false;
                return Err("Could not start the microphone".into());
            }
            self.input = Some(input);
            Ok(())
        }
        #[cfg(not(target_os = "android"))]
        {
            Err("Live recording needs an Android device".into())
        }
    }

    /// Starts a take at `sample_rate` without opening a device; audio then
    /// arrives through [`Recorder::feed`]. Used by tests and by `start`.
    pub fn begin(&mut self, sample_rate: u32) {
        let mut discard = Vec::new();
        self.ring.pop_into(&mut discard);
        self.sample_rate = sample_rate.max(8_000);
        self.take.clear();
        self.take.reserve(self.sample_rate as usize * 30);
        self.detector = Self::detector(self.sample_rate);
        self.level = 0.0;
        self.pitch_hz = 0.0;
        self.recording = true;
    }

    /// Producer entry point (what the device callback does).
    pub fn feed(&self, samples: &[f32]) {
        self.ring.push(samples);
    }

    /// Drains new audio into the take and updates meter + live pitch.
    pub fn poll(&mut self) -> Status {
        let mut fresh = std::mem::take(&mut self.scratch);
        fresh.clear();
        self.ring.pop_into(&mut fresh);
        let mut clipped = false;
        if self.recording && !fresh.is_empty() {
            let room = (MAX_SECONDS * self.sample_rate as f64) as usize;
            let keep = fresh.len().min(room.saturating_sub(self.take.len()));
            self.take.extend_from_slice(&fresh[..keep]);

            let mut peak = 0.0f32;
            for &s in &fresh {
                peak = peak.max(s.abs());
                if let Some(est) = self.detector.push(s, 0.15) {
                    self.pitch_hz = est
                        .period
                        .map_or(0.0, |p| self.sample_rate as f32 / p.max(1.0));
                }
            }
            clipped = peak >= 0.99;
            self.level = peak.max(self.level * 0.8);
            if keep < fresh.len() {
                self.stop();
            }
        } else {
            self.level *= 0.8;
        }
        self.scratch = fresh;
        Status {
            recording: self.recording,
            seconds: self.take.len() as f64 / self.sample_rate as f64,
            level: self.level,
            pitch_hz: if self.recording { self.pitch_hz } else { 0.0 },
            clipped,
        }
    }

    /// Stops the microphone; the take is kept until saved or discarded.
    pub fn stop(&mut self) {
        #[cfg(target_os = "android")]
        {
            // Dropping closes the stream (and waits out its callback).
            self.input = None;
        }
        if self.recording {
            self.recording = false;
            let mut rest = Vec::new();
            self.ring.pop_into(&mut rest);
            let room = (MAX_SECONDS * self.sample_rate as f64) as usize;
            let keep = rest.len().min(room.saturating_sub(self.take.len()));
            self.take.extend_from_slice(&rest[..keep]);
        }
    }

    pub fn discard(&mut self) {
        self.stop();
        self.take = Vec::new();
    }

    pub fn take_seconds(&self) -> f64 {
        self.take.len() as f64 / self.sample_rate as f64
    }

    pub fn dropped_samples(&self) -> u64 {
        self.ring.dropped()
    }

    /// Writes the take as 16-bit mono WAV at `target_rate` (resampled from
    /// the device rate when they differ).
    pub fn save_wav(&self, path: &str, target_rate: u32) -> Result<(), String> {
        if self.take.is_empty() {
            return Err("Nothing was recorded".into());
        }
        let samples = if self.sample_rate == target_rate {
            std::borrow::Cow::Borrowed(&self.take[..])
        } else {
            std::borrow::Cow::Owned(resample_mono_cubic(
                &self.take,
                self.sample_rate,
                target_rate,
            ))
        };
        std::fs::write(path, encode_mono_wav_i16(&samples, target_rate))
            .map_err(|e| format!("Could not write {path}: {e}"))
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    use super::Ring;
    use crate::aaudio::*;

    struct Context {
        ring: Arc<Ring>,
    }

    unsafe extern "C" fn data_callback(
        _stream: *mut Stream,
        user: *mut c_void,
        audio: *mut c_void,
        frames: i32,
    ) -> i32 {
        let ctx = unsafe { &*(user as *const Context) };
        let input =
            unsafe { std::slice::from_raw_parts(audio as *const f32, frames.max(0) as usize) };
        let _ = catch_unwind(AssertUnwindSafe(|| ctx.ring.push(input)));
        CALLBACK_CONTINUE
    }

    unsafe extern "C" fn error_callback(_s: *mut Stream, _u: *mut c_void, _e: i32) {
        // Device lost mid-take: the samples captured so far stay in the take;
        // polling simply stops receiving more.
    }

    pub struct Input {
        api: &'static Api,
        stream: *mut Stream,
        ctx: *mut Context,
        pub sample_rate: u32,
    }

    unsafe impl Send for Input {}

    impl Input {
        /// Mono float capture. Tries the unprocessed preset first (no AGC /
        /// noise suppression, best for singing), then voice recognition,
        /// then the device default; 48 kHz first, then the device's rate.
        pub fn open(ring: Arc<Ring>) -> Result<Input, String> {
            let api = Api::get().ok_or("AAudio is not available (requires Android 8.0+)")?;
            let presets = [
                Some(INPUT_PRESET_UNPROCESSED),
                Some(INPUT_PRESET_VOICE_RECOGNITION),
                None,
            ];
            let mut last = 0;
            for preset in presets {
                for rate in [48_000, 0] {
                    match Self::try_open(api, ring.clone(), preset, rate) {
                        Ok(input) => return Ok(input),
                        Err(code) => last = code,
                    }
                }
            }
            Err(format!(
                "Could not open the microphone (AAudio {last}). Check the microphone permission."
            ))
        }

        fn try_open(
            api: &'static Api,
            ring: Arc<Ring>,
            preset: Option<i32>,
            rate: i32,
        ) -> Result<Input, i32> {
            let ctx = Box::into_raw(Box::new(Context { ring }));
            unsafe {
                let mut builder: *mut Builder = std::ptr::null_mut();
                let r = (api.create_builder)(&mut builder);
                if r != AAUDIO_OK || builder.is_null() {
                    drop(Box::from_raw(ctx));
                    return Err(r);
                }
                (api.set_direction)(builder, DIRECTION_INPUT);
                if rate > 0 {
                    (api.set_sample_rate)(builder, rate);
                }
                (api.set_channel_count)(builder, 1);
                (api.set_format)(builder, FORMAT_PCM_FLOAT);
                (api.set_sharing_mode)(builder, SHARING_MODE_SHARED);
                (api.set_performance_mode)(builder, PERFORMANCE_MODE_NONE);
                if let (Some(p), Some(set)) = (preset, api.set_input_preset) {
                    set(builder, p);
                }
                (api.set_data_callback)(builder, data_callback, ctx as *mut c_void);
                (api.set_error_callback)(builder, error_callback, ctx as *mut c_void);
                let mut stream: *mut Stream = std::ptr::null_mut();
                let r = (api.open_stream)(builder, &mut stream);
                (api.delete_builder)(builder);
                if r != AAUDIO_OK || stream.is_null() {
                    drop(Box::from_raw(ctx));
                    return Err(r);
                }
                let input = Input {
                    api,
                    stream,
                    ctx,
                    sample_rate: (api.get_sample_rate)(stream).max(8_000) as u32,
                };
                if (api.get_channel_count)(stream) != 1
                    || (api.get_format)(stream) != FORMAT_PCM_FLOAT
                {
                    return Err(-1); // dropped: closes the stream
                }
                Ok(input)
            }
        }

        pub fn start(&self) -> bool {
            unsafe { (self.api.request_start)(self.stream) == AAUDIO_OK }
        }
    }

    impl Drop for Input {
        fn drop(&mut self) {
            unsafe {
                (self.api.request_stop)(self.stream);
                (self.api.close)(self.stream);
                drop(Box::from_raw(self.ctx));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, rate: u32, seconds: f32, amp: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn ring_passes_samples_in_order_and_counts_overflow() {
        let ring = Ring::new(8);
        ring.push(&[1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        assert_eq!(ring.pop_into(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        ring.push(&[0.0; 10]); // only 8 fit
        assert_eq!(ring.dropped(), 2);
        out.clear();
        assert_eq!(ring.pop_into(&mut out), 8);
    }

    #[test]
    fn take_accumulates_with_live_level_and_pitch() {
        let mut rec = Recorder::new();
        rec.begin(48_000);
        let tone = sine(220.0, 48_000, 0.5, 0.5);
        let mut status = Status::default();
        for chunk in tone.chunks(800) {
            rec.feed(chunk); // ~one UI frame of audio per poll
            status = rec.poll();
        }
        assert!(status.recording);
        assert!((status.seconds - 0.5).abs() < 1e-3);
        assert!(status.level > 0.4 && status.level <= 0.5 + 1e-3);
        let cents = 1200.0 * (status.pitch_hz / 220.0).log2();
        assert!(cents.abs() < 10.0, "live pitch {} Hz", status.pitch_hz);
        assert!(!status.clipped);

        rec.feed(&[1.0; 64]);
        assert!(rec.poll().clipped);
    }

    #[test]
    fn saved_take_is_resampled_to_the_target_rate() {
        let mut rec = Recorder::new();
        rec.begin(44_100);
        rec.feed(&sine(330.0, 44_100, 1.0, 0.4));
        rec.stop(); // stop drains what's left
        assert!((rec.take_seconds() - 1.0).abs() < 1e-3);
        let path = std::env::temp_dir().join("voxtune_rec_test.wav");
        rec.save_wav(path.to_str().unwrap(), 48_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(&bytes[..4], b"RIFF");
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        assert_eq!(rate, 48_000);
        let frames = (bytes.len() - 44) / 2;
        assert!((frames as i64 - 48_000).abs() < 10, "{frames} frames");
    }

    #[test]
    fn empty_take_is_not_saved() {
        let mut rec = Recorder::new();
        rec.begin(48_000);
        rec.stop();
        assert!(rec.save_wav("unused.wav", 48_000).is_err());
    }
}
