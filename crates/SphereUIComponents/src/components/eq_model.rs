//! The two built-in EQs seen as one, for their shared native editor.
//!
//! EQ-Z8 (eight fixed bands, a mix control) and EQ-ZX (24 slots, cut slopes,
//! mid/side placement, dynamics that engage above or below the threshold)
//! keep their own parameter tables, wire indices and saved state; this module
//! only reads and writes them through one band shape, [`Band`], so the graph,
//! the band strip and the band editor are written once.
//!
//! Every edit still leaves as the plug-in's own wire edits ([`EqParams::wire_diff`]),
//! computed generically from each crate's `ui_values` table, so the state
//! mirror, the project and the host DSP see exactly what the CEF editor used
//! to send. Every curve comes from the DSP crates' own response functions,
//! which build the same coefficients the audio path runs.
//!
//! Pure data — no GPUI — so all of it is unit-tested here.

use equzx::BandChannel;

/// The ranges both DSPs clamp to. EQ-Z8 hard-codes the same numbers; the
/// test below holds the two together.
pub const FREQ_MIN: f32 = equzx::ipc::FREQ_MIN;
pub const FREQ_MAX: f32 = equzx::ipc::FREQ_MAX;
pub const GAIN_MIN_DB: f32 = equzx::ipc::GAIN_MIN_DB;
pub const GAIN_MAX_DB: f32 = equzx::ipc::GAIN_MAX_DB;
pub const Q_MIN: f32 = equzx::ipc::Q_MIN;
pub const Q_MAX: f32 = equzx::ipc::Q_MAX;
pub const THRESHOLD_MIN_DB: f32 = equzx::ipc::THRESHOLD_MIN_DB;
pub const THRESHOLD_MAX_DB: f32 = equzx::ipc::THRESHOLD_MAX_DB;
pub const RANGE_MIN_DB: f32 = equzx::ipc::RANGE_MIN_DB;
pub const RANGE_MAX_DB: f32 = equzx::ipc::RANGE_MAX_DB;
pub const ATTACK_MIN_MS: f32 = equzx::ipc::ATTACK_MIN_MS;
pub const ATTACK_MAX_MS: f32 = equzx::ipc::ATTACK_MAX_MS;
pub const RELEASE_MIN_MS: f32 = equzx::ipc::RELEASE_MIN_MS;
pub const RELEASE_MAX_MS: f32 = equzx::ipc::RELEASE_MAX_MS;
pub const OUTPUT_MIN_DB: f32 = equzx::ipc::OUTPUT_MIN_DB;
pub const OUTPUT_MAX_DB: f32 = equzx::ipc::OUTPUT_MAX_DB;
pub const SLOPES: [f32; 6] = equzx::ipc::SLOPES;
/// The slope an EQ-Z8 cut has: one second-order section.
const Z8_SLOPE: f32 = 12.0;

/// Which of the two EQs an editor is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqKind {
    Z8,
    Zx,
}

impl EqKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Z8 => "EQ-Z8",
            Self::Zx => "EQ-ZX",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Z8 => "8-band dynamic EQ",
            Self::Zx => "24-band dynamic mid/side EQ",
        }
    }

    /// Band slots the plug-in has.
    pub fn slots(self) -> usize {
        match self {
            Self::Z8 => equz8::BAND_COUNT,
            Self::Zx => equzx::BAND_COUNT,
        }
    }

    /// The shapes a band can take, in the order the editor lists them.
    pub fn shapes(self) -> &'static [Shape] {
        match self {
            Self::Z8 => &[
                Shape::LowCut,
                Shape::LowShelf,
                Shape::Bell,
                Shape::Notch,
                Shape::HighShelf,
                Shape::HighCut,
            ],
            Self::Zx => &[
                Shape::LowCut,
                Shape::LowShelf,
                Shape::Bell,
                Shape::Notch,
                Shape::BandPass,
                Shape::HighShelf,
                Shape::HighCut,
            ],
        }
    }

    /// EQ-ZX bands can sit on the mid or side alone, and its cuts have a
    /// selectable slope; EQ-Z8's cannot.
    pub fn has_placement(self) -> bool {
        self == Self::Zx
    }

    pub fn has_slopes(self) -> bool {
        self == Self::Zx
    }

    /// EQ-ZX dynamics can engage below the threshold as well as above.
    pub fn has_dyn_mode(self) -> bool {
        self == Self::Zx
    }

    pub fn has_mix(self) -> bool {
        self == Self::Z8
    }

    /// EQ-Z8's eight bands are always there, each switched on or off, and any
    /// edit to a switched-off band switches it on — moving a band you cannot
    /// hear would be an edit with no result. EQ-ZX bands are created and
    /// removed, and an edit to a band switched off leaves it off.
    pub fn edit_switches_on(self) -> bool {
        self == Self::Z8
    }
}

