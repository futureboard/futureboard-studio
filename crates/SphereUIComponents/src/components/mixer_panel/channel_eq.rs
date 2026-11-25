//! The EQ curve a mixer strip draws: what the strip's EQ inserts — EQ-Z8,
//! EQ-ZX, and FabFilter Pro-Q, Pro-Q 2, Pro-Q 3 and Pro-Q 4 as VST3 — do to
//! the signal together, read from each insert's own parameters.
//!
//! The built-ins draw from their mirrored state with their own response
//! functions, so their curve is the one that plays. A Pro-Q is rebuilt from
//! the parameter list its host reported — each band's frequency, gain, Q,
//! shape and slope, parsed from the plug-in's own text for the value — and
//! drawn with analog filter prototypes: the shape is right, but it is a
//! picture of the settings, not a measurement of the plug-in.

use crate::components::timeline::timeline_state::{
    InsertLoadStatus, InsertPluginFormat, InsertSlotState, PluginParameterState,
};

/// Points across the curve, log-spaced from [`FREQ_MIN`] to [`FREQ_MAX`].
pub(crate) const CURVE_POINTS: usize = 72;
pub(crate) const FREQ_MIN: f32 = 20.0;
pub(crate) const FREQ_MAX: f32 = 20_000.0;
/// The sample rate the built-ins' digital responses are drawn at. The strip
/// shows the shape of a setting, which barely moves with the rate below the
/// top octave.
const DRAW_SAMPLE_RATE: f32 = 48_000.0;
/// Lowest level a curve point is kept at: a notch's centre is -inf.
const FLOOR_DB: f32 = -60.0;

/// The frequency of curve point `index`.
pub(crate) fn point_freq(index: usize) -> f32 {
    let fraction = index as f32 / (CURVE_POINTS - 1) as f32;
    FREQ_MIN * (FREQ_MAX / FREQ_MIN).powf(fraction)
}

/// Fraction across the plot (0 left, 1 right) of `hz`.
pub(crate) fn freq_fraction(hz: f32) -> f32 {
    (hz.clamp(FREQ_MIN, FREQ_MAX) / FREQ_MIN).ln() / (FREQ_MAX / FREQ_MIN).ln()
}

/// Which EQ an insert holds, as far as the strip can draw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EqKind {
    Equz8,
    Equzx,
    /// FabFilter Pro-Q, by major version (1 for the first).
    ProQ(u8),
}

impl EqKind {
    /// The EQ `slot` holds, or `None` for anything the strip cannot draw.
    pub(crate) fn of(slot: &InsertSlotState) -> Option<Self> {
        let plugin_id = slot.plugin_id.as_deref()?;
        // A slot holds a built-in under its stem ("equz8"), the registry
        // under "builtin:equz8": the host's resolver takes either.
        match SpherePluginHost::resolve_builtin_stem(plugin_id) {
            Some(EQUZ8_STEM) => return Some(Self::Equz8),
            Some(EQUZX_STEM) => return Some(Self::Equzx),
            Some(_) => return None,
            None => {}
        }
        if slot.plugin_format != Some(InsertPluginFormat::Vst3) {
            return None;
        }
        // The registry name, else the bundle's: "FabFilter Pro-Q 3".
        let stem = slot
            .plugin_path
            .as_deref()
            .and_then(|path| path.file_stem())
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        pro_q_version(&slot.display_name)
            .or_else(|| pro_q_version(&stem))
            .map(Self::ProQ)
    }
}

/// The built-in EQs' catalog stems.
const EQUZ8_STEM: &str = "equz8";
const EQUZX_STEM: &str = "equzx";

/// The Pro-Q version `name` names, or `None` when it names no Pro-Q.
fn pro_q_version(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    let at = lower.find("pro-q")?;
    let rest = lower[at + "pro-q".len()..].trim_start();
    Some(match rest.chars().next() {
        Some(digit @ '2'..='9') => digit as u8 - b'0',
        _ => 1,
    })
}

