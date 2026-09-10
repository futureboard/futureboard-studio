//! Audio Unit runtime (macOS), hosted inside the plug-in host process.
//!
//! An Audio Unit has no module path and no separate controller, so it is
//! modeled on the built-in DSP contract rather than the VST3 bridge: identity is
//! the scanner's `au:<type>:<subtype>:<manufacturer>` component id, blocks are
//! deinterleaved stereo in and interleaved out, and automation arrives as
//! normalized 0..1 values that the native layer denormalizes through each
//! parameter's own min/max.
//!
//! Thread contract: [`AuHostProcessor::render`] runs on the audio producer
//! thread; state, parameter enumeration, and reset run on the IPC thread. Both
//! go through one mutex, which is the same way the VST3 voice mutex serializes
//! `setState` against `process` (see `plugin_host_preview::set_instance_state`).
//! Loads are rare and short; the block path never contends in steady state.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uchar, c_uint};

use parking_lot::Mutex;

use crate::audio_bridge::SharedMidiEvent;

/// Transport the engine published for the current block, handed to the unit's
/// host callbacks. Mirrors `SphereAuTransport` in `sphere_au_host.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AuTransport {
    pub tempo_bpm: f64,
    pub ppq_position: f64,
    pub bar_position_ppq: f64,
    pub project_time_samples: i64,
    pub time_sig_num: c_uint,
    pub time_sig_den: c_uint,
    pub playing: c_int,
    pub recording: c_int,
}

/// Mirrors `SphereAuParameterInfo` in `sphere_au_host.h`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AuParameterInfoRaw {
    id: c_uint,
    name: [c_char; 64],
    unit: [c_char; 32],
    normalized_default: f32,
    automatable: c_int,
    read_only: c_int,
    hidden: c_int,
}

/// One global-scope Audio Unit parameter, in the host's normalized world.
#[derive(Clone, Debug)]
pub struct AuParameterDescriptor {
    pub id: u32,
    pub title: String,
    pub unit: String,
    pub normalized_default: f32,
    pub automatable: bool,
    pub read_only: bool,
    pub hidden: bool,
}

#[repr(C)]
struct SphereAuInstance {
    _opaque: [u8; 0],
}

