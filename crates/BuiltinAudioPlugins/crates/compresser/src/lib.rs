//! Compressor — single-band and four-band (multiband) compression.
//!
//! **Single** runs one stereo-linked stage over the whole signal, with an
//! optional high-pass on the detector so the low end does not pump the mix.
//! **Multi** splits left and right through the shared four-way
//! Linkwitz–Riley crossover ([`FourBandSplitter`]) and gives every band its
//! own stage. The bands sum back to the input through one all-pass, so a
//! multiband setting with nothing over threshold is flat — no comb, no level
//! change — and the dry side of the mix is that same band sum, so a partial
//! mix never combs against a phase-shifted wet signal.
//!
//! Each stage computes its static curve on the instantaneous, stereo-linked
//! detector level and smooths the *reduction* in the dB domain with separate
//! attack and release (the branching peak detector of Giannoulis, Massberg &
//! Reiss). Smoothing the reduction rather than the level means a moved
//! threshold or ratio glides in at the stage's own time constants instead of
//! stepping the gain.
//!
//! Realtime contract: filters, smoothers and meters are all allocated in
//! [`Dsp::new`]; `process_stereo` and `apply_wire_param` only do arithmetic on
//! them.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};
use builtin_dsp_core::crossover::{FourBandSplitter, SPLIT_BANDS};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    flush_denormal, linear_to_db, max_filter_frequency, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};

pub const PLUGIN_ID: &str = "futureboard.compresser";

pub const BAND_COUNT: usize = 4;
pub const CROSSOVER_COUNT: usize = BAND_COUNT - 1;
const _: () = assert!(BAND_COUNT == SPLIT_BANDS);

pub const MIN_CROSSOVER_HZ: f32 = 20.0;
pub const MAX_CROSSOVER_HZ: f32 = 20_000.0;
pub const DEFAULT_CROSSOVERS_HZ: [f32; CROSSOVER_COUNT] = [120.0, 1_000.0, 5_000.0];

pub const MIN_THRESHOLD_DB: f32 = -60.0;
pub const MAX_THRESHOLD_DB: f32 = 0.0;
pub const MIN_RATIO: f32 = 1.0;
pub const MAX_RATIO: f32 = 20.0;
pub const MAX_KNEE_DB: f32 = 24.0;
pub const MIN_ATTACK_MS: f32 = 0.1;
pub const MAX_ATTACK_MS: f32 = 200.0;
pub const MIN_RELEASE_MS: f32 = 5.0;
pub const MAX_RELEASE_MS: f32 = 2_000.0;
pub const MIN_MAKEUP_DB: f32 = -12.0;
pub const MAX_MAKEUP_DB: f32 = 24.0;
pub const MIN_OUTPUT_DB: f32 = -24.0;
pub const MAX_OUTPUT_DB: f32 = 12.0;

/// The detector high-pass (Single mode) is off at or below this frequency.
pub const SIDECHAIN_OFF_HZ: f32 = 20.0;
pub const MAX_SIDECHAIN_HZ: f32 = 500.0;

/// `solo_band` value meaning every band is heard.
pub const SOLO_NONE: i32 = -1;

/// Makeup, mix and output changes glide over this long, so a dragged control
/// does not step the level sample to sample.
const SMOOTHING_SEC: f32 = 0.02;
/// Integration time of the RMS meters.
const RMS_SEC: f32 = 0.3;
/// Peak meters fall back over this long.
const PEAK_RELEASE_SEC: f32 = 0.35;
/// A reduction smaller than this is unity gain. 1e-4 dB is a gain error of
/// about −99 dB — far below audibility — and lets a stage that has let go skip
/// the exponential on every sample while its release tail decays.
const REDUCTION_FLOOR_DB: f32 = 1.0e-4;
/// Detector high-pass Q: a plain 2nd-order Butterworth.
const SIDECHAIN_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Which compressor is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// One stage over the whole signal.
    #[default]
    Single,
    /// Four crossover bands, each with its own stage.
    Multi,
}

impl Mode {
    /// Wire form: `0` Single, `1` Multi. Anything from one half up is Multi,
    /// so a host stepping it in normalised units cannot land between the two.
    pub fn from_wire(value: f32) -> Self {
        if value >= 0.5 {
            Self::Multi
        } else {
            Self::Single
        }
    }

    pub fn to_wire(self) -> f32 {
        match self {
            Self::Single => 0.0,
            Self::Multi => 1.0,
        }
    }
}

/// One multiband band's own stage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Band {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    /// A bypassed band passes through the crossover untouched.
    pub bypass: bool,
}

/// Defaults per band, lowest first. The low end gets slower time constants —
/// a fast release on a bass fundamental follows its waveform and distorts —
/// and the top end faster ones to catch transients.
pub const DEFAULT_BANDS: [Band; BAND_COUNT] = [
    Band {
        threshold_db: -20.0,
        ratio: 3.0,
        attack_ms: 20.0,
        release_ms: 200.0,
        makeup_db: 0.0,
        bypass: false,
    },
    Band {
        threshold_db: -20.0,
        ratio: 3.0,
        attack_ms: 10.0,
        release_ms: 150.0,
        makeup_db: 0.0,
        bypass: false,
    },
    Band {
        threshold_db: -20.0,
        ratio: 3.0,
        attack_ms: 5.0,
        release_ms: 100.0,
        makeup_db: 0.0,
        bypass: false,
    },
    Band {
        threshold_db: -20.0,
        ratio: 3.0,
        attack_ms: 2.0,
        release_ms: 80.0,
        makeup_db: 0.0,
        bypass: false,
    },
];

