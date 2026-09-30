# BurnLimit editor

Embedded CEF editor for the built-in BurnLimit (lookahead brickwall limiter). React + Vite +
Tailwind, bundled to a single self-contained `dist/index.html` that is
embedded into the plugin library and served at
`mikoplugin://burnlimit/index.html`. It shares its layout and components with
the other dynamics editors (BurnLimit, 67Clipper, Transient) and the tokens of
every built-in editor: Futureboard's default theme, Mona Sans, Phosphor icons.

```bash
bun install          # from the repo root — this is a workspace package
bun run dev          # browser preview with a simulated host (see below)
bun run build        # tsc, then the embedded single-file bundle
bun run test         # checks the editor's copy of the Rust constants
```

`cargo build -p burnlimit` runs a frozen `bun install` and builds this editor
into Cargo's `OUT_DIR` itself, so a clean checkout needs no prebuilt `dist/`.

## Debugging in a browser

`bun run dev` (or the `burnlimit-editorui` entry in `.claude/launch.json`) serves the
editor to an ordinary browser. There is no Futureboard host behind it, so
`src/dev/previewHost.ts` stands in: it answers `bridgeReady` with a
`selectInstance`, takes the page's `setParams`, and posts `meters` at ~30 Hz
from a synthetic drum groove run through a rough model of the plugin
(`src/dev/simulation.ts`). The header says **Browser preview · simulated
signal** while it runs.

None of it reaches the embedded editor: `main.tsx` imports the preview host
only under `import.meta.env.DEV`, which the production build compiles to
`false`, and `bridge.ts` only hands messages to it on that same condition.
To debug the real embedded page inside Futureboard instead, start the app with
`FUTUREBOARD_PLUGIN_VIEW_DEBUG=1` and open `http://127.0.0.1:9222` in a browser:
the CEF host then exposes Chromium's remote-debugging endpoint.

## Where authority lives

| Concern | Owner |
| --- | --- |
| parameter ids, wire order, ranges, clamping | `../src/ipc.rs` |
| defaults, style order, DSP, telemetry | `../src/lib.rs` |
| layout, formatting, presets | `src/lib/params.ts`, `src/lib/presets.ts` |

`tests/params.test.ts` reads the real `.rs` files and compares, so a change on
either side that is not mirrored fails the test instead of shipping an editor
that quietly disagrees with the DSP.

## What the editor draws

- **Stage** — the last ten seconds of the insert's own meter frames: input
  peak as the grey body, output peak as the bright line, and the gain reduction hanging
  from the 0 dB line on the same scale. The ceiling is
  drawn across it.
- **Meters** — In, GR and Out, with the loudest of the last ~1.2 s held
  as the readout. Both read from the same history ring, so every frame the
  host sent counts, however slowly the page paints.

Everything shown comes from `futureboard.meters`; nothing in the embedded
editor is simulated.
