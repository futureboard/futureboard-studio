//! Rodhareist as a VST2, VST3, and CLAP plug-in.
//!
//! The DSP is Futureboard Studio's `rodharerist` crate, unchanged: the same
//! `Dsp`, wire table, factory presets and NAM/IR loaders a Studio insert
//! runs. The editor is Studio's own Rodhareist window, compiled from its
//! source tree. The modules declared with `#[path]` are those Studio files,
//! mounted at the paths their `crate::` imports name; the rest of each such
//! module tree is small local stand-ins for the Studio services the editor
//! touches.
//!
//! Threading follows the core's contract: parameter changes reach the `Dsp`
//! at block boundaries on the audio thread, as Studio's plug-in host applies
//! them; captures and IRs are built on the background task thread and adopted
//! lock-free at the next `Dsp::begin_block`.

use std::num::NonZeroU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use builtin_dsp_core::StereoEffect;
use nih_plug::prelude::*;
use nih_plug_gpui::GpuiState;
use rodharerist::{Dsp, StageKind, PATH_SLOTS};

mod editor;
mod embedded_assets;
mod files;
mod params;
mod shared;

use params::{EntryKind, RodhareistParams};
use shared::{MeterReading, PathOrder, Shared};

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
#[path = "../../../crates/SphereUIComponents/src/paths.rs"]
mod paths;
#[allow(dead_code, unused_imports)]
#[path = "../../../crates/SphereUIComponents/src/theme.rs"]
mod theme;

// Stand-ins for the Studio services those modules reach for.
mod keymap;
mod native_win32_menu;
mod platform_chrome;
mod window_position;

// The editor names Studio's plug-in host crate for its preset folder, the
// `.pst` format and the telemetry size; these modules stand in for it.
#[allow(dead_code)]
mod audio_bridge;
#[allow(dead_code)]
mod preset;
mod registry;
extern crate self as SpherePluginHost;

pub struct Rodhareist {
    params: Arc<RodhareistParams>,
    shared: Arc<Shared>,
    editor_state: Arc<GpuiState>,
    /// The id the editor's state mirror knows this instance by.
    insert_id: String,
    /// Built in `initialize`, once the sample rate is known.
    dsp: Option<Box<Dsp>>,
    sample_rate: f32,
    /// The value last applied to the `Dsp` for each wire entry, so a block
    /// only routes what changed.
    applied: Vec<f32>,
    /// Entry index of each `path_slot_*`.
    path_slots: [usize; PATH_SLOTS],
    amp_model: usize,
    tone_engine: usize,
    /// Mono input on a mono → stereo layout: the left channel is the signal.
    mono_input: bool,
    latency: u32,
    /// Host maximum block. The NAM chunk follows this, not each callback's
    /// length, so a short block does not move the reported latency.
    nam_block: usize,
    /// A path order we held back. Without an editor target, the same order
    /// again is what the host meant; a different one is still a move in
    /// progress.
    held_path: Option<PathOrder>,
}

impl Default for Rodhareist {
    fn default() -> Self {
        let params = Arc::new(RodhareistParams::default());
        let wire = &params.wire;
        let mut path_slots = [0; PATH_SLOTS];
        for (i, e) in wire.entries().iter().enumerate() {
            if let EntryKind::PathSlot(slot) = e.kind {
                path_slots[slot] = i;
            }
        }
        let amp_model = wire.index_of("amp_model").expect("amp_model");
        let tone_engine = wire.index_of("tone_engine").expect("tone_engine");
        let applied = vec![f32::NAN; wire.entries().len()];
        Self {
            insert_id: editor::insert_id(&params),
            params,
            shared: Arc::new(Shared::default()),
            editor_state: editor::default_state(),
            dsp: None,
            sample_rate: 0.0,
            applied,
            path_slots,
            amp_model,
            tone_engine,
            mono_input: false,
            latency: 0,
            nam_block: 1,
            held_path: None,
        }
    }
}

