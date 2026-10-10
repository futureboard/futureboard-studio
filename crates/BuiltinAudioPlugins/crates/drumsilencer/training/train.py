"""Train Drum Silencer's mask network on MUSDB18-HQ.

    python train.py --data ~/drumsil/data/musdb18hq --out runs/v1

Examples are remixed on the fly: the drums of one song under the bass, other
and vocals of (half the time) other songs, each at a random gain, sometimes
resampled to 48 kHz so the model hears both bin spacings, sometimes with no
drums at all so it learns to leave drumless music alone. The target is the
mix without its drums; the loss is a compressed-spectrum loss on both what
the mask keeps and what it takes, so the Solo mode is trained too.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import time
from pathlib import Path

import numpy as np
import soundfile as sf
import torch
from scipy.signal import resample_poly
from torch.utils.data import DataLoader, IterableDataset

import dsil

STEMS = ("drums", "bass", "other", "vocals")
# The standard MUSDB18 validation split (held out of training).
VALIDATION = {
    "Actions - One Minute Smile",
    "Clara Berry And Wooldog - Waltz For My Victims",
    "Johnny Lokke - Promises & Lies",
    "Patrick Talbot - A Reason To Leave",
    "Triviul - Angelsaint",
    "Alexander Ross - Goodbye Bolero",
    "Fergessen - Nos Palpitants",
    "Leaf - Summerghost",
    "Skelpolu - Human Mistakes",
    "Young Griffo - Pennies",
    "ANiMAL - Rockshow",
    "James May - On The Line",
    "Meaxic - Take A Step",
    "Traffic Experiment - Sing Along",
}
SR = 44_100
SEGMENT = 500 * dsil.M  # 500 frames (1.3 s at 48 kHz) per example
COMPRESS = 0.3
# The two loss terms brought to a similar size: the spectral term runs from
# ~0.13 (no separation) to ~0.03 (the ideal mask); the dB term over ~20 dB.
SPECTRAL_SCALE = 0.1
SDR_SCALE_DB = 20.0


def songs(root: Path, split: str) -> list[Path]:
    base = root / ("test" if split == "test" else "train")
    out = []
    for d in sorted(base.iterdir()):
        if not d.is_dir():
            continue
        valid = d.name in VALIDATION
        if split == "train" and valid or split == "valid" and not valid:
            continue
        out.append(d)
    return out


def read(path: Path, start: int, length: int) -> np.ndarray:
    data, _ = sf.read(path, start=start, frames=length, dtype="float32", always_2d=True)
    if data.shape[0] < length:
        data = np.pad(data, ((0, length - data.shape[0]), (0, 0)))
    return data.T  # [C, T]


class Remix(IterableDataset):
    def __init__(self, tracks: list[Path], seed: int):
        self.tracks = tracks
        self.seed = seed
        self.frames = {t: sf.info(t / "mixture.wav").frames for t in tracks}

    def example(self, rng: random.Random) -> tuple[np.ndarray, np.ndarray]:
        to48 = rng.random() < 0.35
        need = math.ceil(SEGMENT * SR / 48_000) + 64 if to48 else SEGMENT
        same = rng.random() < 0.5
        base = rng.choice(self.tracks)
        base_start = rng.randrange(max(1, self.frames[base] - need))
        parts = {}
        for stem in STEMS:
            track = base if same or stem == "drums" else rng.choice(self.tracks)
            start = base_start if track is base else rng.randrange(max(1, self.frames[track] - need))
            x = read(track / f"{stem}.wav", start, need)
            x *= 10 ** (rng.uniform(-9.0, 3.0) / 20.0)
            if rng.random() < 0.1:
                x = x[::-1].copy()  # channel swap
            parts[stem] = x
        # Drumless music and drums alone, so neither extreme is a surprise.
        roll = rng.random()
        if roll < 0.10:
            parts["drums"] *= 0.0
        elif roll < 0.14:
            for stem in ("bass", "other", "vocals"):
                parts[stem] *= 0.0
        drums = parts["drums"]
        rest = parts["bass"] + parts["other"] + parts["vocals"]
        if to48:
            drums = resample_poly(drums, 160, 147, axis=-1)[:, :SEGMENT].astype(np.float32)
            rest = resample_poly(rest, 160, 147, axis=-1)[:, :SEGMENT].astype(np.float32)
        if rng.random() < 0.1:  # mono sources
            drums = np.repeat(drums.mean(0, keepdims=True), 2, 0)
            rest = np.repeat(rest.mean(0, keepdims=True), 2, 0)
        gain = 10 ** (rng.uniform(-24.0, 4.0) / 20.0)
        peak = max(np.abs(drums + rest).max(), 1e-4)
        gain = min(gain, 0.99 / peak)
        return (drums * gain).astype(np.float32), (rest * gain).astype(np.float32)

    def __iter__(self):
        info = torch.utils.data.get_worker_info()
        wid = info.id if info else 0
        rng = random.Random(self.seed * 1000 + wid + int(time.time()))
        while True:
            yield self.example(rng)


def fixed_batch(tracks, count, seed):
    data = Remix(tracks, 0)
    rng = random.Random(seed)
    items = [data.example(rng) for _ in range(count)]
    return (
        torch.tensor(np.stack([d for d, _ in items])),
        torch.tensor(np.stack([r for _, r in items])),
    )


def compress(z: torch.Tensor) -> torch.Tensor:
    mag = z.abs().clamp_min(1e-8)
    return z * (mag ** (COMPRESS - 1.0))


def losses(model, drums, rest, analysis):
    mix = drums + rest
    X = dsil.stft(mix, analysis)  # [B, C, F, K]
    T = dsil.stft(rest, analysis)
    D = dsil.stft(drums, analysis)
    mask, _ = model(dsil.features(X, 48_000.0))
    m = mask.unsqueeze(1)
    keep, take = X * m, X * (1.0 - m)
    # Two terms per path. The compressed-spectrum loss shapes the quiet bins;
    # on its own it all but ignores which bins carry the energy, so a
    # signal-to-distortion term (in dB, per example) weighs the loud ones. Its
    # floor, a thousandth of the mix's energy, keeps a drumless example (or
    # one with nothing but drums) from dividing by zero.
    mix_energy = X.abs().pow(2).sum(dim=(1, 2, 3)).float()
    floor = 1e-3 * mix_energy + 1e-8
    loss = 0.0
    for pred, target in ((keep, T), (take, D)):
        pc, tc = compress(pred), compress(target)
        spectral = 0.7 * (pc.abs() - tc.abs()).pow(2).mean() + 0.3 * (pc - tc).abs().pow(2).mean()
        err = (pred - target).abs().pow(2).sum(dim=(1, 2, 3)).float()
        energy = target.abs().pow(2).sum(dim=(1, 2, 3)).float()
        sdr_db = 10.0 * torch.log10((err + floor) / (energy + floor))
        loss = loss + spectral / SPECTRAL_SCALE + sdr_db.mean() / SDR_SCALE_DB
    return loss, keep, take, T, D, m


def db(x):
    return 10.0 * math.log10(max(x, 1e-12))


@torch.no_grad()
def validate(model, batch, analysis, device, chunk=16):
    """Sums over the fixed batch, a chunk at a time to fit the GPU."""
    model.eval()
    sums = dict(loss=0.0, err=0.0, base=0.0, drums_left=0.0, drums=0.0, rest_left=0.0, rest=0.0)
    count = batch[0].shape[0]
    for s in range(0, count, chunk):
        drums, rest = (t[s : s + chunk].to(device) for t in batch)
        loss, keep, _, T, D, m = losses(model, drums, rest, analysis)
        n = drums.shape[0]
        sums["loss"] += loss.item() * n
        sums["err"] += (keep - T).abs().pow(2).sum().item()
        sums["base"] += D.abs().pow(2).sum().item()  # the mix's error is its drums
        sums["drums_left"] += (D * m).abs().pow(2).sum().item()
        sums["drums"] += D.abs().pow(2).sum().item()
        sums["rest_left"] += (T * m).abs().pow(2).sum().item()
        sums["rest"] += T.abs().pow(2).sum().item()
    model.train()
    return {
        "loss": sums["loss"] / count,
        "sdr_gain_db": db(sums["base"] / max(sums["err"], 1e-12)),
        "drums_db": db(sums["drums_left"] / max(sums["drums"], 1e-12)),
        "music_db": db(sums["rest_left"] / max(sums["rest"], 1e-12)),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--steps", type=int, default=30_000)
    ap.add_argument("--batch", type=int, default=64)
    ap.add_argument("--lr", type=float, default=1.5e-3)
    ap.add_argument("--workers", type=int, default=14)
    ap.add_argument("--device", default="cuda" if torch.cuda.is_available() else "cpu")
    ap.add_argument("--resume", action="store_true")
    ap.add_argument("--arch", default="v2", choices=["v1", "v2"])
    ap.add_argument("--every", type=int, default=500, help="steps between checkpoints")
    ap.add_argument("--no-amp", action="store_true", help="train in float32 throughout")
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    device = torch.device(args.device)
    if device.type == "cpu":
        torch.set_num_threads(max(1, os.cpu_count() - args.workers // 2))

    train, valid = songs(args.data, "train"), songs(args.data, "valid")
    print(f"{len(train)} train / {len(valid)} valid songs on {device}", flush=True)
    analysis = torch.tensor(dsil.windows()[0], dtype=torch.float32, device=device)
    model = dsil.build(args.arch).to(device)
    opt = torch.optim.AdamW(model.parameters(), lr=args.lr, weight_decay=1e-4)
    sched = torch.optim.lr_scheduler.OneCycleLR(
        opt, max_lr=args.lr, total_steps=args.steps, pct_start=0.03
    )
    amp = device.type == "cuda" and not args.no_amp
    scaler = torch.amp.GradScaler("cuda", enabled=amp)
    step = 0
    best = -1e9
    ckpt = args.out / "last.pt"
    if args.resume and ckpt.exists():
        state = torch.load(ckpt, map_location=device)
        model.load_state_dict(state["model"])
        opt.load_state_dict(state["opt"])
        sched.load_state_dict(state["sched"])
        if "scaler" in state:
            scaler.load_state_dict(state["scaler"])
        step, best = state["step"], state["best"]
        print(f"resumed at step {step}", flush=True)

    val_batch = fixed_batch(valid, 96, seed=1234)
    loader = DataLoader(
        Remix(train, seed=step + 1),
        batch_size=args.batch,
        num_workers=args.workers,
        persistent_workers=True,
        prefetch_factor=4,
    )
    log = open(args.out / "log.jsonl", "a")
    started = time.time()
    first = step
    running = 0.0
    for drums, rest in loader:
        if step >= args.steps:
            break
        drums, rest = drums.to(device, non_blocking=True), rest.to(device, non_blocking=True)
        # Mixed precision for the network; the transforms and the loss stay
        # float32 (torch.fft is not autocast).
        with torch.autocast("cuda", dtype=torch.float16, enabled=amp):
            loss, *_ = losses(model, drums, rest, analysis)
        opt.zero_grad(set_to_none=True)
        scaler.scale(loss).backward()
        scaler.unscale_(opt)
        torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
        scaler.step(opt)
        scaler.update()
        sched.step()
        step += 1
        running += loss.item()
        if step % 100 == 0:
            rate = (step - first) / max(time.time() - started, 1e-6)
            print(f"step {step} loss {running / 100:.4f} {rate:.2f} it/s", flush=True)
            running = 0.0
        if step % args.every == 0 or step == args.steps:
            v = validate(model, val_batch, analysis, device)
            v["step"] = step
            print("valid", json.dumps(v), flush=True)
            log.write(json.dumps(v) + "\n")
            log.flush()
            state = {
                "model": model.state_dict(),
                "opt": opt.state_dict(),
                "sched": sched.state_dict(),
                "scaler": scaler.state_dict(),
                "step": step,
                "arch": args.arch,
                "best": max(best, v["sdr_gain_db"]),
            }
            torch.save(state, ckpt)
            if v["sdr_gain_db"] > best:
                best = v["sdr_gain_db"]
                torch.save(
                    {"model": model.state_dict(), "step": step, "arch": args.arch, "valid": v},
                    args.out / "best.pt",
                )
    print("done", flush=True)


if __name__ == "__main__":
    main()
