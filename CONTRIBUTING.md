# Contributing to Futureboard Studio

[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](#pull-requests)
[![Rust](https://img.shields.io/badge/Rust-2024-ce422b?logo=rust&logoColor=white)](https://rustup.rs)
[![GPUI](https://img.shields.io/badge/UI-GPUI-06b6d4)](https://www.gpui.rs)
[![Code Style](https://img.shields.io/badge/style-rustfmt%20%2B%20clippy-blue)](#validation)

Thanks for your interest in contributing. This guide covers the rules and
conventions specific to Futureboard Studio. By participating you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md).

> For setup, build and packaging commands, see the
> [README](README.md#getting-started). This guide assumes you can already build
> and run the app.

Read before you change code:

- **[DESIGN.md](DESIGN.md)** for anything that touches UI, layout, windows,
  panels, dialogs or plug-in editors.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** for how the crates fit together.
- **[AGENTS.md](AGENTS.md)** if you contribute with a coding agent — it points
  the agent at the same rules as this guide.

---

## What the product is

Futureboard Studio is the native Rust application in
[`apps/native/studio`](apps/native/studio): GPUI owns the shell, windows,
commands and state; the audio engine runs in process; plug-ins run in a
separate host process.

- **Target the native app.** New features and fixes land there.
- **Web technology is allowed in one place only:** a built-in plug-in's editor,
  under `crates/BuiltinAudioPlugins/crates/*/editor` or `editorui`. It is
  compiled to static assets, embedded in the binary and shown through CEF. It is
  a plug-in view, not a second application.
- **The general-purpose web app and the Electron wrapper are retired.** Do not
  add to, repair, port from or validate against them, and do not treat them as
  product or design authority. Touch them only for an agreed removal.

---

## Where things live

| Path | What |
| --- | --- |
| [`apps/native/studio`](apps/native/studio) | The app: startup, packaging hooks, edition wiring |
| [`crates/SphereUIComponents`](crates/SphereUIComponents) | The GPUI shell, editors, mixer, windows, project format, theme |
| [`crates/SphereDirectAudioEngine`](crates/SphereDirectAudioEngine) | Real-time engine: graph, transport, mixing, recording, export |
| [`crates/SpherePluginHost`](crates/SpherePluginHost) | Plug-in scanning, the out-of-process host, editor attach |
| [`crates/BuiltinAudioPlugins`](crates/BuiltinAudioPlugins) | Built-in plug-in DSP and their embedded editors |
| [`crates/SphereWebView`](crates/SphereWebView) | The CEF host the built-in editors run in |
| [`crates/gpui`](crates/gpui) | The GPUI fork the app is built on |
| [`packages/shared`](packages/shared) | Locales, themes, the menu source, fonts and icons |
| [`external`](external) | Vendored SDKs (submodules) and patched dependencies |
| [`xtask`](xtask) | Build and package orchestration |

---

## How to work

**Work in the smallest safe scope.**

1. Check `git status` first and leave unrelated changes alone.
2. Trace the real call path and who owns the state before you edit.
3. Know which kind of code you are touching: real-time callback, audio
   control, plug-in producer, UI, scanner/offline, build-time or test-only.
4. Make the smallest patch that fully connects the behavior — then stop. Do not
   rewrite neighbouring systems or finish an unrequested roadmap.
5. Reuse the existing models, commands, stores and components before adding
   new ones.

**Keep behavior real.**

- A control that looks active must drive real project or runtime state. If
  something is not finished, disable or label it rather than faking success.
- No mock runtime data outside tests.
- Preserve save/load and undo for every state change.
- Keep identifiers stable across the UI, the project file, the engine, the
  bridge and plug-in instances.
- Add dependencies only for a concrete need.
- Edition behavior goes through the existing verified edition/license
  provider; do not add parallel entitlement checks.

---

## Real-time rules

Audio callbacks run against a hard deadline; anything that can wait causes
dropouts, pops and clicks.

> [!CAUTION]
> **Real-time and producer-hot paths must never:**
>
> - allocate, free or grow a buffer;
> - lock a mutex, wait, sleep, or use an unbounded queue;
> - do filesystem, network, scanning or serialization work;
> - parse JSON or look anything up by string per block;
> - log or format strings;
> - touch UI state;
> - panic, or unwind across FFI.

Use preallocated buffers, immutable snapshots, compact IDs resolved before
playback, atomics, bounded lock-free queues, and diagnostics rings drained off
the real-time thread.

Do not hide an xrun with a debounce, a sleep, a wider stale window or a removed
freshness guard. Trace the request, the wake, the processing and the response,
and fix the stage that is slow or wrongly synchronized.

---

## Plug-ins

For every hosted or bridged plug-in:

- Route MIDI, parameters, state and responses to the exact plug-in instance.
- Keep the freshness and sequence guards.
- Persist component and controller state as opaque data, and restore it before
  playback or editor use.
- Derive tempo, position, play state and time signature from the real
  transport — never hardcode them outside tests.
- Carry automation from the project through the engine and the bridge into the
  plug-in.
- Include plug-in and bridge latency in delay compensation.
- Keep scanning and editor lifecycle off the audio thread, and never open,
  resize or destroy an editor window from it.

External editors are plug-in-owned native child views: match the measured
client rectangle, handle DPI and resize requests, forward focus and input, and
detach the child before destroying its parent.

---

## Native UI

[DESIGN.md](DESIGN.md) is the visual and interaction contract. In short:

- Use the semantic tokens in `crates/SphereUIComponents/src/theme.rs` — colors,
  radius, spacing, sizes, state layers, typography. No raw literals.
- Use the shared controls in `components/controls.rs` before writing a one-off.
- Give every region one layout owner, one scroll owner and one clip owner.
- Draw and hit-test through the same coordinate transform.
- Keep per-frame visuals (playhead, meters) out of broad entity re-renders.
- Keep render functions free of filesystem work, decoding and project mutation.
- New colour tokens need a real value in every theme, including `Light.json`.

---

## Built-in plug-in editors

- Rust is the authority for parameter IDs, ranges, defaults, normalization,
  DSP, state and persistence; the editor is a thin view over the parameter
  bridge.
- Build to deterministic static assets. No dev server, CDN or network access
  at runtime.
- The custom scheme, asset lookup and CEF lifecycle stay native-owned.
- Keep the editor's React/Tailwind conventions inside the editor — they do not
  belong in the GPUI app chrome.

Each editor has its own package scripts, for example:

```bash
bun run --cwd crates/BuiltinAudioPlugins/crates/rodharerist/editorui build
bun run build:plugin-editors   # every editor
```

---

## Project files

Projects use a versioned binary format
(`crates/SphereUIComponents/src/project/format.rs`).

- A change to what is saved bumps `PROJECT_VERSION` and documents the new
  version next to it.
- Older files must keep loading: read new fields only from the version that
  wrote them, with defaults that reproduce how an older project sounded.
- Add a round-trip test for the new data and a test that an older version
  still decodes.

---

## Translations and menus

- User-facing strings come from the Fluent catalogs in
  `packages/shared/locales/<locale>/app.ftl`. A new key goes into every locale
  file; English is the source.
- The menu is defined once in `packages/shared/src/menu/menuItems.ts`. After
  editing it, regenerate the native menu with
  `bun run scripts/sync-shared-menu.mjs`. A test fails when a menu label is
  missing from any locale.
- Translations themselves are contributed through
  [Crowdin](https://crowdin.com/project/futureboard-studio); see the
  [translation guide](packages/shared/locales/translation.md).

---

## Code style

- **Rust** — edition 2024, `rustfmt` and `clippy` clean. Isolate and document
  `unsafe` and FFI invariants; return `Result` instead of panicking, and never
  panic across FFI. Avoid mutable global state.
- **C++** — keep third-party SDKs behind a minimal C ABI so SDK headers never
  reach the Rust engine, and manage plug-in lifecycles defensively.
- **TypeScript** (plug-in editors only) — explicit types, no `any`; UI code
  talks to native only through the editor bridge.
- **Line endings** — the repository uses LF. Configure your editor so a save
  does not rewrite a file to CRLF.

---

## Validation

Run the checks for what you touched, narrowest first:

```bash
cargo fmt --all -- --check
cargo check -p futureboard_native
cargo check -p sphere_ui_components
cargo test  -p sphere_ui_components --lib
cargo test  -p sphere_directaudioengine --lib
cargo check -p sphere-plugin-host
cargo test  -p sphere-plugin-host
cargo check -p BuiltinAudioPlugins
cargo clippy --workspace -- -D warnings
```

Broaden to `cargo check --workspace` / `cargo test --workspace` when a change
crosses crate boundaries. A runnable debug build also needs the helper
binaries beside the app:

```bash
cargo build -p futureboard_native
cargo build -p sphere-plugin-host --bins
```

A passing compile is not proof that a gesture, audio output, an editor, window
lifecycle, project restore or a layout works. Exercise those paths when your
change touches them.

---

## Pull requests

- One focused pull request per feature, fix or refactor.
- Present-tense, imperative commit subjects — `Fix VST3 editor child
  parenting` — naming the crate in the body when the scope is not obvious.
  Squash noisy work-in-progress commits before review.
- In the description, report separately:
  - what changed and which files or crates;
  - the commands you ran and their results;
  - what you checked by hand at runtime or visually — and what you did not.

  Never describe a check as done when it was not run.
- UI changes include screenshots, ideally at normal, narrow and high-DPI sizes.
