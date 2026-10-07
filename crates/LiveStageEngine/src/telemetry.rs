//! What a built-in insert measures while it plays — levels, gain reduction,
//! the spectrum arriving at it, a multiband reduction, a stereo image, a pitch
//! trace — for an editor to draw. The same readings Studio's plug-in host
//! publishes for its native editors, taken from the same DSP accessors.
//!
//! Only an insert someone is looking at measures anything: the control
//! thread sets [`InsertTelemetry::watched`], and the audio thread skips all
//! of it otherwise. The audio thread stores with relaxed atomics into fixed
//! arrays and never waits; the control thread reads whenever it likes. A
//! frame read while it is being written can mix two blocks' readings — fine
//! for something only ever drawn.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use builtin_dsp_core::spectrum::{SPECTRUM_BINS, SpectrumAnalyzer, quantize_db};
use serde::Serialize;

use crate::builtin_fx::BuiltinFx;
use crate::graph::AtomicF32;

/// Rack positions a built-in may report levels for (MixStation's rack).
pub const RACK_SLOTS: usize = 6;
/// Bands a multiband built-in reports a reduction for (the Compressor).
pub const REDUCTION_BANDS: usize = 4;
/// Bands a stereo image is measured in (the Imager).
pub const IMAGE_BANDS: usize = 4;
/// Interleaved left/right samples in one vectorscope frame.
pub const SCOPE_SAMPLES: usize = 256;
/// Pitch readings block (WhiteSharp's `telemetry::SLOTS`).
pub const PITCH_SLOTS: usize = 64;

/// A built-in's own level meters: linear levels after its trims, the
/// reduction it is applying (dB, positive), clip latches, and per rack
/// position levels for a built-in with a user-ordered rack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct LevelFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub gain_reduction_db: f32,
    pub in_clip: bool,
    pub out_clip: bool,
    pub slot_in_peak: [f32; RACK_SLOTS],
    pub slot_out_peak: [f32; RACK_SLOTS],
}

/// The Imager's measurement of its output image.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageFrame {
    /// −1 (one side inverted) … +1 (mono); 0 while too quiet to tell.
    pub correlation: f32,
    pub band_correlation: [f32; IMAGE_BANDS],
    /// Output RMS per band, linear.
    pub band_level: [f32; IMAGE_BANDS],
    /// Decimated output, interleaved left/right, oldest first.
    pub scope: Vec<f32>,
}

/// Everything new since the last [`InsertTelemetry::take`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TelemetryFrame {
    /// Absent for a built-in that does not meter itself (the EQs, the
    /// reverb and delay, WhiteSharp): not measured is not silence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub levels: Option<LevelFrame>,
    /// The signal arriving at the insert: 128 log-spaced bins, 20 Hz–20 kHz,
    /// each `0` (−100 dB) … `255` (0 dB). Only when a new analysis ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectrum: Option<Vec<u8>>,
    /// Reduction per band, dB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band_reduction: Option<[f32; REDUCTION_BANDS]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageFrame>,
    /// WhiteSharp's newest pitch readings (`whitesharp::telemetry` layout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<Vec<f32>>,
}

fn atomics<const N: usize>() -> [AtomicF32; N] {
    std::array::from_fn(|_| AtomicF32::new(0.0))
}

/// One insert's published readings. Lives in its `InsertCell`.
pub struct InsertTelemetry {
    /// Set by the control thread while an editor shows this insert.
    pub watched: AtomicBool,
    // Each group has a sequence number the audio thread bumps after writing
    // it, and the control thread remembers the last one it took.
    level_seq: AtomicU32,
    level_taken: AtomicU32,
    levels: [AtomicF32; 5 + 2 * RACK_SLOTS],
    clips: [AtomicBool; 2],
    spectrum_seq: AtomicU32,
    spectrum_taken: AtomicU32,
    spectrum: [AtomicF32; SPECTRUM_BINS],
    bands_seq: AtomicU32,
    bands_taken: AtomicU32,
    bands: [AtomicF32; REDUCTION_BANDS],
    image_seq: AtomicU32,
    image_taken: AtomicU32,
    image: [AtomicF32; 1 + 2 * IMAGE_BANDS + SCOPE_SAMPLES],
    pitch_seq: AtomicU32,
    pitch_taken: AtomicU32,
    pitch: [AtomicF32; PITCH_SLOTS],
}

