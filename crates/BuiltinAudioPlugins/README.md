# BuiltinAudioPlugins

Engine-agnostic stock DSP cores for Futureboard Studio.

These crates intentionally **do not** depend on `SphereDirectAudioEngine`.
Wire-up into DAUx / `SphereAudioPlugins` happens in a later integration pass.

## Focus crates (this phase)

| Crate | Role | Phase | 3rd-party DSP (license) |
| --- | --- | --- | --- |
| `equz8` | 8-band parametric EQ | 1 easy | [`biquad`](https://crates.io/crates/biquad) (MIT OR Apache-2.0) |
| `compresser` | Compressor — single-band, or four-band (Linkwitz–Riley crossovers) | 2 medium | `biquad` (crossovers, sidechain HPF) |
| `fa2a` | Optical compressor (LA-2A-style) | 1 easy | `biquad` (sidechain HPF) |
| `echospace` | Stereo / ping-pong delay | 2 medium | `biquad` (feedback HP/LP) |
| `fa76` | FET compressor (1176-style) | 2 medium | `biquad` (sidechain HPF) |
| `imager` | Four-band stereo width (M/S, Linkwitz-Riley crossovers) | 2 medium | `biquad` (crossovers, all-pass) |
| `c1073` | 3-band channel EQ + drive | 3 hard | `biquad` |
| `meowsyn` | Polyphonic soft-synth | 3 hard | [`fundsp`](https://crates.io/crates/fundsp) (MIT OR Apache-2.0) |

Other stub crates under `crates/` (`ampstage`, `verbspace`, …) stay placeholders until a later slice.

## Shared contract

Every effect exposes:

- typed `Params` + `default_params()`
- `descriptor()` metadata
- `Dsp::new(sample_rate)` / `set_params` / `reset`
- allocation-free `process_stereo(l, r) -> (l, r)`

`meowsyn` additionally exposes MIDI `note_on` / `note_off`.

## Validate

```bash
cargo test -p BuiltinAudioPlugins
cargo test -p equz8 -p compresser -p fa2a -p echospace -p imager -p fa76 -p c1073 -p meowsyn
```
