"""Convert the MIT KEMAR "compact" HRTF set into `data/mit_kemar.hrir`.

Source: Bill Gardner and Keith Martin, "HRTF Measurements of a KEMAR
Dummy-Head Microphone", MIT Media Lab Perceptual Computing Technical Report
#280, 1994 — https://sound.media.mit.edu/resources/KEMAR.html (compact.zip).
The data are provided free with no restrictions on use, provided the authors
are cited (see `data/MIT_KEMAR_NOTICE.md`).

What the renderer wants, and what this does to get it:

* **Time and filter apart.** Each measured response becomes a minimum-phase
  filter plus the interaural time difference, measured by cross-correlating
  the two ears below 1.5 kHz (where the ear listens to time). Min-phase
  filters interpolate between neighbouring directions without the comb
  filtering two differently delayed responses would make, and the delay is
  interpolated on its own.
* **Diffuse-field equalised.** KEMAR's ear canal and the measurement chain
  colour every response the same way; dividing by the average over all
  directions (1/3-octave smoothed, limited to ±15 dB) leaves only what
  changes with direction, so a mix keeps its tone.
* **A full low end.** 128 taps at 44.1 kHz cannot hold anything below a few
  hundred hertz, where the head does not shade anyway: below 250 Hz each
  response is taken flat at its 250 Hz level.
* **Front and back told apart.** A dummy head's front/back difference is a
  few dB of treble, which a listener whose own ears are not KEMAR's mostly
  does not hear: behind sounds like in front. Each response's log magnitude
  is compared with its mirror image across the ears' axis (azimuth
  `180 - az`, the direction it is confused with), and what differs between
  the two is scaled by `FRONT_BACK_CONTRAST`, above 1 kHz only, lifting
  by at most `CONTRAST_MAX_LIFT_DB` (the front, the direction most of a mix
  sits in, stays close to neutral) and cutting by at most
  `CONTRAST_MAX_CUT_DB` (behind gets darker). The time
  difference and the level difference between the ears are untouched, so
  left/right is as measured.
* **Level.** Normalised so the average ear hears -3 dB: a centred mono
  source lands where a centre-panned one does.

Usage: python kemar_to_hrir.py <folder with elev*/ from compact.zip> <out>
Requires numpy and scipy.

File layout (little endian):
    magic b"SHRIR1\\0\\0", u32 sample_rate, u32 taps, u32 rings,
    per ring:  i32 elevation_deg, u32 count, count x (f32 azimuth_deg,
               f32 itd_seconds, taps x i16 left, taps x i16 right),
    then f32 scale (i16 -> float).
Azimuths are the measured half, 0..180 clockwise (toward the right ear);
the renderer mirrors them for the left. `itd_seconds` is the left ear's
delay minus the right ear's (positive: the source is on the right).
"""

import glob
import os
import struct
import sys
import wave

import numpy as np
from scipy.signal import butter, sosfiltfilt

RATE = 44_100
NFFT = 1024
TAPS = 96
LF_FLAT_HZ = 250.0
FRONT_BACK_CONTRAST = 2.5
CONTRAST_FROM_HZ = 700.0
CONTRAST_FULL_HZ = 1_500.0
CONTRAST_MAX_LIFT_DB = 3.0
CONTRAST_MAX_CUT_DB = 9.0


def load(path):
    w = wave.open(path)
    assert w.getframerate() == RATE and w.getnchannels() == 2
    d = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").reshape(-1, 2)
    return d.astype(np.float64) / 32768.0


def itd_seconds(left, right):
    """Left-ear delay minus right-ear delay, from the low band."""
    sos = butter(4, 1_500.0, "low", fs=RATE, output="sos")
    up = 32
    # Band-limit, then upsample by zero-padding the spectrum.
    lf = sosfiltfilt(sos, np.pad(left, (64, 64)))
    rf = sosfiltfilt(sos, np.pad(right, (64, 64)))
    m = len(lf) * up
    lu = np.fft.irfft(np.fft.rfft(lf), m) * up
    ru = np.fft.irfft(np.fft.rfft(rf), m) * up
    xc = np.fft.irfft(np.fft.rfft(lu, 2 * m) * np.conj(np.fft.rfft(ru, 2 * m)))
    lag = int(np.argmax(np.concatenate([xc[-m:], xc[:m]]))) - m
    # Physical limit: about 0.8 ms either way.
    lag = int(np.clip(lag, -0.0009 * RATE * up, 0.0009 * RATE * up))
    return lag / (RATE * up)


def smooth_fraction_octave(power, freqs, fraction):
    out = np.empty_like(power)
    for i, f in enumerate(freqs):
        if f <= 0:
            out[i] = power[i]
            continue
        lo, hi = f * 2 ** (-0.5 / fraction), f * 2 ** (0.5 / fraction)
        band = (freqs >= lo) & (freqs <= hi)
        out[i] = power[band].mean()
    return out


