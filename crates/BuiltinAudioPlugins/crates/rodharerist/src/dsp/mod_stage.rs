//! The Mod slot: one of several modulation algorithms behind a single set of
//! Rate/Depth/Mix knobs (the same shared-knob pattern the Drive slot uses for
//! its pedal models).
//!
//! Every algorithm stays allocated so switching models never allocates on a
//! control-thread edit and never has to rebuild delay lines mid-session; only
//! the selected one is ticked on the audio thread.

use super::chorus::Chorus;
use super::flanger::Flanger;
use super::phaser::Phaser;
use super::tremolo::Tremolo;
use crate::dsp::ModModel;

#[derive(Debug, Clone)]
pub(super) struct ModStage {
    model: ModModel,
    chorus: Chorus,
    phaser: Phaser,
    flanger: Flanger,
    tremolo: Tremolo,
}

impl ModStage {
    pub(super) fn new(sample_rate: f32) -> Self {
        Self {
            model: ModModel::Chorus,
            chorus: Chorus::new(sample_rate),
            phaser: Phaser::new(sample_rate),
            flanger: Flanger::new(sample_rate),
            tremolo: Tremolo::new(sample_rate),
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.chorus.set_sample_rate(sample_rate);
        self.phaser.set_sample_rate(sample_rate);
        self.flanger.set_sample_rate(sample_rate);
        self.tremolo.set_sample_rate(sample_rate);
    }

    pub(super) fn reset(&mut self) {
        self.chorus.reset();
        self.phaser.reset();
        self.flanger.reset();
        self.tremolo.reset();
    }

    /// `rate`/`depth` are 0..10; `mix` is 0..100 % (the tremolo reads it as
    /// its Shape control). Only the selected model is configured — the others
    /// keep their last coefficients and are reconfigured on selection.
    pub(super) fn configure(&mut self, model: ModModel, rate: f32, depth: f32, mix: f32) {
        if self.model != model {
            self.model = model;
            // Stale delay-line/LFO state from the previous selection would
            // bleed into the first samples of the new sound. Moving between
            // two phaser voices is the exception: `Phaser::configure` swaps
            // the cascade itself and clears only what the new one cannot use.
            match model {
                ModModel::Chorus
                | ModModel::Dimension
                | ModModel::TriChorus
                | ModModel::RazorChorus => self.chorus.reset(),
                ModModel::Flanger
                | ModModel::ThruZero
                | ModModel::SlowJet
                | ModModel::MetalJet
                | ModModel::StabFlange => self.flanger.reset(),
                ModModel::Tremolo
                | ModModel::HarmonicTrem
                | ModModel::BiasTrem
                | ModModel::ChopTrem
                | ModModel::SquareTrem => self.tremolo.reset(),
                _ => {}
            }
        }
        match self.model {
            ModModel::Chorus => self.chorus.configure(rate, depth, mix),
            ModModel::Dimension => self.chorus.configure_voice(
                rate,
                depth,
                mix,
                super::chorus::ChorusVoice::dimension(),
            ),
            ModModel::TriChorus => {
                self.chorus
                    .configure_voice(rate, depth, mix, super::chorus::ChorusVoice::tri())
            }
            ModModel::RazorChorus => self.chorus.configure_voice(
                rate,
                depth,
                mix,
                super::chorus::ChorusVoice::razor(),
            ),
            ModModel::Flanger => self.flanger.configure(rate, depth, mix),
            ModModel::ThruZero => self.flanger.configure_voice(
                rate,
                depth,
                mix,
                super::flanger::FlangerVoice::thru_zero(),
            ),
            ModModel::SlowJet => {
                self.flanger
                    .configure_voice(rate, depth, mix, super::flanger::FlangerVoice::slow())
            }
            ModModel::MetalJet => self.flanger.configure_voice(
                rate,
                depth,
                mix,
                super::flanger::FlangerVoice::metal(),
            ),
            ModModel::StabFlange => self.flanger.configure_voice(
                rate,
                depth,
                mix,
                super::flanger::FlangerVoice::stab(),
            ),
            ModModel::Tremolo => self.tremolo.configure(rate, depth, mix),
            ModModel::HarmonicTrem => {
                self.tremolo
                    .configure_kind(rate, depth, mix, super::tremolo::TremKind::Harmonic)
            }
            ModModel::BiasTrem => {
                self.tremolo
                    .configure_kind(rate, depth, mix, super::tremolo::TremKind::Bias)
            }
            ModModel::ChopTrem => {
                self.tremolo
                    .configure_kind(rate, depth, mix, super::tremolo::TremKind::Chop)
            }
            ModModel::SquareTrem => {
                self.tremolo
                    .configure_kind(rate, depth, mix, super::tremolo::TremKind::Square)
            }
            other => {
                // Every remaining model is a voiced phaser.
                if let Some(voice) = other.phaser_voice() {
                    self.phaser.configure(voice, rate, depth, mix);
                }
            }
        }
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        match self.model {
            ModModel::Chorus
            | ModModel::Dimension
            | ModModel::TriChorus
            | ModModel::RazorChorus => self.chorus.process(left, right),
            ModModel::Flanger
            | ModModel::ThruZero
            | ModModel::SlowJet
            | ModModel::MetalJet
            | ModModel::StabFlange => self.flanger.process(left, right),
            ModModel::Tremolo
            | ModModel::HarmonicTrem
            | ModModel::BiasTrem
            | ModModel::ChopTrem
            | ModModel::SquareTrem => self.tremolo.process(left, right),
            _ => self.phaser.process(left, right),
        }
    }
}