impl Rodhareist {
    /// Routes every host parameter that moved since the last block into the
    /// `Dsp`. Allocation-free: values are compared per block, and a string id
    /// is only matched for a value that actually changed.
    fn apply_param_changes(&mut self) {
        if self.dsp.is_none() {
            return;
        }
        let applied_path = self.applied_path();
        let mut path_changed = false;
        let mut amp_model_changed = false;
        {
            let Some(dsp) = self.dsp.as_deref_mut() else {
                return;
            };
            for (i, entry) in self.params.wire.entries().iter().enumerate() {
                let value = entry.param.plain();
                // The DSP puts the engine back on Classic when the amp model
                // moves; the host's `tone_engine` stays authoritative.
                let force = amp_model_changed && i == self.tone_engine;
                if value == self.applied[i] && !force {
                    continue;
                }
                match entry.kind {
                    // Remembered below, once the whole order is a real chain.
                    // A move writes several slots; storing one of them now would
                    // hide the rest of the move from the next block.
                    EntryKind::PathSlot(_) => path_changed = true,
                    EntryKind::Value => {
                        self.applied[i] = value;
                        dsp.apply_ui_param(entry.id, value);
                        amp_model_changed |= i == self.amp_model;
                    }
                }
            }
        }
        if !path_changed {
            self.held_path = None;
            return;
        }
        // The editor sends a move as one parameter per slot, and the host may
        // hand those writes over across any number of callbacks. Between
        // them a stage sits in two slots or, when the move lands on an empty
        // slot, in none. Hold the previous order until the order the editor
        // published for the edit has arrived. Without such a target (a
        // generic editor, a host edit) the same incomplete order on the next
        // block is deliberate (a removal, or a duplicate) and is applied
        // then; the DSP drops a real duplicate.
        let mut order = [None; PATH_SLOTS];
        for (slot, &entry) in self.path_slots.iter().enumerate() {
            let value = self.params.wire.entries()[entry].param.plain();
            order[slot] = StageKind::from_index(value.round() as i32);
        }
        if self.hold_path(&applied_path, &order) {
            return;
        }
        for &entry in &self.path_slots {
            self.applied[entry] = self.params.wire.entries()[entry].param.plain();
        }
        if let Some(dsp) = self.dsp.as_deref_mut() {
            dsp.set_path_order(order);
        }
    }

    /// Whether this block keeps the running chain instead of `order`. Ends
    /// the editor's chain edit once it is over. One atomic load, and a
    /// compare-exchange when the edit ends.
    fn hold_path(&mut self, applied: &PathOrder, order: &PathOrder) -> bool {
        let target = self.shared.path_target();
        let step = hold_path_order(&mut self.held_path, applied, order, target.as_ref());
        if step.target_done {
            if let Some(target) = &target {
                self.shared.finish_path_target(target);
            }
        }
        step.hold
    }

    fn applied_path(&self) -> PathOrder {
        let mut order = [None; PATH_SLOTS];
        for (slot, &entry) in self.path_slots.iter().enumerate() {
            let value = self.applied[entry];
            order[slot] = value
                .is_finite()
                .then(|| StageKind::from_index(value.round() as i32))
                .flatten();
        }
        order
    }
}

/// What one block does with the slot values it found.
#[derive(Debug, PartialEq, Eq)]
struct PathStep {
    /// Keep the chain that is running.
    hold: bool,
    /// The editor's chain edit is over: it arrived, or the slots left its
    /// way.
    target_done: bool,
}

/// Whether a block applies `next`, the chain the slot values describe.
///
/// With `target`, the order an editor edit ends in: every order on the way
/// from `previous` to it (each slot holding its old or its new stage) is
/// held, for as many blocks as the writes take, and `target` itself is
/// applied. Anything else ends that edit.
///
/// Without one: `next` is held back for one block when it is a move seen
/// halfway, a stage in two slots or a stage from `previous` gone. The same
/// `next` again is applied.
fn hold_path_order(
    held: &mut Option<PathOrder>,
    previous: &PathOrder,
    next: &PathOrder,
    target: Option<&PathOrder>,
) -> PathStep {
    let mut target_done = false;
    if let Some(target) = target {
        if next == target {
            *held = None;
            return PathStep {
                hold: false,
                target_done: true,
            };
        }
        let on_the_way =
            (0..PATH_SLOTS).all(|slot| next[slot] == previous[slot] || next[slot] == target[slot]);
        if on_the_way {
            *held = Some(*next);
            return PathStep {
                hold: true,
                target_done: false,
            };
        }
        target_done = true;
    }
    let hold = path_order_is_mid_move(previous, next) && *held != Some(*next);
    *held = hold.then_some(*next);
    PathStep { hold, target_done }
}

