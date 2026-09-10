// editor_mac.mm — macOS NSWindow + NSView IPlugView embedding
// Compiled as Objective-C++ (.mm) with -fobjc-arc.
//
// Plug-in GUI lifecycle (macOS VST3):
//   open_editor_mac()   → create NSWindow + NSView
//                        → sphere_daux_editor_create_view("NSView")
//                        → sphere_daux_editor_attach_view(NSView*)
//                        → [NSWindow makeKeyAndOrderFront]
//   close_editor_mac()  → sphere_daux_editor_detach_view()
//                        → [NSWindow orderOut] + release retained objects
//   focus_editor_mac()  → [NSWindow makeKeyAndOrderFront]
//   shutdown_editor_mac() → close_editor_mac() (called from dtor / destroy)
//
// ObjC objects (NSWindow, NSView, delegate) are retained via __bridge_retained
// when stored as void* in the processor struct, and released via
// __bridge_transfer when the window is closed.

#include "editor_mac_internal.hpp"

#import <dispatch/dispatch.h>

#include <cmath>
#include <cstdio>

namespace {

constexpr int kMinEditorDimension = 16;
constexpr int kMaxEditorDimension = 8192;

bool daux_plugin_root_view_size(NSView *embed, int *out_width,
                                int *out_height) {
  if (!embed)
    return false;
  CGFloat best_area = 0.0;
  NSSize best = NSZeroSize;
  // VST3 Cocoa editors attach their native root as a direct child of the host
  // NSView. Some wrappers leave getSize() at the host's requested fallback but
  // give this root its real fixed dimensions; use the largest visible root.
  for (NSView *child in embed.subviews) {
    if (child.hidden)
      continue;
    NSSize size = child.frame.size;
    if (size.width < kMinEditorDimension ||
        size.height < kMinEditorDimension || size.width > kMaxEditorDimension ||
        size.height > kMaxEditorDimension)
      continue;
    const CGFloat area = size.width * size.height;
    if (area > best_area) {
      best_area = area;
      best = size;
    }
  }
  if (best_area <= 0.0)
    return false;
  if (out_width)
    *out_width = (int)std::llround(best.width);
  if (out_height)
    *out_height = (int)std::llround(best.height);
  return true;
}

} // namespace

void daux_resize_editor_content(SphereDauxVst3Processor *proc, int width,
                                int height, bool notify_plugin,
                                const char *reason) {
  if (!proc || width < kMinEditorDimension || height < kMinEditorDimension ||
      width > kMaxEditorDimension || height > kMaxEditorDimension)
    return;

  if (!NSThread.isMainThread) {
    dispatch_sync(dispatch_get_main_queue(), ^{
      daux_resize_editor_content(proc, width, height, notify_plugin, reason);
    });
    return;
  }

  void *window_ptr = sphere_daux_editor_get_native_window(proc);
  void *embed_ptr = sphere_daux_editor_get_native_embed(proc);
  void *delegate_ptr = sphere_daux_editor_get_native_delegate(proc);
  if (!window_ptr || !embed_ptr)
    return;


  NSWindow *window = (__bridge NSWindow *)window_ptr;
  (void)embed_ptr;
  DauxEditorWindowDelegate *delegate =
      delegate_ptr ? (__bridge DauxEditorWindowDelegate *)delegate_ptr : nil;

  // `width`/`height` are the *plug-in's* content size; the shared helper adds
  // the chrome strip, keeps the top-left fixed and re-frames both bands. The
  // guard stops the resize we are about to cause coming back as a user resize.
  delegate.applyingHostResize = YES;
  sphere_daux_editor_window_set_plugin_size(
      window, NSMakeSize((CGFloat)width, (CGFloat)height));
  delegate.applyingHostResize = NO;
  sphere_daux_editor_set_content_size(proc, width, height);

  if (notify_plugin)
    sphere_daux_editor_notify_resize(proc, width, height);
  std::fprintf(stderr,
               "[SphereVST3/mac] content resize reason=%s size=%dx%d\n",
               reason ? reason : "unknown", width, height);
}

/// Host-requested resize of the editor window (`sphere_daux_vst3_view_set_size`).
///
/// Unlike `sphere_daux_editor_apply_plugin_resize`, which answers a size the
/// *plug-in* asked for and therefore must not echo it back, this size comes
/// from the host, so the view is told about it — that report is the whole point
/// of the call.
void resize_editor_mac(SphereDauxVst3Processor *proc, int width, int height,
                       const char *reason) {
  daux_resize_editor_content(proc, width, height, true,
                             reason ? reason : "view-host-set-size");
}

