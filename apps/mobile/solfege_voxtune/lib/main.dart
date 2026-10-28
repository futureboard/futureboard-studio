import 'dart:math' as math;
import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter/services.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import 'audio.dart';
import 'draw.dart';
import 'music.dart';
import 'rust_dsp.dart';
import 'theme.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  SystemChrome.setSystemUIOverlayStyle(
    const SystemUiOverlayStyle(
      statusBarColor: Colors.transparent,
      statusBarIconBrightness: Brightness.light,
      systemNavigationBarColor: CantoColors.transportBg,
      systemNavigationBarIconBrightness: Brightness.light,
    ),
  );
  runApp(const CantoApp());
}

class CantoApp extends StatelessWidget {
  const CantoApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'CANTO',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        brightness: Brightness.dark,
        scaffoldBackgroundColor: CantoColors.panel,
        fontFamily: CantoTypography.sansFamily,
        colorScheme: const ColorScheme.dark(
          primary: CantoColors.accent,
          surface: CantoColors.panel,
        ),
      ),
      home: const CantoScreen(),
    );
  }
}

class _DragState {
  final String type; // 'pitch', 'drift', 'time', 'formant', 'level', 'pan', 'scrub', 'pitch-scroll'
  final String? noteId;
  final String edge;
  final double startX;
  final double startY;
  final double startView;
  final double startLo;
  final double startHi;
  bool moved;
  final NoteSnapshot? origin;

  _DragState({
    required this.type,
    this.noteId,
    this.edge = 'body',
    required this.startX,
    required this.startY,
    this.startView = 0,
    this.startLo = 62,
    this.startHi = 75,
    this.moved = false,
    this.origin,
  });
}

class _PressState {
  final HitResult hit;
  bool moved;
  final int timeMs;

  _PressState({
    required this.hit,
    this.moved = false,
    required this.timeMs,
  });
}

class _PinchState {
  final double dist;
  final double duration;
  final double start;
  final double anchor;

  const _PinchState({
    required this.dist,
    required this.duration,
    required this.start,
    required this.anchor,
  });
}

class _OverviewDragState {
  final String mode; // 'pan' | 'move'
  final double startX;
  final double startView;
  bool moved;

  _OverviewDragState({
    required this.mode,
    required this.startX,
    required this.startView,
    this.moved = false,
  });
}

class _ParamModel {
  final String name;
  final double min;
  final double max;
  final double step;
  final double value;
  final String text;
  final bool disabled;
  final bool bipolar;

  const _ParamModel({
    required this.name,
    required this.min,
    required this.max,
    required this.step,
    required this.value,
    required this.text,
    this.disabled = false,
    required this.bipolar,
  });
}

class CantoScreen extends StatefulWidget {
  const CantoScreen({super.key});

  @override
  State<CantoScreen> createState() => CantoScreenState();
}

