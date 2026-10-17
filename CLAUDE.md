# Futureboard Studio Agent Rules

Before changing code, read:

1. `SKILL.md` for repository workflow and engineering constraints.
2. `DESIGN.md` when the task touches UI, layout, interaction, windows, panels, dialogs, or plugin editors.
3. Only the smallest relevant implementation area after that.

`AGENTS.md` delegates here so every coding agent follows the same contract.

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

Electron and the general-purpose Web UI are retired. Do not implement, repair,
port from, validate, or use them as product/design authority unless the user
explicitly asks for legacy removal or archaeology.

The only active Web UI scope is a built-in plugin's embedded editor. Those
editors live under `crates/BuiltinAudioPlugins/crates/*/editor` or `editorui`,
are compiled to static assets, embedded by `builtin_ui_embed`, and hosted by
the native app through `SphereWebView`/CEF. They are plugin views, not a second
Futureboard application.

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
- Do not add fake production behavior, disconnected controls, or mock runtime
  data.
- Do not add dependencies without a concrete need.
- Keep edition behavior behind the existing verified edition/license provider.
- Preserve project compatibility and opaque plugin state.
- Keep native and embedded-plugin-Web-UI boundaries explicit.

When finishing:

- Run the smallest relevant check first.
- Separate compile/test evidence from manual/runtime/visual evidence.
- Report changed files, commands run, failures, and remaining validation.
- Never claim a check or gesture was tested when it was not.

## Realtime and bridge rules

Realtime and producer-hot paths must not perform steady-state heap allocation,
filesystem I/O, JSON/string-map lookup, logging, sleeps, blocking locks,
unbounded queue operations, UI work, or panics across FFI.

Use preallocated buffers, compact resolved identifiers, immutable snapshots,
atomics, bounded SPSC/lock-free queues, event-driven wakeups, and diagnostics
rings drained off the hot path.

For bridged plugins:

- Route MIDI, parameters, state, and responses by the exact plugin instance.
- Keep freshness/sequence guards; never hide stale output by removing them.
- Persist component and controller state as opaque data where available.
- Supply transport-derived process context; do not hardcode tempo, position,
  playing state, or time signature outside explicit tests.
- Carry automation from UI/project state through the engine and bridge into the
  target plugin.
- Include bridged latency in graph delay compensation.

## Native UI rules

- GPUI owns the application shell, commands, focus, state, and native windows.
- Reuse shared components and semantic theme tokens.
- Keep one layout owner, one scroll owner, and one clip owner for each region.
- Use the same coordinate transform for ruler, grid, clips, notes, automation,
  playhead, hit-testing, and overlays.
- Keep per-frame visuals isolated from broad entity rerenders.
- Defer parent mutations when a child callback would create nested GPUI entity
  updates.
- Treat external plugin editors as plugin-owned native child views with exact
  client bounds, DPI handling, focus forwarding, resize, and teardown.

For visual direction and interaction details, follow `DESIGN.md`.

## Built-in plugin editor Web UI

Web technologies are allowed only inside a built-in plugin editor bundle.

- Keep DSP, parameter schema, defaults, normalization, and persistence in Rust.
- Keep the editor a thin view over the real parameter bridge.
- Use stable parameter IDs shared with the Rust core.
- Build to deterministic static assets; do not depend on a dev server or remote
  network content at runtime.
- Keep the custom scheme/asset lookup and CEF lifecycle native-owned.
- Do not import plugin editor React/Tailwind conventions into GPUI app chrome.
- Do not turn the embedded editor exception into a general Web UI platform.

## Validation

Choose only commands relevant to the touched scope:

```bash
cargo fmt --all -- --check
cargo check -p futureboard_native
cargo check -p sphere_ui_components
cargo check -p sphere-plugin-host
cargo test -p sphere-plugin-host
cargo check -p BuiltinAudioPlugins
cargo check --workspace
```

For an embedded plugin editor, run its own package scripts, for example:

```bash
bun run --cwd crates/BuiltinAudioPlugins/crates/rodharerist/editorui build
```

A runnable native debug build also needs the helper binaries next to the app:

```bash
cargo build -p futureboard_native
cargo build -p sphere-plugin-host --bins
```

Use `README.md` and `CONTRIBUTING.md` for broader supported commands. Do not run
retired Electron/Web commands as routine validation.

## Final rule

Ship the smallest complete native change: real state, realtime-safe behavior,
Futureboard-consistent UI, and evidence that matches what was actually tested.