impl Default for InsertTelemetry {
    fn default() -> Self {
        Self {
            watched: AtomicBool::new(false),
            level_seq: AtomicU32::new(0),
            level_taken: AtomicU32::new(0),
            levels: atomics(),
            clips: [AtomicBool::new(false), AtomicBool::new(false)],
            spectrum_seq: AtomicU32::new(0),
            spectrum_taken: AtomicU32::new(0),
            spectrum: atomics(),
            bands_seq: AtomicU32::new(0),
            bands_taken: AtomicU32::new(0),
            bands: atomics(),
            image_seq: AtomicU32::new(0),
            image_taken: AtomicU32::new(0),
            image: atomics(),
            pitch_seq: AtomicU32::new(0),
            pitch_taken: AtomicU32::new(0),
            pitch: atomics(),
        }
    }
}

fn bump(seq: &AtomicU32) {
    seq.fetch_add(1, Ordering::Release);
}

/// Whether `seq` moved since `taken`; marks it taken.
fn fresh(seq: &AtomicU32, taken: &AtomicU32) -> bool {
    let now = seq.load(Ordering::Acquire);
    taken.swap(now, Ordering::Relaxed) != now
}

impl InsertTelemetry {
    pub fn is_watched(&self) -> bool {
        self.watched.load(Ordering::Relaxed)
    }

    /// After a block: store what `fx` measured. Audio thread; no allocation.
    /// `analyzer` was fed the block before the effect ran.
    pub fn publish(&self, fx: &mut BuiltinFx, analyzer: Option<&mut SpectrumAnalyzer>) {
        if let Some(frame) = fx.level_frame() {
            let scalars = [
                frame.in_peak,
                frame.in_rms,
                frame.out_peak,
                frame.out_rms,
                frame.gain_reduction_db,
            ];
            let values = scalars
                .iter()
                .chain(frame.slot_in_peak.iter())
                .chain(frame.slot_out_peak.iter());
            for (cell, value) in self.levels.iter().zip(values) {
                cell.store(*value);
            }
            // Latched until taken, so a one-block clip is not missed.
            if frame.in_clip {
                self.clips[0].store(true, Ordering::Relaxed);
            }
            if frame.out_clip {
                self.clips[1].store(true, Ordering::Relaxed);
            }
            bump(&self.level_seq);
        }
        if let Some(bins) = analyzer.and_then(|a| a.analyze()) {
            for (cell, db) in self.spectrum.iter().zip(bins.iter()) {
                cell.store(*db);
            }
            bump(&self.spectrum_seq);
        }
        if let Some(bands) = fx.band_reduction() {
            for (cell, db) in self.bands.iter().zip(bands.iter()) {
                cell.store(*db);
            }
            bump(&self.bands_seq);
        }
        if let Some(image) = fx.take_image() {
            let values = std::iter::once(&image.correlation)
                .chain(image.band_correlation.iter())
                .chain(image.band_level.iter())
                .chain(image.scope.iter());
            for (cell, value) in self.image.iter().zip(values) {
                cell.store(*value);
            }
            bump(&self.image_seq);
        }
        if let Some(pitch) = fx.pitch_telemetry() {
            for (cell, value) in self.pitch.iter().zip(pitch.iter()) {
                cell.store(*value);
            }
            bump(&self.pitch_seq);
        }
    }