/// A band's filter shape, across both EQs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    LowCut,
    LowShelf,
    Bell,
    Notch,
    BandPass,
    HighShelf,
    HighCut,
}

impl Shape {
    pub fn label(self) -> &'static str {
        match self {
            Self::LowCut => "Low Cut",
            Self::LowShelf => "Low Shelf",
            Self::Bell => "Bell",
            Self::Notch => "Notch",
            Self::BandPass => "Band Pass",
            Self::HighShelf => "High Shelf",
            Self::HighCut => "High Cut",
        }
    }

    /// The two-letter name a band strip cell shows.
    pub fn short(self) -> &'static str {
        match self {
            Self::LowCut => "LC",
            Self::LowShelf => "LS",
            Self::Bell => "BELL",
            Self::Notch => "NOTCH",
            Self::BandPass => "BP",
            Self::HighShelf => "HS",
            Self::HighCut => "HC",
        }
    }

    pub fn has_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Bell | Self::HighShelf)
    }

    pub fn is_cut(self) -> bool {
        matches!(self, Self::LowCut | Self::HighCut)
    }

    /// Whether Q shapes this band in `kind`. EQ-Z8's single-section cuts and
    /// shelves take their Q as resonance; EQ-ZX's cuts are Butterworth (the
    /// slope sets them) and its shelves are designed with the Q too.
    pub fn uses_q(self, kind: EqKind) -> bool {
        match kind {
            EqKind::Z8 => true,
            EqKind::Zx => !self.is_cut(),
        }
    }

    fn to_z8(self) -> equz8::BandType {
        match self {
            Self::LowCut => equz8::BandType::HighPass,
            Self::LowShelf => equz8::BandType::LowShelf,
            Self::Notch => equz8::BandType::Notch,
            Self::HighShelf => equz8::BandType::HighShelf,
            Self::HighCut => equz8::BandType::LowPass,
            Self::Bell | Self::BandPass => equz8::BandType::Bell,
        }
    }

    fn from_z8(kind: equz8::BandType) -> Self {
        match kind {
            equz8::BandType::HighPass => Self::LowCut,
            equz8::BandType::LowShelf => Self::LowShelf,
            equz8::BandType::Bell => Self::Bell,
            equz8::BandType::Notch => Self::Notch,
            equz8::BandType::HighShelf => Self::HighShelf,
            equz8::BandType::LowPass => Self::HighCut,
        }
    }

    fn to_zx(self) -> equzx::BandType {
        match self {
            Self::LowCut => equzx::BandType::LowCut,
            Self::LowShelf => equzx::BandType::LowShelf,
            Self::Bell => equzx::BandType::Bell,
            Self::Notch => equzx::BandType::Notch,
            Self::BandPass => equzx::BandType::BandPass,
            Self::HighShelf => equzx::BandType::HighShelf,
            Self::HighCut => equzx::BandType::HighCut,
        }
    }

    fn from_zx(kind: equzx::BandType) -> Self {
        match kind {
            equzx::BandType::LowCut => Self::LowCut,
            equzx::BandType::LowShelf => Self::LowShelf,
            equzx::BandType::Bell => Self::Bell,
            equzx::BandType::Notch => Self::Notch,
            equzx::BandType::BandPass => Self::BandPass,
            equzx::BandType::HighShelf => Self::HighShelf,
            equzx::BandType::HighCut => Self::HighCut,
        }
    }
}

/// The part of the stereo image a band acts on, or a view shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Stereo,
    Mid,
    Side,
}

impl Placement {
    pub const ALL: [Placement; 3] = [Self::Stereo, Self::Mid, Self::Side];

