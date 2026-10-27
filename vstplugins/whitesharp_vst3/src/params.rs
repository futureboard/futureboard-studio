//! The host-facing parameters: one per WhiteSharp wire param, in wire order,
//! built from the DSP crate's own descriptor so ranges, defaults and names
//! cannot drift from what the DSP takes.
//!
//! Param IDs are the wire IDs (`ipc::UI_PARAM_IDS`): they are the persisted
//! contract, here as in Studio's projects.

use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_gpui::EditorContext;
use whitesharp::ipc::{self, PARAM_COUNT};
use whitesharp::{InputType, LatencyMode, Scale, VibratoShape, NOTE_NAMES};

enum Slot {
    Bool(BoolParam),
    Int(IntParam),
    Float(FloatParam),
}

impl Slot {
    fn plain(&self) -> f32 {
        match self {
            Slot::Bool(param) => f32::from(u8::from(param.value())),
            Slot::Int(param) => param.value() as f32,
            Slot::Float(param) => param.value(),
        }
    }

    fn as_ptr(&self) -> ParamPtr {
        match self {
            Slot::Bool(param) => param.as_ptr(),
            Slot::Int(param) => param.as_ptr(),
            Slot::Float(param) => param.as_ptr(),
        }
    }
}

pub struct WhiteSharpParams {
    /// Never resized after construction: the host holds pointers into it.
    slots: Box<[Slot]>,
}

impl Default for WhiteSharpParams {
    fn default() -> Self {
        let descriptor = whitesharp::descriptor();
        let slots: Box<[Slot]> = ipc::UI_PARAM_IDS
            .iter()
            .map(|id| {
                let param = descriptor
                    .params
                    .iter()
                    .find(|param| param.id == *id)
                    .expect("every wire param has a descriptor entry");
                slot(
                    id,
                    param.name,
                    param.default_value,
                    param.min,
                    param.max,
                    param.unit,
                )
            })
            .collect();
        assert_eq!(slots.len(), PARAM_COUNT);
        Self { slots }
    }
}

fn slot(id: &str, name: &str, default: f32, min: f32, max: f32, unit: &str) -> Slot {
    let int = |labels: Option<Arc<dyn Fn(i32) -> String + Send + Sync>>| {
        let param = IntParam::new(
            name,
            default.round() as i32,
            IntRange::Linear {
                min: min.round() as i32,
                max: max.round() as i32,
            },
        );
        match labels {
            Some(labels) => param.with_value_to_string(labels),
            None => param,
        }
    };
    match unit {
        "bool" => Slot::Bool(BoolParam::new(name, default >= 0.5)),
        "note" => Slot::Int(int(Some(Arc::new(|v| {
            NOTE_NAMES[v.clamp(0, 11) as usize].to_string()
        })))),
        "enum" => {
            let labels: Arc<dyn Fn(i32) -> String + Send + Sync> = match id {
                "scale" => Arc::new(|v| Scale::from_wire(v as f32).label().to_string()),
                "inputType" => Arc::new(|v| InputType::from_wire(v as f32).label().to_string()),
                "vibratoShape" => {
                    Arc::new(|v| VibratoShape::from_wire(v as f32).label().to_string())
                }
                "latency" => Arc::new(|v| LatencyMode::from_wire(v as f32).label().to_string()),
                _ => Arc::new(|v| v.to_string()),
            };
            let param = int(Some(labels));
            // Both move the plug-in's latency: a host can't follow that
            // from automation mid-playback.
            Slot::Int(if matches!(id, "inputType" | "latency") {
                param.non_automatable()
            } else {
                param
            })
        }
        // A 12-bit set of pitch classes, edited on the keyboard.
        "mask" => Slot::Int(int(None).non_automatable()),
        "st" => Slot::Int(int(None).with_unit(" st")),
        unit => Slot::Float(
            FloatParam::new(name, default, FloatRange::Linear { min, max })
                .with_unit(match unit {
                    "ms" => " ms",
                    "%" => " %",
                    "dB" => " dB",
                    "cents" => " ct",
                    "Hz" => " Hz",
                    _ => "",
                })
                .with_value_to_string(formatters::v2s_f32_rounded(1)),
        ),
    }
}

impl WhiteSharpParams {
    /// Every param's plain value, in wire order.
    pub fn wire_values(&self) -> [f32; PARAM_COUNT] {
        std::array::from_fn(|index| self.slots[index].plain())
    }

    /// The params as the DSP crate's struct, for the editor.
    pub fn to_dsp_params(&self) -> whitesharp::Params {
        let mut params = whitesharp::default_params();
        for (index, value) in self.wire_values().into_iter().enumerate() {
            ipc::apply_wire_param(&mut params, index as u32, value);
        }
        params
    }

    /// An editor edit: one complete gesture, so the host records it.
    pub fn set_from_editor(&self, context: &EditorContext, index: u32, value: f32) {
        let Some(slot) = self.slots.get(index as usize) else {
            return;
        };
        let setter = context.setter();
        match slot {
            Slot::Bool(param) => {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, value >= 0.5);
                setter.end_set_parameter(param);
            }
            Slot::Int(param) => {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, value.round() as i32);
                setter.end_set_parameter(param);
            }
            Slot::Float(param) => {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, value);
                setter.end_set_parameter(param);
            }
        }
    }
}

unsafe impl Params for WhiteSharpParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        ipc::UI_PARAM_IDS
            .iter()
            .zip(self.slots.iter())
            .map(|(id, slot)| (id.to_string(), slot.as_ptr(), String::new()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_dsp() {
        let params = WhiteSharpParams::default();
        assert_eq!(params.to_dsp_params(), whitesharp::default_params());
    }

    #[test]
    fn ids_are_the_wire_ids_in_order() {
        let params = WhiteSharpParams::default();
        let ids: Vec<String> = params.param_map().into_iter().map(|(id, ..)| id).collect();
        assert_eq!(ids, ipc::UI_PARAM_IDS.map(String::from));
    }
}
