//! WrapSynth's wavetables: each a stack of single-cycle frames the
//! oscillator morphs through, stored band-limited so a note never folds
//! harmonics back past Nyquist.
//!
//! Every table is generated here (no audio files). Each frame's spectrum is
//! worked out once — exactly, from its Fourier series, for the classic and
//! additive tables; with an FFT of a finely drawn cycle for the rest — and
//! resynthesised at [`LEVELS`] levels, half an octave apart, each keeping
//! only the harmonics notes at that pitch can carry: a mipmap in frequency.
//! Every level is stored [`OVERSAMPLE`] times finer than its top harmonic
//! needs, so the cubic interpolation between samples stays clean, with a
//! few samples of wrap-around either side so a read never has to wrap.
//!
//! A playing oscillator asks for the fullest level its pitch allows
//! ([`mip_for`]) and, near the top of that level's range, crossfades into
//! the next one, so a sweep never pops between levels.
//!
//! The whole bank is built once per process, off the audio path, the first
//! time a synth is made ([`bank`]); after that every voice of every instance
//! only reads it.

use std::sync::OnceLock;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use serde::{Deserialize, Serialize};

/// Frames in a morphing table: the oscillator's position sweeps through them.
pub const FRAMES: usize = 32;
/// Harmonics in the fullest level: enough for a 40 Hz note to reach 20 kHz;
/// below that the top harmonics are already 50 dB under the fundamental.
pub const MAX_HARMONICS: usize = 512;
/// Levels, half an octave apart, from [`MAX_HARMONICS`] down to the
/// fundamental alone.
pub const LEVELS: usize = 19;
/// Samples per cycle of each level's top harmonic: eight, the 4× oversampled
/// data (in the interpolator's terms) [cubic] is made for.
pub const OVERSAMPLE: usize = 8;
const MIN_LEVEL_SIZE: usize = 64;
/// Samples a time-drawn cycle is taken at before its spectrum is measured:
/// fine enough that the measured top harmonics are within a few percent.
const RAW_SIZE: usize = 8_192;
/// The highest a level's top harmonic may play, in cycles per sample: just
/// under Nyquist, so nothing folds back.
pub const BAND_LIMIT: f32 = 0.49;
/// The share of a level's half-octave range, at its top, over which the next
/// level fades in.
const FADE: f32 = 0.5;
/// Samples stored around each cycle: one before, two after.
const GUARD: usize = 3;

/// The tables an oscillator can play. The first four are WrapSynth's first
/// shapes, kept (with the same names on the wire and in saved projects) so
/// an old patch sounds as it did; their position still bends them. The
/// classic shapes appended at the end are the pure waveforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Wavetable {
    /// A saw bent by the position, blended toward a triangle.
    #[default]
    Saw,
    Square,
    Triangle,
    Sine,
    /// Sine → triangle → saw → square.
    BasicShapes,
    /// One harmonic, then more and more, to a full saw.
    Harmonics,
    /// A square narrowing to a thin pulse.
    Pulse,
    /// A saw hard-synced from 1× up to 8× its own rate.
    HardSync,
    /// A sung vowel moving A → E → I → O → U.
    Formant,
    /// Two-operator FM, the index rising.
    Fm,
    /// The pure classic shapes: one frame, no morph.
    ClassicSaw,
    ClassicSquare,
    ClassicTriangle,
    ClassicSine,
}

