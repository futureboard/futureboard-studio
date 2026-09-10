#pragma once
// The Cocoa half of the shared editor shell: the content view that holds the
// chrome strip and, under it, the area a plug-in's own view goes in.
//
// Objective-C++ only, and separate from `sphere_daux_editor_chrome.h` for that
// reason — the chrome's update and action ABI is plain C so the processor cores
// can call it, while these two need `NSView` and `NSRect` and are only ever
// used from a `.mm`.
//
// Implemented in editor_mac_shell.mm alongside the strip itself; every format
// bridge that opens a macOS editor window builds its content view from here, so
// a VST2 editor and a VST3 editor are laid out by the same code.

#if defined(__APPLE__)

#import <Cocoa/Cocoa.h>

// ── The whole window ────────────────────────────────────────────────────────
//
// Every format bridge opens its editor through these three, so there is one
// place that knows a host-owned editor window is "chrome strip, then plug-in"
// and one place that does the arithmetic.
//
// That is not tidiness for its own sake. Each bridge used to build its own
// window and position its own container, and each could get it wrong on its
// own: VST2's `effEditOpen` path reset the container to (0,0) after attaching,
// which in the shell's flipped coordinates is the top-left — so the plug-in
// sat over the chrome and the strip was invisible while every other part of
// the machinery reported success. The container's frame belongs to the shell
// now, and a bridge that writes it anyway is corrected on the next layout.

/// Open an editor window for a plug-in of `plugin_size` points.
///
/// The window's content is the chrome strip plus that size — callers never add
/// the strip's height themselves. `resizable` comes from the plug-in's own
/// answer (`IPlugView::canResize`, `sizeWindow`, `clap.gui->can_resize`), so a
/// fixed-size editor gets a window the user cannot drag out of shape.
///
/// The window is returned unshown; the caller orders it front once the plug-in
/// has actually attached.
NSWindow *sphere_daux_editor_window_create(NSSize plugin_size, NSString *title,
                                           BOOL resizable,
                                           id<NSWindowDelegate> delegate);

/// The view a plug-in attaches into, or nil for a window that is not one of
/// ours. Its frame belongs to the shell — set the size through
/// `sphere_daux_editor_window_set_plugin_size` instead of writing it.
NSView *sphere_daux_editor_window_plugin_container(NSWindow *window);

/// Resize so the plug-in's area is exactly `plugin_size`, keeping the window's
/// top-left fixed — AppKit's screen coordinates grow upward, and a resize that
/// forgets that makes the editor jump around the display.
///
/// Re-frames the container and every view the plug-in put inside it.
void sphere_daux_editor_window_set_plugin_size(NSWindow *window,
                                               NSSize plugin_size);

/// The plug-in's area, in points, as the window currently has it.
NSSize sphere_daux_editor_window_plugin_size(NSWindow *window);

#endif // __APPLE__
