"""Octave-band RT60 (T20, Schroeder, noise-floor compensated), DRR and C50
of the MIT IR Survey, grouped by the kind of space Virtual Speaker models.

Traer & McDermott, "Statistics of natural reverberation enable perceptual
separation of sound and space", PNAS 2016. CC BY 4.0. Source-microphone
distance 1.5 m in every space.

Usage: python mit_ir_survey_stats.py  (with `survey.parquet`, the Hugging Face
repack `benjamin-paine/mit-impulse-response-survey`, in the working
directory). Requires numpy and scipy. The results calibrate the Virtual
Speaker rooms in `src/listening.rs` (`rooms_measure_like_real_rooms`); no
survey audio is shipped."""
import io, json, wave
import numpy as np
from scipy.signal import butter, sosfiltfilt
from parquet_lite import read

GROUPS = {
    'car': ['Car'],
    'bedroom': ['Bedroom', 'MasterBedroom', 'BabysRoom'],
    'living': ['LivingRoom'],
    'dining_kitchen': ['DiningRoom', 'Diningroom', 'Kitchen'],
    'office': ['Office', 'DoctorsOffice', 'ComputerRoom'],
    'bar': ['Bar', 'WineBar', 'Pizzeria', 'Restaurant'],
    'theater': ['MovieTheater', 'Auditorium'],
}
BANDS = [125, 250, 500, 1000, 2000, 4000, 8000]


def decode(b):
    w = wave.open(io.BytesIO(b))
    n, ch, sw, sr = w.getnframes(), w.getnchannels(), w.getsampwidth(), w.getframerate()
    raw = w.readframes(n)
    if sw == 3:
        a = np.frombuffer(raw, np.uint8).reshape(-1, 3)
        x = (a[:, 0].astype(np.int32) | (a[:, 1].astype(np.int32) << 8) | (a[:, 2].astype(np.int32) << 16))
        x = np.where(x >= 1 << 23, x - (1 << 24), x) / float(1 << 23)
    elif sw == 2:
        x = np.frombuffer(raw, '<i2') / 32768.0
    else:
        x = np.frombuffer(raw, '<i4') / float(1 << 31)
    x = x.reshape(-1, ch)[:, 0]
    return x.astype(np.float64), sr


def t20(ir, sr):
    """T20 from the Schroeder curve, after subtracting the noise floor
    (taken from the last 10 %) and cutting where the decay meets it."""
    e = ir ** 2
    onset = int(np.argmax(e))
    e = e[onset:]
    tail = e[int(len(e) * 0.9):]
    noise = tail.mean()
    # Where the smoothed energy falls to the noise floor.
    win = max(1, int(0.01 * sr))
    smooth = np.convolve(e, np.ones(win) / win, mode='same')
    cut = np.argmax(smooth < noise * 2.0) or len(e)
    ec = np.clip(e[:cut] - noise, 0, None)
    edc = np.cumsum(ec[::-1])[::-1]
    if edc[0] <= 0:
        return None
    db = 10 * np.log10(np.maximum(edc / edc[0], 1e-30))
    try:
        i5 = np.argmax(db <= -5); i25 = np.argmax(db <= -25)
    except ValueError:
        return None
    if i25 <= i5 or db[i25] > -24:
        return None
    t = np.arange(i5, i25) / sr
    slope = np.polyfit(t, db[i5:i25], 1)[0]
    return -60.0 / slope if slope < 0 else None


def clarity(ir, sr):
    e = ir ** 2
    onset = int(np.argmax(np.abs(ir) > 0.1 * np.abs(ir).max()))
    s = max(0, onset - int(0.0005 * sr))
    d = e[s:onset + int(0.0025 * sr)].sum()
    rest = e[onset + int(0.0025 * sr):].sum()
    early = e[s:onset + int(0.05 * sr)].sum()
    late = e[onset + int(0.05 * sr):].sum()
    return 10 * np.log10(d / rest), 10 * np.log10(early / late)


def main():
    schema, cols = read('survey.parquet')
    paths = [p.decode() for p in cols['audio.path']]
    results = {g: {'rt': {b: [] for b in BANDS}, 'drr': [], 'c50': [], 'n': 0} for g in GROUPS}
    for audio, path in zip(cols['audio.bytes'], paths):
        loc = path.split('_')[1]
        group = next((g for g, locs in GROUPS.items() if loc in locs), None)
        if group is None:
            continue
        ir, sr = decode(audio)
        r = results[group]; r['n'] += 1
        for b in BANDS:
            lo, hi = b / np.sqrt(2), min(b * np.sqrt(2), sr * 0.45)
            sos = butter(4, [lo, hi], 'bandpass', fs=sr, output='sos')
            v = t20(sosfiltfilt(sos, ir), sr)
            if v is not None and 0.02 < v < 5:
                r['rt'][b].append(v)
        mid = sosfiltfilt(butter(4, [500, 2000], 'bandpass', fs=sr, output='sos'), ir)
        drr, c50 = clarity(mid, sr)
        r['drr'].append(drr); r['c50'].append(c50)
    summary = {}
    for g, r in results.items():
        med = {b: (round(float(np.median(v)), 3) if v else None) for b, v in r['rt'].items()}
        summary[g] = {
            'n': r['n'],
            'rt60_median_by_octave': med,
            'drr_1p5m_mid_median_db': round(float(np.median(r['drr'])), 1) if r['drr'] else None,
            'c50_1p5m_mid_median_db': round(float(np.median(r['c50'])), 1) if r['c50'] else None,
            'c50_iqr': [round(float(np.percentile(r['c50'], q)), 1) for q in (25, 75)] if r['c50'] else None,
        }
    print(json.dumps(summary, indent=1))
    json.dump(summary, open('summary.json', 'w'), indent=1)


main()
