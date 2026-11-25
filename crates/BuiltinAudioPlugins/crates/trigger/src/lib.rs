//! Trigger — a drum trigger and replacer.
//!
//! ```text
//!     ┌─ mono ─ high-pass ─ low-pass ─ peak follower ─ threshold / re-arm ─ peak window ─┐
//! in ─┤                                                                          hit + velocity
//!     └─ lookahead ─ dry (Blend only) ─────────────────── + ─── sample voices ◄───────────┘
//!                                                          └─ out
//! ```
//!
//! A hit is found on the undelayed input, and its sample starts where that hit
//! sits in the delayed dry signal (moved by Offset), so a replacement lands on
//! the original transient instead of a detector's reaction time after it. The
//! lookahead is fixed ([`LATENCY_MS`]) and reported as latency, so the host's
//! delay compensation keeps the track in time with the rest of the mix.
//!
//! The level of each hit — its peak over a short window after it crosses the
//! threshold — sets the velocity the sample plays at, by as much as Dynamics
//! says. The sample plays through Quick Sampler's voices (`quicksampler::Dsp`):
//! they own the decoded sample and its wait-free hand-off from the host's IPC
//! thread ([`Dsp::sample_loader`]), pitch, the fade and voice stealing.
//!
//! # Realtime shape
//!
//! Everything is sized in [`Dsp::new`]: the lookahead rings, the render
//! scratch, the scheduled-event table and the voice pool. The audio path
//! filters, compares, indexes and multiplies; a parameter edit retunes the
//! detector filters and hands the voices a new `Copy` parameter set.

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, clamp, db_to_linear, flush_denormal,
    linear_to_db, make_eq_coefficients, time_constant,
};
use quicksampler::{FilterMode, LoopMode, QuickSamplerParams, SampleLoader};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod synth;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.trigger";
/// The catalog name, which also names the plug-in's user folder (its Samples).
pub const PLUGIN_NAME: &str = "Trigger";

/// The fixed lookahead, in milliseconds: the latency the plug-in reports.
pub const LATENCY_MS: f32 = 5.0;
/// How long after crossing the threshold a hit's peak is measured.
pub const PEAK_WINDOW_MS: f32 = 1.5;
/// Offset's range: early enough to pre-empt a slow sample's attack, never
/// earlier than the lookahead leaves room for.
pub const OFFSET_MIN_MS: f32 = -3.0;
pub const OFFSET_MAX_MS: f32 = 20.0;
/// Length at its top plays the whole sample, whatever its length.
pub const LENGTH_FULL_MS: f32 = 2_000.0;
pub const MAX_VOICES: u32 = 8;
/// How far above the threshold a hit's level is followed, in dB: hits this
/// much louder than the threshold, or more, play at full level.
pub const DYNAMICS_WINDOW_DB: f32 = 24.0;

/// Frames rendered per inner chunk: the detector pass, then the voices.
const CHUNK: usize = 256;
/// Scheduled note-ons and note-offs the table holds.
const PENDING: usize = 32;
/// Voices cycle through this many note numbers, so a note-off reaches only
/// the hit it belongs to.
const NOTE_CYCLE: u8 = 64;
const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.120;
/// The detector's peak follower: quick, so two close hits read as two.
const DETECTOR_RELEASE_SECONDS: f32 = 0.010;
/// The detector reading the meter frame carries, held a little longer so the
/// telemetry rate does not miss a short hit.
const DETECTOR_DISPLAY_SECONDS: f32 = 0.080;
/// The detector re-arms once it falls this far under the threshold.
const REARM_HYSTERESIS_DB: f32 = 4.0;
/// How long a hit keeps its flag up in the meter frame: longer than one
/// telemetry poll, so the editor sees every hit.
const HIT_HOLD_SECONDS: f32 = 0.050;
/// The hit light's fall.
const FLASH_SECONDS: f32 = 0.150;
const SMOOTH_MS: f32 = 20.0;
/// The power crossfade.
const FADE_MS: f32 = 10.0;
/// Detector high-pass at or under this is off; low-pass at or over the next.
const HPF_OFF_HZ: f32 = 20.5;
const LPF_OFF_HZ: f32 = 19_950.0;
const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// The lowest sample rate a DSP is built for; anything under it is taken as
/// this.
const MIN_SAMPLE_RATE: f32 = 8_000.0;

/// A choice param: wire values are positions in `ALL`, the persisted
/// contract.
macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn to_wire(self) -> f32 {
                Self::ALL.iter().position(|v| *v == self).unwrap_or(0) as f32
            }

            pub fn from_wire(value: f32) -> Self {
                let index = if value.is_finite() {
                    value.round().max(0.0) as usize
                } else {
                    0
                };
                Self::ALL[index.min(Self::ALL.len() - 1)]
            }

            pub fn label(self) -> &'static str {
                match self {
                    $($name::$variant => $label),+
                }
            }
        }
    };
}