def min_phase(magnitude):
    """Minimum-phase impulse response of a one-sided magnitude (NFFT/2+1)."""
    full = np.concatenate([magnitude, magnitude[-2:0:-1]])
    cep = np.fft.ifft(np.log(np.maximum(full, 1e-8))).real
    fold = np.zeros(NFFT)
    fold[0] = cep[0]
    fold[1 : NFFT // 2] = 2 * cep[1 : NFFT // 2]
    fold[NFFT // 2] = cep[NFFT // 2]
    return np.fft.ifft(np.exp(np.fft.fft(fold))).real


def main(src, out):
    rings = {}
    for path in glob.glob(os.path.join(src, "elev*", "*.wav")):
        elev = int(os.path.basename(os.path.dirname(path))[4:])
        az = int(os.path.basename(path).split("e")[1][:3])
        rings.setdefault(elev, []).append((az, load(path)))
    for elev in rings:
        rings[elev].sort(key=lambda item: item[0])

    freqs = np.fft.rfftfreq(NFFT, 1 / RATE)
    mags = {}
    power_sum = np.zeros(len(freqs))
    count = 0
    for elev, items in rings.items():
        for az, d in items:
            ml = np.abs(np.fft.rfft(d[:, 0], NFFT))
            mr = np.abs(np.fft.rfft(d[:, 1], NFFT))
            mags[(elev, az)] = (ml, mr)
            # Each compact response stands for itself and its mirror image;
            # weight rings by the solid angle each of their points covers.
            weight = np.cos(np.radians(elev)) * 360.0 / max(len(items) * 2 - 2, 1)
            if elev == 90:
                weight = 360.0 / 72.0 * 0.05
            power_sum += weight * (ml**2 + mr**2)
            count += weight * 2
    diffuse = smooth_fraction_octave(power_sum / count, freqs, 3.0)
    eq = 1.0 / np.sqrt(np.maximum(diffuse, 1e-12))
    ref = eq[(freqs > 500) & (freqs < 2_000)].mean()
    eq = np.clip(eq / ref, 10 ** (-15 / 20), 10 ** (15 / 20)) * ref

    lf_bin = int(np.searchsorted(freqs, LF_FLAT_HZ))
    hf_bin = int(np.searchsorted(freqs, 18_000.0))
    fade = np.hanning(2 * 16)[16:]

    def smoothed_db(m):
        m = smooth_fraction_octave((m * eq) ** 2, freqs, 12.0) ** 0.5
        m[:lf_bin] = m[lf_bin]
        m[hf_bin:] = m[hf_bin]
        return 20 * np.log10(np.maximum(m, 1e-9))

    db = {key: (smoothed_db(ml), smoothed_db(mr)) for key, (ml, mr) in mags.items()}

    def ring_db(elev, az, ear):
        """An ear's response in `elev`'s ring at `az` (0..180), interpolated
        between the measured azimuths."""
        azs = [a for a, _ in rings[elev]]
        if az <= azs[0]:
            return db[(elev, azs[0])][ear]
        if az >= azs[-1]:
            return db[(elev, azs[-1])][ear]
        j = int(np.searchsorted(azs, az))
        a0, a1 = azs[j - 1], azs[j]
        w = (az - a0) / (a1 - a0)
        return (1 - w) * db[(elev, a0)][ear] + w * db[(elev, a1)][ear]

    # Where the contrast applies: from nothing at CONTRAST_FROM_HZ to all of
    # it at CONTRAST_FULL_HZ, on a log scale.
    ramp = np.clip(
        np.log(np.maximum(freqs, 1.0) / CONTRAST_FROM_HZ)
        / np.log(CONTRAST_FULL_HZ / CONTRAST_FROM_HZ),
        0.0,
        1.0,
    )

    def shaped(elev, az, ear, contrast=True):
        own = db[(elev, az)][ear]
        if contrast:
            mirror = ring_db(elev, 180 - az, ear)
            added = (FRONT_BACK_CONTRAST - 1.0) * 0.5 * (own - mirror) * ramp
            own = own + np.clip(added, -CONTRAST_MAX_CUT_DB, CONTRAST_MAX_LIFT_DB)
        h = min_phase(10 ** (own / 20))[:TAPS].copy()
        h[-16:] *= fade
        return h

    out_rings = []
    energies = []
    kept = []
    for elev in sorted(rings):
        entries = []
        for az, d in rings[elev]:
            ml, mr = mags[(elev, az)]
            hl, hr = shaped(elev, az, 0), shaped(elev, az, 1)
            full = min_phase(ml * eq)
            kept.append(np.sum(full[:TAPS] ** 2) / np.sum(full**2))
            # The level is set from the measured responses, before the
            # contrast: it only moves the tone of a direction, not the mix.
            energies += [
                np.sum(shaped(elev, az, 0, False) ** 2),
                np.sum(shaped(elev, az, 1, False) ** 2),
            ]
            entries.append((az, itd_seconds(d[:, 0], d[:, 1]), hl, hr))
        out_rings.append((elev, entries))

    gain = np.sqrt(0.5 / np.mean(energies))
    peak = max(np.abs(h).max() for _, e in out_rings for _, _, hl, hr in e for h in (hl, hr))
    scale = peak * gain / 32767.0

    with open(out, "wb") as f:
        f.write(b"SHRIR1\0\0")
        f.write(struct.pack("<III", RATE, TAPS, len(out_rings)))
        for elev, entries in out_rings:
            f.write(struct.pack("<iI", elev, len(entries)))
            for az, itd, hl, hr in entries:
                f.write(struct.pack("<ff", float(az), float(itd)))
                for h in (hl, hr):
                    q = np.round(h * gain / scale).astype("<i2")
                    f.write(q.tobytes())
        f.write(struct.pack("<f", scale))

    print(f"{sum(len(e) for _, e in out_rings)} responses, {TAPS} taps, "
          f"energy kept in {TAPS} taps: min {min(kept):.4f}")
    for elev, entries in out_rings:
        if elev == 0:
            for az, itd, hl, hr in entries[::6]:
                print(f"  az {az:3d}: itd {itd * 1e6:+7.1f} us, "
                      f"L {10 * np.log10(np.sum((hl * gain) ** 2)):+5.1f} dB, "
                      f"R {10 * np.log10(np.sum((hr * gain) ** 2)):+5.1f} dB")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