/// A strip's EQ: the curve its EQ inserts make together, and which insert a
/// click on it opens.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChannelEq {
    /// Level at each of [`CURVE_POINTS`] frequencies ([`point_freq`]), in dB.
    pub db: Vec<f32>,
    /// The first EQ insert in the chain: its index in the chain and its id.
    pub first: (usize, String),
    /// The EQ inserts drawn, in chain order, for the tooltip.
    pub names: Vec<String>,
    /// Whether any EQ insert is in the signal path. A chain whose EQs are all
    /// bypassed or switched off still has a section, drawn flat and dim.
    pub live: bool,
    /// Whether a Pro-Q's curve is still unknown because its host has not
    /// reported its values yet.
    pub pending: bool,
}

/// What `inserts` (a channel's chain, instrument included) do as EQ, or
/// `None` when the chain holds no EQ the strip can draw.
pub(crate) fn channel_eq(inserts: &[InsertSlotState]) -> Option<ChannelEq> {
    let mut eq: Option<ChannelEq> = None;
    for (index, slot) in inserts.iter().enumerate() {
        let Some(kind) = EqKind::of(slot) else {
            continue;
        };
        let entry = eq.get_or_insert_with(|| ChannelEq {
            db: vec![0.0; CURVE_POINTS],
            first: (index, slot.id.clone()),
            names: Vec::new(),
            live: false,
            pending: false,
        });
        entry.names.push(slot.display_name.clone());
        let in_path = slot.enabled
            && !slot.bypassed
            && !matches!(slot.load_status, InsertLoadStatus::Disabled);
        if !in_path {
            continue;
        }
        let added = match kind {
            EqKind::Equz8 => builtin::equz8(&slot.id, &mut entry.db),
            EqKind::Equzx => builtin::equzx(&slot.id, &mut entry.db),
            EqKind::ProQ(version) => match ProQ::read(&slot.parameters, version) {
                Some(pro_q) => pro_q.add_to(&mut entry.db),
                None => {
                    entry.pending = true;
                    false
                }
            },
        };
        entry.live |= added;
    }
    eq
}

/// `wet_db` heard through a wet/dry blend at `mix` (0..1), with the two in
/// phase: what the strip can say about a mix without the plug-in's phase.
fn blend_db(wet_db: f32, mix: f32) -> f32 {
    if mix >= 0.999 {
        return wet_db;
    }
    let gain = (1.0 - mix) + mix * 10f32.powf(wet_db / 20.0);
    20.0 * gain.max(1e-3).log10()
}

#[cfg(feature = "builtin-plugin-editor")]
mod builtin {
    use super::{blend_db, point_freq, DRAW_SAMPLE_RATE};
    use crate::components::builtin_plugin_editor as host;

    /// Adds an EQ-Z8 insert's response to `db`. A fresh insert with nothing
    /// mirrored plays its defaults. `false` when the plug-in is switched off.
    pub(super) fn equz8(insert_id: &str, db: &mut [f32]) -> bool {
        let params = host::builtin_equz8_params(insert_id).unwrap_or_else(equz8::default_params);
        if !params.power {
            return false;
        }
        let mix = (params.mix / 100.0).clamp(0.0, 1.0);
        for (index, level) in db.iter_mut().enumerate() {
            let wet = equz8::response_db(&params, point_freq(index), DRAW_SAMPLE_RATE);
            *level += blend_db(wet, mix);
        }
        true
    }

    /// [`equz8`] for an EQ-ZX: its whole-image curve, auto gain included.
    pub(super) fn equzx(insert_id: &str, db: &mut [f32]) -> bool {
        let params = host::builtin_equzx_params(insert_id).unwrap_or_else(equzx::default_params);
        if !params.power {
            return false;
        }
        let mix = (params.mix / 100.0).clamp(0.0, 1.0);
        let auto_gain = equzx::auto_gain_db(&params, DRAW_SAMPLE_RATE);
        for (index, level) in db.iter_mut().enumerate() {
            let wet = equzx::response_db(
                &params,
                equzx::BandChannel::Stereo,
                point_freq(index),
                DRAW_SAMPLE_RATE,
            ) + auto_gain;
            *level += blend_db(wet, mix);
        }
        true
    }
}

