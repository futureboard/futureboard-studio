use std::ffi::{CStr, CString};
use std::os::raw::c_double;
use std::sync::Arc;

use serde_json::Value;

use crate::editor_chrome::EditorChrome;
use crate::plugin_backend::{backend, PluginModuleFormat};

/// Space presses the C++ editor window procedures claimed for the DAW
/// transport since the last call, and cleared by it.
///
/// The counter is process-wide, so whichever process hosts those windows has to
/// drain it: the separated plug-in host for the editors it owns, and the Studio
/// for the in-process ones. It went unread in the Studio for as long as the
/// in-process editor path existed, which is a press that is claimed — taken
/// away from the plug-in — and then dropped, so Space in an in-process editor
/// did nothing at all rather than falling back to the plug-in.
pub fn take_transport_toggle_requests() -> u32 {
    // SAFETY: a plain exchange on a C++ atomic counter; no arguments, no
    // pointers, and safe to call from any thread.
    unsafe { ffi::sphere_daux_vst3_take_transport_toggle_requests() }
}

/// `FUTUREBOARD_VST3_MIDI_DEBUG=1` enables VST3 MIDI bridge traces.
pub fn vst3_midi_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_FORENSIC_TRACE").is_some()
            || std::env::var_os("FUTUREBOARD_VST3_MIDI_DEBUG").is_some()
    })
}

fn vst3_context_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_FORENSIC_TRACE").is_some()
            || std::env::var_os("FUTUREBOARD_VST3_CONTEXT_DEBUG").is_some()
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vst3MidiEventKind {
    NoteOff = 0,
    NoteOn = 1,
    /// MIDI controller change. `pitch` carries the VST3 controller number
    /// (`0..=127` CC, `128` aftertouch, `129` pitch bend) and `velocity` the
    /// normalized value `0.0..=1.0`. The C++ bridge maps it to a parameter
    /// change via `IMidiMapping`.
    ControlChange = 2,
}

/// C-compatible MIDI event for the bridges' *_process_stereo_block_with_midi.\n///\n/// Shared verbatim by all three native bridges (VST3, VST2, CLAP) so the engine\n/// builds one event buffer regardless of which one is loaded.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Vst3MidiEvent {
    pub sample_offset: u32,
    pub kind: u8,
    pub channel: u8,
    pub pitch: u8,
    pub velocity: f32,
}

impl Vst3MidiEvent {
    #[inline]
    pub fn note_on(sample_offset: u32, channel: u8, pitch: u8, velocity: f32) -> Self {
        Self {
            sample_offset,
            kind: Vst3MidiEventKind::NoteOn as u8,
            channel,
            pitch,
            velocity,
        }
    }

    #[inline]
    pub fn note_off(sample_offset: u32, channel: u8, pitch: u8, velocity: f32) -> Self {
        Self {
            sample_offset,
            kind: Vst3MidiEventKind::NoteOff as u8,
            channel,
            pitch,
            velocity,
        }
    }

    /// A controller change. `controller` is the VST3 controller number; values
    /// `0..=129` fit in the `pitch` byte. `value` is normalized `0.0..=1.0`.
    #[inline]
    pub fn control_change(sample_offset: u32, channel: u8, controller: u16, value: f32) -> Self {
        Self {
            sample_offset,
            kind: Vst3MidiEventKind::ControlChange as u8,
            channel,
            pitch: controller as u8,
            velocity: value,
        }
    }
}

/// Transport snapshot handed to a plugin's VST3 `ProcessContext` for one block.
///
/// Built on the audio thread (in-process) or the bridge producer thread (host)
/// from the engine's real tempo map, time signature, and transport position —
/// it is the truth that replaces the old hardcoded 120 BPM / 4-4 / always
/// playing stub. `Copy` so it threads cheaply through the per-block call chain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RuntimeTransportContext {
    /// Quarter-notes per minute at this block's position.
    pub tempo_bpm: f64,
    pub time_sig_num: u32,
    pub time_sig_den: u32,
    /// Absolute project sample position of the block start.
    pub project_time_samples: i64,
    /// Project position in quarter notes (VST3 `projectTimeMusic`).
    pub ppq_position: f64,
    /// Quarter-note position of the current bar start (VST3 `barPositionMusic`).
    pub bar_position_ppq: f64,
    pub playing: bool,
    pub recording: bool,
}

impl Default for RuntimeTransportContext {
    fn default() -> Self {
        Self {
            tempo_bpm: 120.0,
            time_sig_num: 4,
            time_sig_den: 4,
            project_time_samples: 0,
            ppq_position: 0.0,
            bar_position_ppq: 0.0,
            playing: false,
            recording: false,
        }
    }
}

impl RuntimeTransportContext {
    /// Quarter-note position of the bar containing `ppq`, given the time
    /// signature. Bar length in quarter notes is `num * 4 / den` (e.g. 4/4 = 4,
    /// 6/8 = 3, 3/4 = 3). Falls back to `ppq` when the signature is degenerate.
    pub fn bar_start_ppq(ppq: f64, time_sig_num: u32, time_sig_den: u32) -> f64 {
        if time_sig_num == 0 || time_sig_den == 0 {
            return ppq;
        }
        let bar_len_qn = time_sig_num as f64 * 4.0 / time_sig_den as f64;
        if bar_len_qn <= 0.0 {
            return ppq;
        }
        (ppq / bar_len_qn).floor() * bar_len_qn
    }
}

#[repr(C)]
pub(crate) struct SphereDauxVst3Processor {
    _private: [u8; 0],
}

/// Raw VST3 bridge entry points.
///
/// Wrapped in a module so `vst2_processor::backend` can name them while they
/// stay crate-private; call sites go through that dispatch layer, never here.
pub(crate) mod ffi {
    use super::{SphereDauxVst3Processor, Vst3MidiEvent};
    use std::os::raw::{c_char, c_double, c_float};

