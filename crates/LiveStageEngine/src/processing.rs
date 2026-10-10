//! Every strip's own processing section — what a console has on each channel
//! before its insert points: high-pass, gate, four-band EQ, compressor and
//! delay. Plain settings (saved with the session) and the realtime processor
//! that runs them.
//!
//! Signal flow on a strip: input → trim → polarity → **HPF → Gate → EQ ↔ Comp
//! (order selectable) → Delay** → inserts → fader → pan. Buses and the master
//! run the same section on their sum.

use serde::{Deserialize, Serialize};

/// The section's settings. `Copy`, so a change travels to the audio thread
/// whole through a bounded channel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Processing {
    pub hpf: Hpf,
    pub gate: Gate,
    pub eq: Eq,
    pub comp: Comp,
    pub delay: Delay,
    pub order: ProcessingOrder,
}

impl Default for Processing {
    fn default() -> Self {
        Self {
            hpf: Hpf::default(),
            gate: Gate::default(),
            eq: Eq::default(),
            comp: Comp::default(),
            delay: Delay::default(),
            order: ProcessingOrder::EqThenComp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingOrder {
    /// HPF → Gate → EQ → Comp: the usual channel.
    #[default]
    EqThenComp,
    /// HPF → Gate → Comp → EQ.
    CompThenEq,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hpf {
    pub on: bool,
    /// 20 … 600 Hz.
    pub hz: f32,
    /// 12, 18 or 24 dB/oct.
    pub slope_db: u8,
}

impl Default for Hpf {
    fn default() -> Self {
        Self {
            on: false,
            hz: 80.0,
            slope_db: 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gate {
    pub on: bool,
    /// −80 … 0 dB.
    pub threshold_db: f32,
    /// Attenuation when closed, −80 (a full mute) … 0 dB.
    pub range_db: f32,
    /// 0.05 … 100 ms.
    pub attack_ms: f32,
    /// 0 … 2000 ms.
    pub hold_ms: f32,
    /// 5 … 4000 ms.
    pub release_ms: f32,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            on: false,
            threshold_db: -50.0,
            range_db: -80.0,
            attack_ms: 0.5,
            hold_ms: 20.0,
            release_ms: 150.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EqKind {
    LowShelf,
    Bell,
    HighShelf,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqBand {
    pub kind: EqKind,
    /// 20 … 20 000 Hz.
    pub hz: f32,
    /// −18 … +18 dB.
    pub gain_db: f32,
    /// 0.1 … 10.
    pub q: f32,
}

impl Default for EqBand {
    fn default() -> Self {
        Self {
            kind: EqKind::Bell,
            hz: 1_000.0,
            gain_db: 0.0,
            q: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Eq {
    pub on: bool,
    /// Low, low-mid, high-mid, high.
    pub bands: [EqBand; EQ_BANDS],
}

pub const EQ_BANDS: usize = 4;

impl Default for Eq {
    fn default() -> Self {
        let band = |kind, hz, q| EqBand {
            kind,
            hz,
            gain_db: 0.0,
            q,
        };
        Self {
            on: true,
            bands: [
                band(EqKind::LowShelf, 100.0, 0.7),
                band(EqKind::Bell, 400.0, 1.0),
                band(EqKind::Bell, 2_500.0, 1.0),
                band(EqKind::HighShelf, 8_000.0, 0.7),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Comp {
    pub on: bool,
    /// −60 … 0 dB.
    pub threshold_db: f32,
    /// 1 … 20 (20 is a limiter).
    pub ratio: f32,
    /// 0.1 … 200 ms.
    pub attack_ms: f32,
    /// 10 … 2000 ms.
    pub release_ms: f32,
    /// 0 … 24 dB.
    pub knee_db: f32,
    /// 0 … 24 dB.
    pub makeup_db: f32,
}

impl Default for Comp {
    fn default() -> Self {
        Self {
            on: false,
            threshold_db: -20.0,
            ratio: 3.0,
            attack_ms: 10.0,
            release_ms: 150.0,
            knee_db: 6.0,
            makeup_db: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Delay {
    pub on: bool,
    /// 0 … [`MAX_DELAY_MS`] ms.
    pub ms: f32,
}

impl Default for Delay {
    fn default() -> Self {
        Self { on: false, ms: 0.0 }
    }
}

pub const MAX_DELAY_MS: f32 = 1_000.0;

/// What the section reports for the strip's meters.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct ProcessingMeters {
    /// The gate is open (or off).
    pub gate_open: bool,
    /// What the gate takes off now, dB, ≥ 0.
    pub gate_db: f32,
    /// What the compressor takes off now, dB, ≥ 0.
    pub comp_db: f32,
}

// ---------------------------------------------------------------------------
// The realtime processor
// ---------------------------------------------------------------------------

/// Every block is cut into runs of at most this many frames; the gliding
/// filter values are turned into coefficients once per run.
const SUB: usize = 16;
/// On/off switches, order and slope changes and delay-tap moves crossfade
/// over this long.
const FADE_MS: f32 = 10.0;
/// Time constant of every continuous value's glide.
const GLIDE_MS: f32 = 15.0;
/// The gate opens at its threshold and closes this far (as a gain) below it:
/// −4 dB.
const GATE_HYSTERESIS: f32 = 0.630_957_3;
/// Fall time constant of the gate's peak detector.
const GATE_DETECT_MS: f32 = 10.0;
/// A gate range at (or under) this is a full mute: shut, the gain is 0.
const GATE_FLOOR_DB: f32 = -80.0;
/// An EQ band at 0 dB whose filter memory has decayed under this stops
/// running (−180 dB).
const QUIET: f64 = 1.0e-9;
/// The highest filter frequency, as a fraction of the sample rate.
const MAX_FILTER_FRACTION: f32 = 0.45;
const DB_TO_LOG2: f32 = 0.166_096_4; // log2(10) / 20
const LOG2_TO_DB: f32 = 6.020_6; // 20 · log10(2)

#[inline]
fn db_to_lin(db: f32) -> f32 {
    (db * DB_TO_LOG2).exp2()
}

/// Smoothstep: 0 → 0, 1 → 1, flat at both ends.
#[inline]
fn ease(pos: f32) -> f32 {
    pos * pos * (3.0 - 2.0 * pos)
}

/// `dry` at weight 0 (exactly), the processed `wet` at 1, a straight blend
/// between.
#[inline]
fn mix_in(dry: &[f32], wet: &mut [f32], weight: &[f32]) {
    for ((y, &d), &w) in wet.iter_mut().zip(dry).zip(weight) {
        *y = d + (*y - d) * w;
    }
}

/// `value` held to `min ..= max`; NaN becomes `fallback`.
#[inline]
fn clamp_or(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_nan() {
        fallback
    } else {
        value.clamp(min, max)
    }
}

/// The settings held to their documented ranges (NaN → the default).
fn sanitize(settings: &Processing) -> Processing {
    let d = Processing::default();
    let mut s = *settings;
    s.hpf.hz = clamp_or(s.hpf.hz, 20.0, 600.0, d.hpf.hz);
    s.hpf.slope_db = match s.hpf.slope_db {
        0..=15 => 12,
        16..=21 => 18,
        _ => 24,
    };
    let g = &mut s.gate;
    g.threshold_db = clamp_or(g.threshold_db, -80.0, 0.0, d.gate.threshold_db);
    g.range_db = clamp_or(g.range_db, GATE_FLOOR_DB, 0.0, d.gate.range_db);
    g.attack_ms = clamp_or(g.attack_ms, 0.05, 100.0, d.gate.attack_ms);
    g.hold_ms = clamp_or(g.hold_ms, 0.0, 2_000.0, d.gate.hold_ms);
    g.release_ms = clamp_or(g.release_ms, 5.0, 4_000.0, d.gate.release_ms);
    for (band, def) in s.eq.bands.iter_mut().zip(d.eq.bands) {
        band.hz = clamp_or(band.hz, 20.0, 20_000.0, def.hz);
        band.gain_db = clamp_or(band.gain_db, -18.0, 18.0, 0.0);
        band.q = clamp_or(band.q, 0.1, 10.0, def.q);
    }
    let c = &mut s.comp;
    c.threshold_db = clamp_or(c.threshold_db, -60.0, 0.0, d.comp.threshold_db);
    c.ratio = clamp_or(c.ratio, 1.0, 20.0, d.comp.ratio);
    c.attack_ms = clamp_or(c.attack_ms, 0.1, 200.0, d.comp.attack_ms);
    c.release_ms = clamp_or(c.release_ms, 10.0, 2_000.0, d.comp.release_ms);
    c.knee_db = clamp_or(c.knee_db, 0.0, 24.0, d.comp.knee_db);
    c.makeup_db = clamp_or(c.makeup_db, 0.0, 24.0, d.comp.makeup_db);
    s.delay.ms = clamp_or(s.delay.ms, 0.0, MAX_DELAY_MS, 0.0);
    s
}

/// Per-sample-rate constants.
#[derive(Debug, Clone, Copy)]
struct Ctx {
    sr: f32,
    /// One frame's step of a [`GLIDE_MS`] one-pole.
    k_sample: f32,
    /// A full [`SUB`]-frame run's step of the same one-pole.
    k_run: f32,
    /// Per-frame step of a [`FADE_MS`] crossfade.
    fade_step: f32,
}

impl Ctx {
    fn new(sr: f32) -> Self {
        let tau = GLIDE_MS * 1.0e-3 * sr;
        Self {
            sr,
            k_sample: 1.0 - (-1.0 / tau).exp(),
            k_run: 1.0 - (-(SUB as f32) / tau).exp(),
            fade_step: 1.0 / (FADE_MS * 1.0e-3 * sr).max(1.0),
        }
    }

    /// The glide step for a run of `n` frames.
    #[inline]
    fn k_for(&self, n: usize) -> f32 {
        if n == SUB {
            self.k_run
        } else {
            1.0 - (1.0 - self.k_sample).powi(n as i32)
        }
    }

    /// Angular frequency, held under Nyquist.
    #[inline]
    fn w0(&self, hz: f32) -> f64 {
        let hz = hz.min(self.sr * MAX_FILTER_FRACTION) as f64;
        std::f64::consts::TAU * hz / self.sr as f64
    }

    fn samples(&self, ms: f32) -> f32 {
        ms * 1.0e-3 * self.sr
    }
}

/// A continuous value gliding toward its target (a one-pole); lands exactly
/// on the target once within `eps`.
#[derive(Debug, Clone, Copy)]
struct Glide {
    value: f32,
    target: f32,
    eps: f32,
}

impl Glide {
    fn new(value: f32, eps: f32) -> Self {
        Self {
            value,
            target: value,
            eps,
        }
    }

    #[inline]
    fn moving(&self) -> bool {
        self.value != self.target
    }

    #[inline]
    fn tick(&mut self, k: f32) -> f32 {
        let d = self.target - self.value;
        if d.abs() <= self.eps {
            self.value = self.target;
        } else {
            self.value += d * k;
        }
        self.value
    }

    fn settle(&mut self) {
        self.value = self.target;
    }
}

/// A 0 … 1 crossfade position: moves linearly toward its target over
/// [`FADE_MS`] and is read eased, so neither end has a corner. Lands exactly
/// on 0 or 1.
#[derive(Debug, Clone, Copy)]
struct Fade {
    pos: f32,
    target: f32,
}

impl Fade {
    fn new(on: bool) -> Self {
        let at = if on { 1.0 } else { 0.0 };
        Self {
            pos: at,
            target: at,
        }
    }

    /// From fully off toward fully on: a fresh crossfade.
    fn start() -> Self {
        Self {
            pos: 0.0,
            target: 1.0,
        }
    }

    fn set(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
    }

    fn settle(&mut self) {
        self.pos = self.target;
    }

    /// Off and done fading: the section can be skipped.
    #[inline]
    fn idle(&self) -> bool {
        self.pos <= 0.0 && self.target <= 0.0
    }

    /// On and done fading: no dry blend needed.
    #[inline]
    fn full(&self) -> bool {
        self.pos >= 1.0 && self.target >= 1.0
    }

    /// The eased weight of the "on" side for each of the next frames.
    #[inline]
    fn fill(&mut self, step: f32, weights: &mut [f32]) {
        for w in weights {
            if self.pos < self.target {
                self.pos = (self.pos + step).min(self.target);
            } else if self.pos > self.target {
                self.pos = (self.pos - step).max(self.target);
            }
            *w = ease(self.pos);
        }
    }
}

// --- Biquads -----------------------------------------------------------------

/// Normalised biquad coefficients (f64: a 20 Hz high-pass at 192 kHz puts
/// its poles too close to 1 for f32).
#[derive(Debug, Clone, Copy)]
struct Coefs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coefs {
    const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// Divides (not multiplies by the reciprocal) so that `b == a` gives an
    /// exact identity.
    fn normalized(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// RBJ second-order high-pass.
    fn highpass(w0: f64, q: f64) -> Self {
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b0 = (1.0 + cos) * 0.5;
        Self::normalized(b0, -(1.0 + cos), b0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    /// Bilinear first-order high-pass (as a biquad with `b2 = a2 = 0`).
    fn highpass_first_order(w0: f64) -> Self {
        let k = (w0 * 0.5).tan();
        let b0 = 1.0 / (1.0 + k);
        Self {
            b0,
            b1: -b0,
            b2: 0.0,
            a1: (k - 1.0) / (k + 1.0),
            a2: 0.0,
        }
    }

    /// RBJ low shelf / peaking / high shelf.
    fn eq(kind: EqKind, w0: f64, gain_db: f64, q: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        match kind {
            EqKind::Bell => Self::normalized(
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            EqKind::LowShelf => {
                let sa = 2.0 * a.sqrt() * alpha;
                Self::normalized(
                    a * ((a + 1.0) - (a - 1.0) * cos + sa),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - sa),
                    (a + 1.0) + (a - 1.0) * cos + sa,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - sa,
                )
            }
            EqKind::HighShelf => {
                let sa = 2.0 * a.sqrt() * alpha;
                Self::normalized(
                    a * ((a + 1.0) + (a - 1.0) * cos + sa),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - sa),
                    (a + 1.0) - (a - 1.0) * cos + sa,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - sa,
                )
            }
        }
    }
}

/// One channel's transposed-direct-form-II memory.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    s1: f64,
    s2: f64,
}

impl Biquad {
    #[inline]
    fn tick(&mut self, c: &Coefs, x: f64) -> f64 {
        let y = c.b0 * x + self.s1;
        self.s1 = c.b1 * x - c.a1 * y + self.s2;
        self.s2 = c.b2 * x - c.a2 * y;
        y
    }

    #[inline]
    fn run(&mut self, c: &Coefs, buf: &mut [f32]) {
        for x in buf {
            *x = self.tick(c, *x as f64) as f32;
        }
    }

    /// Snaps decayed memory to zero before it goes subnormal, and drops
    /// memory a non-finite input poisoned.
    #[inline]
    fn flush(&mut self) {
        let keep = |s: f64| s.abs() > 1.0e-30 && s.abs() < 1.0e30;
        if !keep(self.s1) {
            self.s1 = 0.0;
        }
        if !keep(self.s2) {
            self.s2 = 0.0;
        }
    }

    #[inline]
    fn quiet(&self) -> bool {
        self.s1.abs() < QUIET && self.s2.abs() < QUIET
    }
}

// --- HPF ---------------------------------------------------------------------

/// A Butterworth high-pass of one slope: 12 = one biquad; 18 = first order
/// plus a biquad (Q 1); 24 = two biquads (Q 0.541, 1.307).
#[derive(Debug, Clone, Copy)]
struct HpfChain {
    slope: u8,
    coefs: [Coefs; 2],
    /// `[channel][stage]`.
    state: [[Biquad; 2]; 2],
}

impl HpfChain {
    fn new(slope: u8) -> Self {
        Self {
            slope,
            coefs: [Coefs::IDENTITY; 2],
            state: Default::default(),
        }
    }

    fn stages(&self) -> usize {
        if self.slope <= 12 { 1 } else { 2 }
    }

    fn design(&mut self, w0: f64) {
        match self.slope {
            0..=12 => self.coefs[0] = Coefs::highpass(w0, std::f64::consts::FRAC_1_SQRT_2),
            13..=18 => {
                self.coefs[0] = Coefs::highpass_first_order(w0);
                self.coefs[1] = Coefs::highpass(w0, 1.0);
            }
            _ => {
                self.coefs[0] = Coefs::highpass(w0, 0.541_196_100_146_197);
                self.coefs[1] = Coefs::highpass(w0, 1.306_562_964_876_376_6);
            }
        }
    }

    #[inline]
    fn run(&mut self, ch: usize, buf: &mut [f32]) {
        for stage in 0..self.stages() {
            self.state[ch][stage].run(&self.coefs[stage], buf);
        }
    }

    fn flush(&mut self) {
        for s in self.state.iter_mut().flatten() {
            s.flush();
        }
    }

    fn clear(&mut self) {
        self.state = Default::default();
    }
}

struct HpfSection {
    fade: Fade,
    log_hz: Glide,
    /// The slope asked for.
    slope: u8,
    /// The running chain and, while a slope change crossfades, the old one.
    chains: [HpfChain; 2],
    active: usize,
    /// Weight of `chains[active]` against the other; full when not swapping.
    swap: Fade,
    dirty: bool,
}

impl HpfSection {
    fn new(s: &Hpf) -> Self {
        Self {
            fade: Fade::new(s.on),
            log_hz: Glide::new(s.hz.log2(), 1.0e-5),
            slope: s.slope_db,
            chains: [HpfChain::new(s.slope_db); 2],
            active: 0,
            swap: Fade::new(true),
            dirty: true,
        }
    }

    fn design(&mut self, ctx: &Ctx) {
        let w0 = ctx.w0(self.log_hz.value.exp2());
        self.chains[self.active].design(w0);
        if !self.swap.full() {
            self.chains[self.active ^ 1].design(w0);
        }
        self.dirty = false;
    }

    fn settle(&mut self, ctx: &Ctx) {
        self.fade.settle();
        self.log_hz.settle();
        self.swap.settle();
        if self.chains[self.active].slope != self.slope {
            self.chains[self.active] = HpfChain::new(self.slope);
        }
        if self.fade.idle() {
            self.chains[self.active].clear();
        }
        self.design(ctx);
    }

    fn run(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32], stereo: bool) {
        if self.fade.idle() {
            // Off: nothing runs. Land the values so a later switch-on starts
            // where it is set.
            self.log_hz.settle();
            self.swap.settle();
            if self.chains[self.active].slope != self.slope {
                self.chains[self.active] = HpfChain::new(self.slope);
            }
            self.dirty = true;
            return;
        }
        let n = l.len();
        if self.swap.full() && self.chains[self.active].slope != self.slope {
            if self.fade.pos <= 0.0 {
                // Nothing of the filter is heard yet: change it outright.
                self.chains[self.active] = HpfChain::new(self.slope);
            } else {
                self.active ^= 1;
                self.chains[self.active] = HpfChain::new(self.slope);
                self.swap = Fade::start();
            }
            self.dirty = true;
        }
        if self.log_hz.moving() {
            self.log_hz.tick(ctx.k_for(n));
            self.dirty = true;
        }
        if self.dirty {
            self.design(ctx);
        }
        let fading = !self.fade.full();
        let swapping = !self.swap.full();
        let mut weights = [1.0f32; SUB];
        if fading {
            self.fade.fill(ctx.fade_step, &mut weights[..n]);
        }
        let mut swap_weights = [1.0f32; SUB];
        if swapping {
            self.swap.fill(ctx.fade_step, &mut swap_weights[..n]);
        }
        let (active, old) = (self.active, self.active ^ 1);
        for ch in 0..1 + stereo as usize {
            let buf: &mut [f32] = if ch == 0 { &mut *l } else { &mut *r };
            let mut dry = [0.0f32; SUB];
            dry[..n].copy_from_slice(buf);
            if swapping {
                let mut previous = dry;
                self.chains[old].run(ch, &mut previous[..n]);
                self.chains[active].run(ch, buf);
                mix_in(&previous[..n], buf, &swap_weights[..n]);
            } else {
                self.chains[active].run(ch, buf);
            }
            if fading {
                mix_in(&dry[..n], buf, &weights[..n]);
            }
        }
        for chain in &mut self.chains {
            chain.flush();
        }
        if self.fade.idle() {
            self.chains[0].clear();
            self.chains[1].clear();
        }
    }
}

// --- Gate --------------------------------------------------------------------

struct GateSection {
    fade: Fade,
    /// The threshold as a linear peak level.
    open_level: Glide,
    range_db: Glide,
    attack_step: f32,
    release_step: f32,
    hold: u32,
    /// Peak detector fall per frame.
    fall: f32,
    /// Detector level, linear.
    level: f32,
    /// Triggered: above the threshold, inside the hysteresis band or held.
    open: bool,
    hold_left: u32,
    /// 0 open … 1 closed; read eased as the dB ramp toward the range.
    closed: f32,
}

impl GateSection {
    fn new(ctx: &Ctx, s: &Gate) -> Self {
        let mut gate = Self {
            fade: Fade::new(s.on),
            open_level: Glide::new(db_to_lin(s.threshold_db), 1.0e-8),
            range_db: Glide::new(s.range_db, 1.0e-4),
            attack_step: 1.0,
            release_step: 1.0,
            hold: 0,
            fall: (-1.0 / ctx.samples(GATE_DETECT_MS)).exp(),
            level: 0.0,
            open: false,
            hold_left: 0,
            closed: 0.0,
        };
        gate.set(ctx, s);
        gate
    }

    fn set(&mut self, ctx: &Ctx, s: &Gate) {
        self.fade.set(s.on);
        self.open_level.target = db_to_lin(s.threshold_db);
        self.range_db.target = s.range_db;
        self.attack_step = 1.0 / ctx.samples(s.attack_ms).max(1.0);
        self.release_step = 1.0 / ctx.samples(s.release_ms).max(1.0);
        self.hold = ctx.samples(s.hold_ms) as u32;
    }

    fn clear(&mut self) {
        self.level = 0.0;
        self.open = false;
        self.hold_left = 0;
        self.closed = 0.0;
    }

    #[inline]
    fn gain(closed: f32, range_db: f32) -> f32 {
        if closed <= 0.0 {
            1.0
        } else if closed >= 1.0 && range_db <= GATE_FLOOR_DB + 1.0e-3 {
            0.0
        } else {
            db_to_lin(range_db * ease(closed.min(1.0)))
        }
    }

    /// What it takes off now, dB, ≥ 0.
    fn attenuation_db(&self) -> f32 {
        if self.closed <= 0.0 {
            0.0
        } else {
            (-self.range_db.value * ease(self.closed.min(1.0))).max(0.0)
        }
    }

    fn run(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32], stereo: bool) {
        if self.fade.idle() {
            self.open_level.settle();
            self.range_db.settle();
            return;
        }
        let n = l.len();
        let mut weights = [1.0f32; SUB];
        if !self.fade.full() {
            self.fade.fill(ctx.fade_step, &mut weights[..n]);
        }
        let k = ctx.k_sample;
        for i in 0..n {
            let key = if stereo {
                l[i].abs().max(r[i].abs())
            } else {
                l[i].abs()
            };
            // NaN fails both tests and reads as silence.
            let key = if key < 1.0e3 {
                key
            } else if key >= 1.0e3 {
                1.0e3
            } else {
                0.0
            };
            self.level = key.max(self.level * self.fall);
            let open_at = self.open_level.tick(k);
            if self.level >= open_at {
                self.open = true;
                self.hold_left = self.hold;
            } else if self.open {
                if self.level >= open_at * GATE_HYSTERESIS {
                    self.hold_left = self.hold;
                } else if self.hold_left > 0 {
                    self.hold_left -= 1;
                } else {
                    self.open = false;
                }
            }
            self.closed = if self.open {
                (self.closed - self.attack_step).max(0.0)
            } else {
                (self.closed + self.release_step).min(1.0)
            };
            let range = self.range_db.tick(k);
            let g = 1.0 + (Self::gain(self.closed, range) - 1.0) * weights[i];
            l[i] *= g;
            if stereo {
                r[i] *= g;
            }
        }
        if self.level < 1.0e-20 {
            self.level = 0.0;
        }
        if self.fade.idle() {
            self.clear();
        }
    }
}

// --- EQ ----------------------------------------------------------------------

/// One band's gliding values and coefficients (shared by both orders).
#[derive(Debug, Clone, Copy)]
struct BandCtl {
    /// The kind running now; a change of kind first glides the gain to 0 dB
    /// (where every kind is the identity), swaps, and glides back.
    kind: EqKind,
    kind_target: EqKind,
    gain_target: f32,
    log_hz: Glide,
    gain_db: Glide,
    log_q: Glide,
    coefs: Coefs,
    /// At 0 dB with its memory decayed: skipped.
    idle: bool,
    dirty: bool,
}

impl BandCtl {
    fn new(b: &EqBand) -> Self {
        Self {
            kind: b.kind,
            kind_target: b.kind,
            gain_target: b.gain_db,
            log_hz: Glide::new(b.hz.log2(), 1.0e-5),
            gain_db: Glide::new(b.gain_db, 1.0e-3),
            log_q: Glide::new(b.q.log2(), 1.0e-5),
            coefs: Coefs::IDENTITY,
            idle: b.gain_db == 0.0,
            dirty: true,
        }
    }

    fn set(&mut self, b: &EqBand) {
        self.kind_target = b.kind;
        self.gain_target = b.gain_db;
        self.log_hz.target = b.hz.log2();
        self.log_q.target = b.q.log2();
        self.gain_db.target = if self.kind == self.kind_target {
            self.gain_target
        } else {
            0.0
        };
    }

    fn design(&mut self, ctx: &Ctx) {
        self.coefs = Coefs::eq(
            self.kind,
            ctx.w0(self.log_hz.value.exp2()),
            self.gain_db.value as f64,
            self.log_q.value.exp2() as f64,
        );
        self.dirty = false;
    }

    /// Lands every value and the kind (the band's memory is the caller's).
    fn settle(&mut self) {
        self.kind = self.kind_target;
        self.gain_db.target = self.gain_target;
        self.log_hz.settle();
        self.gain_db.settle();
        self.log_q.settle();
        self.dirty = true;
    }

    /// Once per run, before the audio.
    fn update(&mut self, ctx: &Ctx, n: usize) {
        if self.kind != self.kind_target && (self.idle || self.gain_db.value == 0.0) {
            self.kind = self.kind_target;
            self.dirty = true;
        }
        self.gain_db.target = if self.kind == self.kind_target {
            self.gain_target
        } else {
            0.0
        };
        if self.idle {
            if self.gain_db.target == 0.0 {
                self.log_hz.settle();
                self.log_q.settle();
                self.gain_db.settle();
                return;
            }
            self.idle = false;
            self.dirty = true;
        }
        if self.log_hz.moving() || self.gain_db.moving() || self.log_q.moving() {
            let k = ctx.k_for(n);
            self.log_hz.tick(k);
            self.gain_db.tick(k);
            self.log_q.tick(k);
            self.dirty = true;
        }
        if self.dirty {
            self.design(ctx);
        }
    }

    /// At 0 dB, nothing pending: may go idle once its memory has decayed.
    fn at_identity(&self) -> bool {
        !self.idle
            && self.gain_db.value == 0.0
            && self.gain_db.target == 0.0
            && self.kind == self.kind_target
    }
}

struct EqSection {
    fade: Fade,
    bands: [BandCtl; EQ_BANDS],
}

/// The EQ's filter memory for one order: `[channel][band]`.
#[derive(Debug, Clone, Copy, Default)]
struct EqState {
    s: [[Biquad; EQ_BANDS]; 2],
}

// --- Comp --------------------------------------------------------------------

struct CompSection {
    fade: Fade,
    threshold_db: Glide,
    /// `1 − 1/ratio`: how much of the overshoot comes off.
    slope: Glide,
    knee_db: Glide,
    makeup_db: Glide,
    attack: f32,
    release: f32,
}

impl CompSection {
    fn new(ctx: &Ctx, s: &Comp) -> Self {
        let mut comp = Self {
            fade: Fade::new(s.on),
            threshold_db: Glide::new(s.threshold_db, 1.0e-4),
            slope: Glide::new(1.0 - 1.0 / s.ratio, 1.0e-6),
            knee_db: Glide::new(s.knee_db, 1.0e-4),
            makeup_db: Glide::new(s.makeup_db, 1.0e-4),
            attack: 0.0,
            release: 0.0,
        };
        comp.set(ctx, s);
        comp
    }

    fn set(&mut self, ctx: &Ctx, s: &Comp) {
        self.fade.set(s.on);
        self.threshold_db.target = s.threshold_db;
        self.slope.target = 1.0 - 1.0 / s.ratio;
        self.knee_db.target = s.knee_db;
        self.makeup_db.target = s.makeup_db;
        self.attack = (-1.0 / ctx.samples(s.attack_ms).max(1.0e-3)).exp();
        self.release = (-1.0 / ctx.samples(s.release_ms).max(1.0e-3)).exp();
    }

    fn settle_values(&mut self) {
        self.threshold_db.settle();
        self.slope.settle();
        self.knee_db.settle();
        self.makeup_db.settle();
    }
}

/// The compressor's curve for each frame of a run (both orders read it).
struct CompCurve {
    threshold_db: [f32; SUB],
    slope: [f32; SUB],
    knee_db: [f32; SUB],
    makeup_db: [f32; SUB],
}

/// What one run's EQ and compressor read, whichever order they run in.
struct PathCtx<'a> {
    bands: &'a [BandCtl; EQ_BANDS],
    eq_on: bool,
    eq_fading: bool,
    eq_weights: &'a [f32],
    comp_on: bool,
    comp_weights: &'a [f32],
    curve: &'a CompCurve,
    attack: f32,
    release: f32,
}

fn eq_stage(state: &mut EqState, pc: &PathCtx, l: &mut [f32], r: &mut [f32], stereo: bool) {
    if !pc.eq_on {
        return;
    }
    let n = l.len();
    for ch in 0..1 + stereo as usize {
        let buf: &mut [f32] = if ch == 0 { &mut *l } else { &mut *r };
        let mut dry = [0.0f32; SUB];
        if pc.eq_fading {
            dry[..n].copy_from_slice(buf);
        }
        for (band, ctl) in pc.bands.iter().enumerate() {
            if !ctl.idle {
                state.s[ch][band].run(&ctl.coefs, buf);
            }
        }
        if pc.eq_fading {
            mix_in(&dry[..n], buf, &pc.eq_weights[..n]);
        }
    }
}

/// Feed-forward, peak-detected, soft knee; gain reduction smoothed in dB
/// (attack while it rises, release while it falls).
fn comp_stage(gr: &mut f32, pc: &PathCtx, l: &mut [f32], r: &mut [f32], stereo: bool) {
    if !pc.comp_on {
        return;
    }
    let c = pc.curve;
    for i in 0..l.len() {
        let key = if stereo {
            l[i].abs().max(r[i].abs())
        } else {
            l[i].abs()
        };
        // NaN fails the test and reads as silence.
        let key = if key > 1.0e-9 { key.min(1.0e6) } else { 1.0e-9 };
        let level_db = LOG2_TO_DB * key.log2();
        let over = level_db - c.threshold_db[i];
        let knee = c.knee_db[i];
        let target = if 2.0 * over <= -knee {
            0.0
        } else if 2.0 * over < knee {
            let t = over + 0.5 * knee;
            c.slope[i] * t * t / (2.0 * knee)
        } else {
            c.slope[i] * over
        };
        let coeff = if target > *gr { pc.attack } else { pc.release };
        *gr = target + coeff * (*gr - target);
        let g = 1.0 + (db_to_lin(c.makeup_db[i] - *gr) - 1.0) * pc.comp_weights[i];
        l[i] *= g;
        if stereo {
            r[i] *= g;
        }
    }
    if *gr < 1.0e-9 {
        *gr = 0.0;
    }
}

/// One order's EQ and compressor: path 0 runs EQ → Comp, path 1 Comp → EQ.
fn run_path(
    path: usize,
    pc: &PathCtx,
    eq: &mut EqState,
    gr: &mut f32,
    l: &mut [f32],
    r: &mut [f32],
    stereo: bool,
) {
    if path == 0 {
        eq_stage(eq, pc, l, r, stereo);
        comp_stage(gr, pc, l, r, stereo);
    } else {
        comp_stage(gr, pc, l, r, stereo);
        eq_stage(eq, pc, l, r, stereo);
    }
}

fn order_index(order: ProcessingOrder) -> usize {
    match order {
        ProcessingOrder::EqThenComp => 0,
        ProcessingOrder::CompThenEq => 1,
    }
}

/// The EQ/Comp order: switching runs both orders for [`FADE_MS`] and
/// crossfades (the new one starts from a copy of the old one's memory).
struct OrderSwitch {
    current: usize,
    previous: usize,
    target: usize,
    /// Weight of `current` against `previous`; full when not switching.
    fade: Fade,
}

// --- Delay -------------------------------------------------------------------

/// A delay time that changes while audio runs: the output crossfades from
/// the old tap to the new one over [`FADE_MS`]. A change that arrives
/// mid-fade waits for the fade to finish, so a dragged value walks over in
/// clean steps and always lands on the latest one.
#[derive(Debug, Clone, Copy)]
struct TapSwitch {
    from: usize,
    to: usize,
    pending: usize,
    pos: f32,
}

impl TapSwitch {
    fn new(delay: usize) -> Self {
        Self {
            from: delay,
            to: delay,
            pending: delay,
            pos: 1.0,
        }
    }

    fn settle(&mut self) {
        self.from = self.pending;
        self.to = self.pending;
        self.pos = 1.0;
    }

    /// Settled at no delay: the line only records.
    fn bypassed(&self) -> bool {
        self.pos >= 1.0 && self.to == 0 && self.pending == 0
    }

    /// One frame further: `(from, to, weight of to)`.
    #[inline]
    fn next(&mut self, step: f32) -> (usize, usize, f32) {
        if self.pos >= 1.0 {
            if self.pending == self.to {
                return (self.to, self.to, 1.0);
            }
            self.from = self.to;
            self.to = self.pending;
            self.pos = 0.0;
        }
        self.pos = (self.pos + step).min(1.0);
        (self.from, self.to, ease(self.pos))
    }
}

struct DelaySection {
    /// Preallocated in `new`: [`MAX_DELAY_MS`] plus the current frame.
    lines: [Box<[f32]>; 2],
    write: usize,
    max: usize,
    switch: TapSwitch,
}

impl DelaySection {
    fn new(ctx: &Ctx) -> Self {
        let max = ctx.samples(MAX_DELAY_MS).ceil() as usize;
        Self {
            lines: [
                vec![0.0; max + 1].into_boxed_slice(),
                vec![0.0; max + 1].into_boxed_slice(),
            ],
            write: 0,
            max,
            switch: TapSwitch::new(0),
        }
    }

    fn delay_samples(&self, ctx: &Ctx, s: &Delay) -> usize {
        if s.on {
            (ctx.samples(s.ms).round() as usize).min(self.max)
        } else {
            0
        }
    }

    #[inline]
    fn tap(line: &[f32], write: usize, delay: usize) -> f32 {
        let index = if write >= delay {
            write - delay
        } else {
            write + line.len() - delay
        };
        line[index]
    }

    /// Always records (a copy: the line holds real history when a delay is
    /// switched on); reads only when a delay is set or moving.
    fn run(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32], stereo: bool) {
        let len = self.lines[0].len();
        let bypassed = self.switch.bypassed();
        for i in 0..l.len() {
            let w = self.write;
            self.lines[0][w] = l[i];
            if stereo {
                self.lines[1][w] = r[i];
            }
            if !bypassed {
                let (from, to, weight) = self.switch.next(ctx.fade_step);
                let read = |line: &[f32]| {
                    let b = Self::tap(line, w, to);
                    if weight >= 1.0 {
                        b
                    } else {
                        let a = Self::tap(line, w, from);
                        a + (b - a) * weight
                    }
                };
                l[i] = read(&self.lines[0]);
                if stereo {
                    r[i] = read(&self.lines[1]);
                }
            }
            self.write = if w + 1 == len { 0 } else { w + 1 };
        }
    }
}

// --- The strip processor -----------------------------------------------------

/// The realtime processor. Allocates in [`StripProcessor::new`] only.
///
/// HPF (12/18/24 dB/oct Butterworth) → gate (peak detector, 4 dB hysteresis,
/// hold, dB ramps toward the range) → four RBJ EQ bands ↔ a feed-forward
/// soft-knee compressor (order selectable) → delay. Stereo strips run L/R
/// through linked detectors. Continuous values glide (filters are redesigned
/// every [`SUB`] frames from the gliding values); on/off, order, slope and
/// delay changes crossfade over [`FADE_MS`]. A section that is off and done
/// fading costs nothing (the delay line keeps recording, a copy), and so
/// does an EQ band at 0 dB.
pub struct StripProcessor {
    ctx: Ctx,
    /// The latest settings, held to their ranges.
    target: Processing,
    hpf: HpfSection,
    gate: GateSection,
    eq: EqSection,
    comp: CompSection,
    /// Per order (see [`run_path`]): EQ memory and compressor gain
    /// reduction (dB).
    eq_state: [EqState; 2],
    comp_gr: [f32; 2],
    order: OrderSwitch,
    delay: DelaySection,
}

impl StripProcessor {
    /// Sized for `sample_rate` (the delay line holds [`MAX_DELAY_MS`]).
    pub fn new(sample_rate: u32) -> Self {
        let ctx = Ctx::new((sample_rate as f32).clamp(8_000.0, 768_000.0));
        let target = sanitize(&Processing::default());
        let order = order_index(target.order);
        let mut processor = Self {
            ctx,
            target,
            hpf: HpfSection::new(&target.hpf),
            gate: GateSection::new(&ctx, &target.gate),
            eq: EqSection {
                fade: Fade::new(target.eq.on),
                bands: target.eq.bands.map(|b| BandCtl::new(&b)),
            },
            comp: CompSection::new(&ctx, &target.comp),
            eq_state: [EqState::default(); 2],
            comp_gr: [0.0; 2],
            order: OrderSwitch {
                current: order,
                previous: order,
                target: order,
                fade: Fade::new(true),
            },
            delay: DelaySection::new(&ctx),
        };
        processor.settle();
        processor
    }

    /// New settings. Realtime-safe; continuous values glide, sections fade
    /// in and out.
    pub fn set(&mut self, settings: &Processing) {
        let s = sanitize(settings);
        self.target = s;
        let ctx = self.ctx;
        self.hpf.fade.set(s.hpf.on);
        self.hpf.log_hz.target = s.hpf.hz.log2();
        self.hpf.slope = s.hpf.slope_db;
        self.gate.set(&ctx, &s.gate);
        self.eq.fade.set(s.eq.on);
        for (ctl, band) in self.eq.bands.iter_mut().zip(&s.eq.bands) {
            ctl.set(band);
        }
        self.comp.set(&ctx, &s.comp);
        self.order.target = order_index(s.order);
        self.delay.switch.pending = self.delay.delay_samples(&ctx, &s.delay);
    }

    /// Lands on the current settings at once (a session load): no glide.
    pub fn settle(&mut self) {
        let ctx = self.ctx;
        self.hpf.settle(&ctx);

        self.gate.fade.settle();
        self.gate.open_level.settle();
        self.gate.range_db.settle();
        if self.gate.fade.idle() {
            self.gate.clear();
        }

        let order = &mut self.order;
        if order.current != order.target {
            self.eq_state[order.target] = self.eq_state[order.current];
            self.comp_gr[order.target] = self.comp_gr[order.current];
        }
        order.current = order.target;
        order.previous = order.target;
        order.fade.settle();

        self.eq.fade.settle();
        if self.eq.fade.idle() {
            self.eq_state = [EqState::default(); 2];
        }
        for ctl in &mut self.eq.bands {
            ctl.settle();
            ctl.design(&ctx);
            ctl.idle = false;
        }
        self.retire_quiet_bands(true);

        self.comp.fade.settle();
        self.comp.settle_values();
        if self.comp.fade.idle() {
            self.comp_gr = [0.0; 2];
        }

        self.delay.switch.settle();
    }

    /// Processes one block in place. A mono strip carries its signal in
    /// `left` only (`stereo == false`): `right` is left untouched.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32], stereo: bool) {
        let frames = if stereo {
            left.len().min(right.len())
        } else {
            left.len()
        };
        let mut start = 0;
        while start < frames {
            let end = (start + SUB).min(frames);
            let l = &mut left[start..end];
            let r: &mut [f32] = if stereo {
                &mut right[start..end]
            } else {
                &mut []
            };
            self.run(l, r, stereo);
            start = end;
        }
    }

    pub fn reset(&mut self) {
        for chain in &mut self.hpf.chains {
            chain.clear();
        }
        self.gate.clear();
        self.eq_state = [EqState::default(); 2];
        self.comp_gr = [0.0; 2];
        for line in &mut self.delay.lines {
            line.fill(0.0);
        }
        self.delay.write = 0;
    }

    pub fn meters(&self) -> ProcessingMeters {
        let gate_on = self.target.gate.on;
        ProcessingMeters {
            gate_open: !gate_on || self.gate.open || self.gate.closed <= 0.0,
            gate_db: if gate_on {
                self.gate.attenuation_db()
            } else {
                0.0
            },
            comp_db: if self.target.comp.on {
                self.comp_gr[self.order.current].max(0.0)
            } else {
                0.0
            },
        }
    }

    /// One run of at most [`SUB`] frames (`r` is empty for a mono strip).
    fn run(&mut self, l: &mut [f32], r: &mut [f32], stereo: bool) {
        let ctx = self.ctx;
        let n = l.len();
        self.hpf.run(&ctx, l, r, stereo);
        self.gate.run(&ctx, l, r, stereo);
        self.run_eq_comp(&ctx, l, r, stereo, n);
        self.delay.run(&ctx, l, r, stereo);
    }

    fn run_eq_comp(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32], stereo: bool, n: usize) {
        // EQ: values and coefficients once per run.
        let eq_on = !self.eq.fade.idle();
        let eq_fading = !self.eq.fade.full();
        let mut eq_weights = [1.0f32; SUB];
        if eq_on {
            if eq_fading {
                self.eq.fade.fill(ctx.fade_step, &mut eq_weights[..n]);
            }
            for ctl in &mut self.eq.bands {
                ctl.update(ctx, n);
            }
        } else {
            for ctl in &mut self.eq.bands {
                ctl.settle();
                ctl.idle = ctl.gain_target == 0.0;
            }
        }

        // Comp: its curve per frame.
        let comp_on = !self.comp.fade.idle();
        let mut comp_weights = [1.0f32; SUB];
        let mut curve = CompCurve {
            threshold_db: [0.0; SUB],
            slope: [0.0; SUB],
            knee_db: [0.0; SUB],
            makeup_db: [0.0; SUB],
        };
        if comp_on {
            if !self.comp.fade.full() {
                self.comp.fade.fill(ctx.fade_step, &mut comp_weights[..n]);
            }
            let k = ctx.k_sample;
            let c = &mut self.comp;
            for i in 0..n {
                curve.threshold_db[i] = c.threshold_db.tick(k);
                curve.slope[i] = c.slope.tick(k);
                curve.knee_db[i] = c.knee_db.tick(k);
                curve.makeup_db[i] = c.makeup_db.tick(k);
            }
        } else {
            self.comp.settle_values();
        }

        // Order: start a crossfade if asked (and not already in one).
        let order = &mut self.order;
        if order.fade.full() && order.current != order.target {
            let (from, to) = (order.current, order.target);
            self.eq_state[to] = self.eq_state[from];
            self.comp_gr[to] = self.comp_gr[from];
            order.previous = from;
            order.current = to;
            if eq_on || comp_on {
                order.fade = Fade::start();
            }
        }

        let pc = PathCtx {
            bands: &self.eq.bands,
            eq_on,
            eq_fading,
            eq_weights: &eq_weights,
            comp_on,
            comp_weights: &comp_weights,
            curve: &curve,
            attack: self.comp.attack,
            release: self.comp.release,
        };
        let (current, previous) = (self.order.current, self.order.previous);
        let switching = !self.order.fade.full();
        if switching {
            let mut order_weights = [1.0f32; SUB];
            self.order.fade.fill(ctx.fade_step, &mut order_weights[..n]);
            let mut new_l = [0.0f32; SUB];
            let mut new_r = [0.0f32; SUB];
            new_l[..n].copy_from_slice(l);
            if stereo {
                new_r[..n].copy_from_slice(r);
            }
            let new_r: &mut [f32] = if stereo { &mut new_r[..n] } else { &mut [] };
            run_path(
                previous,
                &pc,
                &mut self.eq_state[previous],
                &mut self.comp_gr[previous],
                l,
                r,
                stereo,
            );
            run_path(
                current,
                &pc,
                &mut self.eq_state[current],
                &mut self.comp_gr[current],
                &mut new_l[..n],
                new_r,
                stereo,
            );
            for i in 0..n {
                l[i] += (new_l[i] - l[i]) * order_weights[i];
            }
            if stereo {
                for i in 0..n {
                    r[i] += (new_r[i] - r[i]) * order_weights[i];
                }
            }
        } else {
            run_path(
                current,
                &pc,
                &mut self.eq_state[current],
                &mut self.comp_gr[current],
                l,
                r,
                stereo,
            );
        }

        // Housekeeping.
        if eq_on {
            for (band, ctl) in self.eq.bands.iter().enumerate() {
                if !ctl.idle {
                    for ch in self.eq_state.iter_mut().flat_map(|state| &mut state.s) {
                        ch[band].flush();
                    }
                }
            }
            self.retire_quiet_bands(stereo);
            if self.eq.fade.idle() {
                self.eq_state = [EqState::default(); 2];
            }
        }
        if self.comp.fade.idle() {
            self.comp_gr = [0.0; 2];
        }
    }

    /// Bands at 0 dB whose memory (in the order(s) running, on the channels
    /// running) has decayed stop running; their memory is zeroed.
    fn retire_quiet_bands(&mut self, stereo: bool) {
        let switching = !self.order.fade.full();
        let paths = [self.order.current, self.order.previous];
        let paths = if switching { &paths[..] } else { &paths[..1] };
        let channels = 1 + stereo as usize;
        for (band, ctl) in self.eq.bands.iter_mut().enumerate() {
            if !ctl.at_identity() {
                continue;
            }
            let quiet = paths.iter().all(|&p| {
                self.eq_state[p].s[..channels]
                    .iter()
                    .all(|ch| ch[band].quiet())
            });
            if quiet {
                ctl.idle = true;
                for state in &mut self.eq_state {
                    for ch in &mut state.s {
                        ch[band] = Biquad::default();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
