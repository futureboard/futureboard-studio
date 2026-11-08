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
    /// Cached process-path params (refreshed on rebuild / once per block).
    /// Avoids string-keyed HashMap lookups and allocations on the sample loop.
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

    /// Resolve sample-path scalars from the live param map. Call from the control
    /// thread (rebuild) or once per audio block before the sample loop.
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

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        if output.is_finite() {
            output
        } else {
            0.0
        }
    }
}

/// Process one stereo sample through a plugin insert.
///
/// This function is allocation-free for the realtime path. Any plugin that
/// needs prepared coefficients must keep them inside `AudioPluginDspState` and
/// rebuild from the control thread when parameters change.
#[inline]
pub fn process_stereo_sample(
    plugin_id: &str,
    enabled: bool,
    params: &PluginParams,
    state: &mut AudioPluginDspState,
    l: f32,
    r: f32,
) -> (f32, f32) {
    // Keep params usable for callers that mutate the map without rebuild —
    // refresh is cheap vs HashMap lookups *per sample*.
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
    } else if kind.eq_ignore_ascii_case("comp")
        || kind.eq_ignore_ascii_case("compressor")
        || kind.eq_ignore_ascii_case("limiter")
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
