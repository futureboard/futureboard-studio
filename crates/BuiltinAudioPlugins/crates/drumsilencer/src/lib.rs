//! Drum Silencer — takes the drums out of a mix (or keeps only them), live.
//!
//! A small network trained on MUSDB18-HQ (see `training/` and [`model`]) —
//! a GRU for context, a causal time × frequency convolution for each bin's
//! recent shape — looks at each short-time spectrum and says, per bin, how
//! much of it is not drums.
//! That mask is applied to the spectrum and the audio rebuilt, with:
//!
//! * **Low latency.** An asymmetric analysis/synthesis window pair (Mauler &
//!   Martin): the analysis window is 1024 samples long for frequency
//!   resolution, the synthesis window only its last 2 × 128, so the delay is
//!   255 samples — 5.3 ms at 48 kHz — not a whole window. The network is
//!   causal; nothing looks ahead.
//! * **One transform for both channels.** Left and right ride one complex FFT
//!   as `L + iR`; the mask is real and the same for both, so it is applied to
//!   that spectrum directly and one inverse FFT rebuilds both.
//! * **Higher rates.** At 88.2 kHz and up every length scales by
//!   `round(rate / 48 kHz)`, so the network still sees ~47 Hz bins and
//!   2.7 ms hops; it reads every `scale`-th bin, and the bins between share
//!   their neighbour's gain. The latency stays 5.3 ms.
//!
//! Remove takes `amount` of the drum estimate out; Solo keeps only it. A
//! frequency range limits either to a band (the rest passes untouched in
//! Remove, and is silent in Solo). Bypass still runs the transform with unit
//! gain, so the output keeps its delay and switching never clicks.
//!
//! Allocation-free after construction: buffers and FFT plans for every scale
//! up to 192 kHz are made by [`Dsp::new`].

use std::sync::{Arc, OnceLock};

use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, clamp, db_to_linear, flush_denormal,
    time_constant,
};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod model;
pub mod presets;
pub mod ui;

pub use builtin_dsp_core::StereoEffect;
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use model::{Model, ModelError, ModelState};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.drumsilencer";

/// A low edge at (or under) this is off.
pub const LOW_OFF_HZ: f32 = 20.0;
/// A high edge at (or over) this is off.
pub const HIGH_OFF_HZ: f32 = 20_000.0;
/// How far past an edge (in octaves) the effect fades to nothing.
pub const EDGE_OCTAVES: f32 = 0.5;
/// The frequency bands the meters report, upper edges in Hz (the last is open).
pub const BAND_EDGES_HZ: [f32; BANDS - 1] = [100.0, 300.0, 1_000.0, 3_000.0, 8_000.0];
pub const BANDS: usize = 6;
/// The quietest a band meter reads, in dB.
pub const METER_FLOOR_DB: f32 = -90.0;

/// The weights shipped with the plug-in.
static EMBEDDED_WEIGHTS: &[u8] = include_bytes!("../model/drumsilencer.dsil");
/// The highest scale with prepared buffers: 4 × 48 kHz.
const MAX_SCALE: usize = 4;

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
/// The band meters' fall, per second, in dB.
const BAND_FALL_DB_PER_SEC: f32 = 30.0;

/// The shipped network, parsed once and shared by every instance.
pub fn embedded_model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| {
        Model::parse(EMBEDDED_WEIGHTS).expect("the embedded Drum Silencer weights are valid")
    })
}

/// What the plug-in keeps. Wire order: Remove, Solo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The mix without its drums.
    Remove,
    /// The drums alone.
    Solo,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remove => "remove",
            Self::Solo => "solo",
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Remove => 0.0,
            Self::Solo => 1.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        if value >= 0.5 { Self::Solo } else { Self::Remove }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub mode: Mode,
    /// How much of the drum estimate is taken out (Remove) or how much of the
    /// rest (Solo), 0–100 %.
    pub amount: f32,
    /// The effect fades out below this; [`LOW_OFF_HZ`] is off.
    pub low_hz: f32,
    /// The effect fades out above this; [`HIGH_OFF_HZ`] is off.
    pub high_hz: f32,
    pub output_db: f32,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: Mode::Remove,
        amount: 100.0,
        low_hz: LOW_OFF_HZ,
        high_hz: HIGH_OFF_HZ,
        output_db: 0.0,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Drum Silencer",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: &PARAMS,
    }
}

