# Patched GPUI

This vendored GPUI library has been modified for Futureboard Studio.

## Device-Scale Anti-Aliasing

Windows HLSL and WGPU WGSL SDF edges use fragment derivatives (`fwidth`) so
coverage follows the actual device-pixel footprint at fractional scale factors.
The compositing helper also avoids undefined RGB values for fully transparent
pixels.

The Futureboard workspace enables WGPU's DirectX 12 backend for Windows GPU
surfaces.

## macOS Traffic-Light Guard

`MacWindowState::move_traffic_light` now returns early when any of the three
standard window buttons is missing. AppKit removes all of them while a window is
presented as a sheet, and a style mask without `.miniaturizable` never creates
the minimize button, so the previous unconditional `frame` message was sent to a
nil button and aborted the process. Futureboard opens non-minimizable windows for
dialogs and session transactions, so this guard is required.

## Frame Profile Hook

`gpui::frame_profile` publishes the duration of the two phases an embedder
cannot otherwise see — `Window::draw` (element tree build, layout, prepaint,
paint) and `Window::present` (handing the scene to the platform) — as relaxed
atomics from the most recent frame.

Futureboard's Profiler overlay times its own `render` functions, but those cover
only element construction. Without this hook a 40 ms frame containing 0.2 ms of
app work is indistinguishable from a broken profiler, and there is nothing to
optimize against.

It also splits the draw into its phases — prepaint (element tree build plus
layout), paint, and accessibility-tree rebuild — and reports the readings that
say *why* a phase is expensive: microseconds spent shaping text that missed the
two-frame line-layout cache (`text_system/line_layout.rs`), the layout node
count (`taffy.rs`), and the primitive count of the finished scene. Layout nodes
matter more than primitives here: containers lay out without drawing, so a frame
can walk a large tree while emitting few primitives.

Cost is a handful of `Instant::now()` calls per frame plus one per shaping cache
miss; no behavior change.

## Truncating Text Re-Measured Every Pass

`TextLayout::layout` refused its own cached size whenever the style truncated,
because a cached layout *might* have been produced without truncation. Taffy
measures a node more than once per frame and Futureboard truncates nearly every
label it draws (track names, clip names, mixer channels), so those elements
re-ran line wrapping and truncation on every measure pass of every frame.

`TextLayoutInner` now records the truncation width its layout was produced with,
so the guard compares widths instead of disabling the cache. Same output, and a
layout is reused only when it was built for exactly the width being asked for.

Measured on a 31-track session: layout measure callbacks cost 13.5 ms per frame
across 3,637 calls, inside a 20.9 ms layout solve.

## Windows: Keys Aimed at Non-GPUI Windows Were Dropped

The Windows message loop asks whether GPUI already handled a `WM_KEYDOWN` by
sending the target window a private `WM_GPUI_KEYDOWN` and reading the reply,
treating `0` as "handled" and skipping `TranslateMessage`/`DispatchMessage`.

`DefWindowProc` answers `0` to any message it does not recognise. So every
window in the process that is *not* GPUI's -- and a DAW has several on the same
queue: a plug-in's native `IPlugView`, a hosted CEF browser, an editor shell's
own chrome -- claimed every key aimed at it, and the loop threw the message away
before the window's own procedure ever ran. Text fields inside an in-process
plug-in editor could be clicked but never typed into.

`translate_accelerator` now puts the question only to windows in
`raw_window_handles`. Anything else is dispatched to whoever it was addressed
to, which is what Win32 would have done anyway.

## Nested Scroll Containers All Scrolled At Once

`HitboxId::should_handle_scroll` deliberately returns true for *every* hitbox
under the pointer -- the hovered element and all of its ancestors -- and
`Interactivity::paint_scroll_listener` never claimed the event. Every enclosing
scroll container therefore applied the same wheel delta on the same tick, so
scrolling an inner list also scrolled the panel, the dialog body, and anything
else scrollable above it.

The listener now clamps the new offset against the scroll maximum recorded by
the preceding prepaint (`Interactivity::scroll_max`) instead of leaving the
clamp to the next frame. That makes "the offset moved" mean "this container
actually scrolled", which is the signal the fix needs: a container that moved
claims the event through `Window::mark_scroll_consumed`, and every other scroll
container skips a claimed event. A container already at its limit does not
claim, so the delta still chains outward to its ancestors the way a browser
chains overscroll.

The claim is a window flag rather than `cx.stop_propagation()` on purpose.
GPUI registers the internal scroll listener *after* the element's own
`on_scroll_wheel` listeners, and the bubble phase runs listeners in reverse
registration order, so stopping propagation there would swallow application
wheel handlers attached to the same scrollable element.

## Maintenance Notes

When updating GPUI from upstream, preserve this Futureboard patch or port it
forward deliberately.

## Windows: `Floating` windows are owned, not topmost

`gpui_windows` used to create `WindowKind::Floating` windows with
`WS_EX_TOPMOST` and no owner, so every Studio tool window (plug-in editors,
the MIDI editor, FX/EQ, samplers) floated above *every application* on the
desktop. A floating window is now created *owned by* the application's main
window instead (`floating_owner_window`: the root owner of the active window,
or the first visible unowned window of the UI thread). Windows keeps an owned
window above its owner, hides it when the owner is minimized and destroys it
with the owner — which is what macOS's floating level and X11's
`WM_TRANSIENT_FOR` already gave the kind on those platforms.

## Windows: activation leaves an embedded child's focus alone

`WM_MOUSEACTIVATE` called `SetActiveWindow` on every click that reached the
window, including clicks inside an embedded child window (a plug-in editor's
view, possibly in another process). When the window was not active that ran the
default activation, which focuses the top-level window. `activate()` likewise
called `SetActiveWindow` + `SetFocus` on the window, synthesized an Alt
press/release and called `SetForegroundWindow`, even when the window was
already in front.

A JUCE popup menu closes itself as soon as focus leaves the plug-in's own
windows, so either path made a plug-in's dropdown flash open and shut. Now
`WM_MOUSEACTIVATE` (events.rs) activates only a window that is not already the
active one, and `activate()` (window.rs) returns after restoring a minimized
window when `GetForegroundWindow()` is already this window.