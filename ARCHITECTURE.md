# Architecture

> Status: **pre-alpha**. This document describes the intended shape of the
> project and how its parts relate today. Names, boundaries, and crate
> membership are still moving.

This file is the high-level map. For per-crate detail, read the crate READMEs;
for contribution rules, see [CONTRIBUTING.md](CONTRIBUTING.md) and
[DESIGN.md](DESIGN.md).

---

## Surfaces

Futureboard Studio is the native app. LiveStage, the live console, is a
separate native app built on the same engine.

### Futureboard Studio — native

- **Location:** [`apps/native`](apps/native) + the GPUI UI kit in
  [`crates/SphereUIComponents`](crates/SphereUIComponents).
- **Stack:** Rust + [GPUI](https://www.gpui.rs) (the rendering framework behind
  the Zed editor), driving the in-process Rust audio engine directly.
- **Role:** This *is* Futureboard Studio. It is the canonical UI, the surface
  new features land on first, and the only one with first-class access to the
  native audio engine and plugin host. When this document says "the app"
  unqualified, it means the native app.

The executable `FutureboardNative` is a launcher: the Studio itself is the
`FutureboardEngine_x64` library (`cdylib` of `apps/native/studio`) it loads
from its own directory.

### LiveStage

- **Location:** [`apps/native/livestage`](apps/native/livestage), with its web
  console in `apps/native/livestage/webui`, embedded through `builtin_ui_embed`.
- **Role:** A live mixing console on the same engine and plugin host.

### CEF runtime

[`crates/SphereWebView`](crates/SphereWebView) hosts Chromium Embedded
Framework, staged beside the app by `xtask`. Built-in plugin editors are
native GPUI views today, so no editor needs it; the runtime is kept on
purpose.

The Electron app, the browser (Lite) app, the collaboration server and the
N-API bindings have been removed.

---

## Audio engine ownership

Audio is owned by Rust:

| Engine | Crate | Owns | Used by |
|---|---|---|---|
| **Direct audio engine (DAUx)** | [`crates/SphereDirectAudioEngine`](crates/SphereDirectAudioEngine) | The realtime graph, transport, mixer, device I/O (WASAPI professional + MMCSS / CoreAudio / ALSA via `cpal`), and the realtime render thread. | The native app and LiveStage. |

Rules of ownership:

- **The engine owns realtime state.** UI code never mutates DSP state on the
  audio thread. Inserts are owned by the engine as `RuntimeInsert { kind,
  params, dsp }`; the UI sends intent, the engine applies it. See
  [`crates/SphereAudioPlugins`](crates/SphereAudioPlugins) for the realtime-safe
  DSP contract (no allocation, locks, I/O, or logging on the render path).
- **Native plugins are hosted, not embedded ad hoc.**
  [`crates/SpherePluginHost`](crates/SpherePluginHost) wraps the C++ VST3/CLAP
  SDKs (`external/vst3sdk`, `external/clap`) and embeds the native editor inside
  the GPUI window. This path is native-only.
> Stabilization note: the audio engine is treated as load-bearing and is **not**
> rewritten as part of routine cleanup. Changes to realtime code are deliberate
> and reviewed against the realtime-safety rules above.

---

## Crate map (`crates/`)

Active, workspace-member crates:

- **SphereDirectAudioEngine** — native realtime audio engine (DAUx). *(see above)*
- **SphereUIComponents** — GPUI/Skia native UI kit and CoreUI for the native app.
- **SpherePluginHost** — VST3/CLAP host wrapper, native editor embedding.
- **SphereAudioPlugins** — built-in realtime-safe stock DSP (EQ, comp, delay…).
- **SphereAudioEditor** — audio editing consumed by the native app.

---

## Experimental crates policy

Some crates under `crates/` are **placeholders** — a name and a `main.rs` stub
with no real implementation yet (today: `SphereVoiceTune`). To keep the build,
CI, and dependency
graph honest, these are **quarantined**: listed under `exclude` in the workspace
[`Cargo.toml`](Cargo.toml) rather than built as members.

What this means in practice:

- A quarantined crate is **not** compiled, linted, or tested by CI, and does not
  appear in `Cargo.lock`. Its directory stays in the tree as a reserved name and
  a place to start.
- **Do not** add a placeholder crate to the workspace members until it has real,
  compiling, tested content. Promote it by removing its line from the `exclude`
  list (and running `cargo build` to relock).
- **Do not** let other crates depend on a quarantined crate — by definition it
  has nothing to offer yet.
- New experimental work should either start quarantined (stub + `exclude` entry)
  or live on a branch until it builds and has at least a smoke test. The default
  is: if it is in the workspace, it must pass `fmt`, `clippy -D warnings`, and
  `cargo test`.

This policy keeps the green-CI surface equal to the *maintained* surface, which
is the point of the stabilization pass that introduced it.

---

## CI gates

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) enforces correctness on
every push and pull request (release/packaging stays in `build-desktop.yml` and
`daily-build.yml`):

- **Rust format** — `cargo fmt --all --check`.
- **Rust clippy + tests** — `cargo clippy --workspace --all-targets -- -D
  warnings` and `cargo test --workspace` (with audio SDK submodules and Linux
  system deps initialized).

Because quarantined crates are excluded from the workspace, these gates cover
exactly the maintained code and nothing that is known-incomplete.