const PARAMS: [ParamDescriptor; ipc::PARAM_COUNT] = {
    const fn param(
        id: &'static str,
        name: &'static str,
        default_value: f32,
        min: f32,
        max: f32,
        unit: &'static str,
    ) -> ParamDescriptor {
        ParamDescriptor {
            id,
            name,
            default_value,
            min,
            max,
            unit,
        }
    }
    [
        param("power", "Power", 1.0, 0.0, 1.0, "bool"),
        param("mode", "Mode", 0.0, 0.0, 1.0, "enum"),
        param("amount", "Amount", 100.0, 0.0, 100.0, "%"),
        param("lowHz", "Low", LOW_OFF_HZ, LOW_OFF_HZ, 2_000.0, "Hz"),
        param("highHz", "High", HIGH_OFF_HZ, 1_000.0, HIGH_OFF_HZ, "Hz"),
        param("outputDb", "Output", 0.0, -24.0, 12.0, "dB"),
    ]
};

/// How much of the effect applies at `hz` for a range (`0` none … `1` all):
/// one inside it, fading linearly in log frequency to nothing
/// [`EDGE_OCTAVES`] past an edge that is on. What the editors draw the range
/// from.
pub fn range_weight(hz: f32, low_hz: f32, high_hz: f32) -> f32 {
    let mut w = 1.0f32;
    if low_hz > LOW_OFF_HZ + 0.5 {
        let octaves = (hz.max(1.0e-3) / low_hz).log2();
        w *= clamp(1.0 + octaves / EDGE_OCTAVES, 0.0, 1.0);
    }
    if high_hz < HIGH_OFF_HZ - 0.5 {
        let octaves = (hz.max(1.0e-3) / high_hz).log2();
        w *= clamp(1.0 - octaves / EDGE_OCTAVES, 0.0, 1.0);
    }
    w
}

/// What the editors draw.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// The drums the network hears, as a level (RMS, linear) — whatever the
    /// amount, so it reads even at 0 %.
    pub drums_rms: f32,
    /// Per [`BAND_EDGES_HZ`] band, the drums heard (RMS, linear, held).
    pub drum_bands: [f32; BANDS],
    /// Per band, the whole input (RMS, linear, held).
    pub input_bands: [f32; BANDS],
    pub in_clip: bool,
    pub out_clip: bool,
}

impl MeterFrame {
    /// The rack-position blocks of a host level frame `N` positions wide: the
    /// drum bands ride the input levels, the input bands the output levels.
    /// Studio's plug-in host and LiveStage both publish through this; the
    /// editors read the same positions back.
    pub fn rack_slots<const N: usize>(&self) -> ([f32; N], [f32; N]) {
        let mut slot_in = [0.0; N];
        let mut slot_out = [0.0; N];
        for band in 0..BANDS.min(N) {
            slot_in[band] = self.drum_bands[band];
            slot_out[band] = self.input_bands[band];
        }
        (slot_in, slot_out)
    }
}

#[derive(Debug, Clone)]
struct Meters {
    in_peak: f32,
    out_peak: f32,
    in_ms: f32,
    out_ms: f32,
    rms_coeff: f32,
    peak_coeff: f32,
    in_clip: bool,
    out_clip: bool,
}

impl Meters {
    fn new(sample_rate: f32) -> Self {
        Self {
            in_peak: 0.0,
            out_peak: 0.0,
            in_ms: 0.0,
            out_ms: 0.0,
            rms_coeff: time_constant(sample_rate, RMS_WINDOW_SECONDS),
            peak_coeff: time_constant(sample_rate, PEAK_FALL_SECONDS),
            in_clip: false,
            out_clip: false,
        }
    }

    fn reset(&mut self) {
        let (rms, peak) = (self.rms_coeff, self.peak_coeff);
        *self = Self {
            rms_coeff: rms,
            peak_coeff: peak,
            ..Self::new(1.0)
        };
    }

    #[inline]
    fn fall(held: f32, now: f32, coeff: f32) -> f32 {
        if now > held {
            now
        } else {
            flush_denormal(held * coeff)
        }
    }

    #[inline]
    fn push(&mut self, input: (f32, f32), output: (f32, f32)) {
        let in_abs = input.0.abs().max(input.1.abs());
        let out_abs = output.0.abs().max(output.1.abs());
        self.in_peak = Self::fall(self.in_peak, in_abs, self.peak_coeff);
        self.out_peak = Self::fall(self.out_peak, out_abs, self.peak_coeff);
        let in_sq = 0.5 * (input.0 * input.0 + input.1 * input.1);
        let out_sq = 0.5 * (output.0 * output.0 + output.1 * output.1);
        self.in_ms = flush_denormal(self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * in_sq);
        self.out_ms =
            flush_denormal(self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * out_sq);
        self.in_clip |= in_abs >= CLIP_THRESHOLD;
        self.out_clip |= out_abs >= CLIP_THRESHOLD;
    }
}

