# Futureboard Visualizer

A standalone C++ meter and analysis display for Futureboard Studio, drawn on
the lcdemu virtual LCD (SDL3). It reads Studio's **visualizer link**, a
shared-memory region Studio publishes while the app runs:

- transport: position, tempo, time signature, play / record / loop
- every track's post-fader peak and RMS, fader, pan, mute / solo / arm,
  selection, folder depth, colour (up to 512 tracks, mixer order)
- master and Control Room levels
- the master's post-fader audio
- one track's post-fader audio, the track the app asks for

The protocol and its Rust side live in [`crates/VisualizerLink`](../../../crates/VisualizerLink/src/lib.rs);
Studio publishes from `crates/SphereUIComponents/src/visualizer_link.rs`. Studio
does no work for the link — no audio copies, no table — until the app is
reading, and it stops again about two seconds after the app quits.

## Tabs

| Key | Tab | Shows |
| --- | --- | --- |
| 1 | Meters | every track's meter, master at the right; `L` switches bridge ↔ named list |
| 2 | Mixer | strips: name, M/S/R, meter, fader, dB, peak hold, pan; master strip |
| 3 | Loudness | master EBU R128: momentary, short-term, integrated, LRA, peaks, history |
| 4 | Spectrum | master spectrum, 256 log bands |
| 5 | Stereo | master vectorscope, correlation, width, balance |
| 6 | Scope | master oscilloscope, triggered |
| 7 | Overview | master loudness, spectrum, imager and scope together |
| 8 | Track | the same four for the watched track |
| 9 | Transport | bar.beat, time, BPM, signature, lamps, master and Control Room |

The **watched track** follows the track selected in Studio. `Up` / `Down` or
clicking a meter or strip picks another one (and stops following); `F` follows
Studio again.

On Windows the app starts **docked along the top of the desktop**, as lcdemu's
mixviz did: a full-width, always-on-top strip the shell reserves, so maximised
windows stay below it. `B` undocks it into a normal window and docks it again;
`--window` starts undocked. The strip is 150 px tall by default;
`Ctrl+Up` / `Ctrl+Down` step it through 128, 150, 200, 240 and 320 px
(128 px is one screen pixel per LCD row, the smallest that shows every row),
and `--height PX` sets any height from 128. The height, tab and theme are
remembered in `%APPDATA%\Futureboard\Visualizer\settings.txt`. On the wide
strip the Overview and Track tabs put loudness, spectrum, imager and scope
side by side.

Other keys: `Tab` / `Shift+Tab` cycle tabs (or click the header's left / right
half), `Left` / `Right` or the wheel page through tracks, `R` resets loudness,
`S` spectrum tilt, `A` imager gain, `[` `]` scope time, `T` theme, `G` pixel
grid, `H` ghosting, `+` `-` backlight, `Space` hold, `P` save a BMP, `F11` full
screen (undocked), `F1` help, `Esc` quit.

Options: `--tab N`, `--list`, `--watch N` (watch track N), `--window`,
`--height PX`, `--size W H` (undocked window size), `--theme N`, `--grid`,
`--snap FILE.bmp [--snap-after S]` (render hidden and undocked, save the LCD,
exit), `--link NAME` (another region name).

## Build

CMake ≥ 3.24 and a C++20 compiler. SDL3 comes from an installed package or is
built from source; for an offline build point FetchContent at a checkout:

```powershell
cmake -S apps/native/visualizer -B target/visualizer-build -A x64 -DFETCHCONTENT_SOURCE_DIR_SDL3=W:/sources/SDL3
cmake --build target/visualizer-build --config Release
```

Outputs `FutureboardVisualizer.exe` and `FutureboardVisualizerProbe.exe` in
`target/visualizer-build/Release`.

`cargo package-release` / `package-dev` build it the same way (in
`target/visualizer/<profile>`) and stage `FutureboardVisualizer.exe` beside
the Studio on native Windows and Linux packages; set `SDL3_SOURCE_DIR` for an
offline first configure, or pass `--no-visualizer` to leave it out. Linux builds use POSIX shared memory
(`shm_open`); the desktop toolbar mode is Windows only.

## Check without Studio

`cargo run -p visualizer_link --example demo` publishes a synthetic session
(sixteen tracks, a playing transport, master and per-track tones). Then:

```powershell
target/visualizer-build/Release/FutureboardVisualizerProbe.exe
```

prints the frame, the track table and audio counts, asks for one track's
audio, and exits non-zero when any part of the link does not work. The same
probe works against a running Studio.

## Layout

- `lcd/` — lcdemu's `lcdcore` (Canvas, 5×7 font, LcdRenderer), unchanged
- `src/mixviz/` — lcdemu's mixviz DSP and views (loudness, spectrum, imager,
  scope) and the desktop toolbar, unchanged
- `src/link/` — the link reader; its structs mirror the Rust layout with
  `static_assert`ed offsets
- `src/Analysis.*` — one signal's analysis chain (master, watched track)
- `src/MeterViews.*` — meters, mixer and transport tabs
- `src/main.cpp` — window, tabs, input, link connection
- `src/probe.cpp` — console link check
