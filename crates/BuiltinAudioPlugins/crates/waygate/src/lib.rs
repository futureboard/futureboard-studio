//! WayGate — noise gate and ducker.
//!
//! A console-channel style gate with the controls of a studio one:
//!
//! * **Detector.** The key is the insert's own signal through a sidechain
//!   high-pass and low-pass (12 dB/oct each, off at their extremes). A peak
//!   follower (instant rise, [`DETECT_RELEASE_SEC`] fall) feeds a two-threshold
//!   comparator: it opens at the threshold and lets go only below
//!   `threshold − hysteresis`, so a signal hovering at the threshold cannot
//!   chatter. Hold keeps it open that long after the key falls away.
//! * **Gain.** One envelope per detector moves between idle and triggered at a
//!   fixed rate — Attack is the time from fully closed to fully open, Release
//!   from fully open to fully closed — and maps onto the range as a ramp that
//!   is linear in dB, sample by sample. A range at its floor (−80 dB) is a
//!   full mute: the ramp runs down to −80 dB and then lets go to silence.
//! * **Duck** turns it over: the range is taken off while the key is above
//!   the threshold.
//! * **Lookahead** delays the audio, not the key, so the gate has opened by
//!   the time a transient arrives. It is reported as latency and kept while
//!   bypassed, so toggling power never moves the track in time.
//!
//! Allocation-free after construction: the lookahead lines are sized for
//! [`MAX_LOOKAHEAD_MS`] at up to 384 kHz when the DSP is built.

use biquad::{Biquad, DirectForm1};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, clamp, db_to_linear, flush_denormal,
    linear_to_db, make_eq_coefficients, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

/// The processing contract, so a caller can run the gate without depending
/// on the core crate itself.
pub use builtin_dsp_core::StereoEffect;
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.waygate";

/// The range's floor. A range here is a full mute (−∞), not −80 dB.
pub const RANGE_FLOOR_DB: f32 = -80.0;
/// The longest lookahead, in milliseconds.
pub const MAX_LOOKAHEAD_MS: f32 = 10.0;
/// The detector's fall time constant. Short, so the gate follows the key
/// closely; hysteresis and hold are what keep it from chattering.
pub const DETECT_RELEASE_SEC: f32 = 0.005;
/// A key high-pass at (or under) this is off.
pub const KEY_HPF_OFF_HZ: f32 = 20.0;
/// A key low-pass at (or over) this is off.
pub const KEY_LPF_OFF_HZ: f32 = 20_000.0;
/// The quietest gain a meter reports, in dB: what a full mute reads.
pub const METER_FLOOR_DB: f32 = RANGE_FLOOR_DB;

/// Frames each lookahead line holds: 10 ms at 384 kHz is 3,840. A power of
/// two so the read position is a mask, not a modulo.
const LOOKAHEAD_CAPACITY: usize = 4_096;
const LOOKAHEAD_MASK: usize = LOOKAHEAD_CAPACITY - 1;
const KEY_FILTER_Q: f32 = 0.707;

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
/// Range smoothing, per stage of a [`Glide`], so a dragged range cannot
/// zipper a closed gate.
const SMOOTH_MS: f32 = 7.0;
/// Length of the Power / Key Listen / Mode crossfades and of a lookahead
/// change's read-position crossfade.
const FADE_MS: f32 = 10.0;

/// What the detector does when the key crosses the threshold. Wire order:
/// Gate, Duck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Open above the threshold, attenuate by the range below it.
    Gate,
    /// The inverse: attenuate by the range while the key is above it.
    Duck,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Duck => "duck",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "gate" => Some(Self::Gate),
            "duck" => Some(Self::Duck),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Gate => 0.0,
            Self::Duck => 1.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        if value >= 0.5 { Self::Duck } else { Self::Gate }
    }
}

/// What the editors draw: levels, the key, the gate's gain and state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// The key (the filtered sidechain) the detector compares, held peak,
    /// linear.
    pub key_peak: f32,
    /// The gate's gain right now, in dB: `0` open, down to the range closed
    /// ([`METER_FLOOR_DB`] for a full mute). The lower channel's when
    /// unlinked.
    pub gain_db: f32,
    /// `-gain_db`: what the gate is taking off, positive.
    pub gain_reduction_db: f32,
    /// Whether the detector is triggered — the gate held open, or in Duck
    /// mode the duck engaged.
    pub open: bool,
    pub in_clip: bool,
    pub out_clip: bool,
}

/// How a [`MeterFrame`] travels in a host's standard level frame, whose
/// per-rack-position blocks a gate (a single stage) has no use for: the key
/// rides position [`KEY_SLOT`]'s input level, and the detector's state its
/// output level (`1` triggered, `0` not). In and out levels and the
/// reduction travel in their usual fields. Studio's plug-in host and
/// LiveStage both publish through [`MeterFrame::rack_slots`]; the editors read
/// the same positions back.
pub const KEY_SLOT: usize = 0;

