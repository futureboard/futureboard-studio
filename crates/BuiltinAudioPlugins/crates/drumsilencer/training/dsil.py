"""Drum Silencer's model and framing, shared by training, export and evaluation.

This file is the contract with the Rust DSP (`drumsilencer/src`): the window,
the features and the network must match it exactly, and `export.py` writes a
parity vector the Rust tests replay.

Framing (at 44.1/48 kHz; the model always sees ~43-47 Hz bins and ~2.7-2.9 ms
hops; at 88.2 kHz and up the Rust DSP decimates the bins, see `src/lib.rs`):

* N = 1024 analysis samples, hop M = 128, K = 513 bins.
* Frame f ends at input sample (f + 1) M - 1, with zero history before the
  signal.
* An asymmetric analysis/synthesis window pair (Mauler & Martin): the analysis
  window is long, for frequency resolution; the synthesis window only covers
  the last 2M samples, so the latency is 2M - 1 samples (5.3 ms at 48 kHz),
  not N. Their product is a 2M Hann, which overlap-adds to exactly 1 at hop M.

Features: P = mean over channels of |X|^2; lp = log10(P + 1e-10); a causal
running level mu (one-pole, TAU seconds, of the frame's mean lp floored at
LEVEL_FLOOR; mu starts at the first frame's) is subtracted and the result
divided by FEATURE_SCALE, clamped to +-FEATURE_CLAMP.

Network: Linear(K -> H) + ReLU, GRU x LAYERS (PyTorch gate order r, z, n),
Linear(H -> K) plus a per-bin skip `d * feature`, sigmoid: the share of each
bin that is NOT drums. One mask for both channels.
"""

from __future__ import annotations

import math

import numpy as np
import torch
from torch import nn

N = 1024
M = 128
K = N // 2 + 1
LATENCY = 2 * M - 1
HIDDEN = 192
LAYERS = 2
TAU_SECONDS = 1.0
LEVEL_FLOOR = -7.0
FEATURE_SCALE = 4.0
FEATURE_CLAMP = 5.0
EPS = 1e-10


def _hann(length: int, n: np.ndarray) -> np.ndarray:
    """Periodic Hann of `length`, at positions `n`."""
    return 0.5 - 0.5 * np.cos(2.0 * math.pi * n / length)


def windows() -> tuple[np.ndarray, np.ndarray]:
    """The asymmetric analysis and synthesis windows (float64, length N)."""
    idx = np.arange(N, dtype=np.float64)
    rise = N - M
    analysis = np.empty(N)
    analysis[:rise] = np.sqrt(_hann(2 * rise, idx[:rise]))
    analysis[rise:] = np.sqrt(_hann(2 * M, idx[rise:] - (N - 2 * M)))
    synthesis = np.zeros(N)
    synthesis[N - 2 * M :] = (
        _hann(2 * M, idx[N - 2 * M :] - (N - 2 * M)) / analysis[N - 2 * M :]
    )
    return analysis, synthesis


def stft(x: torch.Tensor, analysis: torch.Tensor) -> torch.Tensor:
    """Causal frames of `x` [..., T] -> complex [..., T // M, K]."""
    x = torch.nn.functional.pad(x, (N - M, 0))
    frames = x.unfold(-1, N, M)
    return torch.fft.rfft(frames * analysis, dim=-1)


def istft(spec: torch.Tensor, synthesis: torch.Tensor, samples: int) -> torch.Tensor:
    """Overlap-add of `spec` [..., F, K]: out[i] is the rebuilt input[i - LATENCY].

    Frame f's synthesis tail covers input samples f M - M ... f M + M - 1; the
    streaming DSP emits each sample once no later frame can touch it.
    """
    frames = torch.fft.irfft(spec, n=N, dim=-1)[..., N - 2 * M :]
    frames = frames * synthesis[N - 2 * M :]
    lead = frames.shape[:-2]
    count = frames.shape[-2]
    # buf[j + M] holds rebuilt input sample j.
    buf = torch.zeros(*lead, (count + 1) * M, dtype=frames.dtype, device=frames.device)
    buf[..., : count * M] += frames[..., :M].reshape(*lead, count * M)
    buf[..., M : (count + 1) * M] += frames[..., M:].reshape(*lead, count * M)
    # out[i] = rebuilt[i - (2M - 1)] = buf[i - M + 1]
    out = torch.nn.functional.pad(buf, (M - 1, 0))
    if out.shape[-1] < samples:
        out = torch.nn.functional.pad(out, (0, samples - out.shape[-1]))
    return out[..., :samples]


def features(spec: torch.Tensor, sample_rate: float) -> torch.Tensor:
    """Model input from a spectrum [B, C, F, K] -> [B, F, K]."""
    power = (spec.real**2 + spec.imag**2).mean(dim=1)
    lp = torch.log10(power + EPS)
    level = lp.mean(dim=-1).clamp_min(LEVEL_FLOOR)  # [B, F]
    alpha = math.exp(-M / (sample_rate * TAU_SECONDS))
    mu = running_level(level, alpha)
    return ((lp - mu.unsqueeze(-1)) / FEATURE_SCALE).clamp(-FEATURE_CLAMP, FEATURE_CLAMP)


_WEIGHTS: dict = {}
_BLOCK = 1024


