use std::fmt;

/// All errors that can originate from the SphereDirectAudioEngine.
#[derive(Debug)]
pub enum SphereAudioError {
    BackendUnavailable(String),
    DeviceNotFound(String),
    StreamOpenFailed(String),
    StreamStartFailed(String),
    EngineNotOpen,
    InvalidConfig(String),
    ProjectDeserialize(String),
    InvalidRoutingGraph(String),
    NativeError(String),
}

impl fmt::Display for SphereAudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable(s) => write!(f, "Audio backend unavailable: {s}"),
            Self::DeviceNotFound(s) => write!(f, "Device not found: {s}"),
            Self::StreamOpenFailed(s) => write!(f, "Stream open failed: {s}"),
            Self::StreamStartFailed(s) => write!(f, "Stream start failed: {s}"),
            Self::EngineNotOpen => write!(f, "Engine stream is not open"),
            Self::InvalidConfig(s) => write!(f, "Invalid configuration: {s}"),
            Self::ProjectDeserialize(s) => write!(f, "Project deserialization failed: {s}"),
            Self::InvalidRoutingGraph(s) => write!(f, "Invalid routing graph: {s}"),
            Self::NativeError(s) => write!(f, "Native error: {s}"),
        }
    }
}

impl std::error::Error for SphereAudioError {}

impl From<crate::audio_graph::GraphValidationError> for SphereAudioError {
    fn from(e: crate::audio_graph::GraphValidationError) -> Self {
        let cycle_detail = if e.cycles.is_empty() {
            String::new()
        } else {
            format!(" cycles={:?}", e.cycles)
        };
        Self::InvalidRoutingGraph(format!(
            "{}{} rejected_routes={}",
            e.message,
            cycle_detail,
            e.rejected_routes.len()
        ))
    }
}