wire_enum!(
    /// What the output carries: the sample alone, or the sample over the
    /// track.
    Mode { Replace => "Replace", Blend => "Blend" }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    /// File name inside the plug-in's Samples folder. Persistence only: the
    /// audio reaches the DSP through [`Dsp::sample_loader`].
    pub sample_name: Option<String>,
    /// Hear the filtered detector instead of the output.
    pub listen: bool,
    pub hpf_hz: f32,
    pub lpf_hz: f32,
    pub threshold_db: f32,
    /// The shortest time between two hits.
    pub retrigger_ms: f32,
    /// Percent: how much of each hit's level the sample follows.
    pub dynamics: f32,
    /// Where the sample starts against the hit, ms (negative is earlier).
    pub offset_ms: f32,
    /// Semitones.
    pub pitch: f32,
    /// How long each hit plays; [`LENGTH_FULL_MS`] plays the whole sample.
    pub length_ms: f32,
    /// The fade a shortened hit ends on.
    pub fade_ms: f32,
    /// Hits that sound at once; 1 cuts the last hit off at the next.
    pub voices: u32,
    pub mode: Mode,
    /// The track's own signal under the sample, in Blend.
    pub dry_db: f32,
    pub sample_db: f32,
}

impl Default for Params {
    fn default() -> Self {
        default_params()
    }
}

pub fn default_params() -> Params {
    Params {
        power: true,
        sample_name: None,
        listen: false,
        hpf_hz: 20.0,
        lpf_hz: 20_000.0,
        threshold_db: -20.0,
        retrigger_ms: 60.0,
        dynamics: 75.0,
        offset_ms: 0.0,
        pitch: 0.0,
        length_ms: LENGTH_FULL_MS,
        fade_ms: 30.0,
        voices: 4,
        mode: Mode::Replace,
        dry_db: 0.0,
        sample_db: 0.0,
    }
}

impl Params {
    /// Whether every hit plays its whole sample.
    pub fn full_length(&self) -> bool {
        self.length_ms >= LENGTH_FULL_MS - 0.5
    }
}

/// The gain, in dB (0 or under), a hit whose detector peak is `peak_db` plays
/// at.
pub fn hit_gain_db(params: &Params, peak_db: f32) -> f32 {
    let above = clamp(peak_db - params.threshold_db, 0.0, DYNAMICS_WINDOW_DB);
    let depth = clamp(params.dynamics, 0.0, 100.0) / 100.0;
    -depth * (DYNAMICS_WINDOW_DB - above)
}

/// The MIDI velocity a hit's gain plays the voices at (their velocity
/// response is linear in level).
pub fn velocity_for_gain_db(gain_db: f32) -> u8 {
    (db_to_linear(gain_db) * 127.0).round().clamp(1.0, 127.0) as u8
}

/// The voices' settings for `params`: the sample at its own pitch moved by
/// Pitch, at unity level, every velocity point a level step.
pub fn voice_params(params: &Params) -> QuickSamplerParams {
    let pitch = clamp(params.pitch, -24.0, 24.0);
    let semitones = pitch.trunc();
    QuickSamplerParams {
        root_note: 60,
        transpose: semitones as i8,
        fine_cents: (pitch - semitones) * 100.0,
        keytrack: false,
        start: 0.0,
        end: 1.0,
        reverse: false,
        loop_mode: LoopMode::Off,
        loop_start: 0.0,
        loop_end: 1.0,
        normalize: false,
        attack_ms: 0.0,
        decay_ms: 0.0,
        sustain: 1.0,
        release_ms: params.fade_ms,
        filter: FilterMode::Off,
        cutoff_hz: quicksampler::CUTOFF_MAX_HZ,
        resonance: 0.0,
        volume: 1.0,
        pan: 0.0,
        velocity: 1.0,
        polyphony: params.voices.clamp(1, MAX_VOICES) as u8,
        pitch_bend_range: 0,
        one_shot: params.full_length(),
        glide_ms: 0.0,
    }
    .sanitized()
}

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

const PARAMS: &[ParamDescriptor] = &[
    param("power", "Power", 1.0, 0.0, 1.0, "bool"),
    param("listen", "Listen", 0.0, 0.0, 1.0, "bool"),
    param("hpfHz", "Detector High-Pass", 20.0, 20.0, 2_000.0, "Hz"),
    param(
        "lpfHz",
        "Detector Low-Pass",
        20_000.0,
        100.0,
        20_000.0,
        "Hz",
    ),
    param("thresholdDb", "Threshold", -20.0, -60.0, 0.0, "dB"),
    param("retriggerMs", "Retrigger", 60.0, 5.0, 500.0, "ms"),
    param("dynamics", "Dynamics", 75.0, 0.0, 100.0, "%"),
    param(
        "offsetMs",
        "Offset",
        0.0,
        OFFSET_MIN_MS,
        OFFSET_MAX_MS,
        "ms",
    ),
    param("pitch", "Pitch", 0.0, -24.0, 24.0, "st"),
    param(
        "lengthMs",
        "Length",
        LENGTH_FULL_MS,
        10.0,
        LENGTH_FULL_MS,
        "ms",
    ),
    param("fadeMs", "Fade", 30.0, 1.0, 500.0, "ms"),
    param("voices", "Voices", 4.0, 1.0, MAX_VOICES as f32, ""),
    param("mode", "Mode", 0.0, 0.0, 1.0, "enum"),
    param("dryDb", "Dry", 0.0, -48.0, 6.0, "dB"),
    param("sampleDb", "Sample", 0.0, -24.0, 12.0, "dB"),
];

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: PLUGIN_NAME,
        vendor: "Futureboard Digital Technologies",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: PARAMS,
    }
}