    pub fn label(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo",
            Self::Mid => "Mid",
            Self::Side => "Side",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Stereo => "ST",
            Self::Mid => "M",
            Self::Side => "S",
        }
    }

    fn to_zx(self) -> BandChannel {
        match self {
            Self::Stereo => BandChannel::Stereo,
            Self::Mid => BandChannel::Mid,
            Self::Side => BandChannel::Side,
        }
    }

    fn from_zx(channel: BandChannel) -> Self {
        match channel {
            BandChannel::Stereo => Self::Stereo,
            BandChannel::Mid => Self::Mid,
            BandChannel::Side => Self::Side,
        }
    }

    /// Whether a band placed here is heard in a view of `view`.
    pub fn heard_in(self, view: Placement) -> bool {
        match view {
            Placement::Stereo => true,
            _ => self == Placement::Stereo || self == view,
        }
    }
}

/// One band, whichever EQ it belongs to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub active: bool,
    pub shape: Shape,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
    /// dB/oct; meaningful for an EQ-ZX cut only.
    pub slope: f32,
    pub placement: Placement,
    pub dynamic: bool,
    /// Engages as the level falls below the threshold (EQ-ZX only).
    pub dyn_below: bool,
    pub threshold_db: f32,
    pub range_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Band {
    /// Whether its dynamics can run: switched on, on a shape with gain.
    pub fn dynamics_live(&self) -> bool {
        self.dynamic && self.shape.has_gain()
    }

    /// The gain the node sits at: its own on a shape with gain, 0 dB on the
    /// rest, which have none.
    pub fn node_gain_db(&self) -> f32 {
        if self.shape.has_gain() {
            self.gain_db
        } else {
            0.0
        }
    }

    fn from_z8(band: &equz8::BandParams) -> Self {
        Self {
            active: band.active,
            shape: Shape::from_z8(band.band_type),
            freq: band.freq,
            gain_db: band.gain_db,
            q: band.q,
            slope: Z8_SLOPE,
            placement: Placement::Stereo,
            dynamic: band.dynamic,
            dyn_below: false,
            threshold_db: band.threshold_db,
            range_db: band.range_db,
            attack_ms: band.attack_ms,
            release_ms: band.release_ms,
        }
    }

    fn write_z8(&self, band: &mut equz8::BandParams) {
        band.active = self.active;
        band.band_type = self.shape.to_z8();
        band.freq = self.freq;
        band.gain_db = self.gain_db;
        band.q = self.q;
        band.dynamic = self.dynamic;
        band.threshold_db = self.threshold_db;
        band.range_db = self.range_db;
        band.attack_ms = self.attack_ms;
        band.release_ms = self.release_ms;
    }

    fn from_zx(band: &equzx::BandParams) -> Self {
        Self {
            active: band.active,
            shape: Shape::from_zx(band.band_type),
            freq: band.freq,
            gain_db: band.gain_db,
            q: band.q,
            slope: band.slope,
            placement: Placement::from_zx(band.channel),
            dynamic: band.dynamic,
            dyn_below: band.dyn_mode == equzx::DynMode::Below,
            threshold_db: band.threshold_db,
            range_db: band.range_db,
            attack_ms: band.attack_ms,
            release_ms: band.release_ms,
        }
    }

    fn write_zx(&self, band: &mut equzx::BandParams) {
        band.active = self.active;
        band.band_type = self.shape.to_zx();
        band.channel = self.placement.to_zx();
        band.freq = self.freq;
        band.gain_db = self.gain_db;
        band.q = self.q;
        band.slope = self.slope;
        band.dynamic = self.dynamic;
        band.dyn_mode = if self.dyn_below {
            equzx::DynMode::Below
        } else {
            equzx::DynMode::Above
        };
        band.threshold_db = self.threshold_db;
        band.range_db = self.range_db;
        band.attack_ms = self.attack_ms;
        band.release_ms = self.release_ms;
    }
}

/// One EQ's whole parameter set.
#[derive(Clone, Debug)]
pub enum EqParams {
    Z8(equz8::Params),
    Zx(equzx::Params),
}

impl EqParams {
    pub fn defaults(kind: EqKind) -> Self {
        match kind {
            EqKind::Z8 => Self::Z8(equz8::default_params()),
            EqKind::Zx => Self::Zx(equzx::default_params()),
        }
    }

    pub fn kind(&self) -> EqKind {
        match self {
            Self::Z8(_) => EqKind::Z8,
            Self::Zx(_) => EqKind::Zx,
        }
    }

    pub fn power(&self) -> bool {
        match self {
            Self::Z8(p) => p.power,
            Self::Zx(p) => p.power,
        }
    }

