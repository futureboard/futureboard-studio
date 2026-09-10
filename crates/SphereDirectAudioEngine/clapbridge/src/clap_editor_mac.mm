// macOS editor hosting for CLAP plug-ins.
//
// A Cocoa CLAP GUI is parented to an `NSView` through `clap_window.cocoa`, so
// this file owns a small NSWindow + container NSView per instance rather than
// reusing the Win32 `daux_editor_*` shell (which is HWND-based).
//
// Embedded mode receives the GPUI-provided `NSView*` directly and parents the
// container into it; standalone mode creates its own titled window.

#if !defined(__APPLE__)
#error "clap_editor_mac.mm is macOS-only"
#endif

#include "clap_processor_internal.hpp"

#include "sphere_daux_editor_chrome.h"
#include "sphere_daux_editor_shell_mac.h"

#include <cstdint>
#include <string>

#import <Cocoa/Cocoa.h>

@interface DauxClapEditorWindowDelegate : NSObject <NSWindowDelegate>
@property(nonatomic, assign) SphereDauxClapProcessor *processor;
@end

@implementation DauxClapEditorWindowDelegate
- (BOOL)windowShouldClose:(NSWindow *)sender {
  (void)sender;
  if (self.processor) {
    // Report the user close; the host tears the editor down and keeps the
    // audio instance alive.
    self.processor->embed_user_closed.store(true, std::memory_order_release);
  }
  return NO;
}
@end

namespace {

/// Run the `clap.gui` attach sequence into `container`.
bool attach_into(SphereDauxClapProcessor *p, NSView *container, int *width,
                 int *height) {
  if (!p || !p->plugin || !p->ext_gui || !container) {
    return false;
  }

  if (!p->gui_created) {
    if (!p->ext_gui->is_api_supported ||
        !p->ext_gui->is_api_supported(p->plugin, CLAP_WINDOW_API_COCOA,
                                      false)) {
      clap_set_last_error("CLAP plug-in does not support an embedded Cocoa GUI");
      return false;
    }
    if (!p->ext_gui->create ||
        !p->ext_gui->create(p->plugin, CLAP_WINDOW_API_COCOA, false)) {
      clap_set_last_error("clap_plugin_gui->create() returned false");
      return false;
    }
    p->gui_created = true;
  }

  if (p->ext_gui->set_scale) {
    const double scale =
        container.window ? container.window.backingScaleFactor : 1.0;
    p->ext_gui->set_scale(p->plugin, scale);
  }

  p->preferred_gui_size(width, height);

  clap_window_t window{};
  window.api = CLAP_WINDOW_API_COCOA;
  window.cocoa = (__bridge void *)container;
  if (!p->ext_gui->set_parent || !p->ext_gui->set_parent(p->plugin, &window)) {
    clap_set_last_error("clap_plugin_gui->set_parent() returned false");
    return false;
  }

  if (p->ext_gui->show) {
    p->ext_gui->show(p->plugin);
  }

  // Re-query after show: some plug-ins only settle their size once visible.
  p->preferred_gui_size(width, height);
  if (*width > 0 && *height > 0) {
    p->embed_content_w = *width;
    p->embed_content_h = *height;
    // Only the plug-in's own views. The container's frame belongs to whoever
    // made it — the shell in a host-owned window, the caller in an embedded
    // one — so resizing it from here would fight the layout that owns it.
    for (NSView *child in container.subviews) {
      child.frame = NSMakeRect(0, 0, *width, *height);
    }
  }

  p->editor_attached = true;
  std::fprintf(stderr, "[clap-editor] attached instance=%s size=%dx%d\n",
               p->embed_instance_label.empty()
                   ? "<unknown>"
                   : p->embed_instance_label.c_str(),
               *width, *height);
  return true;
}

} // namespace

// ── Platform entry points ───────────────────────────────────────────────────