/// A missing field in a saved blob takes its default, so a state written
/// before a parameter existed still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    pub mode: Mode,

    // Single band.
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    /// Detector high-pass in Hz; at or below [`SIDECHAIN_OFF_HZ`] it is off.
    pub sidechain_hpf_hz: f32,

    // Both modes.
    pub knee_db: f32,
    /// Wet share in percent.
    pub mix: f32,
    pub output_db: f32,

    // Multi band.
    /// Crossover frequencies, low to high. The DSP sorts them before use, so
    /// a hand-edited blob or a host automating one past its neighbour gives
    /// four valid bands rather than a folded response.
    pub crossover_hz: [f32; CROSSOVER_COUNT],
    pub bands: [Band; BAND_COUNT],
    /// Band heard alone, or [`SOLO_NONE`].
    pub solo_band: i32,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: Mode::Single,
        threshold_db: -18.0,
        ratio: 4.0,
        attack_ms: 10.0,
        release_ms: 100.0,
        makeup_db: 0.0,
        sidechain_hpf_hz: 0.0,
        knee_db: 6.0,
        mix: 100.0,
        output_db: 0.0,
        crossover_hz: DEFAULT_CROSSOVERS_HZ,
        bands: DEFAULT_BANDS,
        solo_band: SOLO_NONE,
    }
}

impl Default for Params {
    fn default() -> Self {
        default_params()
    }
}

pub fn descriptor() -> PluginDescriptor {
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
    const fn crossover(id: &'static str, name: &'static str, index: usize) -> ParamDescriptor {
        param(
            id,
            name,
            DEFAULT_CROSSOVERS_HZ[index],
            MIN_CROSSOVER_HZ,
            MAX_CROSSOVER_HZ,
            "Hz",
        )
    }
    /// The six parameters of one band, in wire order.
    macro_rules! band {
        ($n:literal, $i:expr) => {
            [
                param(
                    concat!("band", $n, "ThresholdDb"),
                    concat!("Band ", $n, " Threshold"),
                    DEFAULT_BANDS[$i].threshold_db,
                    MIN_THRESHOLD_DB,
                    MAX_THRESHOLD_DB,
                    "dB",
                ),
                param(
                    concat!("band", $n, "Ratio"),
                    concat!("Band ", $n, " Ratio"),
                    DEFAULT_BANDS[$i].ratio,
                    MIN_RATIO,
                    MAX_RATIO,
                    ":1",
                ),
                param(
                    concat!("band", $n, "AttackMs"),
                    concat!("Band ", $n, " Attack"),
                    DEFAULT_BANDS[$i].attack_ms,
                    MIN_ATTACK_MS,
                    MAX_ATTACK_MS,
                    "ms",
                ),
                param(
                    concat!("band", $n, "ReleaseMs"),
                    concat!("Band ", $n, " Release"),
                    DEFAULT_BANDS[$i].release_ms,
                    MIN_RELEASE_MS,
                    MAX_RELEASE_MS,
                    "ms",
                ),
                param(
                    concat!("band", $n, "MakeupDb"),
                    concat!("Band ", $n, " Makeup"),
                    DEFAULT_BANDS[$i].makeup_db,
                    MIN_MAKEUP_DB,
                    MAX_MAKEUP_DB,
                    "dB",
                ),
                param(
                    concat!("band", $n, "Bypass"),
                    concat!("Band ", $n, " Bypass"),
                    0.0,
                    0.0,
                    1.0,
                    "bool",
                ),
            ]
        };
    }
    const BAND_1: [ParamDescriptor; 6] = band!("1", 0);
    const BAND_2: [ParamDescriptor; 6] = band!("2", 1);
    const BAND_3: [ParamDescriptor; 6] = band!("3", 2);
    const BAND_4: [ParamDescriptor; 6] = band!("4", 3);
    // A const so the table is a 'static promotion — the helper calls above
    // would otherwise make it a temporary.
    const PARAMS: &[ParamDescriptor] = &[
        param("power", "Power", 1.0, 0.0, 1.0, "bool"),
        param("mode", "Mode", 0.0, 0.0, 1.0, "enum"),
        param(
            "thresholdDb",
            "Threshold",
            -18.0,
            MIN_THRESHOLD_DB,
            MAX_THRESHOLD_DB,
            "dB",
        ),
        param("ratio", "Ratio", 4.0, MIN_RATIO, MAX_RATIO, ":1"),
        param(
            "attackMs",
            "Attack",
            10.0,
            MIN_ATTACK_MS,
            MAX_ATTACK_MS,
            "ms",
        ),
        param(
            "releaseMs",
            "Release",
            100.0,
            MIN_RELEASE_MS,
            MAX_RELEASE_MS,
            "ms",
        ),
        param(
            "makeupDb",
            "Makeup",
            0.0,
            MIN_MAKEUP_DB,
            MAX_MAKEUP_DB,
            "dB",
        ),
        param(
            "sidechainHpfHz",
            "Sidechain HPF",
            0.0,
            0.0,
            MAX_SIDECHAIN_HZ,
            "Hz",
        ),
        param("kneeDb", "Knee", 6.0, 0.0, MAX_KNEE_DB, "dB"),
        param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
        param(
            "outputDb",
            "Output",
            0.0,
            MIN_OUTPUT_DB,
            MAX_OUTPUT_DB,
            "dB",
        ),
        crossover("crossover1Hz", "Crossover 1", 0),
        crossover("crossover2Hz", "Crossover 2", 1),
        crossover("crossover3Hz", "Crossover 3", 2),
        BAND_1[0],
        BAND_1[1],
        BAND_1[2],
        BAND_1[3],
        BAND_1[4],
        BAND_1[5],
        BAND_2[0],
        BAND_2[1],
        BAND_2[2],
        BAND_2[3],
        BAND_2[4],
        BAND_2[5],
        BAND_3[0],
        BAND_3[1],
        BAND_3[2],
        BAND_3[3],
        BAND_3[4],
        BAND_3[5],
        BAND_4[0],
        BAND_4[1],
        BAND_4[2],
        BAND_4[3],
        BAND_4[4],
        BAND_4[5],
        param(
            "soloBand",
            "Solo Band",
            SOLO_NONE as f32,
            SOLO_NONE as f32,
            (BAND_COUNT - 1) as f32,
            "enum",
        ),
    ];
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Compressor",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: PARAMS,
    }
}