    pub fn set_power(&mut self, on: bool) {
        match self {
            Self::Z8(p) => p.power = on,
            Self::Zx(p) => p.power = on,
        }
    }

    pub fn output_db(&self) -> f32 {
        match self {
            Self::Z8(p) => p.output_db,
            Self::Zx(p) => p.output_db,
        }
    }

    pub fn set_output_db(&mut self, db: f32) {
        match self {
            Self::Z8(p) => p.output_db = db,
            Self::Zx(p) => p.output_db = db,
        }
        self.sanitize();
    }

    /// The dry/wet mix (0..100), on the EQ that has one.
    pub fn mix(&self) -> Option<f32> {
        match self {
            Self::Z8(p) => Some(p.mix),
            Self::Zx(_) => None,
        }
    }

    pub fn set_mix(&mut self, mix: f32) {
        if let Self::Z8(p) = self {
            p.mix = mix;
        }
        self.sanitize();
    }

    /// The band being auditioned alone.
    pub fn solo(&self) -> Option<usize> {
        let index = match self {
            Self::Z8(p) => p.solo_band,
            Self::Zx(p) => p.solo_band,
        };
        usize::try_from(index)
            .ok()
            .filter(|i| *i < self.kind().slots())
    }

    pub fn set_solo(&mut self, band: Option<usize>) {
        let index = band
            .filter(|i| *i < self.kind().slots())
            .map_or(-1, |i| i as i32);
        match self {
            Self::Z8(p) => p.solo_band = index,
            Self::Zx(p) => p.solo_band = index,
        }
    }

    pub fn band(&self, index: usize) -> Band {
        match self {
            Self::Z8(p) => Band::from_z8(&p.bands[index.min(equz8::BAND_COUNT - 1)]),
            Self::Zx(p) => Band::from_zx(&p.bands[index.min(equzx::BAND_COUNT - 1)]),
        }
    }

    /// Writes `band` into slot `index`, then brings every value back into the
    /// range the DSP accepts — so the editor never shows a value the plug-in
    /// would quietly pin.
    pub fn set_band(&mut self, index: usize, band: Band) {
        match self {
            Self::Z8(p) => {
                if let Some(slot) = p.bands.get_mut(index) {
                    band.write_z8(slot);
                }
            }
            Self::Zx(p) => {
                if let Some(slot) = p.bands.get_mut(index) {
                    band.write_zx(slot);
                }
            }
        }
        self.sanitize();
    }

    fn sanitize(&mut self) {
        match self {
            Self::Z8(p) => equz8::ipc::sanitize_params(p),
            Self::Zx(p) => equzx::ipc::sanitize_params(p),
        }
    }

    /// Every wire parameter's value, by id.
    pub fn ui_values(&self) -> Vec<(&'static str, f32)> {
        match self {
            Self::Z8(p) => equz8::ipc::ui_values(p),
            Self::Zx(p) => equzx::ipc::ui_values(p),
        }
    }

    fn wire_index(&self, id: &str) -> Option<u32> {
        match self {
            Self::Z8(_) => equz8::ui_param_index(id),
            Self::Zx(_) => equzx::ui_param_index(id),
        }
    }

    /// `next` as wire edits against `self`: every wire id whose value
    /// differs, with its wire index. Generic over the whole table, so a
    /// parameter added to either plug-in needs no change here.
    pub fn wire_diff(&self, next: &EqParams) -> Vec<(u32, f32)> {
        if self.kind() != next.kind() {
            return Vec::new();
        }
        let before: std::collections::HashMap<&str, f32> = self.ui_values().into_iter().collect();
        next.ui_values()
            .into_iter()
            .filter(|(id, value)| before.get(id) != Some(value))
            .filter_map(|(id, value)| self.wire_index(id).map(|index| (index, value)))
            .collect()
    }

    /// Whether the two sound the same: every value but the power switch and
    /// the solo, which are listening state, not the sound.
    pub fn same_sound(&self, other: &EqParams) -> bool {
        let sound = |p: &EqParams| -> Vec<(&'static str, f32)> {
            p.ui_values()
                .into_iter()
                .filter(|(id, _)| *id != "power" && *id != "soloBand")
                .collect()
        };
        self.kind() == other.kind() && sound(self) == sound(other)
    }