unsigned long long clap_embed_editor_mac(SphereDauxClapProcessor *p,
                                         unsigned long long parent_view, int x,
                                         int y, int width, int height) {
  if (!p || !p->plugin || !p->ext_gui) {
    clap_set_last_error("CLAP embed editor: plug-in exposes no GUI");
    return 0;
  }
  NSView *parent = (__bridge NSView *)reinterpret_cast<void *>(
      static_cast<std::uintptr_t>(parent_view));
  if (!parent) {
    clap_set_last_error("CLAP embed editor: invalid parent NSView");
    return 0;
  }

  if (p->editor_attached && p->editor_native_embed) {
    NSView *existing = (__bridge NSView *)p->editor_native_embed;
    existing.frame = NSMakeRect(x, y, width, height);
    return p->editor_handle;
  }

  int w = width > 0 ? width : 640;
  int h = height > 0 ? height : 480;

  NSView *container = [[NSView alloc] initWithFrame:NSMakeRect(x, y, w, h)];
  [parent addSubview:container];
  p->editor_native_embed = (__bridge_retained void *)container;
  p->embed_mode = true;
  p->embed_host_kind = 0; // child view

  if (!attach_into(p, container, &w, &h)) {
    [container removeFromSuperview];
    CFRelease(p->editor_native_embed);
    p->editor_native_embed = nullptr;
    p->embed_mode = false;
    return 0;
  }

  p->editor_handle = clap_next_editor_handle();
  return p->editor_handle;
}

void clap_embed_set_bounds_mac(SphereDauxClapProcessor *p, int x, int y,
                               int width, int height) {
  if (!p || !p->editor_native_embed || width <= 0 || height <= 0) {
    return;
  }
  NSView *container = (__bridge NSView *)p->editor_native_embed;
  container.frame = NSMakeRect(x, y, width, height);
  p->embed_host_x = x;
  p->embed_host_y = y;
  p->embed_host_w = width;
  p->embed_host_h = height;
  if (!p->editor_resizable || !p->gui_created || !p->ext_gui) {
    return;
  }
  // Let the plug-in snap the request to a size it accepts before applying it.
  auto w = static_cast<uint32_t>(width);
  auto h = static_cast<uint32_t>(height);
  if (p->ext_gui->adjust_size) {
    p->ext_gui->adjust_size(p->plugin, &w, &h);
  }
  if (p->ext_gui->set_size) {
    p->ext_gui->set_size(p->plugin, w, h);
  }
  p->embed_content_w = static_cast<int>(w);
  p->embed_content_h = static_cast<int>(h);
  for (NSView *child in container.subviews) {
    child.frame = NSMakeRect(0, 0, static_cast<int>(w), static_cast<int>(h));
  }
}

unsigned long long clap_open_editor_mac(SphereDauxClapProcessor *p,
                                        const char *window_id,
                                        const char *title, int width,
                                        int height) {
  if (!p || !p->plugin || !p->ext_gui) {
    clap_set_last_error("CLAP editor: plug-in exposes no GUI");
    return 0;
  }
  p->editor_window_id = window_id ? window_id : "";
  if (title && *title) {
    p->editor_title = title;
  }

  if (p->editor_attached && p->editor_native_window) {
    NSWindow *existing = (__bridge NSWindow *)p->editor_native_window;
    [existing makeKeyAndOrderFront:nil];
    return p->editor_handle;
  }

  int w = width > 0 ? width : 640;
  int h = height > 0 ? height : 480;

  // Window, chrome strip and the container the GUI attaches into all come from
  // the shared editor window — the one place that knows a host-owned editor
  // window is "chrome strip, then plug-in". `w`/`h` stay the *plug-in's* size
  // throughout, the only size a plug-in ever agrees to.
  DauxClapEditorWindowDelegate *delegate =
      [[DauxClapEditorWindowDelegate alloc] init];
  delegate.processor = p;

  NSWindow *window = sphere_daux_editor_window_create(
      NSMakeSize(w, h),
      [NSString stringWithUTF8String:p->editor_title.empty()
                                         ? "Plug-in Editor"
                                         : p->editor_title.c_str()],
      p->editor_resizable ? YES : NO, delegate);
  NSView *container = sphere_daux_editor_window_plugin_container(window);

  p->editor_native_window = (__bridge_retained void *)window;
  p->editor_native_embed = (__bridge_retained void *)container;
  p->editor_native_delegate = (__bridge_retained void *)delegate;
  p->embed_mode = false;
  p->embed_host_kind = 2; // detached top-level

  if (!attach_into(p, container, &w, &h)) {
    clap_close_editor_mac(p);
    return 0;
  }

  // What `clap_plugin_gui->get_size` settled on after `show`, which is where a
  // GUI that scales to the display reports its real size.
  sphere_daux_editor_window_set_plugin_size(
      window, NSMakeSize(p->embed_content_w, p->embed_content_h));
  [window makeKeyAndOrderFront:nil];
  [NSApp activateIgnoringOtherApps:YES];

  p->editor_handle = clap_next_editor_handle();
  return p->editor_handle;
}

