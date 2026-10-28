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

  /// Cut from the output (plays silence). Still shown and editable.
  bool muted;

  /// ลูกคอ (vocal ornament): style ([kOrnNone]…[kOrnFall]), width in
  /// semitones and speed 0 (slow) … 1 (fast). Rendered by the engine.
  int ornStyle;
  double ornDepth;
  double ornSpeed;

  /// Onset emphasis 0…1: up to +6 dB over the note's attack.
  double accent;
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
    this.muted = false,
    this.ornStyle = kOrnNone,
    this.ornDepth = 1.0,
    this.ornSpeed = 0.5,
    this.accent = 0.0,
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
      muted: (json['muted'] as bool?) ?? false,
      ornStyle: ((json['orn_style'] as num?) ?? kOrnNone).toInt(),
      ornDepth: ((json['orn_depth'] as num?) ?? 1.0).toDouble(),
      ornSpeed: ((json['orn_speed'] as num?) ?? 0.5).toDouble(),
      accent: ((json['accent'] as num?) ?? 0.0).toDouble(),
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
    bool? muted,
    int? ornStyle,
    double? ornDepth,
    double? ornSpeed,
    double? accent,
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
      muted: muted ?? this.muted,
      ornStyle: ornStyle ?? this.ornStyle,
      ornDepth: ornDepth ?? this.ornDepth,
      ornSpeed: ornSpeed ?? this.ornSpeed,
      accent: accent ?? this.accent,
      scoop: scoop,
      drift: drift,
      vib: vib,
      vibCycles: vibCycles,
      vibPhase: vibPhase,
      contour: contour,
      envelope: envelope,
    );
  }

  /// Independent copy (contour/envelope lists are never mutated in place,
  /// so they are shared).
  VocalNote copy() => copyWith();

  Map<String, dynamic> toJson() => {
    'id': id,
    'lyric': lyric,
    'unpitched': unpitched,
    'detected': detected,
    'pitch': pitch,
    'detected_start': detectedStart,
    'start': start,
    'detected_dur': detectedDur,
    'dur': dur,
    'modulation': modulation,
    'formant': formant,
    'gain_db': gainDb,
    'detected_gain': detectedGain,
    'pinned': pinned,
    'muted': muted,
    'orn_style': ornStyle,
    'orn_depth': ornDepth,
    'orn_speed': ornSpeed,
    'accent': accent,
    'scoop': scoop,
    'drift': drift,
    'vib': vib,
    'vib_cycles': vibCycles,
    'vib_phase': vibPhase,
    'contour': contour,
    'envelope': envelope,
  };

  /// Same structure and edit state (used to drop no-op undo steps).
  bool sameAs(VocalNote o) =>
      id == o.id &&
      detected == o.detected &&
      detectedStart == o.detectedStart &&
      detectedDur == o.detectedDur &&
      pitch == o.pitch &&
      start == o.start &&
      dur == o.dur &&
      modulation == o.modulation &&
      formant == o.formant &&
      gainDb == o.gainDb &&
      pinned == o.pinned &&
      muted == o.muted &&
      ornStyle == o.ornStyle &&
      ornDepth == o.ornDepth &&
      ornSpeed == o.ornSpeed &&
      accent == o.accent;

  /// Has a ลูกคอ or an accent.
  bool get hasExpression => ornStyle != kOrnNone || accent > 0;
}

/// ลูกคอ styles; must match `ORN_*` in the Rust engine.
const int kOrnNone = 0;

/// Quick flick up to the upper neighbour and back, right after the onset.
const int kOrnTurnUp = 1;

/// The same flick down to the lower neighbour.
const int kOrnTurnDown = 2;

/// Alternates with the upper neighbour through the note (trill-like).
const int kOrnShake = 3;

/// Drops away at the end of the note.
const int kOrnFall = 4;

double _lerp(double a, double b, double t) => a + (b - a) * clampVal(t, 0, 1);

double _bump(double t, double start, double len) {
  if (len <= 0 || t <= start || t >= start + len) return 0;
  return 0.5 - 0.5 * math.cos((t - start) / len * math.pi * 2);
}

