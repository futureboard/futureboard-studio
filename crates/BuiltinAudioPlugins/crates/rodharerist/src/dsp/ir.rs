//! Impulse-response cabinet engine — a real convolution alternative to the
//! modeled [`super::cab::Cabinet`], selectable as [`super::CabModel::Ir`].
//!
//! Loading a `.wav` IR decodes, resamples, trims and FFTs it into partition
//! spectra — allocation-heavy control-thread work, exactly like a NAM capture.
//! [`prepare_ir_runtime`] does that off the audio thread and hands back a
//! [`PreparedIrRuntime`] the caller boxes and pushes into [`IrLoader::submit`].
//! The audio thread adopts it at a block boundary
//! ([`IrConvolver::begin_block`]), briefly ducking through the swap, and the
//! retired runtime goes back to the control thread to be dropped — never
//! inside [`IrConvolver::process`].
//!
//! ## Algorithm
//!
//! Uniform-partitioned overlap-save: the IR is cut into [`PARTITION`]-sample
//! chunks, each zero-padded to [`FFT_SIZE`] and transformed once at load time.
//! Every [`PARTITION`] input samples the convolver transforms one
//! double-length input block, pushes it into a frequency-delay line, and
//! accumulates the FDL against the partition spectra. Latency is exactly
//! [`PARTITION`] samples, reported to the host for delay compensation.
//!
//! Three things keep that affordable:
//!
//! - **Both channels ride one transform.** Left goes in the real plane and
//!   right in the imaginary one, so a stereo block costs one forward and one
//!   inverse FFT instead of four. The two real spectra are untangled from the
//!   packed transform with a handful of adds per bin.
//! - **Only half of each spectrum is stored.** A real signal's transform is
//!   conjugate-symmetric, so DC..Nyquist ([`BINS`]) is the whole of the
//!   information — halving both the frequency-delay line and the
//!   multiply-accumulate loop that dominates the cost.
//! - **Real and imaginary parts live in separate planes**, so that loop is a
//!   pair of contiguous `f32` sweeps a compiler can vectorize, rather than a
//!   stride-2 walk over interleaved complex values.
//!
//! Cost still scales with IR length, which is why [`MAX_IR_SECONDS`] caps it
//! and why the inaudible tail is trimmed at load ([`TAIL_ENERGY_FLOOR`]): this
//! is a *cabinet* IR engine, and a uniform partitioning is the wrong shape for
//! a multi-second reverb tail.
//!
//! ## Realtime rules
//!
//! Every buffer — partition spectra, FDL, FFT scratch, block accumulators —
//! is allocated in [`prepare_ir_runtime`] or [`IrConvolver::new`].
//! [`IrConvolver::process`] performs no allocation, locking or logging.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::handoff::{HandoffCell, RetireBin, Retired};
use super::wav::{WavError, parse_wav};

/// Convolution partition size, in samples — also the engine's exact latency.
/// 128 samples is 2.7 ms at 48 kHz: short enough to play through, long enough
/// that the per-block FFT cost stays modest.
pub const PARTITION: usize = 128;

/// Overlap-save transform length: twice the partition.
const FFT_SIZE: usize = PARTITION * 2;

/// Bins kept per partition: DC through Nyquist. Everything above is the
/// conjugate mirror of a real signal's spectrum, so storing it would double
/// both the memory and the accumulate loop for no extra information.
const BINS: usize = FFT_SIZE / 2 + 1;

/// Longest IR the engine keeps, in seconds. A guitar cabinet IR is tens of
/// milliseconds of useful content; anything past this is truncated (with a
/// fade so the cut is not a click) rather than paying uniform-partition cost
/// for a tail this engine is not shaped to run.
pub const MAX_IR_SECONDS: f32 = 0.5;

/// Samples of raised-cosine fade applied at a truncation point.
const TRUNCATE_FADE: usize = 64;

/// Fraction of an IR's total energy allowed to sit in the tail that gets cut.
///
/// Cabinet IR files are routinely padded out to a round length — half a second
/// of file for thirty milliseconds of cabinet — and under a *uniform*
/// partitioning every one of those padded partitions costs the same
/// multiply-accumulate sweep as the ones carrying the sound. `1e-7` is −70 dB
/// of energy: the discarded tail is a hundredth of a percent of the signal,
/// well under the noise floor of the capture itself, and cutting it is worth
/// several times the convolution cost on a typical file.
const TAIL_ENERGY_FLOOR: f32 = 1.0e-7;

/// Never trim below this many samples, however little energy an IR carries —
/// a short, quiet IR is still the cabinet the user chose.
const MIN_TAIL_FRAMES: usize = PARTITION * 2;

/// Shortest IR worth loading — below this a file is a click, not a cabinet.
const MIN_IR_FRAMES: usize = 8;

/// Duck time either side of a runtime swap, in milliseconds.
const SWAP_DUCK_MS: f32 = 3.0;

/// Bounds on the unit-energy normalization gain, so a pathological file cannot
/// produce a silent or explosive cabinet.
const MIN_NORM_GAIN: f32 = 1.0e-3;
const MAX_NORM_GAIN: f32 = 1.0e3;

/// An IR file failed to load.
#[derive(Debug)]
pub enum IrLoadError {
    /// The container itself could not be decoded.
    Wav(WavError),
    /// Decoded fine but holds too few frames to be an impulse response.
    TooShort,
    /// Decoded fine but is entirely silent — convolving with it would mute the
    /// cabinet, which is never what the user meant.
    Silent,
    /// The file's rate is too far from the engine's to resample sensibly.
    UnusableSampleRate { file: f64, engine: f64 },
}

impl std::fmt::Display for IrLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IrLoadError::Wav(e) => write!(f, "IR failed to load: {e}"),
            IrLoadError::TooShort => write!(f, "IR is too short to be an impulse response"),
            IrLoadError::Silent => write!(f, "IR contains only silence"),
            IrLoadError::UnusableSampleRate { file, engine } => write!(
                f,
                "IR was captured at {file} Hz, which is too far from the engine's {engine} Hz"
            ),
        }
    }
}

impl std::error::Error for IrLoadError {}

/// What the host/UI shows about a loaded IR. Serde-serializable so it can
/// travel over the plugin-host IPC as-is, like [`super::NamCaptureInfo`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IrInfo {
    pub name: String,
    /// Rate the file declared, before any resampling.
    pub source_sample_rate: f64,
    /// Frames actually convolved, at the engine's rate — after resampling and
    /// after the inaudible tail past [`TAIL_ENERGY_FLOOR`] is trimmed, so this
    /// is the length the CPU cost scales with rather than the file's.
    pub frames: usize,
    /// True when the file carried two distinct channels (true-stereo IR).
    pub stereo: bool,
    /// True when the file was longer than [`MAX_IR_SECONDS`] and got cut.
    pub truncated: bool,
    /// Engine samples of latency the convolution adds — always [`PARTITION`].
    pub latency_samples: usize,
}