void clap_close_editor_mac(SphereDauxClapProcessor *p) {
  if (!p) {
    return;
  }

  if (p->gui_created && p->plugin && p->ext_gui) {
    if (p->ext_gui->hide) {
      p->ext_gui->hide(p->plugin);
    }
    if (p->ext_gui->destroy) {
      p->ext_gui->destroy(p->plugin);
    }
    p->gui_created = false;
  }
  p->editor_attached = false;

  if (p->editor_native_embed) {
    NSView *container = (__bridge_transfer NSView *)p->editor_native_embed;
    [container removeFromSuperview];
    p->editor_native_embed = nullptr;
  }
  if (p->editor_native_window) {
    NSWindow *window = (__bridge_transfer NSWindow *)p->editor_native_window;
    window.delegate = nil;
    [window orderOut:nil];
    [window close];
    p->editor_native_window = nullptr;
  }
  if (p->editor_native_delegate) {
    DauxClapEditorWindowDelegate *delegate =
        (__bridge_transfer DauxClapEditorWindowDelegate *)
            p->editor_native_delegate;
    delegate.processor = nullptr;
    p->editor_native_delegate = nullptr;
  }

  p->embed_mode = false;
  p->editor_handle = 0;
}

int clap_focus_editor_mac(SphereDauxClapProcessor *p) {
  if (!p) {
    return 0;
  }
  if (p->editor_native_window) {
    NSWindow *window = (__bridge NSWindow *)p->editor_native_window;
    [window makeKeyAndOrderFront:nil];
    return 1;
  }
  if (p->editor_native_embed) {
    NSView *container = (__bridge NSView *)p->editor_native_embed;
    [container.window makeFirstResponder:container];
    return 1;
  }
  return 0;
}

// ── C API (macOS) ───────────────────────────────────────────────────────────

