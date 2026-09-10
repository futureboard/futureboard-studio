// Editor stub for platforms with no CLAP GUI host (Linux).
//
// Unlike VST2, CLAP *processing* works here — the core loads modules through
// dlopen and runs them normally. Only the embedded editor is missing, because
// hosting an X11 CLAP GUI needs the same XEmbed plumbing the VST3 bridge has
// and that is not wired up yet. These entry points report "no editor" rather
// than pretending a window opened; the generic parameter view still works.

#if defined(_WIN32) || defined(__APPLE__)
#error "clap_editor_stub.cpp must not be compiled on Windows or macOS"
#endif

#include "clap_processor_internal.hpp"

extern "C" {

unsigned long long sphere_daux_clap_embed_editor(SphereDauxClapProcessor *,
                                                 unsigned long long, int, int,
                                                 int, int) {
  clap_set_last_error("CLAP editors are not supported on this platform");
  return 0;
}

void sphere_daux_clap_embed_set_bounds(SphereDauxClapProcessor *, int, int, int,
                                       int) {}

void sphere_daux_clap_embed_refresh(SphereDauxClapProcessor *) {}

unsigned long long sphere_daux_clap_embed_attach_hwnd(
    SphereDauxClapProcessor *) {
  return 0;
}

void sphere_daux_clap_embed_detach(SphereDauxClapProcessor *) {}

int sphere_daux_clap_embed_is_valid(SphereDauxClapProcessor *) { return 0; }

int sphere_daux_clap_embed_has_visible_ui(SphereDauxClapProcessor *) {
  return 0;
}

unsigned long long sphere_daux_clap_open_editor(SphereDauxClapProcessor *,
                                                const char *, const char *, int,
                                                int) {
  clap_set_last_error("CLAP editors are not supported on this platform");
  return 0;
}

void sphere_daux_clap_close_editor(SphereDauxClapProcessor *) {}

int sphere_daux_clap_focus_editor(SphereDauxClapProcessor *) { return 0; }

// ── Host-owned view host ────────────────────────────────────────────────────

int sphere_daux_clap_view_attach(SphereDauxClapProcessor *, unsigned long long,
                                 int, int, int *, int *) {
  clap_set_last_error("CLAP editors are not supported on this platform");
  return 0;
}

void sphere_daux_clap_view_detach(SphereDauxClapProcessor *) {}

int sphere_daux_clap_view_is_attached(SphereDauxClapProcessor *) { return 0; }

int sphere_daux_clap_view_set_size(SphereDauxClapProcessor *, int, int) {
  return 0;
}

int sphere_daux_clap_view_get_size(SphereDauxClapProcessor *, int *, int *) {
  return 0;
}

int sphere_daux_clap_view_can_resize(SphereDauxClapProcessor *) { return 0; }

int sphere_daux_clap_view_constrain(SphereDauxClapProcessor *, int *, int *) {
  return 0;
}

int sphere_daux_clap_view_take_resize_request(SphereDauxClapProcessor *, int *,
                                              int *) {
  return 0;
}


/// No host-owned editor window on this platform, so nothing for the chrome to
/// attach to — and a 0 handle is exactly how the chrome ABI says so.
unsigned long long
sphere_daux_clap_editor_native_window(SphereDauxClapProcessor *) {
  return 0;
}

} // extern "C"