impl MeterFrame {
    /// The rack-position blocks of a host level frame `N` positions wide.
    pub fn rack_slots<const N: usize>(&self) -> ([f32; N], [f32; N]) {
        let mut slot_in = [0.0; N];
        let mut slot_out = [0.0; N];
        if N > KEY_SLOT {
            slot_in[KEY_SLOT] = self.key_peak;
            slot_out[KEY_SLOT] = f32::from(self.open);
        }
        (slot_in, slot_out)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub mode: Mode,
    pub threshold_db: f32,
    /// Attenuation when closed, `0` (none) to [`RANGE_FLOOR_DB`] (a mute).
    pub range_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
    /// How far below the threshold the key must fall before the gate closes.
    pub hysteresis_db: f32,
    pub key_hpf_hz: f32,
    pub key_lpf_hz: f32,
    /// Monitor the key instead of the gated signal.
    pub key_listen: bool,
    pub lookahead_ms: f32,
    pub stereo_link: bool,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: Mode::Gate,
        threshold_db: -40.0,
        range_db: RANGE_FLOOR_DB,
        attack_ms: 0.5,
        hold_ms: 20.0,
        release_ms: 150.0,
        hysteresis_db: 4.0,
        key_hpf_hz: KEY_HPF_OFF_HZ,
        key_lpf_hz: KEY_LPF_OFF_HZ,
        key_listen: false,
        lookahead_ms: 0.0,
        stereo_link: true,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "WayGate",
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
        param("thresholdDb", "Threshold", -40.0, -80.0, 0.0, "dB"),
        param(
            "rangeDb",
            "Range",
            RANGE_FLOOR_DB,
            RANGE_FLOOR_DB,
            0.0,
            "dB",
        ),
        param("attackMs", "Attack", 0.5, 0.01, 100.0, "ms"),
        param("holdMs", "Hold", 20.0, 0.0, 2_000.0, "ms"),
        param("releaseMs", "Release", 150.0, 5.0, 4_000.0, "ms"),
        param("hysteresisDb", "Hysteresis", 4.0, 0.0, 20.0, "dB"),
        param(
            "keyHpfHz",
            "Key HPF",
            KEY_HPF_OFF_HZ,
            KEY_HPF_OFF_HZ,
            4_000.0,
            "Hz",
        ),
        param(
            "keyLpfHz",
            "Key LPF",
            KEY_LPF_OFF_HZ,
            100.0,
            KEY_LPF_OFF_HZ,
            "Hz",
        ),
        param("keyListen", "Key Listen", 0.0, 0.0, 1.0, "bool"),
        param("lookaheadMs", "Lookahead", 0.0, 0.0, MAX_LOOKAHEAD_MS, "ms"),
        param("stereoLink", "Stereo Link", 1.0, 0.0, 1.0, "bool"),
    ]
};

/// The gate's gain, in dB, for a range and how far closed the envelope is
/// (`0` open … `1` closed). Linear in dB across the range; a full-mute range
/// ramps over [`RANGE_FLOOR_DB`] and reads the floor once shut. What the
/// editors draw the gain axis from.
pub fn gain_db_at(range_db: f32, closed: f32) -> f32 {
    clamp(range_db, RANGE_FLOOR_DB, 0.0) * clamp(closed, 0.0, 1.0)
}

/// Whether a range is a full mute.
pub fn is_full_mute(range_db: f32) -> bool {
    range_db <= RANGE_FLOOR_DB + 1.0e-3
}

#[derive(Debug, Clone)]
struct Meters {
    in_peak: f32,
    out_peak: f32,
    key_peak: f32,
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
            key_peak: 0.0,
            in_ms: 0.0,
            out_ms: 0.0,
            rms_coeff: time_constant(sample_rate, RMS_WINDOW_SECONDS),
            peak_coeff: time_constant(sample_rate, PEAK_FALL_SECONDS),
            in_clip: false,
            out_clip: false,
        }
    }

    fn reset(&mut self) {
        self.in_peak = 0.0;
        self.out_peak = 0.0;
        self.key_peak = 0.0;
        self.in_ms = 0.0;
        self.out_ms = 0.0;
        self.in_clip = false;
        self.out_clip = false;
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
    fn push(&mut self, input: (f32, f32), output: (f32, f32), key: f32) {
        let in_abs = input.0.abs().max(input.1.abs());
        let out_abs = output.0.abs().max(output.1.abs());
        self.in_peak = Self::fall(self.in_peak, in_abs, self.peak_coeff);
        self.out_peak = Self::fall(self.out_peak, out_abs, self.peak_coeff);
        self.key_peak = Self::fall(self.key_peak, key, self.peak_coeff);

        let in_sq = 0.5 * (input.0 * input.0 + input.1 * input.1);
        let out_sq = 0.5 * (output.0 * output.0 + output.1 * output.1);
        self.in_ms = flush_denormal(self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * in_sq);
        self.out_ms =
            flush_denormal(self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * out_sq);

        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
    }
}

/// How close (relative) a smoothed value must come before it lands exactly.
const SETTLE: f32 = 1.0e-6;

/// One sample of `value` toward its target ([`Smoothed::next`]). Lands
/// exactly once the rest is negligible — or once a step no longer moves it in
/// `f32` — so a finished move leaves the arithmetic it started from.
#[inline]
fn glide(value: &mut Smoothed, step: f32) -> f32 {
    if value.value != value.target {
        let before = value.value;
        value.next(step);
        if value.value == before
            || (value.target - value.value).abs() <= SETTLE * (1.0 + value.target.abs())
        {
            value.settle();
        }
    }
    value.value
}

/// Two [`Smoothed`] stages in series. The second starts every move with zero
/// slope, so even a target that jumps bends in without a corner.
#[derive(Debug, Clone, Copy)]
struct Glide {
    inner: Smoothed,
    outer: Smoothed,
}

impl Glide {
    fn at(value: f32) -> Self {
        Self {
            inner: Smoothed::at(value),
            outer: Smoothed::at(value),
        }
    }

    fn set(&mut self, target: f32) {
        self.inner.target = target;
    }

    fn settle(&mut self) {
        self.inner.settle();
        self.outer = Smoothed::at(self.inner.target);
    }

    #[inline]
    fn moving(&self) -> bool {
        self.outer.value != self.inner.target || self.inner.value != self.inner.target
    }

    /// One sample further; the smoothed value.
    #[inline]
    fn next(&mut self, step: f32) -> f32 {
        if self.moving() {
            glide(&mut self.inner, step);
            self.outer.target = self.inner.value;
            glide(&mut self.outer, step);
        }
        self.outer.value
    }
}

/// Eased 0…1: zero slope at both ends, so a fade has no corner.
#[inline]
fn ease(pos: f32) -> f32 {
    pos * pos * (3.0 - 2.0 * pos)
}

/// Samples in a [`FADE_MS`] crossfade, as a per-sample step.
fn fade_step(sample_rate: f32) -> f32 {
    1.0 / (FADE_MS * 0.001 * sample_rate.max(1.0)).max(1.0)
}

/// A 0…1 crossfade position: moves linearly toward its target over
/// [`FADE_MS`] and is read eased, so neither end of the fade has a corner.
/// Lands exactly on 0 or 1.
#[derive(Debug, Clone, Copy)]
struct Fade {
    pos: f32,
    target: f32,
    step: f32,
}

impl Fade {
    fn new(on: bool, sample_rate: f32) -> Self {
        let at = if on { 1.0 } else { 0.0 };
        Self {
            pos: at,
            target: at,
            step: fade_step(sample_rate),
        }
    }

    fn set(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
    }