/// Telemetry the editor reads from the host's pad-level block.
pub mod telemetry {
    /// Slots in the block (the host's `BUILTIN_PAD_SLOTS`).
    pub const SLOTS: usize = 64;
    /// The detector's level now, dBFS.
    pub const DETECTOR_DB: usize = 0;
    /// 1 as a hit fires, falling back to 0.
    pub const HIT: usize = 1;
    /// The gain the last hit played at, dB (0 or under).
    pub const LAST_GAIN_DB: usize = 2;
    /// Hits fired since the DSP started, wrapping at a million.
    pub const HITS: usize = 3;
    /// 1 while a sample is loaded.
    pub const LOADED: usize = 4;
    /// Voices sounding now.
    pub const VOICES: usize = 5;
    /// The last hit's detector peak, dBFS.
    pub const LAST_PEAK_DB: usize = 6;
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// The detector's level (linear peak): what the host's meter frame
    /// carries in slot 0's input peak, so the editor's history records it.
    pub detector_peak: f32,
    /// True for a short while after each hit: slot 0's output flag in the
    /// host's meter frame, which the editor's history marks hits by.
    pub hit: bool,
    pub in_clip: bool,
    pub out_clip: bool,
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
        self.in_peak = 0.0;
        self.out_peak = 0.0;
        self.in_ms = 0.0;
        self.out_ms = 0.0;
        self.in_clip = false;
        self.out_clip = false;
    }

    #[inline]
    fn push(&mut self, in_l: f32, in_r: f32, out_l: f32, out_r: f32) {
        let in_abs = in_l.abs().max(in_r.abs());
        let out_abs = out_l.abs().max(out_r.abs());
        self.in_peak = if in_abs > self.in_peak {
            in_abs
        } else {
            flush_denormal(self.in_peak * self.peak_coeff)
        };
        self.out_peak = if out_abs > self.out_peak {
            out_abs
        } else {
            flush_denormal(self.out_peak * self.peak_coeff)
        };
        let input_ms = (in_l * in_l + in_r * in_r) * 0.5;
        let output_ms = (out_l * out_l + out_r * out_r) * 0.5;
        self.in_ms =
            flush_denormal(self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * input_ms);
        self.out_ms =
            flush_denormal(self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * output_ms);
        self.in_clip |= in_abs >= CLIP_THRESHOLD;
        self.out_clip |= out_abs >= CLIP_THRESHOLD;
    }
}

/// Pass-through biquad coefficients.
fn neutral() -> Coefficients<f32> {
    Coefficients {
        a1: 0.0,
        a2: 0.0,
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
    }
}

fn coefficients(kind: &str, hz: f32, sample_rate: f32) -> Coefficients<f32> {
    make_eq_coefficients(kind, hz, 0.0, BUTTERWORTH_Q, sample_rate).unwrap_or_else(neutral)
}

/// A delay of a fixed number of samples, sized once.
#[derive(Debug, Clone)]
struct Lookahead {
    ring: Vec<f32>,
    pos: usize,
}

impl Lookahead {
    fn new(delay: usize) -> Self {
        Self {
            ring: vec![0.0; delay.max(1)],
            pos: 0,
        }
    }

    fn clear(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
    }

    /// Pushes `x` and returns the sample from the ring's length ago.
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let out = self.ring[self.pos];
        self.ring[self.pos] = x;
        self.pos += 1;
        if self.pos == self.ring.len() {
            self.pos = 0;
        }
        out
    }
}

/// A note-on or note-off waiting for its sample.
#[derive(Debug, Clone, Copy, Default)]
struct Event {
    /// Input clock the event lands on.
    due: u64,
    note: u8,
    /// Velocity for a note-on; 0 for a note-off.
    velocity: u8,
    live: bool,
}

/// Fixed table of scheduled events. Full, a new note-on replaces nothing and
/// is dropped; a new note-off replaces the earliest one (its voice is the
/// oldest, and the first a busy pool steals).
#[derive(Debug, Clone)]
struct Schedule {
    events: [Event; PENDING],
}

impl Schedule {
    fn new() -> Self {
        Self {
            events: [Event::default(); PENDING],
        }
    }

    fn clear(&mut self) {
        self.events = [Event::default(); PENDING];
    }

    fn push(&mut self, event: Event) -> bool {
        if let Some(slot) = self.events.iter_mut().find(|e| !e.live) {
            *slot = event;
            return true;
        }
        if event.velocity == 0 {
            let earliest = self
                .events
                .iter_mut()
                .filter(|e| e.velocity == 0)
                .min_by_key(|e| e.due);
            if let Some(slot) = earliest {
                *slot = event;
                return true;
            }
        }
        false
    }