/// Everything one transform size needs.
struct Plan {
    window: usize,
    hop: usize,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    analysis: Box<[f32]>,
    /// Only the last `2 × hop` samples; the rest of the window is zero.
    synthesis_tail: Box<[f32]>,
    /// `2 / (window × Σ analysis²)`: one-sided bin power to mean square.
    power_to_ms: f32,
}

impl Plan {
    fn new(planner: &mut FftPlanner<f32>, scale: usize, base: &model::Framing) -> Self {
        let n = base.window * scale;
        let m = base.hop * scale;
        let (analysis, synthesis) = windows(n, m);
        let energy: f64 = analysis.iter().map(|w| w * w).sum();
        Self {
            window: n,
            hop: m,
            forward: planner.plan_fft_forward(n),
            inverse: planner.plan_fft_inverse(n),
            analysis: analysis.iter().map(|&w| w as f32).collect(),
            synthesis_tail: synthesis[n - 2 * m..]
                .iter()
                .map(|&w| (w / n as f64) as f32) // the inverse FFT is unnormalised
                .collect(),
            power_to_ms: (2.0 / (n as f64 * energy)) as f32,
        }
    }
}

/// The asymmetric window pair of `training/dsil.py`: a long rising
/// square-root Hann into the falling half of a short one for analysis; for
/// synthesis, a `2m` Hann over the last `2m` samples divided by it.
pub fn windows(n: usize, m: usize) -> (Vec<f64>, Vec<f64>) {
    let hann = |len: usize, i: f64| 0.5 - 0.5 * (std::f64::consts::TAU * i / len as f64).cos();
    let rise = n - m;
    let analysis: Vec<f64> = (0..n)
        .map(|i| {
            if i < rise {
                hann(2 * rise, i as f64).sqrt()
            } else {
                hann(2 * m, (i - (n - 2 * m)) as f64).sqrt()
            }
        })
        .collect();
    let synthesis = (0..n)
        .map(|i| {
            if i < n - 2 * m {
                0.0
            } else {
                hann(2 * m, (i - (n - 2 * m)) as f64) / analysis[i]
            }
        })
        .collect();
    (analysis, synthesis)
}

pub struct Dsp {
    params: Params,
    sample_rate: f32,
    model: &'static Model,
    state: ModelState,
    plans: Box<[Plan]>,
    /// Index into `plans`: `scale - 1`, but only scales 1, 2 and 4 exist.
    plan: usize,
    scale: usize,

    /// Input history, `window` long at the largest scale; a ring.
    hist_l: Box<[f32]>,
    hist_r: Box<[f32]>,
    write: usize,
    /// Samples into the current hop.
    hop_pos: usize,
    /// The overlap-add accumulator, `2 × hop` per channel.
    ola_l: Box<[f32]>,
    ola_r: Box<[f32]>,
    /// Where in `ola` the next output sample is, or `None` before the first
    /// frame.
    read: Option<usize>,

    spectrum: Box<[Complex<f32>]>,
    scratch: Box<[Complex<f32>]>,
    features: Box<[f32]>,
    mask: Box<[f32]>,
    /// The per-model-bin gain, rebuilt from the mask every frame.
    gain: Box<[f32]>,
    /// Per model bin, how much of the effect applies (the range).
    weight: Box<[f32]>,
    /// The running level the features are measured against.
    level: Option<f32>,
    level_alpha: f32,

