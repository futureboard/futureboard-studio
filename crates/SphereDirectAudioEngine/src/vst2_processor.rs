//! VST2 runtime bridge FFI.
//!
//! Declares the `sphere_daux_vst2_*` C surface (see
//! `vst2bridge/include/sphere_daux_vst2_processor.h`). Format selection and
//! dispatch live in [`crate::plugin_backend`].

/// Opaque VST2 instance. Distinct from the VST3 and CLAP handles at the C
/// level; all three are carried through Rust as `*mut SphereDauxVst3Processor`
/// because the runtime handle stores exactly one pointer, tagged by
/// [`crate::plugin_backend::PluginModuleFormat`].
#[repr(C)]
pub struct SphereDauxVst2Processor {
    _private: [u8; 0],
}
/// Raw VST2 bridge entry points, mirroring `vst3_processor::ffi`.
pub(crate) mod ffi {
    use super::SphereDauxVst2Processor;
    use crate::vst3_processor::Vst3MidiEvent;
    use std::os::raw::{c_char, c_double, c_float};

    extern "C" {
        pub(crate) fn sphere_daux_vst2_bridge_probe() -> i32;
        pub(crate) fn sphere_daux_vst2_last_error() -> *const c_char;
        pub(crate) fn sphere_daux_vst2_create(
            plugin_path: *const c_char,
            class_id: *const c_char,
            sample_rate: c_double,
        ) -> *mut SphereDauxVst2Processor;
        pub(crate) fn sphere_daux_vst2_destroy(processor: *mut SphereDauxVst2Processor);
        pub(crate) fn sphere_daux_vst2_process_stereo_sample(
            processor: *mut SphereDauxVst2Processor,
            in_l: c_float,
            in_r: c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
        ) -> i32;
        #[allow(dead_code)]
        pub(crate) fn sphere_daux_vst2_process_stereo_block(
            processor: *mut SphereDauxVst2Processor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
            frames: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_process_stereo_block_with_midi(
            processor: *mut SphereDauxVst2Processor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
            frames: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_process_main_output_block_with_midi(
            processor: *mut SphereDauxVst2Processor,
            in_l: *const c_float,
            in_r: *const c_float,
            out_interleaved: *mut c_float,
            frames: i32,
            output_channels: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_event_input_bus_count(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_audio_input_bus_count(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_audio_output_bus_count(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_main_audio_input_channel_count(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_main_audio_output_channel_count(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_output_bus_channel_counts(
            processor: *mut SphereDauxVst2Processor,
            out_counts: *mut i32,
            max_count: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_process_count(
            processor: *mut SphereDauxVst2Processor,
        ) -> u64;
        pub(crate) fn sphere_daux_vst2_last_input_peak(
            processor: *mut SphereDauxVst2Processor,
        ) -> c_double;
        pub(crate) fn sphere_daux_vst2_last_output_peak(
            processor: *mut SphereDauxVst2Processor,
        ) -> c_double;
        pub(crate) fn sphere_daux_vst2_last_difference_peak(
            processor: *mut SphereDauxVst2Processor,
        ) -> c_double;
        pub(crate) fn sphere_daux_vst2_set_param(
            processor: *mut SphereDauxVst2Processor,
            param_id: u32,
            value: c_double,
        );
        pub(crate) fn sphere_daux_vst2_open_editor(
            processor: *mut SphereDauxVst2Processor,
            window_id: *const c_char,
            title: *const c_char,
            width: i32,
            height: i32,
        ) -> u64;
        pub(crate) fn sphere_daux_vst2_close_editor(processor: *mut SphereDauxVst2Processor);
        pub(crate) fn sphere_daux_vst2_focus_editor(processor: *mut SphereDauxVst2Processor)
            -> i32;
        pub(crate) fn sphere_daux_vst2_is_valid(processor: *mut SphereDauxVst2Processor) -> i32;
        pub(crate) fn sphere_daux_vst2_get_latency_samples(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_set_process_context(
            processor: *mut SphereDauxVst2Processor,
            tempo: c_double,
            time_sig_num: i32,
            time_sig_den: i32,
            project_time_samples: i64,
            ppq: c_double,
            bar_ppq: c_double,
            playing: i32,
            recording: i32,
        );
        pub(crate) fn sphere_daux_vst2_embed_editor(
            processor: *mut SphereDauxVst2Processor,
            parent_hwnd: u64,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> u64;
        pub(crate) fn sphere_daux_vst2_embed_set_bounds(
            processor: *mut SphereDauxVst2Processor,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        );
        pub(crate) fn sphere_daux_vst2_embed_refresh(processor: *mut SphereDauxVst2Processor);
        pub(crate) fn sphere_daux_vst2_embed_attach_hwnd(
            processor: *mut SphereDauxVst2Processor,
        ) -> u64;
        pub(crate) fn sphere_daux_vst2_embed_detach(processor: *mut SphereDauxVst2Processor);
        pub(crate) fn sphere_daux_vst2_embed_is_valid(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_embed_has_visible_ui(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_embed_host_kind(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_embed_take_user_close(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_embed_set_waiting_stage(
            processor: *mut SphereDauxVst2Processor,
            stage: *const c_char,
        );
        pub(crate) fn sphere_daux_vst2_embed_content_size(
            processor: *mut SphereDauxVst2Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_embed_set_instance_label(
            processor: *mut SphereDauxVst2Processor,
            instance_id: *const c_char,
        );
        pub(crate) fn sphere_daux_vst2_set_editor_title(
            processor: *mut SphereDauxVst2Processor,
            title: *const c_char,
        );
        pub(crate) fn sphere_daux_vst2_prepare_editor_view(
            processor: *mut SphereDauxVst2Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_take_pending_shell_resize(
            processor: *mut SphereDauxVst2Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        /// `NSWindow*` of this instance's host-owned editor as an opaque handle,
        /// or 0 when there is no such window. Addresses the shared editor
        /// chrome strip — see [`crate::plugin_backend::editor_chrome`].
        pub(crate) fn sphere_daux_vst2_editor_native_window(
            processor: *mut SphereDauxVst2Processor,
        ) -> u64;
        pub(crate) fn sphere_daux_vst2_editor_resizable(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        // Host-owned view host: the caller owns the window, these drive only
        // the plug-in's `effEdit*` surface.
        pub(crate) fn sphere_daux_vst2_view_attach(
            processor: *mut SphereDauxVst2Processor,
            parent_hwnd: u64,
            width: i32,
            height: i32,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_view_detach(processor: *mut SphereDauxVst2Processor);
        pub(crate) fn sphere_daux_vst2_view_is_attached(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_view_set_size(
            processor: *mut SphereDauxVst2Processor,
            width: i32,
            height: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_view_get_size(
            processor: *mut SphereDauxVst2Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_view_can_resize(
            processor: *mut SphereDauxVst2Processor,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_view_constrain(
            processor: *mut SphereDauxVst2Processor,
            io_width: *mut i32,
            io_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_view_take_resize_request(
            processor: *mut SphereDauxVst2Processor,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_view_idle(processor: *mut SphereDauxVst2Processor);
        pub(crate) fn sphere_daux_vst2_get_state(
            processor: *mut SphereDauxVst2Processor,
            out_component: *mut *mut u8,
            out_component_len: *mut i32,
            out_controller: *mut *mut u8,
            out_controller_len: *mut i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_set_state(
            processor: *mut SphereDauxVst2Processor,
            component_data: *const u8,
            component_len: i32,
            controller_data: *const u8,
            controller_len: i32,
        ) -> i32;
        pub(crate) fn sphere_daux_vst2_state_free(data: *mut u8);
        pub(crate) fn sphere_daux_vst2_list_parameters_json(
            processor: *mut SphereDauxVst2Processor,
        ) -> *mut c_char;
        pub(crate) fn sphere_daux_vst2_parameters_json_free(data: *mut c_char);
    }

    // Short aliases, matching `vst3_processor::ffi`, so `backend` names one
    // identifier per operation regardless of format.
    pub(crate) use self::{
        sphere_daux_vst2_audio_input_bus_count as audio_input_bus_count,
        sphere_daux_vst2_audio_output_bus_count as audio_output_bus_count,
        sphere_daux_vst2_bridge_probe as bridge_probe,
        sphere_daux_vst2_close_editor as close_editor, sphere_daux_vst2_create as create,
        sphere_daux_vst2_destroy as destroy,
        sphere_daux_vst2_editor_native_window as editor_native_window,
        sphere_daux_vst2_editor_resizable as editor_resizable,
        sphere_daux_vst2_embed_attach_hwnd as embed_attach_hwnd,
        sphere_daux_vst2_embed_content_size as embed_content_size,
        sphere_daux_vst2_embed_detach as embed_detach,
        sphere_daux_vst2_embed_editor as embed_editor,
        sphere_daux_vst2_embed_has_visible_ui as embed_has_visible_ui,
        sphere_daux_vst2_embed_host_kind as embed_host_kind,
        sphere_daux_vst2_embed_is_valid as embed_is_valid,
        sphere_daux_vst2_embed_refresh as embed_refresh,
        sphere_daux_vst2_embed_set_bounds as embed_set_bounds,
        sphere_daux_vst2_embed_set_instance_label as embed_set_instance_label,
        sphere_daux_vst2_embed_set_waiting_stage as embed_set_waiting_stage,
        sphere_daux_vst2_embed_take_user_close as embed_take_user_close,
        sphere_daux_vst2_event_input_bus_count as event_input_bus_count,
        sphere_daux_vst2_focus_editor as focus_editor,
        sphere_daux_vst2_get_latency_samples as get_latency_samples,
        sphere_daux_vst2_get_state as get_state, sphere_daux_vst2_is_valid as is_valid,
        sphere_daux_vst2_last_difference_peak as last_difference_peak,
        sphere_daux_vst2_last_error as last_error,
        sphere_daux_vst2_last_input_peak as last_input_peak,
        sphere_daux_vst2_last_output_peak as last_output_peak,
        sphere_daux_vst2_list_parameters_json as list_parameters_json,
        sphere_daux_vst2_main_audio_input_channel_count as main_audio_input_channel_count,
        sphere_daux_vst2_main_audio_output_channel_count as main_audio_output_channel_count,
        sphere_daux_vst2_open_editor as open_editor,
        sphere_daux_vst2_output_bus_channel_counts as output_bus_channel_counts,
        sphere_daux_vst2_parameters_json_free as parameters_json_free,
        sphere_daux_vst2_prepare_editor_view as prepare_editor_view,
        sphere_daux_vst2_process_count as process_count,
        sphere_daux_vst2_process_main_output_block_with_midi as process_main_output_block_with_midi,
        sphere_daux_vst2_process_stereo_block_with_midi as process_stereo_block_with_midi,
        sphere_daux_vst2_process_stereo_sample as process_stereo_sample,
        sphere_daux_vst2_set_editor_title as set_editor_title,
        sphere_daux_vst2_set_param as set_param,
        sphere_daux_vst2_set_process_context as set_process_context,
        sphere_daux_vst2_set_state as set_state, sphere_daux_vst2_state_free as state_free,
        sphere_daux_vst2_take_pending_shell_resize as take_pending_shell_resize,
        sphere_daux_vst2_view_attach as view_attach,
        sphere_daux_vst2_view_can_resize as view_can_resize,
        sphere_daux_vst2_view_constrain as view_constrain,
        sphere_daux_vst2_view_detach as view_detach,
        sphere_daux_vst2_view_get_size as view_get_size, sphere_daux_vst2_view_idle as view_idle,
        sphere_daux_vst2_view_is_attached as view_is_attached,
        sphere_daux_vst2_view_set_size as view_set_size,
        sphere_daux_vst2_view_take_resize_request as view_take_resize_request,
    };
}