extern "C" {

unsigned long long sphere_daux_clap_embed_editor(SphereDauxClapProcessor *p,
                                                 unsigned long long parent,
                                                 int x, int y, int width,
                                                 int height) {
  return clap_embed_editor_mac(p, parent, x, y, width, height);
}

void sphere_daux_clap_embed_set_bounds(SphereDauxClapProcessor *p, int x, int y,
                                       int width, int height) {
  clap_embed_set_bounds_mac(p, x, y, width, height);
}

void sphere_daux_clap_embed_refresh(SphereDauxClapProcessor *) {
  // CLAP GUIs drive their own repaint through clap.timer-support; there is no
  // host idle call to make.
}

unsigned long long
sphere_daux_clap_embed_attach_hwnd(SphereDauxClapProcessor *p) {
  if (!p || !p->editor_native_embed) {
    return 0;
  }
  return static_cast<unsigned long long>(
      reinterpret_cast<std::uintptr_t>(p->editor_native_embed));
}

void sphere_daux_clap_embed_detach(SphereDauxClapProcessor *p) {
  clap_close_editor_mac(p);
}

int sphere_daux_clap_embed_is_valid(SphereDauxClapProcessor *p) {
  return (p && p->embed_mode && p->editor_attached && p->editor_native_embed)
             ? 1
             : 0;
}

int sphere_daux_clap_embed_has_visible_ui(SphereDauxClapProcessor *p) {
  if (!p || !p->editor_native_embed) {
    return 0;
  }
  NSView *container = (__bridge NSView *)p->editor_native_embed;
  return (container.subviews.count > 0 && !container.hiddenOrHasHiddenAncestor)
             ? 1
             : 0;
}

unsigned long long sphere_daux_clap_open_editor(SphereDauxClapProcessor *p,
                                                const char *window_id,
                                                const char *title, int width,
                                                int height) {
  return clap_open_editor_mac(p, window_id, title, width, height);
}

void sphere_daux_clap_close_editor(SphereDauxClapProcessor *p) {
  clap_close_editor_mac(p);
}

int sphere_daux_clap_focus_editor(SphereDauxClapProcessor *p) {
  return clap_focus_editor_mac(p);
}

/// The `NSWindow*` of this instance's host-owned editor, as an opaque handle.
///
/// 0 whenever no editor is open. The caller passes it straight to the shared
/// chrome ABI, which treats 0 as "no strip to update".
unsigned long long
sphere_daux_clap_editor_native_window(SphereDauxClapProcessor *p) {
  if (!p) {
    return 0;
  }
  return static_cast<unsigned long long>(
      reinterpret_cast<std::uintptr_t>(p->editor_native_window));
}

// ── Host-owned view host ────────────────────────────────────────────────────
//
// On Windows the host hands this side a window and this side only fills it.
// Cocoa has no way to put a view inside a window another process owns, so the
// same job is split differently here: the *window* is created next to the GUI,
// by `clap_open_editor_mac` above. The caller drives macOS through the same
// calls it drives Windows through and never learns which side made the window.

int sphere_daux_clap_view_attach(SphereDauxClapProcessor *p,
                                 unsigned long long parent_view, int width,
                                 int height, int *out_width, int *out_height) {
  if (!p || !p->plugin || !p->ext_gui) {
    clap_set_last_error("view host: plug-in exposes no GUI");
    return 0;
  }
  // Never a parent. The caller passes 0 on this platform; anything else is an
  // owner reference from a process whose handles mean nothing here.
  (void)parent_view;
  // A previous window's close flag must not be read as this one's — a stale
  // `true` would tear down an editor that has only just opened.
  p->embed_user_closed.store(false, std::memory_order_release);

  // The instance label, not `editor_window_id`: the label is what the host set
  // for this insert, and `editor_window_id` is only ever what a previous open
  // stored there — empty on the first one.
  const std::string window_id = p->embed_instance_label;
  const std::string title = p->editor_title;
  const unsigned long long handle = clap_open_editor_mac(
      p, window_id.c_str(), title.empty() ? "Plugin Editor" : title.c_str(),
      width, height);
  if (handle == 0) {
    return 0;
  }
  // What the GUI settled on: `clap_plugin_gui->get_size` after `show`, which is
  // where a plug-in that scales to the display reports its real size.
  if (out_width) {
    *out_width = p->embed_content_w > 0 ? p->embed_content_w : width;
  }
  if (out_height) {
    *out_height = p->embed_content_h > 0 ? p->embed_content_h : height;
  }
  return 1;
}

void sphere_daux_clap_view_detach(SphereDauxClapProcessor *p) {
  // `clap_plugin_gui->destroy` first, then the window —
  // `clap_close_editor_mac` does both in that order. The audio instance is
  // untouched.
  clap_close_editor_mac(p);
}

int sphere_daux_clap_view_is_attached(SphereDauxClapProcessor *p) {
  return (p && p->editor_attached && p->editor_native_window) ? 1 : 0;
}

int sphere_daux_clap_view_set_size(SphereDauxClapProcessor *p, int width,
                                   int height) {
  if (!p || !p->editor_native_window || width <= 0 || height <= 0) {
    return 0;
  }
  // The plug-in gets the last word on its own size, exactly as the embedded
  // path does: `adjust_size` snaps the request, `set_size` applies it.
  auto w = static_cast<uint32_t>(width);
  auto h = static_cast<uint32_t>(height);
  if (p->editor_resizable && p->gui_created && p->ext_gui) {
    if (p->ext_gui->adjust_size) {
      p->ext_gui->adjust_size(p->plugin, &w, &h);
    }
    if (p->ext_gui->set_size) {
      p->ext_gui->set_size(p->plugin, w, h);
    }
  } else {
    w = static_cast<uint32_t>(p->embed_content_w > 0 ? p->embed_content_w
                                                     : width);
    h = static_cast<uint32_t>(p->embed_content_h > 0 ? p->embed_content_h
                                                     : height);
  }

  NSWindow *window = (__bridge NSWindow *)p->editor_native_window;
  sphere_daux_editor_window_set_plugin_size(
      window, NSMakeSize((CGFloat)w, (CGFloat)h));
  p->embed_content_w = static_cast<int>(w);
  p->embed_content_h = static_cast<int>(h);
  return 1;
}

int sphere_daux_clap_view_get_size(SphereDauxClapProcessor *p, int *out_width,
                                   int *out_height) {
  if (!p || !out_width || !out_height) {
    return 0;
  }
  uint32_t w = 0;
  uint32_t h = 0;
  if (p->gui_created && p->ext_gui && p->ext_gui->get_size &&
      p->ext_gui->get_size(p->plugin, &w, &h) && w > 0 && h > 0) {
    *out_width = static_cast<int>(w);
    *out_height = static_cast<int>(h);
    return 1;
  }
  if (p->embed_content_w > 0 && p->embed_content_h > 0) {
    *out_width = p->embed_content_w;
    *out_height = p->embed_content_h;
    return 1;
  }
  return 0;
}

int sphere_daux_clap_view_can_resize(SphereDauxClapProcessor *p) {
  return (p && p->editor_resizable) ? 1 : 0;
}

int sphere_daux_clap_view_constrain(SphereDauxClapProcessor *p, int *io_width,
                                    int *io_height) {
  if (!p || !io_width || !io_height || *io_width <= 0 || *io_height <= 0) {
    return 0;
  }
  // A fixed-size GUI snaps back to its own size; a resizable one runs the
  // request through `adjust_size`, which is CLAP's constraint query.
  if (!p->editor_resizable || !p->gui_created || !p->ext_gui ||
      !p->ext_gui->adjust_size) {
    return sphere_daux_clap_view_get_size(p, io_width, io_height);
  }
  auto w = static_cast<uint32_t>(*io_width);
  auto h = static_cast<uint32_t>(*io_height);
  if (!p->ext_gui->adjust_size(p->plugin, &w, &h) || w == 0 || h == 0) {
    return sphere_daux_clap_view_get_size(p, io_width, io_height);
  }
  *io_width = static_cast<int>(w);
  *io_height = static_cast<int>(h);
  return 1;
}

int sphere_daux_clap_view_take_resize_request(SphereDauxClapProcessor *p,
                                              int *out_width, int *out_height) {
  // A CLAP plug-in asks for a size through `clap_host_gui->request_resize`,
  // which the bridge applies to its own window as it arrives, because that
  // window is right here. Nothing is ever left pending for the host to collect.
  (void)p;
  (void)out_width;
  (void)out_height;
  return 0;
}

} // extern "C"
