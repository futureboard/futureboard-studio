# Futureboard Studio Design Contract

This document defines Futureboard Studio's visual language and the rules for
extending it. It describes the **token system in `crates/SphereUIComponents/src/theme.rs`**,
which is the authority. Where this document and the tokens disagree, the tokens win
and this document is wrong and should be fixed.

## The Futureboard signature

Futureboard is a signal-first creative instrument. Its signature is:

- a graphite ramp of clearly separated working planes, lit from value rather
  than from effects;
- one restrained cyan signal that marks focus, selection, live routing, and the
  primary action — and nothing else;
- rounded, deliberately sized controls whose radius follows their height;
- quiet chrome around expressive musical content;
- typography and numbers calibrated for fast scanning;
- motion that confirms cause and effect, never decorates idle space;
- musical time expressed through consistent grids, rhythm, and alignment;
- plugin identities that can be distinctive inside a disciplined Studio frame.

The interface should feel unmistakably Futureboard with all logos hidden. That
identity comes from proportion, density, state language, timing, and craft.

**On references.** Studying how Ableton, BandLab, Bitwig, Logic and Studio One
solve a problem is encouraged, and the current system was built by doing exactly
that. What travels is *principle and proportion*: radius-to-height ratios, plane
separation, state-layer mechanics, control grouping. What must never travel is
another product's trade dress — its palette, its signature layout identity, its
brand accent, its clip or device look, its wordmark. Take the reasoning, not the
skin.

## Token layers

Never write a raw literal for any of these. Every one lives in `theme.rs`.

| Layer | Module | What it holds |
| --- | --- | --- |
| Color | `Colors::*` (161 semantic tokens) | Themed via `packages/shared/themes/*.json` |
| Radius | `theme::radius` | `NONE 0 · MICRO 3 · CONTROL_SM 4 · CONTROL 6 · SURFACE 10 · DIALOG 14 · PILL` |
| Spacing | `theme::space` | `NONE 0 · HAIR 2 · TIGHT 4 · SNUG 6 · BASE 8 · LOOSE 12 · SECTION 16 · BLOCK 24 · PAGE 32` |
| Size | `theme::size` | Control-height ladder + `hit_target()` |
| State | `theme::state` | State-layer alphas |
| Motion | `theme::motion` | `MICRO 110ms · FAST 160ms · SLOW 240ms` |
| Elevation | `theme::elevation` | Shadow specs and the focus ring |
| Type | `theme::typography` | Size ladder |

The color tokens and the theme JSON files are generated from the same source, so
they cannot drift. `theme.rs`'s `theme_color!` fallbacks are that source; the
JSON files are produced from them. If you add a token, add it in both, and give
`Light.json` a real light value — a missing key silently inherits the dark one.

## Radius

Pick a token by **what the thing is**, never by how big it looks.

Radius tracks control height at roughly 0.2–0.25×, which is why there are two
control tiers rather than one flat value:

- 16–20 px controls → `CONTROL_SM` (4)
- 24–32 px controls → `CONTROL` (6)
- containing surfaces → `SURFACE` (10)
- windows and modals → `DIALOG` (14)

**Nesting.** An element inside a rounded container uses
`radius::inner(outer, padding)`. The scale is built so `inner(SURFACE, TIGHT)`
lands exactly on `CONTROL` — an inset control inside a panel is concentric by
construction. Using the parent's radius on a child makes the child's corner
read too tight.

**Content quads.** Anything whose size is data-driven — clips at any zoom, notes
at any row height, meter segments — goes through `radius::clamped(r, w, h)`,
which drops to square below a 10 px short side and caps at a quarter of it.
`Window::paint_quad` does *not* clamp corner radii the way the `div` path does,
so batched painters must call this explicitly.

### These must stay square

Rounding any of these is how a redesign ruins a DAW:

- timeline grid cells and bar shades — tiled corners expose a dot lattice;
- full-bleed track lanes and rows — a corner exposes a wedge at the boundary and
  reads as a clipping bug;
