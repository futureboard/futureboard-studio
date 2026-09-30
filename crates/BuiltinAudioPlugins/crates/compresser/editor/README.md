# Compressor editor

Embedded CEF editor for the built-in Compressor (single-band and four-band).
React + Vite + Tailwind, bundled to a single self-contained `dist/index.html`
that `build.rs` embeds into the plugin library and the native host serves at
`mikoplugin://compresser/index.html`. Same visual language and palette as
Imager and EQUZ8: Futureboard's default theme tokens, Mona Sans, Phosphor
icons.

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
| defaults, gain curve, DSP, telemetry | `../src/lib.rs` |
| layout, formatting | `src/lib/params.ts` |

`tests/params.test.ts` reads the real `.rs` files and compares — ids, ranges,
every default, and the curve at the points the Rust tests pin — so a change on
either side that is not mirrored fails the test instead of shipping an editor
that quietly disagrees with the DSP.

## What the editor draws

- **Mode** — Single runs one stage over the whole signal; Multi splits it at
  three crossovers and runs one stage per band.
- **Transfer curve** (Single) — the static input→output curve with the knee
  shaded. The threshold is a handle: drag it sideways, arrow keys to nudge,
  double-click to reset. The dot is the live operating point: the input peak
  from `futureboard.meters` against that peak less the reduction the DSP is
  applying right now.
- **Band display** (Multi) — the bands on a log-frequency axis over the input
  spectrum (`futureboard.spectrum`, measured before the DSP). Each band's
  reduction hangs from the top of its region, from
  `futureboard.bandReduction`. Drag a crossover sideways to move it.
- **Meters** — overall reduction (the single stage's, or the largest band's)
  and in/out levels, from `futureboard.meters`; each band card has its own
  reduction meter.

Everything is measured by the DSP; nothing in the editor is simulated.
