# Drum Silencer (shelved)

Low-latency drum removal: a causal network (GRU context + a causal
time × frequency convolution per bin, `src/model.rs`) masks an asymmetric-window
STFT with 5.3 ms of latency at 48 kHz (`src/lib.rs`). Remove takes the drums
out, Solo keeps only them, and a Low/High range limits either to a band.

**Status (2026-10-07): shelved.** The crate builds and its tests pass, but
nothing in the product uses it: it is a workspace member only, not a
dependency of Studio, the plug-in host, LiveStage or Audio Repair.

## The weights

`model/drumsilencer.dsil` is checkpoint step 17,500 of run `v2c` (of 30,000
planned; training was stopped at 19,000). On the remixed validation set it
takes the drums down 5.7 dB and the music 0.6 dB (SDR +4.2 dB), against
−10.6 / −0.7 dB for the ideal mask at the same framing. It has not been
measured on the MUSDB18-HQ test songs or listened to.

Trained on MUSDB18-HQ, whose licence is non-commercial: decide before shipping
these weights. See `training/README.md`.

The checkpoints (`best.pt`, `last.pt` for resuming) are on the training box in
`~/drumsil/runs/v2c/`:

```sh
nohup sh run_remote.sh v2c --arch v2 --batch 32 > ~/drumsil/runs/v2c.log 2>&1 < /dev/null &
```

## Wiring it back in

`integration/integration.patch` holds everything that connected it, as it was
when shelved:

* Studio: the built-in catalog and host process (latency, meters, state), the
  umbrella crate, and a native GPUI editor (`drum_silencer_panel.rs`,
  `drum_silencer_model.rs`, a preview scene).
* LiveStage: the engine's effect catalog and meters, and a web editor
  (`drumsilencer.tsx` / `.css`).
* Audio Repair: the Drum Silencer module (offline render with the latency
  removed, 1 s of pre-roll context and 10 ms edge crossfades).

```sh
git apply crates/BuiltinAudioPlugins/crates/drumsilencer/integration/integration.patch
```

It applied cleanly to `3d3459a1`; on a later tree, expect to resolve
conflicts.
