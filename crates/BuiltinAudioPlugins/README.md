# BuiltinAudioPlugins

Engine-agnostic stock DSP cores for Futureboard Studio.

These crates intentionally **do not** depend on `SphereDirectAudioEngine`.
Wire-up into DAUx / `SphereAudioPlugins` happens in a later integration pass.

## Focus crates (this phase)

| Crate | Role | Phase | 3rd-party DSP (license) |
| --- | --- | --- | --- |
| `equz8` | 8-band dynamic parametric EQ (native editor shared with `equzx`) | 1 easy | [`biquad`](https://crates.io/crates/biquad) (MIT OR Apache-2.0) |
| `equzx` | 24-band dynamic mid/side parametric EQ, cuts to 96 dB/oct (native editor) | 2 medium | `biquad` |
| `compresser` | Compressor — single-band, or four-band (Linkwitz–Riley crossovers) (native editor shared with `imager`) | 2 medium | `biquad` (crossovers, sidechain HPF) |
| `fa2a` | Optical compressor (LA-2A-style) (native dynamics editor) | 1 easy | `biquad` (sidechain HPF) |
| `echospace` | Stereo / ping-pong / mono delay with wow, ducking and diffusion (native editor shared with `verbspace`) | 2 medium | `biquad` (tone stage HP/LP) |
| `verbspace` | 16-line FDN reverb with early reflections and three-band decay (native editor) | 3 hard | `biquad` (wet cuts) |
| `whitesharp` | Realtime pitch correction (Auto mode): YIN tracking, key/scale targets, stereo TD-PSOLA (native editor, wgpu correction meter) | 3 hard | `biquad` (detector decimation) |
| `fa76` | FET compressor (1176-style) (native dynamics editor) | 2 medium | `biquad` (sidechain HPF) |
| `imager` | Four-band stereo width with per-band stereoize (Haas / decorrelated) and Recover Sides (M/S, Linkwitz-Riley crossovers) (native editor) | 2 medium | `biquad` (crossovers, all-pass) |
| `c1073` | 3-band channel EQ + drive | 3 hard | `biquad` |
| `meowsyn` | Polyphonic soft-synth | 3 hard | [`fundsp`](https://crates.io/crates/fundsp) (MIT OR Apache-2.0) |

Other stub crates under `crates/` (`ampstage`, …) stay placeholders until a later slice.

## Shared contract

Every effect exposes:

- typed `Params` + `default_params()`
- `descriptor()` metadata
- `factory_presets()`, the bank its native editor loads
- `Dsp::new(sample_rate)` / `set_params` / `reset`
- allocation-free `process_stereo(l, r) -> (l, r)`

`meowsyn` additionally exposes MIDI `note_on` / `note_off`.

## Validate

```bash
cargo test -p BuiltinAudioPlugins
cargo test -p equz8 -p compresser -p fa2a -p echospace -p verbspace -p whitesharp -p imager -p fa76 -p c1073 -p meowsyn
```
