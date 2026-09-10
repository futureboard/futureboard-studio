// Editor stub for platforms where VST2 hosting is not supported (Linux).
//
// `sphere_daux_vst2_create` already fails on these platforms, so these entry
// points exist only so the shared C surface links. They report "no editor"
// rather than pretending an editor opened.

#if defined(_WIN32) || defined(__APPLE__)
#error "vst2_editor_stub.cpp must not be compiled on Windows or macOS"
#endif

#include "vst2_processor_internal.hpp"

extern "C" {

unsigned long long sphere_daux_vst2_embed_editor(SphereDauxVst2Processor *,
                                                 unsigned long long, int, int,
                                                 int, int) {
  vst2_set_last_error("VST2 editors are not supported on this platform");
  return 0;
}

void sphere_daux_vst2_embed_set_bounds(SphereDauxVst2Processor *, int, int, int,
                                       int) {}

void sphere_daux_vst2_embed_refresh(SphereDauxVst2Processor *) {}

unsigned long long sphere_daux_vst2_embed_attach_hwnd(
    SphereDauxVst2Processor *) {
  return 0;
}

void sphere_daux_vst2_embed_detach(SphereDauxVst2Processor *) {}

int sphere_daux_vst2_embed_is_valid(SphereDauxVst2Processor *) { return 0; }

int sphere_daux_vst2_embed_has_visible_ui(SphereDauxVst2Processor *) {
  return 0;
}

unsigned long long sphere_daux_vst2_open_editor(SphereDauxVst2Processor *,
                                                const char *, const char *, int,
                                                int) {
  vst2_set_last_error("VST2 editors are not supported on this platform");
  return 0;
}

void sphere_daux_vst2_close_editor(SphereDauxVst2Processor *) {}

int sphere_daux_vst2_focus_editor(SphereDauxVst2Processor *) { return 0; }

// ── Host-owned view host ────────────────────────────────────────────────────

int sphere_daux_vst2_view_attach(SphereDauxVst2Processor *, unsigned long long,
                                 int, int, int *, int *) {
  vst2_set_last_error("VST2 editors are not supported on this platform");
  return 0;
}

void sphere_daux_vst2_view_detach(SphereDauxVst2Processor *) {}

int sphere_daux_vst2_view_is_attached(SphereDauxVst2Processor *) { return 0; }

int sphere_daux_vst2_view_set_size(SphereDauxVst2Processor *, int, int) {
  return 0;
}

int sphere_daux_vst2_view_get_size(SphereDauxVst2Processor *, int *, int *) {
  return 0;
}

int sphere_daux_vst2_view_can_resize(SphereDauxVst2Processor *) { return 0; }

int sphere_daux_vst2_view_constrain(SphereDauxVst2Processor *, int *, int *) {
  return 0;
}

int sphere_daux_vst2_view_take_resize_request(SphereDauxVst2Processor *, int *,
                                              int *) {
  return 0;
}

void sphere_daux_vst2_view_idle(SphereDauxVst2Processor *) {}


/// No host-owned editor window on this platform, so nothing for the chrome to
/// attach to — and a 0 handle is exactly how the chrome ABI says so.
unsigned long long
sphere_daux_vst2_editor_native_window(SphereDauxVst2Processor *) {
  return 0;
}

} // extern "C"