/// Without the built-in editors their state is not mirrored: nothing to draw.
#[cfg(not(feature = "builtin-plugin-editor"))]
mod builtin {
    pub(super) fn equz8(_insert_id: &str, _db: &mut [f32]) -> bool {
        false
    }

    pub(super) fn equzx(_insert_id: &str, _db: &mut [f32]) -> bool {
        false
    }
}

// ── Pro-Q ──────────────────────────────────────────────────────────────────

/// The band shapes the strip draws. A Pro-Q shape outside these (Flat Tilt,
/// All Pass) is left out of the curve rather than drawn as something else.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Bell,
    LowShelf,
    HighShelf,
    LowCut,
    HighCut,
    Notch,
    BandPass,
    TiltShelf,
}

impl Shape {
    fn parse(text: &str) -> Option<Self> {
        let text = text.to_ascii_lowercase();
        // Most specific first: "Flat Tilt" is not a tilt shelf.
        if text.contains("flat tilt") || text.contains("all pass") {
            return None;
        }
        Some(if text.contains("low cut") {
            Self::LowCut
        } else if text.contains("high cut") {
            Self::HighCut
        } else if text.contains("low shelf") {
            Self::LowShelf
        } else if text.contains("high shelf") {
            Self::HighShelf
        } else if text.contains("tilt") {
            Self::TiltShelf
        } else if text.contains("notch") {
            Self::Notch
        } else if text.contains("band pass") || text.contains("bandpass") {
            Self::BandPass
        } else if text.contains("bell") {
            Self::Bell
        } else {
            return None;
        })
    }