    fn settle(&mut self) {
        self.pos = self.target;
    }

    /// One sample further; the eased weight of the "on" side.
    #[inline]
    fn next(&mut self) -> f32 {
        if self.pos < self.target {
            self.pos = (self.pos + self.step).min(self.target);
        } else if self.pos > self.target {
            self.pos = (self.pos - self.step).max(self.target);
        }
        ease(self.pos)
    }
}

/// `a` at `t` 0, `b` at `t` 1 (both exactly), a straight blend between.
#[inline]
fn blend(a: f32, b: f32, t: f32) -> f32 {
    if t <= 0.0 {
        a
    } else if t >= 1.0 {
        b
    } else {
        a + (b - a) * t
    }
}

/// A delay time that changes while audio runs: instead of jumping the read
/// position (a click), the output crossfades from the old position to the
/// new one over [`FADE_MS`]. A change that arrives mid-fade waits for the
/// fade to finish, so a dragged knob walks over in clean steps and always
/// lands on the latest value.
#[derive(Debug, Clone, Copy)]
struct DelaySwitch {
    from: usize,
    to: usize,
    /// The latest delay asked for: what the next fade goes to.
    pending: usize,
    pos: f32,
    step: f32,
}

impl DelaySwitch {
    fn new(delay: usize, sample_rate: f32) -> Self {
        Self {
            from: delay,
            to: delay,
            pending: delay,
            pos: 1.0,
            step: fade_step(sample_rate),
        }
    }

    fn request(&mut self, delay: usize) {
        self.pending = delay;
    }

    /// Straight to the latest delay, no fade.
    fn settle(&mut self) {
        self.from = self.pending;
        self.to = self.pending;
        self.pos = 1.0;
    }

    /// One sample further: `(from, to, weight of to)`. Settled, the weight
    /// is exactly 1.
    #[inline]
    fn next(&mut self) -> (usize, usize, f32) {
        if self.pos >= 1.0 {
            if self.pending == self.to {
                return (self.to, self.to, 1.0);
            }
            self.from = self.to;
            self.to = self.pending;
            self.pos = 0.0;
        }
        self.pos = (self.pos + self.step).min(1.0);
        (self.from, self.to, ease(self.pos))
    }
}

/// Everything the per-sample path reads, resolved from the params once per
/// edit.
#[derive(Debug, Clone, Copy)]
struct Coeffs {
    /// `0` gate … `1` duck; between the two while Mode crossfades.
    duck: f32,
    open_level: f32,
    close_level: f32,
    detect_fall: f32,
    hold_samples: u32,
    attack_step: f32,
    release_step: f32,
    /// The range the ramp runs over, ≤ 0 dB.
    span_db: f32,
    /// The gain once fully closed: `db_to_linear(span_db)`, or `0` for a mute.
    closed_gain: f32,
}

/// One detector and its gain envelope.
#[derive(Debug, Clone, Copy, Default)]
struct GateChannel {
    /// The key's peak follower, linear.
    level: f32,
    /// Triggered: above the threshold, or held.
    open: bool,
    hold_left: u32,
    /// `0` idle … `1` triggered.
    env: f32,
}

impl GateChannel {
    /// Advance one sample on the key's magnitude; the gain to apply.
    #[inline]
    fn step(&mut self, key_abs: f32, c: &Coeffs) -> f32 {
        self.level = flush_denormal(key_abs.max(self.level * c.detect_fall));
        if self.level >= c.open_level {
            self.open = true;
            self.hold_left = c.hold_samples;
        } else if self.open {
            if self.level >= c.close_level {
                // Inside the hysteresis band: still held from the top.
                self.hold_left = c.hold_samples;
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
            } else {
                self.open = false;
            }
        }
        self.env = if self.open {
            (self.env + c.attack_step).min(1.0)
        } else {
            (self.env - c.release_step).max(0.0)
        };
        let gate = Self::gain_at(1.0 - self.env, c);
        if c.duck <= 0.0 {
            gate
        } else if c.duck >= 1.0 {
            Self::gain_at(self.env, c)
        } else {
            // Mode crossfading: a straight blend of the gate's and the
            // ducker's gains, so neither the flip nor a full mute's snap to
            // silence can step the output.
            blend(gate, Self::gain_at(self.env, c), c.duck)
        }
    }