impl Wavetable {
    /// Every table in wire order.
    pub const ALL: [Wavetable; 14] = [
        Self::Saw,
        Self::Square,
        Self::Triangle,
        Self::Sine,
        Self::BasicShapes,
        Self::Harmonics,
        Self::Pulse,
        Self::HardSync,
        Self::Formant,
        Self::Fm,
        Self::ClassicSaw,
        Self::ClassicSquare,
        Self::ClassicTriangle,
        Self::ClassicSine,
    ];
    /// Every table in the order a picker lists them: the classic shapes,
    /// the morphing tables, then the legacy shapes.
    pub const MENU: [Wavetable; 14] = [
        Self::ClassicSaw,
        Self::ClassicSquare,
        Self::ClassicTriangle,
        Self::ClassicSine,
        Self::BasicShapes,
        Self::Harmonics,
        Self::Pulse,
        Self::HardSync,
        Self::Formant,
        Self::Fm,
        Self::Saw,
        Self::Square,
        Self::Triangle,
        Self::Sine,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn to_wire(self) -> f32 {
        self.index() as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[value.round().clamp(0.0, (Self::COUNT - 1) as f32) as usize]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Saw => "Legacy Saw",
            Self::Square => "Legacy Square",
            Self::Triangle => "Legacy Triangle",
            Self::Sine => "Legacy Sine",
            Self::BasicShapes => "Basic Shapes",
            Self::Harmonics => "Harmonic Series",
            Self::Pulse => "Pulse Width",
            Self::HardSync => "Hard Sync",
            Self::Formant => "Vowels",
            Self::Fm => "FM Bell",
            Self::ClassicSaw => "Classic Saw",
            Self::ClassicSquare => "Classic Square",
            Self::ClassicTriangle => "Classic Triangle",
            Self::ClassicSine => "Classic Sine",
        }
    }

    /// Frames the table holds: one for a classic shape, which the position
    /// does not move.
    pub fn frames(self) -> usize {
        match self {
            Self::ClassicSaw | Self::ClassicSquare | Self::ClassicTriangle | Self::ClassicSine => 1,
            _ => FRAMES,
        }
    }

    /// Whether the position morphs this table.
    pub fn morphs(self) -> bool {
        self.frames() > 1
    }
}

/// One level of one table: every frame, back to back, each `size` samples
/// with [`GUARD`] samples of wrap-around.
struct Level {
    size: usize,
    data: Box<[f32]>,
}

impl Level {
    fn stride(&self) -> usize {
        self.size + GUARD
    }

    fn row(&self, frame: usize) -> &[f32] {
        let stride = self.stride();
        &self.data[frame * stride..(frame + 1) * stride]
    }
}

struct Table {
    frames: usize,
    levels: Vec<Level>,
}

pub struct Bank {
    tables: Vec<Table>,
}

impl std::fmt::Debug for Bank {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bank")
            .field("tables", &self.tables.len())
            .finish()
    }
}

/// The highest harmonic level `level` keeps: [`MAX_HARMONICS`], half an
/// octave fewer each level, rounded down so a level never holds more than
/// [`mip_for`] reckons with.
pub fn level_harmonics(level: usize) -> usize {
    ((MAX_HARMONICS as f64 * 0.5f64.powf(level as f64 * 0.5)).floor() as usize).max(1)
}

/// Samples in one cycle of level `level`.
fn level_size(level: usize) -> usize {
    (level_harmonics(level) * OVERSAMPLE).max(MIN_LEVEL_SIZE)
}

/// Which level a cycle plays from, and how far it has faded into the next.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mip {
    pub level: usize,
    /// 0..1: the next, duller level's share.
    pub fade: f32,
}

impl Mip {
    pub const FULL: Mip = Mip {
        level: 0,
        fade: 0.0,
    };
}

/// The level for a cycle advancing `increment` (cycles per sample): the
/// fullest one whose top harmonic stays under [`BAND_LIMIT`], fading into
/// the next over the top of its range.
#[inline]
pub fn mip_for(increment: f32) -> Mip {
    let increment = increment.abs();
    if increment.is_nan() || increment <= 0.0 {
        return Mip::FULL;
    }
    // In half-octave steps: how far the fullest level's top harmonic is
    // over the limit. Level `L` holds at most MAX_HARMONICS · 2^(−L/2), so
    // it is safe from `L ≥ over`.
    let over = 2.0 * (MAX_HARMONICS as f32 * increment / BAND_LIMIT).log2();
    let level = over.ceil().max(0.0);
    if level >= (LEVELS - 1) as f32 {
        return Mip {
            level: LEVELS - 1,
            fade: 0.0,
        };
    }
    Mip {
        level: level as usize,
        fade: ((over - (level - FADE)) / FADE).clamp(0.0, 1.0),
    }
}

