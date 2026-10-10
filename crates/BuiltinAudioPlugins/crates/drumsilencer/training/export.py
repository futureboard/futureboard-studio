"""Write a checkpoint as the Rust DSP's weight file, plus a parity vector.

    python export.py runs/v2/best.pt ../model/drumsilencer.dsil
    python export.py --random --hidden 16 ../src/testdata/random16.dsil   # untrained, for tests

Weight file (little-endian), the v2 network (`dsil.DrumSilencerNet2`):
    b"DSIL", u32 version (2), u32 N, u32 M, u32 K, u32 hidden, u32 layers,
    u32 conv_channels, u32 conv_time, u32 conv_freq, u32 head,
    f32 tau_seconds, f32 level_floor, f32 feature_scale, f32 feature_clamp,
    then f32 tensors, row-major, in this order:
    inp.weight [H, K], inp.bias [H],
    per layer: weight_ih [3H, H], weight_hh [3H, H], bias_ih [3H], bias_hh [3H]
    (gate rows r, z, n, as PyTorch),
    ctx.weight [2K, H], ctx.bias [2K]  (bin k's two values at 2k, 2k + 1),
    conv.weight [C, T, F] (row t = 0 is the oldest frame), conv.bias [C],
    head1.weight [HEAD, C + 3] (inputs: C conv, 2 context, the feature),
    head1.bias [HEAD], head2.weight [HEAD], head2.bias [1].

Parity vector (`<weights>.parity`): b"DSPV", u32 sample_rate, u32 frames,
then the stereo input and the expected keep output, both interleaved f32 —
what `separate` (the offline twin of the streaming DSP) gives.
"""

from __future__ import annotations

import argparse
import struct
from pathlib import Path

import numpy as np
import torch

import dsil


def write_weights(model: dsil.DrumSilencerNet2, path: Path) -> None:
    sd = {k: v.detach().cpu().float().numpy() for k, v in model.state_dict().items()}
    hidden, layers = model.gru.hidden_size, model.gru.num_layers
    with open(path, "wb") as f:
        f.write(b"DSIL")
        f.write(struct.pack("<6I", 2, dsil.N, dsil.M, dsil.K, hidden, layers))
        f.write(struct.pack("<4I", dsil.CONV_CHANNELS, dsil.CONV_TIME, dsil.CONV_FREQ, dsil.HEAD))
        f.write(
            struct.pack(
                "<4f", dsil.TAU_SECONDS, dsil.LEVEL_FLOOR, dsil.FEATURE_SCALE, dsil.FEATURE_CLAMP
            )
        )
        tensors = [sd["inp.weight"], sd["inp.bias"]]
        for layer in range(layers):
            for name in ("weight_ih", "weight_hh", "bias_ih", "bias_hh"):
                tensors.append(sd[f"gru.{name}_l{layer}"])
        tensors += [sd["ctx.weight"], sd["ctx.bias"], sd["conv.weight"], sd["conv.bias"]]
        tensors += [sd["head1.weight"], sd["head1.bias"], sd["head2.weight"], sd["head2.bias"]]
        for t in tensors:
            f.write(np.ascontiguousarray(t, dtype="<f4").tobytes())


def parity_signal(sample_rate: int, seconds: float) -> np.ndarray:
    """Tones, a bass line and noise bursts: something with drums-like hits."""
    rng = np.random.default_rng(7)
    t = np.arange(int(sample_rate * seconds)) / sample_rate
    tone = 0.2 * np.sin(2 * np.pi * 220 * t) + 0.1 * np.sin(2 * np.pi * 330 * t + 1.0)
    bass = 0.25 * np.sin(2 * np.pi * 55 * t)
    hits = np.zeros_like(t)
    for start in np.arange(0.05, seconds, 0.125):
        i = int(start * sample_rate)
        n = min(len(t) - i, int(0.08 * sample_rate))
        hits[i : i + n] += rng.standard_normal(n) * np.exp(-np.arange(n) / (0.015 * sample_rate)) * 0.5
    left = tone + bass + hits
    right = 0.8 * tone + bass + 0.6 * hits
    return np.stack([left, right]).astype(np.float32)


def write_parity(model: dsil.DrumSilencerNet2, path: Path, sample_rate: int = 48_000) -> None:
    audio = parity_signal(sample_rate, 0.6)
    keep, _ = dsil.separate(model, audio, float(sample_rate))
    with open(path, "wb") as f:
        f.write(b"DSPV")
        f.write(struct.pack("<2I", sample_rate, audio.shape[1]))
        f.write(np.ascontiguousarray(audio.T, dtype="<f4").tobytes())
        f.write(np.ascontiguousarray(keep.T, dtype="<f4").tobytes())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("checkpoint", nargs="?")
    ap.add_argument("output", type=Path)
    ap.add_argument("--random", action="store_true", help="export an untrained model")
    ap.add_argument("--hidden", type=int, default=dsil.HIDDEN)
    args = ap.parse_args()
    torch.manual_seed(0)
    model = dsil.DrumSilencerNet2(hidden=args.hidden)
    if args.random:
        with torch.no_grad():  # a mask that moves, so the parity test sees it
            model.head2.weight.mul_(8.0)
    else:
        state = torch.load(args.checkpoint, map_location="cpu")
        if state.get("arch") != "v2":
            raise SystemExit("only v2 checkpoints export")
        model.load_state_dict(state["model"])
    model.eval()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    write_weights(model, args.output)
    write_parity(model, args.output.with_suffix(args.output.suffix + ".parity"))
    print(f"wrote {args.output} ({args.output.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