/// Ornament pitch offset in semitones at normalized position [t]. Mirrors
/// `ornament_semis` in the engine, so the drawn curve is what plays.
double ornamentSemis(VocalNote note, double t) {
  if (note.ornStyle == kOrnNone || note.unpitched || note.ornDepth == 0) {
    return 0;
  }
  final dur = math.max(0.05, note.dur);
  final sec = clampVal(t, 0, 1) * dur;
  final depth = clampVal(note.ornDepth, -4, 4);
  final speed = clampVal(note.ornSpeed, 0, 1);
  switch (note.ornStyle) {
    case kOrnTurnUp:
    case kOrnTurnDown:
      final len = math.min(_lerp(0.26, 0.09, speed), dur * 0.6);
      final start = math.min(0.03, dur * 0.1);
      final sign = note.ornStyle == kOrnTurnUp ? 1.0 : -1.0;
      return sign * depth * _bump(sec, start, len);
    case kOrnShake:
      final hz = _lerp(4.5, 9.0, speed);
      final start = math.min(math.max(dur * 0.25, 0.08), dur * 0.5);
      final end = dur - math.min(0.05, dur * 0.1);
      if (sec <= start || sec >= end) return 0;
      final fadeIn = smoothstep((sec - start) / 0.08);
      final fadeOut = smoothstep((end - sec) / 0.05);
      final wave = 0.5 - 0.5 * math.cos((sec - start) * hz * math.pi * 2);
      return depth * wave * fadeIn * fadeOut;
    case kOrnFall:
      final len = math.min(_lerp(0.32, 0.12, speed), dur * 0.5);
      return -depth * smoothstep((sec - (dur - len)) / len);
  }
  return 0;
}

/// Cuts [note] at [time] (seconds, inside the note) into two notes that
/// each play their own part of the source. Each half gets its own pitch
/// centre from its part of the contour, so after a cut across a slide both
/// halves sit on their own pitch. Edits (offset from the detected pitch,
/// modulation, formant, level, mute) carry over to both halves.
///
/// Returns null when either half would be shorter than [minDur] seconds.
(VocalNote, VocalNote)? splitNote(
  VocalNote note,
  double time,
  String Function(String base) newId, {
  double minDur = 0.04,
}) {
  final r = (time - note.start) / note.dur;
  if (note.dur * r < minDur || note.dur * (1 - r) < minDur) return null;

  (double detected, List<double> contour, List<double> envelope) part(
    double from,
    double to,
  ) {
    const points = 32;
    final c = [
      for (var k = 0; k < points; k++)
        _sampleCurve(note.contour, from + (to - from) * k / (points - 1)),
    ];
    final e = [
      for (var k = 0; k < points; k++)
        _sampleCurve(note.envelope, from + (to - from) * k / (points - 1)),
    ];
    if (note.contour.isEmpty || note.unpitched) {
      return (
        note.detected,
        const <double>[],
        note.envelope.isEmpty ? const <double>[] : e,
      );
    }
    // Re-centre: this half's mean deviation becomes its detected pitch.
    final mean = c.reduce((a, b) => a + b) / c.length;
    return (
      note.detected + mean / 100.0,
      [for (final v in c) v - mean],
      note.envelope.isEmpty ? const <double>[] : e,
    );
  }

  final offset = note.pitch - note.detected; // the user's pitch edit
  VocalNote half(String id, double from, double to) {
    final (detected, contour, envelope) = part(from, to);
    return VocalNote(
      id: id,
      lyric: note.lyric,
      unpitched: note.unpitched,
      detected: detected,
      pitch: detected + offset,
      detectedStart: note.detectedStart + note.detectedDur * from,
      start: note.start + note.dur * from,
      detectedDur: note.detectedDur * (to - from),
      dur: note.dur * (to - from),
      modulation: note.modulation,
      formant: note.formant,
      gainDb: note.gainDb,
      detectedGain: note.detectedGain,
      pinned: note.pinned,
      muted: note.muted,
      // The onset flick and accent stay with the first half, the fall with
      // the second; a shake continues in both.
      ornStyle: switch (note.ornStyle) {
        kOrnTurnUp || kOrnTurnDown => from == 0 ? note.ornStyle : kOrnNone,
        kOrnFall => to == 1 ? kOrnFall : kOrnNone,
        _ => note.ornStyle,
      },
      ornDepth: note.ornDepth,
      ornSpeed: note.ornSpeed,
      accent: from == 0 ? note.accent : 0,
      contour: contour,
      envelope: envelope,
    );
  }

  return (half(newId(note.id), 0, r), half(newId(note.id), r, 1));
}

