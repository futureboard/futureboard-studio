//! Solfege Voxtune as a VST3 plug-in, with ARA 2.
//!
//! The engine is `sphere_voxtune` and the editor is Studio's Voxtune editor,
//! compiled from its source tree (`#[path]` modules below); the rest of each
//! mounted module tree is small stand-ins for Studio services.
//!
//! Two ways to work:
//!
//! - **ARA** (Studio One, Cubase, Logic, Reaper, Futureboard Studio …): the
//!   host hands the take over through ARA ([`ara`]); every region the host
//!   assigns this instance plays tuned, in sync with the arrangement.
//! - **Plain VST3**: open a take in the editor and align it to the
//!   timeline; it plays in place of the track's input while the host plays.
//!   The take's path, alignment and edits are saved with the host project.

use std::num::NonZeroU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_gpui::GpuiState;
use parking_lot::RwLock;
use sphere_voxtune::shared::Shared;

pub mod ara;
mod editor;
pub mod instance;

use instance::{Instance, PersistedTake};

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
mod keymap;
mod native_win32_menu;
mod paths;

#[derive(Params)]
pub struct VoxParams {
    /// The take (plain VST3 mode): path, alignment and edits.
    #[persist = "take"]
    pub take: RwLock<Option<PersistedTake>>,
    #[persist = "editor-state"]
    pub editor_state: Arc<GpuiState>,
}

pub struct Voxtune {
    params: Arc<VoxParams>,
    instance: Arc<Instance>,
    /// The audio thread's copy of the take engine, refreshed on change.
    engine: Arc<Shared>,
    engine_generation: u64,
    /// Interleaved stereo render buffer, sized in `initialize`.
    scratch: Vec<f32>,
    /// The ARA extension instance; lives as long as the plug-in.
    binding: Option<Box<ara::Binding>>,
    /// The take path last loaded, to reload only when it or the rate changes.
    loaded: Option<(std::path::PathBuf, u32)>,
}

impl Default for Voxtune {
    fn default() -> Self {
        let instance = Arc::new(Instance::default());
        Self {
            params: Arc::new(VoxParams {
                take: RwLock::new(None),
                editor_state: editor::default_state(),
            }),
            engine: instance.engine(),
            engine_generation: 0,
            instance,
            scratch: Vec::new(),
            binding: None,
            loaded: None,
        }
    }
}

impl Plugin for Voxtune {
    const NAME: &'static str = "Solfege Voxtune";
    const VENDOR: &'static str = "Futureboard";
    const URL: &'static str = "https://futureboard.studio";
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

    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.instance.clone())
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        let rate = buffer_config.sample_rate.round() as u32;
        self.instance.host_rate.store(rate, Ordering::Relaxed);
        let _ = ara::HOST_RATE.compare_exchange(0, rate, Ordering::Relaxed, Ordering::Relaxed);
        self.scratch = vec![0.0; buffer_config.max_buffer_size as usize * 2];
        // A saved take (or a rate change) loads it at the host's rate.
        if !self.instance.ara_bound() {
            let take = self.params.take.read().clone();
            if let Some(take) = take {
                let key = (take.path.clone(), rate);
                if self.loaded.as_ref() != Some(&key) {
                    self.loaded = Some(key);
                    self.instance.load_take(take.path, take.offset_sec, take.doc);
                }
            }
        }
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let transport = context.transport();
        let rate = transport.sample_rate;
        let playing = transport.playing;
        let pos = transport
            .pos_samples()
            .map(|s| s as f64 / rate as f64)
            .unwrap_or(0.0);
        self.instance.publish_transport(playing, pos);
        let frames = buffer.samples();
        if frames == 0 || self.scratch.len() < frames * 2 {
            return ProcessStatus::Normal;
        }

        // ARA: this instance plays the regions the host assigned it.
        if let Some(binding) = self.binding.as_ref().filter(|b| b.shared.plays()) {
            let channels = buffer.as_slice();
            let (left, rest) = channels.split_at_mut(1);
            let right = rest.first_mut().map(|r| &mut **r);
            ara::render_regions(
                &binding.shared,
                pos,
                playing,
                rate,
                &mut left[0][..frames],
                right.map(|r| &mut r[..frames]),
                &mut self.scratch,
            );
            return ProcessStatus::Normal;
        }

        // Plain VST3: the loaded take, aligned to the timeline.
        let generation = self.instance.generation.load(Ordering::Acquire);
        if generation != self.engine_generation {
            if let Some(engine) = self.instance.try_engine() {
                self.engine = engine;
                self.engine_generation = generation;
            }
        }
        let len = self.instance.take_len();
        if len <= 0.0 || (self.engine.sample_rate() - rate).abs() > 0.5 {
            return ProcessStatus::Normal;
        }
        let take_sec = pos - self.instance.offset();
        let block = frames as f64 / rate as f64;
        let in_take = playing && take_sec + block > 0.0 && take_sec < len;
        let out = &mut self.scratch[..frames * 2];
        if !self.engine.render_following(take_sec, in_take, out) {
            return ProcessStatus::Normal;
        }
        let channels = buffer.as_slice();
        let stereo = channels.len() >= 2;
        for (c, channel) in channels.iter_mut().enumerate() {
            for (i, s) in channel[..frames].iter_mut().enumerate() {
                let v = if stereo {
                    out[i * 2 + c.min(1)]
                } else {
                    (out[i * 2] + out[i * 2 + 1]) * 0.5
                };
                // Inside the take the tuned take replaces the input; outside
                // it (and while stopped, for note previews) it is added.
                *s = if in_take { v } else { *s + v };
            }
        }
        ProcessStatus::Normal
    }
}

impl Vst3Plugin for Voxtune {
    const VST3_CLASS_ID: [u8; 16] = *b"FbSolfVoxtune001";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::PitchShift,
    ];
    const VST3_ARA_FACTORY_CLASS_ID: Option<[u8; 16]> = Some(*b"FbSolfVoxtuneARA");

    fn ara_factory() -> *const std::ffi::c_void {
        ara::factory()
    }

    fn ara_bind_to_document_controller(
        &mut self,
        document_controller_ref: *mut std::ffi::c_void,
        known_roles: i32,
        assigned_roles: i32,
    ) -> *const std::ffi::c_void {
        if self.binding.is_some() {
            // ARA binds once per instance.
            return std::ptr::null();
        }
        let Some(binding) =
            (unsafe { ara::bind(document_controller_ref, known_roles, assigned_roles) })
        else {
            return std::ptr::null();
        };
        *self.instance.ara.lock() = Some(binding.shared.clone());
        let extension = binding.extension();
        self.binding = Some(binding);
        extension
    }
}

nih_export_vst3!(Voxtune);
