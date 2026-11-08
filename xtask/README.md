# xtask — build chaining & binary packaging

`xtask` is the Futureboard workspace task runner. It has two jobs:

- **`build-all` / `check-all`** — chain the per-edition cargo aliases
  (`build-ce`, `build-professional-win`, …) from `.cargo/config.toml`, because
  Cargo aliases cannot chain commands. The Windows-only Professional alias is
  skipped automatically on non-Windows hosts unless a Windows target is passed.
- **`package`** — build `FutureboardNative` and stage a clean, runnable
  application tree into `out/`, kept separate from the Cargo `target/` cache.

## `target/` vs `out/`

| Directory | Purpose | Committed? |
| --------- | ------- | ---------- |
| `target/` | Cargo compiler cache and intermediate output (`.pdb`, `.rlib`, `deps/`, `incremental/`, the linked binary). Owned by Cargo. | No |
| `out/`    | Distributable application layout — only the files a user needs to run the app. Produced by `xtask package`. | No |

`out/` and `target/` are both git-ignored. Packaging **never** copies the Cargo
target tree wholesale; it copies only the executable, known runtime sibling
libraries, the generated `Plugins/`/`Resources/` directories, and
`build-info.json`.

## Why packaging is not in `build.rs`

`build.rs` runs inside *every* compilation and must stay hermetic and fast.
Packaging is an explicit, post-build workflow that copies files, writes
metadata, validates the result, and atomically publishes it. Mixing that into
`build.rs` would run it on every incremental compile and couple the compiler
cache to the distributable layout. Packaging therefore lives in `xtask`.

## Output layout

```text
out/
├─ dev/
│  └─ windows-x64/                    # cargo package-dev
│     ├─ FutureboardNative.exe
│     ├─ FutureboardPluginHostX64.exe # out-of-process plugin/editor host (spawned by the app)
│     ├─ FutureboardPluginScanner.exe # isolated plugin scanner (spawned by the app)
│     ├─ libcef.dll                   # shared CEF runtime, staged FLAT (never a CEF/ subdir)
│     ├─ chrome_elf.dll
│     ├─ icudtl.dat
│     ├─ resources.pak
│     ├─ chrome_100_percent.pak
│     ├─ chrome_200_percent.pak
│     ├─ v8_context_snapshot.bin
│     ├─ locales/                     # the one CEF subdirectory Chromium requires
│     ├─ onnxruntime.dll              # staged only if present beside the binary
│     ├─ crashpad_handler.exe         # Crashpad handler bundled with Studio
│     ├─ Plugins/                     # Built-in Plugin dynamic libraries (with `--plugins`)
│     │  ├─ c1073.dll                  # each is one cdylib — DSP + (optional) embedded React UI
│     │  ├─ compresser.dll             # (no CEF, no PluginUI/ — CEF is shared at the app root)
│     │  ├─ echospace.dll
│     │  ├─ equz8.dll
│     │  ├─ meowsyn.dll
│     │  └─ rodharerist.dll
│     ├─ Resources/
│     └─ build-info.json
└─ release/
   ├─ community/
   │  └─ windows-x64/         # cargo package-ce
   └─ professional/
      └─ windows-x64/         # cargo package-professional
```

There is deliberately **no `CEF/` folder and no `PluginUI/` folder** — CEF ships
flat beside the executable (its default resolution base), and each plugin's React
UI is embedded inside the plugin's own dynamic library. Package validation
actively rejects either directory.

- `dev` profile → `out/dev/<platform>` (edition omitted).
- any other profile → `out/<profile>/<edition>/<platform>`.

Target triples map to readable platform folders
(`x86_64-pc-windows-msvc → windows-x64`, `aarch64-apple-darwin → macos-arm64`,
etc.). Unknown triples are normalized to a safe slug instead of panicking.

## Creating packages (Windows PowerShell)

Development package (fast, unoptimized):

```powershell
cargo package-dev
# equivalent to:
cargo run -p xtask -- package --profile dev --edition community
```

Community release:

```powershell
cargo package-ce
# equivalent to:
cargo run -p xtask -- package `
  --profile release `
  --target x86_64-pc-windows-msvc `
  --edition community