    /// Whether any switched-on band acts on the mid or side alone.
    pub fn uses_mid_side(&self) -> bool {
        match self {
            Self::Z8(_) => false,
            Self::Zx(p) => equzx::uses_mid_side(p),
        }
    }

    /// The whole EQ's response at `hz`, in dB, for the part of the image
    /// `view` shows, output gain included.
    pub fn response_db(&self, view: Placement, hz: f32, sample_rate: f32) -> f32 {
        match self {
            Self::Z8(p) => equz8::response_db(p, hz, sample_rate),
            Self::Zx(p) => equzx::response_db(p, view.to_zx(), hz, sample_rate),
        }
    }

    /// Band `index`'s response at `hz` with its gain at `gain_db`.
    pub fn band_response_db(&self, index: usize, gain_db: f32, hz: f32, sample_rate: f32) -> f32 {
        match self {
            Self::Z8(p) => p.bands.get(index).map_or(0.0, |band| {
                equz8::band_response_at_gain_db(band, gain_db, hz, sample_rate)
            }),
            Self::Zx(p) => p.bands.get(index).map_or(0.0, |band| {
                equzx::band_response_at_gain_db(band, gain_db, hz, sample_rate)
            }),
        }
    }

    /// Whether slot `index` holds a band the editor lists. Every EQ-Z8 band
    /// does. An EQ-ZX slot does once it is switched on or set to anything but
    /// an empty slot's values — so a band switched off keeps its place, and a
    /// slot never touched stays free.
    pub fn in_use(&self, index: usize) -> bool {
        match self {
            Self::Z8(_) => index < equz8::BAND_COUNT,
            Self::Zx(_) => {
                let band = self.band(index);
                band.active || band != empty_band(EqKind::Zx)
            }
        }
    }

    /// The slots the editor lists, in slot order.
    pub fn listed(&self) -> Vec<usize> {
        (0..self.kind().slots())
            .filter(|index| self.in_use(*index))
            .collect()
    }

    /// The slot a new band takes: EQ-Z8's first switched-off band, EQ-ZX's
    /// first free slot.
    pub fn free_slot(&self) -> Option<usize> {
        match self {
            Self::Z8(_) => (0..equz8::BAND_COUNT).find(|index| !self.band(*index).active),
            Self::Zx(_) => (0..equzx::BAND_COUNT).find(|index| !self.in_use(*index)),
        }
    }
}

/// What an empty slot holds: EQ-ZX's unused band, and what removing a band
/// leaves behind.
pub fn empty_band(kind: EqKind) -> Band {
    match kind {
        EqKind::Z8 => Band::from_z8(&equz8::default_params().bands[0]),
        EqKind::Zx => Band::from_zx(&equzx::default_params().bands[0]),
    }
}

/// Slot `index`'s default values: a double-click on a control resets to
/// these.
pub fn default_band(kind: EqKind, index: usize) -> Band {
    EqParams::defaults(kind).band(index)
}

/// A preset the editor can load.
#[derive(Clone, Debug)]
pub struct Preset {
    pub name: &'static str,
    pub params: EqParams,
}

/// `kind`'s presets, the neutral state first: EQ-Z8's factory bank; for
/// EQ-ZX, which ships none, just the flat starting point.
pub fn presets(kind: EqKind) -> Vec<Preset> {
    match kind {
        EqKind::Z8 => equz8::factory_presets()
            .iter()
            .map(|preset| Preset {
                name: preset.name,
                params: EqParams::Z8(preset.params.clone()),
            })
            .collect(),
        EqKind::Zx => vec![Preset {
            name: "Flat",
            params: EqParams::defaults(EqKind::Zx),
        }],
    }
}

/// `preset` as loaded over `current`: its sound, with the insert's power
/// switch kept and any audition ended.
pub fn preset_applied(current: &EqParams, preset: &EqParams) -> EqParams {
    let mut next = preset.clone();
    next.set_power(current.power());
    next.set_solo(None);
    next
}

/// The preset `params` sounds like, if any.
pub fn matching_preset(params: &EqParams) -> Option<usize> {
    presets(params.kind())
        .iter()
        .position(|preset| preset.params.same_sound(params))
}

