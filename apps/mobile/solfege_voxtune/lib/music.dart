import 'dart:math' as math;

const double kBpm = 120;
const int kBeatsPerBar = 4;
const double kPhraseBeats = 16;

class KeyDef {
  final String name;
  final int pc;
  const KeyDef(this.name, this.pc);
}

const List<KeyDef> kKeys = [
  KeyDef('C', 0),
  KeyDef('C#', 1),
  KeyDef('D', 2),
  KeyDef('Eb', 3),
  KeyDef('E', 4),
  KeyDef('F', 5),
  KeyDef('F#', 6),
  KeyDef('G', 7),
  KeyDef('Ab', 8),
  KeyDef('A', 9),
  KeyDef('Bb', 10),
  KeyDef('B', 11),
];

const List<String> _sharp = [
  'C',
  'C#',
  'D',
  'D#',
  'E',
  'F',
  'F#',
  'G',
  'G#',
  'A',
  'A#',
  'B',
];

const List<String> _flat = [
  'C',
  'Db',
  'D',
  'Eb',
  'E',
  'F',
  'Gb',
  'G',
  'Ab',
  'A',
  'Bb',
  'B',
];

const List<String> _solfege = [
  'do',
  'di',
  're',
  'me',
  'mi',
  'fa',
  'fi',
  'sol',
  'le',
  'la',
  'te',
  'ti',
];

const Set<String> _flatKeys = {'F', 'Bb', 'Eb', 'Ab', 'Db', 'Gb'};

const Map<String, List<int>> kModes = {
  'minor': [0, 2, 3, 5, 7, 8, 10],
  'major': [0, 2, 4, 5, 7, 9, 11],
  'penta': [0, 3, 5, 7, 10],
};

const Map<String, String> kModeLabels = {
  'minor': 'Natural minor',
  'major': 'Major',
  'penta': 'Minor pentatonic',
};

const Map<String, String> kModeShort = {
  'minor': 'min',
  'major': 'maj',
  'penta': 'pent',
};

String solfegeForMidi(num midi, String key) {
  final pc = ((midi.round() % 12) + 12) % 12;
  final root = keyPc(key);
  final rel = (pc + 12 - root) % 12;
  return _solfege[rel];
}

class VocalNote {
  final String id;
  String lyric;
  final bool unpitched;
  final double detected;
  double pitch;
  final double detectedStart;
  double start;
  final double detectedDur;
  double dur;
  double modulation;
  double formant;
  double gainDb;
  final double detectedGain;
  bool pinned;
  final double scoop;
  final double drift;
  final double vib;
  final double vibCycles;
  final double vibPhase;
  final List<double> contour;
  final List<double> envelope;

  VocalNote({
    required this.id,
    required this.lyric,
    required this.unpitched,
    required this.detected,
    required this.pitch,
    required this.detectedStart,
    required this.start,
    required this.detectedDur,
    required this.dur,
    this.modulation = 1.0,
    this.formant = 0.0,
    required this.gainDb,
    required this.detectedGain,
    this.pinned = false,
    this.scoop = 0.0,
    this.drift = 0.0,
    this.vib = 0.0,
    this.vibCycles = 0.0,
    this.vibPhase = 0.0,
    this.contour = const [],
    this.envelope = const [],
  });

  factory VocalNote.fromJson(Map<String, dynamic> json) {
    final contourRaw = (json['contour'] as List<dynamic>?) ?? const [];
    final envRaw = (json['envelope'] as List<dynamic>?) ?? const [];
    return VocalNote(
      id: (json['id'] as String?) ?? 'n1',
      lyric: (json['lyric'] as String?) ?? '',
      unpitched: (json['unpitched'] as bool?) ?? false,
      detected: ((json['detected'] as num?) ?? 69.0).toDouble(),
      pitch: ((json['pitch'] as num?) ?? 69.0).toDouble(),
      detectedStart: ((json['detected_start'] as num?) ?? 0.0).toDouble(),
      start: ((json['start'] as num?) ?? 0.0).toDouble(),
      detectedDur: ((json['detected_dur'] as num?) ?? 0.2).toDouble(),
      dur: ((json['dur'] as num?) ?? 0.2).toDouble(),
      modulation: ((json['modulation'] as num?) ?? 1.0).toDouble(),
      formant: ((json['formant'] as num?) ?? 0.0).toDouble(),
      gainDb: ((json['gain_db'] as num?) ?? 0.0).toDouble(),
      detectedGain: ((json['detected_gain'] as num?) ?? 0.0).toDouble(),
      pinned: (json['pinned'] as bool?) ?? false,
      scoop: ((json['scoop'] as num?) ?? 0.0).toDouble(),
      drift: ((json['drift'] as num?) ?? 0.0).toDouble(),
      vib: ((json['vib'] as num?) ?? 0.0).toDouble(),
      vibCycles: ((json['vib_cycles'] as num?) ?? 0.0).toDouble(),
      vibPhase: ((json['vib_phase'] as num?) ?? 0.0).toDouble(),
      contour: contourRaw.map((e) => (e as num).toDouble()).toList(),
      envelope: envRaw.map((e) => (e as num).toDouble()).toList(),
    );
  }