/// A duplicate stage, or a stage that left the chain without arriving
/// anywhere else. Empty slots are fine. Adding a stage is not a mid-move.
fn path_order_is_mid_move(previous: &PathOrder, next: &PathOrder) -> bool {
    let mut seen = [false; StageKind::COUNT];
    for stage in next.iter().flatten() {
        let index = stage.index() as usize;
        if index >= seen.len() || seen[index] {
            return true;
        }
        seen[index] = true;
    }
    previous
        .iter()
        .flatten()
        .any(|stage| !seen[stage.index() as usize])
}

impl Plugin for Rodhareist {
    const NAME: &'static str = "Rodhareist";
    const VENDOR: &'static str = "Futureboard Digital Technologies";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        // A guitar on a mono track; the rig's modulation, delay and reverb
        // are stereo.
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(2),
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

    // The executor holds the wrapper strongly; keeping it would leak every
    // instance. The editor loads files on its own thread (`editor.rs`).
    fn editor(&mut self, _executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.params.clone(),
            self.shared.clone(),
            self.editor_state.clone(),
        )
    }

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        let sample_rate = buffer_config.sample_rate;
        self.mono_input = audio_io_layout.main_input_channels == NonZeroU32::new(1);

        if self.dsp.is_none() || sample_rate != self.sample_rate {
            // A capture or IR is built for one engine rate: a new `Dsp` gets
            // new loaders, and `restore` below rebuilds both at this rate.
            let dsp = Box::new(Dsp::new(sample_rate));
            // Anything still building was aimed at the `Dsp` we just replaced.
            self.shared
                .install_control(dsp.nam_loader(), dsp.ir_loader(), f64::from(sample_rate));
            self.dsp = Some(dsp);
            self.sample_rate = sample_rate;
        }

        // A state load lands here too: take the whole parameter set as saved,
        // power included, without fades.
        let core = self.params.wire.to_core_params();
        // The NAM chunk is the host's maximum block, capped inside the DSP.
        // A callback shorter than that maximum must not change it: hosts that
        // split blocks would otherwise restart delay compensation every time.
        self.nam_block = (buffer_config.max_buffer_size as usize).max(1);
        if let Some(dsp) = self.dsp.as_deref_mut() {
            dsp.set_params(core);
            dsp.begin_block_with_frames(self.nam_block);
        }
        for (applied, entry) in self.applied.iter_mut().zip(self.params.wire.entries()) {
            *applied = entry.param.plain();
        }
        // The chain just applied is the one the host holds; an edit that was
        // still arriving is part of it or was replaced.
        self.held_path = None;
        self.shared.clear_path_target();
        files::restore(&self.shared, &self.params, &self.insert_id);
        // Titles follow the stomps in the state that just loaded. Not the
        // audio thread: initialize runs before processing. The host is told
        // once initialize has returned.
        if self.params.wire.refresh_host_names() {
            context.param_titles_changed();
        }

        self.shared
            .set_stream(sample_rate, buffer_config.max_buffer_size);
        self.latency = self
            .dsp
            .as_deref()
            .map_or(0, |dsp| dsp.latency_samples() as u32);
        self.shared.set_latency(self.latency);
        context.set_latency_samples(self.latency);
        true
    }

    fn reset(&mut self) {
        if let Some(dsp) = self.dsp.as_deref_mut() {
            dsp.reset();
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        self.apply_param_changes();
        let mono_input = self.mono_input;
        let Some(dsp) = self.dsp.as_deref_mut() else {
            return ProcessStatus::Normal;
        };
        if self.shared.clear_clip.swap(false, Ordering::Relaxed) {
            dsp.clear_clip();
        }

        // A queued capture or IR is adopted here, at the block boundary.
        // The chunk is the host maximum from `initialize`, not this
        // callback's length. A fixed 32-sample host therefore infers inside
        // 32 samples, and a host that varies the callback keeps one latency.
        dsp.begin_block_with_frames(self.nam_block);
        if let Some(tempo) = context.transport().tempo {
            dsp.set_tempo_bpm(tempo as f32);
        }
        match buffer.as_slice() {
            [left, right, ..] => {
                if mono_input {
                    right.copy_from_slice(left);
                }
                for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                    (*l, *r) = dsp.process_stereo(*l, *r);
                }
            }
            [mono] => {
                for s in mono.iter_mut() {
                    *s = dsp.process_stereo(*s, *s).0;
                }
            }
            [] => {}
        }

        if self.editor_state.is_open() {
            let frame = dsp.meter_frame();
            self.shared.publish_meter(MeterReading {
                in_peak: frame.in_peak,
                in_rms: frame.in_rms,
                out_peak: frame.out_peak,
                out_rms: frame.out_rms,
                in_clip: frame.in_clip,
                out_clip: frame.out_clip,
            });
            self.shared.publish_telemetry(&dsp.telemetry());
            self.shared
                .set_tempo(context.transport().tempo.unwrap_or(0.0) as f32);
        }

        // NAM chunking and the IR partition move the delay: report it when it
        // changes, for the host's delay compensation.
        let latency = dsp.latency_samples() as u32;
        if latency != self.latency {
            self.latency = latency;
            self.shared.set_latency(latency);
            context.set_latency_samples(latency);
        }

        // Delay and reverb tails ring well past the input going silent.
        ProcessStatus::KeepAlive
    }
}

