# Futureboard Studio Agent Rules

Before changing code, read `DESIGN.md` when the task touches UI, layout,
interaction, windows, panels, dialogs, or plugin editors — then only the
smallest relevant implementation area.

`AGENTS.md` and `GEMINI.md` delegate here so every coding agent follows the
same contract.

## Product scope

Futureboard Studio is the native Rust/GPUI application:

- app: `apps/native/studio`
- binary: `FutureboardNative` (launcher only; loads the engine library)
- engine library: `FutureboardEngine_x64` (`cdylib` of the same package; all
  UI and audio code runs here — `cargo run` alone does not rebuild it)
- package: `futureboard_native`
- native UI: `crates/SphereUIComponents`
- audio engine: `crates/SphereDirectAudioEngine`
- plugin host and scanner: `crates/SpherePluginHost`
- built-in plugins: `crates/BuiltinAudioPlugins`

Electron, N-API bindings and the general-purpose Web UI have been removed. Do
not reintroduce them unless the user explicitly asks.

Built-in plugin editors are native GPUI views (`plugin_kit`). The CEF runtime
(`crates/SphereWebView`, `apps/native/cef_helper`, xtask's CEF staging) is
kept on purpose even though no built-in editor uses it today: do not remove
it. LiveStage's web console (`apps/native/livestage/webui`, embedded through
`builtin_ui_embed`) is the only web front end.

## Operating contract

Work in the smallest safe scope.

Before editing:

1. Inspect `git status` and preserve unrelated work.
2. Trace the real call path and current state ownership.
3. Classify the changed code: realtime, audio control, plugin producer, UI,
   scanner/offline, build-time, or test-only.
4. Identify the smallest complete patch and its validation target.
5. Reuse current abstractions and visual language.

While editing:

- Do not rewrite adjacent systems or complete an unrequested roadmap.
- Connect controls to real state and runtime behavior. Do not add fake
  production behavior, disconnected controls, or mock runtime data; disable or
  label incomplete behavior instead of faking success.
- Preserve save/load and undo semantics when state changes, and keep stable
  identifiers across UI, project persistence, engine, bridge, and plugin
  instances.
- Do not add dependencies without a concrete need.
- Keep edition behavior behind the existing verified edition/license provider;
  do not add parallel entitlement checks.
- Preserve project compatibility and opaque plugin state.
- Keep expensive diagnostics gated and off realtime threads.

When finishing:

- Run the smallest relevant check first.
- Separate compile/test evidence from manual/runtime/visual evidence. A
  successful compile is not proof that gestures, audio output, editor
  embedding, window lifecycle, project restore, or layout work at runtime.
- Report the behavior delivered, changed files, commands run with their exact
  outcomes, checks actually performed, and anything unverified or blocked.
- Never claim a check or gesture was tested when it was not.

## Realtime and bridge rules

Realtime and producer-hot paths must not perform steady-state heap allocation,
filesystem I/O, JSON/string-map lookup, logging, sleeps, blocking locks,
unbounded queue operations, UI work, or panics across FFI.

Use preallocated buffers, compact resolved identifiers, immutable snapshots,
atomics, bounded SPSC/lock-free queues, event-driven wakeups, and diagnostics
rings drained off the hot path. Do not mask xruns with debounce, sleeps, larger
stale windows, or removed freshness guards: find the slow or mis-synchronized
stage.

For hosted and bridged plugins:

- Route MIDI, parameters, state, and responses by the exact plugin instance.
- Keep freshness/sequence guards; never hide stale output by removing them.
- Persist component and controller state as opaque data where available, and
  restore it after instantiation, before playback or editor use.
- Supply transport-derived process context; do not hardcode tempo, position,
  playing state, or time signature outside explicit tests.
- Carry automation from UI/project state through the engine and bridge into the
  target plugin.
- Include plugin and bridged latency in graph delay compensation.
- Keep scanning and editor lifecycle off the audio thread, and isolate plugin
  failure from the Studio process wherever the architecture allows.

## Native UI rules

- GPUI owns the application shell, commands, focus, state, and native windows.
- Reuse shared components and semantic theme tokens.
- Keep one layout owner, one scroll owner, and one clip owner for each region.
- Use the same coordinate transform for ruler, grid, clips, notes, automation,
  playhead, hit-testing, and overlays.
- Virtualize or cull large track, mixer, MIDI, waveform, and automation views.
- Keep per-frame visuals isolated from broad entity rerenders.
- Defer parent mutations when a child callback would create nested GPUI entity
  updates.
- Treat external plugin editors as plugin-owned native child views with exact
  client bounds, DPI handling, focus forwarding, resize, and teardown; detach
  the child before destroying its parent.

For visual direction and interaction details, follow `DESIGN.md`.

## Validation

Choose only commands relevant to the touched scope:

```bash
cargo fmt --all -- --check
cargo check -p futureboard_native
cargo check -p sphere-plugin-host
cargo test -p sphere-plugin-host
cargo check --workspace
```

A runnable native build also needs the helper binaries next to the app; the
packager builds everything together:

```bash
cargo build -p futureboard_native
cargo build -p sphere-plugin-host --bins
cargo package-dev
```

Use `README.md` and `CONTRIBUTING.md` for broader supported commands.

## Final rule

Ship the smallest complete native change: real state, realtime-safe behavior,
Futureboard-consistent UI, and evidence that matches what was actually tested.
