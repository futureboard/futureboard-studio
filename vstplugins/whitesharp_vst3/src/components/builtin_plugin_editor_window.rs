//! The host-ops types Studio's built-in editors are written against, with
//! just the sources the WhiteSharp editor uses.

use std::sync::Arc;

pub const BUILTIN_PAD_SLOTS: usize = crate::shared::TELEMETRY_SLOTS;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginInstanceKey {
    pub track_id: String,
    pub insert_id: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BuiltinMeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

pub type BuiltinParamForwarder = Arc<dyn Fn(&PluginInstanceKey, u32, f32, &mut gpui::App)>;
pub type BuiltinMeterSource = Arc<dyn Fn(&PluginInstanceKey) -> Option<BuiltinMeterFrame>>;
/// `(sample_rate, block_frames, latency_samples, tempo_bpm)`.
pub type BuiltinHostStatusSource = Arc<dyn Fn(&PluginInstanceKey) -> Option<(u32, u32, u32, f64)>>;
/// `(publish sequence, telemetry slots)`.
pub type BuiltinPadLevelSource =
    Arc<dyn Fn(&PluginInstanceKey) -> Option<(u32, [f32; BUILTIN_PAD_SLOTS])>>;

#[derive(Clone, Default)]
pub struct BuiltinEditorHostOps {
    pub forward_param: Option<BuiltinParamForwarder>,
    pub meter_source: Option<BuiltinMeterSource>,
    pub host_status_source: Option<BuiltinHostStatusSource>,
    pub pad_level_source: Option<BuiltinPadLevelSource>,
}
