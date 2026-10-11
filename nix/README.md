# Nix builds

`flake.nix` at the repository root builds every native app for
**x86_64-linux**, laid out the way the existing Linux packaging
(`xtask package`, `packaging/linux`, `packaging/aur`, `packaging/livestage`)
lays it out.

> New files must be known to git before a flake can see them. After pulling
> or editing, `git add` new files (no commit needed); `nix build` then uses the
> working tree.

## Usage

```bash
nix build .#studio              # = nix build (default package)
nix build .#livestage-server
nix build .#all                 # every app, as result/<name>

nix run                         # Futureboard Studio
nix run .#livestage-server -- --http 0.0.0.0:8730
nix run .#apak -- --help

nix develop                     # toolchain + native deps + prefetched SDKs
nix flake check                 # builds livestage-server + apakinstaller, runs smoke tests
```

## Packages

| Package               | Contents                                                                                                                                                                                                                                                                                                                                                                         |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `studio` (`default`)  | `bin/futureboard-studio` (+ `FutureboardNative`). Runtime tree in `libexec/futureboard-studio/` exactly like the xtask package: `FutureboardNative`, `FutureboardPluginHostX64`, `FutureboardPluginScanner`, `bin/{apakinstaller,apak,makeapak}`, `crashpad_handler`, `libonnxruntime.so`, the CEF runtime flat with `locales/`, `Plugins/lib*.so` (every built-in plug-in cdylib), `Resources/`, `build-info.json`. Desktop file, MIME type (`.fbproj`) and icon from `packaging/linux`. |
| `livestage`           | `LiveStage` (GPUI desktop mixer) and `livestage-server`, default features (third-party plug-ins through the plug-in host, which is linked in beside them), web UI embedded.                                                                                                                                                                                                      |
| `livestage-server`    | Headless `livestage-server`, `--no-default-features` (ALSA, built-in effects only), web UI embedded.                                                                                                                                                                                                                                                                             |
| `livestage-appliance` | `livestage-server`, `livestage-setup`, `livestage-installer` with `--no-default-features --features appliance`, as `packaging/livestage/make-image.sh` builds them (glibc instead of the image's musl).                                                                                                                                                                         |
| `jamsession`          | `FutureboardJam`.                                                                                                                                                                                                                                                                                                                                                                |
| `singer`              | `Singer`.                                                                                                                                                                                                                                                                                                                                                                        |
| `apakinstaller`       | `apakinstaller` (GUI), `apak`, `makeapak`.                                                                                                                                                                                                                                                                                                                                       |
| `plugin-host`         | `FutureboardPluginHostX64`, `FutureboardPluginScanner` on their own.                                                                                                                                                                                                                                                                                                             |
| `cef-helper`          | `futureboard_cef_helper` with a CEF runtime beside it. Futureboard only ships this helper on macOS; on Linux Studio runs CEF subprocesses as itself.                                                                                                                                                                                                                         |
| `all`                 | A link farm of all of the above.                                                                                                                                                                                                                                                                                                                                                 |
| `cef`, `livestage-webui` | Building blocks, exposed so their hashes can be refreshed on their own.                                                                                                                                                                                                                                                                                                       |

All Rust packages are `--release` builds (the workspace profile: fat LTO,
one codegen unit), so Studio and the other GPUI apps take a long time and a
lot of memory to link. Workspace tests are not run inside the package builds.

## How downloads are handled

Nothing downloads inside a build. Everything below is a fixed-output source
pinned in `nix/sources.nix` (or `nix/packages.nix` for the two dependency
hashes).

| What                               | Upstream behaviour                                                         | Nix build                                                                                                                                                                                   |
| ---------------------------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cargo crates (crates.io + 9 git repositories) | `cargo` fetches                                                   | One `rustPlatform.fetchCargoVendor` derivation (`cargoDeps`, hash in `nix/packages.nix`) shared by every package. `cargoLock.lockFile` + `outputHashes` cannot be used: `Cargo.lock` has `download-cef 2.3.2` both from crates.io and from the cef-rs git repo, which importCargoLock's flat vendor directory cannot hold. |
| Git submodules                     | `git submodule update` (CI: `.github/workflows/submodules-init.sh`)        | Pinned per commit and placed into the source tree (`nix/source.nix`): cpal, VST3 SDK (`base`, `pluginterfaces`, `public.sdk`), CLAP, clap-helpers, ARA_SDK/ARA_API, lucide and tabler `icons/`. |
| CEF 150.0.11 SDK + runtime         | cef-dll-sys downloads when `CEF_PATH` is unset (CI runs `install_cef`)     | The linux64 *minimal* archive, unpacked into the `install_cef` layout with `archive.json`, Chromium binaries patched for the store; `CEF_PATH` points at it.                                 |
| Crashpad 0.2.7 prebuilt            | crashpad-rs-sys `prebuilt` downloads its release archive                   | Prefetched archive placed at `$CRASHPAD_CACHE_DIR/prebuilt/0.2.7/x86_64-unknown-linux-gnu/.crashpad-ok`, so the build script links it; `crashpad_handler` ships beside Studio.            |
| ONNX Runtime 1.23.2                | Studio `build.rs` downloads it beside the binary (`stem-onnx`)             | `FUTUREBOARD_ORT_SKIP_DOWNLOAD=1`; Microsoft's 1.23.2 release is prefetched, patched, and linked in as `libonnxruntime.so` beside `FutureboardNative` (nixpkgs ships a newer, non-matching 1.27). |
| LiveStage web UI (`webui/dist`)    | `bun run --cwd apps/native/livestage/webui build` before cargo            | `livestage-webui`: `bun install --frozen-lockfile --filter ./apps/native/livestage/webui` from the root `bun.lock` in a fixed-output derivation, then `tsc -b && vite build` offline; copied into `webui/dist` before cargo so `build.rs` embeds the real page. |

Other build-time configuration:

- `FUTUREBOARD_AUTH_API_URL` defaults to `https://futureboard.studio`, the same
  public origin CI bakes (override: `authApiUrl`).
- **APAK trust anchor.** Release builds of the APAK tools require
  `APAK_VERIFYING_KEY`; the production key is a CI secret and
  `apak.public.key` is git-ignored. The Nix packages take it from, in order:
  the `apakVerifyingKey` argument, `APAK_VERIFYING_KEY` when evaluated with
  `--impure`, otherwise the public **development** key (the one debug builds
  use), which does not verify official packages:

  ```bash
  APAK_VERIFYING_KEY="$(cat apak.public.key)" nix build --impure .#studio
  ```

  `nix eval .#studio.apakTrustAnchor` says which one a build uses.
- The TONE3000 API key and the Discord client id are not baked (no `.env`,
  as in a fresh clone); Professional Edition is not built.

## Runtime

- Executables keep `$ORIGIN` (Studio's own run path) and have the libraries
  GPUI/wgpu open with `dlopen` (Vulkan loader, Wayland, xkbcommon, Xlib
  extensions, libGL/EGL, fontconfig, ALSA) prepended to their RUNPATH. Nothing
  is put into `LD_LIBRARY_PATH`, so programs the apps launch are unaffected.
- GPU drivers come from the host: on NixOS enable `hardware.graphics`; on
  other distributions run through [nixGL](https://github.com/nix-community/nixGL)
  if Vulkan/GL cannot find a driver.
- Audio is ALSA (PipeWire/PulseAudio through their ALSA plug-ins, as with
  the AppImage).
- CEF runs unsandboxed (`no_sandbox`, as Studio already configures it), so
  the setuid `chrome-sandbox` is not needed.

## Platforms

Only `x86_64-linux`. The CEF archive, the Crashpad prebuilt and the ONNX
Runtime release are pinned for linux-x64 (aarch64 would need different
archives, Crashpad's from-source `vendored` build, and a second node_modules
hash), and none of that could be verified here. macOS is not provided: its
packaging (universal CEF, `.app` bundle, codesigning) lives in
`packaging/native` and is not a Nix job.

## Refreshing hashes

| When                                        | Update                                                       |
| ------------------------------------------- | ------------------------------------------------------------ |
| `Cargo.lock` changes                        | `cargoDeps.hash` in `nix/packages.nix`                       |
| `bun.lock` changes for the LiveStage web UI | `nodeModulesHash` in `nix/packages.nix`                      |
| A submodule is bumped                       | its `rev` + `hash` in `nix/sources.nix`                      |
| CEF / Crashpad / ONNX Runtime version bump  | the archive URL + `hash` in `nix/sources.nix` (and `nix/prebuilt.nix` paths) |

Set the hash to `lib.fakeHash`, build, and copy the `got:` hash Nix reports.