class CantoScreenState extends State<CantoScreen>
    with SingleTickerProviderStateMixin {
  static const List<String> _tools = [
    'pitch',
    'drift',
    'time',
    'formant',
    'level',
  ];

  final List<VocalNote> _notes = [];
  List<double> _waveformPeaks = const [];
  String? _fileName;
  double _audioDuration = 0.0;
  bool _hasAudio = false;
  bool _analyzing = false;
  String? _errorMessage;

  String _tool = 'pitch';
  String _snap = 'scale'; // 'off' | 'semi' | 'scale'
  String _key = 'A';
  String _mode = 'minor';
  double _bpm = kBpm;
  bool _playing = false;
  double _playhead = 0.0;
  bool _loop = true;
  bool _bypass = false;
  bool _follow = true;
  String? _selectedId;
  String? _hoverId;
  double _correct = 0.0;
  double _viewStart = 0.0;
  double _viewDuration = beatsToSec(7.6);
  double _pitchLo = 55.0;
  double _pitchHi = 68.0;

  final List<EditorSnapshot> _undoStack = [];
  final List<EditorSnapshot> _redoStack = [];
  EditorSnapshot? _pendingSnapshot;
  List<EditorSnapshot>? _redoBackup;

  _DragState? _drag;
  _PressState? _press;
  _PinchState? _pinch;
  _OverviewDragState? _overviewDrag;
  final Map<int, Offset> _pointers = {};

  int _lastTapMs = 0;
  String? _lastTapId;

  bool _keyPopOpen = false;
  double _editorWidth = 390.0;
  double _editorHeight = 460.0;
  int _sliderTarget = 69;

  late final Ticker _ticker;
  final SynthEngine _synth = SynthEngine();
  Duration _lastTickElapsed = Duration.zero;
  double _level = 0.0;

  List<VocalNote> get notes => List.unmodifiable(_notes);
  String? get fileName => _fileName;
  bool get hasAudio => _hasAudio;

  @override
  void initState() {
    super.initState();
    _ticker = createTicker(_onTick);
  }

  @override
  void dispose() {
    _ticker.dispose();
    _synth.dispose();
    super.dispose();
  }

  VocalNote? get _selectedNote =>
      _notes.where((n) => n.id == _selectedId).firstOrNull;

  double get _duration =>
      _audioDuration > 0.0 ? _audioDuration : phraseDuration(_bpm);

  Future<void> pickAndLoadAudio() async {
    if (_analyzing) return;
    final picked = await _synth.pickAudioFile();
    if (picked == null || !mounted) return;
    loadAudioPath(picked.path, fileName: picked.name);
  }

  void loadAudioPath(String path, {String? fileName}) {
    if (_playing) {
      _pause();
    }
    setState(() {
      _analyzing = true;
      _errorMessage = null;
    });
    final res = _synth.loadAudioFile(path);
    final label = (fileName != null && fileName.isNotEmpty)
        ? fileName
        : path.split(RegExp(r'[/\\]')).last;
    _applyAnalysisResult(res, fileName: label);
  }

  void loadAudioBytes(
    Uint8List bytes, {
    String fileName = 'Loaded Audio.wav',
    String? extHint,
  }) {
    if (_playing) {
      _pause();
    }
    setState(() {
      _analyzing = true;
      _errorMessage = null;
    });
    final res = _synth.loadAudioBytes(bytes, extHint: extHint);
    _applyAnalysisResult(res, fileName: fileName);
  }

  void analyzeLoadedAudio() {
    if (!_hasAudio) {
      pickAndLoadAudio();
      return;
    }
    if (_playing) {
      _pause();
    }
    setState(() {
      _analyzing = true;
      _errorMessage = null;
    });
    final res = _synth.analyzeLoadedAudio();
    _applyAnalysisResult(res, fileName: _fileName);
  }

  void _applyAnalysisResult(CantoAnalysisResult res, {String? fileName}) {
    setState(() {
      _analyzing = false;
      if (!res.ok) {
        _errorMessage = res.error ?? 'Failed to decode or analyze audio file';
        return;
      }
      _errorMessage = null;
      _hasAudio = res.durationSec > 0.0;
      _fileName = fileName ?? _fileName ?? 'Audio clip';
      _audioDuration = math.max(0.5, res.durationSec);
      _bpm = res.bpm.clamp(60.0, 200.0);
      _key = kKeys.any((k) => k.name == res.key) ? res.key : 'A';
      _mode = kModes.containsKey(res.mode) ? res.mode : 'minor';
      _waveformPeaks = res.waveformPeaks;
      _notes
        ..clear()
        ..addAll(res.notes);
      _correct = 0.0;
      _playhead = 0.0;
      _undoStack.clear();
      _redoStack.clear();
      _pendingSnapshot = null;
      _redoBackup = null;

      final pitched = _notes.where((n) => !n.unpitched).toList();
      _selectedId = pitched.isNotEmpty
          ? pitched.first.id
          : (_notes.isNotEmpty ? _notes.first.id : null);

      _fitViewportToNotes();
      _syncRustEngine();
      _synth.seek(0.0);
    });
  }

  void _fitViewportToNotes() {
    _viewStart = 0.0;
    _viewDuration = math.min(_duration, math.max(beatsToSec(4.0, _bpm), 6.0));
    _clampView();

    final pitched = _notes.where((n) => !n.unpitched).toList();
    if (pitched.isEmpty) {
      _pitchLo = 55.0;
      _pitchHi = 68.0;
      return;
    }
    double minP = double.infinity;
    double maxP = double.negativeInfinity;
    for (final n in pitched) {
      if (n.detected < minP) minP = n.detected;
      if (n.detected > maxP) maxP = n.detected;
    }
    final center = (minP + maxP) * 0.5;
    final span = math.max(13.0, (maxP - minP) + 6.0);
    _pitchLo = clampVal((center - span * 0.5).floorToDouble(), 36.0, 76.0);
    _pitchHi = math.min(92.0, _pitchLo + span);
  }

  int _countOff() {
    int count = 0;
    for (final note in _notes) {
      if (note.unpitched) continue;
      final shown = presentNote(note, false);
      if (centsFromTarget(shown.pitch, _key, _mode).cents.abs() >= 15) {
        count++;
      }
    }
    return count;
  }

  EditorLayout _currentLayout() {
    return EditorLayout.compute(
      _editorWidth,
      _editorHeight,
      pitchLo: _pitchLo,
      pitchHi: _pitchHi,
      viewDuration: _viewDuration,
    );
  }

  void _clampView() {
    final total = math.max(0.5, _duration);
    final minView = math.min(total, beatsToSec(1.5, _bpm));
    _viewDuration = clampVal(_viewDuration, minView, total);
    _viewStart = clampVal(_viewStart, 0.0, math.max(0.0, total - _viewDuration));
  }

  void _ensurePitchVisible(double pitch) {
    final span = _pitchHi - _pitchLo;
    if (pitch > _pitchHi - 0.7) {
      _pitchHi = math.min(88.0, pitch + 1.0);
      _pitchLo = _pitchHi - span;
    } else if (pitch < _pitchLo + 0.7) {
      _pitchLo = math.max(36.0, pitch - 1.0);
      _pitchHi = _pitchLo + span;
    }
  }

  EditorSnapshot _takeSnapshot() {
    return EditorSnapshot(
      correct: _correct,
      key: _key,
      mode: _mode,
      notes: _notes.map(NoteSnapshot.fromNote).toList(),
    );
  }

  void _applySnapshot(EditorSnapshot snap) {
    _correct = snap.correct;
    _key = snap.key;
    _mode = snap.mode;
    for (final item in snap.notes) {
      final note = _notes.where((n) => n.id == item.id).firstOrNull;
      if (note == null) continue;
      note.pitch = item.pitch;
      note.modulation = item.modulation;
      note.formant = item.formant;
      note.gainDb = item.gainDb;
      note.start = item.start;
      note.dur = item.dur;
      note.pinned = item.pinned;
      if (!note.unpitched) {
        note.lyric = solfegeForMidi(note.pitch, _key);
      }
    }
    _syncRustEngine();
  }

  void _pushUndo() {
    final snap = _takeSnapshot();
    _pendingSnapshot = snap;
    _undoStack.add(snap);
    if (_undoStack.length > 80) _undoStack.removeAt(0);
    _redoBackup = List<EditorSnapshot>.from(_redoStack);
    _redoStack.clear();
  }

  void _commitUndo() {
    final pending = _pendingSnapshot;
    if (pending == null) return;
    if (_takeSnapshot().matches(pending)) {
      if (_undoStack.isNotEmpty) _undoStack.removeLast();
      if (_redoBackup != null) {
        _redoStack
          ..clear()
          ..addAll(_redoBackup!);
      }
    }
    _pendingSnapshot = null;
    _redoBackup = null;
    _syncRustEngine();
  }

  void _undo() {
    if (_undoStack.isEmpty) return;
    setState(() {
      final snap = _undoStack.removeLast();
      _redoStack.add(_takeSnapshot());
      _applySnapshot(snap);
    });
  }

  void _redo() {
    if (_redoStack.isEmpty) return;
    setState(() {
      final snap = _redoStack.removeLast();
      _undoStack.add(_takeSnapshot());
      _applySnapshot(snap);
    });
  }

  void _reapplyCorrect() {
    final amount = _correct;
    for (final note in _notes) {
      if (note.unpitched) continue;
      if (!note.pinned) {
        final dest = nearestScale(note.detected, _key, _mode);
        note.pitch = note.detected + (dest - note.detected) * amount;
        note.modulation = clampVal(1.0 - amount * 0.30, 0.0, 2.0);
      }
      note.lyric = solfegeForMidi(note.pitch, _key);
    }
  }

  List<VocalNote> _sungOrder() {
    return _notes.where((n) => !n.unpitched).toList();
  }

  void _clampTiming(VocalNote note, String edge) {
    const gap = 0.012;
    final sung = _sungOrder();
    final index = sung.indexWhere((item) => item.id == note.id);
    final prev = index > 0 ? sung[index - 1] : null;
    final next = index >= 0 && index < sung.length - 1 ? sung[index + 1] : null;
    final minStart = prev != null ? prev.start + prev.dur + gap : 0.0;
    final maxEnd = next != null ? next.start - gap : _duration;
    if (edge == 'start') {
      final end = note.start + note.dur;
      note.start = clampVal(note.start, minStart, end - 0.06);
      note.dur = math.max(0.06, end - note.start);
    } else if (edge == 'end') {
      note.dur = clampVal(note.dur, 0.06, maxEnd - note.start);
    } else {
      if (note.dur < 0.06) note.dur = 0.06;
      note.start = clampVal(note.start, minStart, maxEnd - note.dur);
    }
    if (note.start < 0) note.start = 0;
    if (note.start + note.dur > _duration) {
      note.dur = math.max(0.06, _duration - note.start);
    }
  }

  void _setTool(String tool) {
    if (!_tools.contains(tool)) return;
    setState(() {
      _tool = tool;
    });
  }

  void _cycleSnap() {
    setState(() {
      _snap = _snap == 'off'
          ? 'semi'
          : _snap == 'semi'
              ? 'scale'
              : 'off';
    });
  }

  void _seek(double time) {
    _playhead = clampVal(time, 0.0, _duration);
    _synth.seek(_playhead);
  }

  void _syncRustEngine() {
    _synth.syncState(
      notes: _notes,
      keyName: _key,
      mode: _mode,
      correct: _correct,
      playing: _playing,
      loop: _loop,
      bypass: _bypass,
    );
  }

  void _followPlayhead() {
    final left = _viewStart + _viewDuration * 0.08;
    final right = _viewStart + _viewDuration * 0.78;
    if (_playhead < left || _playhead > right) {
      _viewStart = _playhead - _viewDuration * 0.22;
      _clampView();
    }
  }

  void _onTick(Duration elapsed) {
    if (_lastTickElapsed == Duration.zero) {
      _lastTickElapsed = elapsed;
    }
    final dtSec =
        ((elapsed - _lastTickElapsed).inMicroseconds / 1e6).clamp(0.0, 0.1);
    _lastTickElapsed = elapsed;

    final frames =
        (dtSec * SynthEngine.sampleRate).round().clamp(128, 4096);

    if (_playing) {
      final step = _synth.stepAudio(frames);
      if (!step.isPlaying && !_loop && step.playhead >= _duration - 0.02) {
        setState(() {
          _playhead = _duration;
        });
        _pause();
        return;
      }
      setState(() {
        _playhead = step.playhead;
        _level = step.level;
        if (_follow && _drag == null && _pinch == null) {
          _followPlayhead();
        }
      });
    } else if (_drag != null && _drag!.moved && _drag!.noteId != null) {
      final step = _synth.stepAudio(frames);
      setState(() {
        _level = step.level;
      });
    } else {
      _ticker.stop();
      _lastTickElapsed = Duration.zero;
    }
  }

  void _pause() {
    _playing = false;
    _ticker.stop();
    _lastTickElapsed = Duration.zero;
    _synth.silence();
    _syncRustEngine();
    setState(() {
      _level = 0.0;
    });
  }

  void _togglePlay() {
    if (!_hasAudio) {
      pickAndLoadAudio();
      return;
    }
    if (_playing) {
      _pause();
      return;
    }
    setState(() {
      if (_playhead >= _duration - 0.03) _playhead = 0.0;
      _playing = true;
      _follow = true;
      _lastTickElapsed = Duration.zero;
    });
    _synth.ensureStarted();
    _syncRustEngine();
    _synth.seek(_playhead);
    if (!_ticker.isActive) {
      _ticker.start();
    }
  }

  void _zoomAt(double factor, double anchor) {
    setState(() {
      final minView = math.min(_duration, beatsToSec(1.5, _bpm));
      final next = clampVal(_viewDuration * factor, minView, _duration);
      final fraction = (anchor - _viewStart) / math.max(0.01, _viewDuration);
      _viewDuration = next;
      _viewStart = anchor - fraction * next;
      _follow = false;
      _clampView();
    });
  }

  void _tuneNote(VocalNote note) {
    if (note.unpitched || _bypass) return;
    setState(() {
      _pushUndo();
      note.pitch = nearestScale(note.pitch, _key, _mode).toDouble();
      note.modulation = math.min(note.modulation, 0.70);
      note.lyric = solfegeForMidi(note.pitch, _key);
      note.pinned = true;
      _commitUndo();
    });
  }

  void _tune() {
    if (_notes.isEmpty) return;
    final note = _selectedNote;
    if (note != null && !note.unpitched) {
      _tuneNote(note);
      return;
    }
    setState(() {
      _pushUndo();
      _correct = 1.0;
      _reapplyCorrect();
      _commitUndo();
    });
  }

  void _resetAll() {
    if (_notes.isEmpty) return;
    setState(() {
      _pushUndo();
      _correct = 0.0;
      for (final note in _notes) {
        note.pitch = note.detected;
        note.start = note.detectedStart;
        note.dur = note.detectedDur;
        note.modulation = 1.0;
        note.formant = 0.0;
        note.gainDb = note.detectedGain;
        note.pinned = false;
        if (!note.unpitched) {
          note.lyric = solfegeForMidi(note.pitch, _key);
        }
      }
      _commitUndo();
    });
  }

  void _revertNote() {
    final note = _selectedNote;
    if (note == null || !note.pinned) return;
    setState(() {
      _pushUndo();
      note.pinned = false;
      final dest = nearestScale(note.detected, _key, _mode);
      note.pitch = note.detected + (dest - note.detected) * _correct;
      if (!note.unpitched) {
        note.lyric = solfegeForMidi(note.pitch, _key);
      }
      _commitUndo();
    });
  }

  _ParamModel _paramModel(VocalNote? note) {
    if (note == null || note.unpitched) {
      return const _ParamModel(
        name: 'Pitch',
        min: -100,
        max: 100,
        step: 1,
        value: 0,
        text: '—',
        disabled: true,
        bipolar: true,
      );
    }
    if (_tool == 'drift') {
      final pct = (note.modulation * 100).round();
      return _ParamModel(
        name: 'Drift',
        min: 0,
        max: 145,
        step: 1,
        value: pct.toDouble(),
        text: '$pct%',
        bipolar: false,
      );
    }
    if (_tool == 'formant') {
      final val = (note.formant * 10).round() / 10.0;
      return _ParamModel(
        name: 'Formant',
        min: -12,
        max: 12,
        step: 0.1,
        value: val,
        text: '${formatSigned(note.formant, 1)} st',
        bipolar: true,
      );
    }
    if (_tool == 'level') {
      final val = (note.gainDb * 10).round() / 10.0;
      return _ParamModel(
        name: 'Level',
        min: -18,
        max: 6,
        step: 0.1,
        value: val,
        text: formatDb(note.gainDb),
        bipolar: false,
      );
    }
    if (_tool == 'time') {
      final maxStart = math.max(0.01, _duration - note.dur);
      return _ParamModel(
        name: 'Time',
        min: 0,
        max: maxStart,
        step: 0.01,
        value: clampVal(note.start, 0.0, maxStart),
        text: formatBars(note.start, _bpm),
        bipolar: false,
      );
    }
    final reading = centsFromTarget(note.pitch, _key, _mode);
    return _ParamModel(
      name: 'Pitch',
      min: -100,
      max: 100,
      step: 1,
      value: clampVal(reading.cents.toDouble(), -100, 100),
      text: formatCents(reading.cents),
      bipolar: true,
    );
  }

  void _applyParam(double value) {
    final note = _selectedNote;
    if (note == null || note.unpitched || _bypass) return;
    setState(() {
      if (_tool == 'pitch') {
        note.pitch = _sliderTarget + value / 100.0;
        note.lyric = solfegeForMidi(note.pitch, _key);
        note.pinned = true;
        _ensurePitchVisible(note.pitch);
      } else if (_tool == 'drift') {
        note.modulation = value / 100.0;
      } else if (_tool == 'formant') {
        note.formant = value;
      } else if (_tool == 'level') {
        note.gainDb = value;
      } else if (_tool == 'time') {
        note.start = value;
        _clampTiming(note, 'body');
      }
      _syncRustEngine();
    });
  }

  String _floaterLabel(VocalNote note) {
    final type = _drag?.type;
    if (type == 'drift') return 'Drift ${(note.modulation * 100).round()}%';
    if (type == 'formant') return 'Formant ${formatSigned(note.formant, 1)} st';
    if (type == 'level') return formatDb(note.gainDb);
    if (type == 'time') {
      final delta = ((note.start - note.detectedStart) * 1000).round();
      final sign = delta > 0 ? '+' : delta < 0 ? '−' : '';
      return '${formatBars(note.start, _bpm)}  $sign${delta.abs()} ms';
    }
    final reading = centsFromTarget(note.pitch, _key, _mode);
    return '${spellMidi(reading.target, _key)}  ${formatCents(reading.cents)}';
  }

  void _onEditorPointerDown(PointerDownEvent event) {
    if (_keyPopOpen) {
      setState(() => _keyPopOpen = false);
    }
    final point = event.localPosition;
    _pointers[event.pointer] = point;
    if (_pointers.length >= 2) {
      final pts = _pointers.values.toList();
      final dist = (pts[0] - pts[1]).distance;
      final midX = (pts[0].dx + pts[1].dx) / 2.0;
      setState(() {
        _pinch = _PinchState(
          dist: math.max(8.0, dist),
          duration: _viewDuration,
          start: _viewStart,
          anchor: _currentLayout().xToTime(_viewStart, midX),
        );
        _drag = null;
        _follow = false;
      });
      return;
    }

    final hit = hitTestEditor(
      x: point.dx,
      y: point.dy,
      width: _editorWidth,
      height: _editorHeight,
      notes: _notes,
      selectedId: _selectedId,
      tool: _tool,
      bypass: _bypass,
      viewStart: _viewStart,
      viewDuration: _viewDuration,
      pitchLo: _pitchLo,
      pitchHi: _pitchHi,
    );

    final nowMs = DateTime.now().millisecondsSinceEpoch;
    _press = _PressState(hit: hit, moved: false, timeMs: nowMs);

    setState(() {
      if (hit.type == 'note' && hit.note != null) {
        _selectedId = hit.note!.id;
      }
      if (hit.type == 'note' &&
          hit.note != null &&
          !_bypass &&
          !hit.note!.unpitched) {
        _drag = _DragState(
          type: _tool,
          noteId: hit.note!.id,
          edge: hit.edge,
          startX: point.dx,
          startY: point.dy,
          moved: false,
          origin: NoteSnapshot.fromNote(hit.note!),
        );
      } else if (hit.type == 'ruler') {
        _drag = _DragState(
          type: 'scrub',
          moved: true,
          startX: point.dx,
          startY: point.dy,
        );
        _seek(_currentLayout().xToTime(_viewStart, point.dx));
        _press!.moved = true;
      } else if (hit.type == 'pitch-ruler') {
        _drag = _DragState(
          type: 'pitch-scroll',
          startX: point.dx,
          startY: point.dy,
          startLo: _pitchLo,
          startHi: _pitchHi,
          moved: false,
        );
      } else if (hit.type == 'empty') {
        _drag = _DragState(
          type: 'pan',
          startX: point.dx,
          startY: point.dy,
          startView: _viewStart,
          moved: false,
        );
      }
    });
  }

  void _onEditorPointerMove(PointerMoveEvent event) {
    final point = event.localPosition;
    if (_pointers.containsKey(event.pointer)) {
      _pointers[event.pointer] = point;
    }
    if (_pointers.length >= 2 && _pinch != null) {
      final pts = _pointers.values.toList();
      final dist = math.max(8.0, (pts[0] - pts[1]).distance);
      final fraction =
          (_pinch!.anchor - _pinch!.start) / math.max(0.01, _pinch!.duration);
      setState(() {
        final minView = math.min(_duration, beatsToSec(1.5, _bpm));
        _viewDuration = clampVal(
          _pinch!.duration * (_pinch!.dist / dist),
          minView,
          _duration,
        );
        _viewStart = _pinch!.anchor - fraction * _viewDuration;
        _clampView();
      });
      return;
    }

    final drag = _drag;
    if (drag == null) return;
    final dx = point.dx - drag.startX;
    final dy = point.dy - drag.startY;
    if (!drag.moved && math.sqrt(dx * dx + dy * dy) >= 3.0) {
      drag.moved = true;
      _press?.moved = true;
      if (!const ['pan', 'scrub', 'pitch-scroll'].contains(drag.type)) {
        _pushUndo();
      }
      if (drag.type == 'pan' || drag.type == 'pitch-scroll') {
        _follow = false;
      }
    }
    if (drag.type == 'scrub' || drag.moved) {
      setState(() {
        _applyDrag(point);
      });
    }
  }

  void _applyDrag(Offset point) {
    final drag = _drag;
    if (drag == null) return;
    final layout = _currentLayout();
    if (drag.type == 'pan') {
      _viewStart = drag.startView - (point.dx - drag.startX) / layout.pxPerSec;
      _clampView();
      return;
    }
    if (drag.type == 'scrub') {
      _seek(layout.xToTime(_viewStart, point.dx));
      return;
    }
    if (drag.type == 'pitch-scroll') {
      final delta = (point.dy - drag.startY) / layout.pxPerSemi;
      final span = drag.startHi - drag.startLo;
      final lo = clampVal(drag.startLo + delta, 36.0, 92.0 - span);
      _pitchLo = lo;
      _pitchHi = lo + span;
      return;
    }

    final noteIndex = _notes.indexWhere((n) => n.id == drag.noteId);
    if (noteIndex < 0) return;
    final note = _notes[noteIndex];
    final origin = drag.origin;
    if (origin == null) return;
    final dSemi = (drag.startY - point.dy) / layout.pxPerSemi;
    if (drag.type == 'pitch') {
      double pitch = origin.pitch + dSemi;
      if (_snap == 'semi') pitch = pitch.roundToDouble();
      if (_snap == 'scale') {
        pitch = nearestScale(pitch, _key, _mode).toDouble();
      }
      note.pitch = pitch;
      note.lyric = solfegeForMidi(pitch, _key);
      note.pinned = true;
      _ensurePitchVisible(pitch);
    } else if (drag.type == 'drift') {
      note.modulation = clampVal(origin.modulation + dSemi * 0.9, 0.0, 1.45);
    } else if (drag.type == 'formant') {
      note.formant = clampVal(origin.formant + dSemi * 1.7, -12.0, 12.0);
    } else if (drag.type == 'level') {
      note.gainDb = clampVal(origin.gainDb + dSemi * 2.4, -18.0, 6.0);
    } else if (drag.type == 'time') {
      final dt = (point.dx - drag.startX) / layout.pxPerSec;
      if (drag.edge == 'start') {
        final end = origin.start + origin.dur;
        note.start = origin.start + dt;
        note.dur = end - note.start;
      } else if (drag.edge == 'end') {
        note.start = origin.start;
        note.dur = origin.dur + dt;
      } else {
        note.start = origin.start + dt;
        note.dur = origin.dur;
      }
      _clampTiming(note, drag.edge);
    }

    _syncRustEngine();
    if (!_playing && _hasAudio) {
      _synth.auditionNote(noteIndex);
      if (!_ticker.isActive) {
        _lastTickElapsed = Duration.zero;
        _ticker.start();
      }
    }
  }

  void _onEditorPointerUp(PointerEvent event) {
    _pointers.remove(event.pointer);
    if (_pointers.length < 2) _pinch = null;
    final drag = _drag;
    final press = _press;
    _drag = null;
    _press = null;
    _synth.stopAudition();
    if (!_playing) {
      _level = 0.0;
    }
    final moved = (drag?.moved ?? false) || (press?.moved ?? false);

    if (!moved && press?.hit.type == 'note' && press?.hit.note != null) {
      final note = press!.hit.note!;
      final nowMs = DateTime.now().millisecondsSinceEpoch;
      if (note.id == _lastTapId &&
          nowMs - _lastTapMs < 340 &&
          !note.unpitched &&
          !_bypass) {
        _tuneNote(note);
      }
      _lastTapMs = nowMs;
      _lastTapId = note.id;
    } else if (!moved && press?.hit.type == 'empty') {
      _selectedId = null;
    }
    setState(() {
      _commitUndo();
    });
  }

  void _onOverviewDown(PointerDownEvent event, double width) {
    final time =
        clampVal((event.localPosition.dx / width) * _duration, 0.0, _duration);
    final inside = time >= _viewStart && time <= _viewStart + _viewDuration;
    _overviewDrag = _OverviewDragState(
      mode: inside ? 'pan' : 'move',
      startX: event.localPosition.dx,
      startView: _viewStart,
      moved: false,
    );
    setState(() {
      _follow = false;
      if (!inside) {
        _seek(time);
        _viewStart = time - _viewDuration / 2.0;
        _clampView();
        _overviewDrag!.moved = true;
      }
    });
  }

  void _onOverviewMove(PointerMoveEvent event, double width) {
    final ov = _overviewDrag;
    if (ov == null) return;
    final dx = event.localPosition.dx - ov.startX;
    if (dx.abs() > 3) ov.moved = true;
    final dt = (dx / math.max(1.0, width)) * _duration;
    setState(() {
      if (ov.mode == 'pan') {
        _viewStart = ov.startView + dt;
        _clampView();
      } else {
        final time = clampVal(
          (event.localPosition.dx / width) * _duration,
          0.0,
          _duration,
        );
        _seek(time);
        _viewStart = time - _viewDuration / 2.0;
        _clampView();
      }
    });
  }

  void _onOverviewUp(PointerEvent event, double width) {
    final ov = _overviewDrag;
    if (ov == null) return;
    setState(() {
      if (ov.mode == 'pan' && !ov.moved) {
        final time = clampVal(
          (event.localPosition.dx / width) * _duration,
          0.0,
          _duration,
        );
        _seek(time);
      }
      _overviewDrag = null;
    });
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: CantoColors.panel,
      body: SafeArea(
        bottom: true,
        child: Stack(
          children: [
            Column(
              children: [
                _buildHeader(),
                _buildToolbar(),
                Expanded(child: _buildEditorArea()),
                _buildInspector(),
                _buildOverviewBar(),
                _buildTransportFooter(),
                _buildHomeIndicator(),
              ],
            ),
            if (_keyPopOpen)
              Positioned.fill(
                child: GestureDetector(
                  behavior: HitTestBehavior.translucent,
                  onTap: () => setState(() => _keyPopOpen = false),
                  child: const SizedBox.expand(),
                ),
              ),
            if (_keyPopOpen)
              Positioned(
                top: 48,
                left: 8,
                right: 8,
                child: _buildKeyPopover(),
              ),
          ],
        ),
      ),
    );
  }

  Widget _buildHeader() {
    final off = _countOff();
    final hasNotes = _notes.any((n) => !n.unpitched);
    return Container(
      height: 44,
      padding: const EdgeInsets.symmetric(horizontal: 10),
      decoration: const BoxDecoration(
        color: CantoColors.panel,
        border: Border(
          bottom: BorderSide(color: CantoColors.line, width: 1),
        ),
      ),
      child: Row(
        children: [
          const SizedBox(
            width: 16,
            height: 16,
            child: CustomPaint(painter: _CantoWaveLogoPainter()),
          ),
          const SizedBox(width: 7),
          Expanded(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.baseline,
              textBaseline: TextBaseline.alphabetic,
              children: [
                Text(
                  'CANTO',
                  style: CantoTypography.sans(
                    size: 12.5,
                    weight: FontWeight.w700,
                    color: CantoColors.ink,
                    letterSpacing: 2.2,
                  ),
                ),
                const SizedBox(width: 7),
                Flexible(
                  child: Text(
                    _fileName ?? 'No audio loaded',
                    key: const Key('clip-title'),
                    overflow: TextOverflow.ellipsis,
                    style: CantoTypography.sans(
                      size: 11,
                      weight: FontWeight.w500,
                      color: _hasAudio ? CantoColors.muted : CantoColors.faint,
                    ),
                  ),
                ),
              ],
            ),
          ),
          if (hasNotes) ...[
            Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                Icon(
                  off == 0 ? LucideIcons.check : LucideIcons.circleAlert,
                  size: 11,
                  color: off == 0 ? CantoColors.faint : CantoColors.heat,
                ),
                const SizedBox(width: 3),
                Text(
                  off == 0 ? 'in key' : '$off off',
                  style: CantoTypography.mono(
                    size: 10,
                    weight: FontWeight.w500,
                    color: off == 0 ? CantoColors.faint : CantoColors.heat,
                    letterSpacing: 0.4,
                  ),
                ),
              ],
            ),
            const SizedBox(width: 6),
          ],
          GestureDetector(
            key: const Key('load-audio-btn'),
            onTap: _analyzing ? null : pickAndLoadAudio,
            child: Container(
              height: 26,
              padding: const EdgeInsets.symmetric(horizontal: 7),
              decoration: BoxDecoration(
                color: const Color(0xFF1C1B18),
                borderRadius: BorderRadius.circular(2),
                border: Border.all(color: const Color(0xFF3A342C)),
              ),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  const Icon(
                    LucideIcons.folderOpen,
                    size: 11,
                    color: Color(0xFFF0E6D6),
                  ),
                  const SizedBox(width: 4),
                  Text(
                    'LOAD',
                    style: CantoTypography.sans(
                      size: 9.5,
                      weight: FontWeight.w600,
                      color: const Color(0xFFF0E6D6),
                      letterSpacing: 0.9,
                    ),
                  ),
                ],
              ),
            ),
          ),
          const SizedBox(width: 5),
          GestureDetector(
            key: const Key('analyze-btn'),
            onTap: _analyzing ? null : analyzeLoadedAudio,
            child: Opacity(
              opacity: _hasAudio ? 1.0 : 0.65,
              child: Container(
                height: 26,
                padding: const EdgeInsets.symmetric(horizontal: 7),
                decoration: BoxDecoration(
                  color: _hasAudio
                      ? const Color(0xFF2A2118)
                      : const Color(0xFF1C1B18),
                  borderRadius: BorderRadius.circular(2),
                  border: Border.all(
                    color: _hasAudio
                        ? const Color(0xFF6A4A32)
                        : const Color(0xFF3A342C),
                  ),
                ),
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Icon(
                      LucideIcons.scanSearch,
                      size: 11,
                      color: _hasAudio
                          ? CantoColors.accent
                          : const Color(0xFFC8C2B6),
                    ),
                    const SizedBox(width: 4),
                    Text(
                      _analyzing ? '...' : 'ANALYZE',
                      style: CantoTypography.sans(
                        size: 9.5,
                        weight: FontWeight.w600,
                        color: _hasAudio
                            ? const Color(0xFFF0D3B4)
                            : const Color(0xFFC8C2B6),
                        letterSpacing: 0.9,
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
          const SizedBox(width: 5),
          GestureDetector(
            key: const Key('key-btn'),
            onTap: () => setState(() => _keyPopOpen = !_keyPopOpen),
            child: Container(
              height: 26,
              padding: const EdgeInsets.symmetric(horizontal: 7),
              decoration: BoxDecoration(
                color: const Color(0xFF1C1B18),
                borderRadius: BorderRadius.circular(2),
                border: Border.all(
                  color: _keyPopOpen
                      ? CantoColors.accent
                      : const Color(0xFF3A342C),
                ),
              ),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(
                    LucideIcons.music2,
                    size: 11,
                    color: _keyPopOpen
                        ? CantoColors.accent
                        : const Color(0xFFF0E6D6),
                  ),
                  const SizedBox(width: 4),
                  Text(
                    '$_key ${kModeShort[_mode]}',
                    style: CantoTypography.mono(
                      size: 10.5,
                      weight: FontWeight.w500,
                      color: _keyPopOpen
                          ? CantoColors.accent
                          : const Color(0xFFF0E6D6),
                    ),
                  ),
                  const SizedBox(width: 2),
                  Icon(
                    _keyPopOpen
                        ? LucideIcons.chevronUp
                        : LucideIcons.chevronDown,
                    size: 11,
                    color: _keyPopOpen
                        ? CantoColors.accent
                        : CantoColors.muted,
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildKeyPopover() {
    return Container(
      padding: const EdgeInsets.all(10),
      decoration: BoxDecoration(
        color: const Color(0xFF1C1B18),
        borderRadius: BorderRadius.circular(2),
        border: Border.all(color: const Color(0xFF3A362F)),
        boxShadow: const [
          BoxShadow(
            color: Color.fromRGBO(0, 0, 0, 0.45),
            blurRadius: 40,
            offset: Offset(0, 18),
          ),
        ],
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(
            'KEY',
            style: CantoTypography.sans(
              size: 9,
              weight: FontWeight.w600,
              color: CantoColors.faint,
              letterSpacing: 1.5,
            ),
          ),
          const SizedBox(height: 6),
          Row(
            children: [
              for (int i = 0; i < kKeys.length; i++) ...[
                if (i > 0) const SizedBox(width: 3),
                Expanded(
                  child: GestureDetector(
                    onTap: () {
                      final k = kKeys[i].name;
                      if (_key == k) return;
                      setState(() {
                        _pushUndo();
                        _key = k;
                        _reapplyCorrect();
                        _commitUndo();
                      });
                    },
                    child: Container(
                      height: 28,
                      alignment: Alignment.center,
                      decoration: BoxDecoration(
                        color: _key == kKeys[i].name
                            ? CantoColors.accent
                            : const Color(0xFF141311),
                        borderRadius: BorderRadius.circular(2),
                      ),
                      child: Text(
                        kKeys[i].name,
                        style: CantoTypography.mono(
                          size: 10,
                          weight: _key == kKeys[i].name
                              ? FontWeight.w600
                              : FontWeight.w500,
                          color: _key == kKeys[i].name
                              ? const Color(0xFF1A1612)
                              : const Color(0xFFC8C2B6),
                        ),
                      ),
                    ),
                  ),
                ),
              ],
            ],
          ),
          const SizedBox(height: 12),
          Text(
            'SCALE',
            style: CantoTypography.sans(
              size: 9,
              weight: FontWeight.w600,
              color: CantoColors.faint,
              letterSpacing: 1.5,
            ),
          ),
          const SizedBox(height: 6),
          Row(
            children: [
              for (final entry in ['minor', 'major', 'penta'].indexed) ...[
                if (entry.$1 > 0) const SizedBox(width: 4),
                Expanded(
                  child: GestureDetector(
                    onTap: () {
                      final m = entry.$2;
                      if (_mode == m) return;
                      setState(() {
                        _pushUndo();
                        _mode = m;
                        _reapplyCorrect();
                        _commitUndo();
                      });
                    },
                    child: Container(
                      height: 28,
                      alignment: Alignment.center,
                      decoration: BoxDecoration(
                        color: _mode == entry.$2
                            ? const Color(0xFF2A241C)
                            : const Color(0xFF141311),
                        borderRadius: BorderRadius.circular(2),
                        border: Border.all(
                          color: _mode == entry.$2
                              ? CantoColors.accent
                              : Colors.transparent,
                        ),
                      ),
                      child: Text(
                        kModeLabels[entry.$2]!.toUpperCase(),
                        style: CantoTypography.sans(
                          size: 9,
                          weight: FontWeight.w600,
                          color: _mode == entry.$2
                              ? CantoColors.inkBright
                              : const Color(0xFFC8C2B6),
                          letterSpacing: 0.72,
                        ),
                      ),
                    ),
                  ),
                ),
              ],
            ],
          ),
        ],
      ),
    );
  }

  Widget _buildToolbar() {
    const toolDefs = [
      (id: 'pitch', label: 'Pitch', icon: LucideIcons.moveVertical),
      (id: 'drift', label: 'Drift', icon: LucideIcons.activity),
      (id: 'time', label: 'Time', icon: LucideIcons.moveHorizontal),
      (id: 'formant', label: 'Formant', icon: LucideIcons.blend),
      (id: 'level', label: 'Level', icon: LucideIcons.audioLines),
    ];

    final snapLabels = {'off': 'Off', 'semi': 'Semi', 'scale': 'Key'};
    final snapColor = _snap == 'scale'
        ? CantoColors.accent
        : _snap == 'semi'
            ? CantoColors.ink
            : const Color(0xFF8A857C);

    return Container(
      height: 48,
      decoration: const BoxDecoration(
        color: CantoColors.panel,
        border: Border(
          bottom: BorderSide(color: CantoColors.line, width: 1),
        ),
      ),
      child: Row(
        children: [
          Expanded(
            child: Row(
              children: [
                for (final item in toolDefs)
                  Expanded(
                    child: GestureDetector(
                      key: Key('tool-${item.id}'),
                      behavior: HitTestBehavior.opaque,
                      onTap: () => _setTool(item.id),
                      child: Container(
                        height: 48,
                        decoration: BoxDecoration(
                          gradient: _tool == item.id
                              ? const LinearGradient(
                                  begin: Alignment.bottomCenter,
                                  end: Alignment.topCenter,
                                  colors: [
                                    Color.fromRGBO(227, 154, 98, 0.09),
                                    Colors.transparent,
                                  ],
                                  stops: [0.0, 0.72],
                                )
                              : null,
                          border: Border(
                            bottom: BorderSide(
                              color: _tool == item.id
                                  ? CantoColors.accent
                                  : Colors.transparent,
                              width: 2,
                            ),
                          ),
                        ),
                        child: Column(
                          mainAxisAlignment: MainAxisAlignment.center,
                          children: [
                            Icon(
                              item.icon,
                              size: 16,
                              color: _tool == item.id
                                  ? CantoColors.inkBright
                                  : const Color(0xFF8A857C),
                            ),
                            const SizedBox(height: 3),
                            Text(
                              item.label.toUpperCase(),
                              style: CantoTypography.sans(
                                size: 9,
                                weight: FontWeight.w600,
                                color: _tool == item.id
                                    ? CantoColors.inkBright
                                    : const Color(0xFF8A857C),
                                letterSpacing: 0.72,
                              ),
                            ),
                          ],
                        ),
                      ),
                    ),
                  ),
              ],
            ),
          ),
          Container(
            decoration: const BoxDecoration(
              border: Border(
                left: BorderSide(color: CantoColors.line, width: 1),
              ),
            ),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                GestureDetector(
                  key: const Key('snap-btn'),
                  behavior: HitTestBehavior.opaque,
                  onTap: _cycleSnap,
                  child: SizedBox(
                    width: 40,
                    height: 48,
                    child: Column(
                      mainAxisAlignment: MainAxisAlignment.center,
                      children: [
                        Icon(LucideIcons.magnet, size: 16, color: snapColor),
                        const SizedBox(height: 2),
                        Text(
                          snapLabels[_snap]!.toUpperCase(),
                          style: CantoTypography.sans(
                            size: 8,
                            weight: FontWeight.w600,
                            color: snapColor,
                            letterSpacing: 0.64,
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
                _buildSideIconButton(
                  key: const Key('undo-btn'),
                  icon: LucideIcons.undo2,
                  enabled: _undoStack.isNotEmpty,
                  onTap: _undo,
                ),
                _buildSideIconButton(
                  key: const Key('redo-btn'),
                  icon: LucideIcons.redo2,
                  enabled: _redoStack.isNotEmpty,
                  onTap: _redo,
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildSideIconButton({
    required Key key,
    required IconData icon,
    required bool enabled,
    required VoidCallback onTap,
  }) {
    return GestureDetector(
      key: key,
      behavior: HitTestBehavior.opaque,
      onTap: enabled ? onTap : null,
      child: Opacity(
        opacity: enabled ? 1.0 : 0.32,
        child: SizedBox(
          width: 40,
          height: 48,
          child: Center(
            child: Icon(
              icon,
              size: 16,
              color: const Color(0xFF8A857C),
            ),
          ),
        ),
      ),
    );
  }

  Widget _buildEditorArea() {
    return LayoutBuilder(
      builder: (context, constraints) {
        _editorWidth = math.max(1.0, constraints.maxWidth);
        _editorHeight = math.max(1.0, constraints.maxHeight);

        final dragNote = (_drag?.moved ?? false) && _drag?.noteId != null
            ? _notes.where((n) => n.id == _drag!.noteId).firstOrNull
            : null;
        Offset? floaterOffset;
        if (dragNote != null) {
          floaterOffset = noteAnchorOffset(
            note: dragNote,
            width: _editorWidth,
            height: _editorHeight,
            bypass: _bypass,
            viewStart: _viewStart,
            viewDuration: _viewDuration,
            pitchLo: _pitchLo,
            pitchHi: _pitchHi,
          );
        }

        return Container(
          color: CantoColors.well,
          child: Stack(
            clipBehavior: Clip.hardEdge,
            children: [
              Listener(
                behavior: HitTestBehavior.opaque,
                onPointerDown: _onEditorPointerDown,
                onPointerMove: _onEditorPointerMove,
                onPointerUp: _onEditorPointerUp,
                onPointerCancel: _onEditorPointerUp,
                child: SizedBox.expand(
                  child: CustomPaint(
                    painter: EditorPainter(
                      notes: _notes,
                      keyName: _key,
                      mode: _mode,
                      bpm: _bpm,
                      duration: _duration,
                      playhead: _playhead,
                      loop: _loop,
                      bypass: _bypass,
                      selectedId: _selectedId,
                      hoverId: _hoverId,
                      tool: _tool,
                      viewStart: _viewStart,
                      viewDuration: _viewDuration,
                      pitchLo: _pitchLo,
                      pitchHi: _pitchHi,
                      isDraggingPitch:
                          (_drag?.moved ?? false) && _drag?.type == 'pitch',
                      dragNoteId: _drag?.noteId,
                    ),
                  ),
                ),
              ),
              if (!_hasAudio || _notes.isEmpty || _errorMessage != null)
                _buildEmptyOverlay(),
              if (dragNote != null && floaterOffset != null)
                Positioned(
                  left: clampVal(floaterOffset.dx - 44, 6, _editorWidth - 96),
                  top: floaterOffset.dy < 52
                      ? floaterOffset.dy + 14
                      : floaterOffset.dy - 32,
                  child: IgnorePointer(
                    child: Container(
                      padding: const EdgeInsets.symmetric(
                        horizontal: 6,
                        vertical: 3,
                      ),
                      decoration: BoxDecoration(
                        color: CantoColors.inkBright,
                        borderRadius: BorderRadius.circular(2),
                        boxShadow: const [
                          BoxShadow(
                            color: Color.fromRGBO(0, 0, 0, 0.28),
                            blurRadius: 18,
                            offset: Offset(0, 8),
                          ),
                        ],
                      ),
                      child: Text(
                        _floaterLabel(dragNote),
                        style: CantoTypography.mono(
                          size: 11,
                          weight: FontWeight.w600,
                          color: const Color(0xFF1A1612),
                        ),
                      ),
                    ),
                  ),
                ),
            ],
          ),
        );
      },
    );
  }

  Widget _buildEmptyOverlay() {
    return Center(
      child: Container(
        margin: const EdgeInsets.symmetric(horizontal: 28),
        padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 18),
        decoration: BoxDecoration(
          color: const Color.fromRGBO(22, 21, 19, 0.94),
          borderRadius: BorderRadius.circular(3),
          border: Border.all(color: const Color(0xFF3A342C)),
          boxShadow: const [
            BoxShadow(
              color: Color.fromRGBO(0, 0, 0, 0.45),
              blurRadius: 32,
              offset: Offset(0, 12),
            ),
          ],
        ),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(
              _errorMessage != null
                  ? LucideIcons.circleAlert
                  : LucideIcons.fileAudio,
              size: 24,
              color: _errorMessage != null
                  ? CantoColors.heat
                  : CantoColors.accent,
            ),
            const SizedBox(height: 10),
            Text(
              _errorMessage != null
                  ? 'Audio Load Error'
                  : _hasAudio
                      ? 'No Pitched Vocal Notes Found'
                      : 'Load a Vocal Audio File',
              textAlign: TextAlign.center,
              style: CantoTypography.sans(
                size: 14,
                weight: FontWeight.w600,
                color: CantoColors.inkBright,
              ),
            ),
            const SizedBox(height: 6),
            Text(
              _errorMessage ??
                  (_hasAudio
                      ? 'The loaded clip did not contain pitched vocal phrases above the noise threshold.'
                      : 'Select a monophonic vocal recording (WAV, MP3, FLAC, OGG, M4A) to run YIN pitch & TD-PSOLA analysis.'),
              textAlign: TextAlign.center,
              style: CantoTypography.sans(
                size: 11,
                weight: FontWeight.w400,
                color: CantoColors.muted,
                height: 1.35,
              ),
            ),
            const SizedBox(height: 14),
            Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                GestureDetector(
                  key: const Key('empty-load-btn'),
                  onTap: _analyzing ? null : pickAndLoadAudio,
                  child: Container(
                    height: 32,
                    padding: const EdgeInsets.symmetric(horizontal: 14),
                    decoration: BoxDecoration(
                      color: CantoColors.accent,
                      borderRadius: BorderRadius.circular(2),
                    ),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        const Icon(
                          LucideIcons.folderOpen,
                          size: 13,
                          color: Color(0xFF1A1612),
                        ),
                        const SizedBox(width: 6),
                        Text(
                          _analyzing ? 'ANALYZING...' : 'LOAD AUDIO FILE',
                          style: CantoTypography.sans(
                            size: 10.5,
                            weight: FontWeight.w700,
                            color: const Color(0xFF1A1612),
                            letterSpacing: 1.1,
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
                if (_hasAudio) ...[
                  const SizedBox(width: 8),
                  GestureDetector(
                    onTap: _analyzing ? null : analyzeLoadedAudio,
                    child: Container(
                      height: 32,
                      padding: const EdgeInsets.symmetric(horizontal: 12),
                      decoration: BoxDecoration(
                        color: const Color(0xFF221E19),
                        borderRadius: BorderRadius.circular(2),
                        border: Border.all(color: const Color(0xFF4A3B2C)),
                      ),
                      child: Row(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          const Icon(
                            LucideIcons.scanSearch,
                            size: 13,
                            color: CantoColors.ink,
                          ),
                          const SizedBox(width: 5),
                          Text(
                            'RE-ANALYZE',
                            style: CantoTypography.sans(
                              size: 10.5,
                              weight: FontWeight.w600,
                              color: CantoColors.ink,
                              letterSpacing: 1.0,
                            ),
                          ),
                        ],
                      ),
                    ),
                  ),
                ],
              ],
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildInspector() {
    final off = _countOff();
    final note = _selectedNote;
    final hasNote = note != null;

    String noteNameText = '—';
    String noteCentsText = '—';
    String noteMetaText = 'Select a note';
    String noteHzText = '—';
    bool centsIsHeat = false;

    if (hasNote) {
      if (note.unpitched) {
        noteNameText = 'Noise';
        noteCentsText = '';
        noteHzText = '—';
        noteMetaText = 'Unpitched breath';
      } else {
        final shown = presentNote(note, false);
        final reading = centsFromTarget(shown.pitch, _key, _mode);
        final heard = midiToHz(shown.pitch);
        noteNameText = spellMidi(reading.target, _key);
        noteCentsText = formatCents(reading.cents);
        centsIsHeat = reading.cents.abs() >= 15;
        noteHzText = heard >= 1000
            ? heard.toStringAsFixed(0)
            : heard.toStringAsFixed(1);
        final was = centsFromTarget(note.detected, _key, _mode);
        final parts = <String>[
          if (note.lyric.isNotEmpty) note.lyric,
          formatBars(note.start, _bpm),
        ];
        if ((note.pitch - note.detected).abs() > 0.03) {
          parts.add('was ${formatCents(was.cents)}');
        }
        if (note.pinned) parts.add('edited');
        noteMetaText = parts.join(' · ');
      }
    }

    final emptyTitleText = !_hasAudio
        ? 'No audio loaded'
        : _notes.isEmpty
            ? 'No notes detected'
            : off == 0
                ? 'In the scale'
                : off == 1
                    ? '1 note off the scale'
                    : '$off notes off the scale';
    final emptySubText = !_hasAudio
        ? 'Tap Load to open an audio file'
        : '$_key ${kModeLabels[_mode]!.toLowerCase()} · ${_notes.where((n) => !n.unpitched).length} notes';

    final model = _paramModel(note);
    final statsDisabled = note == null || note.unpitched;
    final stDrift = statsDisabled ? '—' : '${(note.modulation * 100).round()}%';
    final stFormant = statsDisabled ? '—' : formatSigned(note.formant, 1);
    final stLevel = statsDisabled ? '—' : formatDb(note.gainDb);

    final canTuneNote = note != null && !note.unpitched && !_bypass;
    final showRevert = note != null && note.pinned;

    return Container(
      padding: const EdgeInsets.fromLTRB(12, 10, 12, 8),
      decoration: const BoxDecoration(
        color: CantoColors.panel,
        border: Border(
          top: BorderSide(color: CantoColors.line, width: 1),
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          if (hasNote)
            Row(
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Row(
                        crossAxisAlignment: CrossAxisAlignment.baseline,
                        textBaseline: TextBaseline.alphabetic,
                        children: [
                          Text(
                            noteNameText,
                            key: const Key('note-name'),
                            style: CantoTypography.mono(
                              size: 22,
                              weight: FontWeight.w600,
                              color: CantoColors.ink,
                              letterSpacing: -0.5,
                              height: 1.0,
                            ),
                          ),
                          const SizedBox(width: 8),
                          Text(
                            noteCentsText,
                            key: const Key('note-cents'),
                            style: CantoTypography.mono(
                              size: 13,
                              weight: FontWeight.w500,
                              color: centsIsHeat
                                  ? CantoColors.heat
                                  : CantoColors.muted,
                              height: 1.0,
                            ),
                          ),
                        ],
                      ),
                      const SizedBox(height: 5),
                      Text(
                        noteMetaText,
                        key: const Key('note-meta'),
                        style: CantoTypography.sans(
                          size: 11,
                          weight: FontWeight.w500,
                          color: CantoColors.muted,
                          height: 1.0,
                        ),
                      ),
                    ],
                  ),
                ),
                Row(
                  crossAxisAlignment: CrossAxisAlignment.baseline,
                  textBaseline: TextBaseline.alphabetic,
                  children: [
                    Text(
                      noteHzText,
                      key: const Key('note-hz'),
                      style: CantoTypography.mono(
                        size: 20,
                        weight: FontWeight.w500,
                        color: CantoColors.ink,
                        letterSpacing: -0.4,
                        height: 1.0,
                      ),
                    ),
                    const SizedBox(width: 2),
                    Text(
                      'Hz',
                      style: CantoTypography.sans(
                        size: 10,
                        weight: FontWeight.w500,
                        color: CantoColors.faint,
                      ),
                    ),
                  ],
                ),
              ],
            )
          else
            Row(
              mainAxisAlignment: MainAxisAlignment.spaceBetween,
              crossAxisAlignment: CrossAxisAlignment.baseline,
              textBaseline: TextBaseline.alphabetic,
              children: [
                Text(
                  emptyTitleText,
                  style: CantoTypography.sans(
                    size: 13,
                    weight: FontWeight.w600,
                    color: CantoColors.ink,
                    height: 1.0,
                  ),
                ),
                const SizedBox(height: 6),
                Text(
                  emptySubText,
                  style: CantoTypography.sans(
                    size: 11,
                    weight: FontWeight.w500,
                    color: CantoColors.muted,
                    height: 1.0,
                  ),
                ),
              ],
            ),
          const SizedBox(height: 10),
          Row(
            children: [
              Expanded(
                child: _buildStatCard(
                  toolId: 'drift',
                  label: 'Drift',
                  value: stDrift,
                  icon: LucideIcons.activity,
                  disabled: statsDisabled,
                ),
              ),
              const SizedBox(width: 4),
              Expanded(
                child: _buildStatCard(
                  toolId: 'formant',
                  label: 'Formant',
                  value: stFormant,
                  icon: LucideIcons.blend,
                  disabled: statsDisabled,
                ),
              ),
              const SizedBox(width: 4),
              Expanded(
                child: _buildStatCard(
                  toolId: 'level',
                  label: 'Level',
                  value: stLevel,
                  icon: LucideIcons.audioLines,
                  disabled: statsDisabled,
                ),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              SizedBox(
                width: 52,
                child: Text(
                  model.name.toUpperCase(),
                  style: CantoTypography.sans(
                    size: 9,
                    weight: FontWeight.w600,
                    color: CantoColors.faint,
                    letterSpacing: 1.26,
                  ),
                ),
              ),
              Expanded(
                child: _CantoRangeSlider(
                  value: model.value,
                  min: model.min,
                  max: model.max,
                  bipolar: model.bipolar,
                  disabled: model.disabled || _bypass,
                  onChangeStart: () {
                    final n = _selectedNote;
                    if (n == null || n.unpitched || _bypass) return;
                    _pushUndo();
                    if (_tool == 'pitch') {
                      _sliderTarget =
                          centsFromTarget(n.pitch, _key, _mode).target;
                    }
                  },
                  onChanged: (val) => _applyParam(val),
                  onChangeEnd: () => setState(() => _commitUndo()),
                ),
              ),
              SizedBox(
                width: 56,
                child: Text(
                  model.text,
                  textAlign: TextAlign.right,
                  style: CantoTypography.mono(
                    size: 11,
                    weight: FontWeight.w500,
                    color: CantoColors.ink,
                  ),
                ),
              ),
            ],
          ),
          const SizedBox(height: 4),
          Row(
            children: [
              SizedBox(
                width: 52,
                child: Text(
                  'CORRECT',
                  style: CantoTypography.sans(
                    size: 9,
                    weight: FontWeight.w600,
                    color: CantoColors.faint,
                    letterSpacing: 1.26,
                  ),
                ),
              ),
              Expanded(
                child: _CantoRangeSlider(
                  value: (_correct * 100).roundToDouble(),
                  min: 0,
                  max: 100,
                  bipolar: false,
                  disabled: _notes.isEmpty,
                  onChangeStart: _pushUndo,
                  onChanged: (val) {
                    setState(() {
                      _correct = val / 100.0;
                      _reapplyCorrect();
                    });
                  },
                  onChangeEnd: () => setState(() => _commitUndo()),
                ),
              ),
              SizedBox(
                width: 34,
                child: Text(
                  '${(_correct * 100).round()}%',
                  textAlign: TextAlign.right,
                  style: CantoTypography.mono(
                    size: 11,
                    weight: FontWeight.w500,
                    color: CantoColors.ink,
                  ),
                ),
              ),
              const SizedBox(width: 8),
              if (showRevert)
                GestureDetector(
                  key: const Key('revert-btn'),
                  onTap: _revertNote,
                  child: Padding(
                    padding: const EdgeInsets.symmetric(
                      horizontal: 4,
                      vertical: 4,
                    ),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        const Icon(
                          LucideIcons.rotateCcw,
                          size: 10,
                          color: CantoColors.muted,
                        ),
                        const SizedBox(width: 3),
                        Text(
                          'REVERT',
                          style: CantoTypography.sans(
                            size: 10,
                            weight: FontWeight.w600,
                            color: CantoColors.muted,
                            letterSpacing: 1.2,
                          ),
                        ),
                      ],
                    ),
                  ),
                )
              else
                GestureDetector(
                  key: const Key('reset-btn'),
                  onTap: _resetAll,
                  child: Padding(
                    padding: const EdgeInsets.symmetric(
                      horizontal: 4,
                      vertical: 4,
                    ),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        const Icon(
                          LucideIcons.rotateCcw,
                          size: 10,
                          color: CantoColors.muted,
                        ),
                        const SizedBox(width: 3),
                        Text(
                          'RESET',
                          style: CantoTypography.sans(
                            size: 10,
                            weight: FontWeight.w600,
                            color: CantoColors.muted,
                            letterSpacing: 1.2,
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              const SizedBox(width: 6),
              GestureDetector(
                key: const Key('tune-btn'),
                onTap: (_bypass || _notes.isEmpty) ? null : _tune,
                child: Opacity(
                  opacity: (_bypass || _notes.isEmpty) ? 0.32 : 1.0,
                  child: Container(
                    height: 26,
                    padding: const EdgeInsets.symmetric(horizontal: 8),
                    decoration: BoxDecoration(
                      color: const Color(0xFF2A2118),
                      borderRadius: BorderRadius.circular(2),
                      border: Border.all(color: const Color(0xFF6A4A32)),
                    ),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        const Icon(
                          LucideIcons.sparkles,
                          size: 11,
                          color: Color(0xFFF0D3B4),
                        ),
                        const SizedBox(width: 4),
                        Text(
                          canTuneNote ? 'TUNE' : 'TUNE ALL',
                          style: CantoTypography.sans(
                            size: 10,
                            weight: FontWeight.w600,
                            color: const Color(0xFFF0D3B4),
                            letterSpacing: 1.4,
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _buildStatCard({
    required String toolId,
    required String label,
    required String value,
    required IconData icon,
    required bool disabled,
  }) {
    final active = _tool == toolId;
    return GestureDetector(
      key: Key('stat-$toolId'),
      onTap: disabled ? null : () => _setTool(toolId),
      child: Opacity(
        opacity: disabled ? 0.32 : 1.0,
        child: Container(
          padding: const EdgeInsets.fromLTRB(6, 5, 6, 4),
          decoration: BoxDecoration(
            color: active ? const Color(0xFF221E19) : CantoColors.well,
            borderRadius: BorderRadius.circular(2),
            border: Border.all(
              color: active ? const Color(0xFF4A3B2C) : Colors.transparent,
            ),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Icon(
                    icon,
                    size: 10,
                    color: active ? CantoColors.accent : CantoColors.faint,
                  ),
                  const SizedBox(width: 4),
                  Text(
                    label.toUpperCase(),
                    style: CantoTypography.sans(
                      size: 9,
                      weight: FontWeight.w600,
                      color: CantoColors.faint,
                      letterSpacing: 1.08,
                    ),
                  ),
                ],
              ),
              const SizedBox(height: 2),
              Text(
                value,
                style: CantoTypography.mono(
                  size: 12,
                  weight: FontWeight.w500,
                  color: active ? CantoColors.accent : CantoColors.ink,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _buildOverviewBar() {
    return Container(
      height: 40,
      decoration: const BoxDecoration(
        color: CantoColors.transportBg,
        border: Border(
          top: BorderSide(color: CantoColors.line, width: 1),
        ),
      ),
      child: Row(
        children: [
          Expanded(
            child: LayoutBuilder(
              builder: (context, constraints) {
                final w = math.max(1.0, constraints.maxWidth);
                return Listener(
                  behavior: HitTestBehavior.opaque,
                  onPointerDown: (e) => _onOverviewDown(e, w),
                  onPointerMove: (e) => _onOverviewMove(e, w),
                  onPointerUp: (e) => _onOverviewUp(e, w),
                  onPointerCancel: (e) => _onOverviewUp(e, w),
                  child: SizedBox.expand(
                    child: CustomPaint(
                      painter: OverviewPainter(
                        notes: _notes,
                        waveformPeaks: _waveformPeaks,
                        keyName: _key,
                        mode: _mode,
                        bpm: _bpm,
                        duration: _duration,
                        playhead: _playhead,
                        bypass: _bypass,
                        selectedId: _selectedId,
                        viewStart: _viewStart,
                        viewDuration: _viewDuration,
                        pitchLo: _pitchLo,
                        pitchHi: _pitchHi,
                      ),
                    ),
                  ),
                );
              },
            ),
          ),
          Container(
            width: 36,
            decoration: const BoxDecoration(
              color: CantoColors.panel,
              border: Border(
                left: BorderSide(color: CantoColors.line, width: 1),
              ),
            ),
            child: Column(
              children: [
                Expanded(
                  child: GestureDetector(
                    key: const Key('zoom-in'),
                    behavior: HitTestBehavior.opaque,
                    onTap: () =>
                        _zoomAt(0.8, _viewStart + _viewDuration / 2.0),
                    child: const Center(
                      child: Icon(
                        LucideIcons.plus,
                        size: 13,
                        color: Color(0xFFC8C2B6),
                      ),
                    ),
                  ),
                ),
                Container(height: 1, color: CantoColors.line),
                Expanded(
                  child: GestureDetector(
                    key: const Key('zoom-out'),
                    behavior: HitTestBehavior.opaque,
                    onTap: () =>
                        _zoomAt(1.25, _viewStart + _viewDuration / 2.0),
                    child: const Center(
                      child: Icon(
                        LucideIcons.minus,
                        size: 13,
                        color: Color(0xFFC8C2B6),
                      ),
                    ),
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildTransportFooter() {
    const meterShape = [0.35, 0.5, 0.68, 0.84, 1.0];
    return Container(
      height: 58,
      padding: const EdgeInsets.only(left: 2, right: 10),
      decoration: const BoxDecoration(
        color: CantoColors.transportBg,
        border: Border(
          top: BorderSide(color: CantoColors.line, width: 1),
        ),
      ),
      child: Row(
        children: [
          Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              GestureDetector(
                key: const Key('rewind-btn'),
                behavior: HitTestBehavior.opaque,
                onTap: () {
                  setState(() {
                    _seek(0.0);
                    _viewStart = 0.0;
                    _follow = true;
                  });
                },
                child: const SizedBox(
                  width: 36,
                  height: 40,
                  child: Center(
                    child: Icon(
                      LucideIcons.skipBack,
                      size: 16,
                      color: Color(0xFFB7B1A6),
                    ),
                  ),
                ),
              ),
              GestureDetector(
                key: const Key('play-btn'),
                behavior: HitTestBehavior.opaque,
                onTap: _togglePlay,
                child: SizedBox(
                  width: 36,
                  height: 40,
                  child: Center(
                    child: Icon(
                      _playing ? LucideIcons.pause : LucideIcons.play,
                      size: 18,
                      color: _playing
                          ? CantoColors.accent
                          : CantoColors.inkBright,
                    ),
                  ),
                ),
              ),
              GestureDetector(
                key: const Key('loop-btn'),
                behavior: HitTestBehavior.opaque,
                onTap: () {
                  setState(() => _loop = !_loop);
                  _syncRustEngine();
                },
                child: SizedBox(
                  width: 36,
                  height: 40,
                  child: Center(
                    child: Icon(
                      LucideIcons.repeat,
                      size: 16,
                      color: _loop
                          ? CantoColors.accent
                          : const Color(0xFFB7B1A6),
                    ),
                  ),
                ),
              ),
            ],
          ),
          Expanded(
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Text(
                  formatClock(_playhead),
                  key: const Key('clock-text'),
                  style: CantoTypography.mono(
                    size: 18,
                    weight: FontWeight.w600,
                    color: CantoColors.inkBright,
                    letterSpacing: -0.54,
                    height: 1.0,
                  ),
                ),
                const SizedBox(height: 4),
                Text(
                  '${formatBars(_playhead, _bpm)} · 4/4 · ${_bpm.round()}',
                  key: const Key('bars-text'),
                  style: CantoTypography.mono(
                    size: 10,
                    weight: FontWeight.w500,
                    color: CantoColors.faint,
                    height: 1.0,
                  ),
                ),
              ],
            ),
          ),
          Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              SizedBox(
                height: 16,
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.end,
                  children: [
                    for (int i = 0; i < 5; i++) ...[
                      if (i > 0) const SizedBox(width: 2),
                      Builder(
                        builder: (context) {
                          final lit = _level > 0.05 &&
                              _level >= ((i + 0.45) / 5.0) * 0.9;
                          final h = lit ? 4.0 + meterShape[i] * 12.0 : 3.0;
                          Color barColor = const Color(0xFF2E2B26);
                          if (lit) {
                            if (i == 3) {
                              barColor = CantoColors.accent;
                            } else if (i == 4) {
                              barColor = CantoColors.heat;
                            } else {
                              barColor = const Color(0xFFC8B49A);
                            }
                          }
                          return Container(
                            width: 3,
                            height: h,
                            color: barColor,
                          );
                        },
                      ),
                    ],
                  ],
                ),
              ),
              const SizedBox(width: 8),
              GestureDetector(
                key: const Key('bypass-btn'),
                onTap: () {
                  setState(() {
                    _bypass = !_bypass;
                    _syncRustEngine();
                  });
                },
                child: Container(
                  height: 26,
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  decoration: BoxDecoration(
                    color: _bypass
                        ? const Color(0xFF3A221C)
                        : Colors.transparent,
                    borderRadius: BorderRadius.circular(2),
                    border: Border.all(
                      color: _bypass
                          ? const Color(0xFFA85B45)
                          : const Color(0xFF3A342C),
                    ),
                  ),
                  child: Row(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      Icon(
                        _bypass ? LucideIcons.earOff : LucideIcons.ear,
                        size: 11,
                        color: _bypass
                            ? const Color(0xFFF0B8A4)
                            : CantoColors.muted,
                      ),
                      const SizedBox(width: 4),
                      Text(
                        'BYPASS',
                        style: CantoTypography.sans(
                          size: 10,
                          weight: FontWeight.w600,
                          color: _bypass
                              ? const Color(0xFFF0B8A4)
                              : CantoColors.muted,
                          letterSpacing: 1.4,
                        ),
                      ),
                    ],
                  ),
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _buildHomeIndicator() {
    return Container(
      height: 14,
      color: CantoColors.transportBg,
      alignment: Alignment.center,
      child: Container(
        width: 104,
        height: 4,
        decoration: BoxDecoration(
          color: const Color(0xFF34312C),
          borderRadius: BorderRadius.circular(999),
        ),
      ),
    );
  }
}

class _CantoWaveLogoPainter extends CustomPainter {
  const _CantoWaveLogoPainter();

  @override
  void paint(Canvas canvas, Size size) {
    final sx = size.width / 16.0;
    final sy = size.height / 16.0;
    canvas.save();
    canvas.scale(sx, sy);

    final path = Path()
      ..moveTo(1.4, 10.1)
      ..relativeCubicTo(1.1, -2.5, 2.2, -3.6, 3.5, -2.1)
      ..relativeCubicTo(1.1, 1.3, 1.6, 2.5, 2.9, 2.3)
      ..relativeCubicTo(1.4, -0.2, 1.8, -2.3, 3.1, -2.1)
      ..relativeCubicTo(1.2, 0.2, 1.8, 1.7, 2.9, 0.9);

    final paint = Paint()
      ..color = CantoColors.accent
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.7
      ..strokeCap = StrokeCap.round;

    canvas.drawPath(path, paint);
    canvas.restore();
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) => false;
}

class _CantoRangeSlider extends StatelessWidget {
  final double value;
  final double min;
  final double max;
  final bool bipolar;
  final bool disabled;
  final VoidCallback? onChangeStart;
  final ValueChanged<double>? onChanged;
  final VoidCallback? onChangeEnd;

  const _CantoRangeSlider({
    required this.value,
    required this.min,
    required this.max,
    required this.bipolar,
    required this.disabled,
    this.onChangeStart,
    this.onChanged,
    this.onChangeEnd,
  });

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final width = math.max(1.0, constraints.maxWidth);
        void handleDx(double dx) {
          if (disabled || onChanged == null) return;
          final t = clampVal(dx / width, 0.0, 1.0);
          final next = min + (max - min) * t;
          onChanged!(next);
        }

        return Opacity(
          opacity: disabled ? 0.38 : 1.0,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onHorizontalDragStart: disabled
                ? null
                : (details) {
                    onChangeStart?.call();
                    handleDx(details.localPosition.dx);
                  },
            onHorizontalDragUpdate: disabled
                ? null
                : (details) => handleDx(details.localPosition.dx),
            onHorizontalDragEnd: disabled ? null : (_) => onChangeEnd?.call(),
            onTapDown: disabled
                ? null
                : (details) {
                    onChangeStart?.call();
                    handleDx(details.localPosition.dx);
                    onChangeEnd?.call();
                  },
            child: SizedBox(
              height: 22,
              child: CustomPaint(
                painter: _RangeSliderPainter(
                  value: value,
                  min: min,
                  max: max,
                  bipolar: bipolar,
                ),
              ),
            ),
          ),
        );
      },
    );
  }
}

class _RangeSliderPainter extends CustomPainter {
  final double value;
  final double min;
  final double max;
  final bool bipolar;

  const _RangeSliderPainter({
    required this.value,
    required this.min,
    required this.max,
    required this.bipolar,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final w = size.width;
    final cy = size.height / 2.0;
    final span = (max - min).abs() < 1e-6 ? 1.0 : (max - min);
    final p = clampVal((value - min) / span, 0.0, 1.0);

    final trackPaint = Paint()..color = const Color(0xFF3A372F);
    canvas.drawRRect(
      RRect.fromRectAndRadius(
        Rect.fromLTWH(0, cy - 1, w, 2),
        const Radius.circular(1),
      ),
      trackPaint,
    );

    final activePaint = Paint()..color = CantoColors.accent;
    if (bipolar) {
      final mid = clampVal((0.0 - min) / span, 0.0, 1.0);
      final a = math.min(mid, p) * w;
      final b = math.max(mid, p) * w;
      if (b > a) {
        canvas.drawRect(Rect.fromLTWH(a, cy - 1, b - a, 2), activePaint);
      }
    } else {
      final fillW = p * w;
      if (fillW > 0) {
        canvas.drawRect(Rect.fromLTWH(0, cy - 1, fillW, 2), activePaint);
      }
    }

    final thumbX = clampVal(p * w, 6.0, math.max(6.0, w - 6.0));
    final thumbCenter = Offset(thumbX, cy);
    canvas.drawCircle(
      thumbCenter,
      6.5,
      Paint()..color = const Color.fromRGBO(0, 0, 0, 0.35),
    );
    canvas.drawCircle(
      thumbCenter,
      6.0,
      Paint()..color = CantoColors.inkBright,
    );
    canvas.drawCircle(
      thumbCenter,
      6.0,
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1.0
        ..color = const Color(0xFF2A241C),
    );
  }

  @override
  bool shouldRepaint(covariant _RangeSliderPainter oldDelegate) =>
      oldDelegate.value != value ||
      oldDelegate.min != min ||
      oldDelegate.max != max ||
      oldDelegate.bipolar != bipolar;
}
