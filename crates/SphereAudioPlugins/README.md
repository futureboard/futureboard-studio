# SphereAudioPlugins

Realtime-safe audio plugin framework used by DAUx (`crates/SphereDirectAudioEngine`).

## Goals

- Shared descriptor format for built-in and extension audio plugins.
- Realtime-safe DSP state owned by the audio engine.
- Stable plugin IDs (`sphere.eq8`, `sphere.drive`, `sphere.comp`) usable from project snapshots and extension manifests.
- Extension-friendly metadata similar to VSCode contributions.

## DAUx integration

DAUx stores each insert as `RuntimeInsert { kind, params, dsp }`.

- `kind` is canonicalized with `canonical_plugin_id()`.
- `AudioPluginDspState` holds prepared DSP state such as EQ biquads.
- `process_stereo_sample()` is called from the realtime render path.
- `should_rebuild_state()` tells DAUx when parameter changes require coefficient rebuilds.

## Realtime rules

Plugin processing must avoid:

- heap allocation inside sample processing
- locks
- file/network I/O
- logging
- blocking calls

Extension backends should follow the same contract.
