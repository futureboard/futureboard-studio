//! Imager — four-band stereo width.
//!
//! The input is taken apart into mid (`(L + R) / 2`) and side (`(L − R) / 2`),
//! and *both* run through the same four-way crossover
//! ([`builtin_dsp_core::crossover::FourBandSplitter`]): 4th-order
//! Linkwitz–Riley splits, with the all-pass each branch needs to line its
//! phase up with the other. Each band's side is scaled by that band's width,
//! the bands are summed, and mid/side is turned back into left/right.
//!
//! Splitting mid as well as side is what makes the plugin transparent at rest:
//! the two see the identical phase response, so with every width at 100 % the
//! output is the input through one shared all-pass — no comb, no level change,
//! no image shift. Splitting side alone would leave mid dry and side phased,
//! and the image would move with nothing touched. It also lets a band be soloed
//! whole, mid and side together.
//!
//! Realtime contract: filters, smoothers and the telemetry block are all
//! allocated in [`Dsp::new`]; `process_stereo` and `apply_wire_param` only do
//! arithmetic on them.

use builtin_dsp_core::crossover::FourBandSplitter as BandSplitter;
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    flush_denormal, max_filter_frequency, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};

pub const PLUGIN_ID: &str = "futureboard.imager";

pub const BAND_COUNT: usize = 4;
pub const CROSSOVER_COUNT: usize = BAND_COUNT - 1;

pub const MIN_CROSSOVER_HZ: f32 = 20.0;
pub const MAX_CROSSOVER_HZ: f32 = 20_000.0;
pub const DEFAULT_CROSSOVERS_HZ: [f32; CROSSOVER_COUNT] = [120.0, 1_000.0, 6_000.0];

/// Width is the side level in percent: 0 folds the band to mono, 100 leaves it
/// as it came in, 200 doubles its side.
pub const MAX_WIDTH: f32 = 200.0;
pub const DEFAULT_WIDTH: f32 = 100.0;

/// `solo_band` value meaning every band is heard.
pub const SOLO_NONE: i32 = -1;

pub const MIN_OUTPUT_DB: f32 = -24.0;
pub const MAX_OUTPUT_DB: f32 = 12.0;

/// Output samples the vectorscope keeps, as left/right pairs.
pub const SCOPE_POINTS: usize = 128;
/// Output samples between two scope points. 128 × 12 = 1536 samples, so a
/// fresh scope frame is ready about 31 times a second at 48 kHz — the rate the
/// editor's telemetry runs at.
const SCOPE_DECIMATION: usize = 12;

/// Width and output changes glide over this long, so a dragged control does
/// not step the side level sample to sample.
const SMOOTHING_SEC: f32 = 0.02;
/// Integration time of the correlation and band-level meters — slow enough to
/// read, fast enough to follow a chorus into a verse.
const CORRELATION_SEC: f32 = 0.3;
/// Peak meters fall back over this long.
const PEAK_RELEASE_SEC: f32 = 0.35;
/// Below this mean-square energy a correlation is not a measurement: silence
/// has no stereo image, and reporting its ratio of two near-zero numbers would
/// make the meter twitch.
const CORRELATION_FLOOR: f32 = 1.0e-9;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    /// Crossover frequencies, low to high. The DSP sorts them before use, so
    /// a hand-edited blob or a host automating one past its neighbour gives
    /// four valid bands rather than a folded response.
    pub crossover_hz: [f32; CROSSOVER_COUNT],
    /// Per-band width in percent, lowest band first.
    pub width: [f32; BAND_COUNT],
    /// Band heard alone, or [`SOLO_NONE`].
    pub solo_band: i32,
    pub output_db: f32,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        crossover_hz: DEFAULT_CROSSOVERS_HZ,
        width: [DEFAULT_WIDTH; BAND_COUNT],
        solo_band: SOLO_NONE,
        output_db: 0.0,
    }
}

