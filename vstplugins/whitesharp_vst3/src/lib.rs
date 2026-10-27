//! WhiteSharp as a VST3 and CLAP plug-in.
//!
//! The DSP is Futureboard Studio's `whitesharp` crate and the editor is
//! Studio's own WhiteSharp window, compiled from its source tree. The modules
//! declared with `#[path]` are those Studio files, mounted at the paths their
//! `crate::` imports name; the rest of each such module tree is small local
//! stand-ins for the Studio services the editor touches.

use std::num::NonZeroU32;
use std::sync::Arc;

use builtin_dsp_core::StereoEffect;
use nih_plug::prelude::*;
use nih_plug_gpui::GpuiState;
use whitesharp::ipc::PARAM_COUNT;
use whitesharp::telemetry;

mod editor;
mod params;
mod shared;

use params::WhiteSharpParams;
use shared::{BlockMeter, Shared};

// Studio's UI modules, under the names its editor files import them by.
#[allow(dead_code, unused_imports)]
#[path = "../../../crates/SphereUIComponents/src/assets.rs"]
mod assets;
#[allow(dead_code, unused_imports)]
#[path = "../../../crates/SphereUIComponents/src/color.rs"]
mod color;
#[allow(dead_code, unused_imports)]
mod components;
#[allow(dead_code, unused_imports)]
mod fonts;
#[allow(dead_code, unused_imports)]
#[path = "../../../crates/SphereUIComponents/src/theme.rs"]
mod theme;

// Stand-ins for the Studio services those modules reach for.
mod audio_tap;
mod keymap;
mod native_win32_menu;
mod paths;
mod platform_chrome;
mod window_position;

// The visualizer analysis reads Studio's master-bus tap as
// `DirectAudio::visualizer_tap()`, the audio engine's crate name.
extern crate self as DirectAudio;
use audio_tap::visualizer_tap;

pub struct WhiteSharp {
    params: Arc<WhiteSharpParams>,
    shared: Arc<Shared>,
    editor_state: Arc<GpuiState>,
    dsp: whitesharp::Dsp,
    /// The plain value of every param as the DSP last took it, in wire order.
    applied: [f32; PARAM_COUNT],
    latency: u32,
    /// The telemetry counter last handed to the editor.
    published: f32,
}

impl Default for WhiteSharp {
    fn default() -> Self {
        let params = Arc::new(WhiteSharpParams::default());
        let mut dsp = whitesharp::Dsp::new(48_000.0);
        let applied = params.wire_values();
        apply_all(&mut dsp, &applied);
        Self {
            params,
            shared: Arc::new(Shared::default()),
            editor_state: editor::default_state(),
            dsp,
            applied,
            latency: 0,
            published: f32::NAN,
        }
    }
}

fn apply_all(dsp: &mut whitesharp::Dsp, values: &[f32; PARAM_COUNT]) {
    for (index, value) in values.iter().enumerate() {
        dsp.apply_wire_param(index as u32, *value);
    }
}

impl WhiteSharp {
    /// Hands the DSP every param the host or the editor moved since the last
    /// block. Allocation-free: a compare and, on a change, a wire update.
    fn sync_params(&mut self) {
        for (index, value) in self.params.wire_values().into_iter().enumerate() {
            if value != self.applied[index] {
                self.applied[index] = value;
                self.dsp.apply_wire_param(index as u32, value);
            }
        }
    }

    /// Reports the DSP's delay for delay compensation when it moved: input
    /// type and latency mode both change it.
    fn report_latency(&mut self, set: impl FnOnce(u32)) {
        let latency = self.dsp.latency_samples() as u32;
        if latency != self.latency {
            self.latency = latency;
            self.shared.set_latency(latency);
            set(latency);
        }
    }
}

impl Plugin for WhiteSharp {
    const NAME: &'static str = "WhiteSharp";
    const VENDOR: &'static str = "Futureboard";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    // The DSP smooths every param it takes itself.
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.params.clone(),
            self.shared.clone(),
            self.editor_state.clone(),
        )
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        // Rebuilds the DSP's buffers for the rate: not realtime.
        self.dsp.set_sample_rate(buffer_config.sample_rate);
        self.applied = self.params.wire_values();
        apply_all(&mut self.dsp, &self.applied);
        self.shared
            .set_stream(buffer_config.sample_rate, buffer_config.max_buffer_size);
        self.latency = u32::MAX;
        self.report_latency(|latency| context.set_latency_samples(latency));
        true
    }

    fn reset(&mut self) {
        self.dsp.reset();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        self.sync_params();
        self.report_latency(|latency| context.set_latency_samples(latency));

        let mut meter = BlockMeter::default();
        for mut frame in buffer.iter_samples() {
            let mut channels = frame.iter_mut();
            match (channels.next(), channels.next()) {
                (Some(left), Some(right)) => {
                    meter.input(*left, *right);
                    let (l, r) = self.dsp.process_stereo(*left, *right);
                    *left = l;
                    *right = r;
                    meter.output(l, r);
                }
                (Some(mono), None) => {
                    meter.input(*mono, *mono);
                    let (out, _) = self.dsp.process_stereo(*mono, *mono);
                    *mono = out;
                    meter.output(out, out);
                }
                _ => {}
            }
        }
        self.shared.publish_meter(&meter);

        if self.editor_state.is_open() {
            let slots = self.dsp.telemetry();
            if slots[0] != self.published {
                self.published = slots[0];
                self.shared.publish_telemetry(&slots);
            }
        }

        ProcessStatus::Normal
    }
}

const _: () = assert!(telemetry::SLOTS == shared::TELEMETRY_SLOTS);

impl ClapPlugin for WhiteSharp {
    const CLAP_ID: &'static str = "com.futureboard.whitesharp";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Realtime pitch correction");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::PitchCorrection,
        ClapFeature::Stereo,
        ClapFeature::Mono,
    ];
}

impl Vst3Plugin for WhiteSharp {
    const VST3_CLASS_ID: [u8; 16] = *b"FbWhiteSharpVst3";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::PitchShift];
}

nih_export_clap!(WhiteSharp);
nih_export_vst3!(WhiteSharp);