    fn has_gain(self) -> bool {
        matches!(
            self,
            Self::Bell | Self::LowShelf | Self::HighShelf | Self::TiltShelf
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Band {
    shape: Shape,
    freq: f32,
    gain_db: f32,
    q: f32,
    /// Cut slope in dB/oct.
    slope: f32,
}

impl Band {
    fn response_db(&self, hz: f32) -> f32 {
        // Frequency relative to the band's, where the prototypes are written.
        let w = hz / self.freq.max(1.0);
        let q = self.q.max(0.025);
        let db = match self.shape {
            Shape::Bell => {
                let a = 10f32.powf(self.gain_db / 40.0);
                analog_db([1.0, a / q, 1.0], [1.0, 1.0 / (a * q), 1.0], w)
            }
            Shape::LowShelf => low_shelf_db(self.gain_db, q, w),
            Shape::HighShelf => high_shelf_db(self.gain_db, q, w),
            // Half the tilt each side of the frequency, through gentle shelves.
            Shape::TiltShelf => {
                high_shelf_db(self.gain_db / 2.0, 0.5, w)
                    + low_shelf_db(-self.gain_db / 2.0, 0.5, w)
            }
            Shape::LowCut => cut_db(self.slope, q, w, true),
            Shape::HighCut => cut_db(self.slope, q, w, false),
            Shape::Notch => analog_db([1.0, 0.0, 1.0], [1.0, 1.0 / q, 1.0], w),
            Shape::BandPass => analog_db([0.0, 1.0 / q, 0.0], [1.0, 1.0 / q, 1.0], w),
        };
        db.max(FLOOR_DB)
    }
}

/// `|H(jw)|` in dB of `(n0 s² + n1 s + n2) / (d0 s² + d1 s + d2)`.
fn analog_db(num: [f32; 3], den: [f32; 3], w: f32) -> f32 {
    let w2 = w * w;
    let (nr, ni) = (num[2] - num[0] * w2, num[1] * w);
    let (dr, di) = (den[2] - den[0] * w2, den[1] * w);
    let num_mag = nr * nr + ni * ni;
    let den_mag = (dr * dr + di * di).max(1e-30);
    10.0 * (num_mag / den_mag).max(1e-12).log10()
}

fn low_shelf_db(gain_db: f32, q: f32, w: f32) -> f32 {
    let a = 10f32.powf(gain_db / 40.0);
    let k = a.sqrt() / q;
    analog_db([a, a * k, a * a], [a, k, 1.0], w)
}

fn high_shelf_db(gain_db: f32, q: f32, w: f32) -> f32 {
    let a = 10f32.powf(gain_db / 40.0);
    let k = a.sqrt() / q;
    analog_db([a * a, a * k, a], [1.0, k, a], w)
}

/// A low cut (`high_pass`) or high cut of `slope` dB/oct. 12 dB/oct keeps the
/// band's Q (its resonance); any other slope is a Butterworth of that order.
fn cut_db(slope: f32, q: f32, w: f32, high_pass: bool) -> f32 {
    let order = (slope / 6.0).round().max(1.0);
    if order == 2.0 {
        return if high_pass {
            analog_db([1.0, 0.0, 0.0], [1.0, 1.0 / q, 1.0], w)
        } else {
            analog_db([0.0, 0.0, 1.0], [1.0, 1.0 / q, 1.0], w)
        };
    }
    let ratio = if high_pass { 1.0 / w.max(1e-6) } else { w };
    // 1 + ratio^(2n), in dB; past 1e6 the ratio term is all of it.
    let log_ratio = ratio.max(1e-12).log10() * 2.0 * order;
    if log_ratio > 6.0 {
        -10.0 * log_ratio
    } else {
        -10.0 * (1.0 + 10f32.powf(log_ratio)).log10()
    }
}

/// A Pro-Q's settings, as its parameters report them.
#[derive(Clone, Debug, PartialEq)]
struct ProQ {
    bands: Vec<Band>,
    output_db: f32,
    bypassed: bool,
}

impl ProQ {
    /// The settings in `parameters`, or `None` while they say nothing yet —
    /// no list, or a list without the values' text.
    fn read(parameters: &[PluginParameterState], version: u8) -> Option<Self> {
        if parameters.iter().all(|param| param.value_text.is_empty()) {
            return None;
        }
        let find = |name: &str| {
            parameters
                .iter()
                .find(|param| param.name.eq_ignore_ascii_case(name))
                .map(|param| param.value_text.as_str())
        };
        let bypassed = find("Bypass").is_some_and(says_bypassed)
            || find("Host Bypass").is_some_and(says_bypassed);
        let output_db = find("Output Level")
            .or_else(|| find("Output Gain"))
            .and_then(parse_number)
            .unwrap_or(0.0);
        let gain_scale = find("Gain Scale")
            .and_then(parse_number)
            .map_or(1.0, |percent| percent / 100.0);
        // The first Pro-Q counts its bands instead of flagging each one.
        let active_bands = if version == 1 {
            find("Num Active Bands")
                .and_then(parse_number)
                .map(|count| count.max(0.0) as usize)
        } else {
            None
        };

        let mut bands = Vec::new();
        for n in 1..=24 {
            let band = |what: &str| find(&format!("Band {n} {what}"));
            // No such band: the list is done. A band whose text Studio has
            // just cleared (it set the value itself) waits for the re-read.
            let Some(freq_text) = band("Frequency") else {
                break;
            };
            let Some(freq) = parse_number(freq_text) else {
                continue;
            };
            let used = if let Some(used) = band("Used") {
                !used.eq_ignore_ascii_case("unused")
            } else if let Some(state) = band("State") {
                state.eq_ignore_ascii_case("enabled")
            } else {
                active_bands.is_some_and(|count| n <= count)
            };
            let enabled = band("Enabled").is_none_or(|text| !text.eq_ignore_ascii_case("disabled"));
            if !used || !enabled {
                continue;
            }
            let Some(shape) = band("Shape").and_then(Shape::parse) else {
                continue;
            };
            let gain_db = if shape.has_gain() {
                band("Gain").and_then(parse_number).unwrap_or(0.0) * gain_scale
            } else {
                0.0
            };
            let slope = band("Slope")
                .or_else(|| band("LP/HP Slope"))
                .map_or(12.0, parse_slope);
            bands.push(Band {
                shape,
                freq,
                gain_db,
                q: band("Q").and_then(parse_number).unwrap_or(1.0),
                slope,
            });
        }
        Some(Self {
            bands,
            output_db,
            bypassed,
        })
    }

    /// Adds this Pro-Q's curve to `db`; `false` when it is bypassed.
    fn add_to(&self, db: &mut [f32]) -> bool {
        if self.bypassed {
            return false;
        }
        for (index, level) in db.iter_mut().enumerate() {
            let hz = point_freq(index);
            *level += self.output_db
                + self
                    .bands
                    .iter()
                    .map(|band| band.response_db(hz))
                    .sum::<f32>();
        }
        true
    }
}

/// Whether a bypass parameter's text says bypassed: "Bypassed", not "Not
/// Bypassed".
fn says_bypassed(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("bypass") && !text.contains("not")
}

/// The first number in `text`, scaled by a "k" straight after it: "1.00 kHz"
/// is 1000, "-0.22 dB" is -0.22, "100.0%" is 100.
fn parse_number(text: &str) -> Option<f32> {
    let text = text.replace('\u{2212}', "-");
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|byte| {
        byte.is_ascii_digit() || *byte == b'-' || *byte == b'+' || *byte == b'.'
    })?;
    let mut end = start + 1;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
        end += 1;
    }
    let value: f32 = text[start..end].parse().ok()?;
    let unit = text[end..].trim_start();
    let scale = if unit.starts_with('k') || unit.starts_with('K') {
        1_000.0
    } else {
        1.0
    };
    Some(value * scale)
}

/// A cut slope's text in dB/oct: "24 dB/oct"; "Brickwall" as the steepest
/// slope drawn.
fn parse_slope(text: &str) -> f32 {
    if text.to_ascii_lowercase().contains("brick") {
        return 120.0;
    }
    parse_number(text).unwrap_or(12.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(id: u32, name: &str, text: &str) -> PluginParameterState {
        PluginParameterState {
            id,
            name: name.to_string(),
            value_normalized: 0.5,
            automatable: true,
            hidden: false,
            read_only: false,
            unit: String::new(),
            value_text: text.to_string(),
        }
    }

    fn pro_q_slot(name: &str, parameters: Vec<PluginParameterState>) -> InsertSlotState {
        let mut slot = InsertSlotState::empty("pq".to_string());
        slot.plugin_id = Some("ABCDEF0123456789ABCDEF0123456789".to_string());
        slot.plugin_format = Some(InsertPluginFormat::Vst3);
        slot.display_name = name.to_string();
        slot.enabled = true;
        slot.load_status = InsertLoadStatus::Ready;
        slot.parameters = parameters;
        slot
    }

    fn db_at(eq: &ChannelEq, hz: f32) -> f32 {
        let index = (0..CURVE_POINTS)
            .min_by(|a, b| {
                (point_freq(*a) / hz)
                    .ln()
                    .abs()
                    .total_cmp(&(point_freq(*b) / hz).ln().abs())
            })
            .unwrap();
        eq.db[index]
    }

    /// Band 1 of a Pro-Q 3 or 4 as its host reports it.
    fn pro_q3_band(
        used: &str,
        freq: &str,
        gain: &str,
        q: &str,
        shape: &str,
        slope: &str,
    ) -> Vec<PluginParameterState> {
        vec![
            param(0, "Band 1 Used", used),
            param(1, "Band 1 Enabled", "Enabled"),
            param(2, "Band 1 Frequency", freq),
            param(3, "Band 1 Gain", gain),
            param(7, "Band 1 Q", q),
            param(8, "Band 1 Shape", shape),
            param(9, "Band 1 Slope", slope),
            param(315, "Output Level", "0.00 dB"),
            param(318, "Bypass", "Not Bypassed"),
            param(314, "Gain Scale", "100.0%"),
        ]
    }

    #[test]
    fn pro_q_versions_are_told_apart_by_name() {
        assert_eq!(pro_q_version("FabFilter Pro-Q"), Some(1));
        assert_eq!(pro_q_version("FabFilter Pro-Q 2"), Some(2));
        assert_eq!(pro_q_version("FabFilter Pro-Q 3"), Some(3));
        assert_eq!(pro_q_version("Pro-Q 4"), Some(4));
        assert_eq!(pro_q_version("FabFilter Pro-C 2"), None);
    }

    #[test]
    fn the_hosts_value_text_parses() {
        assert_eq!(parse_number("1000.0 Hz"), Some(1000.0));
        assert_eq!(parse_number("1.00 kHz"), Some(1000.0));
        assert_eq!(parse_number("-0.22 dB"), Some(-0.22));
        assert_eq!(parse_number("+3.00 dB"), Some(3.0));
        assert_eq!(parse_number("100.0%"), Some(100.0));
        assert_eq!(parse_number("Bell"), None);
        assert_eq!(parse_slope("48 dB/oct"), 48.0);
        assert_eq!(parse_slope("Brickwall"), 120.0);
        assert!(says_bypassed("Bypassed"));
        assert!(!says_bypassed("Not Bypassed"));
    }

    #[test]
    fn a_pro_q_bell_peaks_at_its_frequency() {
        let slot = pro_q_slot(
            "FabFilter Pro-Q 3",
            pro_q3_band("Used", "1000.0 Hz", "6.00 dB", "1.000", "Bell", "12 dB/oct"),
        );
        let eq = channel_eq(&[slot]).expect("a Pro-Q is an EQ");
        assert!(eq.live && !eq.pending);
        assert!(
            (db_at(&eq, 1_000.0) - 6.0).abs() < 0.3,
            "{}",
            db_at(&eq, 1_000.0)
        );
        assert!(db_at(&eq, 50.0).abs() < 0.3);
        assert!(db_at(&eq, 15_000.0).abs() < 0.3);
    }

    #[test]
    fn an_unused_pro_q_band_draws_nothing() {
        let slot = pro_q_slot(
            "FabFilter Pro-Q 4",
            pro_q3_band(
                "Unused",
                "200.0 Hz",
                "0.00 dB",
                "1.000",
                "Low Cut",
                "48 dB/oct",
            ),
        );
        let eq = channel_eq(&[slot]).unwrap();
        assert!(eq.db.iter().all(|db| db.abs() < 1e-6));
    }

    #[test]
    fn a_pro_q_low_cut_falls_at_its_slope() {
        let slot = pro_q_slot(
            "FabFilter Pro-Q 3",
            pro_q3_band(
                "Used",
                "200.00 Hz",
                "0.00 dB",
                "0.707",
                "Low Cut",
                "24 dB/oct",
            ),
        );
        let eq = channel_eq(&[slot]).unwrap();
        // Two octaves under the cutoff a 24 dB/oct cut is down about 48 dB.
        assert!(
            (db_at(&eq, 50.0) + 48.0).abs() < 3.0,
            "{}",
            db_at(&eq, 50.0)
        );
        assert!(db_at(&eq, 5_000.0).abs() < 0.3);
    }

    #[test]
    fn a_bypassed_pro_q_is_shown_flat() {
        let mut parameters =
            pro_q3_band("Used", "1000.0 Hz", "6.00 dB", "1.000", "Bell", "12 dB/oct");
        parameters.retain(|param| param.name != "Bypass");
        parameters.push(param(318, "Bypass", "Bypassed"));
        let eq = channel_eq(&[pro_q_slot("FabFilter Pro-Q 3", parameters)]).unwrap();
        assert!(!eq.live);
        assert!(eq.db.iter().all(|db| db.abs() < 1e-6));
    }

    #[test]
    fn a_pro_q_without_values_yet_is_pending() {
        let mut parameters =
            pro_q3_band("Used", "1000.0 Hz", "6.00 dB", "1.000", "Bell", "12 dB/oct");
        for param in &mut parameters {
            param.value_text.clear();
        }
        let eq = channel_eq(&[pro_q_slot("FabFilter Pro-Q 3", parameters)]).unwrap();
        assert!(eq.pending && !eq.live);
    }

    #[test]
    fn the_first_pro_q_counts_its_active_bands() {
        let parameters = vec![
            param(0, "Num Active Bands", "1"),
            param(1, "Band 1 Frequency", "1000.0 Hz"),
            param(2, "Band 1 Gain", "-6.00 dB"),
            param(3, "Band 1 Q", "1.000"),
            param(4, "Band 1 Shape", "Bell"),
            param(5, "Band 1 LP/HP Slope", "24 dB/oct"),
            param(7, "Band 1 Enabled", "Enabled"),
            param(8, "Band 2 Frequency", "100.0 Hz"),
            param(9, "Band 2 Gain", "12.00 dB"),
            param(10, "Band 2 Q", "1.000"),
            param(11, "Band 2 Shape", "Bell"),
            param(14, "Band 2 Enabled", "Enabled"),
            param(169, "Output Gain", "0.00 dB"),
        ];
        let eq = channel_eq(&[pro_q_slot("FabFilter Pro-Q", parameters)]).unwrap();
        assert!((db_at(&eq, 1_000.0) + 6.0).abs() < 0.3);
        assert!(
            db_at(&eq, 100.0).abs() < 1.0,
            "band 2 is past the active count"
        );
    }

    #[test]
    fn a_chain_without_an_eq_has_no_curve() {
        let mut slot = pro_q_slot("FabFilter Pro-C 2", Vec::new());
        slot.plugin_path = None;
        assert!(channel_eq(&[slot]).is_none());
    }

    #[test]
    fn the_first_eq_insert_is_the_one_a_click_opens() {
        let other = {
            let mut slot = pro_q_slot("FabFilter Pro-C 2", Vec::new());
            slot.id = "comp".to_string();
            slot
        };
        let eq = pro_q_slot(
            "FabFilter Pro-Q 3",
            pro_q3_band("Used", "1000.0 Hz", "6.00 dB", "1.000", "Bell", "12 dB/oct"),
        );
        let channel = channel_eq(&[other, eq]).unwrap();
        assert_eq!(channel.first, (1, "pq".to_string()));
    }

    #[test]
    fn the_built_in_eqs_are_found_by_either_id() {
        let mut slot = InsertSlotState::empty("eq".to_string());
        for (id, kind) in [
            ("equz8", EqKind::Equz8),
            ("builtin:equz8", EqKind::Equz8),
            ("equzx", EqKind::Equzx),
            ("builtin:equzx", EqKind::Equzx),
        ] {
            slot.plugin_id = Some(id.to_string());
            assert_eq!(EqKind::of(&slot), Some(kind), "{id}");
        }
        slot.plugin_id = Some("zapp".to_string());
        assert_eq!(EqKind::of(&slot), None);
    }

    #[cfg(feature = "builtin-plugin-editor")]
    #[test]
    fn a_fresh_equz8_draws_its_defaults_flat() {
        let mut slot = InsertSlotState::empty("eq8-fresh-mixer-test".to_string());
        slot.plugin_id = Some(EQUZ8_STEM.to_string());
        slot.display_name = "Equz8".to_string();
        slot.enabled = true;
        slot.load_status = InsertLoadStatus::Ready;
        let eq = channel_eq(&[slot]).unwrap();
        assert!(eq.live);
        assert!(eq.db.iter().all(|db| db.abs() < 0.01), "{:?}", eq.db);
    }
}
