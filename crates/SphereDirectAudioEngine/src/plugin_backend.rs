//! Which native bridge backs a runtime plug-in instance, and the dispatch layer
//! that routes [`crate::vst3_processor::Vst3RuntimeProcessor`] to it.
//!
//! There is deliberately no second (or third) Rust processor type: the engine,
//! the mixer, project state, PDC, the offline renderer, and the isolated host
//! process all address plug-ins through one handle, so the format branch lives
//! here and nowhere else.

/// Which native bridge backs a runtime processor instance.
///
/// Resolved once at construction — never re-derived on the audio path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginModuleFormat {
    Vst3,
    Vst2,
    Clap,
}

impl PluginModuleFormat {
    /// Explicit format label as it travels in insert params / IPC
    /// (`"VST3"`, `"VST2"`, `"CLAP"`). Unknown labels return `None` so the
    /// caller can fall back to path detection rather than silently guessing
    /// VST3 — `"AU"` and `"BuiltIn"` are not module formats and take other
    /// routes entirely.
    pub fn from_label(label: &str) -> Option<Self> {
        if label.eq_ignore_ascii_case("VST3") {
            Some(Self::Vst3)
        } else if label.eq_ignore_ascii_case("VST2") || label.eq_ignore_ascii_case("VST") {
            Some(Self::Vst2)
        } else if label.eq_ignore_ascii_case("CLAP") {
            Some(Self::Clap)
        } else {
            None
        }
    }

    /// Detect from the module path. `.vst3` is VST3 and `.clap` is CLAP on
    /// every platform; a bare `.dll` (Windows) or a `.vst` bundle (macOS) is
    /// VST2. Used when no explicit label is available — e.g. a legacy project
    /// or an older host process that predates the IPC format field.
    pub fn detect(plugin_path: &str) -> Self {
        let lower = plugin_path
            .trim_end_matches(['/', '\\'])
            .to_ascii_lowercase();
        if lower.ends_with(".vst3") {
            Self::Vst3
        } else if lower.ends_with(".clap") {
            Self::Clap
        } else if lower.ends_with(".dll") || lower.ends_with(".vst") || lower.ends_with(".vst2") {
            Self::Vst2
        } else {
            Self::Vst3
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Vst3 => "VST3",
            Self::Vst2 => "VST2",
            Self::Clap => "CLAP",
        }
    }
}

/// Format dispatch for the shared runtime handle.
///
/// Every function takes the resolved [`PluginModuleFormat`] plus the opaque
/// instance pointer and forwards to the matching `sphere_daux_{vst3,vst2,clap}_*`
/// entry point. The branch is a single predictable compare on an enum stored
/// beside the pointer — no string compare, no map lookup, nothing that would
/// violate the realtime rules on the process path.
pub(crate) mod backend {
    use super::PluginModuleFormat;
    use crate::clap_processor::{ffi as clap, SphereDauxClapProcessor};
    use crate::vst2_processor::{ffi as vst2, SphereDauxVst2Processor};
    use crate::vst3_processor::{ffi as vst3, SphereDauxVst3Processor, Vst3MidiEvent};
    use std::os::raw::{c_char, c_double, c_float};

    /// Reinterpret the shared opaque handle as a VST2 instance. Sound because
    /// the pointer was produced by [`create`] with the same format tag, and
    /// every one of these C types is opaque.
    #[inline(always)]
    fn as_vst2(raw: *mut SphereDauxVst3Processor) -> *mut SphereDauxVst2Processor {
        raw.cast()
    }

    /// Same contract as [`as_vst2`], for the CLAP bridge.
    #[inline(always)]
    fn as_clap(raw: *mut SphereDauxVst3Processor) -> *mut SphereDauxClapProcessor {
        raw.cast()
    }

