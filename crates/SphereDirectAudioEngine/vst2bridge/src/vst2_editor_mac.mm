// macOS editor hosting for VST2 plug-ins.
//
// A Cocoa VST2 editor takes an `NSView*` as its `effEditOpen` parent, so this
// file owns a small NSWindow + container NSView per instance rather than
// reusing the Win32 `daux_editor_*` shell (which is HWND-based).
//
// Embedded mode receives the GPUI-provided `NSView*` directly and parents the
// container into it; standalone mode creates its own titled window.

#if !defined(__APPLE__)
#error "vst2_editor_mac.mm is macOS-only"
#endif

#include "vst2_processor_internal.hpp"

#include "sphere_daux_editor_chrome.h"
#include "sphere_daux_editor_shell_mac.h"

#include <string>

#import <Cocoa/Cocoa.h>

@interface DauxVst2EditorWindowDelegate : NSObject <NSWindowDelegate>
@property(nonatomic, assign) SphereDauxVst2Processor *processor;
@end

@implementation DauxVst2EditorWindowDelegate
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

/// Preferred editor size from the plug-in, falling back to the requested size.
void preferred_size(SphereDauxVst2Processor *p, int *width, int *height) {
  ERect *rect = nullptr;
  p->dispatch(effEditGetRect, 0, 0, &rect);
  if (rect) {
    const int w = rect->right - rect->left;
    const int h = rect->bottom - rect->top;
    if (w > 0 && h > 0) {
      *width = w;
      *height = h;
    }
  }
}

/// Create the container view the plug-in draws into, attach it, and record the
/// resulting size. Returns false when the plug-in produced no subview.
bool attach_into(SphereDauxVst2Processor *p, NSView *container, int width,
                 int height) {
  const auto result =
      p->dispatch(effEditOpen, 0, 0, (__bridge void *)container);
  if (container.subviews.count == 0) {
    std::fprintf(stderr,
                 "[vst2-editor] effEditOpen produced no subview (result=%lld)\n",
                 static_cast<long long>(result));
    p->dispatch(effEditClose);
    return false;
  }

  int w = width;
  int h = height;
  preferred_size(p, &w, &h);
  if (w > 0 && h > 0) {
    p->embed_content_w = w;
    p->embed_content_h = h;
    // Only the plug-in's own views. The container's frame belongs to whoever
    // made it — the shell in a host-owned window, the caller in an embedded
    // one — and writing `(0, 0, w, h)` here is what once put a VST2 editor on
    // top of the chrome strip: in the shell's flipped coordinates that origin
    // is the top-left corner, not the bottom-left.
    for (NSView *child in container.subviews) {
      child.frame = NSMakeRect(0, 0, w, h);
    }
  }

  p->editor_attached = true;
  std::fprintf(stderr, "[vst2-editor] attached instance=%s size=%dx%d\n",
               p->embed_instance_label.empty()
                   ? "<unknown>"
                   : p->embed_instance_label.c_str(),
               w, h);
  return true;
}

} // namespace

// ── Platform entry points used by vst2_processor.cpp ────────────────────────

unsigned long long vst2_embed_editor_mac(SphereDauxVst2Processor *p,
                                         unsigned long long parent_view,
                                         int x, int y, int width, int height) {
  if (!p || !p->effect || !p->has_editor) {
    vst2_set_last_error("VST2 embed editor: no editor on this plug-in");
    return 0;
  }
  NSView *parent = (__bridge NSView *)reinterpret_cast<void *>(
      static_cast<std::uintptr_t>(parent_view));
  if (!parent) {
    vst2_set_last_error("VST2 embed editor: invalid parent NSView");
    return 0;
  }

  if (p->editor_attached && p->editor_native_embed) {
    NSView *existing = (__bridge NSView *)p->editor_native_embed;
    existing.frame = NSMakeRect(x, y, width, height);
    return p->editor_handle;
  }

  int w = width > 0 ? width : 640;
  int h = height > 0 ? height : 480;
  preferred_size(p, &w, &h);

  NSView *container =
      [[NSView alloc] initWithFrame:NSMakeRect(x, y, w, h)];
  [parent addSubview:container];
  p->editor_native_embed = (__bridge_retained void *)container;
  p->embed_mode = true;
  p->embed_host_kind = 0; // child view

  if (!attach_into(p, container, w, h)) {
    [container removeFromSuperview];
    CFRelease(p->editor_native_embed);
    p->editor_native_embed = nullptr;
    p->embed_mode = false;
    vst2_set_last_error("VST2 embed editor: effEditOpen created no view");
    return 0;
  }

  p->editor_handle = vst2_next_editor_handle();
  return p->editor_handle;
}

