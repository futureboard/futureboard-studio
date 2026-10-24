# Futureboard Patch Record

This repository contains local changes to vendored libraries. These changes are
intentional and should be preserved when updating dependencies.

## GPUI

Path: `crates/gpui`

Reason: Futureboard Studio needs app-specific native DAW desktop behavior and
rendering fixes that are not present in upstream GPUI.

Current patches:

- Uses the standard GPUI/platform cursor styles for the native Studio; the
  former Futureboard cursor extension and bundled cursor assets were removed.
- Added derivative-based SDF coverage and zero-alpha guards to the Windows
  HLSL and WGPU WGSL render paths.
- Enabled WGPU's DirectX 12 backend in the Futureboard workspace for Windows
  GPU-rendered surfaces.
- Guarded macOS traffic-light repositioning against missing standard window
  buttons, which AppKit removes for sheets and for non-minimizable style masks.
- Windows: `translate_accelerator` asks only GPUI's own windows whether they
  handled a key. It asks by sending a private message and reading the reply,
  treating 0 as "handled" — and `DefWindowProc` answers 0 to any message it does
  not know, so upstream every *other* window in the process claimed every key
  aimed at it and the loop dropped the message before dispatching it. A DAW has
  such windows on the same queue (plug-in editor views, hosted browsers, editor
  shell chrome), and their keystrokes never reached their window procedures.
- Windows: `WM_MOUSEACTIVATE` activates a window only when it is not already
  active, and `activate()` does nothing past restoring a minimized window when
  the window is already in the foreground. Both used to re-run activation and
  `SetFocus` on the window itself, which pulled focus off a plug-in editor's
  own popup (a JUCE dropdown) and closed it as it opened.

When rebasing GPUI, keep this file and `crates/gpui/PATCHED.md` updated with
the exact local changes that remain.
