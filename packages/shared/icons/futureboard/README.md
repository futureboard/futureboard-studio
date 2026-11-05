# Futureboard icons

Studio's own glyphs for its DAW vocabulary: transport, edit tools, track and
signal types, mixer and routing. General UI (chevrons, close, search, files,
panels) stays on Lucide.

These are original drawings. Cubase was studied for the *principles* a DAW
icon set needs at small sizes: weight, density, and solid masses over hairlines.
No glyph, outline or asset was taken from it or from any other product. Do not
add traced or converted glyphs from another icon font.

## Grid and rules

- **Canvas:** 16 × 16, `viewBox="0 0 16 16"`, with content inside a 1.5 px
  margin. GPUI rasterizes the SVG as a coverage mask and tints it from the
  theme, so every paint is `currentColor` and no file carries a final colour.
- **Masses** are solid fills. A 1 px round-join stroke of the same colour
  softens their corners by 0.5 px, the icon counterpart of Studio's control
  radius.
- **Lines** are 1.75 px with round caps and joins. Lenses and the snap magnet
  use 2.5 and 3 px, where the line itself is the mass.
- **Detail** is cut out of a mass (an SVG mask, 1.25 px), not drawn on top of
  it. That keeps the glyph one shape that reads at 14 px.
- **One idea per glyph.** The meaning a glyph carries in Studio decides its
  name, not the object it depicts: `snap`, not `magnet`; `bus`, not `merge`.

## Where they are used

`crates/SphereUIComponents/src/assets.rs` maps each file onto the virtual path
the old glyph used (`FUTUREBOARD_ICONS`), so call sites did not change:

| File | Replaces | Meaning in Studio |
| --- | --- | --- |
| play, pause, stop, record, skip-back | Lucide play, pause, square, circle, skip-back | Transport |
| cycle | Lucide repeat | Loop / cycle, rescan |
| metronome | Lucide metronome | Click |
| pointer, pencil, scissors, eraser | Lucide mouse-pointer, pencil, scissors, eraser | Edit tools |
| zoom-in, zoom-out | Lucide zoom-in, zoom-out | Zoom tools |
| snap | Lucide magnet | Snap to grid |
| mic, instrument, midi-note, waveform, video | Lucide mic, keyboard-music, music, audio-lines, film | Track and content types |
| bus, group, return | Lucide git-merge, git-fork, corner-down-left | Bus, group and return tracks |
| speaker, mute | Lucide volume-2, volume-x | Output, mute |
| mixer | Lucide sliders-horizontal | Mixer and settings |
| automation | Tabler automation | Automation |
| insert, routing | Lucide plug, route | Plug-in inserts, routing |
| freeze, power, mono, stack | Lucide snowflake, power, circle-dot, layers | Freeze, bypass, mono, presets |

To see the set as Studio draws it:

```bash
cargo run -p sphere_ui_components --features ui-preview --bin preview -- icons
```

`generate.py` (in this directory) regenerates every file from the coordinates in it.
