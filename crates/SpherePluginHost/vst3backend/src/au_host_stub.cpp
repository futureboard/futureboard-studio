// Non-macOS stub for the Audio Unit runtime, so the host's block path and IPC
// dispatch can name AU unconditionally instead of carrying a `cfg` at every
// branch. `sphere_au_open` reports the platform truth and everything else is
// unreachable, because no instance can exist without it.
//
// Mirrors the `au_scanner_stub.cpp` convention.

#include "sphere_au_host.h"

#include <cstring>

extern "C" {

SPHERE_AU_HOST_API SphereAuInstance* sphere_au_open(
    const char* /*component_id*/,
    double /*sample_rate*/,
    unsigned int /*max_block_frames*/,
    char* error,
    size_t error_len) {
  static const char message[] = "Audio Unit hosting requires macOS";
  if (error != nullptr && error_len > 0) {
    const size_t copy =
        (sizeof(message) - 1) < (error_len - 1) ? (sizeof(message) - 1) : (error_len - 1);
    std::memcpy(error, message, copy);
    error[copy] = '\0';
  }
  return nullptr;
}

SPHERE_AU_HOST_API void sphere_au_close(SphereAuInstance* /*instance*/) {}

SPHERE_AU_HOST_API unsigned int sphere_au_output_channels(const SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API unsigned int sphere_au_input_channels(const SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API int sphere_au_accepts_midi(const SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API int sphere_au_is_instrument(const SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API unsigned int sphere_au_latency_samples(const SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API unsigned int sphere_au_render(
    SphereAuInstance* /*instance*/,
    const float* /*in_l*/,
    const float* /*in_r*/,
    unsigned int /*frames*/,
    float* /*out_interleaved*/,
    unsigned int /*out_channels*/,
    const SphereAuTransport* /*transport*/) {
  return 0;
}

SPHERE_AU_HOST_API void sphere_au_set_parameter_normalized(
    SphereAuInstance* /*instance*/,
    unsigned int /*param_id*/,
    float /*normalized*/) {}

SPHERE_AU_HOST_API void sphere_au_send_midi(
    SphereAuInstance* /*instance*/,
    unsigned char /*status*/,
    unsigned char /*data1*/,
    unsigned char /*data2*/,
    unsigned int /*offset_frames*/) {}

SPHERE_AU_HOST_API void sphere_au_reset(SphereAuInstance* /*instance*/) {}

SPHERE_AU_HOST_API unsigned int sphere_au_parameter_count(const SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API int sphere_au_parameter_info(
    const SphereAuInstance* /*instance*/,
    unsigned int /*index*/,
    SphereAuParameterInfo* /*out_info*/) {
  return 0;
}

SPHERE_AU_HOST_API size_t sphere_au_get_state(
    const SphereAuInstance* /*instance*/,
    unsigned char* /*out*/,
    size_t /*capacity*/) {
  return 0;
}

SPHERE_AU_HOST_API int sphere_au_set_state(
    SphereAuInstance* /*instance*/,
    const unsigned char* /*data*/,
    size_t /*len*/) {
  return 0;
}

SPHERE_AU_HOST_API unsigned long long sphere_au_open_editor(
    SphereAuInstance* /*instance*/,
    const char* /*title*/,
    unsigned int /*preferred_width*/,
    unsigned int /*preferred_height*/,
    unsigned int* out_width,
    unsigned int* out_height) {
  if (out_width != nullptr) {
    *out_width = 0;
  }
  if (out_height != nullptr) {
    *out_height = 0;
  }
  return 0;
}

SPHERE_AU_HOST_API unsigned long long
sphere_au_editor_native_window(SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API void sphere_au_close_editor(SphereAuInstance* /*instance*/) {}

SPHERE_AU_HOST_API int sphere_au_focus_editor(SphereAuInstance* /*instance*/) {
  return 0;
}

SPHERE_AU_HOST_API int sphere_au_take_editor_user_close(SphereAuInstance* /*instance*/) {
  return 0;
}

}  // extern "C"
