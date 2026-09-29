# Imager editor

Embedded CEF editor for the built-in Imager (four-band stereo width). React +
Vite + Tailwind, bundled to a single self-contained `dist/index.html` that
`build.rs` embeds into the plugin library and the native host serves at
`mikoplugin://imager/index.html`. It follows the same visual language and
palette as EQUZ8: Futureboard's default theme tokens, Mona Sans, Phosphor
icons, the shared preset popover.

```bash
bun install          # from the repo root — this is a workspace package
bun run build        # tsc, then the embedded single-file bundle
bun run test         # checks the editor's copy of the Rust constants
```

After `bun run build`, rebuild the crate so the new bundle is embedded.

## Where authority lives

| Concern | Owner |
| --- | --- |
| parameter ids, wire order, ranges, clamping | `../src/ipc.rs` |
| defaults, DSP, telemetry | `../src/lib.rs` |
| layout, formatting, presets | `src/lib/params.ts`, `src/lib/presets.ts` |

`tests/params.test.ts` reads the real `.rs` files and compares, so a change on
either side that is not mirrored fails the test instead of shipping an editor
that quietly disagrees with the DSP.

## What the editor draws

- **Band display** — the four bands on a log-frequency axis over the input
  spectrum (`futureboard.spectrum`, measured before the DSP). Each band's
  shape height is its width: a hairline is mono, the dashed guides are 100 %,
  full height is 200 %. Drag a band vertically for width, a crossover
  sideways to move it; double-click resets either.
- **Vectorscope** — the plugin's *output*, mid up and side across, from the
  decimated samples the DSP publishes in `futureboard.stereoImage`. The scope
  scales itself to the signal and says by how much.
- **Correlation** — overall and per band, from the same message. A band too
  quiet to measure reads "No signal" rather than a misleading 0.
- **In/Out levels** — from `futureboard.meters`.

Everything is measured by the DSP; nothing in the editor is simulated.