  VocalNote copyWith({
    String? lyric,
    double? pitch,
    double? start,
    double? dur,
    double? modulation,
    double? formant,
    double? gainDb,
    bool? pinned,
  }) {
    return VocalNote(
      id: id,
      lyric: lyric ?? this.lyric,
      unpitched: unpitched,
      detected: detected,
      pitch: pitch ?? this.pitch,
      detectedStart: detectedStart,
      start: start ?? this.start,
      detectedDur: detectedDur,
      dur: dur ?? this.dur,
      modulation: modulation ?? this.modulation,
      formant: formant ?? this.formant,
      gainDb: gainDb ?? this.gainDb,
      detectedGain: detectedGain,
      pinned: pinned ?? this.pinned,
      scoop: scoop,
      drift: drift,
      vib: vib,
      vibCycles: vibCycles,
      vibPhase: vibPhase,
      contour: contour,
      envelope: envelope,
    );
  }
}

class NoteSnapshot {
  final String id;
  final double pitch;
  final double modulation;
  final double formant;
  final double gainDb;
  final double start;
  final double dur;
  final bool pinned;

  const NoteSnapshot({
    required this.id,
    required this.pitch,
    required this.modulation,
    required this.formant,
    required this.gainDb,
    required this.start,
    required this.dur,
    required this.pinned,
  });

  factory NoteSnapshot.fromNote(VocalNote note) => NoteSnapshot(
        id: note.id,
        pitch: note.pitch,
        modulation: note.modulation,
        formant: note.formant,
        gainDb: note.gainDb,
        start: note.start,
        dur: note.dur,
        pinned: note.pinned,
      );

  bool matches(NoteSnapshot other) =>
      id == other.id &&
      pitch == other.pitch &&
      modulation == other.modulation &&
      formant == other.formant &&
      gainDb == other.gainDb &&
      start == other.start &&
      dur == other.dur &&
      pinned == other.pinned;
}

class EditorSnapshot {
  final double correct;
  final String key;
  final String mode;
  final List<NoteSnapshot> notes;

  const EditorSnapshot({
    required this.correct,
    required this.key,
    required this.mode,
    required this.notes,
  });

  bool matches(EditorSnapshot other) {
    if (correct != other.correct || key != other.key || mode != other.mode) {
      return false;
    }
    if (notes.length != other.notes.length) return false;
    for (int i = 0; i < notes.length; i++) {
      if (!notes[i].matches(other.notes[i])) return false;
    }
    return true;
  }
}

double clampVal(double value, double min, double max) {
  return math.min(max, math.max(min, value));
}

double beatsToSec(double beats, [double bpm = kBpm]) {
  return (beats * 60.0) / bpm;
}

double phraseDuration([double bpm = kBpm]) {
  return beatsToSec(kPhraseBeats, bpm);
}

int keyPc(String name) {
  for (final k in kKeys) {
    if (k.name == name) return k.pc;
  }
  return 0;
}

bool useFlats(String key) {
  return _flatKeys.contains(key);
}

String spellMidi(num midi, String key) {
  final rounded = midi.round();
  final pc = ((rounded % 12) + 12) % 12;
  final octave = (rounded / 12).floor() - 1;
  final names = useFlats(key) ? _flat : _sharp;
  return '${names[pc]}$octave';
}

List<int> scalePcs(String key, String mode) {
  final root = keyPc(key);
  final degrees = kModes[mode] ?? kModes['minor']!;
  return degrees.map((d) => (d + root) % 12).toList();
}

int nearestScale(double midi, String key, String mode) {
  if (mode == 'chromatic') return midi.round();
  final pcs = scalePcs(key, mode).toSet();
  final base = midi.round();
  int best = base;
  double bestDist = double.infinity;
  for (int note = base - 4; note <= base + 4; note += 1) {
    final pc = ((note % 12) + 12) % 12;
    if (!pcs.contains(pc)) continue;
    final dist = (note - midi).abs();
    if (dist < bestDist) {
      bestDist = dist;
      best = note;
    }
  }
  return best;
}

({int target, int cents}) centsFromTarget(double midi, String key, String mode) {
  final target = nearestScale(midi, key, mode);
  return (target: target, cents: ((midi - target) * 100).round());
}

double midiToHz(double midi) {
  return 440.0 * math.pow(2.0, (midi - 69.0) / 12.0);
}

