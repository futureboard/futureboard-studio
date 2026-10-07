//! Slicer — Futureboard's built-in beat slicer.
//!
//! One audio file cut into slices — at its hits, on its beat grid, or into
//! equal lengths, then moved by hand — with one slice per key from C3 up.
//! A slice can choke the one before it, as hits in one loop do, and every
//! slice plays through the same envelope, filter, pitch and output.
//!
//! The voices are Quick Sampler's engine, played a slice at a time
//! ([`quicksampler::Dsp::note_on_region`]); this crate adds the slices, the
//! key map and the choke. Like every built-in it runs in the plug-in host:
//! wire params arrive through [`Dsp::apply_wire_param`], MIDI through
//! [`Dsp::note_on_channel`] and friends, and a decoded sample through the
//! same wait-free [`SampleLoader`] Quick Sampler uses.
//!
//! The cut itself is worked out by the editor ([`slicing`]) and sent as
//! slice points; the DSP only reads them.

pub mod ipc;
mod params;
pub mod slicing;
pub mod telemetry;
pub mod ui;

use serde::{Deserialize, Serialize};

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use params::{
    BeatDivision, DEFAULT_FIRST_KEY, MAX_SAMPLE_BPM, MAX_SLICES, MIN_EQUAL_SLICES, MIN_SAMPLE_BPM,
    SliceMode, SlicerParams, sanitize_bpm,
};
pub use quicksampler::{MeterFrame, SampleData, SampleLoader};

pub const PLUGIN_ID: &str = "futureboard.slicer";
pub const PLUGIN_NAME: &str = "Slicer";

/// Everything the plug-in persists: the cut, how it plays, and which file it
/// cuts. The audio is re-read from the plug-in's Samples folder by name and
/// sent again whenever the DSP (re)starts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Params {
    /// File name inside the plug-in's Samples folder. Persistence only.
    pub sample_name: Option<String>,
    pub slicer: SlicerParams,
}

pub fn default_params() -> Params {
    Params::default()
}

pub struct Dsp {
    voices: quicksampler::Dsp,
    slicer: SlicerParams,
}

