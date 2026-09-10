// The editor chrome strip on platforms that do not draw one.
//
// macOS is the only platform where the plug-in editor's window belongs to the
// host process, so it is the only one that has to draw the tab strip and
// control row itself — everywhere else the studio's own GPUI window carries
// them above an embedded plug-in view. The real implementation is
// `editorplatform/macos/editor_mac_shell.mm`; this file is what the other
// targets link instead.
//
// These are no-ops rather than absent on purpose. The studio pushes chrome
// without first asking whether anyone is drawing it — `sphere_daux_*_editor_
// native_window` answers 0 on these platforms and every call below does
// nothing — which keeps one code path in the caller instead of a platform
// branch at every push. A height of 0 is what makes a window sized as
// "plug-in + chrome" come out exactly plug-in sized here.

#if !defined(__APPLE__)

extern "C" {

double sphere_daux_editor_chrome_height(void) { return 0.0; }

void sphere_daux_editor_chrome_begin(unsigned long long) {}

void sphere_daux_editor_chrome_set_header(unsigned long long, int,
                                          const char *, const char *,
                                          const char *, const char *) {}

void sphere_daux_editor_chrome_add_preset(unsigned long long, const char *,
                                          int) {}

void sphere_daux_editor_chrome_add_tab(unsigned long long, const char *,
                                       const char *, int) {}

void sphere_daux_editor_chrome_set_palette(unsigned long long,
                                           const unsigned int *, int) {}

void sphere_daux_editor_chrome_commit(unsigned long long, const char *) {}

int sphere_daux_editor_chrome_take_action(unsigned long long, int *, int *,
                                          char *, int) {
  return 0;
}

} // extern "C"

#endif // !__APPLE__