```

Professional release (requires the private `crates/ExclusiveEdition/` source tree):

```powershell
cargo package-professional
# equivalent to:
cargo run -p xtask -- package `
  --profile release `
  --target x86_64-pc-windows-msvc `
  --edition professional
```

Optional flags:

- `--target <triple>` — cross to another platform folder (default: host triple).
- `--out <dir>` — root output directory (default: `out`).
- `--symbols` — also copy the `.pdb` into a separate `symbols/` directory.
- `--plugin <spec>` — build the Built-in Plugin dynamic libraries and stage them
  into `Plugins/`. `spec` is one of:
  - `all` — every built-in crate (`c1073`, `compresser`, `echospace`, `equz8`,
    `meowsyn`, `rodharerist`, …); fails if any expected library is
    missing from the staged package.
  - `none` — build no plugins (the default).
  - a comma-separated list of crate names, e.g. `rodharerist,equz8`.

  ```powershell
  cargo xtask package --profile release --edition community --plugin all
  ```

  `--plugins` (no value) is a legacy alias for `--plugin all`. All eight built-in
  crates build as `cdylib`s; the loadable-plugin C ABI is still being wired up.
- `--no-cef` — intentionally create a developer package without the shared CEF
  runtime. Normal packages fail when `build/cef` is unavailable.
- `--no-bundle` — skip wrapping a macOS package into `Futureboard Studio.app`.
  On macOS hosts, `package` otherwise runs `packaging/native/bundle-macos.sh`
  and writes `out/<profile>/<edition>/<platform>-app/Futureboard Studio.app`
  with Apple's required `Contents/` layout (never `Content/`).

## CEF runtime staging (shared, flat)

CEF is a single shared runtime, not one copy per plugin. Packaging copies it from
the repository's already-prepared distribution at `build/cef` (populated by
`SphereWebView`'s `install_cef` example — packaging never downloads another
runtime) into the application root:

- `build/cef/Release/*` (minus `.lib`/loader artifacts) → app root
- `build/cef/Resources/*` (paks, `icudtl.dat`) → app root
- `build/cef/Resources/locales/*` → `locales/`

`src/cef.rs` verifies the flat layout is complete before publishing. If
`build/cef` is absent, packaging fails rather than publishing a runtime whose
built-in plugin editors cannot open. Use `--no-cef` only for an intentional
CEF-free developer package.

The distribution is always selected by the *target* triple, never by the host, so
`--target x86_64-apple-darwin` on an Apple Silicon machine stages the `macosx64`
runtime and passes that same path to Cargo as `CEF_PATH`. A stale `CEF_PATH` in
the shell cannot leak into the build. Install a non-host distribution with:

```bash
cargo run -p SphereWebView --example install_cef --features installer -- --target x86_64-apple-darwin
```

`--target universal-macos` installs both Apple distributions, which is what
`packaging/native/merge-universal-macos.sh` needs to lipo one universal package
out of two single-architecture ones.

## Built-in Plugin embedded UI (BuildInHelper)

Each Built-in Plugin embeds its compiled React/Vite UI (`editorui/dist`) as
immutable `&'static [u8]` bytes inside its own dynamic library. The reusable
infrastructure lives in the `BuiltinAudioPlugins` crate (the *BuildInHelper*):

- `builtin_audio_plugins::ui` — runtime lookup (`EmbeddedUiAsset`,
  `EmbeddedUiAssetTable`, `EmbeddedPluginUi`, path normalization, MIME). CEF-free.
- `builtin_audio_plugins::ui::generate` — build-time table generator (behind the
  `ui-generate` feature) that a plugin `build.rs` runs against `editorui/dist`.

To wire a plugin's editor UI (e.g. `rodharerist`):

1. `crate-type = ["cdylib"]` and add
   `builtin-audio-plugins.workspace = true` (runtime) plus
   `[build-dependencies] builtin-audio-plugins = { workspace = true, features = ["ui-generate"] }`.
2. `build.rs`:

   ```rust
   fn main() {
       let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
       let dist = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("editorui/dist");
       builtin_audio_plugins::ui::generate::generate(
           &builtin_audio_plugins::ui::generate::GenerateOptions::from_out_dir(dist, out),
       ).expect("embed editor UI");
   }
   ```