/// One level, ready to read at any phase.
#[derive(Clone, Copy)]
struct CursorLevel<'a> {
    size: f32,
    last: usize,
    near: &'a [f32],
    far: &'a [f32],
    /// The far frame's share.
    blend: f32,
    weight: f32,
}

impl CursorLevel<'_> {
    #[inline]
    fn read(&self, phase: f32) -> f32 {
        let x = phase * self.size;
        // Saturating: a negative or NaN phase reads the start of the cycle.
        let i = (x as usize).min(self.last);
        let t = (x - i as f32).clamp(0.0, 1.0);
        let near = cubic(&self.near[i..i + 4], t);
        if self.blend > 0.0 {
            near + (cubic(&self.far[i..i + 4], t) - near) * self.blend
        } else {
            near
        }
    }
}

/// A table at one position and pitch, set up once and then read at every
/// unison voice's phase.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    levels: [CursorLevel<'a>; 2],
    used: usize,
}

impl Cursor<'_> {
    /// The cycle at `phase` (0..1). Allocation-free.
    #[inline]
    pub fn read(&self, phase: f32) -> f32 {
        let mut out = 0.0;
        for level in &self.levels[..self.used] {
            out += level.read(phase) * level.weight;
        }
        out
    }
}

/// 4-point, third-order interpolation between `y[1]` and `y[2]` at `t`,
/// with the polynomial optimised for data oversampled 4× (Niemitalo,
/// "Polynomial Interpolators for High-Quality Resampling of Oversampled
/// Audio", 2001): it trades passing exactly through the samples for far
/// stronger rejection of the images a plain cubic leaves, which fold back
/// as noise between the harmonics.
#[inline]
fn cubic(y: &[f32], t: f32) -> f32 {
    let z = t - 0.5;
    let (even1, odd1) = (y[2] + y[1], y[2] - y[1]);
    let (even2, odd2) = (y[3] + y[0], y[3] - y[0]);
    let c0 = even1 * 0.468_354_97 + even2 * 0.031_645_028;
    let c1 = odd1 * 0.560_012_9 + odd2 * 0.146_662_39;
    let c2 = even1 * -0.250_038_76 + even2 * 0.250_038_76;
    let c3 = odd1 * -0.499_498_5 + odd2 * 0.166_499_35;
    ((c3 * z + c2) * z + c1) * z + c0
}

impl Bank {
    /// `table` at `position` (0..1 across its frames) and `mip`, ready to
    /// read. Allocation-free.
    #[inline]
    pub fn cursor(&self, table: Wavetable, mip: Mip, position: f32) -> Cursor<'_> {
        let table = &self.tables[table.index()];
        let (first, blend) = if table.frames > 1 {
            let frame = position.clamp(0.0, 1.0) * (table.frames - 1) as f32;
            let first = (frame as usize).min(table.frames - 2);
            (first, frame - first as f32)
        } else {
            (0, 0.0)
        };
        let second = (first + 1).min(table.frames - 1);
        let at = |level: usize, weight: f32| {
            let level_data = &table.levels[level];
            CursorLevel {
                size: level_data.size as f32,
                last: level_data.size - 1,
                near: level_data.row(first),
                far: level_data.row(second),
                blend,
                weight,
            }
        };
        let level = mip.level.min(LEVELS - 1);
        if mip.fade > 0.0 && level + 1 < LEVELS {
            Cursor {
                levels: [at(level, 1.0 - mip.fade), at(level + 1, mip.fade)],
                used: 2,
            }
        } else {
            Cursor {
                levels: [at(level, 1.0), at(level, 0.0)],
                used: 1,
            }
        }
    }

    /// Reads `table` at `position` and `phase` (0..1 of the cycle), from mip
    /// level `level` alone. Allocation-free.
    #[inline]
    pub fn read(&self, table: Wavetable, level: usize, position: f32, phase: f32) -> f32 {
        self.cursor(table, Mip { level, fade: 0.0 }, position)
            .read(phase)
    }
}

