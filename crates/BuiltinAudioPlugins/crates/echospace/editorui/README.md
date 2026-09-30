# EchoSpace editor

Embedded editor UI for the `echospace` built-in plugin. Svelte 5 + Vite,
bundled to a single self-contained `dist/index.html` that `build.rs` embeds into
the plugin library and the native CEF host serves at
`mikoplugin://echospace/index.html`.

This is a plugin view, not an application. It has no router, no dev server
dependency at runtime, and no network access — a strict single-file bundle is
the deliverable. It shares its control language and palette — Futureboard's
default theme — with the VerbSpace editor; only the display and the parameter
set differ.

## Commands

```bash
bun install
bun run build      # svelte-check, then the embedded single-file bundle
bun test           # schema/model checks (also run in the Rust-side workflow)
bun run dev        # standalone preview; the bridge no-ops without a host
```

After `bun run build`, rebuild the crate so the new assets are embedded:

```bash
cargo build -p echospace
```

## Where authority lives

Rust owns everything that decides what a value *means*:

| Concern | Owner |
| --- | --- |
| parameter ids and wire indices | `../src/ipc.rs` (`UI_PARAM_IDS`) |
| ranges, clamping, defaults | `../src/ipc.rs`, `../src/lib.rs` |
| DSP, persistence, state blob | `../src/lib.rs`, `../src/ipc.rs` |
| taper, layout, formatting | `src/params.ts` |

`src/params.ts` and `src/model.ts` duplicate a few Rust constants so the UI can
lay out a control and draw the echo display. `src/params.test.ts` and
`src/model.test.ts` parse the real `.rs` files and compare, so a change on
either side that is not mirrored fails the tests rather than shipping a UI that
quietly disagrees with the DSP.

## The echo display

`EchoView` draws the repeat pattern the current parameters produce: each round
trip as a bar on its channel's lane, on a decibel axis, with the feedback
envelope behind it and the tap times marked. Three things it gets from the real
model rather than from decoration:

- **Ping-pong alternation.** The DSP swaps the two feedback paths every round
  trip, so a line's odd passes come back on the opposite side — the bars follow
  that, and mono collapses both lanes onto the left tap.
- **Loop gain.** Cross-feed is normalised in the DSP so it changes *where* the
  repeats go, not how loud the loop is. The display uses the same rule.
- **Per-pass dulling.** Each repeat runs through the low and high cut once
  more, so successive bars are pulled toward grey by the magnitude those
  2-pole sections actually have at a reference tone.

It is a model computed from parameters, labelled as such in the UI, not
measured audio; no analyser feed exists for this plugin.

## Bridge

`src/bridge.ts` speaks the shared built-in editor protocol: the host pushes
`futureboard.selectInstance` with the authoritative state, the page answers
`futureboard.instanceReady`, and gestures go back as `futureboard.setParams`
batched per animation frame and tagged with the binding they were made against.
Stale batches are dropped by the host, so an edit made against a torn-down
instance can never land on its replacement.