- the timeline ruler — it is a coordinate axis and must share the grid's edge;
- meter fills and segments — the topmost lit pixel must *be* the value;
- waveform, spectrum and custom GPU canvases — the content mask is an
  axis-aligned rect;
- MIDI notes below ~12 px row height;
- the inner edges of a segmented control, split button, or any touching pair;
- full-width table and list rows — only an inset backplate may round;
- the app chrome row, status bar, dock tab bars, window caption buttons.

## Color and surface hierarchy

Use semantic tokens. Do not place arbitrary colors in feature components.

Surface order, monotonic by value:

```txt
canvas / input (recessed)
  -> window / titlebar / sidebar / statusbar
    -> base workspace
      -> panel / card / mixer strip
        -> raised / popover / badge
          -> hover -> selected
```

Depth is carried by **value first, hairline second, shadow only for genuinely
floating layers**. On a dark panel a black shadow has almost no dynamic range
left, which is why shadow is reserved for menus, popovers and drag ghosts.
Borders are white-alpha so one token composites correctly on every plane.

Use cyan for active meaning only: focus and keyboard target, selection, live
connection or routed signal, and the one primary action. Do not wash panels in
accent. Decorative accent bars, gradients, glass, and glow do not belong in
Studio chrome. Functional meter, fade, spectrum, and waveform gradients are
allowed because they encode data.

## State language

The same state looks and behaves the same everywhere.

- **Rest** — a ghost control paints nothing; a filled control paints its surface
  token plus `border.subtle`.
- **Hover** — composite `state.hover` over the control's *rest* fill using
  `Colors::composite`. The border does not change. A GPUI div has exactly one
  background, so `.hover(|s| s.bg(token))` would replace the fill rather than
  lift it; resolve the composited color up front and hand that to the closure.
- **Pressed** — composite `state.recessed`, which goes *darker* than rest and
  reads as physical depression with no bevel. Accent-filled controls drop to
  `accent.pressed` instead. Use GPUI `.active()`.
- **Selected** — `state.selected` fill plus a leading-edge accent marker drawn as
  an overlay, never by growing a border (that reflows the row).
- **Focus** — `elevation::focus_ring()`, a zero-blur spread shadow outside the
  bounds, on `.focus_visible()` only. Focus is a ring, never a border recolor: a
  1 px color change is indistinguishable from hover.
- **Disabled** — content at `state::DISABLED_CONTENT`, `text.disabled` labels, no
  state layer, no pointer events.
- **Latched / armed** — the DAW states. Fill with the semantic hue at
  `ARMED_WASH`, border it at `ARMED_BORDER`, and paint the glyph at full
  strength. Use `Colors::latched()`.

**Latched hues are fixed and distinct.** `accent.primary` is forbidden on any
latched track toggle — the accent already marks selection, focus and playback, so
reusing it would make "is anything soloed?" unanswerable across a large
arrangement:

| State | Token | Hue |
| --- | --- | --- |
| Mute | `state.mute` | blue |
| Solo | `state.solo` | amber |
| Arm / record | `state.arm` | red |
| Input monitor | `state.monitor` | green |
| Automation | `state.automation` | violet |
| Frozen | `state.freeze` | frost |

**Hard rules.** Every state is encoded on two channels — fill *and* border, or
color *and* glyph — never hue alone. Radius never changes between states:
geometry is identity, not feedback.

UI must not lie. A control that appears active must connect to real project or
runtime state. If behavior is incomplete, disable it or label it honestly.

## Density and hit targets

Visual heights are deliberately smaller than hit targets. `size::hit_target()`
returns the transparent padding that lifts a 16 or 20 px control to a
comfortable 24 px clickable area, so the app reads tighter and clicks easier at
the same time. Never solve a hit-target problem by growing the visible control.

Every region must have an explicit contract: owner, state owner, coordinate
space, size source and min/max, scroll owner, clip owner, overflow behavior,
layer order, focus behavior. Do not fix geometry with spacer elements,
unexplained offsets, repeated local constants, or clipping on the wrong
ancestor.