/// The bank every WrapSynth reads, built on first use.
pub fn bank() -> &'static Bank {
    static BANK: OnceLock<Bank> = OnceLock::new();
    BANK.get_or_init(build_bank)
}

fn build_bank() -> Bank {
    let mut planner = FftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(RAW_SIZE);
    let inverse: Vec<_> = (0..LEVELS)
        .map(|level| planner.plan_fft_inverse(level_size(level)))
        .collect();
    let mut raw = vec![Complex::new(0.0, 0.0); RAW_SIZE];
    let mut coefficients = vec![Complex::new(0.0, 0.0); MAX_HARMONICS + 1];
    let mut bins = vec![Complex::new(0.0, 0.0); level_size(0)];
    let tables = Wavetable::ALL
        .iter()
        .map(|&table| {
            let frames = table.frames();
            let mut levels: Vec<Level> = (0..LEVELS)
                .map(|level| Level {
                    size: level_size(level),
                    data: vec![0.0; frames * (level_size(level) + GUARD)].into_boxed_slice(),
                })
                .collect();
            for frame in 0..frames {
                let position = if frames > 1 {
                    frame as f32 / (frames - 1) as f32
                } else {
                    0.0
                };
                frame_spectrum(table, position, &*forward, &mut raw, &mut coefficients);
                let mut peak = 0.0f32;
                for (level, plan) in inverse.iter().enumerate() {
                    let size = level_size(level);
                    let bins = &mut bins[..size];
                    bins.fill(Complex::new(0.0, 0.0));
                    for (k, value) in coefficients
                        .iter()
                        .enumerate()
                        .take(level_harmonics(level) + 1)
                        .skip(1)
                    {
                        bins[k] = *value;
                        bins[size - k] = value.conj();
                    }
                    plan.process(bins);
                    let stride = size + GUARD;
                    let row = &mut levels[level].data[frame * stride..(frame + 1) * stride];
                    for (out, bin) in row[1..=size].iter_mut().zip(bins.iter()) {
                        *out = bin.re;
                    }
                    if level == 0 {
                        peak = row[1..=size]
                            .iter()
                            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                    }
                }
                // Every frame at full scale, so a morph keeps its level.
                let scale = 1.0 / peak.max(1.0e-6);
                for level in &mut levels {
                    let size = level.size;
                    let stride = level.stride();
                    let row = &mut level.data[frame * stride..(frame + 1) * stride];
                    for sample in row[1..=size].iter_mut() {
                        *sample *= scale;
                    }
                    row[0] = row[size];
                    row[size + 1] = row[1];
                    row[size + 2] = row[2];
                }
            }
            Table { frames, levels }
        })
        .collect();
    Bank { tables }
}

