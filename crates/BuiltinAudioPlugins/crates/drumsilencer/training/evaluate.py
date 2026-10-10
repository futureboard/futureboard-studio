"""Measure a checkpoint on MUSDB18-HQ's test songs (never trained on).

    python evaluate.py runs/v1/best.pt --data ~/drumsil/data/musdb18hq [--wav out/]

Per song, with the streaming chain (`dsil.separate`) and its latency undone:
* keep SDR: the output against the mix without drums, in dB, and the same for
  the untouched mix (what doing nothing scores), so the gain is visible;
* drums / music: how much of the true drums and of the true music the mask
  lets through, in dB (the mask applied to each stem's own spectrum);
* the oracle: the same two numbers for the ideal ratio mask under the same
  5 ms framing — the ceiling any mask at this latency could reach.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import soundfile as sf
import torch

import dsil
from train import songs


def sdr(estimate: np.ndarray, reference: np.ndarray) -> float:
    err = np.sum((estimate - reference) ** 2)
    return float(10 * np.log10(np.sum(reference**2) / max(err, 1e-12)))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("checkpoint")
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--wav", type=Path)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--device", default="cuda" if torch.cuda.is_available() else "cpu")
    args = ap.parse_args()
    state = torch.load(args.checkpoint, map_location="cpu")
    hidden = state["model"]["inp.weight"].shape[0]
    model = dsil.build(state.get("arch", "v1"), hidden).to(args.device).eval()
    model.load_state_dict(state["model"])
    analysis = torch.tensor(dsil.windows()[0], dtype=torch.float32, device=args.device)
    tracks = songs(args.data, "test")
    if args.limit:
        tracks = tracks[: args.limit]
    rows = []
    L = dsil.LATENCY
    for track in tracks:
        mix, sr = sf.read(track / "mixture.wav", dtype="float32", always_2d=True)
        drums, _ = sf.read(track / "drums.wav", dtype="float32", always_2d=True)
        mix, drums = mix.T, drums.T
        rest = mix - drums
        keep, take = dsil.separate(model, mix, float(sr), device=args.device)
        keep, take = keep[:, L:], take[:, L:]
        n = keep.shape[1]
        row = {
            "song": track.name,
            "keep_sdr": sdr(keep, rest[:, :n]),
            "mix_sdr": sdr(mix[:, :n], rest[:, :n]),
            "drums_sdr": sdr(take, drums[:, :n]),
        }
        with torch.no_grad():
            X = dsil.stft(torch.tensor(mix, device=args.device), analysis)
            mask, _ = model(dsil.features(X.unsqueeze(0), float(sr)))
            D = dsil.stft(torch.tensor(drums, device=args.device), analysis)
            T = X - D
            m = mask[0].unsqueeze(0)

            def share(spec, gain):
                return 10 * torch.log10(
                    (spec * gain).abs().pow(2).sum() / spec.abs().pow(2).sum().clamp_min(1e-12)
                ).item()

            row["drums_db"] = share(D, m)
            row["music_db"] = share(T, m)
            irm = T.abs() / (T.abs() + D.abs()).clamp_min(1e-8)
            row["oracle_drums_db"] = share(D, irm)
            row["oracle_music_db"] = share(T, irm)
        rows.append(row)
        print(json.dumps({k: (round(v, 2) if isinstance(v, float) else v) for k, v in row.items()}), flush=True)
        if args.wav:
            args.wav.mkdir(parents=True, exist_ok=True)
            stem = track.name.replace("/", "_")
            sf.write(args.wav / f"{stem} - silenced.wav", keep.T, sr)
            sf.write(args.wav / f"{stem} - drums.wav", take.T, sr)
    keys = [k for k in rows[0] if k != "song"]
    summary = {k: float(np.median([r[k] for r in rows])) for k in keys}
    print("median", json.dumps({k: round(v, 2) for k, v in summary.items()}), flush=True)


if __name__ == "__main__":
    main()