/// A fully-built, ready-to-run IR: partition spectra plus every buffer the
/// audio thread will need, so adopting it allocates nothing.
pub struct PreparedIrRuntime {
    info: IrInfo,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// Partition spectra: `partitions * BINS` bins per channel, real and
    /// imaginary parts in separate planes. Already scaled by `1/FFT_SIZE` so
    /// the round trip comes back at unity.
    h_l: Spectra,
    h_r: Spectra,
    /// Frequency-delay lines of past input blocks, same layout as `h_*`.
    fdl_l: Spectra,
    fdl_r: Spectra,
    /// Partition slot holding the newest input block.
    fdl_head: usize,
    partitions: usize,
    /// The previous block's input samples (the overlap in overlap-save).
    prev_l: Vec<f32>,
    prev_r: Vec<f32>,
    /// The one transform both channels ride: left in the real plane, right in
    /// the imaginary one. `FFT_SIZE` long.
    block: Vec<Complex<f32>>,
    /// Accumulated product of the FDL against the partition spectra, one
    /// half-spectrum per channel.
    acc_l: HalfSpectrum,
    acc_r: HalfSpectrum,
    fft_scratch: Vec<Complex<f32>>,
}

/// One half-spectrum (DC..Nyquist) in split real/imaginary planes.
#[derive(Debug, Clone)]
struct HalfSpectrum {
    re: Vec<f32>,
    im: Vec<f32>,
}

impl HalfSpectrum {
    fn zeros(bins: usize) -> Self {
        Self {
            re: vec![0.0; bins],
            im: vec![0.0; bins],
        }
    }

    fn clear(&mut self) {
        self.re.fill(0.0);
        self.im.fill(0.0);
    }
}

/// `partitions` consecutive half-spectra, indexed by partition slot.
type Spectra = HalfSpectrum;

impl std::fmt::Debug for PreparedIrRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedIrRuntime")
            .field("info", &self.info)
            .field("partitions", &self.partitions)
            .finish_non_exhaustive()
    }
}

impl PreparedIrRuntime {
    pub fn info(&self) -> IrInfo {
        self.info.clone()
    }

    fn clear(&mut self) {
        self.fdl_l.clear();
        self.fdl_r.clear();
        self.fdl_head = 0;
        self.prev_l.fill(0.0);
        self.prev_r.fill(0.0);
    }

    /// Audio thread: convolve one [`PARTITION`]-sample stereo block. Input and
    /// output buffers are the caller's fixed accumulation buffers; nothing
    /// here allocates.
    fn process_block(&mut self, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        self.fdl_head = (self.fdl_head + 1) % self.partitions;
        let head = self.fdl_head;
        let p = self.partitions;

        // Overlap-save input block — the previous partition then this one —
        // with both channels packed into a single complex transform.
        for i in 0..PARTITION {
            self.block[i] = Complex::new(self.prev_l[i], self.prev_r[i]);
            self.block[PARTITION + i] = Complex::new(in_l[i], in_r[i]);
        }
        self.fft
            .process_with_scratch(&mut self.block, &mut self.fft_scratch);

        // Untangle the two real spectra: for X = FFT(l + j·r),
        // L[k] = (X[k] + conj(X[N-k]))/2 and R[k] = -j·(X[k] - conj(X[N-k]))/2.
        let base = head * BINS;
        for k in 0..BINS {
            let a = self.block[k];
            let mirror = self.block[(FFT_SIZE - k) % FFT_SIZE];
            let (b_re, b_im) = (mirror.re, -mirror.im);
            self.fdl_l.re[base + k] = 0.5 * (a.re + b_re);
            self.fdl_l.im[base + k] = 0.5 * (a.im + b_im);
            let d_re = 0.5 * (a.re - b_re);
            let d_im = 0.5 * (a.im - b_im);
            self.fdl_r.re[base + k] = d_im;
            self.fdl_r.im[base + k] = -d_re;
        }

        // Accumulate every partition against the matching past input block.
        // Split planes and a half spectrum make this the vectorizable sweep it
        // needs to be — it is the whole cost of a long IR.
        let Self {
            h_l,
            h_r,
            fdl_l,
            fdl_r,
            acc_l,
            acc_r,
            ..
        } = self;
        acc_l.clear();
        acc_r.clear();
        for part in 0..p {
            let slot = (head + p - part) % p;
            let x = slot * BINS;
            let h = part * BINS;
            accumulate(
                &fdl_l.re[x..x + BINS],
                &fdl_l.im[x..x + BINS],
                &h_l.re[h..h + BINS],
                &h_l.im[h..h + BINS],
                &mut acc_l.re,
                &mut acc_l.im,
            );
            accumulate(
                &fdl_r.re[x..x + BINS],
                &fdl_r.im[x..x + BINS],
                &h_r.re[h..h + BINS],
                &h_r.im[h..h + BINS],
                &mut acc_r.re,
                &mut acc_r.im,
            );
        }

        // Repack the two results into one inverse transform (`acc_l + j·acc_r`),
        // rebuilding the conjugate mirror the half spectrum left implicit.
        for k in 0..BINS {
            self.block[k] = Complex::new(
                self.acc_l.re[k] - self.acc_r.im[k],
                self.acc_l.im[k] + self.acc_r.re[k],
            );
            if k > 0 && k < FFT_SIZE / 2 {
                self.block[FFT_SIZE - k] = Complex::new(
                    self.acc_l.re[k] + self.acc_r.im[k],
                    self.acc_r.re[k] - self.acc_l.im[k],
                );
            }
        }
        self.ifft
            .process_with_scratch(&mut self.block, &mut self.fft_scratch);

        // Overlap-save: only the second half of the block is valid output, and
        // the two channels come back out of the two planes they went in on.
        for i in 0..PARTITION {
            out_l[i] = self.block[PARTITION + i].re;
            out_r[i] = self.block[PARTITION + i].im;
        }
        self.prev_l.copy_from_slice(in_l);
        self.prev_r.copy_from_slice(in_r);
    }
}

/// One partition's complex multiply-accumulate over split real/imaginary
/// planes. Every slice is `BINS` long; the equal lengths are asserted once so
/// the inner loop carries no bounds checks.
#[inline]
fn accumulate(
    x_re: &[f32],
    x_im: &[f32],
    h_re: &[f32],
    h_im: &[f32],
    acc_re: &mut [f32],
    acc_im: &mut [f32],
) {
    let n = BINS;
    let (x_re, x_im) = (&x_re[..n], &x_im[..n]);
    let (h_re, h_im) = (&h_re[..n], &h_im[..n]);
    let (acc_re, acc_im) = (&mut acc_re[..n], &mut acc_im[..n]);
    for k in 0..n {
        acc_re[k] += x_re[k] * h_re[k] - x_im[k] * h_im[k];
        acc_im[k] += x_re[k] * h_im[k] + x_im[k] * h_re[k];
    }
}