/// The coefficients `c[h]` of frame `position` of `table`, as the cycle
/// `Σ 2·Re(c[h]·e^(i2πhφ))` for harmonics 1..=[`MAX_HARMONICS`]; `c[0]`
/// (DC) is left at zero, as a table is a cycle of tone, never an offset.
fn frame_spectrum(
    table: Wavetable,
    position: f32,
    forward: &dyn rustfft::Fft<f32>,
    raw: &mut [Complex<f32>],
    c: &mut [Complex<f32>],
) {
    c.fill(Complex::new(0.0, 0.0));
    // A sine term `amplitude · sin(2πhφ)`.
    let sine = |amplitude: f32| Complex::new(0.0, -0.5 * amplitude);
    match table {
        Wavetable::ClassicSaw
        | Wavetable::ClassicSquare
        | Wavetable::ClassicTriangle
        | Wavetable::ClassicSine => {
            let shape = match table {
                Wavetable::ClassicSaw => Shape::Saw,
                Wavetable::ClassicSquare => Shape::Square,
                Wavetable::ClassicTriangle => Shape::Triangle,
                _ => Shape::Sine,
            };
            for (h, value) in c.iter_mut().enumerate().skip(1) {
                *value = shape.coefficient(h);
            }
        }
        Wavetable::BasicShapes => {
            let shapes = [Shape::Sine, Shape::Triangle, Shape::Saw, Shape::Square];
            let span = position * 3.0;
            let first = (span as usize).min(2);
            let blend = span - first as f32;
            for (h, value) in c.iter_mut().enumerate().skip(1) {
                *value = shapes[first].coefficient(h) * (1.0 - blend)
                    + shapes[first + 1].coefficient(h) * blend;
            }
        }
        Wavetable::Harmonics => {
            let count = 1.0 + position * 63.0;
            for (h, value) in c.iter_mut().enumerate().skip(1).take(64) {
                let weight = (count - (h - 1) as f32).clamp(0.0, 1.0);
                *value = sine(weight / h as f32);
            }
        }
        Wavetable::Formant => {
            let span = position * (VOWELS.len() - 1) as f32;
            let first = (span as usize).min(VOWELS.len() - 2);
            let blend = span - first as f32;
            // A voice at 110 Hz, its harmonics shaped by the vowel.
            const F0: f32 = 110.0;
            for (h, value) in c.iter_mut().enumerate().skip(1).take(90) {
                let hz = F0 * h as f32;
                let envelope = |vowel: &[(f32, f32); 3]| {
                    vowel
                        .iter()
                        .map(|&(formant, level)| {
                            let width = 60.0 + formant * 0.08;
                            level * (-((hz - formant) / width).powi(2)).exp()
                        })
                        .sum::<f32>()
                };
                let amp =
                    envelope(&VOWELS[first]) * (1.0 - blend) + envelope(&VOWELS[first + 1]) * blend;
                *value = sine(amp + 0.02 / h as f32);
            }
        }
        _ => {
            // Drawn in time, then measured.
            for (i, bin) in raw.iter_mut().enumerate() {
                let phase = i as f32 / RAW_SIZE as f32;
                *bin = Complex::new(drawn_frame(table, position, phase), 0.0);
            }
            forward.process(raw);
            for (h, value) in c.iter_mut().enumerate().skip(1) {
                *value = raw[h] / RAW_SIZE as f32;
            }
        }
    }
}

/// The original WrapSynth shapes, warped and blended by `position` — what
/// the four legacy tables are made of.
fn legacy(phase: f32, wave: Wavetable, position: f32) -> f32 {
    let warped = phase.powf(0.45 + position * 1.55);
    let (primary, secondary) = match wave {
        Wavetable::Square => (Shape::Square, Shape::Sine),
        Wavetable::Triangle => (Shape::Triangle, Shape::Saw),
        Wavetable::Sine => (Shape::Sine, Shape::Square),
        _ => (Shape::Saw, Shape::Triangle),
    };
    primary.at(warped) * (1.0 - position * 0.55) + secondary.at(phase) * position * 0.55
}

#[derive(Clone, Copy)]
enum Shape {
    Saw,
    Square,
    Triangle,
    Sine,
}

impl Shape {
    fn at(self, phase: f32) -> f32 {
        match self {
            Self::Saw => phase * 2.0 - 1.0,
            Self::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
            Self::Sine => (std::f32::consts::TAU * phase).sin(),
        }
    }

