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

/// Build a window's content view: chrome on top, the plug-in's area below.
///
/// `frame` is the whole content rect, chrome included — the caller has already
/// added `sphere_daux_editor_chrome_height()` to the plug-in's own size.
NSView *sphere_daux_editor_shell_create(NSRect frame);

/// Where inside `shell` the plug-in's own view belongs.
///
/// Answers `shell.bounds` for anything that is not one of ours, so a caller
/// that has been handed a plain view still gets a usable rect rather than an
/// empty one.
NSRect sphere_daux_editor_shell_plugin_area(NSView *shell);

#endif // __APPLE__