/// Offline (control-thread) resample of a whole buffer, cubic-Hermite
/// interpolated. The streaming [`super::rate::RateAdapter`] solves a different
/// problem — this one has the entire signal in hand and no realtime budget.
fn resample_offline(input: &[f32], from_rate: f64, to_rate: f64) -> Vec<f32> {
    if input.is_empty() || (from_rate - to_rate).abs() < 0.5 {
        return input.to_vec();
    }
    let ratio = to_rate / from_rate;
    let out_len = ((input.len() as f64) * ratio).round().max(1.0) as usize;
    let step = 1.0 / ratio;
    let at = |i: isize| -> f32 {
        let clamped = i.clamp(0, input.len() as isize - 1) as usize;
        input[clamped]
    };
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let base = pos.floor();
            let f = (pos - base) as f32;
            let idx = base as isize;
            let p0 = at(idx - 1);
            let p1 = at(idx);
            let p2 = at(idx + 1);
            let p3 = at(idx + 2);
            let c1 = 0.5 * (p2 - p0);
            let c2 = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
            let c3 = 0.5 * (p3 - p0) + 1.5 * (p1 - p2);
            ((c3 * f + c2) * f + c1) * f + p1
        })
        .collect()
}

/// Length of the shortest prefix that still holds all but
/// [`TAIL_ENERGY_FLOOR`] of the pair's energy — where the file stops being a
/// cabinet and starts being padding.
fn audible_length(left: &[f32], right: &[f32], total_energy: f32) -> usize {
    let budget = total_energy * TAIL_ENERGY_FLOOR;
    let mut tail = 0.0f32;
    for i in (0..left.len()).rev() {
        tail += 0.5 * (left[i] * left[i] + right[i] * right[i]);
        if tail > budget {
            return (i + 1).max(MIN_TAIL_FRAMES).min(left.len());
        }
    }
    left.len().min(MIN_TAIL_FRAMES)
}

/// Cut to `max_frames`, fading the last [`TRUNCATE_FADE`] samples so the cut
/// is a decay, not a step. Returns whether anything was actually removed.
fn truncate_with_fade(samples: &mut Vec<f32>, max_frames: usize) -> bool {
    if samples.len() <= max_frames {
        return false;
    }
    samples.truncate(max_frames);
    let fade = TRUNCATE_FADE.min(samples.len());
    let start = samples.len() - fade;
    for (i, sample) in samples[start..].iter_mut().enumerate() {
        let t = (i as f32 + 0.5) / fade as f32;
        // Raised cosine from 1 to 0.
        *sample *= 0.5 * (1.0 + (std::f32::consts::PI * t).cos());
    }
    true
}

/// Parse, resample, trim, normalize and transform a `.wav` IR — all on the
/// control thread. `engine_sample_rate` is the rate the convolution will run
/// at; a file captured at another rate is resampled here, once, rather than
/// per-sample at playback.
pub fn prepare_ir_runtime(
    bytes: &[u8],
    name: String,
    engine_sample_rate: f64,
) -> Result<PreparedIrRuntime, IrLoadError> {
    let audio = parse_wav(bytes).map_err(IrLoadError::Wav)?;
    if !(engine_sample_rate.is_finite() && engine_sample_rate >= 1.0) {
        return Err(IrLoadError::UnusableSampleRate {
            file: audio.sample_rate,
            engine: engine_sample_rate,
        });
    }
    let rate_ratio = audio.sample_rate / engine_sample_rate;
    if !(0.1..=10.0).contains(&rate_ratio) {
        return Err(IrLoadError::UnusableSampleRate {
            file: audio.sample_rate,
            engine: engine_sample_rate,
        });
    }

    let source_sample_rate = audio.sample_rate;
    let stereo = audio.channels >= 2;
    // Only the source frames that can reach the kept length are read and
    // resampled. Resampling the whole file first let a long, low-rate WAV
    // inside the size cap grow tenfold (gigabytes) before being cut.
    let max_frames = ((MAX_IR_SECONDS as f64 * engine_sample_rate) as usize).max(PARTITION);
    let source_frames = (max_frames as f64 * source_sample_rate / engine_sample_rate).ceil() as usize
        // Cubic interpolation reads two frames past the last output.
        + 4;
    let mut left = Vec::new();
    audio.channel_prefix_into(0, source_frames, &mut left);
    let mut left = resample_offline(&left, source_sample_rate, engine_sample_rate);
    let mut right = if stereo {
        let mut right = Vec::new();
        audio.channel_prefix_into(1, source_frames, &mut right);
        resample_offline(&right, source_sample_rate, engine_sample_rate)
    } else {
        left.clone()
    };

    let truncated =
        truncate_with_fade(&mut left, max_frames) | truncate_with_fade(&mut right, max_frames);
    if left.len() < MIN_IR_FRAMES {
        return Err(IrLoadError::TooShort);
    }

    // Unit-energy normalization keeps different IRs at comparable loudness —
    // IR files in the wild are recorded at wildly different levels.
    let energy: f32 = left
        .iter()
        .zip(right.iter())
        .map(|(l, r)| 0.5 * (l * l + r * r))
        .sum();
    if !(energy.is_finite() && energy > 0.0) {
        return Err(IrLoadError::Silent);
    }
    let gain = (1.0 / energy.sqrt()).clamp(MIN_NORM_GAIN, MAX_NORM_GAIN);
    for sample in left.iter_mut().chain(right.iter_mut()) {
        *sample *= gain;
    }

    // Drop the padding past the cabinet. Under a uniform partitioning every
    // trailing block of near-silence costs exactly as much as a block carrying
    // sound, and IR files in the wild are mostly padding. The fade window is
    // kept *past* the audible length so it lands on the sub-floor tail rather
    // than attenuating content the cabinet still needs.
    let audible = audible_length(&left, &right, energy * gain * gain) + TRUNCATE_FADE;
    truncate_with_fade(&mut left, audible);
    truncate_with_fade(&mut right, audible);

    let frames = left.len();
    let partitions = frames.div_ceil(PARTITION).max(1);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let ifft = planner.plan_fft_inverse(FFT_SIZE);
    let scratch_len = fft
        .get_inplace_scratch_len()
        .max(ifft.get_inplace_scratch_len());
    let mut fft_scratch = vec![Complex::new(0.0, 0.0); scratch_len];

    // Fold the inverse transform's 1/N into the IR spectra so the hot path
    // never scales anything.
    let norm = 1.0 / FFT_SIZE as f32;
    let mut build = |channel: &[f32]| -> Spectra {
        let mut spectra = Spectra::zeros(partitions * BINS);
        let mut scratch_block = vec![Complex::new(0.0, 0.0); FFT_SIZE];
        for part in 0..partitions {
            let start = part * PARTITION;
            let end = (start + PARTITION).min(channel.len());
            scratch_block.fill(Complex::new(0.0, 0.0));
            for (i, &sample) in channel[start..end].iter().enumerate() {
                scratch_block[i] = Complex::new(sample * norm, 0.0);
            }
            fft.process_with_scratch(&mut scratch_block, &mut fft_scratch);
            // Keep DC..Nyquist only; the rest is the conjugate mirror.
            for (k, bin) in scratch_block[..BINS].iter().enumerate() {
                spectra.re[part * BINS + k] = bin.re;
                spectra.im[part * BINS + k] = bin.im;
            }
        }
        spectra
    };
    let h_l = build(&left);
    let h_r = build(&right);

    Ok(PreparedIrRuntime {
        info: IrInfo {
            name,
            source_sample_rate,
            frames,
            stereo,
            truncated,
            latency_samples: PARTITION,
        },
        fft,
        ifft,
        h_l,
        h_r,
        fdl_l: Spectra::zeros(partitions * BINS),
        fdl_r: Spectra::zeros(partitions * BINS),
        fdl_head: 0,
        partitions,
        prev_l: vec![0.0; PARTITION],
        prev_r: vec![0.0; PARTITION],
        block: vec![Complex::new(0.0, 0.0); FFT_SIZE],
        acc_l: HalfSpectrum::zeros(BINS),
        acc_r: HalfSpectrum::zeros(BINS),
        fft_scratch,
    })
}

