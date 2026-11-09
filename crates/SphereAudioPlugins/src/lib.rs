//! SphereAudioPlugins — lightweight realtime DSP framework for Futureboard Studio.
//!
//! This crate is intentionally UI-agnostic and host-agnostic. DAUx uses it for
//! native realtime insert processing, while extension packages can use the same
//! metadata shape to describe editor UIs and default parameters.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type PluginParams = HashMap<String, Value>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub category: PluginCategory,
    pub version: String,
    pub params: Vec<ParamDescriptor>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PluginCategory {
    Effect,
    Instrument,
    Analyzer,
    Utility,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamDescriptor {
    pub id: String,
    pub name: String,
    pub default_value: f32,
    pub min: f32,
    pub max: f32,
    pub unit: String,
}

#[derive(Debug, Clone)]
pub struct AudioPluginDspState {
    sample_rate: u32,
    eq_l: Vec<Biquad>,
    eq_r: Vec<Biquad>,
    /// Cached process-path params. Resolved when the insert is built or a
    /// parameter changes — never from the sample loop.
    power: bool,
    is_eq: bool,
    drive: f32,
    peak_reduction: f32,
    compress_ratio: f32,
    output_gain: f32,
    mix: f32,
}

impl AudioPluginDspState {
    pub fn new(plugin_id: &str, params: &PluginParams, sample_rate: u32) -> Self {
        let mut state = Self {
            sample_rate: sample_rate.max(1),
            eq_l: Vec::new(),
            eq_r: Vec::new(),
            power: true,
            is_eq: false,
            drive: 0.0,
            peak_reduction: 0.0,
            compress_ratio: 3.0,
            output_gain: 1.0,
            mix: 1.0,
        };
        state.rebuild(plugin_id, params, sample_rate);
        state
    }

    pub fn rebuild(&mut self, plugin_id: &str, params: &PluginParams, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        self.eq_l.clear();
        self.eq_r.clear();
        self.refresh_process_params(plugin_id, params);
        if !self.is_eq {
            return;
        }

        for band in 1..=8 {
            let prefix = format!("band{band}");
            if !param_bool(params, &format!("{prefix}Active"), true) {
                continue;
            }
            let band_type = param_str(params, &format!("{prefix}Type"), "bell");
            let freq = param_f32(params, &format!("{prefix}Freq"), 1000.0).clamp(20.0, 20_000.0);
            let gain = param_f32(params, &format!("{prefix}Gain"), 0.0).clamp(-18.0, 18.0);
            let q = param_f32(params, &format!("{prefix}Q"), 1.0).clamp(0.1, 12.0);
            let Some(filter) =
                Biquad::from_eq_band(band_type, freq, gain, q, self.sample_rate as f32)
            else {
                continue;
            };
            self.eq_l.push(filter.clone());
            self.eq_r.push(filter);
        }
    }

    /// Resolve sample-path scalars from the live param map.
    ///
    /// Call when the insert is built, the sample rate changes, or a parameter
    /// edit lands. The audio block reads the cached scalars and does not look
    /// parameters up by name.
    pub fn refresh_process_params(&mut self, plugin_id: &str, params: &PluginParams) {
        self.power = param_bool(params, "power", true);
        self.is_eq = is_eq_plugin(plugin_id);
        self.drive = (param_f32(params, "drive", 0.0)
            + param_f32(params, "saturation", 0.0) * 0.5
            + param_f32(params, "color", 0.0) * 0.12)
            .clamp(0.0, 100.0)
            / 100.0;
        self.peak_reduction = param_f32(params, "peakReduction", 0.0).clamp(0.0, 100.0) / 100.0;
        self.compress_ratio = if is_limiter_plugin(plugin_id) {
            10.0
        } else {
            3.0
        };
        let mut db = 0.0;
        db += param_f32(params, "outputDb", 0.0);
        db += param_f32(params, "gainDb", 0.0);
        db += param_f32(params, "outputTrimDb", 0.0);
        db += param_f32(params, "out", 0.0);
        self.output_gain = db_to_linear(db);
        self.mix = param_f32(params, "mix", 100.0).clamp(0.0, 100.0) / 100.0;
    }

    /// Take `next`'s settings — scalars and filter coefficients — while this
    /// state keeps its running filter memory. Realtime-safe: it neither
    /// allocates nor frees.
    ///
    /// A parameter edit is prepared on the control thread, whose copy of the
    /// state never processes audio, so its filters sit at rest. Swapping that
    /// copy in reset the running EQ on every edit, even of Output or Mix, and
    /// clicked (#366). Only when the edit changes how many bands run do the
    /// filter lists trade places: the old list goes back in `next` (for the
    /// caller to free off the audio thread) and the new bands start from rest.
    pub fn adopt_settings(&mut self, next: &mut Self) {
        self.sample_rate = next.sample_rate;
        self.power = next.power;
        self.is_eq = next.is_eq;
        self.drive = next.drive;
        self.peak_reduction = next.peak_reduction;
        self.compress_ratio = next.compress_ratio;
        self.output_gain = next.output_gain;
        self.mix = next.mix;
        if self.eq_l.len() == next.eq_l.len() && self.eq_r.len() == next.eq_r.len() {
            for (running, settings) in self
                .eq_l
                .iter_mut()
                .zip(&next.eq_l)
                .chain(self.eq_r.iter_mut().zip(&next.eq_r))
            {
                running.take_coefficients(settings);
            }
        } else {
            std::mem::swap(&mut self.eq_l, &mut next.eq_l);
            std::mem::swap(&mut self.eq_r, &mut next.eq_r);
        }
    }
}

impl Default for AudioPluginDspState {
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            eq_l: Vec::new(),
            eq_r: Vec::new(),
            power: true,
            is_eq: false,
            drive: 0.0,
            peak_reduction: 0.0,
            compress_ratio: 3.0,
            output_gain: 1.0,
            mix: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn from_eq_band(kind: &str, freq: f32, gain_db: f32, q: f32, sample_rate: f32) -> Option<Self> {
        let nyquist = sample_rate * 0.5;
        let f0 = freq.clamp(10.0, nyquist * 0.96);
        let q = q.clamp(0.1, 12.0);
        let w0 = std::f32::consts::TAU * f0 / sample_rate.max(1.0);
        let sin = w0.sin();
        let cos = w0.cos();
        let alpha = sin / (2.0 * q);
        let a = 10.0f32.powf(gain_db / 40.0);

        let (b0, b1, b2, a0, a1, a2) = match kind {
            "bell" | "peak" | "peaking" => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            "notch" => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            "lowpass" | "lp" => (
                (1.0 - cos) * 0.5,
                1.0 - cos,
                (1.0 - cos) * 0.5,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            "highpass" | "hp" => (
                (1.0 + cos) * 0.5,
                -(1.0 + cos),
                (1.0 + cos) * 0.5,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            "lowshelf" | "ls" => make_shelf(true, cos, sin, a, q),
            "highshelf" | "hs" => make_shelf(false, cos, sin, a, q),
            _ => return None,
        };

        let inv_a0 = 1.0 / a0.max(1.0e-8);
        Some(Self {
            b0: b0 * inv_a0,
            b1: b1 * inv_a0,
            b2: b2 * inv_a0,
            a1: a1 * inv_a0,
            a2: a2 * inv_a0,
            z1: 0.0,
            z2: 0.0,
        })
    }

    /// New coefficients; the filter memory (`z1`, `z2`) carries on.
    #[inline]
    fn take_coefficients(&mut self, other: &Self) {
        self.b0 = other.b0;
        self.b1 = other.b1;
        self.b2 = other.b2;
        self.a1 = other.a1;
        self.a2 = other.a2;
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        if output.is_finite() {
            output
        } else {
            // A non-finite sample (from upstream, or a blown-up state) has
            // poisoned the state too: muting the output alone would leave
            // the filter at NaN, silent for good. Start over from rest.
            self.z1 = 0.0;
            self.z2 = 0.0;
            0.0
        }
    }
}

/// Process one stereo sample through a plugin insert.
///
/// Allocation-free. Coefficients and scalars live in `AudioPluginDspState`;
/// refresh them when parameters change, not from this function. `plugin_id`
/// and `params` stay in the signature for existing callers.
#[inline]
pub fn process_stereo_sample(
    plugin_id: &str,
    enabled: bool,
    params: &PluginParams,
    state: &mut AudioPluginDspState,
    l: f32,
    r: f32,
) -> (f32, f32) {
    let _ = plugin_id;
    let _ = params;
    if !enabled || !state.power {
        return (l, r);
    }

    let mut wet_l = l;
    let mut wet_r = r;

    if state.is_eq {
        for filter in &mut state.eq_l {
            wet_l = filter.process(wet_l);
        }
        for filter in &mut state.eq_r {
            wet_r = filter.process(wet_r);
        }
    }

    let drive = state.drive;
    if drive > 0.0 {
        let amount = 1.0 + drive * 8.0;
        wet_l = (wet_l * amount).tanh() / amount.tanh().max(0.001);
        wet_r = (wet_r * amount).tanh() / amount.tanh().max(0.001);
    }

    let reduction = state.peak_reduction;
    if reduction > 0.0 {
        let threshold = 0.9 - reduction * 0.82;
        let ratio = state.compress_ratio;
        wet_l = soft_knee_compress(wet_l, threshold, ratio);
        wet_r = soft_knee_compress(wet_r, threshold, ratio);
    }

    wet_l *= state.output_gain;
    wet_r *= state.output_gain;

    let mix = state.mix;
    (
        (l * (1.0 - mix) + wet_l * mix).clamp(-1.5, 1.5),
        (r * (1.0 - mix) + wet_r * mix).clamp(-1.5, 1.5),
    )
}

pub fn builtin_descriptors() -> Vec<PluginDescriptor> {
    vec![
        PluginDescriptor {
            id: "sphere.eq8".to_string(),
            name: "Sphere EQ8".to_string(),
            vendor: "Futureboard Digital Technologies".to_string(),
            category: PluginCategory::Effect,
            version: env!("CARGO_PKG_VERSION").to_string(),
            params: vec![
                param("power", "Power", 1.0, 0.0, 1.0, "bool"),
                param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
                param("outputDb", "Output", 0.0, -24.0, 24.0, "dB"),
            ],
        },
        PluginDescriptor {
            id: "sphere.drive".to_string(),
            name: "Sphere Drive".to_string(),
            vendor: "Futureboard Digital Technologies".to_string(),
            category: PluginCategory::Effect,
            version: env!("CARGO_PKG_VERSION").to_string(),
            params: vec![
                param("power", "Power", 1.0, 0.0, 1.0, "bool"),
                param("drive", "Drive", 0.0, 0.0, 100.0, "%"),
                param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
                param("outputDb", "Output", 0.0, -24.0, 24.0, "dB"),
            ],
        },
        PluginDescriptor {
            id: "sphere.comp".to_string(),
            name: "Sphere Compressor".to_string(),
            vendor: "Futureboard Digital Technologies".to_string(),
            category: PluginCategory::Effect,
            version: env!("CARGO_PKG_VERSION").to_string(),
            params: vec![
                param("power", "Power", 1.0, 0.0, 1.0, "bool"),
                param("peakReduction", "Peak Reduction", 0.0, 0.0, 100.0, "%"),
                param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
                param("outputDb", "Output", 0.0, -24.0, 24.0, "dB"),
            ],
        },
    ]
}

pub fn canonical_plugin_id(kind: &str) -> &str {
    let kind = kind.trim();
    if kind.eq_ignore_ascii_case("eq")
        || kind.eq_ignore_ascii_case("equz8")
        || kind.eq_ignore_ascii_case("sphere.eq8")
    {
        "sphere.eq8"
    } else if kind.eq_ignore_ascii_case("drive")
        || kind.eq_ignore_ascii_case("saturation")
        || kind.eq_ignore_ascii_case("sphere.drive")
    {
        "sphere.drive"
    } else if kind.eq_ignore_ascii_case("limiter") || kind.eq_ignore_ascii_case("sphere.limit") {
        // Distinct from the compressor: the sample path uses this id to pick
        // the 10:1 ratio. Folding it into `sphere.comp` compressed at 3:1.
        "sphere.limit"
    } else if kind.eq_ignore_ascii_case("comp")
        || kind.eq_ignore_ascii_case("compressor")
        || kind.eq_ignore_ascii_case("sphere.comp")
    {
        "sphere.comp"
    } else {
        kind
    }
}

pub fn should_rebuild_state(plugin_id: &str, param_id: &str) -> bool {
    is_eq_plugin(plugin_id) && (param_id == "power" || param_id.starts_with("band"))
}

#[inline]
pub fn is_eq_plugin(plugin_id: &str) -> bool {
    plugin_id.eq_ignore_ascii_case("eq")
        || plugin_id.eq_ignore_ascii_case("equz8")
        || plugin_id.eq_ignore_ascii_case("sphere.eq8")
        || contains_ascii_ignore_case(plugin_id, "eq")
}

#[inline]
fn is_limiter_plugin(plugin_id: &str) -> bool {
    plugin_id.eq_ignore_ascii_case("limiter")
        || plugin_id.eq_ignore_ascii_case("sphere.limit")
        || contains_ascii_ignore_case(plugin_id, "limit")
}

#[inline]
fn contains_ascii_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[inline]
pub fn param_f32(params: &PluginParams, key: &str, fallback: f32) -> f32 {
    params
        .get(key)
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .unwrap_or(fallback)
}

#[inline]
pub fn param_bool(params: &PluginParams, key: &str, fallback: bool) -> bool {
    params
        .get(key)
        .and_then(|v| v.as_bool().or_else(|| v.as_f64().map(|n| n >= 0.5)))
        .unwrap_or(fallback)
}

#[inline]
pub fn param_str<'a>(params: &'a PluginParams, key: &str, fallback: &'a str) -> &'a str {
    params.get(key).and_then(|v| v.as_str()).unwrap_or(fallback)
}

#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

#[inline]
pub fn soft_knee_compress(x: f32, threshold: f32, ratio: f32) -> f32 {
    let sign = x.signum();
    let abs = x.abs();
    if abs <= threshold {
        return x;
    }
    let over = abs - threshold;
    sign * (threshold + over / ratio.max(1.0))
}

fn make_shelf(low: bool, cos: f32, sin: f32, a: f32, q: f32) -> (f32, f32, f32, f32, f32, f32) {
    let slope = q.clamp(0.1, 1.0);
    let alpha = (sin * 0.5)
        * ((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0)
            .max(0.0001)
            .sqrt();
    let beta = 2.0 * a.sqrt() * alpha;
    if low {
        (
            a * ((a + 1.0) - (a - 1.0) * cos + beta),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - beta),
            (a + 1.0) + (a - 1.0) * cos + beta,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - beta,
        )
    } else {
        (
            a * ((a + 1.0) + (a - 1.0) * cos + beta),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - beta),
            (a + 1.0) - (a - 1.0) * cos + beta,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - beta,
        )
    }
}

fn param(
    id: &str,
    name: &str,
    default_value: f32,
    min: f32,
    max: f32,
    unit: &str,
) -> ParamDescriptor {
    ParamDescriptor {
        id: id.to_string(),
        name: name.to_string(),
        default_value,
        min,
        max,
        unit: unit.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq_with_one_band() -> PluginParams {
        let mut params = PluginParams::new();
        for band in 2..=8 {
            params.insert(format!("band{band}Active"), Value::Bool(false));
        }
        params.insert("band1Type".to_string(), Value::from("bell"));
        params.insert("band1Freq".to_string(), Value::from(1_000.0));
        params.insert("band1Gain".to_string(), Value::from(6.0));
        params.insert("band1Q".to_string(), Value::from(1.0));
        params
    }

    /// One NaN or Inf sample from upstream must not silence the EQ for the
    /// rest of the session: the next normal samples pass again.
    #[test]
    fn the_eq_recovers_from_a_non_finite_sample() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let params = eq_with_one_band();
            let mut state = AudioPluginDspState::new("sphere.eq8", &params, 48_000);
            assert_eq!(state.eq_l.len(), 1);
            let sine =
                |n: usize| (n as f32 * std::f32::consts::TAU * 1_000.0 / 48_000.0).sin() * 0.25;
            for n in 0..480 {
                let _ = process_stereo_sample(
                    "sphere.eq8",
                    true,
                    &params,
                    &mut state,
                    sine(n),
                    sine(n),
                );
            }
            let _ = process_stereo_sample("sphere.eq8", true, &params, &mut state, bad, bad);
            let mut peak = 0.0f32;
            for n in 481..4_800 {
                let (l, r) = process_stereo_sample(
                    "sphere.eq8",
                    true,
                    &params,
                    &mut state,
                    sine(n),
                    sine(n),
                );
                assert!(l.is_finite() && r.is_finite(), "{bad}: sample {n}");
                if n > 2_400 {
                    peak = peak.max(l.abs()).max(r.abs());
                }
            }
            // +6 dB at its centre: about twice the 0.25 input.
            assert!(peak > 0.4, "{bad}: the EQ stayed silent (peak {peak})");
        }
    }

    /// #366: an edit prepared on the control thread arrives as a fresh state
    /// whose filters are at rest. Adopting it must keep the running EQ's
    /// filter memory, so a running EQ carries on exactly as one that simply
    /// refreshed its scalars (Output, Mix) or took new coefficients (a band
    /// gain) in place; only a change of band count starts the new bands.
    #[test]
    fn adopting_an_edit_keeps_the_running_filter_memory() {
        let id = "sphere.eq8";
        let sine = |n: usize| (n as f32 * std::f32::consts::TAU * 80.0 / 48_000.0).sin() * 0.5;
        let params = eq_with_one_band();
        let mut running = AudioPluginDspState::new(id, &params, 48_000);
        for n in 0..4_800 {
            let _ = process_stereo_sample(id, true, &params, &mut running, sine(n), sine(n));
        }
        assert!(running.eq_l[0].z1 != 0.0);

        // Output and Mix: only the scalars move.
        let mut edited = params.clone();
        edited.insert("outputDb".to_string(), Value::from(-3.0));
        edited.insert("mix".to_string(), Value::from(80.0));
        let mut expected = running.clone();
        expected.refresh_process_params(id, &edited);
        let mut prepared = AudioPluginDspState::new(id, &edited, 48_000);
        running.adopt_settings(&mut prepared);
        for n in 4_800..4_900 {
            let got = process_stereo_sample(id, true, &edited, &mut running, sine(n), sine(n));
            let want = process_stereo_sample(id, true, &edited, &mut expected, sine(n), sine(n));
            assert_eq!(got, want, "sample {n} after an Output/Mix edit");
        }

        // A band gain: new coefficients, same memory.
        edited.insert("band1Gain".to_string(), Value::from(9.0));
        let mut prepared = AudioPluginDspState::new(id, &edited, 48_000);
        let mut expected = running.clone();
        for (e, p) in expected.eq_l.iter_mut().zip(&prepared.eq_l) {
            e.take_coefficients(p);
        }
        for (e, p) in expected.eq_r.iter_mut().zip(&prepared.eq_r) {
            e.take_coefficients(p);
        }
        let z1 = running.eq_l[0].z1;
        running.adopt_settings(&mut prepared);
        assert_eq!(running.eq_l[0].z1, z1);
        assert_eq!(running.eq_l[0].b0, prepared.eq_l[0].b0);
        let got = process_stereo_sample(id, true, &edited, &mut running, 0.1, 0.1);
        let want = process_stereo_sample(id, true, &edited, &mut expected, 0.1, 0.1);
        assert_eq!(got, want);

        // A second band switched on: the lists trade places, the old one goes
        // back to the caller to be freed off the audio thread.
        edited.insert("band2Active".to_string(), Value::Bool(true));
        let mut prepared = AudioPluginDspState::new(id, &edited, 48_000);
        running.adopt_settings(&mut prepared);
        assert_eq!(running.eq_l.len(), 2);
        assert_eq!(prepared.eq_l.len(), 1);
    }

    fn compressing(kind: &str, peak_reduction: f32) -> (PluginParams, AudioPluginDspState) {
        let mut params = PluginParams::new();
        params.insert("peakReduction".to_string(), Value::from(peak_reduction));
        let id = canonical_plugin_id(kind);
        let state = AudioPluginDspState::new(id, &params, 48_000);
        (params, state)
    }

    /// Full-scale into 100% peak reduction. Threshold is 0.08; the overshoot
    /// is divided by the ratio.
    #[test]
    fn limiter_compresses_at_ten_to_one_and_the_compressor_at_three_to_one() {
        assert_eq!(canonical_plugin_id("limiter"), "sphere.limit");
        assert_eq!(canonical_plugin_id("comp"), "sphere.comp");

        let (params, mut limiter) = compressing("limiter", 100.0);
        let (l, r) = process_stereo_sample(
            canonical_plugin_id("limiter"),
            true,
            &params,
            &mut limiter,
            1.0,
            -1.0,
        );
        let limited = soft_knee_compress(1.0, 0.08, 10.0);
        assert!((l - limited).abs() < 1.0e-5, "limiter left {l}");
        assert!((r + limited).abs() < 1.0e-5, "limiter right {r}");

        let (params, mut comp) = compressing("compressor", 100.0);
        let (l, _) = process_stereo_sample(
            canonical_plugin_id("compressor"),
            true,
            &params,
            &mut comp,
            1.0,
            1.0,
        );
        let compressed = soft_knee_compress(1.0, 0.08, 3.0);
        assert!((l - compressed).abs() < 1.0e-5, "compressor {l}");
        assert!((limited - compressed).abs() > 0.1);
    }

    /// Editing the param map must not change the sample path until a refresh.
    /// That is what keeps name lookups off the audio block.
    #[test]
    fn the_sample_path_uses_cached_params_not_the_name_map() {
        let (mut params, mut state) = compressing("limiter", 100.0);
        let id = canonical_plugin_id("limiter");
        let (before, _) = process_stereo_sample(id, true, &params, &mut state, 1.0, 1.0);
        params.insert("peakReduction".to_string(), Value::from(0.0));
        let (during, _) = process_stereo_sample(id, true, &params, &mut state, 1.0, 1.0);
        assert_eq!(before, during);
        state.refresh_process_params(id, &params);
        let (after, _) = process_stereo_sample(id, true, &params, &mut state, 1.0, 1.0);
        assert!(
            (after - 1.0).abs() < 1.0e-5,
            "refresh did not release the limiter: {after}"
        );
    }
}