extern "C" {
    fn sphere_au_open(
        component_id: *const c_char,
        sample_rate: f64,
        max_block_frames: c_uint,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut SphereAuInstance;
    fn sphere_au_close(instance: *mut SphereAuInstance);
    fn sphere_au_output_channels(instance: *const SphereAuInstance) -> c_uint;
    fn sphere_au_input_channels(instance: *const SphereAuInstance) -> c_uint;
    fn sphere_au_accepts_midi(instance: *const SphereAuInstance) -> c_int;
    fn sphere_au_is_instrument(instance: *const SphereAuInstance) -> c_int;
    fn sphere_au_latency_samples(instance: *const SphereAuInstance) -> c_uint;
    fn sphere_au_render(
        instance: *mut SphereAuInstance,
        in_l: *const f32,
        in_r: *const f32,
        frames: c_uint,
        out_interleaved: *mut f32,
        out_channels: c_uint,
        transport: *const AuTransport,
    ) -> c_uint;
    fn sphere_au_set_parameter_normalized(
        instance: *mut SphereAuInstance,
        param_id: c_uint,
        normalized: f32,
    );
    fn sphere_au_send_midi(
        instance: *mut SphereAuInstance,
        status: c_uchar,
        data1: c_uchar,
        data2: c_uchar,
        offset_frames: c_uint,
    );
    fn sphere_au_reset(instance: *mut SphereAuInstance);
    fn sphere_au_parameter_count(instance: *const SphereAuInstance) -> c_uint;
    fn sphere_au_parameter_info(
        instance: *const SphereAuInstance,
        index: c_uint,
        out_info: *mut AuParameterInfoRaw,
    ) -> c_int;
    fn sphere_au_get_state(
        instance: *const SphereAuInstance,
        out: *mut c_uchar,
        capacity: usize,
    ) -> usize;
    fn sphere_au_set_state(
        instance: *mut SphereAuInstance,
        data: *const c_uchar,
        len: usize,
    ) -> c_int;
    fn sphere_au_open_editor(
        instance: *mut SphereAuInstance,
        title: *const c_char,
        preferred_width: c_uint,
        preferred_height: c_uint,
        out_width: *mut c_uint,
        out_height: *mut c_uint,
    ) -> u64;
    fn sphere_au_editor_native_window(instance: *mut SphereAuInstance) -> u64;
    fn sphere_au_close_editor(instance: *mut SphereAuInstance);
    fn sphere_au_focus_editor(instance: *mut SphereAuInstance) -> c_int;
    fn sphere_au_take_editor_user_close(instance: *mut SphereAuInstance) -> c_int;
}

/// Owns the native instance pointer and closes it exactly once.
struct AuInstancePtr(*mut SphereAuInstance);

// SAFETY: render/state/parameter calls go through the owning `Mutex`. Cocoa
// editor calls only touch the native instance's editor fields and intentionally
// run without holding that render mutex: real AU hosts create and manage views
// on the main thread while the Audio Unit continues rendering. `&self` keeps
// the native instance alive for the full duration of every such call.
unsafe impl Send for AuInstancePtr {}
unsafe impl Sync for AuInstancePtr {}

impl Drop for AuInstancePtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sphere_au_close(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

/// A loaded Audio Unit plus the facts the host's block path needs without
/// touching the instance (channel count, MIDI capability, identity).
pub struct AuHostProcessor {
    instance: Mutex<AuInstancePtr>,
    component_id: String,
    output_channels: u32,
    input_channels: u32,
    accepts_midi: bool,
    is_instrument: bool,
    parameters: Vec<AuParameterDescriptor>,
}

impl std::fmt::Debug for AuHostProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuHostProcessor")
            .field("component_id", &self.component_id)
            .field("output_channels", &self.output_channels)
            .field("input_channels", &self.input_channels)
            .field("accepts_midi", &self.accepts_midi)
            .field("is_instrument", &self.is_instrument)
            .field("parameters", &self.parameters.len())
            .finish()
    }
}

impl AuHostProcessor {
    /// Snapshot the stable native pointer, releasing the render mutex before a
    /// potentially slow Cocoa plug-in view factory runs. Holding this mutex
    /// while UADx constructs its view can otherwise stall audio for >100 ms.
    fn editor_instance(&self) -> *mut SphereAuInstance {
        self.instance.lock().0
    }