    /// The gain for how far closed (`0` open … `1` closed) the ramp is.
    #[inline]
    fn gain_at(closed: f32, c: &Coeffs) -> f32 {
        if closed <= 0.0 {
            1.0
        } else if closed >= 1.0 {
            c.closed_gain
        } else {
            db_to_linear(c.span_db * closed)
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A fixed-length delay line, written every sample whatever the lookahead,
/// so a lookahead change reads real history rather than stale samples.
#[derive(Debug, Clone)]
struct DelayLine {
    buffer: Box<[f32]>,
}

impl DelayLine {
    fn new() -> Self {
        Self {
            buffer: vec![0.0; LOOKAHEAD_CAPACITY].into_boxed_slice(),
        }
    }

    #[inline]
    fn write(&mut self, write: usize, value: f32) {
        self.buffer[write] = value;
    }

    /// The sample `delay` writes before the one at `write`.
    #[inline]
    fn read(&self, write: usize, delay: usize) -> f32 {
        self.buffer[(write + LOOKAHEAD_CAPACITY - delay) & LOOKAHEAD_MASK]
    }

    /// What the line plays at `write` through a [`DelaySwitch`] step.
    #[inline]
    fn read_switched(&self, write: usize, (from, to, weight): (usize, usize, f32)) -> f32 {
        if weight >= 1.0 {
            self.read(write, to)
        } else {
            blend(self.read(write, from), self.read(write, to), weight)
        }
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
    }
}

/// The key filter of one channel: a high-pass then a low-pass, each skipped
/// while it is off.
#[derive(Clone)]
struct KeyFilter {
    hpf: DirectForm1<f32>,
    lpf: DirectForm1<f32>,
}

impl KeyFilter {
    fn new(sample_rate: f32) -> Self {
        let hpf = make_eq_coefficients("highpass", 100.0, 0.0, KEY_FILTER_Q, sample_rate)
            .expect("a 100 Hz high-pass always builds");
        let lpf = make_eq_coefficients("lowpass", 5_000.0, 0.0, KEY_FILTER_Q, sample_rate)
            .expect("a 5 kHz low-pass always builds");
        Self {
            hpf: DirectForm1::<f32>::new(hpf),
            lpf: DirectForm1::<f32>::new(lpf),
        }
    }

    #[inline]
    fn run(&mut self, x: f32, hpf_on: bool, lpf_on: bool) -> f32 {
        let y = if hpf_on { self.hpf.run(x) } else { x };
        if lpf_on { self.lpf.run(y) } else { y }
    }

    fn reset(&mut self) {
        self.hpf.reset_state();
        self.lpf.reset_state();
    }
}

pub struct Dsp {
    params: Params,
    /// Audio has run since construction or [`StereoEffect::reset`]. Until
    /// it has, an edit lands at once — an insert or session being set up
    /// replays its stored values, and there is nothing yet to glide over.
    started: bool,
    sample_rate: f32,
    meters: Meters,
    coeffs: Coeffs,
    channels: [GateChannel; 2],
    keys: [KeyFilter; 2],
    hpf_on: bool,
    lpf_on: bool,
    dry: [DelayLine; 2],
    key_line: [DelayLine; 2],
    write: usize,
    /// The lookahead, crossfaded when it changes.
    lookahead: DelaySwitch,
    /// The range in dB, gliding.
    range: Glide,
    smooth_step: f32,
    /// Dry ↔ gated: Power crossfades instead of switching.
    power: Fade,
    /// Gated ↔ key audition.
    listen: Fade,
    /// Gate ↔ duck.
    duck: Fade,
    /// The last sample's lower gain, for the meters.
    gain: f32,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            started: false,
            params: default_params(),
            sample_rate: sr,
            meters: Meters::new(sr),
            coeffs: Coeffs {
                duck: 0.0,
                open_level: 1.0,
                close_level: 1.0,
                detect_fall: 0.0,
                hold_samples: 0,
                attack_step: 1.0,
                release_step: 1.0,
                span_db: RANGE_FLOOR_DB,
                closed_gain: 0.0,
            },
            channels: [GateChannel::default(); 2],
            keys: [KeyFilter::new(sr), KeyFilter::new(sr)],
            hpf_on: false,
            lpf_on: false,
            dry: [DelayLine::new(), DelayLine::new()],
            key_line: [DelayLine::new(), DelayLine::new()],
            write: 0,
            lookahead: DelaySwitch::new(0, sr),
            range: Glide::at(RANGE_FLOOR_DB),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            power: Fade::new(true, sr),
            listen: Fade::new(false, sr),
            duck: Fade::new(false, sr),
            gain: 1.0,
        };
        dsp.apply_params();
        dsp.settle();
        dsp.gain = dsp.coeffs.closed_gain;
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter at once — a state load. Lands on the new
    /// values without a glide or a fade.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
        self.settle();
    }

    pub fn meter_frame(&self) -> MeterFrame {
        let gain_db = if !self.params.power || self.params.key_listen {
            0.0
        } else if self.gain <= 0.0 {
            METER_FLOOR_DB
        } else {
            linear_to_db(self.gain).max(METER_FLOOR_DB)
        };
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            key_peak: self.meters.key_peak,
            gain_db,
            gain_reduction_db: -gain_db,
            open: self.params.power && self.channels.iter().any(|c| c.open),
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        self.apply_params();
        if !self.started {
            self.settle();
        }
        true
    }

    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// The lookahead, in samples. Reported whether or not the gate is on: the
    /// audio is delayed either way, so bypassing never moves the track.
    pub fn latency_samples(&self) -> usize {
        self.lookahead.pending
    }

    fn samples(&self, ms: f32) -> f32 {
        ms * 0.001 * self.sample_rate
    }

    fn apply_params(&mut self) {
        let p = &self.params;
        let sr = self.sample_rate.max(1.0);
        let span_db = clamp(p.range_db, RANGE_FLOOR_DB, 0.0);
        let attack = self.samples(p.attack_ms).max(1.0);
        let release = self.samples(p.release_ms).max(1.0);
        let hold = self.samples(p.hold_ms).round().max(0.0) as u32;
        self.duck.set(p.mode == Mode::Duck);
        self.power.set(p.power);
        self.listen.set(p.key_listen);
        // The range glides; the per-sample path keeps `span_db` and
        // `closed_gain` with it while it moves.
        self.range.set(span_db);
        let span_now = self.range.outer.value;
        self.coeffs = Coeffs {
            duck: self.coeffs.duck,
            open_level: db_to_linear(p.threshold_db),
            close_level: db_to_linear(p.threshold_db - p.hysteresis_db.max(0.0)),
            detect_fall: time_constant(sr, DETECT_RELEASE_SEC),
            hold_samples: hold,
            attack_step: 1.0 / attack,
            release_step: 1.0 / release,
            span_db: span_now,
            closed_gain: closed_gain(span_now, !self.range.moving()),
        };

        // The key filters retune in place; one switched on starts from rest.
        let hpf_on = p.key_hpf_hz > KEY_HPF_OFF_HZ + 0.5;
        let lpf_on = p.key_lpf_hz < KEY_LPF_OFF_HZ - 0.5;
        let hpf = make_eq_coefficients("highpass", p.key_hpf_hz, 0.0, KEY_FILTER_Q, sr);
        let lpf = make_eq_coefficients("lowpass", p.key_lpf_hz, 0.0, KEY_FILTER_Q, sr);
        for key in &mut self.keys {
            if let Some(c) = hpf {
                key.hpf.update_coefficients(c);
            }
            if let Some(c) = lpf {
                key.lpf.update_coefficients(c);
            }
            if hpf_on && !self.hpf_on {
                key.hpf.reset_state();
            }
            if lpf_on && !self.lpf_on {
                key.lpf.reset_state();
            }
        }
        self.hpf_on = hpf_on;
        self.lpf_on = lpf_on;

        self.lookahead.request(
            (self.samples(p.lookahead_ms).round().max(0.0) as usize).min(LOOKAHEAD_CAPACITY - 1),
        );
    }

    /// Land the range, the fades and the lookahead on their targets.
    fn settle(&mut self) {
        self.range.settle();
        self.power.settle();
        self.listen.settle();
        self.duck.settle();
        self.coeffs.duck = self.duck.target;
        self.lookahead.settle();
        self.coeffs.span_db = self.range.outer.value;
        self.coeffs.closed_gain = closed_gain(self.coeffs.span_db, true);
    }

    fn set_sample_rate_internal(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.meters = Meters::new(self.sample_rate);
        self.smooth_step = smoothing_step(SMOOTH_MS, self.sample_rate);
        let step = fade_step(self.sample_rate);
        self.power.step = step;
        self.listen.step = step;
        self.duck.step = step;
        self.lookahead.step = step;
    }
}

/// The gain once fully closed for a range of `span_db`: a mute at the floor
/// once the range has landed there, else the range itself.
#[inline]
fn closed_gain(span_db: f32, settled: bool) -> f32 {
    if settled && is_full_mute(span_db) {
        0.0
    } else {
        db_to_linear(span_db)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.meters.reset();
        for channel in &mut self.channels {
            channel.reset();
        }
        for key in &mut self.keys {
            key.reset();
        }
        for line in self.dry.iter_mut().chain(self.key_line.iter_mut()) {
            line.clear();
        }
        self.settle();
        self.gain = self.coeffs.closed_gain;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.set_sample_rate_internal(sample_rate);
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;
        let key_l = self.keys[0].run(left, self.hpf_on, self.lpf_on);
        let key_r = self.keys[1].run(right, self.hpf_on, self.lpf_on);
        let write = self.write;
        self.write = (write + 1) & LOOKAHEAD_MASK;
        self.dry[0].write(write, left);
        self.dry[1].write(write, right);
        self.key_line[0].write(write, key_l);
        self.key_line[1].write(write, key_r);
        let delay = self.lookahead.next();
        let dry_l = self.dry[0].read_switched(write, delay);
        let dry_r = self.dry[1].read_switched(write, delay);
        let key_peak = key_l.abs().max(key_r.abs());

        let on = self.power.next();
        if on <= 0.0 {
            // Off and faded out: only the delay runs.
            self.gain = 1.0;
            self.meters.push((left, right), (dry_l, dry_r), key_peak);
            return (dry_l, dry_r);
        }

        self.coeffs.duck = self.duck.next();
        if self.range.moving() {
            let span = self.range.next(self.smooth_step);
            self.coeffs.span_db = span;
            self.coeffs.closed_gain = closed_gain(span, !self.range.moving());
        }

        let (gain_l, gain_r) = if self.params.stereo_link {
            let gain = self.channels[0].step(key_peak, &self.coeffs);
            // The unused detector follows, so unlinking starts in step.
            self.channels[1] = self.channels[0];
            (gain, gain)
        } else {
            (
                self.channels[0].step(key_l.abs(), &self.coeffs),
                self.channels[1].step(key_r.abs(), &self.coeffs),
            )
        };
        self.gain = gain_l.min(gain_r);

        let mut out = (dry_l * gain_l, dry_r * gain_r);
        let listen = self.listen.next();
        if listen > 0.0 {
            let late_key_l = self.key_line[0].read_switched(write, delay);
            let late_key_r = self.key_line[1].read_switched(write, delay);
            out = (
                blend(out.0, late_key_l, listen),
                blend(out.1, late_key_r, listen),
            );
        }
        let out = (blend(dry_l, out.0, on), blend(dry_r, out.1, on));
        self.meters.push((left, right), out, key_peak);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 128;
    /// About a second of blocks: one drag across a range and back.
    const DRAG_BLOCKS: usize = 375;

    /// Two low tones: smooth, so a gain that steps shows as a kink.
    fn tone(n: usize) -> (f32, f32) {
        let t = n as f64 / 48_000.0;
        let x = (std::f64::consts::TAU * 110.0 * t).sin() * 0.3
            + (std::f64::consts::TAU * 330.0 * t).sin() * 0.1;
        (x as f32, (x * 0.8) as f32)
    }

    /// The parameter sweep's measure: the biggest second difference where a
    /// block meets the last one, against the biggest one inside a block,
    /// while `change` edits the DSP before each 128-frame block. About 1 is
    /// smooth; a value that steps once per block stands out many times over.
    fn edge_jump(dsp: &mut Dsp, mut change: impl FnMut(&mut Dsp, usize)) -> f32 {
        let mut n = 0;
        let mut last = [(0.0f32, 0.0f32); 2];
        for _ in 0..200 * BLOCK {
            let (l, r) = tone(n);
            n += 1;
            last = [last[1], dsp.process_stereo(l, r)];
        }
        let d2 = |a: f32, b: f32, c: f32| (c - 2.0 * b + a).abs();
        let mut worst = 0.0f32;
        for block in 0..DRAG_BLOCKS {
            change(dsp, block);
            let (mut edge, mut inside) = (0.0f32, 1.0e-7f32);
            for i in 0..BLOCK {
                let (l, r) = tone(n);
                n += 1;
                let out = dsp.process_stereo(l, r);
                assert!(out.0.is_finite() && out.1.is_finite());
                let kink = d2(last[0].0, last[1].0, out.0).max(d2(last[0].1, last[1].1, out.1));
                if i < 2 {
                    edge = edge.max(kink);
                } else {
                    inside = inside.max(kink);
                }
                last = [last[1], out];
            }
            worst = worst.max(edge / inside);
        }
        worst
    }

    /// Up and back down across `[min, max]`, a value per block.
    fn drag(min: f32, max: f32, block: usize) -> f32 {
        let phase = block as f32 / DRAG_BLOCKS as f32 * 2.0;
        let x = if phase < 1.0 { phase } else { 2.0 - phase };
        min + (max - min) * x
    }

    /// A click every quarter second, through every step of a switch.
    fn click(dsp: &mut Dsp, index: u32, steps: usize, block: usize) {
        if block.is_multiple_of(94) {
            dsp.apply_wire_param(index, ((block / 94) % steps) as f32);
        }
    }

    /// A gate the sweep's tone sits under (closed, at −30 dB), with the key
    /// filtered so Key Listen sounds different.
    fn closed_gate() -> Dsp {
        let mut dsp = Dsp::new(SR);
        let mut params = default_params();
        params.threshold_db = 0.0;
        params.range_db = -30.0;
        params.key_hpf_hz = 1_000.0;
        dsp.set_params(params);
        dsp
    }

    #[test]
    fn a_dragged_range_does_not_zipper() {
        let mut dsp = closed_gate();
        let jump = edge_jump(&mut dsp, |dsp, block| {
            dsp.apply_wire_param(ipc::RANGE_INDEX, drag(RANGE_FLOOR_DB, 0.0, block));
        });
        assert!(jump < 4.0, "range: block-edge jump {jump}");
    }

    #[test]
    fn a_dragged_lookahead_crossfades_instead_of_jumping() {
        let mut dsp = closed_gate();
        let jump = edge_jump(&mut dsp, |dsp, block| {
            dsp.apply_wire_param(ipc::LOOKAHEAD_INDEX, drag(0.0, MAX_LOOKAHEAD_MS, block));
        });
        assert!(jump < 4.0, "lookahead: block-edge jump {jump}");
        // The reported latency is the latest value, and the audio lands on it.
        assert!(dsp.apply_ui_param("lookaheadMs", 5.0));
        assert_eq!(dsp.latency_samples(), ms(5.0));
        assert!(dsp.apply_ui_param("thresholdDb", -80.0));
        assert!(dsp.apply_ui_param("power", 0.0));
        for _ in 0..ms(100.0) {
            dsp.process_stereo(0.0, 0.0);
        }
        let out: Vec<f32> = (0..ms(10.0))
            .map(|i| dsp.process_stereo(if i == 0 { 0.5 } else { 0.0 }, 0.0).0)
            .collect();
        assert_eq!(out[ms(5.0)], 0.5);
        assert!(
            out.iter()
                .enumerate()
                .all(|(i, s)| i == ms(5.0) || *s == 0.0)
        );
    }

    #[test]
    fn switches_crossfade_instead_of_stepping() {
        for (index, steps) in [
            (ipc::POWER_INDEX, 2),
            (ipc::MODE_INDEX, 2),
            (ipc::KEY_LISTEN_INDEX, 2),
        ] {
            // Closed at −30 dB, and open at the defaults (a full-mute range,
            // so a Gate ↔ Duck flip runs between unity and silence).
            for (setup, start) in [("closed", closed_gate()), ("open", gate())] {
                let mut dsp = start;
                let jump = edge_jump(&mut dsp, |dsp, block| click(dsp, index, steps, block));
                assert!(
                    jump < 4.0,
                    "{} ({setup}): block-edge jump {jump}",
                    UI_PARAM_IDS[index as usize]
                );
            }
        }
    }

    #[test]
    fn a_state_load_lands_at_once() {
        let mut dsp = gate();
        let mut params = default_params();
        params.range_db = -12.0;
        params.lookahead_ms = 2.0;
        params.power = false;
        dsp.set_params(params);
        assert_eq!(dsp.coeffs.span_db, -12.0);
        assert_eq!(dsp.lookahead.next(), (ms(2.0), ms(2.0), 1.0));
    }

    const SR: f32 = 48_000.0;

    fn ms(n: f32) -> usize {
        (n * 0.001 * SR).round() as usize
    }

    fn sine(i: usize, hz: f32, amplitude: f32) -> f32 {
        (std::f32::consts::TAU * hz * i as f32 / SR).sin() * amplitude
    }

    fn gate() -> Dsp {
        Dsp::new(SR)
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.name, "WayGate");
        assert_eq!(d.category, PluginCategory::Effect);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len(), "duplicate parameter id in descriptor");
        assert_eq!(count, ipc::PARAM_COUNT, "a wire id has no descriptor");

        let defaults = ipc::ui_values(&default_params());
        for param in d.params {
            let (_, actual) = defaults
                .iter()
                .find(|(id, _)| *id == param.id)
                .copied()
                .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
            assert!(
                (actual - param.default_value).abs() < 1.0e-5,
                "{} default drifted: descriptor={} ui_values={}",
                param.id,
                param.default_value,
                actual
            );
            // The descriptor's range is the wire's clamp, for every
            // continuous param.
            let index = ui_param_index(param.id).unwrap() as usize;
            let (min, max) = ipc::RANGES[index];
            if param.unit != "bool" && param.unit != "enum" {
                assert_eq!((param.min, param.max), (min, max), "{} range", param.id);
            }
            assert!(param.min <= param.default_value && param.default_value <= param.max);
        }
    }