void vst2_embed_set_bounds_mac(SphereDauxVst2Processor *p, int x, int y,
                               int width, int height) {
  if (!p || !p->editor_native_embed || width <= 0 || height <= 0)
    return;
  NSView *container = (__bridge NSView *)p->editor_native_embed;
  container.frame = NSMakeRect(x, y, width, height);
  p->embed_host_x = x;
  p->embed_host_y = y;
  p->embed_host_w = width;
  p->embed_host_h = height;
  if (p->editor_resizable) {
    for (NSView *child in container.subviews) {
      child.frame = NSMakeRect(0, 0, width, height);
    }
  }
}

unsigned long long vst2_open_editor_mac(SphereDauxVst2Processor *p,
                                        const char *window_id,
                                        const char *title, int width,
                                        int height) {
  if (!p || !p->effect || !p->has_editor) {
    vst2_set_last_error("VST2 editor: no editor on this plug-in");
    return 0;
  }
  p->editor_window_id = window_id ? window_id : "";
  if (title && *title)
    p->editor_title = title;

  if (p->editor_attached && p->editor_native_window) {
    NSWindow *existing = (__bridge NSWindow *)p->editor_native_window;
    [existing makeKeyAndOrderFront:nil];
    return p->editor_handle;
  }

  int w = width > 0 ? width : 640;
  int h = height > 0 ? height : 480;
  preferred_size(p, &w, &h);

  // Window, chrome strip and the container the plug-in attaches into all come
  // from the shared editor window — the one place that knows a host-owned
  // editor window is "chrome strip, then plug-in". `w`/`h` stay the *plug-in's*
  // size throughout, the only size a plug-in ever agrees to.
  DauxVst2EditorWindowDelegate *delegate =
      [[DauxVst2EditorWindowDelegate alloc] init];
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

  if (!attach_into(p, container, w, h)) {
    vst2_close_editor_mac(p);
    vst2_set_last_error("VST2 editor: effEditOpen created no view");
    return 0;
  }

  // What `effEditGetRect` settled on after the open, which is where several
  // plug-ins first report their real size.
  sphere_daux_editor_window_set_plugin_size(
      window, NSMakeSize(p->embed_content_w, p->embed_content_h));
  [window makeKeyAndOrderFront:nil];
  [NSApp activateIgnoringOtherApps:YES];

  p->editor_handle = vst2_next_editor_handle();
  return p->editor_handle;
}

void vst2_close_editor_mac(SphereDauxVst2Processor *p) {
  if (!p)
    return;

  if (p->editor_attached && p->effect) {
    p->dispatch(effEditClose);
    p->editor_attached = false;
  }

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
    DauxVst2EditorWindowDelegate *delegate =
        (__bridge_transfer DauxVst2EditorWindowDelegate *)
            p->editor_native_delegate;
    delegate.processor = nullptr;
    p->editor_native_delegate = nullptr;
  }

  p->embed_mode = false;
  p->editor_handle = 0;
}