    /// Drops any note-off still waiting for `note`: the note is starting
    /// again for a newer hit.
    fn forget_off(&mut self, note: u8) {
        for event in &mut self.events {
            if event.live && event.velocity == 0 && event.note == note {
                event.live = false;
            }
        }
    }

    /// The earliest event due before `until`, taken out of the table.
    fn take_before(&mut self, until: u64) -> Option<Event> {
        let slot = self
            .events
            .iter_mut()
            .filter(|e| e.live && e.due < until)
            // Same sample: the note-off first, so a voice is let go before
            // the next one starts.
            .min_by_key(|e| (e.due, e.velocity != 0))?;
        slot.live = false;
        Some(*slot)
    }
}

/// The detector's state between samples.
#[derive(Debug, Clone, Copy)]
struct Detector {
    env: f32,
    display: f32,
    armed: bool,
    /// Samples until another hit may fire.
    holdoff: u32,
    /// Samples of the peak window still to measure; 0 when not measuring.
    capture_left: u32,
    capture_peak: f32,
    /// The input clock the hit being measured crossed the threshold at.
    crossed_at: u64,
}

impl Detector {
    const IDLE: Self = Self {
        env: 0.0,
        display: 0.0,
        armed: true,
        holdoff: 0,
        capture_left: 0,
        capture_peak: 0.0,
        crossed_at: 0,
    };
}

/// Values derived from the params and the sample rate.
#[derive(Debug, Clone, Copy)]
struct Derived {
    threshold: f32,
    rearm: f32,
    retrigger: u32,
    window: u32,
    /// Samples from crossing to the sample's start: lookahead plus offset.
    start_delay: u64,
    /// Samples from a hit's start to its note-off; `None` plays it whole.
    length: Option<u64>,
    hpf_on: bool,
    lpf_on: bool,
}

pub struct Dsp {
    params: Params,
    sample_rate: f32,
    voices: quicksampler::Dsp,
    hpf: DirectForm1<f32>,
    lpf: DirectForm1<f32>,
    derived: Derived,
    detector: Detector,
    schedule: Schedule,
    next_note: u8,
    /// Input samples processed since the DSP started.
    clock: u64,
    latency: usize,
    dry_l: Lookahead,
    dry_r: Lookahead,
    listen_ring: Lookahead,
    /// The listen signal over the chunk being rendered.
    listen_scratch: Vec<f32>,
    /// The voices' render target, interleaved stereo.
    wet: Vec<f32>,
    dry_gain: Smoothed,
    wet_gain: Smoothed,
    smooth_step: f32,
    power_fade: f32,
    fade_step: f32,
    detector_release: f32,
    display_release: f32,
    hit_hold: u32,
    hit_hold_samples: u32,
    flash: f32,
    flash_coeff: f32,
    hits: u32,
    last_gain_db: f32,
    last_peak_db: f32,
    meters: Meters,
}

impl std::fmt::Debug for Dsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dsp")
            .field("params", &self.params)
            .field("sample_rate", &self.sample_rate)
            .field("latency", &self.latency)
            .finish_non_exhaustive()
    }
}