impl std::fmt::Debug for Dsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dsp")
            .field("slices", &self.slicer.slice_count)
            .field("voices", &self.voices)
            .finish()
    }
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let slicer = SlicerParams::default();
        let mut voices = quicksampler::Dsp::new(sample_rate);
        voices.set_sampler_params(slicer.voice);
        Self { voices, slicer }
    }

    pub fn params(&self) -> &SlicerParams {
        &self.slicer
    }

    /// Restores persisted state. Control thread, before the DSP is published.
    pub fn set_params(&mut self, params: Params) {
        self.slicer = params.slicer.sanitized();
        self.voices.set_sampler_params(self.slicer.voice);
    }

    /// Applies one wire edit. Allocation-free: a field write, and for a voice
    /// setting the engine's derived constants.
    pub fn apply_wire_param(&mut self, index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.slicer, index, value) {
            return false;
        }
        if index < ipc::SLICE_COUNT_WIRE {
            self.voices.set_sampler_params(self.slicer.voice);
        }
        true
    }

    /// The control-side handle the host submits decoded samples through.
    pub fn sample_loader(&self) -> SampleLoader {
        self.voices.sample_loader()
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) {
        self.note_on_channel(0, note, velocity);
    }

    /// Plays the slice on `note`. A key with no slice is silent.
    pub fn note_on_channel(&mut self, channel: u8, note: u8, velocity: u8) {
        if velocity == 0 {
            self.note_off_channel(channel, note);
            return;
        }
        let Some((start, end)) = self
            .slicer
            .slice_for_note(note)
            .and_then(|index| self.slicer.slice_region(index))
        else {
            return;
        };
        if self.slicer.choke {
            self.voices.choke();
        }
        self.voices
            .note_on_region(channel, note, velocity, start, end);
    }

    pub fn note_off(&mut self, note: u8) {
        self.note_off_channel(0, note);
    }

    /// Releases the slice on `note` — unless slices play one-shot, to their
    /// end whatever the note length.
    pub fn note_off_channel(&mut self, channel: u8, note: u8) {
        if !self.slicer.one_shot {
            self.voices.note_off_channel(channel, note);
        }
    }

    pub fn control_change(&mut self, channel: u8, controller: u8, value: u8) {
        self.voices.control_change(channel, controller, value);
    }

    pub fn pitch_bend(&mut self, channel: u8, value: u16) {
        self.voices.pitch_bend(channel, value);
    }

    pub fn all_notes_off(&mut self) {
        self.voices.all_notes_off();
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.active_voice_count()
    }

    /// The sounding slices' playheads, packed for the host to publish (see
    /// [`telemetry`]). Realtime-safe: no allocation.
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        let mut voices = [(0_u8, 0_u64, 0.0_f32); telemetry::PLAYHEADS];
        let count = self.voices.sounding(&mut voices);
        telemetry::encode(&voices[..count])
    }

    /// Renders `frames` interleaved stereo frames into `interleaved`.
    pub fn process_block(&mut self, interleaved: &mut [f32], frames: usize) {
        self.voices.process_block(interleaved, frames);
    }

    pub fn meter_frame(&self) -> MeterFrame {
        self.voices.meter_frame()
    }

    /// No lookahead: a note sounds on the sample it lands on.
    pub fn latency_samples(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Four bars of 1 s each, at levels 0.1, 0.2, 0.3, 0.4 — each slice
    /// tells by its level which one it is.
    fn steps() -> Arc<SampleData> {
        let samples: Vec<f32> = (0..4 * 8_000)
            .map(|i| 0.1 * (1 + i / 8_000) as f32)
            .collect();
        Arc::new(SampleData::from_interleaved(&samples, 1, 8_000).unwrap())
    }

    fn loaded(edit: impl FnOnce(&mut SlicerParams)) -> Dsp {
        let mut dsp = Dsp::new(8_000.0);
        let mut params = default_params();
        params.slicer = slicing::with_points(params.slicer, &slicing::equal_points(4), 0.0);
        params.slicer.voice.volume = 1.0;
        edit(&mut params.slicer);
        dsp.set_params(params);
        dsp.sample_loader().submit(Some(steps()));
        let mut out = vec![0.0; 2];
        dsp.process_block(&mut out, 1);
        dsp
    }

    fn render(dsp: &mut Dsp, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        dsp.process_block(&mut out, frames);
        out.chunks(2).map(|frame| frame[0]).collect()
    }

    #[test]
    fn each_key_from_c3_plays_its_own_slice() {
        let mut dsp = loaded(|_| {});
        dsp.note_on(DEFAULT_FIRST_KEY + 2, 127);
        let left = render(&mut dsp, 4_000);
        assert!((left[2_000] - 0.3).abs() < 1.0e-3, "{}", left[2_000]);
        // Below the first key, and past the last slice, nothing.
        let mut dsp = loaded(|_| {});
        dsp.note_on(DEFAULT_FIRST_KEY - 1, 127);
        dsp.note_on(DEFAULT_FIRST_KEY + 4, 127);
        assert_eq!(dsp.active_voice_count(), 0);
    }

    #[test]
    fn a_slice_stops_at_the_next_one() {
        let mut dsp = loaded(|_| {});
        dsp.note_on(DEFAULT_FIRST_KEY, 127);
        let left = render(&mut dsp, 9_000);
        assert!(left[7_000] > 0.05);
        assert_eq!(left[8_500], 0.0);
        assert_eq!(dsp.active_voice_count(), 0);
    }

    #[test]
    fn with_choke_a_new_slice_cuts_the_last() {
        let mut dsp = loaded(|_| {});
        dsp.note_on(DEFAULT_FIRST_KEY, 127);
        render(&mut dsp, 100);
        dsp.note_on(DEFAULT_FIRST_KEY + 3, 127);
        let left = render(&mut dsp, 2_000);
        assert_eq!(dsp.active_voice_count(), 1);
        assert!((left[1_000] - 0.4).abs() < 1.0e-3, "{}", left[1_000]);

        let mut open = loaded(|p| p.choke = false);
        open.note_on(DEFAULT_FIRST_KEY, 127);
        open.note_on(DEFAULT_FIRST_KEY + 3, 127);
        let left = render(&mut open, 2_000);
        assert_eq!(open.active_voice_count(), 2);
        assert!((left[1_000] - 0.5).abs() < 1.0e-3, "{}", left[1_000]);
    }

    #[test]
    fn one_shot_slices_ignore_the_note_off() {
        let mut dsp = loaded(|p| p.voice.release_ms = 0.0);
        dsp.note_on(DEFAULT_FIRST_KEY, 127);
        dsp.note_off(DEFAULT_FIRST_KEY);
        assert!(render(&mut dsp, 4_000)[3_000] > 0.05);

        let mut gated = loaded(|p| {
            p.one_shot = false;
            p.voice.release_ms = 0.0;
        });
        gated.note_on(DEFAULT_FIRST_KEY, 127);
        render(&mut gated, 100);
        gated.note_off(DEFAULT_FIRST_KEY);
        assert_eq!(render(&mut gated, 4_000)[3_000], 0.0);
    }

    #[test]
    fn the_telemetry_shows_where_each_sounding_slice_is() {
        let mut dsp = loaded(|_| {});
        assert!(telemetry::decode(&dsp.telemetry()).is_empty());
        dsp.note_on(DEFAULT_FIRST_KEY + 1, 127);
        render(&mut dsp, 4_000);
        let heads = telemetry::decode(&dsp.telemetry());
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].note, DEFAULT_FIRST_KEY + 1);
        // Half a second into the slice starting at 0.25 of a 4 s sample.
        assert!((heads[0].position - 0.375).abs() < 1.0e-3, "{heads:?}");
        // A retrigger is a new start.
        dsp.note_on(DEFAULT_FIRST_KEY + 1, 127);
        let again = telemetry::decode(&dsp.telemetry());
        assert_ne!(again[0].start, heads[0].start);
    }

    #[test]
    fn wire_edits_move_slices_and_shape_the_voice() {
        let mut dsp = loaded(|_| {});
        // Slice 2 now starts at 0.6, so slice 1 runs on into the third bar.
        assert!(dsp.apply_wire_param(ipc::FIRST_SLICE_WIRE + 2, 0.6));
        let volume = ipc::ui_param_index("volume").unwrap();
        assert!(dsp.apply_wire_param(volume, 0.5));
        dsp.note_on(DEFAULT_FIRST_KEY + 1, 127);
        let left = render(&mut dsp, 10_000);
        assert!((left[9_000] - 0.15).abs() < 1.0e-3, "{}", left[9_000]);
    }
}