/// The two lock-free hand-off cells between the control side and the audio
/// side — the same pattern [`super::nam::NamChannel`] uses.
pub struct IrChannel {
    /// Control thread → audio thread: a freshly built runtime awaiting adoption.
    pending: HandoffCell<PreparedIrRuntime>,
    /// Audio thread → control thread: retired runtimes awaiting disposal.
    /// Sized so the audio thread always finds a free slot (see
    /// [`super::handoff::RETIRE_SLOTS`]).
    retired: RetireBin<PreparedIrRuntime>,
    /// Control thread → audio thread: drop the active IR once the duck
    /// reaches silence. A later [`IrLoader::submit`] clears it.
    unload: AtomicBool,
}

/// Control thread: drain the retire bin, then queue `runtime`. The drain has
/// to come first: it is what bounds the bin to
/// [`super::handoff::RETIRE_SLOTS`].
fn submit_to(
    channel: &IrChannel,
    runtime: Box<PreparedIrRuntime>,
) -> (Retired<PreparedIrRuntime>, Option<Box<PreparedIrRuntime>>) {
    let retired = channel.retired.drain();
    channel.unload.store(false, Ordering::Release);
    (retired, channel.pending.put(runtime))
}

/// Cloneable control-side handle to an [`IrConvolver`]'s hand-off cells.
///
/// Thread contract: exactly **one** control thread may use the loader at a
/// time (the cells are single-producer/single-consumer per direction).
#[derive(Clone)]
pub struct IrLoader {
    channel: Arc<IrChannel>,
}

impl IrLoader {
    /// Push a freshly-built runtime for the audio thread to adopt at the next
    /// block boundary. Returns every runtime the audio thread has already
    /// retired and a not-yet-adopted one this call displaced, for the caller
    /// to drop. The audio thread never frees a model. Clears a pending
    /// [`Self::unload`].
    pub fn submit(
        &self,
        runtime: Box<PreparedIrRuntime>,
    ) -> (Retired<PreparedIrRuntime>, Option<Box<PreparedIrRuntime>>) {
        submit_to(&self.channel, runtime)
    }

    /// Drop every runtime the audio thread has retired.
    pub fn collect_garbage(&self) {
        drop(self.channel.retired.drain());
    }

    /// IRs the audio thread had to leak because every retire slot was full.
    /// Always 0 unless the retire bound is broken.
    pub fn leaked_models(&self) -> usize {
        self.channel.retired.leaked()
    }

    /// Parse, build and submit a `.wav` IR in one call (control thread —
    /// decoding, resampling and the load-time FFTs all allocate).
    pub fn load_wav(
        &self,
        bytes: &[u8],
        name: impl Into<String>,
        engine_sample_rate: f64,
    ) -> Result<IrInfo, IrLoadError> {
        let prepared = prepare_ir_runtime(bytes, name.into(), engine_sample_rate)?;
        let info = prepared.info();
        self.collect_garbage();
        self.submit(Box::new(prepared));
        Ok(info)
    }

    /// Queue an unload of the active IR. The not-yet-adopted runtime, if any,
    /// is returned for the caller to drop. The audio thread ducks to silence
    /// and then retires the live one — it does not free it.
    pub fn unload(&self) -> Option<Box<PreparedIrRuntime>> {
        let pending = self.channel.pending.take();
        self.channel.unload.store(true, Ordering::Release);
        pending
    }

    /// Unload whatever IR is active, returning the cabinet slot to a dry pass
    /// through, and drop a runtime the audio thread has already retired.
    pub fn clear(&self) {
        drop(self.unload());
        self.collect_garbage();
    }
}

/// The audio-thread-resident IR engine.
pub(super) struct IrConvolver {
    channel: Arc<IrChannel>,
    active: Option<Box<PreparedIrRuntime>>,
    /// Taken from `pending` and held until the duck reaches silence.
    staged: Option<Box<PreparedIrRuntime>>,
    /// Set with an unload, cleared when a replacement is staged. Honored once
    /// the duck reaches silence and nothing is waiting to take the slot.
    clear_when_silent: bool,

    sample_rate: f32,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    fill: usize,

    /// Output gain, ducked to 0 across a runtime swap so the discontinuity
    /// between two different cabinets is a dip, not a click.
    gain: f32,
    gain_step: f32,
    ducking: bool,
}

