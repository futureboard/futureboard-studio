import 'dart:collection';
import 'dart:convert';

import 'music.dart';

/// Compact on-disk form of a file's undo/redo history.
///
/// Snapshots are full note lists, but consecutive steps share almost every
/// note, so the document stores each distinct note state once (`states`)
/// and each snapshot as indices into it. A note's analysis data (contour,
/// envelope, detected values) is shared by all its states (`bodies`).
///
/// `current` is the editor state the history ends in. On load it must equal
/// the state restored from the project file; otherwise (the project was
/// saved after the history, or edited elsewhere) the history is dropped
/// rather than letting undo jump to a state it doesn't lead from.
class HistoryCodec {
  static const int version = 1;

  static String encode({
    required List<EditorSnapshot> undo,
    required List<EditorSnapshot> redo,
    required EditorSnapshot current,
  }) {
    final bodies = <List<Object?>>[];
    // Bodies are found by the identity of their contour list (copies share
    // it), then confirmed field by field.
    final bodyBuckets =
        LinkedHashMap<Object, List<(VocalNote, int)>>.identity();
    final states = <List<Object?>>[];
    final stateIndex = <String, int>{};

    int bodyOf(VocalNote n) {
      final bucket = bodyBuckets.putIfAbsent(n.contour, () => []);
      for (final (other, i) in bucket) {
        if (_sameBody(n, other)) return i;
      }
      bodies.add([
        n.id,
        n.unpitched,
        n.detected,
        n.detectedStart,
        n.detectedDur,
        n.detectedGain,
        n.scoop,
        n.drift,
        n.vib,
        n.vibCycles,
        n.vibPhase,
        n.contour,
        n.envelope,
      ]);
      bucket.add((n, bodies.length - 1));
      return bodies.length - 1;
    }

    int stateOf(VocalNote n) {
      final state = <Object?>[
        bodyOf(n),
        n.pitch,
        n.start,
        n.dur,
        n.modulation,
        n.formant,
        n.gainDb,
        (n.pinned ? 1 : 0) | (n.muted ? 2 : 0),
        n.ornStyle,
        n.ornDepth,
        n.ornSpeed,
        n.accent,
      ];
      final key = jsonEncode(state);
      return stateIndex.putIfAbsent(key, () {
        states.add(state);
        return states.length - 1;
      });
    }

    Map<String, Object?> snap(EditorSnapshot s) => {
      'l': s.label,
      'k': s.key,
      'm': s.mode,
      'c': s.correct,
      'n': [for (final n in s.notes) stateOf(n)],
    };

    final undoJson = [for (final s in undo) snap(s)];
    final redoJson = [for (final s in redo) snap(s)];
    final currentJson = snap(current);
    return jsonEncode({
      'v': version,
      'bodies': bodies,
      'states': states,
      'undo': undoJson,
      'redo': redoJson,
      'current': currentJson,
    });
  }

  /// The history in [doc] if it continues from [current], else null.
  static ({List<EditorSnapshot> undo, List<EditorSnapshot> redo})? decode(
    Map<String, dynamic> doc,
    EditorSnapshot current,
  ) {
    try {
      if (doc['v'] != version) return null;
      final bodies = [
        for (final b in doc['bodies'] as List<dynamic>) b as List<dynamic>,
      ];
      final states = [
        for (final s in doc['states'] as List<dynamic>) s as List<dynamic>,
      ];
      // One shared contour/envelope list per body, like live notes.
      final contours = <int, (List<double>, List<double>)>{};

      VocalNote note(int index) {
        final s = states[index];
        final bi = (s[0] as num).toInt();
        final b = bodies[bi];
        final lists = contours.putIfAbsent(
          bi,
          () => (_doubles(b[11]), _doubles(b[12])),
        );
        final flags = (s[7] as num).toInt();
        // Histories written before ลูกคอ/accent have 8 values per state.
        double opt(int i, double fallback) =>
            s.length > i ? _d(s[i]) : fallback;
        return VocalNote(
          id: b[0] as String,
          lyric: '',
          unpitched: b[1] as bool,
          detected: _d(b[2]),
          pitch: _d(s[1]),
          detectedStart: _d(b[3]),
          start: _d(s[2]),
          detectedDur: _d(b[4]),
          dur: _d(s[3]),
          modulation: _d(s[4]),
          formant: _d(s[5]),
          gainDb: _d(s[6]),
          detectedGain: _d(b[5]),
          pinned: flags & 1 != 0,
          muted: flags & 2 != 0,
          ornStyle: opt(8, kOrnNone.toDouble()).toInt(),
          ornDepth: opt(9, 1.0),
          ornSpeed: opt(10, 0.5),
          accent: opt(11, 0.0),
          scoop: _d(b[6]),
          drift: _d(b[7]),
          vib: _d(b[8]),
          vibCycles: _d(b[9]),
          vibPhase: _d(b[10]),
          contour: lists.$1,
          envelope: lists.$2,
        );
      }

      EditorSnapshot snap(Map<String, dynamic> j) => EditorSnapshot(
        correct: _d(j['c']),
        key: j['k'] as String,
        mode: j['m'] as String,
        label: (j['l'] as String?) ?? '',
        notes: [
          for (final i in j['n'] as List<dynamic>) note((i as num).toInt()),
        ],
      );

      final saved = snap(doc['current'] as Map<String, dynamic>);
      if (!saved.matches(current)) return null;
      return (
        undo: [
          for (final s in doc['undo'] as List<dynamic>)
            snap(s as Map<String, dynamic>),
        ],
        redo: [
          for (final s in doc['redo'] as List<dynamic>)
            snap(s as Map<String, dynamic>),
        ],
      );
    } catch (_) {
      return null; // unreadable or from another format: start fresh
    }
  }

  static bool _sameBody(VocalNote a, VocalNote b) =>
      a.id == b.id &&
      a.unpitched == b.unpitched &&
      a.detected == b.detected &&
      a.detectedStart == b.detectedStart &&
      a.detectedDur == b.detectedDur &&
      a.detectedGain == b.detectedGain &&
      a.scoop == b.scoop &&
      a.drift == b.drift &&
      a.vib == b.vib &&
      a.vibCycles == b.vibCycles &&
      a.vibPhase == b.vibPhase &&
      identical(a.envelope, b.envelope);

  static double _d(Object? v) => (v as num).toDouble();

  static List<double> _doubles(Object? v) => [
    for (final e in v as List<dynamic>) (e as num).toDouble(),
  ];
}