    /// What arrived since the last call. Control thread.
    pub fn take(&self) -> TelemetryFrame {
        let mut frame = TelemetryFrame::default();
        if fresh(&self.level_seq, &self.level_taken) {
            let v: Vec<f32> = self.levels.iter().map(AtomicF32::load).collect();
            let mut slot_in_peak = [0.0; RACK_SLOTS];
            let mut slot_out_peak = [0.0; RACK_SLOTS];
            slot_in_peak.copy_from_slice(&v[5..5 + RACK_SLOTS]);
            slot_out_peak.copy_from_slice(&v[5 + RACK_SLOTS..]);
            frame.levels = Some(LevelFrame {
                in_peak: v[0],
                in_rms: v[1],
                out_peak: v[2],
                out_rms: v[3],
                gain_reduction_db: v[4],
                in_clip: self.clips[0].swap(false, Ordering::Relaxed),
                out_clip: self.clips[1].swap(false, Ordering::Relaxed),
                slot_in_peak,
                slot_out_peak,
            });
        }
        if fresh(&self.spectrum_seq, &self.spectrum_taken) {
            frame.spectrum = Some(
                self.spectrum
                    .iter()
                    .map(|db| quantize_db(db.load()))
                    .collect(),
            );
        }
        if fresh(&self.bands_seq, &self.bands_taken) {
            frame.band_reduction = Some(std::array::from_fn(|i| self.bands[i].load()));
        }
        if fresh(&self.image_seq, &self.image_taken) {
            let v: Vec<f32> = self.image.iter().map(AtomicF32::load).collect();
            frame.image = Some(ImageFrame {
                correlation: v[0],
                band_correlation: std::array::from_fn(|i| v[1 + i]),
                band_level: std::array::from_fn(|i| v[1 + IMAGE_BANDS + i]),
                scope: v[1 + 2 * IMAGE_BANDS..].to_vec(),
            });
        }
        if fresh(&self.pitch_seq, &self.pitch_taken) {
            frame.pitch = Some(self.pitch.iter().map(AtomicF32::load).collect());
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .map(|i| (i as f32 * 0.06).sin() * amplitude)
            .collect()
    }

    /// A watched compressor reports its levels and reduction; a second take
    /// with nothing new reports nothing.
    #[test]
    fn a_dynamics_insert_publishes_levels_once() {
        let mut fx = BuiltinFx::new("fa76", 48_000).unwrap();
        fx.apply_wire_param(fa76::ipc::INPUT_INDEX, 30.0);
        let telemetry = InsertTelemetry::default();
        let mut left = tone(256, 0.8);
        let mut right = left.clone();
        for _ in 0..8 {
            fx.process(&mut left, &mut right);
            telemetry.publish(&mut fx, None);
        }
        let frame = telemetry.take();
        let levels = frame.levels.expect("FA-76 meters itself");
        assert!(levels.in_peak > 0.1, "input level {}", levels.in_peak);
        assert!(levels.gain_reduction_db > 0.0, "driven hard, it reduces");
        assert_eq!(telemetry.take(), TelemetryFrame::default());
    }

    /// The analyser publishes a frame once enough audio has arrived, and an
    /// EQ, which meters nothing itself, publishes only that.
    #[test]
    fn an_eq_publishes_the_spectrum_only() {
        let mut fx = BuiltinFx::new("equz8", 48_000).unwrap();
        let mut analyzer = SpectrumAnalyzer::new(48_000.0);
        let telemetry = InsertTelemetry::default();
        for _ in 0..16 {
            let mut left = tone(256, 0.5);
            let mut right = left.clone();
            analyzer.push_block(&left, &right);
            fx.process(&mut left, &mut right);
            telemetry.publish(&mut fx, Some(&mut analyzer));
        }
        let frame = telemetry.take();
        assert!(frame.levels.is_none());
        let bins = frame.spectrum.expect("an analysis ran");
        assert_eq!(bins.len(), SPECTRUM_BINS);
        assert!(bins.iter().any(|b| *b > 100), "the tone shows");
    }
}