/// The crossover frequencies the filters actually run at: sorted, and held
/// inside what a biquad can be tuned to at `sample_rate`.
pub fn effective_crossovers(params: &Params, sample_rate: f32) -> [f32; CROSSOVER_COUNT] {
    let ceiling = max_filter_frequency(sample_rate).min(MAX_CROSSOVER_HZ);
    let mut hz = params
        .crossover_hz
        .map(|f| clamp(f, MIN_CROSSOVER_HZ, ceiling));
    hz.sort_by(f32::total_cmp);
    hz
}

/// Reduction in dB (positive) the static curve asks for at `level_db`: none
/// below the knee, `1 − 1/ratio` of the overshoot above it, and a quadratic
/// blend across it.
pub fn curve_reduction_db(level_db: f32, threshold_db: f32, ratio: f32, knee_db: f32) -> f32 {
    let slope = 1.0 - 1.0 / ratio.max(1.0);
    let over = level_db - threshold_db;
    let half = 0.5 * knee_db.max(0.0);
    if over <= -half {
        0.0
    } else if over >= half {
        over * slope
    } else {
        // Only reachable with a knee wider than zero, so the division is safe.
        let t = over + half;
        slope * t * t / (4.0 * half)
    }
}

/// One compressor stage: gain computer, reduction smoother and makeup.
#[derive(Debug, Clone, Copy)]
struct Stage {
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    /// Detector level (linear) at the bottom of the knee. Anything at or under
    /// it asks for no reduction, so a quiet signal skips the logarithm.
    floor: f32,
    attack: f32,
    release: f32,
    /// Smoothed reduction in dB, positive.
    reduction_db: f32,
    makeup: f32,
    makeup_target: f32,
}

impl Stage {
    fn new() -> Self {
        Self {
            threshold_db: 0.0,
            ratio: 1.0,
            knee_db: 0.0,
            floor: 1.0,
            attack: 0.0,
            release: 0.0,
            reduction_db: 0.0,
            makeup: 1.0,
            makeup_target: 1.0,
        }
    }

    fn configure(
        &mut self,
        sample_rate: f32,
        threshold_db: f32,
        ratio: f32,
        knee_db: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
    ) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.knee_db = knee_db.max(0.0);
        self.floor = db_to_linear(threshold_db - 0.5 * self.knee_db);
        self.attack = time_constant(sample_rate, attack_ms * 0.001);
        self.release = time_constant(sample_rate, release_ms * 0.001);
        self.makeup_target = db_to_linear(makeup_db);
    }

    /// Linear gain for one detector sample, makeup included.
    #[inline]
    fn gain(&mut self, level: f32, glide: f32) -> f32 {
        let target = if level <= self.floor {
            0.0
        } else {
            curve_reduction_db(
                linear_to_db(level),
                self.threshold_db,
                self.ratio,
                self.knee_db,
            )
        };
        let coeff = if target > self.reduction_db {
            self.attack
        } else {
            self.release
        };
        // Flushed: the reduction decays toward zero on every release.
        self.reduction_db = flush_denormal(target + coeff * (self.reduction_db - target));
        self.makeup += (self.makeup_target - self.makeup) * glide;
        let reduction = if self.reduction_db > REDUCTION_FLOOR_DB {
            db_to_linear(-self.reduction_db)
        } else {
            1.0
        };
        reduction * self.makeup
    }

    /// A bypassed stage lets go at once, so it neither reports a reduction
    /// nor snaps one back in when it is switched on again.
    #[inline]
    fn release_all(&mut self) {
        self.reduction_db = 0.0;
    }

    fn reset(&mut self) {
        self.reduction_db = 0.0;
        self.makeup = self.makeup_target;
    }
}