int vst2_focus_editor_mac(SphereDauxVst2Processor *p) {
  if (!p)
    return 0;
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

void vst2_editor_idle_mac(SphereDauxVst2Processor *p) {
  if (p && p->editor_attached)
    p->dispatch(effEditIdle);
}

// ── C API (macOS) ───────────────────────────────────────────────────────────

extern "C" {

unsigned long long sphere_daux_vst2_embed_editor(SphereDauxVst2Processor *p,
                                                 unsigned long long parent,
                                                 int x, int y, int width,
                                                 int height) {
  return vst2_embed_editor_mac(p, parent, x, y, width, height);
}

void sphere_daux_vst2_embed_set_bounds(SphereDauxVst2Processor *p, int x, int y,
                                       int width, int height) {
  vst2_embed_set_bounds_mac(p, x, y, width, height);
}

void sphere_daux_vst2_embed_refresh(SphereDauxVst2Processor *p) {
  vst2_editor_idle_mac(p);
}

unsigned long long
sphere_daux_vst2_embed_attach_hwnd(SphereDauxVst2Processor *p) {
  if (!p || !p->editor_native_embed)
    return 0;
  return static_cast<unsigned long long>(
      reinterpret_cast<std::uintptr_t>(p->editor_native_embed));
}

void sphere_daux_vst2_embed_detach(SphereDauxVst2Processor *p) {
  vst2_close_editor_mac(p);
}

int sphere_daux_vst2_embed_is_valid(SphereDauxVst2Processor *p) {
  return (p && p->embed_mode && p->editor_attached && p->editor_native_embed)
             ? 1
             : 0;
}

int sphere_daux_vst2_embed_has_visible_ui(SphereDauxVst2Processor *p) {
  if (!p || !p->editor_native_embed)
    return 0;
  NSView *container = (__bridge NSView *)p->editor_native_embed;
  return (container.subviews.count > 0 && !container.hiddenOrHasHiddenAncestor)
             ? 1
             : 0;
}

unsigned long long sphere_daux_vst2_open_editor(SphereDauxVst2Processor *p,
                                                const char *window_id,
                                                const char *title, int width,
                                                int height) {
  return vst2_open_editor_mac(p, window_id, title, width, height);
}

void sphere_daux_vst2_close_editor(SphereDauxVst2Processor *p) {
  vst2_close_editor_mac(p);
}

int sphere_daux_vst2_focus_editor(SphereDauxVst2Processor *p) {
  return vst2_focus_editor_mac(p);
}

/// The `NSWindow*` of this instance's host-owned editor, as an opaque handle.
///
/// 0 whenever no editor is open. The caller passes it straight to the shared
/// chrome ABI, which treats 0 as "no strip to update".
unsigned long long
sphere_daux_vst2_editor_native_window(SphereDauxVst2Processor *p) {
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
// same job is split differently here: the *window* is created next to the view,
// by `vst2_open_editor_mac` above. The caller drives macOS through the same
// calls it drives Windows through and never learns which side made the window.

int sphere_daux_vst2_view_attach(SphereDauxVst2Processor *p,
                                 unsigned long long parent_view, int width,
                                 int height, int *out_width, int *out_height) {
  if (!p || !p->effect || !p->has_editor) {
    vst2_set_last_error("view host: no editor on this plug-in");
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
  const unsigned long long handle = vst2_open_editor_mac(
      p, window_id.c_str(), title.empty() ? "Plugin Editor" : title.c_str(),
      width, height);
  if (handle == 0) {
    return 0;
  }
  // What the editor settled on: `effEditGetRect` after `effEditOpen`, which is
  // where several plug-ins first report their real size.
  if (out_width) {
    *out_width = p->embed_content_w > 0 ? p->embed_content_w : width;
  }
  if (out_height) {
    *out_height = p->embed_content_h > 0 ? p->embed_content_h : height;
  }
  return 1;
}

void sphere_daux_vst2_view_detach(SphereDauxVst2Processor *p) {
  // `effEditClose` first, then the window — `vst2_close_editor_mac` does both
  // in that order. The audio instance is untouched.
  vst2_close_editor_mac(p);
}

int sphere_daux_vst2_view_is_attached(SphereDauxVst2Processor *p) {
  return (p && p->editor_attached && p->editor_native_window) ? 1 : 0;
}

int sphere_daux_vst2_view_set_size(SphereDauxVst2Processor *p, int width,
                                   int height) {
  if (!p || !p->editor_native_window || width <= 0 || height <= 0) {
    return 0;
  }
  // A VST2 editor that cannot resize keeps the size it reported; only a
  // `sizeWindow`-capable one is given a different one.
  if (!p->editor_resizable) {
    width = p->embed_content_w > 0 ? p->embed_content_w : width;
    height = p->embed_content_h > 0 ? p->embed_content_h : height;
  }
  NSWindow *window = (__bridge NSWindow *)p->editor_native_window;
  sphere_daux_editor_window_set_plugin_size(window, NSMakeSize(width, height));
  p->embed_content_w = width;
  p->embed_content_h = height;
  return 1;
}

int sphere_daux_vst2_view_get_size(SphereDauxVst2Processor *p, int *out_width,
                                   int *out_height) {
  if (!p || !out_width || !out_height) {
    return 0;
  }
  int w = p->embed_content_w;
  int h = p->embed_content_h;
  preferred_size(p, &w, &h);
  if (w <= 0 || h <= 0) {
    return 0;
  }
  *out_width = w;
  *out_height = h;
  return 1;
}

int sphere_daux_vst2_view_can_resize(SphereDauxVst2Processor *p) {
  return (p && p->editor_resizable) ? 1 : 0;
}

int sphere_daux_vst2_view_constrain(SphereDauxVst2Processor *p, int *io_width,
                                    int *io_height) {
  if (!p || !io_width || !io_height) {
    return 0;
  }
  // Fixed-size editors snap back to what they reported; VST2 has no
  // constraint query for the resizable ones, so their request stands.
  if (!p->editor_resizable) {
    return sphere_daux_vst2_view_get_size(p, io_width, io_height);
  }
  return 1;
}

int sphere_daux_vst2_view_take_resize_request(SphereDauxVst2Processor *p,
                                              int *out_width, int *out_height) {
  // A VST2 plug-in asks for a size through `audioMasterSizeWindow`, which the
  // bridge applies to its own window as it arrives, because that window is
  // right here. Nothing is ever left pending for the host to collect.
  (void)p;
  (void)out_width;
  (void)out_height;
  return 0;
}

void sphere_daux_vst2_view_idle(SphereDauxVst2Processor *p) {
  // Not optional and not a no-op: a VST2 editor repaints and animates only
  // while the host calls `effEditIdle`. Without this the window opens and then
  // sits frozen, which reads as a hung plug-in rather than a missing call.
  vst2_editor_idle_mac(p);
}

} // extern "C"