/// A new band where the user asked for one: a bell at `freq` and `gain_db`,
/// placed where the editor's view is, everything else as an empty slot.
pub fn new_band(kind: EqKind, freq: f32, gain_db: f32, view: Placement) -> Band {
    Band {
        active: true,
        shape: Shape::Bell,
        freq: freq.clamp(FREQ_MIN, FREQ_MAX),
        gain_db: gain_db.clamp(GAIN_MIN_DB, GAIN_MAX_DB),
        q: 1.0,
        dynamic: false,
        placement: if kind.has_placement() {
            view
        } else {
            Placement::Stereo
        },
        ..empty_band(kind)
    }
}

/// The next slope a wheel step reaches from `slope`: `steeper` up the list,
/// otherwise down it, stopping at either end.
pub fn step_slope(slope: f32, steeper: bool) -> f32 {
    let index = SLOPES
        .iter()
        .position(|candidate| (*candidate - slope).abs() < 0.5)
        .unwrap_or(1);
    let next = if steeper {
        (index + 1).min(SLOPES.len() - 1)
    } else {
        index.saturating_sub(1)
    };
    SLOPES[next]
}

/// Frequency as a band shows it: `1.25k`, `16.5k`, `440`.
pub fn format_freq(hz: f32) -> String {
    if hz >= 10_000.0 {
        format!("{:.1}", hz / 1_000.0)
            .trim_end_matches(".0")
            .to_string()
            + "k"
    } else if hz >= 1_000.0 {
        let text = format!("{:.2}", hz / 1_000.0);
        text.trim_end_matches('0').trim_end_matches('.').to_string() + "k"
    } else {
        format!("{}", hz.round() as i32)
    }
}

/// A signed decibel value: `+3.5`, `0`, `-12.0`.
pub fn format_db(db: f32) -> String {
    if db.abs() < 0.05 {
        "0".to_string()
    } else if db > 0.0 {
        format!("+{db:.1}")
    } else {
        format!("{db:.1}")
    }
}