    /// Macro for the common shape: same argument list, differing only in which
    /// bridge module the call goes to. Both `ffi` modules expose the same short
    /// alias per operation, so the name is written once here.
    macro_rules! dispatch {
        ($(
            $(#[$meta:meta])*
            fn $name:ident ( $( $arg:ident : $ty:ty ),* $(,)? ) $( -> $ret:ty )? ;
        )*) => {
            $(
                $(#[$meta])*
                #[inline(always)]
                pub unsafe fn $name(
                    format: PluginModuleFormat,
                    raw: *mut SphereDauxVst3Processor,
                    $( $arg : $ty ),*
                ) $( -> $ret )? {
                    match format {
                        PluginModuleFormat::Vst3 => vst3::$name(raw, $( $arg ),*),
                        PluginModuleFormat::Vst2 => vst2::$name(as_vst2(raw), $( $arg ),*),
                        PluginModuleFormat::Clap => clap::$name(as_clap(raw), $( $arg ),*),
                    }
                }
            )*
        };
    }

    dispatch! {
        fn destroy();
        fn process_stereo_sample(
            in_l: c_float,
            in_r: c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
        ) -> i32;
        fn process_stereo_block_with_midi(
            in_l: *const c_float,
            in_r: *const c_float,
            out_l: *mut c_float,
            out_r: *mut c_float,
            frames: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        fn process_main_output_block_with_midi(
            in_l: *const c_float,
            in_r: *const c_float,
            out_interleaved: *mut c_float,
            frames: i32,
            output_channels: i32,
            events: *const Vst3MidiEvent,
            event_count: i32,
        ) -> i32;
        fn event_input_bus_count() -> i32;
        fn audio_input_bus_count() -> i32;
        fn audio_output_bus_count() -> i32;
        fn main_audio_input_channel_count() -> i32;
        fn main_audio_output_channel_count() -> i32;
        fn output_bus_channel_counts(out_counts: *mut i32, max_count: i32) -> i32;
        fn process_count() -> u64;
        fn last_input_peak() -> c_double;
        fn last_output_peak() -> c_double;
        fn last_difference_peak() -> c_double;
        fn set_param(param_id: u32, value: c_double);
        fn open_editor(
            window_id: *const c_char,
            title: *const c_char,
            width: i32,
            height: i32,
        ) -> u64;
        fn close_editor();
        fn focus_editor() -> i32;
        fn is_valid() -> i32;
        fn get_latency_samples() -> i32;
        #[allow(clippy::too_many_arguments)]
        fn set_process_context(
            tempo: c_double,
            time_sig_num: i32,
            time_sig_den: i32,
            project_time_samples: i64,
            ppq: c_double,
            bar_ppq: c_double,
            playing: i32,
            recording: i32,
        );
        // Host-owned view host. The caller owns the window; these drive only
        // the plug-in's editor, so all three formats reach the GPUI editor
        // window through the same calls.
        fn view_attach(
            parent_hwnd: u64,
            width: i32,
            height: i32,
            out_width: *mut i32,
            out_height: *mut i32,
        ) -> i32;
        fn view_detach();
        fn view_is_attached() -> i32;
        fn view_set_size(width: i32, height: i32) -> i32;
        fn view_get_size(out_width: *mut i32, out_height: *mut i32) -> i32;
        fn view_can_resize() -> i32;
        fn view_constrain(io_width: *mut i32, io_height: *mut i32) -> i32;
        fn view_take_resize_request(out_width: *mut i32, out_height: *mut i32) -> i32;
        fn embed_editor(parent_hwnd: u64, x: i32, y: i32, width: i32, height: i32) -> u64;
        fn embed_set_bounds(x: i32, y: i32, width: i32, height: i32);
        fn embed_refresh();
        fn embed_attach_hwnd() -> u64;
        fn embed_detach();
        fn embed_is_valid() -> i32;
        fn embed_has_visible_ui() -> i32;
        fn embed_host_kind() -> i32;
        fn embed_take_user_close() -> i32;
        fn embed_set_waiting_stage(stage: *const c_char);
        fn embed_content_size(out_width: *mut i32, out_height: *mut i32) -> i32;
        fn embed_set_instance_label(instance_id: *const c_char);
        fn set_editor_title(title: *const c_char);
        fn prepare_editor_view(out_width: *mut i32, out_height: *mut i32) -> i32;
        fn take_pending_shell_resize(out_width: *mut i32, out_height: *mut i32) -> i32;
        fn editor_resizable() -> i32;
        // `NSWindow*` of this instance's host-owned editor, as an opaque
        // handle, or 0 where there is none. Dispatched like everything else
        // because each bridge owns its own window; what it addresses — the
        // chrome strip — is shared, and lives in `editor_chrome` below.
        fn editor_native_window() -> u64;
        fn get_state(
            out_component: *mut *mut u8,
            out_component_len: *mut i32,
            out_controller: *mut *mut u8,
            out_controller_len: *mut i32,
        ) -> i32;
        fn set_state(
            component_data: *const u8,
            component_len: i32,
            controller_data: *const u8,
            controller_len: i32,
        ) -> i32;
        fn list_parameters_json() -> *mut c_char;
    }

    /// One editor idle tick. Only VST2 has one: its editor repaints and
    /// animates only while the host calls `effEditIdle`. VST3 and CLAP editors
    /// drive their own timers, so this is deliberately a no-op for them rather
    /// than a third C entry point that would do nothing.
    #[inline]
    pub unsafe fn view_idle(format: PluginModuleFormat, raw: *mut SphereDauxVst3Processor) {
        if format == PluginModuleFormat::Vst2 {
            vst2::view_idle(as_vst2(raw));
        }
    }

    // ── Entry points that take no instance pointer ───────────────────────────

    #[inline]
    pub fn bridge_probe(format: PluginModuleFormat) -> i32 {
        unsafe {
            match format {
                PluginModuleFormat::Vst3 => vst3::bridge_probe(),
                PluginModuleFormat::Vst2 => vst2::bridge_probe(),
                PluginModuleFormat::Clap => clap::bridge_probe(),
            }
        }
    }

    #[inline]
    pub unsafe fn last_error(format: PluginModuleFormat) -> *const c_char {
        match format {
            PluginModuleFormat::Vst3 => vst3::last_error(),
            PluginModuleFormat::Vst2 => vst2::last_error(),
            PluginModuleFormat::Clap => clap::last_error(),
        }
    }

    #[inline]
    pub unsafe fn create(
        format: PluginModuleFormat,
        plugin_path: *const c_char,
        class_id: *const c_char,
        sample_rate: c_double,
    ) -> *mut SphereDauxVst3Processor {
        match format {
            PluginModuleFormat::Vst3 => vst3::create(plugin_path, class_id, sample_rate),
            PluginModuleFormat::Vst2 => vst2::create(plugin_path, class_id, sample_rate).cast(),
            PluginModuleFormat::Clap => clap::create(plugin_path, class_id, sample_rate).cast(),
        }
    }

    // ── Allocator-paired frees ───────────────────────────────────────────────
    // Every bridge hands back `malloc`-owned buffers, but each must be released
    // through its own bridge so the pairing stays explicit if any side ever
    // changes allocator.

    #[inline]
    pub unsafe fn state_free(format: PluginModuleFormat, data: *mut u8) {
        match format {
            PluginModuleFormat::Vst3 => vst3::state_free(data),
            PluginModuleFormat::Vst2 => vst2::state_free(data),
            PluginModuleFormat::Clap => clap::state_free(data),
        }
    }

    #[inline]
    pub unsafe fn parameters_json_free(format: PluginModuleFormat, data: *mut c_char) {
        match format {
            PluginModuleFormat::Vst3 => vst3::parameters_json_free(data),
            PluginModuleFormat::Vst2 => vst2::parameters_json_free(data),
            PluginModuleFormat::Clap => clap::parameters_json_free(data),
        }
    }
}

/// The editor chrome strip: the tab strip and control row a host-owned editor
/// window carries above the plug-in's own view.
///
/// Deliberately outside [`backend`]'s format dispatch. The strip is drawn once,
/// for every format, and is addressed by the *window* it lives in rather than
/// by a processor — each bridge has a processor type of its own and none of
/// them can name the others', but every one of them can say which window its
/// editor is in ([`backend::editor_native_window`]).
///
/// A handle of 0 is not an error: it is how a platform with no host-owned
/// window, or an instance with no editor open, says so, and every call is then
/// a no-op. That is what lets the studio push chrome without asking first.
pub(crate) mod editor_chrome {
    use std::os::raw::c_char;

    extern "C" {
        #[link_name = "sphere_daux_editor_chrome_begin"]
        pub(crate) fn begin(native_window: u64);
        #[link_name = "sphere_daux_editor_chrome_set_header"]
        pub(crate) fn set_header(
            native_window: u64,
            active: i32,
            preset_label: *const c_char,
            cpu_label: *const c_char,
            latency_label: *const c_char,
            active_tab: *const c_char,
            shows_controls: i32,
        );
        #[link_name = "sphere_daux_editor_chrome_add_preset"]
        pub(crate) fn add_preset(native_window: u64, name: *const c_char, selected: i32);
        #[link_name = "sphere_daux_editor_chrome_add_tab"]
        pub(crate) fn add_tab(
            native_window: u64,
            insert_id: *const c_char,
            display_name: *const c_char,
            insert_number: i32,
        );
        #[link_name = "sphere_daux_editor_chrome_set_palette"]
        pub(crate) fn set_palette(native_window: u64, colors: *const u32, count: i32);
        #[link_name = "sphere_daux_editor_chrome_commit"]
        pub(crate) fn commit(native_window: u64, window_title: *const c_char);
        #[link_name = "sphere_daux_editor_chrome_take_action"]
        pub(crate) fn take_action(
            native_window: u64,
            out_kind: *mut i32,
            out_value: *mut i32,
            out_id: *mut c_char,
            out_id_capacity: i32,
        ) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::PluginModuleFormat;

    #[test]
    fn detects_format_from_module_path() {
        assert_eq!(
            PluginModuleFormat::detect(r"C:\Program Files\Common Files\VST3\Thing.vst3"),
            PluginModuleFormat::Vst3
        );
        assert_eq!(
            PluginModuleFormat::detect(r"C:\Program Files\VSTPlugins\Thing.dll"),
            PluginModuleFormat::Vst2
        );
        assert_eq!(
            PluginModuleFormat::detect("/Library/Audio/Plug-Ins/VST/Thing.vst"),
            PluginModuleFormat::Vst2
        );
        // Trailing separators appear on bundle paths taken from directory walks.
        assert_eq!(
            PluginModuleFormat::detect("/Library/Audio/Plug-Ins/VST3/Thing.vst3/"),
            PluginModuleFormat::Vst3
        );
        // Unknown extensions stay on the long-standing VST3 path rather than
        // sending an existing project's insert to a different bridge.
        assert_eq!(
            PluginModuleFormat::detect("/opt/plugins/Thing"),
            PluginModuleFormat::Vst3
        );
    }

    #[test]
    fn parses_explicit_format_labels() {
        assert_eq!(
            PluginModuleFormat::from_label("vst2"),
            Some(PluginModuleFormat::Vst2)
        );
        assert_eq!(
            PluginModuleFormat::from_label("VST3"),
            Some(PluginModuleFormat::Vst3)
        );
        assert_eq!(
            PluginModuleFormat::from_label("clap"),
            Some(PluginModuleFormat::Clap)
        );
        // Not module formats: an Audio Unit is addressed by component id and a
        // built-in has no module at all, so both must stay unroutable here
        // rather than fall through to a bridge that cannot load them.
        assert_eq!(PluginModuleFormat::from_label("AU"), None);
        assert_eq!(PluginModuleFormat::from_label("BuiltIn"), None);
    }

    #[test]
    fn detects_clap_from_module_path() {
        assert_eq!(
            PluginModuleFormat::detect(r"C:\Program Files\Common Files\CLAP\Thing.clap"),
            PluginModuleFormat::Clap
        );
        assert_eq!(
            PluginModuleFormat::detect("/Library/Audio/Plug-Ins/CLAP/Thing.clap/"),
            PluginModuleFormat::Clap
        );
    }
}
