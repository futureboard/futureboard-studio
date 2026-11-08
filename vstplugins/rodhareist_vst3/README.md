# Rodhareist plug-in (VST2 / VST3 / CLAP)

Futureboard's Rodhareist guitar multi-effect for other DAWs, built on the
repo's vendored nih-plug fork (`external/nihplug_gpui`).

- **DSP:** Studio's `rodharerist` crate, linked by path and unchanged: the
  same `Dsp`, wire table, factory presets and NAM/IR loaders a Studio insert
  runs.
- **Editor:** Studio's own Rodhareist window (`rodhareist_panel.rs`,
  `rodhareist_window.rs`, `rodhareist_blocks.rs`, `plate.rs`), compiled from
  `crates/SphereUIComponents` with `#[path]`. A change to the Studio editor is
  a change here. The files under `src/components` without `#[path]`, and
  `registry.rs`, `preset.rs`, `audio_bridge.rs`, stand in for the Studio
  services those files call.

## Build

From the repo root:

```bash
cargo rodhareist-vst3 bundle rodhareist_vst3 --release
```

or `cargo xtask bundle rodhareist_vst3 --release`. Output, in
`vstplugins/rodhareist_vst3/target/bundled/`:

| Format | File | Install to (Windows) |
| --- | --- | --- |
| VST3 | `Rodhareist.vst3` | `C:\Program Files\Common Files\VST3\` |
| CLAP | `Rodhareist.clap` | `C:\Program Files\Common Files\CLAP\` |
| VST2 | `Rodhareist.dll` | the host's VST2 folder |

Checks:

```bash
cargo test --manifest-path vstplugins/rodhareist_vst3/Cargo.toml
```

`src/entry_point_tests.rs` drives each exported entry point (VST3 factory,
CLAP entry, VST2 `VSTPluginMain`) the way a host does and runs audio through
it; the debug build aborts on any allocation in `process`.

## AU

Not built. nih-plug has no Audio Unit wrapper, and an AU can only be built
and signed on macOS. The route there is
[clap-wrapper](https://github.com/free-audio/clap-wrapper), which wraps the
CLAP build as an AUv2 on a Mac.

## What a session saves

- Every wire parameter, under its wire id (`amp_gain`, `path_slot_3`,
  `delay_row`, …): the names Studio's projects use. The path slots and block
  lanes are saved but not automatable; the editor edits them.
- The NAM capture and IR by file name. The files live in
  `Documents/Futureboard Studio/Rodhareist/{NAMs,IRs}`, the folder Studio
  uses, and are reloaded from there when the session opens.
- User presets are Studio's `.pst` files, in
  `Documents/Futureboard Studio/Presets/Plug-ins/Rodhareist`, so a preset
  saved in either shows up in both.
