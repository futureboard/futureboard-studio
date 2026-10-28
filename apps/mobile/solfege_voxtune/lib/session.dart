import 'audio.dart';
import 'music.dart';
import 'project.dart';

/// Editor state for one audio file of the open project.
///
/// Every file keeps its own Rust engine, notes, undo history and view, so
/// switching between files in the sidebar is instant and loses nothing. Only
/// the active session's engine is played.
class FileSession {
  /// Null for the placeholder session of a project with no files yet.
  final ProjectFile? file;
  final SynthEngine synth = SynthEngine();

  /// Decoded and analysed (done lazily, the first time the file is opened).
  bool loaded = false;
  String? error;

  /// The last load adopted the cached analysis instead of analysing.
  bool fromCache = false;

  List<VocalNote> notes = [];
  List<double> waveformPeaks = const [];
  double audioDuration = 0.0;
  bool hasAudio = false;

  String key = 'A';
  String mode = 'minor';
  String? detectedKey;
  String? detectedMode;
  double bpm = kBpm;
  double correct = 0.0;

  String? selectedId;
  double playhead = 0.0;
  double viewStart = 0.0;
  double viewDuration = beatsToSec(7.6);
  double pitchLo = 55.0;
  double pitchHi = 68.0;

  final List<EditorSnapshot> undoStack = [];
  final List<EditorSnapshot> redoStack = [];

  /// The history changed since it was last written to the cache.
  bool historyDirty = false;

  FileSession(this.file);

  /// The current editor state as an undo snapshot.
  EditorSnapshot snapshot({String label = ''}) => EditorSnapshot(
    correct: correct,
    key: key,
    mode: mode,
    notes: notes,
    label: label,
  );

  String? get name => file?.name;

  /// Notes outside the ±15¢ window of the current scale.
  int get offCount => notes.where((n) {
    if (n.unpitched || n.muted) return false;
    return centsFromTarget(n.pitch, key, mode).cents.abs() >= 15;
  }).length;

  /// Copies the editable state into [target] for saving: key/scale,
  /// correction and the full note list (structure included).
  void captureInto(ProjectFile target) {
    target.key = key;
    target.mode = mode;
    target.correct = correct;
    target.notes = [for (final n in notes) n.toJson()];
    target.edits = {};
  }

  /// Applies saved state from [source] onto freshly analysed notes. Returns
  /// true when the note list was replaced (the engine then needs it too).
  bool restoreEdits(ProjectFile source) {
    final saved = source.notes;
    if (saved != null) {
      notes = [for (final j in saved) VocalNote.fromJson(j)];
      for (final n in notes) {
        if (!n.unpitched) n.lyric = solfegeForMidi(n.pitch, key);
      }
      return true;
    }
    // Projects saved before cut/join: edits keyed by analysis id, which is
    // deterministic for the same audio.
    for (final n in notes) {
      final e = source.edits[n.id];
      if (e == null) continue;
      n.pitch = e.pitch;
      n.start = e.start;
      n.dur = e.dur;
      n.modulation = e.modulation;
      n.formant = e.formant;
      n.gainDb = e.gainDb;
      n.pinned = e.pinned;
      if (!n.unpitched) n.lyric = solfegeForMidi(n.pitch, key);
    }
    return false;
  }

  void dispose() => synth.dispose();
}