pub fn format_ms(ms: f32) -> String {
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else if ms < 1_000.0 {
        format!("{} ms", ms.round() as i32)
    } else {
        format!("{:.2} s", ms / 1_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_dsps_clamp_to_the_ranges_the_editor_uses() {
        let mut p = equz8::default_params();
        p.output_db = 99.0;
        p.bands[0].freq = 1.0;
        p.bands[1].gain_db = 99.0;
        p.bands[2].q = 99.0;
        p.bands[3].threshold_db = -99.0;
        p.bands[4].range_db = 99.0;
        p.bands[5].attack_ms = 0.0;
        p.bands[6].release_ms = 99_999.0;
        equz8::ipc::sanitize_params(&mut p);
        assert_eq!(p.output_db, OUTPUT_MAX_DB);
        assert_eq!(p.bands[0].freq, FREQ_MIN);
        assert_eq!(p.bands[1].gain_db, GAIN_MAX_DB);
        assert_eq!(p.bands[2].q, Q_MAX);
        assert_eq!(p.bands[3].threshold_db, THRESHOLD_MIN_DB);
        assert_eq!(p.bands[4].range_db, RANGE_MAX_DB);
        assert_eq!(p.bands[5].attack_ms, ATTACK_MIN_MS);
        assert_eq!(p.bands[6].release_ms, RELEASE_MAX_MS);
    }

    #[test]
    fn every_band_field_round_trips_through_both_plugins() {
        for kind in [EqKind::Z8, EqKind::Zx] {
            let mut params = EqParams::defaults(kind);
            let band = Band {
                active: true,
                shape: Shape::LowShelf,
                freq: 321.0,
                gain_db: -4.5,
                q: 2.25,
                slope: if kind.has_slopes() { 48.0 } else { Z8_SLOPE },
                placement: if kind.has_placement() {
                    Placement::Side
                } else {
                    Placement::Stereo
                },
                dynamic: true,
                dyn_below: kind.has_dyn_mode(),
                threshold_db: -30.0,
                range_db: -6.0,
                attack_ms: 4.0,
                release_ms: 250.0,
            };
            params.set_band(2, band);
            assert_eq!(params.band(2), band, "{kind:?}");
        }
    }

    #[test]
    fn a_wire_diff_carries_each_edit_and_replays_to_the_same_params() {
        for kind in [EqKind::Z8, EqKind::Zx] {
            let before = EqParams::defaults(kind);
            let mut after = before.clone();
            let mut band = after.band(1);
            band.active = true;
            band.gain_db = 3.0;
            after.set_band(1, band);
            after.set_output_db(-2.0);
            after.set_solo(Some(1));
            let diff = before.wire_diff(&after);
            assert_eq!(diff.len(), 4, "{kind:?}: {diff:?}");
            // Replayed into a copy through each plug-in's own wire, the edit
            // arrives whole.
            let mut replayed = before.clone();
            for (index, value) in diff {
                match &mut replayed {
                    EqParams::Z8(p) => assert!(equz8::ipc::apply_wire_param(p, index, value)),
                    EqParams::Zx(p) => assert!(equzx::ipc::apply_wire_param(p, index, value)),
                }
            }
            assert_eq!(replayed.ui_values(), after.ui_values());
        }
    }

    #[test]
    fn a_shape_without_gain_drops_dynamics() {
        for kind in [EqKind::Z8, EqKind::Zx] {
            let mut params = EqParams::defaults(kind);
            let mut band = params.band(0);
            band.shape = Shape::Bell;
            band.dynamic = true;
            params.set_band(0, band);
            assert!(params.band(0).dynamic);
            band.shape = Shape::Notch;
            params.set_band(0, band);
            assert!(!params.band(0).dynamic, "{kind:?}");
        }
    }

    #[test]
    fn eq_zx_lists_used_slots_and_fills_the_first_free_one() {
        let mut params = EqParams::defaults(EqKind::Zx);
        assert!(params.listed().is_empty());
        assert_eq!(params.free_slot(), Some(0));
        params.set_band(0, new_band(EqKind::Zx, 500.0, 3.0, Placement::Mid));
        assert_eq!(params.band(0).placement, Placement::Mid);
        // Switched off, it keeps its place in the list.
        let mut band = params.band(0);
        band.active = false;
        params.set_band(0, band);
        assert_eq!(params.listed(), vec![0]);
        assert_eq!(params.free_slot(), Some(1));
        // Removed, the slot is free again.
        params.set_band(0, empty_band(EqKind::Zx));
        assert!(params.listed().is_empty());
        // EQ-Z8 always lists its eight and fills the first switched off.
        let z8 = EqParams::defaults(EqKind::Z8);
        assert_eq!(z8.listed().len(), 8);
        assert_eq!(z8.free_slot(), Some(0));
        assert_eq!(
            new_band(EqKind::Z8, 1.0, 99.0, Placement::Side).placement,
            Placement::Stereo
        );
    }

    #[test]
    fn presets_match_their_own_params_and_keep_power_off() {
        let presets = presets(EqKind::Z8);
        assert_eq!(presets.len(), 6);
        let mut current = EqParams::defaults(EqKind::Z8);
        current.set_power(false);
        current.set_solo(Some(3));
        let loaded = preset_applied(&current, &presets[2].params);
        assert!(!loaded.power());
        assert_eq!(loaded.solo(), None);
        assert_eq!(matching_preset(&loaded), Some(2));
        let mut edited = loaded.clone();
        edited.set_output_db(1.0);
        assert_eq!(matching_preset(&edited), None);
        assert_eq!(matching_preset(&EqParams::defaults(EqKind::Zx)), Some(0));
    }

    #[test]
    fn slopes_step_and_stop_at_the_ends() {
        assert_eq!(step_slope(24.0, true), 36.0);
        assert_eq!(step_slope(24.0, false), 12.0);
        assert_eq!(step_slope(12.0, false), 12.0);
        assert_eq!(step_slope(96.0, true), 96.0);
    }

    #[test]
    fn readouts_read_like_the_old_editor() {
        assert_eq!(format_freq(440.4), "440");
        assert_eq!(format_freq(1_250.0), "1.25k");
        assert_eq!(format_freq(2_000.0), "2k");
        assert_eq!(format_freq(16_500.0), "16.5k");
        assert_eq!(format_freq(20_000.0), "20k");
        assert_eq!(format_db(0.02), "0");
        assert_eq!(format_db(3.46), "+3.5");
        assert_eq!(format_db(-12.0), "-12.0");
    }

    #[test]
    fn mid_and_side_views_hear_their_own_bands() {
        assert!(Placement::Stereo.heard_in(Placement::Mid));
        assert!(Placement::Mid.heard_in(Placement::Mid));
        assert!(!Placement::Side.heard_in(Placement::Mid));
        assert!(Placement::Side.heard_in(Placement::Stereo));
    }
}