Validate affected layouts at normal, narrow, short, maximized, and high-DPI
sizes, including open/closed side panels and resized bottom panels.

## Typography and numeric language

- Use the registered application font stack and the `theme::typography` ladder.
- Ordinary controls and labels stay compact, normally 11–13 px.
- Use tabular figures for time, bars/beats, dB, pan, percentages, samples,
  frequency, tempo, and parameter readouts.
- Keep units visually quieter than values without reducing clarity.
- Truncate chrome labels predictably; never let them wrap into neighbors.
- Give icons and text a shared baseline and consistent optical weight.

## Controls and interaction

- Use the shared primitives in `components/controls.rs` before writing a one-off:
  `fb_button`, `fb_icon_button`, `fb_toggle`, `fb_segment` / `fb_segmented_track`,
  `fb_checkbox`, `fb_badge`, `fb_progress`, `fb_form_row`, `fb_section_header`.
- Group a toolbar into a few modules rather than one flat run of buttons.
  `chrome_cluster()` is the shell's inset plate for this.
- Make drag controls expose a visible affordance, fine adjustment, a precise
  value, and a predictable reset gesture.
- Keep primary actions scarce; most controls stay visually quiet until active.
- Use destructive styling only for destructive actions and preserve Cancel.
- Anchor menus and popovers to measured bounds and clamp them to the window.
- Escape cancels transient gestures and safe-to-cancel dialogs.
- Respect focus priority: dialog, text/numeric input, focused editor, local
  surface, application command.
- Never trigger transport or destructive edit commands while typing into an
  input.

## Commands, menus, and dialogs

Use one command registry for menus, shortcuts, context menus, and command
search. Do not duplicate labels, enablement, or shortcut logic across surfaces.

Dialogs use the Studio shell: clear title and close behavior, `radius::DIALOG`,
`border.normal`, a consistent action footer, no unnecessary full-window blocker,
explicit Cancel for destructive or project-mutating actions, and focus trapped
only while truly modal.

Utility windows and plugin editors remain independent desktop surfaces with
correct focus, DPI, resize, and teardown behavior.

## Timeline and arrangement

Use one musical coordinate model for ruler, grid, clips, automation, loop range,
markers, playhead, drawing, and hit-testing.

- Bar lines lead, beats support, subdivisions recede.
- Grid density and labels adapt to zoom without collisions.
- Headers and scrollable content occupy separate measured rectangles.
- Gestures use transient previews and commit once at the correct boundary.
- Escape, focus loss, and tool changes cancel active transient gestures safely.
- Zoom preserves the beat or time beneath its anchor.
- Cull work outside the visible viewport.

Layer order:

```txt
workspace and lanes -> grid and ruler -> clips/regions -> gesture previews and
selection -> notes/automation overlays -> playhead -> handles and floating tools
-> menus/dialogs
```

## MIDI, automation, and audio editing

- Stable IDs survive edits, undo, clipboard operations, and persistence.
- Notes, points, fades, handles, and waveform features draw and hit-test through
  the same transform.
- Manual and automated values distinguish base value from effective value.
- Automation-follow updates never create user-command feedback loops.
- Muted or disabled musical data must not reach playback.
- Waveform width reflects effective duration and processing state.
- Processing controls expose the real backend, progress, failure, and cache
  state.
- High item counts require viewport culling, caching, batching, or custom GPU
  drawing rather than broad entity rerenders.

## Track headers, mixer, and inspector

Track headers and mixer strips are compact channel instruments, not cards.

- Keep widths stable and labels truncated.
- Isolate meter updates from parent layout/render work.
- Virtualize large vertical and horizontal collections.
- Keep mute, solo, arm, pan, gain, routing, sends, and inserts connected to the
  same state the engine hears.