    meters: Meters,
    drums_ms: f32,
    drum_bands: [f32; BANDS],
    input_bands: [f32; BANDS],
    band_fall: f32,
    out_gain: f32,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        Self::with_model(sample_rate, embedded_model())
    }

    /// A DSP on other weights (the tests' parity model).
    pub fn with_model(sample_rate: f32, model: &'static Model) -> Self {
        let mut planner = FftPlanner::new();
        let plans: Box<[Plan]> = [1, 2, 4]
            .into_iter()
            .map(|scale| Plan::new(&mut planner, scale, &model.framing))
            .collect();
        let max_n = model.framing.window * MAX_SCALE;
        let max_m = model.framing.hop * MAX_SCALE;
        let scratch_len = plans
            .iter()
            .map(|p| {
                p.forward
                    .get_inplace_scratch_len()
                    .max(p.inverse.get_inplace_scratch_len())
            })
            .max()
            .unwrap_or(0);
        let bins = model.framing.bins;
        let mut dsp = Self {
            params: default_params(),
            sample_rate: 48_000.0,
            model,
            state: model.new_state(),
            plans,
            plan: 0,
            scale: 1,
            hist_l: vec![0.0; max_n].into_boxed_slice(),
            hist_r: vec![0.0; max_n].into_boxed_slice(),
            write: 0,
            hop_pos: 0,
            ola_l: vec![0.0; 2 * max_m].into_boxed_slice(),
            ola_r: vec![0.0; 2 * max_m].into_boxed_slice(),
            read: None,
            spectrum: vec![Complex::default(); max_n].into_boxed_slice(),
            scratch: vec![Complex::default(); scratch_len].into_boxed_slice(),
            features: vec![0.0; bins].into_boxed_slice(),
            mask: vec![1.0; bins].into_boxed_slice(),
            gain: vec![1.0; bins].into_boxed_slice(),
            weight: vec![1.0; bins].into_boxed_slice(),
            level: None,
            level_alpha: 0.0,
            meters: Meters::new(48_000.0),
            drums_ms: 0.0,
            drum_bands: [0.0; BANDS],
            input_bands: [0.0; BANDS],
            band_fall: 1.0,
            out_gain: 1.0,
        };
        dsp.set_sample_rate_internal(sample_rate);
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
    }

    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        self.apply_params();
        true
    }

    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// `2 × hop − 1` samples: 255 at 44.1/48 kHz, 511 at 88.2/96 kHz, 1023 at
    /// 176.4/192 kHz. Constant, bypassed or not.
    pub fn latency_samples(&self) -> usize {
        2 * self.plans[self.plan].hop - 1
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            drums_rms: self.drums_ms.max(0.0).sqrt(),
            drum_bands: self.drum_bands,
            input_bands: self.input_bands,
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    /// The frequency of model bin `k` at the current rate.
    fn bin_hz(&self, k: usize) -> f32 {
        let plan = &self.plans[self.plan];
        (k * self.scale) as f32 * self.sample_rate / plan.window as f32
    }

    fn apply_params(&mut self) {
        self.out_gain = db_to_linear(self.params.output_db);
        for k in 0..self.weight.len() {
            self.weight[k] = range_weight(self.bin_hz(k), self.params.low_hz, self.params.high_hz);
        }
    }

    fn set_sample_rate_internal(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        let scale = ((self.sample_rate / 48_000.0).round() as usize).clamp(1, MAX_SCALE);
        // Only 1, 2 and 4 are planned; a 3× rate (144 kHz) uses 2×.
        let scale = if scale == 3 { 2 } else { scale };
        self.scale = scale;
        self.plan = match scale {
            1 => 0,
            2 => 1,
            _ => 2,
        };
        let framing = &self.model.framing;
        let hop_seconds = (framing.hop * scale) as f32 / self.sample_rate;
        self.level_alpha = (-hop_seconds / framing.tau_seconds).exp();
        self.band_fall = db_to_linear(-BAND_FALL_DB_PER_SEC * hop_seconds);
        self.meters = Meters::new(self.sample_rate);
        self.clear_stream();
    }

    fn clear_stream(&mut self) {
        self.hist_l.fill(0.0);
        self.hist_r.fill(0.0);
        self.ola_l.fill(0.0);
        self.ola_r.fill(0.0);
        self.write = 0;
        self.hop_pos = 0;
        self.read = None;
        self.level = None;
        self.state.reset();
        self.drums_ms = 0.0;
        self.drum_bands = [0.0; BANDS];
        self.input_bands = [0.0; BANDS];
    }

    /// One hop: transform the last window, mask it, add it back.
    fn process_frame(&mut self) {
        let plan = &self.plans[self.plan];
        let (n, m) = (plan.window, plan.hop);
        let hist_len = self.hist_l.len();
        // The window ends at the sample just written.
        let start = (self.write + hist_len - n) % hist_len;
        for i in 0..n {
            let j = (start + i) % hist_len;
            let w = plan.analysis[i];
            self.spectrum[i] = Complex::new(self.hist_l[j] * w, self.hist_r[j] * w);
        }
        let spectrum = &mut self.spectrum[..n];
        plan.forward
            .process_with_scratch(spectrum, &mut self.scratch);

        // Per model bin: the channels' mean power, read from the packed
        // spectrum (|L|² + |R|² = (|Z(k)|² + |Z(n−k)|²) / 2).
        let bins = self.features.len();
        let scale = self.scale;
        let framing = self.model.framing;
        let power = |z: &[Complex<f32>], j: usize| -> f32 {
            let a = z[j];
            let b = z[(n - j) % n];
            0.25 * (a.norm_sqr() + b.norm_sqr())
        };
        let mut level = 0.0f32;
        for k in 0..bins {
            let lp = (power(spectrum, k * scale) + 1.0e-10).log10();
            self.features[k] = lp;
            level += lp;
        }
        let level = (level / bins as f32).max(framing.level_floor);
        let mu = match self.level {
            None => level,
            Some(mu) => self.level_alpha * mu + (1.0 - self.level_alpha) * level,
        };
        self.level = Some(mu);
        for f in self.features.iter_mut() {
            *f = clamp(
                (*f - mu) / framing.feature_scale,
                -framing.feature_clamp,
                framing.feature_clamp,
            );
        }

        if self.params.power {
            self.model
                .step(&mut self.state, &self.features, &mut self.mask);
            let amount = self.params.amount * 0.01;
            let solo = self.params.mode == Mode::Solo;
            for k in 0..bins {
                let drums = (1.0 - self.mask[k]) * self.weight[k];
                let g = if solo {
                    1.0 - amount + amount * drums
                } else {
                    1.0 - amount * drums
                };
                self.gain[k] = g * self.out_gain;
            }
        } else {
            self.gain.fill(1.0);
        }

        // Meters: the drums heard and the input, per band, as mean squares.
        let mut drums_total = 0.0f32;
        let mut drum_band = [0.0f32; BANDS];
        let mut input_band = [0.0f32; BANDS];
        let mut band = 0;
        let bin_hz = self.sample_rate / n as f32;
        for j in 0..=n / 2 {
            let hz = j as f32 * bin_hz;
            while band < BANDS - 1 && hz >= BAND_EDGES_HZ[band] {
                band += 1;
            }
            let k = ((j + scale / 2) / scale).min(bins - 1);
            let p = power(spectrum, j);
            let d = if self.params.power {
                p * (1.0 - self.mask[k]).powi(2)
            } else {
                0.0
            };
            drums_total += d;
            drum_band[band] += d;
            input_band[band] += p;
        }
        let to_ms = plan.power_to_ms;
        self.drums_ms = drums_total * to_ms;
        for b in 0..BANDS {
            let d = (drum_band[b] * to_ms).sqrt();
            let x = (input_band[b] * to_ms).sqrt();
            self.drum_bands[b] = flush_denormal(d.max(self.drum_bands[b] * self.band_fall));
            self.input_bands[b] = flush_denormal(x.max(self.input_bands[b] * self.band_fall));
        }

        // Apply the gain to both halves of the packed spectrum: a real,
        // symmetric gain keeps L and R apart.
        for j in 0..=n / 2 {
            let g = self.gain[((j + scale / 2) / scale).min(bins - 1)];
            spectrum[j] *= g;
            if j != 0 && j != n / 2 {
                spectrum[n - j] *= g;
            }
        }
        plan.inverse
            .process_with_scratch(spectrum, &mut self.scratch);

        // Slide the accumulator by a hop and add this frame's tail.
        let ola_l = &mut self.ola_l[..2 * m];
        let ola_r = &mut self.ola_r[..2 * m];
        ola_l.copy_within(m.., 0);
        ola_r.copy_within(m.., 0);
        ola_l[m..].fill(0.0);
        ola_r[m..].fill(0.0);
        for i in 0..2 * m {
            let z = spectrum[n - 2 * m + i];
            let w = plan.synthesis_tail[i];
            ola_l[i] += z.re * w;
            ola_r[i] += z.im * w;
        }
        self.read = Some(0);
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.meters.reset();
        self.clear_stream();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.set_sample_rate_internal(sample_rate);
        self.apply_params();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        let hist_len = self.hist_l.len();
        self.hist_l[self.write] = left;
        self.hist_r[self.write] = right;
        self.write = (self.write + 1) % hist_len;
        self.hop_pos += 1;
        if self.hop_pos == self.plans[self.plan].hop {
            self.hop_pos = 0;
            self.process_frame();
        }
        let out = match self.read {
            Some(i) => {
                self.read = Some(i + 1);
                (self.ola_l[i], self.ola_r[i])
            }
            None => (0.0, 0.0),
        };
        self.meters.push((left, right), out);
        out
    }
}

#[cfg(test)]
mod tests;