    /// Harmonic `h` of [`Self::at`]'s exact Fourier series, as `c[h]`.
    fn coefficient(self, h: usize) -> Complex<f32> {
        use std::f32::consts::PI;
        let n = h as f32;
        let odd = h % 2 == 1;
        match self {
            // 2φ − 1 = −(2/π) Σ sin(2πhφ)/h.
            Self::Saw => Complex::new(0.0, 1.0 / (PI * n)),
            // (4/π) Σ_odd sin(2πhφ)/h.
            Self::Square if odd => Complex::new(0.0, -2.0 / (PI * n)),
            // −(8/π²) Σ_odd cos(2πhφ)/h².
            Self::Triangle if odd => Complex::new(-4.0 / (PI * PI * n * n), 0.0),
            Self::Sine if h == 1 => Complex::new(0.0, -0.5),
            _ => Complex::new(0.0, 0.0),
        }
    }
}

/// Vowel formants (Hz) and their levels, after Peterson & Barney's adult
/// male averages.
const VOWELS: [[(f32, f32); 3]; 5] = [
    [(730.0, 1.0), (1_090.0, 0.5), (2_440.0, 0.25)],
    [(530.0, 1.0), (1_840.0, 0.45), (2_480.0, 0.3)],
    [(270.0, 1.0), (2_290.0, 0.35), (3_010.0, 0.3)],
    [(570.0, 1.0), (840.0, 0.6), (2_410.0, 0.2)],
    [(300.0, 1.0), (870.0, 0.4), (2_240.0, 0.15)],
];

