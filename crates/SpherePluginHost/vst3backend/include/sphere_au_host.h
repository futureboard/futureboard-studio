#pragma once

// Audio Unit runtime for the plug-in host process (macOS only).
//
// Shape mirrors the built-in DSP contract the host's block path already speaks
// (`process_block(in_l, in_r, interleaved_out, frames) -> channels`) rather than
// the VST3 bridge, because an Audio Unit is hosted entirely inside this process:
// there is no module path, no separate controller, and no in-process engine
// path. Identity is the scanner's component id, `au:<type>:<subtype>:<manuf>`
// (see `au_scanner.mm`), which round-trips to an AudioComponentDescription.
//
// Threading contract, matching `BuiltinHostProcessor`:
//   * open / close / state / parameter enumeration: control thread (IPC).
//   * render / parameter values / MIDI: the single audio producer thread.
// The Rust wrapper serializes control calls against render with one mutex, the
// same way the VST3 voice mutex serializes `setState` against `process`.

#include <stddef.h>

#ifdef _WIN32
#  define SPHERE_AU_HOST_API __declspec(dllexport)
#else
#  define SPHERE_AU_HOST_API __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

/// Opaque per-instance handle. One live Audio Unit plus its preallocated render
/// buffers; never shared between instance ids.
typedef struct SphereAuInstance SphereAuInstance;

/// Transport the engine published for this block, fed to the Audio Unit's host
/// callbacks (beat/tempo, musical time, transport state) so tempo-synced units
/// follow the project instead of a hardcoded default.
typedef struct SphereAuTransport {
  double tempo_bpm;
  double ppq_position;
  double bar_position_ppq;
  long long project_time_samples;
  unsigned int time_sig_num;
  unsigned int time_sig_den;
  int playing;
  int recording;
} SphereAuTransport;

/// One global-scope parameter, already translated into the host's normalized
/// 0..1 world (`normalized_default`); plain-value range stays inside the
/// instance so the block path never converts twice.
typedef struct SphereAuParameterInfo {
  unsigned int id;
  char name[64];
  char unit[32];
  float normalized_default;
  int automatable;
  int read_only;
  int hidden;
} SphereAuParameterInfo;

/// Instantiate and initialize `component_id` at `sample_rate`, sized for
/// `max_block_frames`. Returns NULL on failure and writes a human-readable
/// reason into `error` (may be NULL). Control thread only.
SPHERE_AU_HOST_API SphereAuInstance* sphere_au_open(
    const char* component_id,
    double sample_rate,
    unsigned int max_block_frames,
    char* error,
    size_t error_len);

/// Uninitialize, dispose, and free. Safe with NULL. Control thread only.
SPHERE_AU_HOST_API void sphere_au_close(SphereAuInstance* instance);

/// Channel counts the unit's negotiated stream format actually uses.
SPHERE_AU_HOST_API unsigned int sphere_au_output_channels(const SphereAuInstance* instance);
SPHERE_AU_HOST_API unsigned int sphere_au_input_channels(const SphereAuInstance* instance);

/// True when the component is a MusicDevice/Generator (no audio input bus) or a
/// MusicEffect — i.e. it expects MIDI.
SPHERE_AU_HOST_API int sphere_au_accepts_midi(const SphereAuInstance* instance);
SPHERE_AU_HOST_API int sphere_au_is_instrument(const SphereAuInstance* instance);

/// Reported latency in samples at the open sample rate, or 0 when the unit
/// reports none.
SPHERE_AU_HOST_API unsigned int sphere_au_latency_samples(const SphereAuInstance* instance);

/// Render one block. `in_l`/`in_r` are deinterleaved host input (ignored by
/// instruments), `out_interleaved` must hold `frames * out_channels` floats.
/// Returns the number of channels actually written, or 0 on render failure.
/// Producer thread only; allocation-free.
SPHERE_AU_HOST_API unsigned int sphere_au_render(
    SphereAuInstance* instance,
    const float* in_l,
    const float* in_r,
    unsigned int frames,
    float* out_interleaved,
    unsigned int out_channels,
    const SphereAuTransport* transport);

/// Apply a normalized 0..1 automation value, denormalized through the
/// parameter's own min/max (and rounded for indexed/boolean parameters).
/// Unknown ids are ignored. Producer thread; allocation-free.
SPHERE_AU_HOST_API void sphere_au_set_parameter_normalized(
    SphereAuInstance* instance,
    unsigned int param_id,
    float normalized);

/// Deliver one MIDI message. `offset_frames` is the sample offset inside the
/// current block. No-op for units that do not accept MIDI. Producer thread.
SPHERE_AU_HOST_API void sphere_au_send_midi(
    SphereAuInstance* instance,
    unsigned char status,
    unsigned char data1,
    unsigned char data2,
    unsigned int offset_frames);

/// Flush tails and voices (`AudioUnitReset`). Producer or control thread while
/// no render is in flight.
SPHERE_AU_HOST_API void sphere_au_reset(SphereAuInstance* instance);

/// Parameter enumeration for the plug-in inspector. Control thread.
SPHERE_AU_HOST_API unsigned int sphere_au_parameter_count(const SphereAuInstance* instance);
SPHERE_AU_HOST_API int sphere_au_parameter_info(
    const SphereAuInstance* instance,
    unsigned int index,
    SphereAuParameterInfo* out_info);

/// Opaque state: the unit's `kAudioUnitProperty_ClassInfo` property list
/// serialized as a binary plist. Copies at most `capacity` bytes into `out` and
/// returns the full byte length, so a zero-capacity call sizes the buffer.
/// Returns 0 when the unit has no state. Control thread.
SPHERE_AU_HOST_API size_t sphere_au_get_state(
    const SphereAuInstance* instance,
    unsigned char* out,
    size_t capacity);

/// Restore bytes produced by `sphere_au_get_state`. Returns non-zero on
/// success. Control thread, serialized against render by the caller.
SPHERE_AU_HOST_API int sphere_au_set_state(
    SphereAuInstance* instance,
    const unsigned char* data,
    size_t len);

/// Open the Audio Unit's Cocoa editor in a host-owned top-level NSWindow.
/// `out_width`/`out_height` receive the actual view size. Returns a non-zero
/// opaque window handle on success. Control/main thread only on macOS.
SPHERE_AU_HOST_API unsigned long long sphere_au_open_editor(
    SphereAuInstance* instance,
    const char* title,
    unsigned int preferred_width,
    unsigned int preferred_height,
    unsigned int* out_width,
    unsigned int* out_height);

/// Close only the Cocoa editor; the Audio Unit DSP instance remains alive.
/// The `NSWindow*` of this unit's editor, as an opaque handle; 0 when none is
/// open. Addresses the shared editor chrome strip
/// (`sphere_daux_editor_chrome.h`), which is keyed by window so one strip
/// serves every plug-in format.
SPHERE_AU_HOST_API unsigned long long
sphere_au_editor_native_window(SphereAuInstance* instance);

SPHERE_AU_HOST_API void sphere_au_close_editor(SphereAuInstance* instance);

/// Bring an already-open Cocoa editor to the front.
SPHERE_AU_HOST_API int sphere_au_focus_editor(SphereAuInstance* instance);

/// Returns non-zero once when the user closes the Cocoa editor window.
SPHERE_AU_HOST_API int sphere_au_take_editor_user_close(SphereAuInstance* instance);

#ifdef __cplusplus
}  // extern "C"
#endif