fn ms_to_samples(ms: f32, sample_rate: f32) -> u32 {
    (ms.max(0.0) * 0.001 * sample_rate).round() as u32
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(MIN_SAMPLE_RATE);
        let params = default_params();
        let latency = ms_to_samples(LATENCY_MS, sample_rate) as usize;
        let mut voices = quicksampler::Dsp::new(sample_rate);
        voices.set_sampler_params(voice_params(&params));
        let mut dsp = Self {
            voices,
            sample_rate,
            hpf: DirectForm1::<f32>::new(neutral()),
            lpf: DirectForm1::<f32>::new(neutral()),
            derived: Derived {
                threshold: 0.0,
                rearm: 0.0,
                retrigger: 0,
                window: 1,
                start_delay: latency as u64,
                length: None,
                hpf_on: false,
                lpf_on: false,
            },
            detector: Detector::IDLE,
            schedule: Schedule::new(),
            next_note: 0,
            clock: 0,
            latency,
            dry_l: Lookahead::new(latency),
            dry_r: Lookahead::new(latency),
            listen_ring: Lookahead::new(latency),
            listen_scratch: vec![0.0; CHUNK],
            wet: vec![0.0; CHUNK * 2],
            dry_gain: Smoothed::at(0.0),
            wet_gain: Smoothed::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sample_rate),
            power_fade: 1.0,
            fade_step: 1.0 / (FADE_MS * 0.001 * sample_rate).max(1.0),
            detector_release: time_constant(sample_rate, DETECTOR_RELEASE_SECONDS),
            display_release: time_constant(sample_rate, DETECTOR_DISPLAY_SECONDS),
            hit_hold: 0,
            hit_hold_samples: ms_to_samples(HIT_HOLD_SECONDS * 1_000.0, sample_rate),
            flash: 0.0,
            flash_coeff: time_constant(sample_rate, FLASH_SECONDS),
            hits: 0,
            last_gain_db: 0.0,
            last_peak_db: -120.0,
            meters: Meters::new(sample_rate),
            params,
        };
        dsp.derive();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Restores persisted state. Control thread, before the DSP is published.
    pub fn set_params(&mut self, mut params: Params) {
        ipc::sanitize_params(&mut params);
        self.params = params;
        self.derive();
        self.settle();
    }

    /// Applies one wire edit. Allocation-free: a field write, filter
    /// coefficients and the voices' `Copy` settings.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        self.derive();
        true
    }

    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// The control-side handle the host submits decoded samples through.
    pub fn sample_loader(&self) -> SampleLoader {
        self.voices.sample_loader()
    }

    pub fn has_sample(&self) -> bool {
        self.voices.has_sample()
    }

    /// The fixed lookahead, in samples.
    pub fn latency_samples(&self) -> usize {
        self.latency
    }

    pub fn meter_frame(&self) -> MeterFrame {
        let m = &self.meters;
        MeterFrame {
            in_peak: m.in_peak,
            in_rms: m.in_ms.sqrt(),
            out_peak: m.out_peak,
            out_rms: m.out_ms.sqrt(),
            detector_peak: self.detector.display,
            hit: self.hit_hold > 0,
            in_clip: m.in_clip,
            out_clip: m.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    /// Hits fired since the DSP started.
    pub fn hit_count(&self) -> u32 {
        self.hits
    }

    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        let mut block = [0.0; telemetry::SLOTS];
        block[telemetry::DETECTOR_DB] = linear_to_db(self.detector.display).max(-120.0);
        block[telemetry::HIT] = self.flash;
        block[telemetry::LAST_GAIN_DB] = self.last_gain_db;
        block[telemetry::HITS] = (self.hits % 1_000_000) as f32;
        block[telemetry::LOADED] = f32::from(u8::from(self.voices.has_sample()));
        block[telemetry::VOICES] = self.voices.active_voice_count() as f32;
        block[telemetry::LAST_PEAK_DB] = self.last_peak_db;
        block
    }

    pub fn reset(&mut self) {
        self.hpf.reset_state();
        self.lpf.reset_state();
        self.detector = Detector::IDLE;
        self.schedule.clear();
        self.voices.all_notes_off();
        self.dry_l.clear();
        self.dry_r.clear();
        self.listen_ring.clear();
        self.hit_hold = 0;
        self.flash = 0.0;
        self.meters.reset();
        self.settle();
    }

    /// Recomputes everything the params set. Allocation-free.
    fn derive(&mut self) {
        let p = &self.params;
        let sr = self.sample_rate;
        let threshold = db_to_linear(p.threshold_db);
        let offset = (p.offset_ms.clamp(OFFSET_MIN_MS, OFFSET_MAX_MS) * 0.001 * sr).round() as i64;
        let window = ms_to_samples(PEAK_WINDOW_MS, sr).max(1);
        // Never before the hit is measured: the lookahead leaves room for
        // the whole of Offset's range, this only guards the arithmetic.
        let start_delay = (self.latency as i64 + offset).max(i64::from(window)) as u64;
        let hpf_on = p.hpf_hz > HPF_OFF_HZ;
        let lpf_on = p.lpf_hz < LPF_OFF_HZ;
        if hpf_on {
            self.hpf
                .update_coefficients(coefficients("highpass", p.hpf_hz, sr));
        }
        if lpf_on {
            self.lpf
                .update_coefficients(coefficients("lowpass", p.lpf_hz, sr));
        }
        self.derived = Derived {
            threshold,
            rearm: threshold * db_to_linear(-REARM_HYSTERESIS_DB),
            retrigger: ms_to_samples(p.retrigger_ms, sr).max(1),
            window,
            start_delay,
            length: (!p.full_length()).then(|| u64::from(ms_to_samples(p.length_ms, sr).max(1))),
            hpf_on,
            lpf_on,
        };
        self.dry_gain.target = match p.mode {
            Mode::Replace => 0.0,
            Mode::Blend => db_to_linear(p.dry_db),
        };
        self.wet_gain.target = db_to_linear(p.sample_db);
        self.voices.set_sampler_params(voice_params(p));
    }

    /// Jumps every smoothed value to where it is heading.
    fn settle(&mut self) {
        self.dry_gain.settle();
        self.wet_gain.settle();
        self.power_fade = if self.params.power { 1.0 } else { 0.0 };
    }

    /// One detector sample: the filtered mono signal, which the listen
    /// path plays.
    #[inline]
    fn detect(&mut self, left: f32, right: f32) -> f32 {
        let mut y = 0.5 * (left + right);
        if self.derived.hpf_on {
            y = self.hpf.run(y);
        }
        if self.derived.lpf_on {
            y = self.lpf.run(y);
        }
        let y = flush_denormal(y);
        let level = y.abs();
        let d = self.derived;
        let det = &mut self.detector;
        det.env = if level > det.env {
            level
        } else {
            flush_denormal(det.env * self.detector_release)
        };
        det.display = if level > det.display {
            level
        } else {
            flush_denormal(det.display * self.display_release)
        };
        det.holdoff = det.holdoff.saturating_sub(1);

        if det.capture_left > 0 {
            det.capture_peak = det.capture_peak.max(level);
            det.capture_left -= 1;
            if det.capture_left == 0 {
                let (peak, crossed_at) = (det.capture_peak, det.crossed_at);
                self.fire(peak, crossed_at);
            }
        } else if det.armed && det.holdoff == 0 && det.env >= d.threshold && d.threshold > 0.0 {
            det.armed = false;
            det.holdoff = d.retrigger;
            det.capture_left = d.window;
            det.capture_peak = level;
            det.crossed_at = self.clock;
        } else if !det.armed && det.env < d.rearm {
            det.armed = true;
        }
        y
    }

    /// A hit measured at `peak` that crossed at input clock `crossed_at`:
    /// its sample is scheduled where the hit sits in the delayed output.
    fn fire(&mut self, peak: f32, crossed_at: u64) {
        // Bypassed: the detector keeps reading (the display follows), but
        // nothing new plays.
        if !self.params.power {
            return;
        }
        let peak_db = linear_to_db(peak);
        let gain_db = hit_gain_db(&self.params, peak_db);
        let due = (crossed_at + self.derived.start_delay).max(self.clock);
        let note = self.next_note;
        self.next_note = (self.next_note + 1) % NOTE_CYCLE;
        let scheduled = self.schedule.push(Event {
            due,
            note,
            velocity: velocity_for_gain_db(gain_db),
            live: true,
        });
        if !scheduled {
            return;
        }
        self.hits = self.hits.wrapping_add(1);
        self.last_gain_db = gain_db;
        self.last_peak_db = peak_db;
        self.flash = 1.0;
        self.hit_hold = self.hit_hold_samples;
    }

    /// Plays one event on the voices.
    fn play(&mut self, event: Event) {
        if event.velocity == 0 {
            self.voices.note_off(event.note);
            return;
        }
        self.schedule.forget_off(event.note);
        self.voices.note_on(event.note, event.velocity);
        if let Some(length) = self.derived.length {
            let _ = self.schedule.push(Event {
                due: event.due + length,
                note: event.note,
                velocity: 0,
                live: true,
            });
        }
    }

    /// Renders the voices over `frames` of the chunk that starts at input
    /// clock `start`, playing every event due inside it on its own sample.
    fn render_voices(&mut self, start: u64, frames: usize) {
        let end = start + frames as u64;
        let mut at = 0usize;
        loop {
            let event = self.schedule.take_before(end);
            let until = event.map_or(frames, |e| {
                (e.due.saturating_sub(start) as usize).min(frames)
            });
            if until > at {
                self.voices
                    .process_block(&mut self.wet[at * 2..until * 2], until - at);
                at = until;
            }
            match event {
                Some(event) => self.play(event),
                None => break,
            }
        }
    }

    /// Processes one block: `in_l`/`in_r` in, interleaved stereo out.
    pub fn process_block(&mut self, in_l: &[f32], in_r: &[f32], out: &mut [f32], frames: usize) {
        let frames = frames.min(in_l.len()).min(in_r.len()).min(out.len() / 2);
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(CHUNK);
            let start = self.clock;
            // The detector runs ahead over the whole chunk, so every hit due
            // inside it is scheduled before the voices render it.
            for i in 0..n {
                let (l, r) = (in_l[done + i], in_r[done + i]);
                let (l, r) = if l.is_finite() && r.is_finite() {
                    (l, r)
                } else {
                    (0.0, 0.0)
                };
                let y = self.detect(l, r);
                self.listen_scratch[i] = y;
                self.clock += 1;
            }
            self.render_voices(start, n);
            for i in 0..n {
                let (in_l, in_r) = (in_l[done + i], in_r[done + i]);
                let (in_l, in_r) = if in_l.is_finite() && in_r.is_finite() {
                    (in_l, in_r)
                } else {
                    (0.0, 0.0)
                };
                let dry_l = self.dry_l.run(in_l);
                let dry_r = self.dry_r.run(in_r);
                let listen = self.listen_ring.run(self.listen_scratch[i]);
                let dry_gain = self.dry_gain.next(self.smooth_step);
                let wet_gain = self.wet_gain.next(self.smooth_step);
                let (wet_l, wet_r) = (self.wet[i * 2], self.wet[i * 2 + 1]);
                let (mut l, mut r) = if self.params.listen {
                    (listen, listen)
                } else {
                    (
                        dry_l * dry_gain + wet_l * wet_gain,
                        dry_r * dry_gain + wet_r * wet_gain,
                    )
                };
                let target = if self.params.power { 1.0 } else { 0.0 };
                if self.power_fade != target {
                    self.power_fade = if self.power_fade < target {
                        (self.power_fade + self.fade_step).min(1.0)
                    } else {
                        (self.power_fade - self.fade_step).max(0.0)
                    };
                }
                if self.power_fade < 1.0 {
                    // Bypassed, the track passes through at the reported
                    // latency, so it stays in time.
                    let t = self.power_fade;
                    l = dry_l + (l - dry_l) * t;
                    r = dry_r + (r - dry_r) * t;
                }
                let (l, r) = (flush_denormal(l), flush_denormal(r));
                out[(done + i) * 2] = l;
                out[(done + i) * 2 + 1] = r;
                self.meters.push(in_l, in_r, l, r);
                self.flash = flush_denormal(self.flash * self.flash_coeff);
                self.hit_hold = self.hit_hold.saturating_sub(1);
            }
            done += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const SR: f32 = 48_000.0;

    /// A 2 ms full-scale blip: one sample at 1, the rest silent. Easy to
    /// find in the output.
    fn click_sample() -> Arc<quicksampler::SampleData> {
        let mut samples = vec![0.0; 96];
        samples[0] = 1.0;
        Arc::new(quicksampler::SampleData::from_interleaved(&samples, 1, SR as u32).unwrap())
    }

    fn loaded(params: Params) -> Dsp {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        dsp.sample_loader().submit(Some(click_sample()));
        // Adopted at the next block.
        let mut out = vec![0.0; 64 * 2];
        dsp.process_block(&[0.0; 64], &[0.0; 64], &mut out, 64);
        dsp
    }

    /// Runs `input` (mono, both sides) through in blocks of `block` and
    /// returns the left output.
    fn run(dsp: &mut Dsp, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        for chunk in input.chunks(block) {
            let mut buffer = vec![0.0; chunk.len() * 2];
            dsp.process_block(chunk, chunk, &mut buffer, chunk.len());
            out.extend(buffer.chunks(2).map(|pair| pair[0]));
        }
        out
    }

    /// Silence with a decaying burst at each of `at`, `level` peak.
    fn hits(len: usize, at: &[usize], level: f32) -> Vec<f32> {
        let mut input = vec![0.0; len];
        for &start in at {
            for i in 0..2_000.min(len - start) {
                let env = level * (-(i as f32) / 300.0).exp();
                input[start + i] += env * (i as f32 * 0.13).sin().signum();
            }
        }
        input
    }

    fn first_onset(output: &[f32], from: usize) -> Option<usize> {
        output[from..]
            .iter()
            .position(|v| v.abs() > 1.0e-3)
            .map(|i| i + from)
    }

    #[test]
    fn the_sample_lands_on_the_hit_in_the_delayed_output() {
        let mut dsp = loaded(default_params());
        let latency = dsp.latency_samples();
        assert_eq!(latency, 240);
        let input = hits(8_000, &[1_000], 0.8);
        let out = run(&mut dsp, &input, 128);
        // The input crossed the threshold on its first sample; replaced, the
        // output's first sound is the click at that sample plus the latency.
        let onset = first_onset(&out, 0).expect("the hit played");
        assert_eq!(onset, 1_000 + latency);
        assert_eq!(dsp.hit_count(), 1);
    }

    #[test]
    fn block_size_does_not_move_the_hit() {
        let input = hits(20_000, &[3_000, 9_500], 0.8);
        let onsets: Vec<Vec<usize>> = [1usize, 37, 256, 1_024]
            .into_iter()
            .map(|block| {
                let mut dsp = loaded(default_params());
                let out = run(&mut dsp, &input, block);
                let first = first_onset(&out, 0).unwrap();
                let second = first_onset(&out, first + 500).unwrap();
                vec![first, second]
            })
            .collect();
        assert!(onsets.windows(2).all(|w| w[0] == w[1]), "{onsets:?}");
        assert_eq!(onsets[0], vec![3_240, 9_740]);
    }

    #[test]
    fn offset_moves_the_sample_against_the_hit() {
        let input = hits(8_000, &[1_000], 0.8);
        let mut early = loaded(Params {
            offset_ms: -2.0,
            ..default_params()
        });
        let mut late = loaded(Params {
            offset_ms: 10.0,
            ..default_params()
        });
        assert_eq!(
            first_onset(&run(&mut early, &input, 64), 0),
            Some(1_240 - 96)
        );
        assert_eq!(
            first_onset(&run(&mut late, &input, 64), 0),
            Some(1_240 + 480)
        );
    }

    #[test]
    fn quiet_input_under_the_threshold_plays_nothing() {
        let mut dsp = loaded(default_params());
        let out = run(&mut dsp, &hits(8_000, &[1_000], 0.05), 128);
        assert_eq!(dsp.hit_count(), 0);
        assert!(out.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn retrigger_holds_off_a_second_hit_too_close() {
        let input = hits(30_000, &[1_000, 1_000 + 1_440, 1_000 + 9_600], 0.8);
        // 1 440 samples is 30 ms: inside the 60 ms default, and the
        // detector has fallen and re-armed by then.
        let mut dsp = loaded(default_params());
        run(&mut dsp, &input, 256);
        assert_eq!(dsp.hit_count(), 2);
        let mut quick = loaded(Params {
            retrigger_ms: 10.0,
            ..default_params()
        });
        run(&mut quick, &input, 256);
        assert_eq!(quick.hit_count(), 3);
    }

    #[test]
    fn dynamics_follow_the_hit_level() {
        let params = default_params();
        // −20 dB threshold: a hit at −20 dBFS sits at the bottom of the
        // window, one at +4 dBFS (24 dB over) plays at full level.
        assert_eq!(hit_gain_db(&params, 4.0), 0.0);
        assert!((hit_gain_db(&params, -8.0) - (-0.75 * 12.0)).abs() < 1.0e-4);
        let flat = Params {
            dynamics: 0.0,
            ..params
        };
        assert_eq!(hit_gain_db(&flat, -19.0), 0.0);
        assert_eq!(velocity_for_gain_db(0.0), 127);
        assert_eq!(velocity_for_gain_db(-120.0), 1);

        let peak_of = |level: f32| {
            let mut dsp = loaded(default_params());
            run(&mut dsp, &hits(8_000, &[1_000], level), 128)
                .iter()
                .fold(0.0f32, |peak, v| peak.max(v.abs()))
        };
        let (soft, loud) = (peak_of(0.2), peak_of(0.9));
        assert!(soft > 0.0 && loud > soft * 2.0, "soft {soft} loud {loud}");
    }

    #[test]
    fn blend_keeps_the_track_under_the_sample() {
        let input = hits(8_000, &[1_000], 0.8);
        let mut replace = loaded(default_params());
        let out = run(&mut replace, &input, 128);
        // Replaced: between the click and the next hit, silence.
        assert!(out[1_400..4_000].iter().all(|v| v.abs() < 1.0e-6));
        let mut blend = loaded(Params {
            mode: Mode::Blend,
            ..default_params()
        });
        let out = run(&mut blend, &input, 128);
        let latency = blend.latency_samples();
        for i in [1_100usize, 1_500, 2_000] {
            assert!((out[i + latency] - input[i]).abs() < 1.0e-4, "at {i}");
        }
    }

    #[test]
    fn bypass_is_the_track_at_the_reported_latency() {
        let input = hits(8_000, &[1_000], 0.8);
        let mut dsp = loaded(Params {
            power: false,
            ..default_params()
        });
        let out = run(&mut dsp, &input, 128);
        let latency = dsp.latency_samples();
        for i in 0..input.len() - latency {
            assert!((out[i + latency] - input[i]).abs() < 1.0e-6);
        }
        assert_eq!(dsp.hit_count(), 0);
    }

    #[test]
    fn a_short_length_ends_the_hit_with_its_fade() {
        let long_sample: Vec<f32> = vec![0.5; 48_000];
        let sample = Arc::new(
            quicksampler::SampleData::from_interleaved(&long_sample, 1, SR as u32).unwrap(),
        );
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            length_ms: 100.0,
            fade_ms: 5.0,
            ..default_params()
        });
        dsp.sample_loader().submit(Some(sample));
        let out = run(&mut dsp, &hits(24_000, &[1_000], 0.8), 128);
        let start = 1_240;
        assert!(out[start + 2_400].abs() > 0.1, "plays through its length");
        assert!(out[start + 4_800 + 1_000].abs() < 1.0e-4, "then fades away");
    }

    #[test]
    fn no_sample_still_counts_hits_and_stays_silent() {
        let mut dsp = Dsp::new(SR);
        let out = run(&mut dsp, &hits(8_000, &[1_000], 0.8), 128);
        assert_eq!(dsp.hit_count(), 1);
        assert!(out.iter().all(|v| *v == 0.0));
        assert_eq!(dsp.telemetry()[telemetry::LOADED], 0.0);
    }

    #[test]
    fn listen_plays_the_filtered_detector() {
        let mut dsp = loaded(Params {
            listen: true,
            hpf_hz: 2_000.0,
            ..default_params()
        });
        // A 50 Hz sine: the 2 kHz high-pass takes nearly all of it.
        let input: Vec<f32> = (0..9_600)
            .map(|i| 0.5 * (i as f32 * std::f32::consts::TAU * 50.0 / SR).sin())
            .collect();
        let out = run(&mut dsp, &input, 256);
        let peak = out[4_800..].iter().fold(0.0f32, |p, v| p.max(v.abs()));
        assert!(peak < 0.01, "{peak}");
    }

    #[test]
    fn telemetry_reports_the_last_hit() {
        let mut dsp = loaded(default_params());
        run(&mut dsp, &hits(8_000, &[1_000], 0.8), 128);
        let block = dsp.telemetry();
        assert_eq!(block[telemetry::HITS], 1.0);
        assert_eq!(block[telemetry::LOADED], 1.0);
        assert!(block[telemetry::LAST_PEAK_DB] > -3.0);
        assert!(block[telemetry::LAST_GAIN_DB] <= 0.0);
        let frame = dsp.meter_frame();
        assert!(frame.in_peak > 0.0);
    }

    #[test]
    fn output_stays_finite() {
        let mut dsp = loaded(default_params());
        let mut input = hits(4_000, &[100], 0.8);
        input[200] = f32::NAN;
        input[300] = f32::INFINITY;
        let out = run(&mut dsp, &input, 64);
        assert!(out.iter().all(|v| v.is_finite()));
    }
}