impl IrConvolver {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        Self {
            channel: Arc::new(IrChannel {
                pending: HandoffCell::new(),
                retired: RetireBin::new(),
                unload: AtomicBool::new(false),
            }),
            active: None,
            staged: None,
            clear_when_silent: false,
            sample_rate: sr,
            in_l: vec![0.0; PARTITION],
            in_r: vec![0.0; PARTITION],
            out_l: vec![0.0; PARTITION],
            out_r: vec![0.0; PARTITION],
            fill: 0,
            gain: 1.0,
            gain_step: Self::step_for(sr),
            ducking: false,
        }
    }

    fn step_for(sample_rate: f32) -> f32 {
        1.0 / (sample_rate * (SWAP_DUCK_MS / 1_000.0)).max(1.0)
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.gain_step = Self::step_for(self.sample_rate);
        // Spectra are rate-specific: an IR prepared for the old rate would run
        // at the wrong length. Retire it and let the control side reload.
        if let Some(old) = self.active.take() {
            self.retire(old);
        }
        self.reset();
    }

    pub(super) fn reset(&mut self) {
        self.in_l.fill(0.0);
        self.in_r.fill(0.0);
        self.out_l.fill(0.0);
        self.out_r.fill(0.0);
        self.fill = 0;
        self.gain = if self.ducking { 0.0 } else { 1.0 };
        if let Some(rt) = self.active.as_mut() {
            rt.clear();
        }
    }

    /// Clone out a control-side loader handle for this convolver's cells.
    pub(super) fn loader(&self) -> IrLoader {
        IrLoader {
            channel: Arc::clone(&self.channel),
        }
    }

    /// Control thread: push a freshly-built runtime. The retired runtimes, and
    /// any one already waiting, are returned for this thread to drop. Clears
    /// a pending unload.
    pub(super) fn submit(
        &self,
        runtime: Box<PreparedIrRuntime>,
    ) -> (Retired<PreparedIrRuntime>, Option<Box<PreparedIrRuntime>>) {
        submit_to(&self.channel, runtime)
    }

    /// Control thread: drop every runtime the audio thread has retired.
    pub(super) fn poll_garbage(&mut self) {
        drop(self.channel.retired.drain());
    }

    /// Info about the currently active IR, if one is loaded.
    pub(super) fn active_info(&self) -> Option<IrInfo> {
        self.active.as_ref().map(|rt| rt.info())
    }

    /// Latency the convolution adds, in samples: always [`PARTITION`], because
    /// the block ring runs whether or not a file is loaded.
    ///
    /// Reporting 0 for an empty slot — as this used to — left the host
    /// uncompensated for a delay the engine was applying anyway, so the IR
    /// cabinet sat [`PARTITION`] samples behind every parallel path in the
    /// project. Keeping it constant also means loading a file mid-playback
    /// does not move the plugin's latency under the graph's feet.
    pub(super) fn latency_samples(&self) -> usize {
        PARTITION
    }

    /// Hand `runtime` to the control thread. Never frees it here: the bin
    /// always has a free slot, and would leak rather than drop if it did not.
    fn retire(&mut self, runtime: Box<PreparedIrRuntime>) {
        self.channel.retired.retire(runtime);
    }

    /// Audio thread: adopt a pending runtime once the duck has reached
    /// silence, and hand a retired one back. Called once per audio block.
    pub(super) fn begin_block(&mut self) {
        if self.staged.is_none() {
            if let Some(new_rt) = self.channel.pending.take() {
                // Adopting does not clear the unload flag. An unload that
                // lands after this runtime is taken has to stay set.
                if self.channel.unload.load(Ordering::Acquire) {
                    self.retire(new_rt);
                    if self.active.is_some() {
                        self.clear_when_silent = true;
                        self.ducking = true;
                    }
                } else {
                    self.clear_when_silent = false;
                    if self.active.is_none() {
                        // Nothing playing through the slot yet — no need to duck.
                        self.active = Some(new_rt);
                        self.gain = 1.0;
                        self.ducking = false;
                    } else {
                        self.staged = Some(new_rt);
                        self.ducking = true;
                    }
                }
            } else if self.channel.unload.swap(false, Ordering::AcqRel) && self.active.is_some() {
                self.clear_when_silent = true;
                self.ducking = true;
            }
        }

        if self.ducking && self.gain <= 0.0 {
            if let Some(mut new_rt) = self.staged.take() {
                self.clear_when_silent = false;
                new_rt.clear();
                if let Some(old) = self.active.replace(new_rt) {
                    self.retire(old);
                }
            } else if self.clear_when_silent {
                if let Some(old) = self.active.take() {
                    self.retire(old);
                }
                self.clear_when_silent = false;
            }
            self.ducking = false;
        }
    }

    /// Audio thread hot path. One sample in, one sample out, delayed by
    /// [`PARTITION`]; the convolution itself runs once every [`PARTITION`]
    /// calls. With no IR loaded the slot is a dry pass through at the same
    /// delay, so the reported latency stays honest either way.
    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let out = (self.out_l[self.fill], self.out_r[self.fill]);
        self.in_l[self.fill] = left;
        self.in_r[self.fill] = right;
        self.fill += 1;
        if self.fill >= PARTITION {
            self.fill = 0;
            match self.active.as_mut() {
                Some(rt) => {
                    rt.process_block(&self.in_l, &self.in_r, &mut self.out_l, &mut self.out_r)
                }
                None => {
                    self.out_l.copy_from_slice(&self.in_l);
                    self.out_r.copy_from_slice(&self.in_r);
                }
            }
        }

        self.gain = if self.ducking {
            (self.gain - self.gain_step).max(0.0)
        } else {
            (self.gain + self.gain_step).min(1.0)
        };
        (out.0 * self.gain, out.1 * self.gain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48_000.0;

    /// Build a float-WAV file around raw mono or stereo samples.
    fn wav_file(samples: &[f32], channels: u16, rate: u32) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        let block_align = channels * 4;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&32u16.to_le_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// A short, arbitrary but deterministic IR.
    fn test_ir(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / len as f32;
                ((i as f32 * 0.7).sin() * (1.0 - t) * (1.0 - t)) * 0.5
            })
            .collect()
    }

    /// Push `input` through a convolver and collect the output.
    fn render(conv: &mut IrConvolver, input: &[f32]) -> Vec<f32> {
        input
            .iter()
            .enumerate()
            .map(|(i, &x)| {
                if i % PARTITION == 0 {
                    conv.begin_block();
                }
                conv.process(x, x).0
            })
            .collect()
    }

    /// The engine's whole reason to exist: its output must match a direct
    /// time-domain convolution with the (normalized) IR, delayed by exactly
    /// `PARTITION` samples.
    #[test]
    fn matches_a_direct_convolution_delayed_by_the_partition_size() {
        let ir = test_ir(300);
        let prepared =
            prepare_ir_runtime(&wav_file(&ir, 1, SR as u32), "t".into(), SR).expect("IR must load");

        // Reproduce the load-time normalization for the reference.
        let energy: f32 = ir.iter().map(|h| h * h).sum();
        let gain = 1.0 / energy.sqrt();
        let norm: Vec<f32> = ir.iter().map(|h| h * gain).collect();

        let input: Vec<f32> = (0..2_048)
            .map(|n| {
                if n == 5 {
                    1.0
                } else {
                    (n as f32 * 0.031).sin() * 0.3
                }
            })
            .collect();

        let mut expected = vec![0.0f32; input.len()];
        for (n, out) in expected.iter_mut().enumerate() {
            for (k, &h) in norm.iter().enumerate() {
                if k <= n {
                    *out += input[n - k] * h;
                }
            }
        }

        let mut conv = IrConvolver::new(SR as f32);
        conv.submit(Box::new(prepared));
        let actual = render(&mut conv, &input);

        // The duck ramps in over the first few ms; compare past it, and past
        // the one-partition latency.
        let skip = PARTITION + (SR as usize * 6 / 1_000);
        let mut worst = 0.0f32;
        for n in skip..input.len() {
            worst = worst.max((actual[n] - expected[n - PARTITION]).abs());
        }
        assert!(worst < 1.0e-4, "convolution mismatch, worst error {worst}");
    }

    #[test]
    fn reports_partition_latency_whether_or_not_an_ir_is_loaded() {
        let mut conv = IrConvolver::new(SR as f32);
        // The ring runs either way, so the reported latency must not move when
        // a file lands — the empty slot is a *delayed* pass through.
        assert_eq!(conv.latency_samples(), PARTITION);
        assert!(conv.active_info().is_none());

        let ir = test_ir(200);
        let prepared = prepare_ir_runtime(&wav_file(&ir, 1, SR as u32), "cab".into(), SR).unwrap();
        assert_eq!(prepared.info().latency_samples, PARTITION);
        conv.submit(Box::new(prepared));
        conv.begin_block();
        assert_eq!(conv.latency_samples(), PARTITION);
        assert_eq!(conv.active_info().map(|i| i.name), Some("cab".into()));
    }

    /// Unload ducks to silence and then leaves the dry pass-through. The
    /// runtime is retired, not dropped, on the audio side.
    #[test]
    fn unload_returns_the_slot_to_a_dry_pass_through() {
        let mut conv = IrConvolver::new(SR as f32);
        let prepared =
            prepare_ir_runtime(&wav_file(&test_ir(200), 1, SR as u32), "cab".into(), SR).unwrap();
        let loader = conv.loader();
        loader.submit(Box::new(prepared));
        conv.begin_block();
        assert!(conv.active_info().is_some());

        drop(loader.unload());
        let mut cleared = false;
        for n in 0..4_096 {
            if n % PARTITION == 0 {
                conv.begin_block();
            }
            let (l, r) = conv.process(0.2, -0.2);
            assert!(l.is_finite() && r.is_finite());
            if conv.active_info().is_none() {
                cleared = true;
            }
        }
        assert!(cleared, "the IR is unloaded after the duck");
        assert!(conv.active_info().is_none());
        loader.collect_garbage();
    }

    /// An unload that arrives after an IR is queued must win over that IR.
    #[test]
    fn an_unload_during_adoption_removes_the_ir() {
        let mut conv = IrConvolver::new(SR as f32);
        let loader = conv.loader();
        loader.submit(Box::new(
            prepare_ir_runtime(&wav_file(&test_ir(200), 1, SR as u32), "a".into(), SR).unwrap(),
        ));
        conv.begin_block();
        assert!(conv.active_info().is_some());

        drop(loader.submit(Box::new(
            prepare_ir_runtime(&wav_file(&test_ir(200), 1, SR as u32), "b".into(), SR).unwrap(),
        )));
        conv.channel.unload.store(true, Ordering::Release);
        let input: Vec<f32> = (0..4_096).map(|n| (n as f32 * 0.05).sin() * 0.2).collect();
        let out = render(&mut conv, &input);
        for (n, y) in out.iter().enumerate() {
            assert!(y.is_finite(), "non-finite at {n}");
        }
        assert!(
            conv.active_info().is_none(),
            "the queued IR must not stay active"
        );
        loader.collect_garbage();
    }

    /// Repeated submits must drain the retired slot, or the fourth swap frees
    /// a model on the audio thread.
    #[test]
    fn four_swaps_never_overflow_the_retired_slot() {
        let mut conv = IrConvolver::new(SR as f32);
        let loader = conv.loader();
        let input: Vec<f32> = (0..4_096).map(|n| (n as f32 * 0.05).sin() * 0.2).collect();
        for i in 0..4 {
            drop(
                loader.submit(Box::new(
                    prepare_ir_runtime(
                        &wav_file(&test_ir(128 + i * 16), 1, SR as u32),
                        format!("c{i}"),
                        SR,
                    )
                    .unwrap(),
                )),
            );
            let out = render(&mut conv, &input);
            assert!(out.iter().all(|y| y.is_finite()));
            assert_eq!(loader.leaked_models(), 0, "swap {i} found no retire slot");
        }
        loader.collect_garbage();
    }

    /// One step of a load/unload schedule. The control side never sweeps
    /// garbage on its own, as in the VST3 wrapper: only `submit` drains.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// Control: build and submit a new IR.
        Load,
        /// Control: unload, taking back an IR that was not adopted yet.
        Unload,
        /// Control: an unload that lands after the audio thread has already
        /// taken the queued IR (the #251 race). Only the flag is set.
        RacingUnload,
        /// Audio: one host callback.
        Block,
        /// Audio: enough callbacks for the ducks to resolve.
        Settle,
    }

    /// The control and audio sides of one convolver, driven step by step. The
    /// heap probe is armed around every audio-side call, so an IR freed in
    /// `begin_block` or `process` is counted.
    struct SwapRig {
        conv: IrConvolver,
        loader: IrLoader,
        loads: usize,
        audio_frees: u64,
        finite: bool,
        /// What the control side asked for last.
        wanted: Option<String>,
    }

    impl SwapRig {
        fn new() -> Self {
            let conv = IrConvolver::new(SR as f32);
            let loader = conv.loader();
            Self {
                conv,
                loader,
                loads: 0,
                audio_frees: 0,
                finite: true,
                wanted: None,
            }
        }

        fn step(&mut self, step: Step) {
            match step {
                Step::Load => {
                    let name = format!("ir{}", self.loads);
                    let file = wav_file(&test_ir(128 + 16 * (self.loads % 4)), 1, SR as u32);
                    self.loads += 1;
                    let runtime = prepare_ir_runtime(&file, name.clone(), SR).unwrap();
                    drop(self.loader.submit(Box::new(runtime)));
                    self.wanted = Some(name);
                }
                Step::Unload => {
                    drop(self.loader.unload());
                    self.wanted = None;
                }
                Step::RacingUnload => {
                    self.conv.channel.unload.store(true, Ordering::Release);
                    self.wanted = None;
                }
                Step::Block => self.blocks(1),
                Step::Settle => self.blocks(8),
            }
        }

        fn blocks(&mut self, count: usize) {
            let mut finite = true;
            crate::alloc_probe::reset();
            crate::alloc_probe::arm();
            for block in 0..count {
                self.conv.begin_block();
                for n in 0..PARTITION {
                    let x = ((block * PARTITION + n) as f32 * 0.05).sin() * 0.2;
                    let (l, r) = self.conv.process(x, -x);
                    finite &= l.is_finite() && r.is_finite();
                }
            }
            crate::alloc_probe::disarm();
            self.audio_frees += crate::alloc_probe::frees();
            self.finite &= finite;
        }

        /// Run `steps`, let everything resolve, and check that no IR was
        /// freed on the audio side and the last request is what plays.
        fn run(steps: &[Step], preloaded: bool) {
            let mut rig = Self::new();
            if preloaded {
                rig.step(Step::Load);
                rig.step(Step::Settle);
            }
            for &step in steps {
                rig.step(step);
            }
            rig.blocks(32);
            assert_eq!(
                rig.audio_frees, 0,
                "{steps:?} (preloaded: {preloaded}) freed on the audio thread"
            );
            assert_eq!(rig.loader.leaked_models(), 0, "{steps:?} leaked an IR");
            assert!(rig.finite, "{steps:?} went non-finite");
            assert!(
                rig.conv.staged.is_none() && !rig.conv.ducking,
                "{steps:?} never settled"
            );
            assert_eq!(
                rig.conv.active_info().map(|i| i.name),
                rig.wanted,
                "{steps:?} (preloaded: {preloaded}) plays the wrong IR"
            );
            rig.loader.collect_garbage();
        }
    }

    /// Issue #257: two loads within a couple of callbacks fill every retire
    /// slot, and the unload after them freed an IR inside the callback.
    #[test]
    fn an_unload_after_two_quick_loads_frees_nothing_on_the_audio_thread() {
        use Step::*;
        SwapRig::run(&[Load, Block, Load, Settle, Unload], true);
        SwapRig::run(&[Load, Block, Load, Block, Settle, Unload, Settle], true);
    }

    #[test]
    fn load_load_unload_frees_nothing_on_the_audio_thread() {
        use Step::*;
        for preloaded in [false, true] {
            SwapRig::run(&[Load, Load, Unload], preloaded);
            SwapRig::run(&[Load, Block, Load, Block, Unload], preloaded);
            SwapRig::run(&[Load, Settle, Load, Settle, Unload], preloaded);
        }
    }

    #[test]
    fn load_unload_load_frees_nothing_on_the_audio_thread() {
        use Step::*;
        for preloaded in [false, true] {
            SwapRig::run(&[Load, Unload, Load], preloaded);
            SwapRig::run(&[Load, Block, Unload, Block, Load], preloaded);
            SwapRig::run(&[Load, Settle, Unload, Settle, Load], preloaded);
        }
    }

    #[test]
    fn four_rapid_swaps_then_an_unload_free_nothing_on_the_audio_thread() {
        use Step::*;
        for preloaded in [false, true] {
            SwapRig::run(
                &[Load, Block, Load, Block, Load, Block, Load, Block, Unload],
                preloaded,
            );
            SwapRig::run(&[Load, Load, Load, Load, Settle, Unload], preloaded);
        }
    }

    #[test]
    fn an_unload_during_adoption_frees_nothing_on_the_audio_thread() {
        use Step::*;
        for preloaded in [false, true] {
            SwapRig::run(&[Load, RacingUnload], preloaded);
            SwapRig::run(&[Load, Block, Load, RacingUnload, Settle], preloaded);
            SwapRig::run(&[Load, Block, Load, Block, Load, RacingUnload], preloaded);
        }
    }

    /// Every schedule of five steps, from an empty slot and from a playing
    /// IR. Whatever the order, no IR is freed on the audio side, and the last
    /// request wins.
    #[test]
    fn no_schedule_frees_a_model_on_the_audio_thread() {
        use Step::*;
        const STEPS: [Step; 5] = [Load, Unload, RacingUnload, Block, Settle];
        const LEN: u32 = 5;
        for preloaded in [false, true] {
            for code in 0..STEPS.len().pow(LEN) {
                let mut rest = code;
                let steps: Vec<Step> = (0..LEN)
                    .map(|_| {
                        let step = STEPS[rest % STEPS.len()];
                        rest /= STEPS.len();
                        step
                    })
                    .collect();
                SwapRig::run(&steps, preloaded);
            }
        }
    }

    /// With no IR loaded the slot must pass the signal through unchanged, just
    /// delayed — never mute the cabinet.
    #[test]
    fn without_an_ir_the_slot_is_a_delayed_dry_pass_through() {
        let mut conv = IrConvolver::new(SR as f32);
        let input: Vec<f32> = (0..1_024).map(|n| (n as f32 * 0.05).sin() * 0.4).collect();
        let out = render(&mut conv, &input);
        for n in PARTITION..input.len() {
            assert!(
                (out[n] - input[n - PARTITION]).abs() < 1.0e-6,
                "dry path altered the signal at {n}"
            );
        }
    }

    /// A swap must duck through the discontinuity and retire the old runtime
    /// to the control thread rather than dropping it on the audio thread.
    #[test]
    fn swapping_ducks_and_retires_the_previous_runtime() {
        let mut conv = IrConvolver::new(SR as f32);
        conv.submit(Box::new(
            prepare_ir_runtime(&wav_file(&test_ir(200), 1, SR as u32), "a".into(), SR).unwrap(),
        ));
        let input: Vec<f32> = (0..4_096).map(|n| (n as f32 * 0.05).sin() * 0.4).collect();
        let _ = render(&mut conv, &input);
        assert_eq!(conv.active_info().map(|i| i.name), Some("a".into()));

        conv.submit(Box::new(
            prepare_ir_runtime(&wav_file(&test_ir(400), 1, SR as u32), "b".into(), SR).unwrap(),
        ));
        let out = render(&mut conv, &input);
        for (n, y) in out.iter().enumerate() {
            assert!(y.is_finite(), "non-finite through the swap at {n}");
        }
        assert_eq!(conv.active_info().map(|i| i.name), Some("b".into()));
        assert!(!conv.ducking, "the duck must resolve");
        assert!((conv.gain - 1.0).abs() < 1.0e-6, "gain must come back up");

        // The retired runtime is waiting for the control thread, not dropped.
        conv.poll_garbage();
    }

    #[test]
    fn the_loader_handle_reaches_the_same_cells() {
        let mut conv = IrConvolver::new(SR as f32);
        let loader = conv.loader();
        let info = loader
            .load_wav(&wav_file(&test_ir(256), 1, SR as u32), "via-loader", SR)
            .expect("load through loader");
        assert_eq!(info.name, "via-loader");
        assert!(!info.stereo);
        conv.begin_block();
        assert_eq!(
            conv.active_info().map(|i| i.name),
            Some("via-loader".into())
        );
        loader.collect_garbage();
    }

    #[test]
    fn a_stereo_file_keeps_its_channels_distinct() {
        // Left rings, right is a single tap — the two sides must not converge.
        let mut interleaved = Vec::new();
        for i in 0..256 {
            interleaved.push(((i as f32) * 0.4).sin() * 0.5);
            interleaved.push(if i == 0 { 1.0 } else { 0.0 });
        }
        let info = prepare_ir_runtime(&wav_file(&interleaved, 2, SR as u32), "st".into(), SR)
            .expect("stereo IR must load")
            .info();
        assert!(info.stereo);

        let mut conv = IrConvolver::new(SR as f32);
        conv.submit(Box::new(
            prepare_ir_runtime(&wav_file(&interleaved, 2, SR as u32), "st".into(), SR).unwrap(),
        ));
        let mut diff = 0.0f32;
        for i in 0..2_048 {
            if i % PARTITION == 0 {
                conv.begin_block();
            }
            let x = if i == 300 { 1.0 } else { 0.0 };
            let (l, r) = conv.process(x, x);
            diff = diff.max((l - r).abs());
        }
        assert!(diff > 1.0e-3, "stereo channels collapsed together");
    }

    #[test]
    fn a_file_at_another_rate_is_resampled_to_the_engine_rate() {
        let ir = test_ir(441); // 10 ms at 44.1 kHz
        let info = prepare_ir_runtime(&wav_file(&ir, 1, 44_100), "resampled".into(), SR)
            .expect("a 44.1 kHz IR must load into a 48 kHz engine")
            .info();
        assert_eq!(info.source_sample_rate, 44_100.0);
        // 10 ms at 48 kHz is 480 frames, give or take the interpolation edge.
        assert!(
            info.frames.abs_diff(480) < 4,
            "resampled to {} frames",
            info.frames
        );
    }

    #[test]
    fn an_over_long_file_is_truncated_with_a_fade() {
        let long = vec![0.5f32; (SR as usize) * 2]; // two seconds
        let info = prepare_ir_runtime(&wav_file(&long, 1, SR as u32), "long".into(), SR)
            .expect("a long file still loads")
            .info();
        assert!(info.truncated);
        assert_eq!(info.frames, (MAX_IR_SECONDS as f64 * SR) as usize);
    }

    #[test]
    fn a_long_low_rate_file_is_cut_before_it_is_resampled() {
        // 60 s at 4.8 kHz: resampled whole it would be 2.9M frames per side
        // before the cut. Only the half second that is kept gets resampled,
        // and the result matches a file that was short to begin with.
        let rate = 4_800u32;
        let tone = |n: usize| (n as f32 * 0.37).sin() * 0.5;
        let long: Vec<f32> = (0..rate as usize * 60).map(tone).collect();
        let short: Vec<f32> = (0..rate as usize).map(tone).collect();
        let a = prepare_ir_runtime(&wav_file(&long, 1, rate), "long".into(), SR)
            .expect("a long low-rate file still loads")
            .info();
        let b = prepare_ir_runtime(&wav_file(&short, 1, rate), "short".into(), SR)
            .expect("a one-second file loads")
            .info();
        assert!(a.truncated && b.truncated);
        assert_eq!(a.frames, (MAX_IR_SECONDS as f64 * SR) as usize);
        assert_eq!(a.frames, b.frames);
    }

    #[test]
    fn rejects_silent_short_and_undecodable_files() {
        assert!(matches!(
            prepare_ir_runtime(&wav_file(&[0.0; 512], 1, SR as u32), "s".into(), SR),
            Err(IrLoadError::Silent)
        ));
        assert!(matches!(
            prepare_ir_runtime(&wav_file(&[0.5; 4], 1, SR as u32), "s".into(), SR),
            Err(IrLoadError::TooShort)
        ));
        assert!(matches!(
            prepare_ir_runtime(b"definitely not a wav", "s".into(), SR),
            Err(IrLoadError::Wav(_))
        ));
        // 8 kHz into a 192 kHz engine is a 24x stretch — refused, not guessed.
        assert!(matches!(
            prepare_ir_runtime(&wav_file(&test_ir(256), 1, 8_000), "s".into(), 192_000.0),
            Err(IrLoadError::UnusableSampleRate { .. })
        ));
    }

    /// Different IRs must land at comparable loudness — that is what the
    /// unit-energy normalization is for.
    #[test]
    fn normalization_evens_out_wildly_different_source_levels() {
        let quiet: Vec<f32> = test_ir(256).iter().map(|h| h * 0.001).collect();
        let loud: Vec<f32> = test_ir(256).iter().map(|h| h * 20.0).collect();

        let rms = |ir: &[f32]| -> f32 {
            let mut conv = IrConvolver::new(SR as f32);
            conv.submit(Box::new(
                prepare_ir_runtime(&wav_file(ir, 1, SR as u32), "n".into(), SR).unwrap(),
            ));
            let input: Vec<f32> = (0..8_192).map(|n| (n as f32 * 0.07).sin() * 0.4).collect();
            let out = render(&mut conv, &input);
            let tail = &out[2_048..];
            (tail.iter().map(|y| y * y).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let a = rms(&quiet);
        let b = rms(&loud);
        assert!(a > 1.0e-4 && b > 1.0e-4, "both must be audible: {a} {b}");
        assert!(
            (a / b - 1.0).abs() < 0.01,
            "levels diverged: {a} vs {b} (ratio {})",
            a / b
        );
    }

    #[test]
    fn stays_finite_across_rates_and_block_alignments() {
        for &sr in &[44_100.0f64, 48_000.0, 96_000.0] {
            let mut conv = IrConvolver::new(sr as f32);
            conv.submit(Box::new(
                prepare_ir_runtime(&wav_file(&test_ir(512), 2, sr as u32), "x".into(), sr).unwrap(),
            ));
            for block in 0..64 {
                conv.begin_block();
                // Deliberately uneven blocks: the convolver's own partition
                // boundary must not depend on the host's.
                for n in 0..(37 + block % 11) {
                    let x = ((block * 100 + n) as f32 * 0.03).sin();
                    let (l, r) = conv.process(x, -x);
                    assert!(l.is_finite() && r.is_finite(), "{sr} Hz went non-finite");
                }
            }
        }
    }
}