    /// Instantiate and initialize `component_id`, then apply `state` (a binary
    /// plist from [`Self::state`]) before the processor is published to the
    /// audio producer — the only window where touching it from this thread is
    /// race-free by construction, matching the built-in load path.
    pub fn open(
        component_id: &str,
        sample_rate: u32,
        max_block_frames: u32,
        state: Option<&[u8]>,
    ) -> Result<Self, String> {
        let id = CString::new(component_id)
            .map_err(|_| format!("component id contains a NUL byte: {component_id}"))?;
        let mut error = [0 as c_char; 256];
        let raw = unsafe {
            sphere_au_open(
                id.as_ptr(),
                f64::from(sample_rate.max(1)),
                max_block_frames.max(1),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if raw.is_null() {
            return Err(cstr_to_string(&error)
                .unwrap_or_else(|| format!("failed to open audio unit {component_id}")));
        }
        let instance = AuInstancePtr(raw);

        let output_channels = unsafe { sphere_au_output_channels(raw) }.max(1);
        let input_channels = unsafe { sphere_au_input_channels(raw) };
        let accepts_midi = unsafe { sphere_au_accepts_midi(raw) } != 0;
        let is_instrument = unsafe { sphere_au_is_instrument(raw) } != 0;
        let parameters = read_parameters(raw);

        if let Some(bytes) = state.filter(|bytes| !bytes.is_empty()) {
            let restored = unsafe { sphere_au_set_state(raw, bytes.as_ptr(), bytes.len()) } != 0;
            eprintln!(
                "[plugin-host-au] restore state component={component_id} bytes={} ok={restored}",
                bytes.len()
            );
        }

        Ok(Self {
            instance: Mutex::new(instance),
            component_id: component_id.to_string(),
            output_channels,
            input_channels,
            accepts_midi,
            is_instrument,
            parameters,
        })
    }

    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    /// Channels the unit's negotiated output format carries. Read without the
    /// lock: fixed at open and never renegotiated afterwards.
    pub fn output_channels(&self) -> u32 {
        self.output_channels
    }

    pub fn input_channels(&self) -> u32 {
        self.input_channels
    }

    pub fn accepts_midi(&self) -> bool {
        self.accepts_midi
    }

    pub fn is_instrument(&self) -> bool {
        self.is_instrument
    }

    pub fn parameters(&self) -> &[AuParameterDescriptor] {
        &self.parameters
    }

    /// Render one block. Returns the channels written, or 0 when the unit's
    /// render failed — the caller decides what to do with a failed block rather
    /// than having silence forced on it here.
    pub fn render(
        &self,
        in_l: &[f32],
        in_r: &[f32],
        out_interleaved: &mut [f32],
        frames: usize,
        out_channels: usize,
        transport: AuTransport,
    ) -> u32 {
        if frames == 0 || out_channels == 0 {
            return 0;
        }
        let frames = frames.min(in_l.len()).min(in_r.len());
        if frames == 0 || out_interleaved.len() < frames * out_channels {
            return 0;
        }
        let instance = self.instance.lock();
        unsafe {
            sphere_au_render(
                instance.0,
                in_l.as_ptr(),
                in_r.as_ptr(),
                frames as c_uint,
                out_interleaved.as_mut_ptr(),
                out_channels as c_uint,
                &transport,
            )
        }
    }

    /// Queue one MIDI message for the block being rendered, preserving its
    /// sample offset (Audio Units schedule events within the slice).
    pub fn apply_midi(&self, event: SharedMidiEvent) {
        if !self.accepts_midi {
            return;
        }
        let instance = self.instance.lock();
        unsafe {
            sphere_au_send_midi(
                instance.0,
                event.status,
                event.data1,
                event.data2,
                event.sample_offset,
            )
        };
    }

    /// Apply a normalized 0..1 automation value; the native layer denormalizes
    /// through the parameter's plain min/max.
    pub fn apply_param(&self, param_id: u32, value: f32) {
        let instance = self.instance.lock();
        unsafe { sphere_au_set_parameter_normalized(instance.0, param_id, value) };
    }

    /// All-notes-off plus all-sound-off on every channel, for unload and detach.
    pub fn midi_panic(&self) {
        if !self.accepts_midi {
            return;
        }
        let instance = self.instance.lock();
        for channel in 0..16u8 {
            let status = 0xB0 | channel;
            unsafe {
                sphere_au_send_midi(instance.0, status, 120, 0, 0);
                sphere_au_send_midi(instance.0, status, 123, 0, 0);
            }
        }
    }

    pub fn latency_samples(&self) -> u32 {
        let instance = self.instance.lock();
        unsafe { sphere_au_latency_samples(instance.0) }
    }

    pub fn reset(&self) {
        let instance = self.instance.lock();
        unsafe { sphere_au_reset(instance.0) };
    }

    /// Opaque state bytes (a binary plist of the unit's ClassInfo), for project
    /// persistence. `None` when the unit reports no state.
    pub fn state(&self) -> Option<Vec<u8>> {
        let instance = self.instance.lock();
        let len = unsafe { sphere_au_get_state(instance.0, std::ptr::null_mut(), 0) };
        if len == 0 {
            return None;
        }
        let mut bytes = vec![0u8; len];
        let written = unsafe { sphere_au_get_state(instance.0, bytes.as_mut_ptr(), bytes.len()) };
        if written == 0 {
            return None;
        }
        bytes.truncate(written.min(len));
        Some(bytes)
    }

    pub fn set_state(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        let instance = self.instance.lock();
        unsafe { sphere_au_set_state(instance.0, bytes.as_ptr(), bytes.len()) != 0 }
    }

    /// Open the unit's custom Cocoa view in the plugin-host process. The
    /// returned handle identifies the host-owned NSWindow; the dimensions are
    /// the actual custom view bounds reported by the unit.
    pub fn open_editor(
        &self,
        title: &str,
        preferred_width: u32,
        preferred_height: u32,
    ) -> Option<(u64, u32, u32)> {
        let title = CString::new(title).ok()?;
        let instance = self.editor_instance();
        let mut width = 0;
        let mut height = 0;
        let handle = unsafe {
            sphere_au_open_editor(
                instance,
                title.as_ptr(),
                preferred_width,
                preferred_height,
                &mut width,
                &mut height,
            )
        };
        (handle != 0).then_some((handle, width.max(1), height.max(1)))
    }

    /// This unit's editor chrome strip, when its editor is open.
    ///
    /// The strip is the studio's — same tab strip and control row a VST3, VST2
    /// or CLAP editor gets — and is addressed by the window it lives in, which
    /// is the one piece every format has in common. `None` when no editor is
    /// open, and on any platform that has no host-owned editor window.
    pub fn editor_chrome(&self) -> Option<DirectAudio::editor_chrome::EditorChrome> {
        let instance = self.editor_instance();
        // SAFETY: `instance` is this processor's live unit for as long as it is
        // alive; the call only reads a stored pointer.
        let window = unsafe { sphere_au_editor_native_window(instance) };
        DirectAudio::editor_chrome::EditorChrome::for_window(window)
    }

    pub fn close_editor(&self) {
        let instance = self.editor_instance();
        unsafe { sphere_au_close_editor(instance) };
    }

    pub fn focus_editor(&self) -> bool {
        let instance = self.editor_instance();
        unsafe { sphere_au_focus_editor(instance) != 0 }
    }

    pub fn take_editor_user_close(&self) -> bool {
        let Some(instance) = self.instance.try_lock() else {
            return false;
        };
        let instance = instance.0;
        unsafe { sphere_au_take_editor_user_close(instance) != 0 }
    }
}

fn read_parameters(raw: *const SphereAuInstance) -> Vec<AuParameterDescriptor> {
    let count = unsafe { sphere_au_parameter_count(raw) } as usize;
    let mut parameters = Vec::with_capacity(count);
    for index in 0..count {
        let mut info = AuParameterInfoRaw {
            id: 0,
            name: [0; 64],
            unit: [0; 32],
            normalized_default: 0.0,
            automatable: 0,
            read_only: 0,
            hidden: 0,
        };
        if unsafe { sphere_au_parameter_info(raw, index as c_uint, &mut info) } == 0 {
            continue;
        }
        parameters.push(AuParameterDescriptor {
            id: info.id,
            title: cstr_to_string(&info.name).unwrap_or_else(|| format!("Param {}", info.id)),
            unit: cstr_to_string(&info.unit).unwrap_or_default(),
            normalized_default: info.normalized_default,
            automatable: info.automatable != 0,
            read_only: info.read_only != 0,
            hidden: info.hidden != 0,
        });
    }
    parameters
}

/// Read a NUL-terminated fixed C buffer, returning `None` when it is empty.
fn cstr_to_string(buffer: &[c_char]) -> Option<String> {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The native layer is only real on macOS; elsewhere `open` reports that, so
/// these tests describe macOS behavior only.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_component_id_fails_with_a_reason() {
        let error = AuHostProcessor::open("not-an-au-id", 48_000, 512, None)
            .expect_err("a malformed id must not open");
        assert!(
            error.contains("component id"),
            "error should name the malformed id, got {error}"
        );
    }

    #[test]
    fn a_missing_component_fails_without_panicking() {
        // Well-formed id, no such component installed: the failure must come
        // from the lookup rather than from parsing.
        let error = AuHostProcessor::open("au:6f6f6f6f:6f6f6f6f:6f6f6f6f", 48_000, 512, None)
            .expect_err("an unknown component must not open");
        assert!(
            error.contains("no installed component"),
            "expected a lookup failure, got {error}"
        );
    }

    /// Apple's stock delay (`aufx`/`dely`/`appl`) ships with every macOS install,
    /// which makes it the one component these tests can rely on.
    const APPLE_DELAY: &str = "au:61756678:64656c79:6170706c";

    fn open_stock_delay() -> Option<AuHostProcessor> {
        match AuHostProcessor::open(APPLE_DELAY, 48_000, BLOCK, None) {
            Ok(au) => Some(au),
            Err(error) => {
                eprintln!("skipping: Apple AUDelay unavailable ({error})");
                None
            }
        }
    }

    const BLOCK: u32 = 512;

    #[test]
    fn a_stock_effect_opens_and_reports_a_usable_format() {
        let Some(au) = open_stock_delay() else { return };
        assert!(
            au.output_channels() >= 1,
            "an effect must report at least one output channel"
        );
        assert!(!au.is_instrument(), "AUDelay is an effect");
        assert!(
            !au.parameters().is_empty(),
            "AUDelay exposes global parameters"
        );
    }

    #[test]
    fn a_stock_effect_renders_the_input_it_was_given() {
        let Some(au) = open_stock_delay() else { return };
        let frames = BLOCK as usize;
        let channels = au.output_channels() as usize;

        // A full-scale tone, so a passthrough-or-better result is unmistakable
        // against a silent buffer.
        let tone: Vec<f32> = (0..frames)
            .map(|frame| (frame as f32 * 0.05).sin() * 0.5)
            .collect();
        let mut out = vec![0.0f32; frames * channels];
        let written = au.render(
            &tone,
            &tone,
            &mut out,
            frames,
            channels,
            AuTransport {
                tempo_bpm: 120.0,
                time_sig_num: 4,
                time_sig_den: 4,
                ..AuTransport::default()
            },
        );

        assert!(written >= 1, "render reported no channels");
        let peak = out.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!(peak > 0.01, "rendered block was silent (peak {peak})");
    }

    #[test]
    fn state_round_trips_as_opaque_bytes() {
        let Some(au) = open_stock_delay() else { return };
        let state = au.state().expect("AUDelay reports ClassInfo state");
        assert!(state.len() > 8, "state blob is implausibly small");
        assert!(
            au.set_state(&state),
            "the unit must accept the bytes it just produced"
        );
    }

    #[test]
    fn automation_applies_by_parameter_id_and_ignores_unknown_ids() {
        let Some(au) = open_stock_delay() else { return };
        let parameter = au
            .parameters()
            .iter()
            .find(|parameter| parameter.automatable)
            .cloned()
            .expect("AUDelay exposes an automatable parameter");

        // Moving a real parameter must change the unit's serialized state,
        // which is the only observable both AU and this test agree on.
        let before = au.state().expect("state before automation");
        let target = if parameter.normalized_default > 0.5 {
            0.0
        } else {
            1.0
        };
        au.apply_param(parameter.id, target);
        let after = au.state().expect("state after automation");
        assert_ne!(
            before, after,
            "parameter {} ({}) did not reach the unit",
            parameter.id, parameter.title
        );

        au.apply_param(0xDEAD_BEEF, 0.5);
    }
}