impl ClapPlugin for Rodhareist {
    const CLAP_ID: &'static str = "com.futureboard.rodhareist";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Guitar multi-effect: amps, NAM captures, cabinets and effects in lanes");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Distortion,
        ClapFeature::Stereo,
        ClapFeature::Mono,
    ];
}

impl Vst3Plugin for Rodhareist {
    const VST3_CLASS_ID: [u8; 16] = *b"FbRodhareistVst3";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Distortion,
        Vst3SubCategory::Stereo,
    ];
}

impl Vst2Plugin for Rodhareist {
    /// FourCC `FbRh`. Projects and hosts identify this VST2 plug-in by this id.
    const VST2_UID: [u8; 4] = *b"FbRh";
}

nih_export_clap!(Rodhareist);
nih_export_vst3!(Rodhareist);
nih_export_vst2!(Rodhareist);

#[cfg(test)]
mod entry_point_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_with_dsp() -> Rodhareist {
        Rodhareist {
            dsp: Some(Box::new(Dsp::new(48_000.0))),
            ..Rodhareist::default()
        }
    }

    #[test]
    fn first_block_applies_the_host_defaults_exactly() {
        let mut plugin = plugin_with_dsp();
        plugin.apply_param_changes();
        assert_eq!(
            *plugin.dsp.as_deref().unwrap().params(),
            plugin.params.wire.to_core_params()
        );
    }

    #[test]
    fn a_half_moved_chain_is_not_applied() {
        let mut previous = [None; PATH_SLOTS];
        previous[0] = Some(StageKind::Drive);
        previous[1] = Some(StageKind::Amp);
        let mut order = previous;
        assert!(!path_order_is_mid_move(&previous, &order));
        // The move's first write: Drive now sits in both slots.
        order[1] = Some(StageKind::Drive);
        let mut held = None;
        assert!(hold_path_order(&mut held, &previous, &order, None).hold);
        order[0] = Some(StageKind::Amp);
        assert!(!hold_path_order(&mut held, &previous, &order, None).hold);
        // A duplicate left in place by a generic editor is applied on the
        // next block, not held forever. The DSP drops the extra copy.
        let mut duplicate = previous;
        duplicate[2] = Some(StageKind::Drive);
        let mut held = None;
        assert!(hold_path_order(&mut held, &previous, &duplicate, None).hold);
        assert!(!hold_path_order(&mut held, &previous, &duplicate, None).hold);
    }

    /// The host may deliver the slot writes of one editor move several
    /// callbacks apart. The running chain stays until the last one is in.
    #[test]
    fn an_editor_move_is_held_until_every_slot_arrives() {
        let mut plugin = plugin_with_dsp();
        let mut previous = [None; PATH_SLOTS];
        previous[0] = Some(StageKind::Drive);
        previous[1] = Some(StageKind::Amp);
        previous[2] = Some(StageKind::Cab);
        // Drive moves onto the empty slot after the cabinet.
        let mut target = previous;
        target[0] = None;
        target[3] = Some(StageKind::Drive);
        plugin.shared.publish_path_target(&target);

        let mut halfway = previous;
        halfway[0] = None;
        for block in 0..8 {
            assert!(
                plugin.hold_path(&previous, &halfway),
                "block {block}: the chain without Drive must not run"
            );
        }
        // The destination written first, from the other direction.
        let mut doubled = previous;
        doubled[3] = Some(StageKind::Drive);
        for block in 0..8 {
            assert!(plugin.hold_path(&previous, &doubled), "block {block}");
        }
        assert!(!plugin.hold_path(&previous, &target));
        assert_eq!(plugin.shared.path_target(), None, "the move is over");

        // Something other than the move writes the slots: the move is over
        // and the usual one-block rule applies.
        plugin.shared.publish_path_target(&target);
        let mut elsewhere = previous;
        elsewhere[1] = None;
        assert!(plugin.hold_path(&previous, &elsewhere));
        assert_eq!(plugin.shared.path_target(), None);
        assert!(!plugin.hold_path(&previous, &elsewhere));
    }

    #[test]
    fn a_newer_move_keeps_its_target() {
        let shared = Shared::default();
        let mut first = [None; PATH_SLOTS];
        first[0] = Some(StageKind::Amp);
        let mut second = first;
        second[1] = Some(StageKind::Cab);
        shared.publish_path_target(&second);
        shared.finish_path_target(&first);
        assert_eq!(shared.path_target(), Some(second));
        shared.finish_path_target(&second);
        assert_eq!(shared.path_target(), None);
    }

    #[test]
    fn a_move_onto_an_empty_slot_waits_for_the_destination() {
        let mut previous = [None; PATH_SLOTS];
        previous[0] = Some(StageKind::Drive);
        previous[1] = Some(StageKind::Amp);
        // Source cleared first. Drive is missing, and that must not be applied
        // as the chain. The next write puts Drive in the empty slot.
        let mut halfway = previous;
        halfway[0] = None;
        let mut held = None;
        assert!(hold_path_order(&mut held, &previous, &halfway, None).hold);
        let mut done = halfway;
        done[2] = Some(StageKind::Drive);
        assert!(!hold_path_order(&mut held, &previous, &done, None).hold);
    }

    #[test]
    fn a_deliberate_removal_applies_on_the_next_block() {
        let mut previous = [None; PATH_SLOTS];
        previous[0] = Some(StageKind::Drive);
        previous[1] = Some(StageKind::Amp);
        let mut removed = previous;
        removed[0] = None;
        let mut held = None;
        assert!(hold_path_order(&mut held, &previous, &removed, None).hold);
        assert!(!hold_path_order(&mut held, &previous, &removed, None).hold);
    }

    #[test]
    fn the_nam_chunk_stays_on_the_host_maximum() {
        let mut plugin = plugin_with_dsp();
        plugin.nam_block = 32;
        let dsp = plugin.dsp.as_deref_mut().unwrap();
        dsp.begin_block_with_frames(plugin.nam_block);
        assert_eq!(dsp.nam_latency_samples(), 32);
        // A later callback is shorter. The plug-in passes the maximum again,
        // so the reported latency does not move.
        dsp.begin_block_with_frames(plugin.nam_block);
        assert_eq!(dsp.nam_latency_samples(), 32);
    }

    #[test]
    fn processes_finite_audio() {
        let mut plugin = plugin_with_dsp();
        plugin.apply_param_changes();
        let dsp = plugin.dsp.as_deref_mut().unwrap();
        for block in 0..20 {
            dsp.begin_block();
            for n in 0..256 {
                let x = ((block * 256 + n) as f32 * 0.03).sin() * 0.3;
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite());
            }
        }
    }
}