def running_level(level: torch.Tensor, alpha: float) -> torch.Tensor:
    """mu[0] = level[0], mu[f] = alpha mu[f-1] + (1-alpha) level[f], over [B, F].

    A matrix product per block of frames (so a whole song does not need an
    F x F matrix), carrying mu from one block into the next.
    """
    count = level.shape[1]
    first = min(count, _BLOCK)
    mu = [level[:, :first] @ _level_weights(first, alpha, level.device, level.dtype, True)]
    for start in range(first, count, _BLOCK):
        block = level[:, start : start + _BLOCK]
        n = torch.arange(1, block.shape[1] + 1, device=level.device, dtype=level.dtype)
        carry = mu[-1][:, -1:]
        weights = _level_weights(block.shape[1], alpha, level.device, level.dtype, False)
        mu.append(block @ weights + carry * alpha**n)
    return torch.cat(mu, dim=1)


def _level_weights(count: int, alpha: float, device, dtype, seeded: bool) -> torch.Tensor:
    """W[j, f] = (1-alpha) alpha^(f-j) for j <= f; seeded, row 0 is alpha^f instead
    (the first frame starts mu at its own level)."""
    key = (count, alpha, str(device), dtype, seeded)
    if key not in _WEIGHTS:
        f = torch.arange(count, dtype=torch.float64)
        j = f.unsqueeze(1)  # row: source frame j, column: output frame f
        w = torch.where(j <= f, (1.0 - alpha) * alpha ** (f - j).clamp_min(0), torch.zeros(()))
        if seeded:
            w[0] = alpha**f
        _WEIGHTS[key] = w.to(device=device, dtype=dtype)
    return _WEIGHTS[key]


class DrumSilencerNet(nn.Module):
    arch = "v1"

    def __init__(self, bins: int = K, hidden: int = HIDDEN, layers: int = LAYERS):
        super().__init__()
        self.inp = nn.Linear(bins, hidden)
        self.gru = nn.GRU(hidden, hidden, num_layers=layers, batch_first=True)
        self.out = nn.Linear(hidden, bins)
        self.skip = nn.Parameter(torch.zeros(bins))

    def forward(self, feat: torch.Tensor, state: torch.Tensor | None = None):
        """feat [B, F, K] -> keep-mask [B, F, K] in (0, 1), and the GRU state."""
        h = torch.relu(self.inp(feat))
        h, state = self.gru(h, state)
        return torch.sigmoid(self.out(h) + self.skip * feat), state


CONV_CHANNELS = 16
CONV_TIME = 4  # frames: this one and three before
CONV_FREQ = 5  # bins: this one and two either side
HEAD = 16


class DrumSilencerNet2(nn.Module):
    """v2: the GRU gives each bin context; a causal time x frequency
    convolution gives each bin its own recent shape; a small head shared by
    every bin combines them.

    * global: Linear(K -> H) + ReLU, GRU x LAYERS, Linear(H -> 2K): two
      context values per bin;
    * local: Conv2d(1 -> C, (CONV_TIME, CONV_FREQ)) + ReLU over the features,
      padded with zeros three frames into the past and two bins at each edge;
    * head, per bin: [local C, context 2, feature 1] -> Linear(C+3 -> HEAD) +
      ReLU -> Linear(HEAD -> 1) -> sigmoid.
    """

    arch = "v2"

    def __init__(self, bins: int = K, hidden: int = HIDDEN, layers: int = LAYERS):
        super().__init__()
        self.inp = nn.Linear(bins, hidden)
        self.gru = nn.GRU(hidden, hidden, num_layers=layers, batch_first=True)
        self.ctx = nn.Linear(hidden, 2 * bins)
        self.conv = nn.Conv2d(1, CONV_CHANNELS, (CONV_TIME, CONV_FREQ))
        self.head1 = nn.Linear(CONV_CHANNELS + 3, HEAD)
        self.head2 = nn.Linear(HEAD, 1)

    def forward(self, feat: torch.Tensor, state: torch.Tensor | None = None):
        b, f, k = feat.shape
        h = torch.relu(self.inp(feat))
        h, state = self.gru(h, state)
        ctx = self.ctx(h).view(b, f, k, 2)
        x = torch.nn.functional.pad(
            feat.unsqueeze(1), (CONV_FREQ // 2, CONV_FREQ // 2, CONV_TIME - 1, 0)
        )
        local = torch.relu(self.conv(x)).permute(0, 2, 3, 1)  # [B, F, K, C]
        z = torch.cat([local, ctx, feat.unsqueeze(-1)], dim=-1)
        mask = torch.sigmoid(self.head2(torch.relu(self.head1(z)))).squeeze(-1)
        return mask, state


def build(arch: str = "v2", hidden: int = HIDDEN):
    return (DrumSilencerNet2 if arch == "v2" else DrumSilencerNet)(hidden=hidden)


def separate(model: DrumSilencerNet, audio: np.ndarray, sample_rate: float, device="cpu"):
    """Run the whole streaming chain offline on `audio` [C, T] (float32).

    Returns (keep, drums), each [C, T], delayed by LATENCY like the plug-in.
    """
    analysis, synthesis = (torch.tensor(w, dtype=torch.float32, device=device) for w in windows())
    x = torch.tensor(audio, dtype=torch.float32, device=device).unsqueeze(0)
    spec = stft(x, analysis)
    with torch.no_grad():
        mask, _ = model(features(spec, sample_rate))
    keep = istft(spec * mask.unsqueeze(1), synthesis, audio.shape[-1])
    drums = istft(spec * (1.0 - mask).unsqueeze(1), synthesis, audio.shape[-1])
    return keep[0].cpu().numpy(), drums[0].cpu().numpy()