String formatCents(num cents) {
  final rounded = cents.round();
  if (rounded == 0) return '0¢';
  final sign = rounded > 0 ? '+' : '−';
  return '$sign${rounded.abs()}¢';
}

String formatSigned(double value, [int digits = 1]) {
  final text = value.abs().toStringAsFixed(digits);
  if (value > 0.0001) return '+$text';
  if (value < -0.0001) return '−$text';
  return 0.0.toStringAsFixed(digits);
}

String formatDb(double db) {
  final rounded = (db * 10).round() / 10.0;
  if (rounded.abs() < 0.05) return '0.0 dB';
  return '${formatSigned(rounded, 1)} dB';
}

String formatClock(double sec) {
  final safe = sec.isFinite ? math.max(0.0, sec) : 0.0;
  final minutes = (safe / 60).floor();
  final seconds = safe - minutes * 60;
  final whole = seconds.floor();
  final frac = math.min(99, ((seconds - whole) * 100).floor());
  return '$minutes:${whole.toString().padLeft(2, '0')}.${frac.toString().padLeft(2, '0')}';
}

String formatBars(double sec, [double bpm = kBpm]) {
  final safe = sec.isFinite ? math.max(0.0, sec) : 0.0;
  final beats = safe / (60.0 / bpm);
  final bar = (beats / kBeatsPerBar).floor() + 1;
  final beat = (beats % kBeatsPerBar).floor() + 1;
  final sixteenth = math.min(4, ((beats % 1.0) * 4).floor() + 1);
  return '$bar.$beat.$sixteenth';
}

double smoothstep(double value) {
  final x = clampVal(value, 0.0, 1.0);
  return x * x * (3.0 - 2.0 * x);
}

double _sampleCurve(List<double> curve, double t) {
  if (curve.isEmpty) return 0.0;
  if (curve.length == 1) return curve.first;
  final pos = clampVal(t, 0.0, 1.0) * (curve.length - 1);
  final idx = pos.floor().clamp(0, curve.length - 1);
  final next = (idx + 1).clamp(0, curve.length - 1);
  final frac = pos - idx;
  return curve[idx] + (curve[next] - curve[idx]) * frac;
}

double energy(VocalNote note, double t) {
  if (note.envelope.isNotEmpty) {
    final raw = _sampleCurve(note.envelope, t);
    final edge = t < 0.06
        ? smoothstep(t / 0.06)
        : t > 0.92
            ? smoothstep((1.0 - t) / 0.08)
            : 1.0;
    return math.max(note.unpitched ? 0.30 : 0.18, raw * edge);
  }
  final attack = note.unpitched ? 0.4 : 0.07;
  final release = note.unpitched ? 0.45 : 0.18;
  double env = 1.0;
  if (t < attack) {
    env = smoothstep(t / attack);
  } else if (t > 1.0 - release) {
    env = smoothstep((1.0 - t) / release);
  }
  if (!note.unpitched) {
    env *= 0.84 + 0.16 * math.sin(math.min(1.0, math.max(0.0, t)) * math.pi);
  }
  return math.max(note.unpitched ? 0.35 : 0.2, env);
}

double contourCents(VocalNote note, double t) {
  if (note.unpitched) return 0.0;
  final amount = note.modulation;
  if (note.contour.isNotEmpty) {
    return _sampleCurve(note.contour, t) * amount;
  }
  double cents = 0.0;
  const scoopLength = 0.18;
  if (note.scoop != 0 && t < scoopLength) {
    final u = t / scoopLength;
    cents += note.scoop * (1.0 - smoothstep(u));
  }
  if (note.drift != 0) {
    cents += note.drift * (t - 0.35);
  }
  if (note.vib != 0) {
    final env = clampVal((t - 0.16) / 0.28, 0.0, 1.0);
    final phase = note.vibPhase;
    final cycles = note.vibCycles == 0 ? 4.0 : note.vibCycles;
    cents += math.sin((t * cycles + phase) * math.pi * 2.0) * note.vib * env;
  }
  return cents * amount;
}

double pitchAt(VocalNote note, double t) {
  return note.pitch + contourCents(note, t) / 100.0;
}

double halfSemi(VocalNote note, double t) {
  final env = energy(note, t);
  final level = math.pow(10.0, note.gainDb / 20.0).toDouble();
  final amp = 0.62 + 0.5 * clampVal(level, 0.35, 1.55);
  final bulk = note.unpitched ? 0.7 : 1.0;
  return (0.16 + 0.5 * env * amp) * bulk;
}

VocalNote presentNote(VocalNote note, bool bypass) {
  if (!bypass) return note;
  return note.copyWith(
    pitch: note.detected,
    start: note.detectedStart,
    dur: note.detectedDur,
    modulation: 1.0,
    formant: 0.0,
    gainDb: note.detectedGain,
  );
}