/// Joins [a] with the following note [b] into one note spanning both
/// (including any gap). The pitch centre is the duration-weighted mean; the
/// contour is rebuilt relative to it so the sung curve is unchanged.
VocalNote joinNotes(VocalNote a, VocalNote b, String id) {
  final start = a.start;
  final end = b.start + b.dur;
  final dur = end - start;
  final detStart = a.detectedStart;
  final detEnd = b.detectedStart + b.detectedDur;
  final detDur = detEnd - detStart;
  final detected = (a.detected * a.dur + b.detected * b.dur) / (a.dur + b.dur);
  const points = 32;
  final contour = <double>[];
  final envelope = <double>[];
  for (var k = 0; k < points; k++) {
    // Position in source time, so the curve follows the audio.
    final t = detStart + detDur * k / (points - 1);
    final inA = t <= a.detectedStart + a.detectedDur;
    final n = inA ? a : b;
    final u = ((t - n.detectedStart) / n.detectedDur).clamp(0.0, 1.0);
    final cents = n.contour.isEmpty ? 0.0 : _sampleCurve(n.contour, u);
    contour.add(cents + (n.detected - detected) * 100.0);
    envelope.add(n.envelope.isEmpty ? 1.0 : _sampleCurve(n.envelope, u));
  }
  final offset =
      ((a.pitch - a.detected) * a.dur + (b.pitch - b.detected) * b.dur) /
      (a.dur + b.dur);
  return VocalNote(
    id: id,
    lyric: a.lyric,
    unpitched: a.unpitched && b.unpitched,
    detected: detected,
    pitch: detected + offset,
    detectedStart: detStart,
    start: start,
    detectedDur: detDur,
    dur: dur,
    modulation: (a.modulation + b.modulation) / 2,
    formant: (a.formant + b.formant) / 2,
    gainDb: (a.gainDb * a.dur + b.gainDb * b.dur) / (a.dur + b.dur),
    detectedGain:
        (a.detectedGain * a.dur + b.detectedGain * b.dur) / (a.dur + b.dur),
    pinned: a.pinned || b.pinned,
    muted: a.muted && b.muted,
    // The joined note starts like a and ends like b.
    ornStyle: b.ornStyle == kOrnFall ? kOrnFall : a.ornStyle,
    ornDepth: b.ornStyle == kOrnFall ? b.ornDepth : a.ornDepth,
    ornSpeed: b.ornStyle == kOrnFall ? b.ornSpeed : a.ornSpeed,
    accent: a.accent,
    contour: contour,
    envelope: envelope,
  );
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

/// One undo step: the full note list (structure included, so cuts and joins
/// undo too) plus key/scale/correction.
class EditorSnapshot {
  final double correct;
  final String key;
  final String mode;
  final List<VocalNote> notes;

  /// Code of the edit made after this state (see `history.dart`), e.g.
  /// `cut` or `adv:transpose:2`. Shown in the History list and undo toast.
  final String label;

  EditorSnapshot({
    required this.correct,
    required this.key,
    required this.mode,
    required List<VocalNote> notes,
    this.label = '',
  }) : notes = [for (final n in notes) n.copy()];

  bool matches(EditorSnapshot other) {
    if (correct != other.correct || key != other.key || mode != other.mode) {
      return false;
    }
    if (notes.length != other.notes.length) return false;
    for (int i = 0; i < notes.length; i++) {
      if (!notes[i].sameAs(other.notes[i])) return false;
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

({int target, int cents}) centsFromTarget(
  double midi,
  String key,
  String mode,
) {
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
  return note.pitch + contourCents(note, t) / 100.0 + ornamentSemis(note, t);
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
    muted: false,
    ornStyle: kOrnNone,
    accent: 0,
  );
}