pub fn descriptor() -> PluginDescriptor {
    const fn crossover(id: &'static str, name: &'static str, default: f32) -> ParamDescriptor {
        ParamDescriptor {
            id,
            name,
            default_value: default,
            min: MIN_CROSSOVER_HZ,
            max: MAX_CROSSOVER_HZ,
            unit: "Hz",
        }
    }
    const fn width(id: &'static str, name: &'static str) -> ParamDescriptor {
        ParamDescriptor {
            id,
            name,
            default_value: DEFAULT_WIDTH,
            min: 0.0,
            max: MAX_WIDTH,
            unit: "%",
        }
    }
    // A const so the table is a 'static promotion — the helper calls
    // above would otherwise make it a temporary.
    const PARAMS: &[ParamDescriptor] = &[
        ParamDescriptor {
            id: "power",
            name: "Power",
            default_value: 1.0,
            min: 0.0,
            max: 1.0,
            unit: "bool",
        },
        crossover("crossover1Hz", "Crossover 1", DEFAULT_CROSSOVERS_HZ[0]),
        crossover("crossover2Hz", "Crossover 2", DEFAULT_CROSSOVERS_HZ[1]),
        crossover("crossover3Hz", "Crossover 3", DEFAULT_CROSSOVERS_HZ[2]),
        width("width1", "Width 1"),
        width("width2", "Width 2"),
        width("width3", "Width 3"),
        width("width4", "Width 4"),
        ParamDescriptor {
            id: "soloBand",
            name: "Solo Band",
            default_value: SOLO_NONE as f32,
            min: SOLO_NONE as f32,
            max: (BAND_COUNT - 1) as f32,
            unit: "enum",
        },
        ParamDescriptor {
            id: "outputDb",
            name: "Output",
            default_value: 0.0,
            min: MIN_OUTPUT_DB,
            max: MAX_OUTPUT_DB,
            unit: "dB",
        },
    ];
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Imager",
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

/// Running left/right statistics for one correlation meter.
#[derive(Debug, Clone, Copy, Default)]
struct Correlation {
    lr: f32,
    ll: f32,
    rr: f32,
}

impl Correlation {
    #[inline]
    fn push(&mut self, left: f32, right: f32, coeff: f32) {
        let keep = coeff;
        let take = 1.0 - coeff;
        self.lr = flush_denormal(keep * self.lr + take * left * right);
        self.ll = flush_denormal(keep * self.ll + take * left * left);
        self.rr = flush_denormal(keep * self.rr + take * right * right);
    }

    /// Pearson correlation of left against right: +1 mono, 0 unrelated, −1
    /// one side inverted. `0.0` while there is too little signal to measure.
    fn value(&self) -> f32 {
        let energy = self.ll * self.rr;
        if energy <= CORRELATION_FLOOR * CORRELATION_FLOOR {
            return 0.0;
        }
        clamp(self.lr / energy.sqrt(), -1.0, 1.0)
    }

    /// RMS of the band, both sides together, linear.
    fn level(&self) -> f32 {
        (0.5 * (self.ll + self.rr)).max(0.0).sqrt()
    }

    fn reset(&mut self) {
        *self = Self::default();
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

/// Telemetry the editor draws, read by the host between blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageFrame {
    /// Output correlation, −1..+1.
    pub correlation: f32,
    /// Output correlation of each band on its own.
    pub band_correlation: [f32; BAND_COUNT],
    /// Output RMS of each band, linear — lets the editor grey out the
    /// correlation of a band with nothing in it.
    pub band_level: [f32; BAND_COUNT],
    /// The last [`SCOPE_POINTS`] decimated output samples as interleaved
    /// left/right pairs, oldest first.
    pub scope: [f32; SCOPE_POINTS * 2],
}

/// Levels at the plugin's input and output, in the shape every metering
/// built-in hands the host.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    mid: BandSplitter,
    side: BandSplitter,

    /// Side gain per band, gliding toward `params.width`.
    width_gain: [f32; BAND_COUNT],
    output_gain: f32,
    output_target: f32,
    smooth_coeff: f32,

    correlation_coeff: f32,
    peak_release: f32,
    correlation: Correlation,
    band_correlation: [Correlation; BAND_COUNT],
    input_level: Level,
    output_level: Level,

    /// Ring of decimated output pairs; `scope_write` is the next pair.
    scope: [f32; SCOPE_POINTS * 2],
    scope_write: usize,
    scope_countdown: usize,
    /// Set each time the ring has been written all the way round.
    scope_fresh: bool,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let hz = effective_crossovers(&params, sr);
        let mut dsp = Self {
            sample_rate: sr,
            mid: BandSplitter::new(hz, sr),
            side: BandSplitter::new(hz, sr),
            width_gain: params.width.map(|w| w / 100.0),
            output_gain: 1.0,
            output_target: 1.0,
            smooth_coeff: 0.0,
            correlation_coeff: 0.0,
            peak_release: 0.0,
            correlation: Correlation::default(),
            band_correlation: [Correlation::default(); BAND_COUNT],
            input_level: Level::default(),
            output_level: Level::default(),
            scope: [0.0; SCOPE_POINTS * 2],
            scope_write: 0,
            scope_countdown: SCOPE_DECIMATION,
            scope_fresh: false,
            params,
        };
        dsp.update_time_constants();
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
        self.retune();
        self.width_gain = self.params.width.map(|w| w / 100.0);
        self.output_target = db_to_linear(self.params.output_db);
        self.output_gain = self.output_target;
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    /// Allocation-free: at most a coefficient recompute.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        match wire_index {
            ipc::CROSSOVER_1_INDEX | ipc::CROSSOVER_2_INDEX | ipc::CROSSOVER_3_INDEX => {
                self.retune()
            }
            ipc::OUTPUT_INDEX => self.output_target = db_to_linear(self.params.output_db),
            // Widths are read by the smoother and solo/power straight off
            // `params` in `process_stereo`.
            _ => {}
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

    /// Minimum-phase IIR crossovers: nothing for the graph to compensate.
    pub fn latency_samples(&self) -> usize {
        0
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.input_level.peak,
            in_rms: self.input_level.mean_square.max(0.0).sqrt(),
            out_peak: self.output_level.peak,
            out_rms: self.output_level.mean_square.max(0.0).sqrt(),
            in_clip: self.input_level.clip,
            out_clip: self.output_level.clip,
        }
    }

    /// The image telemetry, once per completed trip round the scope ring;
    /// `None` in between. Called by the host after each block.
    pub fn take_image_frame(&mut self) -> Option<ImageFrame> {
        if !self.scope_fresh {
            return None;
        }
        self.scope_fresh = false;
        Some(self.image_frame())
    }

    /// The image telemetry as it stands, fresh or not.
    pub fn image_frame(&self) -> ImageFrame {
        // Unroll the ring so the frame reads oldest to newest.
        let mut scope = [0.0; SCOPE_POINTS * 2];
        let split = self.scope_write * 2;
        let tail = self.scope.len() - split;
        scope[..tail].copy_from_slice(&self.scope[split..]);
        scope[tail..].copy_from_slice(&self.scope[..split]);
        ImageFrame {
            correlation: self.correlation.value(),
            band_correlation: self.band_correlation.map(|band| band.value()),
            band_level: self.band_correlation.map(|band| band.level()),
            scope,
        }
    }

    fn retune(&mut self) {
        let hz = effective_crossovers(&self.params, self.sample_rate);
        self.mid.retune(hz, self.sample_rate);
        self.side.retune(hz, self.sample_rate);
    }

    fn update_time_constants(&mut self) {
        self.smooth_coeff = time_constant(self.sample_rate, SMOOTHING_SEC);
        self.correlation_coeff = time_constant(self.sample_rate, CORRELATION_SEC);
        self.peak_release = time_constant(self.sample_rate, PEAK_RELEASE_SEC);
    }

    #[inline]
    fn record(&mut self, left: f32, right: f32) {
        self.correlation.push(left, right, self.correlation_coeff);
        self.output_level
            .push(left, right, self.peak_release, self.correlation_coeff);
        self.scope_countdown -= 1;
        if self.scope_countdown == 0 {
            self.scope_countdown = SCOPE_DECIMATION;
            self.scope[self.scope_write * 2] = left;
            self.scope[self.scope_write * 2 + 1] = right;
            self.scope_write += 1;
            if self.scope_write == SCOPE_POINTS {
                self.scope_write = 0;
                self.scope_fresh = true;
            }
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.mid.reset();
        self.side.reset();
        self.correlation.reset();
        for band in &mut self.band_correlation {
            band.reset();
        }
        self.input_level = Level::default();
        self.output_level = Level::default();
        self.scope = [0.0; SCOPE_POINTS * 2];
        self.scope_write = 0;
        self.scope_countdown = SCOPE_DECIMATION;
        self.scope_fresh = false;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        if (sr - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        self.sample_rate = sr;
        self.update_time_constants();
        let hz = effective_crossovers(&self.params, sr);
        self.mid = BandSplitter::new(hz, sr);
        self.side = BandSplitter::new(hz, sr);
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.input_level
            .push(left, right, self.peak_release, self.correlation_coeff);
        if !self.params.power {
            // Bypass is a pure pass-through; the meters keep reading so the
            // editor still shows the image it would be working on.
            self.record(left, right);
            return (left, right);
        }

        let mid = self.mid.run(0.5 * (left + right));
        let side = self.side.run(0.5 * (left - right));

        let glide = 1.0 - self.smooth_coeff;
        let solo = self.params.solo_band;
        let mut mid_out = 0.0;
        let mut side_out = 0.0;
        for band in 0..BAND_COUNT {
            let target = self.params.width[band] * 0.01;
            let gain = &mut self.width_gain[band];
            *gain += (target - *gain) * glide;
            let band_side = side[band] * *gain;
            self.band_correlation[band].push(
                mid[band] + band_side,
                mid[band] - band_side,
                self.correlation_coeff,
            );
            if solo < 0 || solo == band as i32 {
                mid_out += mid[band];
                side_out += band_side;
            }
        }

        self.output_gain += (self.output_target - self.output_gain) * glide;
        let out_l = (mid_out + side_out) * self.output_gain;
        let out_r = (mid_out - side_out) * self.output_gain;
        self.record(out_l, out_r);
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn settle(dsp: &mut Dsp, samples: usize) {
        for i in 0..samples {
            let t = i as f32 / RATE;
            let _ = dsp.process_stereo(0.9 * (t * 440.0).sin(), 0.9 * (t * 660.0).cos());
        }
    }

    /// Steady-state output amplitude of each side for a sine of `hz` fed at
    /// the given input amplitudes. Measured as RMS × √2 over whole periods
    /// rather than as a peak: at 8 samples a period the sampled peak depends
    /// on the phase the filters put the sine at, not on its level.
    fn sine_gain(dsp: &mut Dsp, hz: f32, left_amp: f32, right_amp: f32) -> (f32, f32) {
        dsp.reset();
        let period = (RATE / hz).round() as usize;
        let warm = (RATE * 0.25) as usize;
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

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.category, PluginCategory::Effect);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len(), "duplicate parameter id in descriptor");
        assert_eq!(count, UI_PARAM_IDS.len());

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
        params.width = [0.0; BAND_COUNT];
        dsp.set_params(params);
        assert_eq!(dsp.process_stereo(0.25, -0.5), (0.25, -0.5));
    }

    /// At rest the plugin is an all-pass: every frequency comes out at the
    /// level it went in, on both sides, for a wide (uncorrelated) input.
    #[test]
    fn unity_widths_are_flat_across_the_band_edges() {
        let mut dsp = Dsp::new(RATE);
        for hz in [40.0, 120.0, 400.0, 1_000.0, 3_000.0, 6_000.0, 12_000.0] {
            let (l, r) = sine_gain(&mut dsp, hz, 0.8, 0.3);
            assert!((l - 0.8).abs() < 0.8 * 0.02, "{hz} Hz left came out at {l}");
            assert!(
                (r - 0.3).abs() < 0.3 * 0.03,
                "{hz} Hz right came out at {r}"
            );
        }
    }

    #[test]
    fn zero_width_folds_every_band_to_mono() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.width = [0.0; BAND_COUNT];
        dsp.set_params(params);
        // A pure side signal (left = −right) has nothing left once folded.
        for hz in [60.0, 500.0, 2_000.0, 10_000.0] {
            let (l, r) = sine_gain(&mut dsp, hz, 0.5, -0.5);
            assert!(l < 0.01 && r < 0.01, "{hz} Hz survived as {l}/{r}");
        }
    }

    #[test]
    fn a_band_only_changes_its_own_range() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        // Only the top band (above 6 kHz) is folded.
        params.width[3] = 0.0;
        dsp.set_params(params);
        let (low_l, _) = sine_gain(&mut dsp, 200.0, 0.5, -0.5);
        let (high_l, _) = sine_gain(&mut dsp, 15_000.0, 0.5, -0.5);
        assert!((low_l - 0.5).abs() < 0.02, "200 Hz side moved to {low_l}");
        assert!(high_l < 0.05, "15 kHz side was left at {high_l}");
    }

    #[test]
    fn mid_is_untouched_by_width() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.width = [MAX_WIDTH, 0.0, MAX_WIDTH, 0.0];
        dsp.set_params(params);
        for hz in [80.0, 700.0, 3_000.0, 9_000.0] {
            let (l, r) = sine_gain(&mut dsp, hz, 0.4, 0.4);
            assert!(
                (l - 0.4).abs() < 0.01 && (r - 0.4).abs() < 0.01,
                "{hz} Hz: {l}/{r}"
            );
        }
    }

    #[test]
    fn double_width_doubles_the_side() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.width = [MAX_WIDTH; BAND_COUNT];
        dsp.set_params(params);
        let (l, r) = sine_gain(&mut dsp, 1_500.0, 0.25, -0.25);
        assert!(
            (l - 0.5).abs() < 0.015 && (r - 0.5).abs() < 0.015,
            "{l}/{r}"
        );
    }

    #[test]
    fn solo_plays_one_band_whole() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.solo_band = 0;
        dsp.set_params(params);
        let (inside, _) = sine_gain(&mut dsp, 50.0, 0.5, 0.2);
        let (outside, _) = sine_gain(&mut dsp, 5_000.0, 0.5, 0.2);
        assert!(
            (inside - 0.5).abs() < 0.03,
            "soloed band came out at {inside}"
        );
        assert!(outside < 0.01, "a band outside the solo leaked {outside}");
    }

    #[test]
    fn crossovers_are_used_in_order_whatever_order_they_arrive_in() {
        let mut params = default_params();
        params.crossover_hz = [8_000.0, 90.0, 900.0];
        assert_eq!(effective_crossovers(&params, RATE), [90.0, 900.0, 8_000.0]);
        // …and held under what a biquad can reach at a low rate.
        params.crossover_hz = [100.0, 1_000.0, 20_000.0];
        let hz = effective_crossovers(&params, 22_050.0);
        assert!(hz[2] < 22_050.0 * 0.5);
    }

    #[test]
    fn output_trim_scales_the_result() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.output_db = -6.0;
        dsp.set_params(params);
        let (l, _) = sine_gain(&mut dsp, 1_000.0, 0.5, 0.5);
        assert!((l - 0.5 * db_to_linear(-6.0)).abs() < 0.01, "{l}");
    }

    #[test]
    fn correlation_reads_mono_wide_and_inverted() {
        let mut dsp = Dsp::new(RATE);
        let run = |dsp: &mut Dsp, invert: f32| {
            dsp.reset();
            for i in 0..(RATE as usize) {
                let s = (std::f32::consts::TAU * 330.0 * i as f32 / RATE).sin();
                let _ = dsp.process_stereo(s, s * invert);
            }
            dsp.image_frame().correlation
        };
        assert!(run(&mut dsp, 1.0) > 0.98);
        assert!(run(&mut dsp, -1.0) < -0.98);
        dsp.reset();
        assert_eq!(
            dsp.image_frame().correlation,
            0.0,
            "silence is not a measurement"
        );
    }

    #[test]
    fn narrowing_a_band_raises_its_correlation() {
        let mut dsp = Dsp::new(RATE);
        let feed = |dsp: &mut Dsp| {
            dsp.reset();
            for i in 0..(RATE as usize) {
                let t = i as f32 / RATE;
                let _ = dsp.process_stereo(
                    (std::f32::consts::TAU * 3_000.0 * t).sin(),
                    (std::f32::consts::TAU * 3_000.0 * t).cos(),
                );
            }
            dsp.image_frame().band_correlation[2]
        };
        let wide = feed(&mut dsp);
        let mut params = default_params();
        params.width[2] = 20.0;
        dsp.set_params(params);
        let narrow = feed(&mut dsp);
        assert!(narrow > wide + 0.5, "wide {wide}, narrow {narrow}");
    }

    #[test]
    fn the_scope_fills_and_reports_once_per_trip() {
        let mut dsp = Dsp::new(RATE);
        assert!(dsp.take_image_frame().is_none());
        for _ in 0..SCOPE_POINTS * SCOPE_DECIMATION {
            let _ = dsp.process_stereo(0.5, -0.25);
        }
        let frame = dsp.take_image_frame().expect("a full trip is a frame");
        assert!(
            dsp.take_image_frame().is_none(),
            "the same trip is not reported twice"
        );
        // The newest pairs carry the settled output of a constant input.
        let newest = &frame.scope[SCOPE_POINTS * 2 - 2..];
        assert!((newest[0] - 0.5).abs() < 0.05 && (newest[1] + 0.25).abs() < 0.05);
    }

    #[test]
    fn meters_follow_input_and_output() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.output_db = -12.0;
        dsp.set_params(params);
        settle(&mut dsp, 4_800);
        let frame = dsp.meter_frame();
        assert!(frame.in_peak > 0.8);
        assert!(frame.out_peak < frame.in_peak * 0.5);
        assert!(frame.out_rms > 0.0 && frame.in_rms > frame.out_rms);
        assert!(!frame.in_clip && !frame.out_clip);
    }

    #[test]
    fn extreme_settings_stay_finite() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.crossover_hz = [20.0, 20.0, 20_000.0];
        params.width = [MAX_WIDTH; BAND_COUNT];
        params.output_db = MAX_OUTPUT_DB;
        dsp.set_params(params);
        for i in 0..20_000 {
            let x = if i % 2 == 0 { 1.0 } else { -1.0 };
            let (l, r) = dsp.process_stereo(x, -x);
            assert!(l.is_finite() && r.is_finite());
        }
    }

    #[test]
    fn sample_rate_change_keeps_the_rest_state_flat() {
        let mut dsp = Dsp::new(RATE);
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