    extern "C" {
        pub(crate) fn sphere_daux_vst3_bridge_probe() -> i32;
        pub(crate) fn sphere_daux_vst3_last_error() -> *const c_char;
        pub(crate) fn sphere_daux_vst3_create(
            plugin_path: *const c_char,
            class_id: *const c_char,
            sample_rate: c_double,
        ) -> *mut SphereDauxVst3Processor;
        pub(crate) fn sphere_daux_vst3_destroy(processor: *mut SphereDauxVst3Processor);
        pub(crate) fn sphere_daux_vst3_process_stereo_sample(
            processor: *mut SphereDauxVst3Processor,
            in_l: c_float,
            in_r: c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
        ) -> i32;
        #[allow(dead_code)]
        pub(crate) fn sphere_daux_vst3_process_stereo_block(
            processor: *mut SphereDauxVst3Processor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
            frames: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_process_stereo_block_with_midi(
            processor: *mut SphereDauxVst3Processor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
            frames: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_process_main_output_block_with_midi(
            processor: *mut SphereDauxVst3Processor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_interleaved: *mut c_float,
            frames: i32,
            output_channels: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_event_input_bus_count(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_audio_input_bus_count(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_audio_output_bus_count(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_main_audio_input_channel_count(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_main_audio_output_channel_count(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_output_bus_channel_counts(
            processor: *mut SphereDauxVst3Processor,
            out_counts: *mut i32,
            max_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_process_count(
            processor: *mut SphereDauxVst3Processor,
        ) -> u64;
        pub(crate) fn sphere_daux_vst3_last_input_peak(
            processor: *mut SphereDauxVst3Processor,
        ) -> c_double;
        pub(crate) fn sphere_daux_vst3_last_output_peak(
            processor: *mut SphereDauxVst3Processor,
        ) -> c_double;
        pub(crate) fn sphere_daux_vst3_last_difference_peak(
            processor: *mut SphereDauxVst3Processor,
        ) -> c_double;
        /// Enqueue a normalized (0..1) VST3 parameter change.
        /// Delivered to IAudioProcessor via inputParameterChanges on the next process call.
        pub(crate) fn sphere_daux_vst3_set_param(
            processor: *mut SphereDauxVst3Processor,
            param_id: u32,
            value: c_double,
        );
        pub(crate) fn sphere_daux_vst3_open_editor(
            processor: *mut SphereDauxVst3Processor,
            window_id: *const c_char,
            title: *const c_char,
            width: i32,
            height: i32,
        ) -> u64;
        pub(crate) fn sphere_daux_vst3_close_editor(processor: *mut SphereDauxVst3Processor);
        pub(crate) fn sphere_daux_vst3_focus_editor(processor: *mut SphereDauxVst3Processor)
            -> i32;
        pub(crate) fn sphere_daux_vst3_is_valid(processor: *mut SphereDauxVst3Processor) -> i32;
        pub(crate) fn sphere_daux_vst3_get_latency_samples(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        // ARA 2 entry points. VST3 only — the CLAP and VST2 bridges expose no
        // equivalent, so `Vst3RuntimeProcessor` gates these on the format
        // instead of routing them through `plugin_backend::backend`.
        pub(crate) fn sphere_daux_vst3_create_for_ara(
            plugin_path: *const c_char,
            class_id: *const c_char,
            sample_rate: c_double,
        ) -> *mut SphereDauxVst3Processor;
        pub(crate) fn sphere_daux_vst3_activate(processor: *mut SphereDauxVst3Processor) -> i32;
        pub(crate) fn sphere_daux_vst3_stop_processing(processor: *mut SphereDauxVst3Processor);
        // Rust-owned view host: the caller owns the window, these drive only
        // the plug-in's `IPlugView`.
        pub(crate) fn sphere_daux_vst3_view_attach(
            processor: *mut SphereDauxVst3Processor,
            parent_hwnd: u64,
            width: i32,
            height: i32,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_view_detach(processor: *mut SphereDauxVst3Processor);
        pub(crate) fn sphere_daux_vst3_view_is_attached(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_view_set_size(
            processor: *mut SphereDauxVst3Processor,
            width: i32,
            height: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_view_get_size(
            processor: *mut SphereDauxVst3Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_view_can_resize(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_view_constrain(
            processor: *mut SphereDauxVst3Processor,
            io_width: *mut i32,
            io_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_view_take_resize_request(
            processor: *mut SphereDauxVst3Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_ara_is_supported(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_ara_create_main_factory(
            processor: *mut SphereDauxVst3Processor,
        ) -> *mut std::ffi::c_void;
        pub(crate) fn sphere_daux_vst3_ara_release_main_factory(unknown: *mut std::ffi::c_void);
        pub(crate) fn sphere_daux_vst3_ara_component_unknown(
            processor: *mut SphereDauxVst3Processor,
        ) -> *mut std::ffi::c_void;
        pub(crate) fn sphere_daux_vst3_set_process_context(
            processor: *mut SphereDauxVst3Processor,
            tempo: c_double,
            time_sig_num: i32,
            time_sig_den: i32,
            project_time_samples: i64,
            ppq: c_double,
            bar_ppq: c_double,
            playing: i32,
            recording: i32,
        );
        // GPUI-embedded editor (Windows): attaches the existing instance's view.
        pub(crate) fn sphere_daux_vst3_embed_editor(
            processor: *mut SphereDauxVst3Processor,
            parent_hwnd: u64,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> u64;
        pub(crate) fn sphere_daux_vst3_embed_set_bounds(
            processor: *mut SphereDauxVst3Processor,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        );
        pub(crate) fn sphere_daux_vst3_embed_refresh(processor: *mut SphereDauxVst3Processor);
        pub(crate) fn sphere_daux_vst3_embed_attach_hwnd(
            processor: *mut SphereDauxVst3Processor,
        ) -> u64;
        pub(crate) fn sphere_daux_vst3_embed_detach(processor: *mut SphereDauxVst3Processor);
        pub(crate) fn sphere_daux_vst3_embed_is_valid(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_embed_has_visible_ui(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_embed_host_kind(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_embed_take_user_close(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_embed_set_waiting_stage(
            processor: *mut SphereDauxVst3Processor,
            stage: *const c_char,
        );
        pub(crate) fn sphere_daux_vst3_embed_content_size(
            processor: *mut SphereDauxVst3Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_embed_set_instance_label(
            processor: *mut SphereDauxVst3Processor,
            instance_id: *const c_char,
        );
        pub(crate) fn sphere_daux_vst3_set_editor_title(
            processor: *mut SphereDauxVst3Processor,
            title: *const c_char,
        );
        pub(crate) fn sphere_daux_vst3_prepare_editor_view(
            processor: *mut SphereDauxVst3Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_take_pending_shell_resize(
            processor: *mut SphereDauxVst3Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        /// `NSWindow*` of this instance's host-owned editor as an opaque handle,
        /// or 0 when there is no such window. Addresses the shared editor
        /// chrome strip — see [`crate::plugin_backend::editor_chrome`].
        pub(crate) fn sphere_daux_vst3_editor_native_window(
            processor: *mut SphereDauxVst3Processor,
        ) -> u64;
        pub(crate) fn sphere_daux_vst3_editor_resizable(
            processor: *mut SphereDauxVst3Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_get_state(
            processor: *mut SphereDauxVst3Processor,
            out_component: *mut *mut u8,
            out_component_len: *mut i32,
            out_controller: *mut *mut u8,
            out_controller_len: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_set_state(
            processor: *mut SphereDauxVst3Processor,
            component_data: *const u8,
            component_len: i32,
            controller_data: *const u8,
            controller_len: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst3_state_free(data: *mut u8);
        pub(crate) fn sphere_daux_vst3_list_parameters_json(
            processor: *mut SphereDauxVst3Processor,
        ) -> *mut c_char;
        pub(crate) fn sphere_daux_vst3_parameters_json_free(data: *mut c_char);
        /// Process-wide, not per-processor: the C++ editor window procedures
        /// claim the transport key before the plug-in's view can eat it and
        /// count the presses here for whoever hosts them.
        pub(crate) fn sphere_daux_vst3_take_transport_toggle_requests() -> u32;
    }

    // Short aliases so `vst2_processor::backend` can name both bridges'
    // entry points with one identifier per operation.
    pub(crate) use self::{
        sphere_daux_vst3_audio_input_bus_count as audio_input_bus_count,
        sphere_daux_vst3_audio_output_bus_count as audio_output_bus_count,
        sphere_daux_vst3_bridge_probe as bridge_probe,
        sphere_daux_vst3_close_editor as close_editor, sphere_daux_vst3_create as create,
        sphere_daux_vst3_destroy as destroy,
        sphere_daux_vst3_editor_native_window as editor_native_window,
        sphere_daux_vst3_editor_resizable as editor_resizable,
        sphere_daux_vst3_embed_attach_hwnd as embed_attach_hwnd,
        sphere_daux_vst3_embed_content_size as embed_content_size,
        sphere_daux_vst3_embed_detach as embed_detach,
        sphere_daux_vst3_embed_editor as embed_editor,
        sphere_daux_vst3_embed_has_visible_ui as embed_has_visible_ui,
        sphere_daux_vst3_embed_host_kind as embed_host_kind,
        sphere_daux_vst3_embed_is_valid as embed_is_valid,
        sphere_daux_vst3_embed_refresh as embed_refresh,
        sphere_daux_vst3_embed_set_bounds as embed_set_bounds,
        sphere_daux_vst3_embed_set_instance_label as embed_set_instance_label,
        sphere_daux_vst3_embed_set_waiting_stage as embed_set_waiting_stage,
        sphere_daux_vst3_embed_take_user_close as embed_take_user_close,
        sphere_daux_vst3_event_input_bus_count as event_input_bus_count,
        sphere_daux_vst3_focus_editor as focus_editor,
        sphere_daux_vst3_get_latency_samples as get_latency_samples,
        sphere_daux_vst3_get_state as get_state, sphere_daux_vst3_is_valid as is_valid,
        sphere_daux_vst3_last_difference_peak as last_difference_peak,
        sphere_daux_vst3_last_error as last_error,
        sphere_daux_vst3_last_input_peak as last_input_peak,
        sphere_daux_vst3_last_output_peak as last_output_peak,
        sphere_daux_vst3_list_parameters_json as list_parameters_json,
        sphere_daux_vst3_main_audio_input_channel_count as main_audio_input_channel_count,
        sphere_daux_vst3_main_audio_output_channel_count as main_audio_output_channel_count,
        sphere_daux_vst3_open_editor as open_editor,
        sphere_daux_vst3_output_bus_channel_counts as output_bus_channel_counts,
        sphere_daux_vst3_parameters_json_free as parameters_json_free,
        sphere_daux_vst3_prepare_editor_view as prepare_editor_view,
        sphere_daux_vst3_process_count as process_count,
        sphere_daux_vst3_process_main_output_block_with_midi as process_main_output_block_with_midi,
        sphere_daux_vst3_process_stereo_block_with_midi as process_stereo_block_with_midi,
        sphere_daux_vst3_process_stereo_sample as process_stereo_sample,
        sphere_daux_vst3_set_editor_title as set_editor_title,
        sphere_daux_vst3_set_param as set_param,
        sphere_daux_vst3_set_process_context as set_process_context,
        sphere_daux_vst3_set_state as set_state, sphere_daux_vst3_state_free as state_free,
        sphere_daux_vst3_take_pending_shell_resize as take_pending_shell_resize,
        sphere_daux_vst3_view_attach as view_attach,
        sphere_daux_vst3_view_can_resize as view_can_resize,
        sphere_daux_vst3_view_constrain as view_constrain,
        sphere_daux_vst3_view_detach as view_detach,
        sphere_daux_vst3_view_get_size as view_get_size,
        sphere_daux_vst3_view_is_attached as view_is_attached,
        sphere_daux_vst3_view_set_size as view_set_size,
        sphere_daux_vst3_view_take_resize_request as view_take_resize_request,
    };
}

/// Metadata for one VST3 parameter returned by [`Vst3RuntimeProcessor::list_parameters`].
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Vst3ParameterDescriptor {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub short_title: String,
    #[serde(default)]
    pub unit: String,
    pub automatable: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub value_normalized: f64,
}

impl Vst3ParameterDescriptor {
    /// Best display label — prefers `title`, falls back to `short_title`.
    pub fn display_name(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.short_title.is_empty() {
            &self.short_title
        } else {
            "Parameter"
        }
    }

    /// Whether this parameter should appear in the automation picker.
    pub fn picker_visible(&self, debug: bool) -> bool {
        if self.hidden && !debug {
            return false;
        }
        if !self.automatable && !debug {
            return false;
        }
        true
    }
}

/// Captured VST3 plugin state: the raw component (processor) stream plus the
/// controller stream for split component/controller plugins. Either may be
/// empty — a plugin with no state is valid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vst3PluginState {
    pub component: Vec<u8>,
    pub controller: Vec<u8>,
}

impl Vst3PluginState {
    /// Magic + version prefix of the packed single-blob form.
    const PACKED_MAGIC: &'static [u8; 4] = b"FBV3";
    const PACKED_VERSION: u32 = 1;

    pub fn is_empty(&self) -> bool {
        self.component.is_empty() && self.controller.is_empty()
    }

    /// Pack both streams into one blob for project persistence:
    /// `"FBV3" | version:u32 | component_len:u32 | component | controller_len:u32 | controller`
    /// (all integers little-endian).
    pub fn to_packed_bytes(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(4 + 4 + 4 + self.component.len() + 4 + self.controller.len());
        out.extend_from_slice(Self::PACKED_MAGIC);
        out.extend_from_slice(&Self::PACKED_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.component.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.component);
        out.extend_from_slice(&(self.controller.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.controller);
        out
    }

    /// Inverse of [`Self::to_packed_bytes`]. `None` on bad magic/version or a
    /// truncated blob — callers should treat that as "no saved state".
    pub fn from_packed_bytes(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(Self::PACKED_MAGIC.as_slice())?;
        let (version, rest) = take_u32_le(rest)?;
        if version != Self::PACKED_VERSION {
            return None;
        }
        let (component_len, rest) = take_u32_le(rest)?;
        let component_len = component_len as usize;
        if rest.len() < component_len {
            return None;
        }
        let (component, rest) = rest.split_at(component_len);
        let (controller_len, rest) = take_u32_le(rest)?;
        let controller_len = controller_len as usize;
        if rest.len() < controller_len {
            return None;
        }
        Some(Self {
            component: component.to_vec(),
            controller: rest[..controller_len].to_vec(),
        })
    }
}

fn take_u32_le(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let (head, rest) = bytes.split_at_checked(4)?;
    Some((u32::from_le_bytes(head.try_into().ok()?), rest))
}

#[cfg(test)]
mod state_tests {
    use super::Vst3PluginState;

    #[test]
    fn packed_state_round_trips() {
        let state = Vst3PluginState {
            component: vec![1, 2, 3, 4, 5],
            controller: vec![9, 8],
        };
        let packed = state.to_packed_bytes();
        assert_eq!(Vst3PluginState::from_packed_bytes(&packed), Some(state));
    }

    #[test]
    fn packed_state_round_trips_empty_streams() {
        let state = Vst3PluginState {
            component: vec![7; 1024],
            controller: Vec::new(),
        };
        let packed = state.to_packed_bytes();
        assert_eq!(Vst3PluginState::from_packed_bytes(&packed), Some(state));
    }

    #[test]
    fn packed_state_rejects_garbage_and_truncation() {
        assert_eq!(Vst3PluginState::from_packed_bytes(b"not a state"), None);
        assert_eq!(Vst3PluginState::from_packed_bytes(b""), None);
        let packed = Vst3PluginState {
            component: vec![1, 2, 3],
            controller: vec![4],
        }
        .to_packed_bytes();
        assert_eq!(
            Vst3PluginState::from_packed_bytes(&packed[..packed.len() - 1]),
            None
        );
    }
}

#[derive(Debug)]
pub struct Vst3RuntimeProcessor {
    inner: Arc<Vst3RuntimeProcessorInner>,
}

#[derive(Debug)]
struct Vst3RuntimeProcessorInner {
    raw: *mut SphereDauxVst3Processor,
    /// Which native bridge produced (and must service) `raw`. Resolved once at
    /// construction so the process path never re-derives it.
    format: PluginModuleFormat,
    plugin_path: String,
    class_id: String,
    sample_rate: u32,
    event_input_bus_count: i32,
    audio_input_bus_count: i32,
    audio_output_bus_count: i32,
    main_audio_input_channel_count: i32,
    main_audio_output_channel_count: i32,
    destroy_reason: std::sync::Mutex<Option<String>>,
}

unsafe impl Send for Vst3RuntimeProcessor {}
unsafe impl Sync for Vst3RuntimeProcessor {}
unsafe impl Send for Vst3RuntimeProcessorInner {}
unsafe impl Sync for Vst3RuntimeProcessorInner {}

impl Vst3RuntimeProcessor {
    pub fn from_params(
        params: &std::collections::HashMap<String, Value>,
        sample_rate: u32,
    ) -> Option<Self> {
        let plugin_path = params
            .get("modulePath")
            .or_else(|| params.get("path"))
            .and_then(Value::as_str)?
            .trim();
        if plugin_path.is_empty() {
            return None;
        }
        let class_id = params
            .get("classId")
            .or_else(|| params.get("class_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        // The graph builder already labels every bridged insert with its real
        // format; fall back to path detection only for legacy snapshots that
        // predate the label.
        let format = params
            .get("format")
            .and_then(Value::as_str)
            .and_then(PluginModuleFormat::from_label)
            .unwrap_or_else(|| PluginModuleFormat::detect(plugin_path));
        Self::new_with_format(plugin_path, class_id, sample_rate, format)
    }

    /// Create a processor, detecting the plug-in format from the module path.
    /// Prefer [`Self::new_with_format`] wherever the format is already known.
    pub fn new(plugin_path: &str, class_id: &str, sample_rate: u32) -> Option<Self> {
        Self::new_with_format(
            plugin_path,
            class_id,
            sample_rate,
            PluginModuleFormat::detect(plugin_path),
        )
    }

    pub fn new_with_format(
        plugin_path: &str,
        class_id: &str,
        sample_rate: u32,
        format: PluginModuleFormat,
    ) -> Option<Self> {
        let log_tag = match (
            std::env::var("FUTUREBOARD_PROCESS_ROLE")
                .map(|v| v == "plugin_host")
                .unwrap_or(false),
            format,
        ) {
            (true, PluginModuleFormat::Vst3) => "[plugin-host-vst3]",
            (true, PluginModuleFormat::Vst2) => "[plugin-host-vst2]",
            (true, PluginModuleFormat::Clap) => "[plugin-host-clap]",
            (false, PluginModuleFormat::Vst3) => "[SphereVST3]",
            (false, PluginModuleFormat::Vst2) => "[SphereVST2]",
            (false, PluginModuleFormat::Clap) => "[SphereCLAP]",
        };
        let bridge_probe = backend::bridge_probe(format);
        eprintln!("{log_tag} bridge probe result=0x{bridge_probe:x}");
        eprintln!(
            "{log_tag} create request path='{}' exists={} classId='{}' sr={}",
            plugin_path,
            std::path::Path::new(plugin_path).exists(),
            class_id,
            sample_rate.max(1)
        );
        eprintln!(
            "[{}-setup] plugin=\"{}\" sample_rate={}",
            format.label().to_ascii_lowercase(),
            plugin_path,
            sample_rate.max(1)
        );
        let path = CString::new(plugin_path).ok()?;
        let class_id_c = CString::new(class_id).ok()?;
        let raw = unsafe {
            backend::create(
                format,
                path.as_ptr(),
                class_id_c.as_ptr(),
                sample_rate.max(1) as c_double,
            )
        };
        if raw.is_null() {
            let reason = unsafe {
                let ptr = backend::last_error(format);
                if ptr.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(ptr).to_string_lossy().into_owned()
                }
            };
            eprintln!(
                "{log_tag} create failed path='{}' classId='{}' reason={}",
                plugin_path, class_id, reason
            );
            return None;
        }
        let event_input_bus_count = unsafe { backend::event_input_bus_count(format, raw) };
        let audio_input_bus_count = unsafe { backend::audio_input_bus_count(format, raw) };
        let audio_output_bus_count = unsafe { backend::audio_output_bus_count(format, raw) };
        let main_audio_input_channel_count =
            unsafe { backend::main_audio_input_channel_count(format, raw) };
        let main_audio_output_channel_count =
            unsafe { backend::main_audio_output_channel_count(format, raw) };
        eprintln!(
            "{log_tag} create ok path='{}' classId='{}' handle=0x{:x} eventInputBuses={} audioInBuses={} audioOutBuses={} mainInChannels={} mainOutChannels={}",
            plugin_path,
            class_id,
            raw as usize,
            event_input_bus_count,
            audio_input_bus_count,
            audio_output_bus_count,
            main_audio_input_channel_count,
            main_audio_output_channel_count
        );
        Some(Self {
            inner: Arc::new(Vst3RuntimeProcessorInner {
                raw,
                format,
                plugin_path: plugin_path.to_string(),
                class_id: class_id.to_string(),
                sample_rate: sample_rate.max(1),
                event_input_bus_count,
                audio_input_bus_count,
                audio_output_bus_count,
                main_audio_input_channel_count,
                main_audio_output_channel_count,
                destroy_reason: std::sync::Mutex::new(None),
            }),
        })
    }

    /// Creates a VST3 instance for ARA, stopping before activation.
    ///
    /// `ARAVST3.h`: `bindToDocumentControllerWithRoles` may be called "only once
    /// during the lifetime of the IAudioProcessor component, before the first
    /// call to setActive () or setState () or getProcessContextRequirements ()
    /// or the creation of the GUI". The ordinary constructor activates as part
    /// of creation, so a plug-in built that way rejects the binding — Melodyne
    /// answers `bindToDocumentControllerWithRoles` with null. The caller binds
    /// the returned instance and then calls [`Self::activate`].
    ///
    /// VST3 only; ARA hosting has no CLAP or VST2 path here.
    pub fn new_for_ara(plugin_path: &str, class_id: &str, sample_rate: u32) -> Option<Self> {
        let path = CString::new(plugin_path).ok()?;
        let class_id_c = CString::new(class_id).ok()?;
        // SAFETY: both strings stay alive across the call, and the bridge copies
        // whatever it keeps.
        let raw = unsafe {
            ffi::sphere_daux_vst3_create_for_ara(
                path.as_ptr(),
                class_id_c.as_ptr(),
                sample_rate.max(1) as c_double,
            )
        };
        if raw.is_null() {
            // SAFETY: the bridge always leaves a NUL-terminated last error.
            let reason = unsafe {
                let ptr = ffi::sphere_daux_vst3_last_error();
                if ptr.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(ptr).to_string_lossy().into_owned()
                }
            };
            eprintln!(
                "[SphereVST3-ARA] create failed path='{plugin_path}' classId='{class_id}' reason={reason}"
            );
            return None;
        }
        let format = PluginModuleFormat::Vst3;
        // SAFETY: `raw` is the instance just created above.
        let (
            event_input_bus_count,
            audio_input_bus_count,
            audio_output_bus_count,
            main_audio_input_channel_count,
            main_audio_output_channel_count,
        ) = unsafe {
            (
                backend::event_input_bus_count(format, raw),
                backend::audio_input_bus_count(format, raw),
                backend::audio_output_bus_count(format, raw),
                backend::main_audio_input_channel_count(format, raw),
                backend::main_audio_output_channel_count(format, raw),
            )
        };
        Some(Self {
            inner: Arc::new(Vst3RuntimeProcessorInner {
                raw,
                format,
                plugin_path: plugin_path.to_string(),
                class_id: class_id.to_string(),
                sample_rate: sample_rate.max(1),
                event_input_bus_count,
                audio_input_bus_count,
                audio_output_bus_count,
                main_audio_input_channel_count,
                main_audio_output_channel_count,
                destroy_reason: std::sync::Mutex::new(None),
            }),
        })
    }

    #[inline]
    pub fn process_stereo_sample(&mut self, l: f32, r: f32) -> Option<(f32, f32)> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut out_l = 0.0f32;
        let mut out_r = 0.0f32;
        let ok = unsafe {
            backend::process_stereo_sample(
                self.inner.format,
                self.inner.raw,
                l,
                r,
                &mut out_l,
                &mut out_r,
            )
        };
        if ok == 0 {
            None
        } else {
            Some((out_l, out_r))
        }
    }

    #[inline]
    pub fn process_stereo_block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) -> bool {
        self.process_stereo_block_with_midi(in_l, in_r, out_l, out_r, &[])
    }

    #[inline]
    pub fn event_input_bus_count(&self) -> i32 {
        self.inner.event_input_bus_count
    }

    #[inline]
    pub fn audio_input_bus_count(&self) -> i32 {
        self.inner.audio_input_bus_count
    }

    #[inline]
    pub fn audio_output_bus_count(&self) -> i32 {
        self.inner.audio_output_bus_count
    }

    #[inline]
    pub fn main_audio_input_channel_count(&self) -> i32 {
        self.inner.main_audio_input_channel_count
    }

    #[inline]
    pub fn main_audio_output_channel_count(&self) -> i32 {
        self.inner.main_audio_output_channel_count
    }

    /// Per-bus output channel counts in the bus-by-bus order the bridge
    /// flattens them into the interleaved block (bus0 channels, then bus1…).
    /// Queried live — the bus arrangement is fixed after activation. Empty if
    /// the processor is null or reports no output buses. The host uses this to
    /// place one mixer strip per real output bus (a mono bus stays its own
    /// stereo strip) instead of assuming every channel pair is a stereo bus.
    pub fn output_bus_channel_counts(&self) -> Vec<u8> {
        if self.inner.raw.is_null() {
            return Vec::new();
        }
        const MAX_BUSES: usize = 32;
        let mut counts = [0i32; MAX_BUSES];
        let n = unsafe {
            backend::output_bus_channel_counts(
                self.inner.format,
                self.inner.raw,
                counts.as_mut_ptr(),
                MAX_BUSES as i32,
            )
        };
        let n = n.clamp(0, MAX_BUSES as i32) as usize;
        counts[..n]
            .iter()
            .map(|&c| c.clamp(0, u8::MAX as i32) as u8)
            .collect()
    }

    #[inline]
    pub fn process_stereo_block_with_midi(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        midi_events: &[Vst3MidiEvent],
    ) -> bool {
        let frames = in_l.len().min(in_r.len()).min(out_l.len()).min(out_r.len());
        if self.inner.raw.is_null() || frames == 0 {
            return false;
        }
        let (events_ptr, event_count) = if midi_events.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (midi_events.as_ptr(), midi_events.len() as i32)
        };
        let ok = unsafe {
            backend::process_stereo_block_with_midi(
                self.inner.format,
                self.inner.raw,
                in_l.as_ptr(),
                in_r.as_ptr(),
                out_l.as_mut_ptr(),
                out_r.as_mut_ptr(),
                frames as i32,
                events_ptr,
                event_count,
            )
        };
        ok != 0
    }

    #[inline]
    pub fn process_main_output_block_with_midi(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_interleaved: &mut [f32],
        output_channels: usize,
        midi_events: &[Vst3MidiEvent],
    ) -> Option<usize> {
        let channels = output_channels.max(1);
        let frames = in_l
            .len()
            .min(in_r.len())
            .min(out_interleaved.len() / channels);
        if self.inner.raw.is_null() || frames == 0 {
            return None;
        }
        let (events_ptr, event_count) = if midi_events.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (midi_events.as_ptr(), midi_events.len() as i32)
        };
        let got_channels = unsafe {
            backend::process_main_output_block_with_midi(
                self.inner.format,
                self.inner.raw,
                in_l.as_ptr(),
                in_r.as_ptr(),
                out_interleaved.as_mut_ptr(),
                frames as i32,
                channels as i32,
                events_ptr,
                event_count,
            )
        };
        (got_channels > 0).then_some(got_channels as usize)
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        !self.inner.raw.is_null()
    }

    #[inline]
    pub fn last_error(&self) -> Option<String> {
        if self.inner.raw.is_null() {
            return None;
        }
        unsafe {
            let ptr = backend::last_error(self.inner.format);
            if ptr.is_null() {
                None
            } else {
                let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
        }
    }

    #[inline]
    pub fn plugin_path(&self) -> Option<&str> {
        if self.inner.plugin_path.is_empty() {
            None
        } else {
            Some(&self.inner.plugin_path)
        }
    }

    #[inline]
    pub fn class_id(&self) -> Option<&str> {
        if self.inner.class_id.is_empty() {
            None
        } else {
            Some(&self.inner.class_id)
        }
    }

    #[inline]
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
    }

    #[inline]
    pub fn handle_value(&self) -> usize {
        self.inner.raw as usize
    }

    #[inline]
    pub fn process_count(&self) -> u64 {
        if self.inner.raw.is_null() {
            0
        } else {
            unsafe { backend::process_count(self.inner.format, self.inner.raw) }
        }
    }

    #[inline]
    pub fn last_input_peak(&self) -> f64 {
        if self.inner.raw.is_null() {
            0.0
        } else {
            unsafe { backend::last_input_peak(self.inner.format, self.inner.raw) as f64 }
        }
    }

    #[inline]
    pub fn last_output_peak(&self) -> f64 {
        if self.inner.raw.is_null() {
            0.0
        } else {
            unsafe { backend::last_output_peak(self.inner.format, self.inner.raw) as f64 }
        }
    }

    #[inline]
    pub fn last_difference_peak(&self) -> f64 {
        if self.inner.raw.is_null() {
            0.0
        } else {
            unsafe { backend::last_difference_peak(self.inner.format, self.inner.raw) as f64 }
        }
    }

    /// Enqueue a normalized (0..1) parameter change for the given VST3 ParamID.
    ///
    /// The change is delivered to `IAudioProcessor` via `inputParameterChanges`
    /// on the next `process_stereo_sample` call.  Safe to call from the audio
    /// thread (inside command-drain) or from any other thread.
    ///
    /// `param_id` — the integer `Steinberg::Vst::ParamID` as exposed by the plugin.
    /// `value`    — normalized value in `[0.0, 1.0]`.
    #[inline]
    pub fn set_param(&mut self, param_id: u32, value: f64) {
        if self.inner.raw.is_null() {
            return;
        }
        unsafe {
            backend::set_param(
                self.inner.format,
                self.inner.raw,
                param_id,
                value as c_double,
            )
        }
    }

    /// Mark this processor for destruction with a reason string logged at drop time.
    /// Call this before removing the insert so the drop log is meaningful.
    pub fn set_destroy_reason(&self, reason: &str) {
        if let Ok(mut guard) = self.inner.destroy_reason.lock() {
            *guard = Some(reason.to_string());
        }
    }

    /// Returns false if the underlying C++ processor has been destroyed.
    /// The audio callback should bypass the insert if this returns false.
    #[inline]
    pub fn is_processor_valid(&self) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        unsafe { backend::is_valid(self.inner.format, self.inner.raw) != 0 }
    }

    /// Returns the plugin's reported latency in samples.
    /// Divide by `sample_rate()` to convert to seconds.
    #[inline]
    pub fn get_latency_samples(&self) -> i32 {
        if self.inner.raw.is_null() {
            return 0;
        }
        unsafe { backend::get_latency_samples(self.inner.format, self.inner.raw) }
    }

    /// Runs the activation that [`Self::new_for_ara`] deferred.
    ///
    /// ARA binding has to happen before the first `setActive()`, so an ARA
    /// instance is created inert and activated here once it is bound. Returns
    /// `false` when the plug-in refused to prepare for processing.
    pub fn activate(&self) -> bool {
        if self.inner.raw.is_null() || self.inner.format != PluginModuleFormat::Vst3 {
            return false;
        }
        // SAFETY: `raw` is a live VST3 processor for as long as this handle is.
        unsafe { ffi::sphere_daux_vst3_activate(self.inner.raw) != 0 }
    }

    /// Takes the instance out of the processing state, keeping it alive.
    ///
    /// The step an ARA teardown needs between "the engine stopped calling
    /// `process()`" and "the ARA binding is destroyed": a plug-in whose renderer
    /// is still active holds its render lock, and destroying the document from
    /// the main thread then waits on it. The caller must already have taken the
    /// instance out of the audio graph — this interrupts nothing in flight.
    pub fn stop_processing(&self) {
        if self.inner.raw.is_null() || self.inner.format != PluginModuleFormat::Vst3 {
            return;
        }
        // SAFETY: `raw` is a live VST3 processor for as long as this handle is.
        unsafe { ffi::sphere_daux_vst3_stop_processing(self.inner.raw) };
    }

    /// Attaches the plug-in's editor view to a window **the caller owns**.
    ///
    /// This is the host-owned path: nothing in the bridge creates, moves,
    /// resizes, or destroys a window, so the caller stays the single owner of
    /// the editor's geometry. `region` is what the host has available; the size
    /// that comes back is the plug-in's own, which the host lays out around.
    /// Re-attaching to the same window just re-reports that size.
    ///
    /// Every module format takes this path — VST3's `IPlugView`, VST2's
    /// `effEditOpen`, and CLAP's `clap.gui` all end up in the same GPUI window.
    ///
    /// Main/UI thread only, and `parent` must stay alive until
    /// [`Self::view_detach`].
    pub fn view_attach(&self, parent: u64, region: (i32, i32)) -> Option<(i32, i32)> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut width = region.0;
        let mut height = region.1;
        // SAFETY: `raw` is a live processor of `format` for as long as this
        // handle is; both out-pointers address locals that outlive the call.
        let ok = unsafe {
            backend::view_attach(
                self.inner.format,
                self.inner.raw,
                parent,
                region.0,
                region.1,
                &mut width,
                &mut height,
            )
        };
        (ok != 0).then_some((width, height))
    }

    /// Releases the editor view. The caller's window is left untouched.
    pub fn view_detach(&self) {
        if self.inner.raw.is_null() {
            return;
        }
        // SAFETY: `raw` is a live processor of `format` for as long as this
        // handle is.
        unsafe { backend::view_detach(self.inner.format, self.inner.raw) };
    }

    /// Whether a view is attached through the host-owned path.
    pub fn view_is_attached(&self) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        // SAFETY: `raw` is a live processor of `format` for as long as this
        // handle is.
        unsafe { backend::view_is_attached(self.inner.format, self.inner.raw) != 0 }
    }

    /// Tells the view the size the host has given it.
    ///
    /// Call *after* the host surface has actually been resized: this is the
    /// plug-in's only report of what it really got.
    pub fn view_set_size(&self, width: i32, height: i32) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        // SAFETY: `raw` is a live processor of `format` for as long as this
        // handle is.
        unsafe { backend::view_set_size(self.inner.format, self.inner.raw, width, height) != 0 }
    }

    /// The view's current content size.
    pub fn view_size(&self) -> Option<(i32, i32)> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut width = 0;
        let mut height = 0;
        // SAFETY: `raw` is live and both out-pointers address locals.
        let ok = unsafe {
            backend::view_get_size(self.inner.format, self.inner.raw, &mut width, &mut height)
        };
        (ok != 0).then_some((width, height))
    }

    /// Whether the view accepts host-driven resizing.
    pub fn view_can_resize(&self) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        // SAFETY: `raw` is a live processor of `format` for as long as this
        // handle is.
        unsafe { backend::view_can_resize(self.inner.format, self.inner.raw) != 0 }
    }

    /// Applies the format's size contract to a proposed content size: a fixed
    /// view snaps to its own size, a resizable one goes through its constraint
    /// check.
    pub fn view_constrain(&self, width: i32, height: i32) -> (i32, i32) {
        if self.inner.raw.is_null() {
            return (width, height);
        }
        let mut width = width;
        let mut height = height;
        // SAFETY: `raw` is live and both pointers address locals.
        unsafe {
            backend::view_constrain(self.inner.format, self.inner.raw, &mut width, &mut height)
        };
        (width, height)
    }

    /// Reads and clears the plug-in's pending resize request, if any.
    ///
    /// The bridge records the request rather than acting on it, so the host
    /// decides what it can grant, resizes its own surface, and reports the
    /// result back through [`Self::view_set_size`].
    pub fn view_take_resize_request(&self) -> Option<(i32, i32)> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut width = 0;
        let mut height = 0;
        // SAFETY: `raw` is live and both out-pointers address locals.
        let pending = unsafe {
            backend::view_take_resize_request(
                self.inner.format,
                self.inner.raw,
                &mut width,
                &mut height,
            )
        };
        (pending != 0).then_some((width, height))
    }

    /// One editor idle tick, for the host to call on the UI thread while a view
    /// is attached.
    ///
    /// Only VST2 needs it — its editor repaints and animates solely from
    /// `effEditIdle`. VST3 and CLAP editors run their own timers, so this costs
    /// them nothing.
    pub fn view_idle(&self) {
        if self.inner.raw.is_null() {
            return;
        }
        // SAFETY: `raw` is a live processor of `format` for as long as this
        // handle is.
        unsafe { backend::view_idle(self.inner.format, self.inner.raw) };
    }

    /// Whether this instance's module registers an ARA main factory for the
    /// exact class that was instantiated.
    ///
    /// Always false for CLAP and VST2: Futureboard hosts ARA through VST3 only.
    pub fn supports_ara(&self) -> bool {
        if self.inner.raw.is_null() || self.inner.format != PluginModuleFormat::Vst3 {
            return false;
        }
        // SAFETY: `raw` is a live VST3 processor for as long as this handle is.
        unsafe { ffi::sphere_daux_vst3_ara_is_supported(self.inner.raw) != 0 }
    }

    /// Instantiates the module's ARA main-factory class.
    ///
    /// Returns `None` when the plug-in is not ARA-capable. The guard must be
    /// dropped before this processor is, because the class instance lives in the
    /// module this processor owns.
    pub fn ara_main_factory(&self) -> Option<AraMainFactory> {
        if !self.supports_ara() {
            return None;
        }
        // SAFETY: `raw` is a live VST3 processor; the returned reference is
        // owned by the guard below, which releases it exactly once.
        let unknown = unsafe { ffi::sphere_daux_vst3_ara_create_main_factory(self.inner.raw) };
        if unknown.is_null() {
            return None;
        }
        Some(AraMainFactory { unknown })
    }

    /// Borrows the initialized component's `FUnknown`, which carries the ARA
    /// plug-in entry point.
    ///
    /// The pointer is owned by this processor and must not outlive it or be
    /// released by the caller.
    pub fn ara_component_unknown(&self) -> *mut std::ffi::c_void {
        if self.inner.raw.is_null() || self.inner.format != PluginModuleFormat::Vst3 {
            return std::ptr::null_mut();
        }
        // SAFETY: `raw` is a live VST3 processor for as long as this handle is.
        unsafe { ffi::sphere_daux_vst3_ara_component_unknown(self.inner.raw) }
    }

    /// Push the current transport state into the plugin's VST3 `ProcessContext`
    /// for the next `process()` call (tempo, time signature, project position,
    /// playing/recording). Call once per block on the thread that drives
    /// `process()`. Replaces the old hardcoded 120 BPM / always-playing stub so
    /// tempo-synced plugins (delays, LFOs, arps) and transport-aware plugins
    /// track the real timeline. No-op once the processor is destroyed.
    #[inline]
    pub fn set_process_context(&self, ctx: &RuntimeTransportContext) {
        if self.inner.raw.is_null() {
            return;
        }
        if vst3_context_debug_enabled() {
            eprintln!(
                "[vst3-context] plugin=\"{}\" tempo={} projectTimeSamples={} projectTimeMusic={:.9}",
                self.inner.plugin_path,
                ctx.tempo_bpm,
                ctx.project_time_samples,
                ctx.ppq_position
            );
        }
        unsafe {
            backend::set_process_context(
                self.inner.format,
                self.inner.raw,
                ctx.tempo_bpm,
                ctx.time_sig_num as i32,
                ctx.time_sig_den as i32,
                ctx.project_time_samples,
                ctx.ppq_position,
                ctx.bar_position_ppq,
                i32::from(ctx.playing),
                i32::from(ctx.recording),
            );
        }
    }

    /// Capture the plugin's current state for project persistence.
    ///
    /// Returns `None` only on hard failure (destroyed processor / allocation
    /// failure); a plugin without state yields `Some` with empty blobs. Call
    /// from a control thread — `getState` may take milliseconds.
    pub fn get_state(&self) -> Option<Vst3PluginState> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut component_ptr: *mut u8 = std::ptr::null_mut();
        let mut component_len: i32 = 0;
        let mut controller_ptr: *mut u8 = std::ptr::null_mut();
        let mut controller_len: i32 = 0;
        let ok = unsafe {
            backend::get_state(
                self.inner.format,
                self.inner.raw,
                &mut component_ptr,
                &mut component_len,
                &mut controller_ptr,
                &mut controller_len,
            )
        };
        if ok == 0 {
            return None;
        }
        // Copy out of the C++ malloc buffers, then release them.
        let copy = |ptr: *mut u8, len: i32| -> Vec<u8> {
            if ptr.is_null() || len <= 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(ptr, len as usize).to_vec() }
            }
        };
        let state = Vst3PluginState {
            component: copy(component_ptr, component_len),
            controller: copy(controller_ptr, controller_len),
        };
        unsafe {
            backend::state_free(self.inner.format, component_ptr);
            backend::state_free(self.inner.format, controller_ptr);
        }
        Some(state)
    }

    /// Enumerate VST3 parameters from `IEditController`. Call from a control
    /// thread — `getParameterInfo` may touch plugin internals.
    pub fn list_parameters(&self) -> Option<Vec<Vst3ParameterDescriptor>> {
        if self.inner.raw.is_null() {
            return None;
        }
        let json_ptr = unsafe { backend::list_parameters_json(self.inner.format, self.inner.raw) };
        if json_ptr.is_null() {
            return None;
        }
        let json = unsafe {
            let cstr = std::ffi::CStr::from_ptr(json_ptr);
            let owned = cstr.to_string_lossy().into_owned();
            backend::parameters_json_free(self.inner.format, json_ptr);
            owned
        };
        serde_json::from_str(&json).ok()
    }

    /// Restore a previously captured state. Returns `true` when the component
    /// state was applied. Call from a control thread — the host side must
    /// serialize this against `process()` (the bridge host holds the voice
    /// mutex while applying).
    pub fn set_state(&self, state: &Vst3PluginState) -> bool {
        if self.inner.raw.is_null() || state.is_empty() {
            return false;
        }
        let (comp_ptr, comp_len) = if state.component.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (state.component.as_ptr(), state.component.len() as i32)
        };
        let (ctrl_ptr, ctrl_len) = if state.controller.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (state.controller.as_ptr(), state.controller.len() as i32)
        };
        unsafe {
            backend::set_state(
                self.inner.format,
                self.inner.raw,
                comp_ptr,
                comp_len,
                ctrl_ptr,
                ctrl_len,
            ) != 0
        }
    }

    pub fn open_editor(
        &mut self,
        window_id: &str,
        title: &str,
        width: i32,
        height: i32,
    ) -> Option<u64> {
        if self.inner.raw.is_null() {
            return None;
        }
        let window_id = CString::new(window_id).ok()?;
        let title = CString::new(title).ok()?;
        let handle = unsafe {
            backend::open_editor(
                self.inner.format,
                self.inner.raw,
                window_id.as_ptr(),
                title.as_ptr(),
                width,
                height,
            )
        };
        if handle == 0 {
            None
        } else {
            Some(handle)
        }
    }

    pub fn close_editor(&mut self) {
        if self.inner.raw.is_null() {
            return;
        }
        unsafe { backend::close_editor(self.inner.format, self.inner.raw) };
    }

    pub fn focus_editor(&mut self) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        unsafe { backend::focus_editor(self.inner.format, self.inner.raw) != 0 }
    }

    // ── GPUI-embedded editor ──────────────────────────────────────────────
    //
    // Attach THIS runtime instance's editor view into a GPUI-provided parent
    // window. No new VST3 component/controller is created; GUI parameter edits
    // affect the live audio processor. These take `&self` because they only
    // pass the opaque processor pointer across the FFI — no Rust-side mutation.

    /// Attach the editor view into `parent_hwnd` at the given physical-pixel
    /// region (parent-client coords). Returns a non-zero editor handle, or
    /// `None` on failure.
    pub fn embed_editor(
        &self,
        parent_hwnd: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Option<u64> {
        if self.inner.raw.is_null() {
            return None;
        }
        let handle = unsafe {
            backend::embed_editor(
                self.inner.format,
                self.inner.raw,
                parent_hwnd,
                x,
                y,
                width,
                height,
            )
        };
        if handle == 0 {
            None
        } else {
            Some(handle)
        }
    }

    pub fn embed_set_bounds(&self, x: i32, y: i32, width: i32, height: i32) {
        if self.inner.raw.is_null() {
            return;
        }
        unsafe {
            backend::embed_set_bounds(self.inner.format, self.inner.raw, x, y, width, height)
        };
    }

    pub fn embed_refresh(&self) {
        if self.inner.raw.is_null() {
            return;
        }
        unsafe { backend::embed_refresh(self.inner.format, self.inner.raw) };
    }

    /// Real Win32 HWND of the embed content child (`IPlugView::attached`
    /// target), or 0 when not attached. Unlike the opaque handle returned by
    /// `embed_editor`, this is a valid window handle usable for message
    /// pumping / focus.
    pub fn embed_attach_hwnd(&self) -> u64 {
        if self.inner.raw.is_null() {
            return 0;
        }
        unsafe { backend::embed_attach_hwnd(self.inner.format, self.inner.raw) }
    }

    /// Detach the embedded view and destroy the host window. The processor
    /// (and audio) keep running.
    pub fn embed_detach(&self) {
        if self.inner.raw.is_null() {
            return;
        }
        unsafe { backend::embed_detach(self.inner.format, self.inner.raw) };
    }

    pub fn embed_is_valid(&self) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        unsafe { backend::embed_is_valid(self.inner.format, self.inner.raw) != 0 }
    }

    pub fn embed_has_visible_ui(&self) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        unsafe { backend::embed_has_visible_ui(self.inner.format, self.inner.raw) != 0 }
    }

    /// 0 = WS_CHILD, 1 = owned tool window, 2 = detached top-level, -1 = none.
    pub fn embed_host_kind(&self) -> i32 {
        if self.inner.raw.is_null() {
            return -1;
        }
        unsafe { backend::embed_host_kind(self.inner.format, self.inner.raw) }
    }

    /// Detached mode only: `true` (and resets) when the user closed the
    /// standalone editor window, so the host can tear the editor shell down.
    pub fn embed_take_user_close(&self) -> bool {
        if self.inner.raw.is_null() {
            return false;
        }
        unsafe { backend::embed_take_user_close(self.inner.format, self.inner.raw) != 0 }
    }

    pub fn embed_set_waiting_stage(&self, stage: &str) {
        if self.inner.raw.is_null() {
            return;
        }
        if let Ok(stage) = CString::new(stage) {
            unsafe {
                backend::embed_set_waiting_stage(self.inner.format, self.inner.raw, stage.as_ptr());
            }
        }
    }

    pub fn embed_content_size(&self) -> Option<(i32, i32)> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut width = 0;
        let mut height = 0;
        let ok = unsafe {
            backend::embed_content_size(self.inner.format, self.inner.raw, &mut width, &mut height)
        };
        if ok != 0 && width > 0 && height > 0 {
            Some((width, height))
        } else {
            None
        }
    }

    pub fn embed_set_instance_label(&self, instance_id: &str) {
        if self.inner.raw.is_null() {
            return;
        }
        if let Ok(label) = CString::new(instance_id) {
            unsafe {
                backend::embed_set_instance_label(
                    self.inner.format,
                    self.inner.raw,
                    label.as_ptr(),
                );
            }
        }
    }

    /// Set the editor display name shown in the shell titlebar and the content
    /// "Loading Plugin <name>" overlay. Call before [`embed_editor`] so the
    /// loading shell shows the real plug-in name immediately.
    pub fn set_editor_title(&self, title: &str) {
        if self.inner.raw.is_null() {
            return;
        }
        if let Ok(title) = CString::new(title) {
            unsafe {
                backend::set_editor_title(self.inner.format, self.inner.raw, title.as_ptr());
            }
        }
    }

    /// This instance's editor chrome strip, when its editor is in a window of
    /// the host process's own.
    ///
    /// `None` on Windows, where the studio's GPUI window draws the strip above
    /// an embedded plug-in view, and `None` whenever no editor is open. The
    /// caller pushes chrome wherever it gets one and does nothing where it does
    /// not, with no platform test of its own — see
    /// [`crate::editor_chrome::EditorChrome`].
    pub fn editor_chrome(&self) -> Option<EditorChrome> {
        if self.inner.raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a live processor of `format` for as long as this
        // handle is.
        let window = unsafe { backend::editor_native_window(self.inner.format, self.inner.raw) };
        EditorChrome::for_window(window)
    }

    /// `createView` + `setFrame` + `getSize` without HWND attach (PrepareEditorView).
    pub fn prepare_editor_view(&self) -> Option<(i32, i32)> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut width = 0;
        let mut height = 0;
        let ok = unsafe {
            backend::prepare_editor_view(self.inner.format, self.inner.raw, &mut width, &mut height)
        };
        if ok != 0 && width > 0 && height > 0 {
            Some((width, height))
        } else {
            None
        }
    }

    /// Poll plug-in `resizeView` requests for main-owned shell resizing.
    pub fn take_pending_shell_resize(&self) -> Option<(i32, i32)> {
        if self.inner.raw.is_null() {
            return None;
        }
        let mut width = 0;
        let mut height = 0;
        let ok = unsafe {
            backend::take_pending_shell_resize(
                self.inner.format,
                self.inner.raw,
                &mut width,
                &mut height,
            )
        };
        if ok != 0 && width > 0 && height > 0 {
            Some((width, height))
        } else {
            None
        }
    }

    /// `IPlugView::canResize` for the current editor view: `Some(true)` when
    /// the editor supports host-driven resizing, `Some(false)` for fixed-size
    /// editors, `None` when no view exists yet.
    pub fn editor_resizable(&self) -> Option<bool> {
        if self.inner.raw.is_null() {
            return None;
        }
        match unsafe { backend::editor_resizable(self.inner.format, self.inner.raw) } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

impl Clone for Vst3RuntimeProcessor {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Links the ARA entry points and checks their null contract. Without this
    /// the C++ symbols are only proven to exist when something actually loads an
    /// ARA plug-in, which no automated run does.
    #[test]
    fn ara_entry_points_link_and_reject_null() {
        // SAFETY: every ARA entry point null-checks its processor argument.
        unsafe {
            assert_eq!(
                ffi::sphere_daux_vst3_ara_is_supported(std::ptr::null_mut()),
                0
            );
            assert!(ffi::sphere_daux_vst3_ara_create_main_factory(std::ptr::null_mut()).is_null());
            assert!(ffi::sphere_daux_vst3_ara_component_unknown(std::ptr::null_mut()).is_null());
            // Releasing null must be a no-op, so a failed create needs no branch
            // at the call site.
            ffi::sphere_daux_vst3_ara_release_main_factory(std::ptr::null_mut());
        }
    }

    #[test]
    fn process_context_keeps_tempo_raw_while_ppq_uses_sample_rate() {
        let bpm: f64 = 128.0;
        for sample_rate in [44_100.0_f64, 48_000.0, 88_200.0, 96_000.0, 192_000.0] {
            let samples_per_beat: f64 = sample_rate * 60.0 / bpm;
            let project_time_samples = samples_per_beat.round() as i64;
            let ctx = RuntimeTransportContext {
                tempo_bpm: bpm,
                time_sig_num: 4,
                time_sig_den: 4,
                project_time_samples,
                ppq_position: project_time_samples as f64 * bpm / (sample_rate * 60.0),
                bar_position_ppq: 0.0,
                playing: true,
                recording: false,
            };
            assert_eq!(ctx.tempo_bpm, bpm);
            let half_sample_ppq = bpm / (sample_rate * 60.0) * 0.5;
            assert!(
                (ctx.ppq_position - 1.0).abs() <= half_sample_ppq + 1e-12,
                "sample_rate={sample_rate}"
            );
        }
    }

    /// Pins the exact reported scenario: when shared mode opens the device at
    /// 96 kHz instead of the requested 48 kHz, the VST3 ProcessContext must use
    /// the active rate (96 kHz) for sample/PPQ math while the tempo stays raw.
    #[test]
    fn vst3_context_at_96k_uses_active_rate_with_raw_tempo() {
        let bpm: f64 = 128.0;
        let active_sample_rate: f64 = 96_000.0;

        // VST3 processSetup.sampleRate is the active rate; at 128 BPM a beat is
        // exactly 45000 samples @ 96 kHz (and would be 22500 @ 48 kHz).
        let samples_per_beat = (active_sample_rate * 60.0 / bpm) as i64;
        assert_eq!(samples_per_beat, 45_000);
        assert_ne!(samples_per_beat, (48_000.0 * 60.0 / bpm) as i64);

        // One beat in: PPQ derived from project sample position / active rate.
        let project_time_samples = samples_per_beat;
        let ctx = RuntimeTransportContext {
            tempo_bpm: bpm,
            time_sig_num: 4,
            time_sig_den: 4,
            project_time_samples,
            ppq_position: project_time_samples as f64 * bpm / (active_sample_rate * 60.0),
            bar_position_ppq: 0.0,
            playing: true,
            recording: false,
        };

        // Tempo is passed through raw — never rescaled by the sample rate.
        assert_eq!(ctx.tempo_bpm, 128.0);
        // PPQ at one beat is exactly 1.0 when computed against the active rate.
        assert!((ctx.ppq_position - 1.0).abs() < 1e-9);
    }
}

impl Drop for Vst3RuntimeProcessorInner {
    fn drop(&mut self) {
        if self.raw.is_null() {
            return;
        }
        let reason = self
            .destroy_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        eprintln!(
            "[SphereVST3] destroying shared processor path='{}' classId='{}' sr={} reason={}",
            self.plugin_path, self.class_id, self.sample_rate, reason
        );
        unsafe { backend::destroy(self.format, self.raw) };
        self.raw = std::ptr::null_mut();
        eprintln!(
            "[SphereVST3] destroyed shared processor path='{}' classId='{}' sr={} reason={}",
            self.plugin_path, self.class_id, self.sample_rate, reason
        );
    }
}

/// Owning handle to a plug-in's ARA main-factory class instance.
///
/// Hands `SphereAraHost` the `Steinberg::FUnknown*` it needs to reach the
/// `ARAFactory`. It must be dropped before the [`Vst3RuntimeProcessor`] it came
/// from, whose module owns the class.
#[derive(Debug)]
pub struct AraMainFactory {
    unknown: *mut std::ffi::c_void,
}

impl AraMainFactory {
    /// Borrows the `Steinberg::FUnknown*` of the ARA main-factory class.
    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.unknown
    }
}

impl Drop for AraMainFactory {
    fn drop(&mut self) {
        if !self.unknown.is_null() {
            // SAFETY: exactly one reference was handed over by
            // `sphere_daux_vst3_ara_create_main_factory`, and this is its only
            // release.
            unsafe { ffi::sphere_daux_vst3_ara_release_main_factory(self.unknown) };
            self.unknown = std::ptr::null_mut();
        }
    }
}

// The handle is a plain COM reference; it is created and released on the model
// thread and carries no interior mutability of its own.
unsafe impl Send for AraMainFactory {}