3. In the crate: `include!(concat!(env!("OUT_DIR"), "/embedded_ui_assets.rs"));`
   then expose `EmbeddedUiAssetTable::new(EMBEDDED_UI_ASSETS)` via `EmbeddedPluginUi`.

The editor UI is built into a single self-contained `dist/index.html` (Vite +
`vite-plugin-singlefile` inlines all JS/CSS/assets), so a plugin embeds just one
asset. `xtask package --plugin ...` discovers each selected plugin's editor,
runs its frozen `bun install`, and then builds the frontend before Cargo starts.
The Rust crate can still compile directly with an empty table, while packaging
rejects any incomplete editor bundle.

At runtime the shared CEF host loads the editor via the `mikoplugin://` custom
scheme — `mikoplugin://<plugin>/index.html` — and resolves it through the loaded
plugin's asset provider (one origin per plugin). `builtin_audio_plugins::ui`
provides `PLUGIN_URL_SCHEME`, `parse_plugin_url` and `build_plugin_url` for the
handler. The bun build orchestration and the native CEF resource handler itself
are the remaining integration slices — see the task notes.

## How the binary path is discovered

The packager never assumes `target/release/FutureboardNative.exe`. It runs

```text
cargo build --message-format=json-render-diagnostics ...
```

and parses the `compiler-artifact` JSON messages to read the exact executable
path Cargo produced. This works across custom target triples, profiles, and the
per-edition target directories.

## Adding runtime files to the staging manifest

Runtime files are staged explicitly, not by scraping `target/`:

- **Sidecar executables** — `FutureboardNative` spawns
  `FutureboardPluginHostX64.exe` and `FutureboardPluginScanner.exe`, resolving
  them next to its own binary at runtime. They are separate `[[bin]]` targets of
  the `sphere-plugin-host` package, built in the same cargo invocation and
  staged beside the app. Add more via `SIDECAR_BINARIES` in `src/cargo_build.rs`.
- **Sibling shared libraries** — add names to `RUNTIME_SIBLINGS` in
  `src/staging.rs`. They are staged only when found next to the built binary
  (this is how `onnxruntime.dll` is picked up).
- **Crashpad handler** — `crashpad_handler.exe` on Windows and
  `crashpad_handler` on Unix are required beside the Studio binary. `xtask`
  propagates `CARGO_TARGET_DIR` so the prebuilt Crashpad build script places
  the handler in the same target/profile directory that packaging consumes.
- **Resource files** — copy them into the `Resources/` directory during staging
  (extend `create_layout_dirs` / add a copy step in `src/package.rs`).
- **Binary plugins** — Built-in Plugin dynamic libraries are discovered from
  Cargo metadata (workspace members under `crates/BuiltinAudioPlugins/crates`
  that build a `cdylib`/`dylib`), built via the JSON artifact stream, and staged
  into `Plugins/` when `--plugins` is passed. The known set is
  `plugins::BUILTIN_PLUGIN_CRATES`; `missing_builtin_plugins` enforces package
  completeness. See `src/plugins.rs`.

## build-info.json

Written into every package:

```json
{
  "schemaVersion": 1,
  "application": "Futureboard Studio",
  "binary": "FutureboardNative.exe",
  "edition": "community",
  "profile": "release",
  "target": "x86_64-pc-windows-msvc",
  "platform": "windows-x64",
  "version": "2026.7.2",
  "gitCommit": "…",
  "gitDirty": false,
  "builtAtUtc": "2026-07-19T12:34:56.789+00:00",
  "rustcVersion": "rustc …",
  "cargoVersion": "cargo …"
}
```

`version` comes from the `futureboard_native` package version. Git and toolchain
fields are best-effort — a missing `git`/toolchain yields `null` rather than
failing the package.

## Safe publishing

```text
build → collect artifacts → create staging → copy files → validate → publish
```

Staging happens in `out/.staging/<platform>-<edition>-<profile>/`. Only after
validation passes is the staging directory renamed into its final location
(moving any previous package aside first, restored on failure). A failed package
never leaves `out/` half-updated, and unrelated directories under `out/` are
never touched.