/// One sample of frame `position` of a table drawn in time, before
/// band-limiting.
fn drawn_frame(table: Wavetable, position: f32, phase: f32) -> f32 {
    use std::f32::consts::TAU;
    match table {
        Wavetable::Pulse => {
            let width = 0.5 - position * 0.46;
            if phase < width { 1.0 } else { -1.0 }
        }
        Wavetable::HardSync => {
            let ratio = 1.0 + position * 7.0;
            (phase * ratio).fract() * 2.0 - 1.0
        }
        Wavetable::Fm => {
            let index = position * 6.0;
            (TAU * phase + index * (TAU * 2.0 * phase).sin()).sin()
        }
        _ => legacy(phase, table, position),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_step_down_half_an_octave_and_stay_oversampled() {
        assert_eq!(level_harmonics(0), MAX_HARMONICS);
        assert_eq!(level_harmonics(2), MAX_HARMONICS / 2);
        assert_eq!(level_harmonics(LEVELS - 1), 1);
        for level in 0..LEVELS {
            assert!(level_size(level) >= OVERSAMPLE * level_harmonics(level));
            if level > 0 {
                assert!(level_harmonics(level) <= level_harmonics(level - 1));
            }
        }
    }

    #[test]
    fn the_chosen_level_never_passes_the_band_limit() {
        // Every pitch from a sub-bass to past Nyquist, in fine steps: the
        // level read at full weight, and any faded-in one, stay under it.
        let mut increment = 1.0e-5f32;
        let mut previous = Mip::FULL;
        while increment < 0.5 {
            let mip = mip_for(increment);
            let top = level_harmonics(mip.level) as f32 * increment;
            assert!(
                top <= BAND_LIMIT * 1.0001 || mip.level == LEVELS - 1,
                "{increment}: level {} plays up to {top}",
                mip.level
            );
            // Never jumps back to a fuller level as the pitch rises, and
            // only moves on once the fade has finished.
            let position = previous.level as f32 + previous.fade;
            let now = mip.level as f32 + mip.fade;
            assert!(now + 1.0e-4 >= position, "{previous:?} → {mip:?}");
            assert!(
                now - position < 0.2,
                "{previous:?} → {mip:?} at {increment}"
            );
            previous = mip;
            increment *= 1.002;
        }
        assert_eq!(mip_for(0.0), Mip::FULL);
        assert_eq!(mip_for(f32::NAN), Mip::FULL);
    }

    #[test]
    fn every_table_is_full_scale_and_continuous() {
        let bank = bank();
        for table in Wavetable::ALL {
            // On a frame, which the bank scales to full; a blend of two
            // frames peaks where their cycles meet, which may be lower.
            for position in [0.0, 12.0 / (FRAMES - 1) as f32, 1.0] {
                let mut peak = 0.0f32;
                // 64 harmonics: smooth enough to measure.
                let smooth = (0..LEVELS)
                    .find(|&level| level_harmonics(level) == 64)
                    .expect("a 64-harmonic level");
                let mut previous = bank.read(table, smooth, position, 0.0);
                let mut biggest_step = 0.0f32;
                // Every sample of the fullest level.
                let samples = level_size(0);
                for i in 1..=samples {
                    let phase = i as f32 / samples as f32;
                    peak = peak.max(bank.read(table, 0, position, phase).abs());
                    let value = bank.read(table, smooth, position, phase.min(0.999_999));
                    assert!(value.is_finite());
                    biggest_step = biggest_step.max((value - previous).abs());
                    previous = value;
                }
                assert!(peak > 0.9 && peak < 1.1, "{table:?} at {position}: {peak}");
                // Band-limited: even a square's edge takes several samples.
                assert!(biggest_step < 0.3, "{table:?} jumps {biggest_step}");
            }
        }
    }

    #[test]
    fn the_classic_saw_is_a_saw() {
        let bank = bank();
        // The fullest level, away from the edge: a straight rising ramp,
        // scaled by its Gibbs peak.
        let at = |phase: f32| bank.read(Wavetable::ClassicSaw, 0, 0.0, phase);
        let slope = (at(0.75) - at(0.25)) / 0.5;
        assert!(slope > 1.6 && slope < 2.0, "slope {slope}");
        for phase in [0.1, 0.3, 0.5, 0.7, 0.9] {
            let line = at(0.5) + (phase - 0.5) * slope;
            assert!((at(phase) - line).abs() < 0.01, "{phase}: {}", at(phase));
        }
        // One frame: the position changes nothing.
        assert_eq!(
            bank.read(Wavetable::ClassicSaw, 4, 0.0, 0.3),
            bank.read(Wavetable::ClassicSaw, 4, 1.0, 0.3)
        );
        assert!(!Wavetable::ClassicSaw.morphs() && Wavetable::BasicShapes.morphs());
    }

    #[test]
    fn a_crossfade_between_levels_has_no_step() {
        // Just either side of the point where a level hands over: the read
        // barely moves.
        let bank = bank();
        let mut increment = 1.0e-4f32;
        while mip_for(increment).fade == 0.0 {
            increment *= 1.001;
        }
        let mut before = mip_for(increment);
        while mip_for(increment * 1.001).level == before.level {
            increment *= 1.001;
            before = mip_for(increment);
        }
        let after = mip_for(increment * 1.001);
        assert!(
            before.fade > 0.99 && after.fade == 0.0,
            "{before:?} {after:?}"
        );
        for phase in [0.1, 0.33, 0.6] {
            let a = bank.cursor(Wavetable::ClassicSaw, before, 0.0).read(phase);
            let b = bank.cursor(Wavetable::ClassicSaw, after, 0.0).read(phase);
            assert!((a - b).abs() < 0.01, "{phase}: {a} → {b}");
        }
    }

    #[test]
    fn a_high_level_holds_only_the_fundamental() {
        let bank = bank();
        // The top level of a square is a sine: its value a quarter cycle in
        // is its peak, and the cycle is symmetric.
        let top = LEVELS - 1;
        let quarter = bank.read(Wavetable::BasicShapes, top, 1.0, 0.25);
        let three_quarters = bank.read(Wavetable::BasicShapes, top, 1.0, 0.75);
        assert!((quarter + three_quarters).abs() < 0.05);
        assert!(quarter.abs() > 0.9);
    }
}