- Pin master/global controls intentionally.
- The mixer has **two paint paths** — GPUI elements and the batched
  `mixer_render` painter. A `.rounded_*()` added to the GPUI strip does nothing
  under `FUTUREBOARD_MIXER_GPU=1` unless the snapshot carries the radius too.
  Change both, and test with the flag set.

Inspectors present the selected object's real properties: aligned labels,
tabular values, compact sections, vertical scrolling when needed, no fake
controls, no horizontal overflow.

## Built-in plugin editor signature

A built-in plugin may express its own instrument character, but it still belongs
to Futureboard. The editor may use React/Vite/Tailwind because it is compiled and
embedded as a plugin-specific static surface. Within that boundary:

- make the signal path and parameter hierarchy visually obvious;
- keep the most performance-relevant values scannable;
- use purposeful custom graphics for meters, curves, oscillators, envelopes;
- preserve consistent focus, hover, drag, reset, fine-adjust, disabled, and
  error behavior;
- use stable parameter IDs and reflect authoritative native values;
- avoid application navigation, browser conventions, remote content, and generic
  dashboard composition;
- fit the declared editor bounds and handle native resize/DPI correctly;
- render usefully before optional animation or analysis data arrives.

The native Studio frame owns window chrome, lifecycle, focus integration, asset
loading, and the CEF host. The embedded editor owns only its plugin content. Do
not reuse embedded Web UI components in the native shell.

## External plugin editors

External editors are plugin-owned native child views.

- Match the child view exactly to the measured client rectangle.
- Keep it out of titlebar and host chrome.
- Respect the plugin's resize capability and requests.
- Convert logical and physical coordinates explicitly at each platform boundary.
- Forward focus, keyboard, mouse, and IME behavior correctly.
- Detach the plugin view before destroying its parent.
- Never open, resize, or destroy editor windows from the audio thread.

## Performance and rendering

- Do not make the whole timeline, mixer, or track list rerender on playhead or
  meter ticks.
- `Colors::composite` and `Colors::latched` are control-path helpers. Resolve
  them into captured values; do not call them inside a per-frame paint loop.
- Cache waveforms and expensive analysis; do not regenerate them during render.
- Draw only visible items plus controlled overscan.
- Resize WGPU/custom surfaces only when measured dimensions change.
- Convert logical and physical pixels explicitly.
- Keep debug overlays and high-rate logs environment-gated.
- Keep render functions pure of scanning, filesystem, decoding, and project
  mutation.

## Accessibility and long-session comfort

- Preserve keyboard access and a visible focus ring.
- Provide text/tooltips for icon-only controls.
- Never rely on color alone for destructive, armed, muted, selected, or error
  states — this is what the two-channel rule enforces.
- Keep contrast readable without making inactive chrome visually loud.
- Avoid unnecessary animation; respect reduced-motion behavior where available.
- Keep repeated operations consistent so muscle memory stays reliable.

## Review checklist

- [ ] Radius, spacing, size, state and elevation come from tokens, not literals.
- [ ] The radius tier matches the control's height band.
- [ ] Nested elements use `radius::inner`; content quads use `radius::clamped`.
- [ ] Nothing on the must-stay-square list got rounded.
- [ ] Hover composites over the rest fill; focus is a ring; pressed is recessed.
- [ ] Latched states use their own hue on two channels.
- [ ] Visible state matches real project/runtime state.
- [ ] Drawing and hit-testing share coordinates.
- [ ] Long labels, empty state, errors, and disabled state are handled.
- [ ] Resizing, panels, scrolling, zoom, and DPI do not break geometry.
- [ ] High-frequency updates do not invalidate broad UI trees.
- [ ] Large track/channel/item counts have a bounded rendering strategy.
- [ ] Both mixer paint paths agree.
- [ ] `Light.json` got a real light value for every new token.
- [ ] Compile/test results and visual/runtime checks are reported separately.

## Final rule

Futureboard's signature is precision made visible. Every extension should feel
inevitable inside the existing Studio: compact, calm, musical, honest, and fast.