    #[test]
    fn every_preset_is_in_range_and_named_once() {
        let bank = factory_presets();
        assert_eq!(bank[0].name, "Default");
        let mut names: Vec<_> = bank.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), bank.len());
        for preset in &bank {
            let mut sanitized = preset.params.clone();
            ipc::sanitize_params(&mut sanitized);
            assert_eq!(
                ipc::ui_values(&sanitized),
                ipc::ui_values(&preset.params),
                "{} is out of range",
                preset.name
            );
            assert_eq!(preset.params.lookahead_ms, 0.0, "{}", preset.name);
        }
    }

    /// Below the threshold the output is the input times exactly the range.
    #[test]
    fn a_closed_gate_attenuates_by_exactly_the_range() {
        for range in [-6.0f32, -24.0, -40.0, -79.0] {
            let mut dsp = gate();
            assert!(dsp.apply_ui_param("thresholdDb", -20.0));
            assert!(dsp.apply_ui_param("rangeDb", range));
            for i in 0..ms(100.0) {
                let x = sine(i, 440.0, 0.05); // −26 dB, under the threshold
                let (l, r) = dsp.process_stereo(x, x);
                if x.abs() > 1.0e-3 {
                    let ratio_db = linear_to_db((l / x).abs());
                    assert!((ratio_db - range).abs() < 1.0e-3, "{range}: {ratio_db}");
                    assert_eq!(l, r);
                }
            }
            let frame = dsp.meter_frame();
            assert!(!frame.open);
            assert!((frame.gain_db - range).abs() < 1.0e-3);
            assert!((frame.gain_reduction_db + range).abs() < 1.0e-3);
        }
        // The floor is a mute, not −80 dB.
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("rangeDb", RANGE_FLOOR_DB));
        for i in 0..ms(20.0) {
            let (l, r) = dsp.process_stereo(sine(i, 440.0, 0.005), 0.005);
            assert_eq!((l, r), (0.0, 0.0));
        }
        assert_eq!(dsp.meter_frame().gain_db, METER_FLOOR_DB);
    }

    #[test]
    fn an_open_gate_passes_unity() {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("thresholdDb", -40.0));
        // Opened by the first samples; fully open after the 0.5 ms attack.
        for i in 0..ms(200.0) {
            let x = sine(i, 220.0, 0.5);
            let (l, r) = dsp.process_stereo(x, -x);
            if i > ms(2.0) {
                assert_eq!(l, x, "sample {i}");
                assert_eq!(r, -x, "sample {i}");
            }
        }
        let frame = dsp.meter_frame();
        assert!(frame.open);
        assert_eq!(frame.gain_db, 0.0);
        assert!(frame.key_peak > 0.4);
    }

    /// Counts the detector's open/close changes over a tone whose level
    /// steps 1.5 dB either side of the threshold every 10 ms.
    fn transitions(hysteresis_db: f32) -> usize {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("thresholdDb", -30.0));
        assert!(dsp.apply_ui_param("hysteresisDb", hysteresis_db));
        assert!(dsp.apply_ui_param("holdMs", 0.0));
        assert!(dsp.apply_ui_param("releaseMs", 5.0));
        let high = db_to_linear(-28.5);
        let low = db_to_linear(-31.5);
        let mut was = false;
        let mut count = 0;
        for i in 0..ms(1_000.0) {
            let amplitude = if (i / ms(10.0)).is_multiple_of(2) {
                high
            } else {
                low
            };
            dsp.process_stereo(sine(i, 1_000.0, amplitude), 0.0);
            let open = dsp.meter_frame().open;
            if open != was {
                count += 1;
                was = open;
            }
        }
        count
    }

    #[test]
    fn hysteresis_prevents_chatter_at_the_threshold() {
        // Without it, the gate follows every step down and back up.
        assert!(transitions(0.0) > 50, "{}", transitions(0.0));
        // With 6 dB it opens once and stays open.
        assert_eq!(transitions(6.0), 1);
    }

    /// Attack, hold and release, measured off the gate's own gain.
    #[test]
    fn attack_hold_and_release_take_their_times() {
        let mut dsp = gate();
        for (id, value) in [
            ("thresholdDb", -30.0),
            ("rangeDb", -60.0),
            ("attackMs", 10.0),
            ("holdMs", 50.0),
            ("releaseMs", 100.0),
            ("hysteresisDb", 6.0),
        ] {
            assert!(dsp.apply_ui_param(id, value));
        }
        let level = 0.5f32;
        let onset = ms(20.0);
        let stop = ms(200.0);
        let mut gains = Vec::with_capacity(ms(600.0));
        for i in 0..ms(600.0) {
            let x = if (onset..stop).contains(&i) {
                level
            } else {
                0.0
            };
            dsp.process_stereo(x, x);
            gains.push(dsp.meter_frame().gain_db);
        }
        let first = |from: usize, test: &dyn Fn(f32) -> bool| {
            (from..gains.len()).find(|&i| test(gains[i])).unwrap()
        };
        let tolerance = ms(0.1);

        // Attack: from the onset to fully open.
        let opened = first(onset, &|g| g >= 0.0);
        let attack = opened - onset;
        assert!(attack.abs_diff(ms(10.0)) <= tolerance, "attack {attack}");

        // Hold: the detector's own fall to the close level, then the hold.
        let close_level = db_to_linear(-36.0);
        let fall = time_constant(SR, DETECT_RELEASE_SEC);
        let detect = (close_level / level).ln() / fall.ln();
        let release_start = first(stop, &|g| g < 0.0);
        let expected = detect + ms(50.0) as f32;
        let held = (release_start - stop) as f32;
        assert!(
            (held - expected).abs() <= tolerance as f32,
            "hold {held} expected {expected}"
        );

        // Release: fully open to fully closed, linear in dB.
        let closed = first(release_start, &|g| g <= -60.0 + 1.0e-3);
        let release = closed - release_start;
        assert!(
            release.abs_diff(ms(100.0)) <= tolerance,
            "release {release}"
        );
        let midway = gains[release_start + ms(50.0)];
        assert!((midway + 30.0).abs() < 0.5, "halfway through: {midway}");
    }

    #[test]
    fn lookahead_reports_latency_and_opens_before_the_transient() {
        let lookahead = ms(5.0);
        let onset = ms(20.0);
        let run = |lookahead_ms: f32, power: bool| {
            let mut dsp = gate();
            assert!(dsp.apply_ui_param("lookaheadMs", lookahead_ms));
            assert!(dsp.apply_ui_param("attackMs", 1.0));
            assert!(dsp.apply_ui_param("thresholdDb", -30.0));
            assert!(dsp.apply_ui_param("power", f32::from(power)));
            let out: Vec<f32> = (0..ms(60.0))
                .map(|i| {
                    let x = if i >= onset { 0.5 } else { 0.0 };
                    dsp.process_stereo(x, x).0
                })
                .collect();
            (dsp.latency_samples(), out)
        };

        let (latency, out) = run(5.0, true);
        assert_eq!(latency, lookahead);
        // Nothing comes out before the delayed hit…
        assert!(out[..onset + lookahead].iter().all(|s| *s == 0.0));
        // …and the hit's first sample comes out whole: the gate opened
        // during the lookahead.
        assert_eq!(out[onset + lookahead], 0.5);

        // Without lookahead the hit's front is still being faded in.
        let (latency, out) = run(0.0, true);
        assert_eq!(latency, 0);
        assert!(out[onset].abs() < 0.01, "{}", out[onset]);

        // Bypassed, the delay stays: power never moves the track in time.
        let (latency, out) = run(5.0, false);
        assert_eq!(latency, lookahead);
        assert_eq!(out[onset + lookahead - 1], 0.0);
        assert_eq!(out[onset + lookahead], 0.5);
    }

    /// The key's level after a second of a tone, and whether it opened.
    fn keyed(hz: f32, setup: &[(&str, f32)]) -> (f32, bool) {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("thresholdDb", -20.0));
        for (id, value) in setup {
            assert!(dsp.apply_ui_param(id, *value));
        }
        for i in 0..ms(1_000.0) {
            let x = sine(i, hz, 0.5);
            dsp.process_stereo(x, x);
        }
        let frame = dsp.meter_frame();
        (frame.key_peak, frame.open)
    }

    #[test]
    fn the_key_filters_decide_what_opens_the_gate() {
        // A bass note opens it, until the key's high-pass is above it.
        assert!(keyed(50.0, &[]).1);
        let (key, open) = keyed(50.0, &[("keyHpfHz", 1_000.0)]);
        assert!(!open);
        assert!(linear_to_db(key) < -40.0, "{}", linear_to_db(key));
        // A hat opens it, until the key's low-pass is under it.
        assert!(keyed(10_000.0, &[]).1);
        let (key, open) = keyed(10_000.0, &[("keyLpfHz", 500.0)]);
        assert!(!open);
        assert!(linear_to_db(key) < -40.0, "{}", linear_to_db(key));
        // In its passband the key is the signal.
        let (key, open) = keyed(2_000.0, &[("keyHpfHz", 200.0), ("keyLpfHz", 8_000.0)]);
        assert!(open);
        assert!((linear_to_db(key) - linear_to_db(0.5)).abs() < 1.0);
    }

    #[test]
    fn key_listen_plays_the_key() {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("keyHpfHz", 1_000.0));
        assert!(dsp.apply_ui_param("keyListen", 1.0));
        let mut peak = 0.0f32;
        for i in 0..ms(500.0) {
            let (l, _) = dsp.process_stereo(sine(i, 50.0, 0.5), 0.0);
            if i > ms(100.0) {
                peak = peak.max(l.abs());
            }
        }
        // The bass is what the high-pass took out.
        assert!(linear_to_db(peak) < -40.0, "{}", linear_to_db(peak));
        assert_eq!(dsp.meter_frame().gain_reduction_db, 0.0);
    }

    #[test]
    fn duck_takes_the_range_off_while_the_key_is_loud() {
        let mut dsp = gate();
        for (id, value) in [
            ("mode", 1.0),
            ("thresholdDb", -30.0),
            ("rangeDb", -12.0),
            ("attackMs", 1.0),
        ] {
            assert!(dsp.apply_ui_param(id, value));
        }
        // Quiet: unity.
        let (l, _) = dsp.process_stereo(0.01, 0.01);
        assert_eq!(l, 0.01);
        for _ in 0..ms(20.0) {
            dsp.process_stereo(0.5, 0.5);
        }
        let (l, _) = dsp.process_stereo(0.5, 0.5);
        assert!((linear_to_db(l / 0.5) + 12.0).abs() < 1.0e-3);
        let frame = dsp.meter_frame();
        assert!(frame.open);
        assert!((frame.gain_reduction_db - 12.0).abs() < 1.0e-3);
    }

    #[test]
    fn unlinked_channels_gate_on_their_own() {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("stereoLink", 0.0));
        assert!(dsp.apply_ui_param("rangeDb", -20.0));
        for i in 0..ms(50.0) {
            let (l, r) = dsp.process_stereo(sine(i, 300.0, 0.5), sine(i, 300.0, 0.001));
            assert!(l.is_finite() && r.is_finite());
        }
        let i = ms(50.0);
        let (l, r) = dsp.process_stereo(sine(i, 300.0, 0.5), sine(i, 300.0, 0.001));
        assert_eq!(l, sine(i, 300.0, 0.5));
        assert!((r - sine(i, 300.0, 0.001) * db_to_linear(-20.0)).abs() < 1.0e-7);
        // Linked, the loud side opens both.
        assert!(dsp.apply_ui_param("stereoLink", 1.0));
        let mut last = (0.0, 0.0);
        for i in 0..ms(10.0) {
            last = dsp.process_stereo(sine(i, 300.0, 0.5), 0.001);
        }
        assert_eq!(last.1, 0.001);
    }

    #[test]
    fn power_off_passes_the_signal_and_reports_no_reduction() {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("power", 0.0));
        let (l, r) = dsp.process_stereo(0.001, -0.4);
        assert_eq!((l, r), (0.001, -0.4));
        let frame = dsp.meter_frame();
        assert_eq!(frame.gain_reduction_db, 0.0);
        assert!(!frame.open);
    }

    #[test]
    fn extreme_settings_stay_finite() {
        let mut dsp = gate();
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            for value in [-1.0e9, 0.0, 1.0e9] {
                assert!(dsp.apply_wire_param(index as u32, value), "{id}");
                for i in 0..64 {
                    let (l, r) = dsp.process_stereo(sine(i, 997.0, 0.9), sine(i, 31.0, 0.9));
                    assert!(l.is_finite() && r.is_finite(), "{id}={value}");
                }
            }
        }
        dsp.set_sample_rate(192_000.0);
        assert!(dsp.apply_ui_param("lookaheadMs", 10.0));
        assert_eq!(dsp.latency_samples(), 1_920);
        dsp.set_sample_rate(384_000.0);
        assert_eq!(dsp.latency_samples(), 3_840);
        dsp.reset();
        assert_eq!(dsp.process_stereo(0.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn wire_params_round_trip() {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("thresholdDb", -33.0));
        assert!(dsp.apply_ui_param("rangeDb", -18.0));
        assert!(dsp.apply_ui_param("holdMs", 250.0));
        assert!(dsp.apply_ui_param("keyListen", 1.0));
        assert!(dsp.apply_ui_param("mode", 1.0));
        assert_eq!(dsp.params().threshold_db, -33.0);
        assert_eq!(dsp.params().range_db, -18.0);
        assert_eq!(dsp.params().hold_ms, 250.0);
        assert!(dsp.params().key_listen);
        assert_eq!(dsp.params().mode, Mode::Duck);
        assert_eq!(gain_db_at(-18.0, 0.5), -9.0);
        assert!(is_full_mute(RANGE_FLOOR_DB) && !is_full_mute(-79.0));
    }

    /// An insert being set up replays its stored values before any audio:
    /// they land at once instead of gliding in.
    #[test]
    fn edits_before_the_first_sample_land_at_once() {
        let mut dsp = gate();
        assert!(dsp.apply_ui_param("rangeDb", -24.0));
        assert!(dsp.apply_ui_param("mode", 1.0));
        assert_eq!(dsp.coeffs.span_db, -24.0);
        assert_eq!(dsp.coeffs.duck, 1.0);
        let _ = dsp.process_stereo(0.0, 0.0);
        assert!(dsp.apply_ui_param("rangeDb", -12.0));
        assert_eq!(dsp.coeffs.span_db, -24.0);
    }
}