extern "C" int sphere_daux_editor_apply_plugin_resize(
    SphereDauxVst3Processor *proc, int width, int height) {
  if (!proc || width < kMinEditorDimension || height < kMinEditorDimension ||
      width > kMaxEditorDimension || height > kMaxEditorDimension)
    return 0;
  daux_resize_editor_content(proc, width, height, false, "resizeView");
  return sphere_daux_editor_get_native_window(proc) != nullptr ? 1 : 0;
}

/// Open a floating NSWindow containing an NSView that hosts the plugin's GUI.
/// May be called from any thread; dispatches to the main thread synchronously.
unsigned long long open_editor_mac(SphereDauxVst3Processor *proc,
                                   const char *window_id, const char *title,
                                   int width, int height) {
  if (!proc)
    return 0;

  // All Cocoa work must happen on the main thread.
  if (!NSThread.isMainThread) {
    __block unsigned long long result = 0;
    dispatch_sync(dispatch_get_main_queue(), ^{
      result = open_editor_mac(proc, window_id, title, width, height);
    });
    return result;
  }

  // Already open? Bring it to front and return the existing handle.
  void *existing_win = sphere_daux_editor_get_native_window(proc);
  if (existing_win) {
    NSWindow *win = (__bridge NSWindow *)existing_win;
    [win makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    return sphere_daux_editor_get_handle(proc);
  }

  // ── Step 1: Create the IPlugView and query preferred size ────────────────

  int editor_width = width > 0 ? width : 820;
  int editor_height = height > 0 ? height : 560;

  if (!sphere_daux_editor_create_view(proc, "NSView", &editor_width,
                                      &editor_height)) {
    std::fprintf(stderr, "[SphereVST3/mac] create_view('NSView') failed\n");
    return 0;
  }

  // ── Step 2/3/4: Window, chrome strip and the IPlugView parent ────────────

  // All three come from the shared editor window, which every format bridge
  // opens through: it is the one place that knows a host-owned editor window is
  // "chrome strip, then plug-in", and the one place that positions either band.
  // `editor_width`/`editor_height` stay the *plug-in's* size throughout — the
  // only size a plug-in ever agrees to.
  DauxEditorWindowDelegate *delegate = [[DauxEditorWindowDelegate alloc] init];
  delegate.processor = proc;

  const bool editor_resizable = sphere_daux_editor_can_resize(proc) != 0;
  NSWindow *window = sphere_daux_editor_window_create(
      NSMakeSize((CGFloat)editor_width, (CGFloat)editor_height),
      [NSString stringWithUTF8String:(title && *title) ? title
                                                       : "Plugin Editor"],
      editor_resizable ? YES : NO, delegate);
  [window setBackgroundColor:daux_bg_color()];

  NSView *embed = sphere_daux_editor_window_plugin_container(window);
  embed.layer.backgroundColor = daux_bg_color().CGColor;

  // ── Step 5: Attach IPlugView to NSView ────────────────────────────────────

  // Retain all ObjC objects as void* BEFORE calling attach so the store happens
  // atomically and close_editor_mac can release them even if attach fails
  // mid-way (we'll clear them on failure).
  void *win_retained = (__bridge_retained void *)window;
  void *embed_retained = (__bridge_retained void *)embed;
  void *delegate_retained = (__bridge_retained void *)delegate;

  unsigned long long handle = sphere_daux_editor_next_handle();

  // Store now so that close_editor_mac can run safely if attach fails.
  sphere_daux_editor_store_native(
      proc, win_retained, embed_retained, delegate_retained, handle,
      window_id ? window_id : "", (title && *title) ? title : "Plugin Editor",
      editor_width, editor_height);

  if (!sphere_daux_editor_attach_view(proc, (__bridge void *)embed, "NSView")) {
    std::fprintf(
        stderr,
        "[SphereVST3/mac] attach_view('NSView') failed; handle=%llu\n",
        handle);
    sphere_daux_editor_clear_native(proc);
    // Release retained objects (ARC releases when the local variables go out of
    // scope).
    {
      NSWindow *w = (__bridge_transfer NSWindow *)win_retained;
      [w setDelegate:nil];
      (void)w;
    }
    {
      NSView *v = (__bridge_transfer NSView *)embed_retained;
      (void)v;
    }
    {
      DauxEditorWindowDelegate *d =
          (__bridge_transfer DauxEditorWindowDelegate *)delegate_retained;
      (void)d;
    }
    return 0;
  }

  // A few editors only answer canResize reliably after attached(). Keep the
  // AppKit chrome in sync with that final answer, as the Windows host does.
  const bool attached_resizable = sphere_daux_editor_can_resize(proc) != 0;
  if (attached_resizable != editor_resizable) {
    NSWindowStyleMask updated_style = window.styleMask;
    if (attached_resizable)
      updated_style |= NSWindowStyleMaskResizable;
    else
      updated_style &= ~NSWindowStyleMaskResizable;
    [window setStyleMask:updated_style];
  }

  // ── Step 6: Resize window to plugin's post-attach preferred size ──────────

  // Some plug-ins (Kontakt among them) only finalize getSize() in attached().
  // The old code read embed.frame here, which was still the requested fallback
  // size and therefore left blank space above/right of the actual editor.
  int attached_width = editor_width;
  int attached_height = editor_height;
  int reported_width = 0;
  int reported_height = 0;
  if (sphere_daux_editor_get_view_size(proc, &attached_width,
                                       &attached_height)) {
    editor_width = attached_width;
    editor_height = attached_height;
  }
  if (daux_plugin_root_view_size(embed, &reported_width, &reported_height) &&
      (reported_width != editor_width || reported_height != editor_height)) {
    std::fprintf(stderr,
                 "[SphereVST3/mac] native root overrides stale getSize "
                 "getSize=%dx%d root=%dx%d\n",
                 editor_width, editor_height, reported_width, reported_height);
    editor_width = reported_width;
    editor_height = reported_height;
  }
  daux_resize_editor_content(proc, editor_width, editor_height, false,
                             "attached.getSize");

  // ── Step 7: Show the window ───────────────────────────────────────────────

  [window makeKeyAndOrderFront:nil];
  [NSApp activateIgnoringOtherApps:YES];

  std::fprintf(stderr,
               "[SphereVST3/mac] editor opened handle=%llu windowId=%s w=%d "
               "h=%d\n",
               handle, window_id ? window_id : "", editor_width, editor_height);
  return handle;
}

/// Detach the IPlugView and destroy the NSWindow.
/// May be called from any thread; dispatches to the main thread asynchronously
/// (or synchronously if already on the main thread).
void close_editor_mac(SphereDauxVst3Processor *proc) {
  if (!proc)
    return;

  if (!NSThread.isMainThread) {
    dispatch_async(dispatch_get_main_queue(), ^{ close_editor_mac(proc); });
    return;
  }

  // Grab and immediately clear the native pointers to prevent re-entrancy
  // (e.g. windowShouldClose: → close_editor_mac → [win orderOut] → ... ).
  void *win_ptr = sphere_daux_editor_get_native_window(proc);
  void *embed_ptr = sphere_daux_editor_get_native_embed(proc);
  void *delegate_ptr = sphere_daux_editor_get_native_delegate(proc);

  if (!win_ptr)
    return; // already closed

  sphere_daux_editor_clear_native(proc); // zero the struct fields first
  sphere_daux_editor_detach_view(proc);  // IPlugView::removed()

  // Release retained ObjC objects via __bridge_transfer.
  // Order: clear delegate first so no callbacks fire during window close.
  if (win_ptr) {
    NSWindow *win = (__bridge_transfer NSWindow *)win_ptr;
    [win setDelegate:nil];
    [win orderOut:nil]; // hide without triggering windowShouldClose:
    // ARC releases win here (our __bridge_retained +1 is consumed)
  }
  if (embed_ptr) {
    NSView *v = (__bridge_transfer NSView *)embed_ptr;
    [v removeFromSuperview];
    (void)v; // ARC releases
  }
  if (delegate_ptr) {
    DauxEditorWindowDelegate *d =
        (__bridge_transfer DauxEditorWindowDelegate *)delegate_ptr;
    (void)d; // ARC releases
  }

  std::fprintf(stderr, "[SphereVST3/mac] editor closed\n");
}

/// Bring the plugin editor window to the front.
int focus_editor_mac(SphereDauxVst3Processor *proc) {
  if (!proc)
    return 0;
  void *win_ptr = sphere_daux_editor_get_native_window(proc);
  if (!win_ptr)
    return 0;

  if (!NSThread.isMainThread) {
    dispatch_async(dispatch_get_main_queue(), ^{ focus_editor_mac(proc); });
    return 1;
  }

  NSWindow *win = (__bridge NSWindow *)win_ptr;
  [win makeKeyAndOrderFront:nil];
  [NSApp activateIgnoringOtherApps:YES];
  return 1;
}

/// Called from SphereDauxVst3Processor::shutdown() on macOS.
void shutdown_editor_mac(SphereDauxVst3Processor *proc) { close_editor_mac(proc); }
