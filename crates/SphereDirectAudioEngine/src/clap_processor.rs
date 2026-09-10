//! CLAP runtime bridge FFI.
//!
//! Declares the `sphere_daux_clap_*` C surface (see
//! `clapbridge/include/sphere_daux_clap_processor.h`). Format selection and
//! dispatch live in [`crate::plugin_backend`].

/// Opaque CLAP instance. Distinct from the VST3 and VST2 handles at the C
/// level; all three are carried through Rust as `*mut SphereDauxVst3Processor`
/// because the runtime handle stores exactly one pointer, tagged by
/// [`crate::plugin_backend::PluginModuleFormat`].
#[repr(C)]
pub struct SphereDauxClapProcessor {
    _private: [u8; 0],
}

/// Raw CLAP bridge entry points, mirroring `vst3_processor::ffi`.
pub(crate) mod ffi {
    use super::SphereDauxClapProcessor;
    use crate::vst3_processor::Vst3MidiEvent;
    use std::os::raw::{c_char, c_double, c_float};

    extern "C" {
        pub(crate) fn sphere_daux_clap_bridge_probe() -> i32;
        pub(crate) fn sphere_daux_clap_last_error() -> *const c_char;
        pub(crate) fn sphere_daux_clap_create(
            plugin_path: *const c_char,
            class_id: *const c_char,
            sample_rate: c_double,
        ) -> *mut SphereDauxClapProcessor;
        pub(crate) fn sphere_daux_clap_destroy(processor: *mut SphereDauxClapProcessor);
        pub(crate) fn sphere_daux_clap_process_stereo_sample(
            processor: *mut SphereDauxClapProcessor,
            in_l: c_float,
            in_r: c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
        ) -> i32;
        #[allow(dead_code)]
        pub(crate) fn sphere_daux_clap_process_stereo_block(
            processor: *mut SphereDauxClapProcessor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
            frames: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_process_stereo_block_with_midi(
            processor: *mut SphereDauxClapProcessor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
            frames: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_process_main_output_block_with_midi(
            processor: *mut SphereDauxClapProcessor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_interleaved: *mut c_float,
            frames: i32,
            output_channels: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_event_input_bus_count(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_audio_input_bus_count(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_audio_output_bus_count(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_main_audio_input_channel_count(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_main_audio_output_channel_count(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_output_bus_channel_counts(
            processor: *mut SphereDauxClapProcessor,
            out_counts: *mut i32,
            max_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_process_count(
            processor: *mut SphereDauxClapProcessor,
        ) -> u64;
        pub(crate) fn sphere_daux_clap_last_input_peak(
            processor: *mut SphereDauxClapProcessor,
        ) -> c_double;
        pub(crate) fn sphere_daux_clap_last_output_peak(
            processor: *mut SphereDauxClapProcessor,
        ) -> c_double;
        pub(crate) fn sphere_daux_clap_last_difference_peak(
            processor: *mut SphereDauxClapProcessor,
        ) -> c_double;
        pub(crate) fn sphere_daux_clap_set_param(
            processor: *mut SphereDauxClapProcessor,
            param_id: u32,
            value: c_double,
        );
        pub(crate) fn sphere_daux_clap_open_editor(
            processor: *mut SphereDauxClapProcessor,
            window_id: *const c_char,
            title: *const c_char,
            width: i32,
            height: i32,
        ) -> u64;
        pub(crate) fn sphere_daux_clap_close_editor(processor: *mut SphereDauxClapProcessor);
        pub(crate) fn sphere_daux_clap_focus_editor(processor: *mut SphereDauxClapProcessor)
            -> i32;
        pub(crate) fn sphere_daux_clap_is_valid(processor: *mut SphereDauxClapProcessor) -> i32;
        pub(crate) fn sphere_daux_clap_get_latency_samples(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_set_process_context(
            processor: *mut SphereDauxClapProcessor,
            tempo: c_double,
            time_sig_num: i32,
            time_sig_den: i32,
            project_time_samples: i64,
            ppq: c_double,
            bar_ppq: c_double,
            playing: i32,
            recording: i32,
        );
        pub(crate) fn sphere_daux_clap_embed_editor(
            processor: *mut SphereDauxClapProcessor,
            parent_hwnd: u64,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> u64;
        pub(crate) fn sphere_daux_clap_embed_set_bounds(
            processor: *mut SphereDauxClapProcessor,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        );
        pub(crate) fn sphere_daux_clap_embed_refresh(processor: *mut SphereDauxClapProcessor);
        pub(crate) fn sphere_daux_clap_embed_attach_hwnd(
            processor: *mut SphereDauxClapProcessor,
        ) -> u64;
        pub(crate) fn sphere_daux_clap_embed_detach(processor: *mut SphereDauxClapProcessor);
        pub(crate) fn sphere_daux_clap_embed_is_valid(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_embed_has_visible_ui(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_embed_host_kind(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_embed_take_user_close(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_embed_set_waiting_stage(
            processor: *mut SphereDauxClapProcessor,
            stage: *const c_char,
        );
        pub(crate) fn sphere_daux_clap_embed_content_size(
            processor: *mut SphereDauxClapProcessor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_embed_set_instance_label(
            processor: *mut SphereDauxClapProcessor,
            instance_id: *const c_char,
        );
        pub(crate) fn sphere_daux_clap_set_editor_title(
            processor: *mut SphereDauxClapProcessor,
            title: *const c_char,
        );
        pub(crate) fn sphere_daux_clap_prepare_editor_view(
            processor: *mut SphereDauxClapProcessor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_take_pending_shell_resize(
            processor: *mut SphereDauxClapProcessor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        /// `NSWindow*` of this instance's host-owned editor as an opaque handle,
        /// or 0 when there is no such window. Addresses the shared editor
        /// chrome strip — see [`crate::plugin_backend::editor_chrome`].
        pub(crate) fn sphere_daux_clap_editor_native_window(
            processor: *mut SphereDauxClapProcessor,
        ) -> u64;
        pub(crate) fn sphere_daux_clap_editor_resizable(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        // Host-owned view host: the caller owns the window, these drive only
        // the plug-in's `clap.gui` extension.
        pub(crate) fn sphere_daux_clap_view_attach(
            processor: *mut SphereDauxClapProcessor,
            parent_hwnd: u64,
            width: i32,
            height: i32,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_view_detach(processor: *mut SphereDauxClapProcessor);
        pub(crate) fn sphere_daux_clap_view_is_attached(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_view_set_size(
            processor: *mut SphereDauxClapProcessor,
            width: i32,
            height: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_view_get_size(
            processor: *mut SphereDauxClapProcessor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_view_can_resize(
            processor: *mut SphereDauxClapProcessor,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_view_constrain(
            processor: *mut SphereDauxClapProcessor,
            io_width: *mut i32,
            io_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_view_take_resize_request(
            processor: *mut SphereDauxClapProcessor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_get_state(
            processor: *mut SphereDauxClapProcessor,
            out_component: *mut *mut u8,
            out_component_len: *mut i32,
            out_controller: *mut *mut u8,
            out_controller_len: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_set_state(
            processor: *mut SphereDauxClapProcessor,
            component_data: *const u8,
            component_len: i32,
            controller_data: *const u8,
            controller_len: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_clap_state_free(data: *mut u8);
        pub(crate) fn sphere_daux_clap_list_parameters_json(
            processor: *mut SphereDauxClapProcessor,
        ) -> *mut c_char;
        pub(crate) fn sphere_daux_clap_parameters_json_free(data: *mut c_char);
    }

    // Short aliases, matching `vst3_processor::ffi`, so `backend` names one
    // identifier per operation regardless of format.
    pub(crate) use self::{
        sphere_daux_clap_audio_input_bus_count as audio_input_bus_count,
        sphere_daux_clap_audio_output_bus_count as audio_output_bus_count,
        sphere_daux_clap_bridge_probe as bridge_probe,
        sphere_daux_clap_close_editor as close_editor, sphere_daux_clap_create as create,
        sphere_daux_clap_destroy as destroy,
        sphere_daux_clap_editor_native_window as editor_native_window,
        sphere_daux_clap_editor_resizable as editor_resizable,
        sphere_daux_clap_embed_attach_hwnd as embed_attach_hwnd,
        sphere_daux_clap_embed_content_size as embed_content_size,
        sphere_daux_clap_embed_detach as embed_detach,
        sphere_daux_clap_embed_editor as embed_editor,
        sphere_daux_clap_embed_has_visible_ui as embed_has_visible_ui,
        sphere_daux_clap_embed_host_kind as embed_host_kind,
        sphere_daux_clap_embed_is_valid as embed_is_valid,
        sphere_daux_clap_embed_refresh as embed_refresh,
        sphere_daux_clap_embed_set_bounds as embed_set_bounds,
        sphere_daux_clap_embed_set_instance_label as embed_set_instance_label,
        sphere_daux_clap_embed_set_waiting_stage as embed_set_waiting_stage,
        sphere_daux_clap_embed_take_user_close as embed_take_user_close,
        sphere_daux_clap_event_input_bus_count as event_input_bus_count,
        sphere_daux_clap_focus_editor as focus_editor,
        sphere_daux_clap_get_latency_samples as get_latency_samples,
        sphere_daux_clap_get_state as get_state, sphere_daux_clap_is_valid as is_valid,
        sphere_daux_clap_last_difference_peak as last_difference_peak,
        sphere_daux_clap_last_error as last_error,
        sphere_daux_clap_last_input_peak as last_input_peak,
        sphere_daux_clap_last_output_peak as last_output_peak,
        sphere_daux_clap_list_parameters_json as list_parameters_json,
        sphere_daux_clap_main_audio_input_channel_count as main_audio_input_channel_count,
        sphere_daux_clap_main_audio_output_channel_count as main_audio_output_channel_count,
        sphere_daux_clap_open_editor as open_editor,
        sphere_daux_clap_output_bus_channel_counts as output_bus_channel_counts,
        sphere_daux_clap_parameters_json_free as parameters_json_free,
        sphere_daux_clap_prepare_editor_view as prepare_editor_view,
        sphere_daux_clap_process_count as process_count,
        sphere_daux_clap_process_main_output_block_with_midi as process_main_output_block_with_midi,
        sphere_daux_clap_process_stereo_block_with_midi as process_stereo_block_with_midi,
        sphere_daux_clap_process_stereo_sample as process_stereo_sample,
        sphere_daux_clap_set_editor_title as set_editor_title,
        sphere_daux_clap_set_param as set_param,
        sphere_daux_clap_set_process_context as set_process_context,
        sphere_daux_clap_set_state as set_state, sphere_daux_clap_state_free as state_free,
        sphere_daux_clap_take_pending_shell_resize as take_pending_shell_resize,
        sphere_daux_clap_view_attach as view_attach,
        sphere_daux_clap_view_can_resize as view_can_resize,
        sphere_daux_clap_view_constrain as view_constrain,
        sphere_daux_clap_view_detach as view_detach,
        sphere_daux_clap_view_get_size as view_get_size,
        sphere_daux_clap_view_is_attached as view_is_attached,
        sphere_daux_clap_view_set_size as view_set_size,
        sphere_daux_clap_view_take_resize_request as view_take_resize_request,
    };
}
