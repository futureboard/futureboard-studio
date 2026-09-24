use thiserror::Error;

use crate::device::{InferDevice, StemPlatform};
use crate::model::StemModel;
use crate::stems::StemKind;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StemExtractError {
    #[error("no input audio frames were provided")]
    EmptyInput,

    #[error("channel count must be 1 or 2 (got {0})")]
    UnsupportedChannels(usize),

    #[error("sample rate must be non-zero")]
    InvalidSampleRate,

    #[error("no output stems were selected")]
    NoStemsSelected,

    #[error("stem `{}` is not produced by model {}", stem.label(), model.label())]
    StemNotSupported { model: StemModel, stem: StemKind },

    #[error("device {} unavailable: {reason}", device.label())]
    DeviceUnavailable { device: InferDevice, reason: String },

    #[error("stem extraction is not yet supported on {}", platform.label())]
    UnsupportedPlatform { platform: StemPlatform },

    #[error("extraction cancelled")]
    Cancelled,

    #[error("{0}")]
    Backend(String),
}

impl StemExtractError {
    pub fn user_message(&self) -> String {
        self.to_string()
    }
}