/// Level meter for one end of the plugin: held peak and running RMS.
#[derive(Debug, Clone, Copy, Default)]
struct Level {
    peak: f32,
    mean_square: f32,
    clip: bool,
}

impl Level {
    #[inline]
    fn push(&mut self, left: f32, right: f32, peak_release: f32, rms_coeff: f32) {
        let peak = left.abs().max(right.abs());
        self.peak = if peak > self.peak {
            peak
        } else {
            flush_denormal(self.peak * peak_release)
        };
        let square = 0.5 * (left * left + right * right);
        self.mean_square =
            flush_denormal(rms_coeff * self.mean_square + (1.0 - rms_coeff) * square);
        self.clip |= peak >= 1.0;
    }
}

/// Levels at the plugin's input and output plus the reduction, in the shape
/// every metering built-in hands the host.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// Decibels being taken off, positive: the single stage's, or the largest
    /// band's in Multi mode.
    pub gain_reduction_db: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

fn highpass(hz: f32, sample_rate: f32) -> Coefficients<f32> {
    let hz = clamp(hz, SIDECHAIN_OFF_HZ, max_filter_frequency(sample_rate));
    Coefficients::<f32>::from_params(Type::HighPass, sample_rate.hz(), hz.hz(), SIDECHAIN_Q)
        .unwrap_or(Coefficients {
            a1: 0.0,
            a2: 0.0,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
        })
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,

    single: Stage,
    sidechain_on: bool,
    sidechain_l: DirectForm1<f32>,
    sidechain_r: DirectForm1<f32>,

    split_l: FourBandSplitter,
    split_r: FourBandSplitter,
    bands: [Stage; BAND_COUNT],

    mix: f32,
    mix_target: f32,
    output_gain: f32,
    output_target: f32,
    smooth_coeff: f32,

    rms_coeff: f32,
    peak_release: f32,
    input_level: Level,
    output_level: Level,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let hz = effective_crossovers(&params, sr);
        let mut dsp = Self {
            sample_rate: sr,
            single: Stage::new(),
            sidechain_on: false,
            sidechain_l: DirectForm1::<f32>::new(highpass(SIDECHAIN_OFF_HZ, sr)),
            sidechain_r: DirectForm1::<f32>::new(highpass(SIDECHAIN_OFF_HZ, sr)),
            split_l: FourBandSplitter::new(hz, sr),
            split_r: FourBandSplitter::new(hz, sr),
            bands: [Stage::new(); BAND_COUNT],
            mix: 1.0,
            mix_target: 1.0,
            output_gain: 1.0,
            output_target: 1.0,
            smooth_coeff: 0.0,
            rms_coeff: 0.0,
            peak_release: 0.0,
            input_level: Level::default(),
            output_level: Level::default(),
            params,
        };
        dsp.update_time_constants();
        dsp.configure_all();
        dsp.reset();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter (project restore). The smoothers jump straight
    /// to the new values: a restored state starts where it was saved rather
    /// than gliding in from the defaults.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.configure_all();
        self.reset();
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    /// Allocation-free: at most a coefficient recompute.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        let previous_mode = self.params.mode;
        let previous_sidechain = self.sidechain_on;
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        match wire_index {
            ipc::MODE_INDEX => {
                if self.params.mode != previous_mode {
                    self.enter_mode();
                }
            }
            ipc::THRESHOLD_INDEX
            | ipc::RATIO_INDEX
            | ipc::ATTACK_INDEX
            | ipc::RELEASE_INDEX
            | ipc::MAKEUP_INDEX => self.configure_single(),
            ipc::SIDECHAIN_INDEX => {
                self.configure_sidechain();
                if self.sidechain_on && !previous_sidechain {
                    // Coming on: start the filter from silence rather than
                    // from whatever it held when it was last in use.
                    self.sidechain_l.reset_state();
                    self.sidechain_r.reset_state();
                }
            }
            // The knee is shared by every stage.
            ipc::KNEE_INDEX => {
                self.configure_single();
                self.configure_bands();
            }
            ipc::MIX_INDEX => self.mix_target = self.params.mix * 0.01,
            ipc::OUTPUT_INDEX => self.output_target = db_to_linear(self.params.output_db),
            ipc::CROSSOVER_1_INDEX..=ipc::CROSSOVER_3_INDEX => self.retune(),
            index => {
                if let Some((band, _)) = ipc::band_param(index) {
                    self.configure_band(band);
                }
                // Power, solo and band bypass are read straight off `params`
                // in `process_stereo`.
            }
        }
        true
    }

    /// Resolve a string id off the realtime path (project restore, tests).
    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// Minimum-phase IIR crossovers and no look-ahead: nothing for the graph
    /// to compensate.
    pub fn latency_samples(&self) -> usize {
        0
    }

    /// Decibels currently being taken off, positive. The single stage's, or in
    /// Multi mode the largest active band's.
    pub fn gain_reduction_db(&self) -> f32 {
        if !self.params.power {
            return 0.0;
        }
        match self.params.mode {
            Mode::Single => self.single.reduction_db,
            Mode::Multi => self.band_reduction_db().into_iter().fold(0.0, f32::max),
        }
    }

    /// Each band's reduction in dB, lowest band first. All zero in Single mode
    /// and while bypassed, where no band is running.
    pub fn band_reduction_db(&self) -> [f32; BAND_COUNT] {
        if !self.params.power || self.params.mode != Mode::Multi {
            return [0.0; BAND_COUNT];
        }
        self.bands.map(|band| band.reduction_db)
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.input_level.peak,
            in_rms: self.input_level.mean_square.max(0.0).sqrt(),
            out_peak: self.output_level.peak,
            out_rms: self.output_level.mean_square.max(0.0).sqrt(),
            gain_reduction_db: self.gain_reduction_db(),
            in_clip: self.input_level.clip,
            out_clip: self.output_level.clip,
        }
    }

    fn update_time_constants(&mut self) {
        self.smooth_coeff = time_constant(self.sample_rate, SMOOTHING_SEC);
        self.rms_coeff = time_constant(self.sample_rate, RMS_SEC);
        self.peak_release = time_constant(self.sample_rate, PEAK_RELEASE_SEC);
    }

    fn configure_all(&mut self) {
        self.configure_single();
        self.configure_sidechain();
        self.configure_bands();
        self.retune();
        self.mix_target = self.params.mix * 0.01;
        self.output_target = db_to_linear(self.params.output_db);
    }

    fn configure_single(&mut self) {
        let p = &self.params;
        self.single.configure(
            self.sample_rate,
            p.threshold_db,
            p.ratio,
            p.knee_db,
            p.attack_ms,
            p.release_ms,
            p.makeup_db,
        );
    }

    fn configure_sidechain(&mut self) {
        self.sidechain_on = self.params.sidechain_hpf_hz > SIDECHAIN_OFF_HZ;
        let coefficients = highpass(self.params.sidechain_hpf_hz, self.sample_rate);
        self.sidechain_l.update_coefficients(coefficients);
        self.sidechain_r.update_coefficients(coefficients);
    }

    fn configure_bands(&mut self) {
        for band in 0..BAND_COUNT {
            self.configure_band(band);
        }
    }

    fn configure_band(&mut self, band: usize) {
        let b = self.params.bands[band];
        self.bands[band].configure(
            self.sample_rate,
            b.threshold_db,
            b.ratio,
            self.params.knee_db,
            b.attack_ms,
            b.release_ms,
            b.makeup_db,
        );
    }

    fn retune(&mut self) {
        let hz = effective_crossovers(&self.params, self.sample_rate);
        self.split_l.retune(hz, self.sample_rate);
        self.split_r.retune(hz, self.sample_rate);
    }

    /// Start the newly selected mode from rest: the stages and filters of the
    /// mode that was idle hold whatever they had when it was last used.
    fn enter_mode(&mut self) {
        self.single.reset();
        self.sidechain_l.reset_state();
        self.sidechain_r.reset_state();
        self.split_l.reset();
        self.split_r.reset();
        for band in &mut self.bands {
            band.reset();
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.enter_mode();
        self.mix = self.mix_target;
        self.output_gain = self.output_target;
        self.input_level = Level::default();
        self.output_level = Level::default();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        if (sr - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        self.sample_rate = sr;
        self.update_time_constants();
        let hz = effective_crossovers(&self.params, sr);
        self.split_l = FourBandSplitter::new(hz, sr);
        self.split_r = FourBandSplitter::new(hz, sr);
        self.configure_all();
        self.enter_mode();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.input_level
            .push(left, right, self.peak_release, self.rms_coeff);
        if !self.params.power {
            // Bypass is a pure pass-through; the meters keep reading so the
            // editor still shows the signal it would be working on.
            self.output_level
                .push(left, right, self.peak_release, self.rms_coeff);
            return (left, right);
        }

        let glide = 1.0 - self.smooth_coeff;
        let (dry_l, dry_r, wet_l, wet_r) = match self.params.mode {
            Mode::Single => {
                let (detect_l, detect_r) = if self.sidechain_on {
                    (self.sidechain_l.run(left), self.sidechain_r.run(right))
                } else {
                    (left, right)
                };
                let gain = self.single.gain(detect_l.abs().max(detect_r.abs()), glide);
                (left, right, left * gain, right * gain)
            }
            Mode::Multi => {
                let lows = self.split_l.run(left);
                let highs = self.split_r.run(right);
                let solo = self.params.solo_band;
                let (mut dry_l, mut dry_r, mut wet_l, mut wet_r) = (0.0, 0.0, 0.0, 0.0);
                for band in 0..BAND_COUNT {
                    let (l, r) = (lows[band], highs[band]);
                    let stage = &mut self.bands[band];
                    let gain = if self.params.bands[band].bypass {
                        stage.release_all();
                        1.0
                    } else {
                        stage.gain(l.abs().max(r.abs()), glide)
                    };
                    if solo == SOLO_NONE || solo == band as i32 {
                        dry_l += l;
                        dry_r += r;
                        wet_l += l * gain;
                        wet_r += r * gain;
                    }
                }
                (dry_l, dry_r, wet_l, wet_r)
            }
        };

        self.mix += (self.mix_target - self.mix) * glide;
        self.output_gain += (self.output_target - self.output_gain) * glide;
        let out_l = (dry_l + (wet_l - dry_l) * self.mix) * self.output_gain;
        let out_r = (dry_r + (wet_r - dry_r) * self.mix) * self.output_gain;
        self.output_level
            .push(out_l, out_r, self.peak_release, self.rms_coeff);
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    /// Steady-state output amplitude of each side for a sine of `hz`, measured
    /// as RMS × √2 over whole periods after the stages have settled.
    fn sine_gain(dsp: &mut Dsp, hz: f32, left_amp: f32, right_amp: f32) -> (f32, f32) {
        dsp.reset();
        let period = (RATE / hz).round() as usize;
        let warm = (RATE * 0.5) as usize;
        let window = period * (4_800 / period).max(4);
        let mut energy = (0.0f64, 0.0f64);
        for i in 0..warm + window {
            let s = (std::f32::consts::TAU * hz * i as f32 / RATE).sin();
            let (l, r) = dsp.process_stereo(s * left_amp, s * right_amp);
            if i >= warm {
                energy.0 += f64::from(l * l);
                energy.1 += f64::from(r * r);
            }
        }
        let amplitude = |sum: f64| (2.0 * sum / window as f64).sqrt() as f32;
        (amplitude(energy.0), amplitude(energy.1))
    }

    fn feed_constant(dsp: &mut Dsp, value: f32, samples: usize) -> (f32, f32) {
        let mut out = (0.0, 0.0);
        for _ in 0..samples {
            out = dsp.process_stereo(value, value);
        }
        out
    }

    fn multi(mut params: Params) -> Params {
        params.mode = Mode::Multi;
        params
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.category, PluginCategory::Effect);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            UI_PARAM_IDS.to_vec(),
            "descriptor is not in wire order"
        );
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len(), "duplicate parameter id in descriptor");

        let defaults = ipc::ui_values(&default_params());
        for param in d.params {
            let (_, actual) = defaults
                .iter()
                .find(|(id, _)| *id == param.id)
                .copied()
                .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
            assert!(
                (param.default_value - actual).abs() < 1.0e-6,
                "`{}`: descriptor says {}, default_params() says {actual}",
                param.id,
                param.default_value,
            );
            assert!(param.default_value >= param.min && param.default_value <= param.max);
        }
    }

    #[test]
    fn bypass_when_power_off() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.power = false;
        params.threshold_db = MIN_THRESHOLD_DB;
        params.ratio = MAX_RATIO;
        dsp.set_params(params);
        assert_eq!(dsp.process_stereo(0.9, -0.5), (0.9, -0.5));
        assert_eq!(dsp.gain_reduction_db(), 0.0);
    }

    #[test]
    fn the_curve_is_flat_below_the_knee_and_ratioed_above_it() {
        assert_eq!(curve_reduction_db(-30.0, -20.0, 4.0, 6.0), 0.0);
        assert!((curve_reduction_db(-8.0, -20.0, 4.0, 6.0) - 9.0).abs() < 1.0e-5);
        // The knee is continuous at both edges and reaches a quarter of the
        // hard-knee reduction at the threshold for 4:1 with a 6 dB knee.
        let lo = curve_reduction_db(-23.0, -20.0, 4.0, 6.0);
        let hi = curve_reduction_db(-17.0, -20.0, 4.0, 6.0);
        assert!(lo.abs() < 1.0e-6 && (hi - 2.25).abs() < 1.0e-5, "{lo} {hi}");
        let mid = curve_reduction_db(-20.0, -20.0, 4.0, 6.0);
        assert!((mid - 0.5625).abs() < 1.0e-5, "{mid}");
        // A hard knee and a 1:1 ratio are well defined.
        assert_eq!(curve_reduction_db(-20.0, -20.0, 4.0, 0.0), 0.0);
        assert_eq!(curve_reduction_db(0.0, -20.0, 1.0, 6.0), 0.0);
    }

    /// A steady level 12 dB over a −20 dB threshold at 4:1 comes out 3 dB
    /// over it: 9 dB taken off.
    #[test]
    fn single_band_reaches_the_static_curve() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.threshold_db = -20.0;
        params.ratio = 4.0;
        params.knee_db = 0.0;
        params.attack_ms = 1.0;
        params.release_ms = 50.0;
        dsp.set_params(params);
        let input = db_to_linear(-8.0);
        let (out, _) = feed_constant(&mut dsp, input, 24_000);
        assert!(
            (linear_to_db(out) + 17.0).abs() < 0.05,
            "{}",
            linear_to_db(out)
        );
        assert!((dsp.gain_reduction_db() - 9.0).abs() < 0.05);
        assert_eq!(dsp.band_reduction_db(), [0.0; BAND_COUNT]);
    }

    #[test]
    fn a_quiet_signal_passes_untouched() {
        let mut dsp = Dsp::new(RATE);
        let (out, _) = feed_constant(&mut dsp, db_to_linear(-40.0), 4_800);
        assert!((out - db_to_linear(-40.0)).abs() < 1.0e-7);
        assert_eq!(dsp.gain_reduction_db(), 0.0);
    }

    /// Attack is the time to cover 1 − 1/e of a step in reduction; release
    /// lets go at its own, slower rate.
    #[test]
    fn attack_and_release_follow_their_time_constants() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.threshold_db = -20.0;
        params.ratio = MAX_RATIO;
        params.knee_db = 0.0;
        params.attack_ms = 10.0;
        params.release_ms = 100.0;
        dsp.set_params(params);
        let hot = db_to_linear(0.0);
        let full = curve_reduction_db(0.0, -20.0, MAX_RATIO, 0.0);
        feed_constant(&mut dsp, hot, (RATE * 0.010) as usize);
        let after_attack = dsp.gain_reduction_db() / full;
        assert!((after_attack - 0.632).abs() < 0.02, "{after_attack}");

        feed_constant(&mut dsp, hot, (RATE * 0.2) as usize);
        feed_constant(&mut dsp, 0.0, (RATE * 0.100) as usize);
        let after_release = dsp.gain_reduction_db() / full;
        assert!((after_release - 0.368).abs() < 0.02, "{after_release}");
    }

    #[test]
    fn makeup_mix_and_output_apply() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.threshold_db = MAX_THRESHOLD_DB;
        params.makeup_db = 6.0;
        params.output_db = -6.0;
        dsp.set_params(params.clone());
        let (out, _) = feed_constant(&mut dsp, 0.1, 100);
        assert!(
            (out - 0.1).abs() < 1.0e-4,
            "makeup and output cancel: {out}"
        );

        // A zero mix is the dry signal even while the stage compresses hard.
        params.threshold_db = MIN_THRESHOLD_DB;
        params.ratio = MAX_RATIO;
        params.makeup_db = 0.0;
        params.output_db = 0.0;
        params.mix = 0.0;
        dsp.set_params(params);
        let (out, _) = feed_constant(&mut dsp, 0.5, 4_800);
        assert!((out - 0.5).abs() < 1.0e-6, "{out}");
        assert!(dsp.gain_reduction_db() > 20.0);
    }

    #[test]
    fn the_detector_high_pass_lets_bass_through_uncompressed() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.threshold_db = -20.0;
        params.ratio = 8.0;
        dsp.set_params(params.clone());
        let (plain, _) = sine_gain(&mut dsp, 40.0, 0.5, 0.5);

        params.sidechain_hpf_hz = 400.0;
        dsp.set_params(params);
        let (filtered, _) = sine_gain(&mut dsp, 40.0, 0.5, 0.5);
        assert!(
            plain < 0.3,
            "without the filter 40 Hz is compressed: {plain}"
        );
        assert!(filtered > 0.45, "with it 40 Hz is left alone: {filtered}");
    }

    /// With nothing over threshold the four bands sum back flat.
    #[test]
    fn multi_band_at_rest_is_flat_across_the_crossovers() {
        let mut dsp = Dsp::new(RATE);
        let mut params = multi(default_params());
        for band in &mut params.bands {
            band.threshold_db = MAX_THRESHOLD_DB;
        }
        dsp.set_params(params);
        for hz in [40.0, 120.0, 400.0, 1_000.0, 3_000.0, 5_000.0, 12_000.0] {
            let (l, r) = sine_gain(&mut dsp, hz, 0.5, 0.25);
            assert!((l - 0.5).abs() < 0.5 * 0.02, "{hz} Hz left came out at {l}");
            assert!(
                (r - 0.25).abs() < 0.25 * 0.02,
                "{hz} Hz right came out at {r}"
            );
        }
        assert_eq!(dsp.band_reduction_db(), [0.0; BAND_COUNT]);
    }

    #[test]
    fn only_the_band_carrying_the_energy_compresses() {
        let mut dsp = Dsp::new(RATE);
        dsp.set_params(multi(default_params()));
        let (out, _) = sine_gain(&mut dsp, 60.0, 0.8, 0.8);
        let reduction = dsp.band_reduction_db();
        assert!(reduction[0] > 6.0, "{reduction:?}");
        assert!(reduction[2] < 0.1 && reduction[3] < 0.1, "{reduction:?}");
        assert!(out < 0.5, "{out}");
        assert!((dsp.gain_reduction_db() - reduction[0]).abs() < 1.0e-6);
    }

    #[test]
    fn a_bypassed_band_is_left_alone() {
        let mut dsp = Dsp::new(RATE);
        let mut params = multi(default_params());
        params.bands[0].bypass = true;
        dsp.set_params(params);
        let (out, _) = sine_gain(&mut dsp, 60.0, 0.8, 0.8);
        assert!((out - 0.8).abs() < 0.03, "{out}");
        assert_eq!(dsp.band_reduction_db()[0], 0.0);
    }

    #[test]
    fn solo_plays_one_band() {
        let mut dsp = Dsp::new(RATE);
        let mut params = multi(default_params());
        for band in &mut params.bands {
            band.threshold_db = MAX_THRESHOLD_DB;
        }
        params.solo_band = 3;
        dsp.set_params(params);
        let (inside, _) = sine_gain(&mut dsp, 12_000.0, 0.5, 0.5);
        let (outside, _) = sine_gain(&mut dsp, 200.0, 0.5, 0.5);
        assert!(
            (inside - 0.5).abs() < 0.03,
            "soloed band came out at {inside}"
        );
        assert!(outside < 0.01, "a band outside the solo leaked {outside}");
    }

    #[test]
    fn band_makeup_lifts_only_its_band() {
        let mut dsp = Dsp::new(RATE);
        let mut params = multi(default_params());
        for band in &mut params.bands {
            band.threshold_db = MAX_THRESHOLD_DB;
        }
        params.bands[2].makeup_db = 6.0;
        dsp.set_params(params);
        // Mid-band, the neighbours' Linkwitz–Riley skirts still carry a few
        // per cent of the sine, and that share is not lifted.
        let (lifted, _) = sine_gain(&mut dsp, 2_236.0, 0.25, 0.25);
        let (plain, _) = sine_gain(&mut dsp, 60.0, 0.25, 0.25);
        assert!(
            lifted > 0.25 * db_to_linear(5.0) && lifted < 0.25 * db_to_linear(6.0) + 0.005,
            "{lifted}"
        );
        assert!((plain - 0.25).abs() < 0.01, "{plain}");
    }

    #[test]
    fn wire_updates_reach_the_running_stages() {
        let mut dsp = Dsp::new(RATE);
        assert!(dsp.apply_ui_param("thresholdDb", -30.0));
        assert!(dsp.apply_ui_param("ratio", 10.0));
        let (out, _) = feed_constant(&mut dsp, 0.5, 24_000);
        assert!(out < 0.2, "{out}");

        assert!(dsp.apply_ui_param("mode", 1.0));
        assert_eq!(dsp.params().mode, Mode::Multi);
        assert_eq!(dsp.gain_reduction_db(), 0.0, "a new mode starts at rest");
        assert!(dsp.apply_ui_param("band2ThresholdDb", -40.0));
        assert_eq!(dsp.params().bands[1].threshold_db, -40.0);
        assert!(dsp.apply_ui_param("crossover2Hz", 2_000.0));
        assert!(!dsp.apply_ui_param("notAParam", 1.0));
    }

    #[test]
    fn silence_stays_silent_and_at_rest() {
        let mut dsp = Dsp::new(RATE);
        for mode in [Mode::Single, Mode::Multi] {
            let mut params = default_params();
            params.mode = mode;
            params.threshold_db = MIN_THRESHOLD_DB;
            dsp.set_params(params);
            let (l, r) = feed_constant(&mut dsp, 0.0, 4_800);
            assert_eq!((l, r), (0.0, 0.0));
            assert_eq!(dsp.gain_reduction_db(), 0.0);
        }
    }

    #[test]
    fn extreme_settings_and_mode_flips_stay_finite() {
        let mut dsp = Dsp::new(RATE);
        let mut params = multi(default_params());
        params.crossover_hz = [20.0, 20.0, 20_000.0];
        params.knee_db = MAX_KNEE_DB;
        params.output_db = MAX_OUTPUT_DB;
        params.sidechain_hpf_hz = MAX_SIDECHAIN_HZ;
        for band in &mut params.bands {
            band.threshold_db = MIN_THRESHOLD_DB;
            band.ratio = MAX_RATIO;
            band.attack_ms = MIN_ATTACK_MS;
            band.release_ms = MIN_RELEASE_MS;
            band.makeup_db = MAX_MAKEUP_DB;
        }
        dsp.set_params(params);
        for i in 0..20_000 {
            if i % 5_000 == 0 {
                let mode = if (i / 5_000) % 2 == 0 { 0.0 } else { 1.0 };
                dsp.apply_ui_param("mode", mode);
            }
            let x = if i % 2 == 0 { 1.0 } else { -1.0 };
            let (l, r) = dsp.process_stereo(x, -x);
            assert!(l.is_finite() && r.is_finite());
        }
    }

    #[test]
    fn meters_follow_input_output_and_reduction() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.threshold_db = -30.0;
        params.ratio = 8.0;
        dsp.set_params(params);
        // Long enough for the held output peak of the first, not yet reduced,
        // samples to have fallen away.
        feed_constant(&mut dsp, 0.8, 48_000);
        let frame = dsp.meter_frame();
        assert!(frame.in_peak > 0.79);
        assert!(frame.out_peak < frame.in_peak * 0.5);
        assert!(frame.gain_reduction_db > 10.0);
        assert!(!frame.in_clip && !frame.out_clip);
    }

    #[test]
    fn sample_rate_change_keeps_the_rest_state_flat() {
        let mut dsp = Dsp::new(RATE);
        let mut params = multi(default_params());
        for band in &mut params.bands {
            band.threshold_db = MAX_THRESHOLD_DB;
        }
        dsp.set_params(params);
        dsp.set_sample_rate(96_000.0);
        dsp.reset();
        let mut peak = 0.0f32;
        for i in 0..96_000 {
            let s = (std::f32::consts::TAU * 1_000.0 * i as f32 / 96_000.0).sin();
            let (l, _) = dsp.process_stereo(s * 0.5, 0.0);
            if i > 48_000 {
                peak = peak.max(l.abs());
            }
        }
        assert!((peak - 0.5).abs() < 0.01, "{peak}");
    }
}
