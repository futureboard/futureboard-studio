import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter/services.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import 'audio.dart';
import 'cache.dart';
import 'draw.dart';
import 'history.dart';
import 'l10n/app_localizations.dart';
import 'locale_controller.dart';
import 'pages/home_page.dart';
import 'pages/record_page.dart';
import 'pages/settings_page.dart';
import 'recorder.dart';
import 'music.dart';
import 'project.dart';
import 'session.dart';
import 'ui/advanced_tools.dart';
import 'rust_dsp.dart';
import 'theme.dart';
import 'ui/components.dart';
import 'ui/expression_sheet.dart';
import 'ui/history_sheet.dart';
import 'ui/loading_view.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  // Any orientation: portrait phones get the stacked editor, landscape and
  // tablets the side-panel layout (`CantoScreenState.build`).
  SystemChrome.setSystemUIOverlayStyle(
    const SystemUiOverlayStyle(
      statusBarColor: Colors.transparent,
      statusBarIconBrightness: Brightness.light,
      systemNavigationBarColor: CantoColors.surface,
      systemNavigationBarIconBrightness: Brightness.light,
    ),
  );
  runApp(const CantoApp());
}

class CantoApp extends StatefulWidget {
  /// Injected by tests; the app creates and loads its own otherwise.
  final LocaleController? localeController;
  final SnapDefaultController? snapDefault;

  /// Projects store; resolved from Documents/Solfege Voxtune when null.
  final ProjectStore? store;

  /// Replaces the Home page (tests open the editor directly).
  final Widget? home;

  const CantoApp({
    super.key,
    this.localeController,
    this.snapDefault,
    this.store,
    this.home,
  });

  @override
  State<CantoApp> createState() => _CantoAppState();
}

class _CantoAppState extends State<CantoApp> {
  late final LocaleController _locale;
  late final SnapDefaultController _snapDefault;
  late final Future<ProjectStore> _store;

  @override
  void initState() {
    super.initState();
    _locale = widget.localeController ?? (LocaleController()..load());
    _snapDefault = widget.snapDefault ?? (SnapDefaultController()..load());
    _store = widget.store != null
        ? Future.value(widget.store)
        : ProjectStore.open();
  }

  @override
  void dispose() {
    if (widget.localeController == null) _locale.dispose();
    if (widget.snapDefault == null) _snapDefault.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return SettingsScope(
      locale: _locale,
      snapDefault: _snapDefault,
      child: ValueListenableBuilder<Locale?>(
        valueListenable: _locale,
        builder: (context, locale, _) => _buildApp(locale),
      ),
    );
  }

  Widget _buildHome() {
    return FutureBuilder<ProjectStore>(
      future: _store,
      builder: (context, snapshot) {
        if (snapshot.hasData) return HomePage(store: snapshot.data!);
        final l = AppLocalizations.of(context);
        return Scaffold(
          backgroundColor: CantoColors.surface,
          body: Center(
            child: snapshot.hasError
                ? Padding(
                    padding: const EdgeInsets.all(24),
                    child: Text(
                      l.storageError('${snapshot.error}'),
                      textAlign: TextAlign.center,
                      style: CantoTypography.sans(color: CantoColors.heat),
                    ),
                  )
                : const CircularProgressIndicator(),
          ),
        );
      },
    );
  }

  Widget _buildApp(Locale? locale) {
    return MaterialApp(
      title: 'Solfege Voxtune',
      debugShowCheckedModeBanner: false,
      // null follows the device language (Thai devices get Thai).
      locale: locale,
      supportedLocales: AppLocalizations.supportedLocales,
      localizationsDelegates: AppLocalizations.localizationsDelegates,
      // Mona Sans VF defaults to wght 200; keep unstyled text at Regular.
      builder: (context, child) => DefaultTextStyle.merge(
        style: const TextStyle(
          fontVariations: [FontVariation('wght', 400)],
          fontFamilyFallback: CantoTypography.fallback,
        ),
        child: child!,
      ),
      theme: ThemeData(
        brightness: Brightness.dark,
        scaffoldBackgroundColor: CantoColors.surface,
        fontFamily: CantoTypography.sansFamily,
        colorScheme: const ColorScheme.dark(
          primary: CantoColors.accent,
          surface: CantoColors.surface,
        ),
        bottomSheetTheme: const BottomSheetThemeData(
          backgroundColor: CantoColors.surface,
          surfaceTintColor: Colors.transparent,
          modalBarrierColor: Color(0x99000000),
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.vertical(
              top: Radius.circular(CantoRadii.sheet),
            ),
          ),
        ),
      ),
      home: widget.home ?? _buildHome(),
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

  _PressState({required this.hit, this.moved = false, required this.timeMs});
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

const _toolDefs = [
  (id: 'pitch', icon: LucideIcons.moveVertical),
  (id: 'drift', icon: LucideIcons.activity),
  (id: 'time', icon: LucideIcons.moveHorizontal),
  (id: 'formant', icon: LucideIcons.blend),
  (id: 'level', icon: LucideIcons.audioLines),
];

String _toolLabel(AppLocalizations l, String id) => switch (id) {
  'drift' => l.toolDrift,
  'time' => l.toolTime,
  'formant' => l.toolFormant,
  'level' => l.toolLevel,
  _ => l.toolPitch,
};

/// Lower-case mode name used after a key ("A minor").
String _modeLabel(AppLocalizations l, String mode) => switch (mode) {
  'major' => l.modeMajor,
  'penta' => l.modePenta,
  _ => l.modeMinor,
};

class CantoScreen extends StatefulWidget {
  final Project project;
  final ProjectStore store;

  /// Microphone recorder for new takes; tests inject a fake.
  final LiveRecorder? Function() recorderFactory;

  const CantoScreen({
    super.key,
    required this.project,
    required this.store,
    this.recorderFactory = NativeLiveRecorder.tryOpen,
  });

  @override
  State<CantoScreen> createState() => CantoScreenState();
}

class CantoScreenState extends State<CantoScreen>
    with SingleTickerProviderStateMixin, WidgetsBindingObserver {
  static const List<String> _tools = [
    'pitch',
    'drift',
    'time',
    'formant',
    'level',
  ];

  // ---- Project & per-file sessions --------------------------------------
  // The open project's files each have a FileSession (own engine, notes,
  // undo, view). The fields below delegate to the active one, so the editor
  // code works on "the current file" unchanged.
  late final Project _project = widget.project;
  late final ProjectStore _store = widget.store;
  late final ProjectCache _cache = ProjectCache(_project);
  final List<FileSession> _sessions = [];
  final FileSession _emptySession = FileSession(null);
  FileSession? _active;
  FileSession get _s => _active ?? _emptySession;
  Timer? _saveTimer;
  Timer? _historyTimer;

  // "Undo · Cut" confirmation shown briefly over the editor.
  final ValueNotifier<String?> _toastVN = ValueNotifier(null);
  Timer? _toastTimer;
  final GlobalKey<ScaffoldState> _scaffoldKey = GlobalKey<ScaffoldState>();
  final GlobalKey _editorKey = GlobalKey();
  bool _exporting = false;

  List<VocalNote> get _notes => _s.notes;
  List<double> get _waveformPeaks => _s.waveformPeaks;
  set _waveformPeaks(List<double> v) => _s.waveformPeaks = v;
  String? get _fileName => _s.name;
  double get _audioDuration => _s.audioDuration;
  set _audioDuration(double v) => _s.audioDuration = v;
  bool get _hasAudio => _s.hasAudio;
  set _hasAudio(bool v) => _s.hasAudio = v;
  String? get _errorMessage => _s.error;
  set _errorMessage(String? v) => _s.error = v;
  String get _key => _s.key;
  set _key(String v) => _s.key = v;
  String get _mode => _s.mode;
  set _mode(String v) => _s.mode = v;
  String? get _detectedKey => _s.detectedKey;
  set _detectedKey(String? v) => _s.detectedKey = v;
  String? get _detectedMode => _s.detectedMode;
  set _detectedMode(String? v) => _s.detectedMode = v;
  double get _bpm => _s.bpm;
  set _bpm(double v) => _s.bpm = v;
  double get _correct => _s.correct;
  set _correct(double v) => _s.correct = v;
  String? get _selectedId => _s.selectedId;
  set _selectedId(String? v) => _s.selectedId = v;
  double get _viewStart => _s.viewStart;
  set _viewStart(double v) => _s.viewStart = v;
  double get _viewDuration => _s.viewDuration;
  set _viewDuration(double v) => _s.viewDuration = v;
  double get _pitchLo => _s.pitchLo;
  set _pitchLo(double v) => _s.pitchLo = v;
  double get _pitchHi => _s.pitchHi;
  set _pitchHi(double v) => _s.pitchHi = v;
  List<EditorSnapshot> get _undoStack => _s.undoStack;
  List<EditorSnapshot> get _redoStack => _s.redoStack;
  SynthEngine get _synth => _s.synth;

  /// The file being loaded or re-analysed in the background, if any.
  _FileLoad? _load;
  bool get _analyzing => _load != null;

  /// From opening the project until its first file is ready the whole
  /// screen is the Loading Project page.
  bool _projectOpening = false;

  // Cut mode (note separation): the note being cut and where.
  bool _cutMode = false;
  String? _cutNoteId;
  double? _cutTime;
  int _idSeq = 0;
  bool _snapInitialized = false;
  String _tool = 'pitch';
  String _snap = 'scale'; // 'off' | 'semi' | 'scale'
  bool _playing = false;
  // Per-frame transport values live in notifiers so playback only repaints
  // the playhead overlays, clock and meter instead of rebuilding the screen.
  final ValueNotifier<double> _playheadVN = ValueNotifier(0.0);
  final ValueNotifier<double> _levelVN = ValueNotifier(0.0);
  double get _playhead => _playheadVN.value;
  set _playhead(double value) => _playheadVN.value = value;
  set _level(double value) => _levelVN.value = value;
  bool _loop = true;
  bool _bypass = false;
  bool _follow = true;
  String? _hoverId;
  EditorSnapshot? _pendingSnapshot;
  List<EditorSnapshot>? _redoBackup;

  _DragState? _drag;
  _PressState? _press;
  _PinchState? _pinch;
  _OverviewDragState? _overviewDrag;
  final Map<int, Offset> _pointers = {};

  int _lastTapMs = 0;
  String? _lastTapId;

  double _editorWidth = 390.0;
  double _editorHeight = 460.0;
  int _sliderTarget = 69;

  late final Ticker _ticker;
  Duration _lastTickElapsed = Duration.zero;
  bool _compact = false;
  bool _narrow = false;
  bool _wide = false;
  bool _pinnedSidebar = false;
  double _panelWidth = 340;

  /// Side panel narrower than a phone: secondary labels collapse to icons.
  bool _tightPanel = false;

  List<VocalNote> get notes => List.unmodifiable(_notes);
  AppLocalizations get _l => AppLocalizations.of(context);
  String? get fileName => _fileName;
  bool get hasAudio => _hasAudio;
  Project get project => _project;
  @visibleForTesting
  List<FileSession> get sessionsForTest => List.unmodifiable(_sessions);

  /// Global position of a note's centre on the canvas (same transform the
  /// painter and hit-testing use).
  @visibleForTesting
  Offset? noteCenterForTest(String id) {
    final note = _notes.where((n) => n.id == id).firstOrNull;
    final box = _editorKey.currentContext?.findRenderObject() as RenderBox?;
    if (note == null || box == null) return null;
    return box.localToGlobal(
      noteAnchorOffset(
        note: note,
        width: _editorWidth,
        height: _editorHeight,
        bypass: _bypass,
        viewStart: _viewStart,
        viewDuration: _viewDuration,
        pitchLo: _pitchLo,
        pitchHi: _pitchHi,
      ),
    );
  }

  @override
  void initState() {
    super.initState();
    _ticker = createTicker(_onTick);
    WidgetsBinding.instance.addObserver(this);
    _sessions.addAll(_project.files.map(FileSession.new));
    _projectOpening = _sessions.isNotEmpty;
    if (_sessions.isNotEmpty) {
      // Analyse after the first frame so the editor appears immediately.
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) _activate(_sessions.first);
      });
    }
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    // The editor opens with the user's default snap mode (Settings).
    if (!_snapInitialized) {
      _snapInitialized = true;
      _snap = SettingsScope.of(context).snapDefault.value;
    }
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    // Leaving the app: make sure nothing is lost and audio stops.
    if (state == AppLifecycleState.paused ||
        state == AppLifecycleState.detached) {
      if (_playing) _pause();
      _saveNow();
      _flushHistory();
    }
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _saveNow();
    _flushHistory();
    _toastTimer?.cancel();
    _toastVN.dispose();
    // Freeing a running job cancels it; its worker keeps the engine state
    // alive until it has stopped, so the sessions can go right away.
    _load?.timer?.cancel();
    _load?.job.free();
    _ticker.dispose();
    for (final s in _sessions) {
      s.dispose();
    }
    _emptySession.dispose();
    _playheadVN.dispose();
    _levelVN.dispose();
    super.dispose();
  }

  VocalNote? get _selectedNote =>
      _notes.where((n) => n.id == _selectedId).firstOrNull;

  double get _duration =>
      _audioDuration > 0.0 ? _audioDuration : phraseDuration(_bpm);

  // ---- Project persistence ----------------------------------------------

  /// Debounced autosave; edits commit at the end of each gesture. The undo
  /// history goes to the cache on a longer debounce (it is bigger and only
  /// matters once the app is closed), and right away when the app pauses.
  void _scheduleSave() {
    _saveTimer?.cancel();
    _saveTimer = Timer(const Duration(milliseconds: 600), _saveNow);
    _s.historyDirty = true;
    _historyTimer?.cancel();
    _historyTimer = Timer(const Duration(seconds: 3), _flushHistory);
  }

  /// Writes the undo history of every changed file to the project cache.
  void _flushHistory() {
    _historyTimer?.cancel();
    _historyTimer = null;
    for (final s in _sessions) {
      final file = s.file;
      if (!s.loaded || !s.historyDirty || file == null) continue;
      s.historyDirty = false;
      if (s.undoStack.isEmpty && s.redoStack.isEmpty) {
        _cache.deleteHistory(file);
        continue;
      }
      _cache.writeHistory(
        file,
        HistoryCodec.encode(
          undo: s.undoStack,
          redo: s.redoStack,
          current: s.snapshot(),
        ),
      );
    }
  }

  // ---- Loading -------------------------------------------------------------
  // Decoding and analysis run on a native worker thread (`load_job.rs`), so
  // the UI stays live and shows real progress. One load at a time.

  @visibleForTesting
  bool get isLoadingForTest => _load != null;

  /// Decodes and analyses [session] in the background, reusing the cached
  /// analysis when the audio is unchanged (then it is about as fast as
  /// decoding). With [activate] false the file is only prepared (export) and
  /// the edited file stays as it is. Completes true once installed.
  Future<bool> _loadSession(
    FileSession session, {
    bool reanalyze = false,
    bool activate = true,
    FileSession? previous,
  }) {
    final file = session.file!;
    final cached = reanalyze ? null : _cache.readAnalysis(file);
    final job = reanalyze
        ? session.synth.startReanalyze()
        : session.synth.startLoad(
            _project.pathOf(file),
            cachedAnalysis: cached,
          );
    if (job == null) {
      // No native engine on this platform: a failed load, not a hang.
      session.error = _l.loadFailed;
      setState(() => _projectOpening = false);
      return Future.value(false);
    }
    final load = _FileLoad(
      session: session,
      job: job,
      reanalyze: reanalyze,
      activate: activate,
      hasCache: cached != null,
      previous: previous,
    );
    load.timer = Timer.periodic(
      const Duration(milliseconds: 40),
      (_) => _pollLoad(load),
    );
    setState(() => _load = load);
    return load.done.future;
  }

  void _pollLoad(_FileLoad load) {
    final usedCache = load.job.usedCache;
    final res = load.job.takeResult(fromCache: usedCache);
    if (res == null) {
      final stage = load.job.stage;
      final progress = load.job.progress;
      if (stage != load.stage || (progress - load.progress).abs() >= 0.005) {
        setState(() {
          load
            ..stage = stage
            ..progress = progress;
        });
      }
      return;
    }
    load.timer?.cancel();
    load.job.free();
    setState(() => _load = null);
    if (load.cancelled) {
      // Back to the file that was open before, if any.
      final previous = load.previous;
      if (identical(_active, load.session) &&
          previous != null &&
          previous.loaded &&
          _sessions.contains(previous)) {
        _activate(previous);
      }
      load.done.complete(false);
      return;
    }
    _installLoaded(load, res);
    load.done.complete(res.ok);
  }

  /// Applies a finished load to its session (analysis, saved edits, undo
  /// history) and caches a fresh analysis.
  void _installLoaded(_FileLoad load, CantoAnalysisResult res) {
    final session = load.session;
    final previous = _active;
    final playhead = _playhead;
    _active = session;
    _applyAnalysisResult(res);
    if (res.ok) {
      session.fromCache = res.fromCache;
      if (!res.fromCache && res.json != null) {
        _cache.writeAnalysis(session.file!, res.json!);
      }
      if (!load.reanalyze) _restoreSaved(session);
    }
    if (!load.activate) {
      _active = previous;
      _playhead = playhead;
    }
    if (load.reanalyze) _scheduleSave();
    setState(() => _projectOpening = false);
  }

  void _cancelLoad() {
    final load = _load;
    if (load == null || load.cancelled) return;
    // The worker stops at its next check; `_pollLoad` then finishes up.
    load.job.cancel();
    setState(() => load.cancelled = true);
  }

  void _saveNow() {
    _saveTimer?.cancel();
    _saveTimer = null;
    for (final s in _sessions) {
      if (s.loaded && s.file != null) s.captureInto(s.file!);
    }
    try {
      _store.save(_project);
    } catch (_) {
      // Storage unavailable (removed card, revoked permission): keep
      // editing; the next save retries.
    }
  }

  // ---- Files ---------------------------------------------------------------

  /// Adds an audio file to the project (picked from the device).
  Future<void> pickAndLoadAudio() async {
    if (_analyzing) return;
    final picked = await _synth.pickAudioFile();
    if (picked == null || !mounted) return;
    loadAudioPath(picked.path, fileName: picked.name);
  }

  /// Copies [path] into the project and opens it.
  Future<bool> loadAudioPath(String path, {String? fileName}) {
    final label = (fileName != null && fileName.isNotEmpty)
        ? fileName
        : path.split(RegExp(r'[/\\]')).last;
    final ProjectFile file;
    try {
      file = _store.importFile(_project, path, label);
    } catch (e) {
      setState(() => _emptySession.error = '$e');
      return Future.value(false);
    }
    return _openNewFile(file);
  }

  /// Adds in-memory audio to the project and opens it.
  Future<bool> loadAudioBytes(
    Uint8List bytes, {
    String fileName = 'Loaded Audio.wav',
    String? extHint,
  }) {
    final file = _store.importBytes(_project, bytes, fileName);
    return _openNewFile(file);
  }

  /// Opens the recording screen; a kept take is added to the project and
  /// opened (analysed) right away.
  Future<void> openRecorder() async {
    if (_playing) _pause();
    _synth.stopAudition();
    final recorder = widget.recorderFactory();
    if (recorder == null) {
      ScaffoldMessenger.of(
        context,
      ).showSnackBar(SnackBar(content: Text(_l.recordFailed(_l.loadFailed))));
      return;
    }
    final target = _store.newRecording(_project, _l.takeName);
    final saved = await Navigator.of(context).push<String>(
      MaterialPageRoute(
        builder: (_) => RecordPage(
          recorder: recorder,
          targetPath: target.path,
          projectName: _project.name,
          keyName: _key,
          mode: _mode,
        ),
      ),
    );
    recorder.dispose();
    if (saved == null || !mounted) return;
    _openNewFile(_store.registerFile(_project, saved, target.name));
  }

  Future<bool> _openNewFile(ProjectFile file) {
    final session = FileSession(file);
    _sessions.add(session);
    return _activate(session);
  }

  /// Makes [session] the edited file. The first activation loads it in the
  /// background (decode, analysis or cache, saved edits); later ones are
  /// instant. Completes true once the file is ready.
  Future<bool> _activate(FileSession session) {
    if (_load != null) return Future.value(false);
    if (_playing) _pause();
    _synth.stopAudition();
    final previous = _active;
    previous?.playhead = _playhead;
    _active = session;
    _pendingSnapshot = null;
    _redoBackup = null;
    _drag = null;
    _press = null;
    if (session.loaded) {
      setState(() => _playhead = session.playhead);
      _syncRustEngine();
      _synth.seek(_playhead);
      return Future.value(true);
    }
    setState(() => _errorMessage = null);
    return _loadSession(session, previous: previous);
  }

  /// Saved key/scale/correction and per-note edits, applied after analysis,
  /// then the undo history if the cache has one that ends in this state.
  void _restoreSaved(FileSession session) {
    final file = session.file!;
    setState(() {
      if (file.key != null && kKeys.any((k) => k.name == file.key)) {
        _key = file.key!;
      }
      if (file.mode != null && kModes.containsKey(file.mode)) {
        _mode = file.mode!;
      }
      _correct = file.correct;
      _reapplyCorrect();
      if (session.restoreEdits(file)) _synth.replaceNotes(_notes);
      _syncRustEngine();
      final doc = _cache.readHistory(file);
      final history = doc == null
          ? null
          : HistoryCodec.decode(doc, session.snapshot());
      if (history != null) {
        session.undoStack.addAll(history.undo);
        session.redoStack.addAll(history.redo);
      }
    });
  }

  void _removeFile(FileSession session) {
    if (_exporting) return; // its engine may be rendering right now
    if (_playing) _pause();
    final index = _sessions.indexOf(session);
    _sessions.remove(session);
    _store.removeFile(_project, session.file!);
    _cache.remove(session.file!);
    session.dispose();
    if (identical(_active, session)) {
      _active = null;
      if (_sessions.isNotEmpty) {
        _activate(_sessions[math.min(index, _sessions.length - 1)]);
      }
    }
    setState(() {});
  }

  /// Analyses the current file again (background, with progress).
  Future<bool> analyzeLoadedAudio() {
    final session = _active;
    if (!_hasAudio || session == null) {
      pickAndLoadAudio();
      return Future.value(false);
    }
    if (_load != null) return Future.value(false);
    if (_playing) _pause();
    return _loadSession(session, reanalyze: true);
  }

  void _applyAnalysisResult(CantoAnalysisResult res) {
    setState(() {
      if (!res.ok) {
        _errorMessage = res.error ?? _l.loadFailed;
        return;
      }
      _s.loaded = true;
      _errorMessage = null;
      _hasAudio = res.durationSec > 0.0;
      _audioDuration = math.max(0.5, res.durationSec);
      _bpm = res.bpm.clamp(60.0, 200.0);
      _key = kKeys.any((k) => k.name == res.key) ? res.key : 'A';
      _mode = kModes.containsKey(res.mode) ? res.mode : 'minor';
      _detectedKey = _key;
      _detectedMode = _mode;
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

  bool _isOff(VocalNote note) {
    if (note.unpitched || note.muted) return false;
    final shown = presentNote(note, false);
    return centsFromTarget(shown.pitch, _key, _mode).cents.abs() >= 15;
  }

  int _countOff() => _notes.where(_isOff).length;

  /// Selects the next off-key note after the current selection (wrapping)
  /// and scrolls it into view.
  void _jumpToNextOff() {
    final off = _notes.where(_isOff).toList()
      ..sort((a, b) => a.start.compareTo(b.start));
    if (off.isEmpty) return;
    final current = _selectedNote;
    final target = current == null
        ? off.first
        : (off.where((n) => n.start > current.start + 1e-6).firstOrNull ??
              off.first);
    setState(() {
      _selectedId = target.id;
      _follow = false;
      if (target.start < _viewStart ||
          target.start + target.dur > _viewStart + _viewDuration) {
        _viewStart = target.start + target.dur / 2.0 - _viewDuration / 2.0;
        _clampView();
      }
      _ensurePitchVisible(target.pitch);
    });
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
    _viewStart = clampVal(
      _viewStart,
      0.0,
      math.max(0.0, total - _viewDuration),
    );
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

  EditorSnapshot _takeSnapshot({String label = ''}) =>
      _s.snapshot(label: label);

  /// Restores a snapshot, including note structure (cuts/joins), and hands
  /// the full note list to the engine.
  void _applySnapshot(EditorSnapshot snap) {
    _correct = snap.correct;
    _key = snap.key;
    _mode = snap.mode;
    _notes
      ..clear()
      ..addAll(snap.notes.map((n) => n.copy()));
    for (final note in _notes) {
      if (!note.unpitched) note.lyric = solfegeForMidi(note.pitch, _key);
    }
    if (!_notes.any((n) => n.id == _selectedId)) _selectedId = null;
    _synth.replaceNotes(_notes);
    _syncRustEngine();
  }

  /// Opens an undo step for the edit named [label] (a `history.dart` code).
  void _pushUndo(String label) {
    final snap = _takeSnapshot(label: label);
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
    _scheduleSave();
  }

  /// Undoes or redoes until [target] steps are left to undo. A snapshot's
  /// label names the edit made *after* it, so it travels with the step
  /// (undoing "Cut" moves it to the redo side and back). Only the final
  /// state reaches the engine, however many steps are crossed.
  void _moveHistory(int target) {
    EditorSnapshot relabel(EditorSnapshot s, String label) => EditorSnapshot(
      correct: s.correct,
      key: s.key,
      mode: s.mode,
      notes: s.notes,
      label: label,
    );
    var current = _takeSnapshot();
    while (_undoStack.length > target) {
      final snap = _undoStack.removeLast();
      _redoStack.add(relabel(current, snap.label));
      current = snap;
    }
    while (_undoStack.length < target && _redoStack.isNotEmpty) {
      final snap = _redoStack.removeLast();
      _undoStack.add(relabel(current, snap.label));
      current = snap;
    }
    _applySnapshot(current);
  }

  void _undo() {
    if (_undoStack.isEmpty || _exporting || _load != null) return;
    final label = _undoStack.last.label;
    setState(() => _moveHistory(_undoStack.length - 1));
    _scheduleSave();
    _showToast(_l.undoToast(_historyLabel(label)));
  }

  void _redo() {
    if (_redoStack.isEmpty || _exporting || _load != null) return;
    final label = _redoStack.last.label;
    setState(() => _moveHistory(_undoStack.length + 1));
    _scheduleSave();
    _showToast(_l.redoToast(_historyLabel(label)));
  }

  /// Moves to history row [index] (`_historyRows` order), so every state in
  /// the list stays reachable.
  void _jumpToHistory(int index) {
    if (_exporting) return;
    final target = index.clamp(0, _undoStack.length + _redoStack.length);
    if (target == _undoStack.length) return;
    setState(() => _moveHistory(target));
    _scheduleSave();
  }

  /// Step names from the start of the history to the last redoable step.
  List<String> _historyRows() => [
    _l.historyStart,
    for (final s in _undoStack) _historyLabel(s.label),
    for (final s in _redoStack.reversed) _historyLabel(s.label),
  ];

  /// Localised name of a history code (`_pushUndo` call sites).
  String _historyLabel(String code) {
    final l = _l;
    final parts = code.split(':');
    final arg = parts.length > 1 ? parts[1] : '';
    switch (parts.first) {
      case 'tool':
        return _toolLabel(l, arg);
      case 'key':
        return l.histKey(arg);
      case 'mode':
        return l.histScale(_modeLabel(l, arg));
      case 'tune':
        return l.histSnapNote;
      case 'tuneAll':
        return l.tuneAll;
      case 'correct':
        return l.histCorrect;
      case 'revert':
        return l.revert;
      case 'resetAll':
        return l.resetAll;
      case 'cut':
        return l.histCut;
      case 'join':
        return l.histJoin;
      case 'mute':
        return l.histMute;
      case 'unmute':
        return l.histUnmute;
      case 'reset':
        return l.histReset;
      case 'orn':
        return l.histOrnament;
      case 'accent':
        return l.histAccent;
      case 'exprClear':
        return l.histExprClear;
      case 'adv':
        return switch (arg) {
          'center' => l.advCenter,
          'drift' => l.advDrift,
          'quantize' => l.advQuantize,
          'transpose' =>
            '${l.advTranspose} '
                '${formatSigned(double.tryParse(parts.last) ?? 0, 0)} st',
          'level' => l.advLevel,
          'accentBeats' => l.advAccentBeats,
          _ => l.advancedTools,
        };
      default:
        return l.histEdit;
    }
  }

  void _showToast(String text) {
    _toastTimer?.cancel();
    _toastVN.value = text;
    _toastTimer = Timer(const Duration(milliseconds: 1400), () {
      _toastVN.value = null;
    });
  }

  void _openHistory() {
    if (_playing) _pause();
    showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      constraints: BoxConstraints(
        maxHeight: MediaQuery.sizeOf(context).height * 0.8,
      ),
      builder: (_) => StatefulBuilder(
        builder: (sheetContext, setSheetState) => HistorySheet(
          labels: _historyRows(),
          current: _undoStack.length,
          onJump: (i) {
            _jumpToHistory(i);
            setSheetState(() {});
          },
        ),
      ),
    );
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

  void _setKey(String key) {
    if (_key == key) return;
    setState(() {
      _pushUndo('key:$key');
      _key = key;
      _reapplyCorrect();
      _commitUndo();
    });
  }

  void _setMode(String mode) {
    if (_mode == mode) return;
    setState(() {
      _pushUndo('mode:$mode');
      _mode = mode;
      _reapplyCorrect();
      _commitUndo();
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
    final dtSec = ((elapsed - _lastTickElapsed).inMicroseconds / 1e6).clamp(
      0.0,
      0.1,
    );
    _lastTickElapsed = elapsed;

    final frames = (dtSec * SynthEngine.sampleRate).round().clamp(128, 4096);

    if (_playing) {
      final step = _synth.stepAudio(frames);
      if (!step.isPlaying && !_loop && step.playhead >= _duration - 0.02) {
        _playhead = _duration;
        _pause();
        return;
      }
      // Notifiers repaint only the playhead overlays, clock and meter; the
      // full tree rebuilds only when follow-scrolling moves the view.
      _playhead = step.playhead;
      _level = step.level;
      if (_follow && _drag == null && _pinch == null) {
        final before = _viewStart;
        _followPlayhead();
        if (_viewStart != before) setState(() {});
      }
    } else if (_synth.isAuditioning) {
      // Note preview: native output just reports the level; the fallback
      // path renders here.
      _level = _synth.stepAudio(frames).level;
    } else {
      _level = 0.0;
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
    _syncRustEngine();
    _synth.seek(_playhead);
    _synth.activateOutput();
    if (!_ticker.isActive) {
      _ticker.start();
    }
  }

  void _rewind() {
    setState(() {
      _seek(0.0);
      _viewStart = 0.0;
      _follow = true;
    });
  }

  void _toggleLoop() {
    setState(() => _loop = !_loop);
    _syncRustEngine();
  }

  void _toggleBypass() {
    setState(() {
      _bypass = !_bypass;
      _syncRustEngine();
    });
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
      _pushUndo('tune');
      note.pitch = nearestScale(note.pitch, _key, _mode).toDouble();
      note.modulation = math.min(note.modulation, 0.70);
      note.lyric = solfegeForMidi(note.pitch, _key);
      note.pinned = true;
      _commitUndo();
    });
  }

  void _tuneAll() {
    if (_notes.isEmpty || _bypass) return;
    setState(() {
      _pushUndo('tuneAll');
      _correct = 1.0;
      _reapplyCorrect();
      _commitUndo();
    });
  }

  void _resetAll() {
    if (_notes.isEmpty) return;
    setState(() {
      _pushUndo('resetAll');
      _correct = 0.0;
      for (final note in _notes) {
        note.pitch = note.detected;
        note.start = note.detectedStart;
        note.dur = note.detectedDur;
        note.modulation = 1.0;
        note.formant = 0.0;
        note.gainDb = note.detectedGain;
        note.pinned = false;
        note.ornStyle = kOrnNone;
        note.accent = 0;
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
      _pushUndo('revert');
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
    final name = _toolLabel(_l, _tool);
    if (note == null || note.unpitched) {
      return _ParamModel(
        name: name,
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
        name: name,
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
        name: name,
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
        name: name,
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
        name: name,
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
      name: name,
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
      // Only this note changed; a full sync per slider tick is wasted work.
      _synth.syncNote(_notes.indexOf(note), note);
    });
  }

  // ---- Note structure: cut / join / mute / reset ----------------------------

  static const double _minCut = 0.04;

  double _clampCut(VocalNote note, double time) =>
      clampVal(time, note.start + _minCut, note.start + note.dur - _minCut);

  String _newNoteId(String base) {
    String id;
    do {
      id = '$base.${++_idSeq}';
    } while (_notes.any((n) => n.id == id));
    return id;
  }

  /// A change to the note list itself, as one undo step. The engine gets the
  /// whole new list (its per-index sync can't express added/removed notes).
  void _structuralEdit(String label, VoidCallback change) {
    setState(() {
      _pushUndo(label);
      change();
      for (final n in _notes) {
        if (!n.unpitched) n.lyric = solfegeForMidi(n.pitch, _key);
      }
      _synth.replaceNotes(_notes);
      _commitUndo();
    });
  }

  void _cutNote(VocalNote note, double time) {
    final halves = splitNote(note, time, _newNoteId, minDur: _minCut);
    if (halves == null) return;
    _structuralEdit('cut', () {
      final i = _notes.indexOf(note);
      _notes
        ..removeAt(i)
        ..insertAll(i, [halves.$1, halves.$2]);
      _selectedId = halves.$2.id;
    });
  }

  VocalNote? _nextNote(VocalNote note) {
    final i = _notes.indexOf(note);
    return i >= 0 && i + 1 < _notes.length ? _notes[i + 1] : null;
  }

  void _joinWithNext(VocalNote note) {
    final next = _nextNote(note);
    if (next == null) return;
    _structuralEdit('join', () {
      final joined = joinNotes(note, next, _newNoteId(note.id));
      final i = _notes.indexOf(note);
      _notes
        ..removeRange(i, i + 2)
        ..insert(i, joined);
      _selectedId = joined.id;
    });
  }

  void _toggleMute(VocalNote note) {
    setState(() {
      _pushUndo(note.muted ? 'unmute' : 'mute');
      note.muted = !note.muted;
      _commitUndo();
    });
  }

  /// Back to exactly what analysis found for this note.
  void _resetNote(VocalNote note) {
    setState(() {
      _pushUndo('reset');
      note
        ..pitch = note.detected
        ..start = note.detectedStart
        ..dur = note.detectedDur
        ..modulation = 1.0
        ..formant = 0.0
        ..gainDb = note.detectedGain
        ..pinned = false
        ..muted = false
        ..ornStyle = kOrnNone
        ..accent = 0;
      if (!note.unpitched) note.lyric = solfegeForMidi(note.pitch, _key);
      _commitUndo();
    });
  }

  // ---- ลูกคอ & accent ----------------------------------------------------------

  /// Per-note ornament and accent sheet. Changes are heard right away
  /// (engine sync + Listen) and each gesture is one undo step.
  void _openExpression(VocalNote note) {
    if (_playing) _pause();
    final id = note.id;
    VocalNote? current() => _notes.where((n) => n.id == id).firstOrNull;
    showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      constraints: BoxConstraints(
        maxHeight: MediaQuery.sizeOf(context).height * 0.88,
      ),
      builder: (_) => ExpressionSheet(
        note: current,
        onBegin: _pushUndo,
        onUpdate: (change) {
          final n = current();
          if (n == null) return;
          setState(() {
            change(n);
            _synth.syncNote(_notes.indexOf(n), n);
          });
        },
        onEnd: () => setState(_commitUndo),
        onPreview: () {
          final n = current();
          if (n == null) return;
          // Plays the note once, from its start, as it now sounds.
          _synth.auditionNote(_notes.indexOf(n));
          _synth.releaseAudition();
          if (!_ticker.isActive) {
            _lastTickElapsed = Duration.zero;
            _ticker.start();
          }
        },
      ),
    );
  }

  // ---- Advanced tools (macros) ------------------------------------------------

  void _openAdvanced() {
    if (_playing) _pause();
    final selected = _selectedNote;
    showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      constraints: BoxConstraints(
        maxHeight: MediaQuery.sizeOf(context).height * 0.88,
      ),
      builder: (_) => AdvancedToolsSheet(
        hasSelection: selected != null,
        onApply: applyAdvanced,
        canUndo: () => _undoStack.isNotEmpty,
        canRedo: () => _redoStack.isNotEmpty,
        onUndo: _undo,
        onRedo: _redo,
      ),
    );
  }

  /// Runs one macro over the selected note or all notes as a single undo
  /// step. Returns how many notes actually changed.
  int applyAdvanced(AdvancedRequest r) {
    final selected = _selectedNote;
    final scope = r.selectedOnly && selected != null ? [selected] : [..._notes];
    var count = 0;
    setState(() {
      _pushUndo(
        r.op == AdvancedOp.transpose
            ? 'adv:transpose:${r.semitones}'
            : 'adv:${r.op.name}',
      );
      switch (r.op) {
        case AdvancedOp.center:
          for (final n in scope) {
            if (n.unpitched || n.muted) continue;
            final target = nearestScale(n.pitch, _key, _mode);
            final next = n.pitch + (target - n.pitch) * r.amount;
            if ((next - n.pitch).abs() < 1e-4) continue;
            n
              ..pitch = next
              ..pinned = true;
            count++;
          }
        case AdvancedOp.drift:
          for (final n in scope) {
            if (n.unpitched || n.muted) continue;
            final next = clampVal(n.modulation * (1 - r.amount), 0, 1.45);
            if ((next - n.modulation).abs() < 1e-4) continue;
            n.modulation = next;
            count++;
          }
        case AdvancedOp.quantize:
          final grid = beatsToSec(r.gridBeats, _bpm);
          for (final n in scope..sort((a, b) => a.start.compareTo(b.start))) {
            if (n.muted) continue;
            final snapped = (n.start / grid).round() * grid;
            final before = n.start;
            n.start += (snapped - n.start) * r.amount;
            _clampTiming(n, 'body');
            if ((n.start - before).abs() > 1e-4) count++;
          }
        case AdvancedOp.transpose:
          for (final n in scope) {
            if (n.unpitched) continue;
            n
              ..pitch += r.semitones
              ..pinned = true;
            count++;
          }
          // Keep the transposed notes on screen.
          final lo = clampVal(
            _pitchLo + r.semitones,
            24,
            96 - (_pitchHi - _pitchLo),
          );
          _pitchHi = lo + (_pitchHi - _pitchLo);
          _pitchLo = lo;
        case AdvancedOp.level:
          final voiced = scope.where((n) => !n.muted).toList();
          if (voiced.length < 2) break;
          final mean = voiced.fold(0.0, (s, n) => s + n.gainDb) / voiced.length;
          for (final n in voiced) {
            final next = clampVal(
              n.gainDb + (mean - n.gainDb) * r.amount,
              -18,
              6,
            );
            if ((next - n.gainDb).abs() < 1e-3) continue;
            n.gainDb = next;
            count++;
          }
        case AdvancedOp.accentBeats:
          // Notes that start on a beat (within 60 ms) get the accent.
          final beat = beatsToSec(1, _bpm);
          for (final n in scope) {
            if (n.unpitched || n.muted) continue;
            final offBeat = (n.start - (n.start / beat).round() * beat).abs();
            if (offBeat > 0.06 || (n.accent - r.amount).abs() < 1e-3) continue;
            n.accent = r.amount;
            count++;
          }
      }
      for (final n in _notes) {
        if (!n.unpitched) n.lyric = solfegeForMidi(n.pitch, _key);
      }
      _commitUndo();
    });
    return count;
  }

  bool _actionsOnTop() {
    final note = _selectedNote;
    if (note == null) return false;
    final y = noteAnchorOffset(
      note: note,
      width: _editorWidth,
      height: _editorHeight,
      bypass: _bypass,
      viewStart: _viewStart,
      viewDuration: _viewDuration,
      pitchLo: _pitchLo,
      pitchHi: _pitchHi,
    ).dy;
    return y > _editorHeight * 0.55;
  }

  /// Floating toolbar for the selected note (or the cut-mode hint).
  Widget? _buildNoteActions() {
    if (!_hasAudio) return null;
    final l = _l;
    if (_cutMode) {
      return FloatingActionBar(
        key: const Key('cut-bar'),
        children: [
          const Padding(
            padding: EdgeInsets.only(left: 6),
            child: Icon(
              LucideIcons.scissors,
              size: 16,
              color: CantoColors.accent,
            ),
          ),
          const SizedBox(width: 8),
          Flexible(
            child: Text(
              l.cutHint,
              style: CantoTypography.sans(
                size: 12.5,
                weight: FontWeight.w600,
                color: CantoColors.ink,
              ),
            ),
          ),
          const SizedBox(width: 10),
          CantoButton(
            key: const Key('cut-done'),
            label: l.done,
            icon: LucideIcons.check,
            height: 34,
            style: CantoButtonStyle.tonal,
            onTap: () => setState(() => _cutMode = false),
          ),
        ],
      );
    }
    final note = _selectedNote;
    if (note == null) return null;
    final editable = !_bypass;
    return FloatingActionBar(
      key: const Key('note-actions'),
      children: [
        ActionBarButton(
          key: const Key('note-cut'),
          icon: LucideIcons.scissors,
          label: l.noteCut,
          onTap: editable ? () => setState(() => _cutMode = true) : null,
        ),
        ActionBarButton(
          key: const Key('note-join'),
          icon: LucideIcons.merge,
          label: l.noteJoin,
          semanticLabel: l.noteJoinSemantic,
          onTap: editable && _nextNote(note) != null
              ? () => _joinWithNext(note)
              : null,
        ),
        ActionBarButton(
          key: const Key('note-expression'),
          icon: LucideIcons.spline,
          label: l.noteExpression,
          active: note.hasExpression,
          onTap: editable && !note.unpitched
              ? () => _openExpression(note)
              : null,
        ),
        ActionBarButton(
          key: const Key('note-mute'),
          icon: note.muted ? LucideIcons.volume2 : LucideIcons.volumeX,
          label: note.muted ? l.noteUnmute : l.noteMute,
          active: note.muted,
          warning: true,
          onTap: editable ? () => _toggleMute(note) : null,
        ),
        ActionBarButton(
          key: const Key('note-reset'),
          icon: LucideIcons.rotateCcw,
          label: l.noteReset,
          onTap: editable ? () => _resetNote(note) : null,
        ),
      ],
    );
  }

  String _floaterLabel(VocalNote note) {
    final type = _drag?.type;
    if (type == 'drift') return 'Drift ${(note.modulation * 100).round()}%';
    if (type == 'formant') return 'Formant ${formatSigned(note.formant, 1)} st';
    if (type == 'level') return formatDb(note.gainDb);
    if (type == 'time') {
      final delta = ((note.start - note.detectedStart) * 1000).round();
      final sign = delta > 0
          ? '+'
          : delta < 0
          ? '−'
          : '';
      return '${formatBars(note.start, _bpm)}  $sign${delta.abs()} ms';
    }
    final reading = centsFromTarget(note.pitch, _key, _mode);
    return '${spellMidi(reading.target, _key)}  ${formatCents(reading.cents)}';
  }

  void _onEditorPointerDown(PointerDownEvent event) {
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
      _synth.stopAudition(); // a second finger means zoom, not preview
      _cutNoteId = null;
      _cutTime = null;
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

    // Cut mode: pressing a note starts a cut line that follows the finger;
    // lifting cuts there. Empty space still pans.
    if (_cutMode && hit.type == 'note' && hit.note != null && !_bypass) {
      final note = hit.note!;
      setState(() {
        _selectedId = note.id;
        _cutNoteId = note.id;
        _cutTime = _clampCut(
          note,
          _currentLayout().xToTime(_viewStart, point.dx),
        );
      });
      return;
    }

    final nowMs = DateTime.now().millisecondsSinceEpoch;
    _press = _PressState(hit: hit, moved: false, timeMs: nowMs);

    // Press-to-hear, like Melodyne: the note sounds as soon as the finger
    // lands, keeps sounding while held (edits are heard live) and finishes
    // its pass on release.
    if (hit.type == 'note' && hit.note != null && _hasAudio && !_playing) {
      _synth.auditionNote(_notes.indexOf(hit.note!));
      if (!_ticker.isActive) {
        _lastTickElapsed = Duration.zero;
        _ticker.start();
      }
    }

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

    if (_cutNoteId != null) {
      final note = _notes.where((n) => n.id == _cutNoteId).firstOrNull;
      if (note != null) {
        setState(() {
          _cutTime = _clampCut(
            note,
            _currentLayout().xToTime(_viewStart, point.dx),
          );
        });
      }
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
        _pushUndo('tool:${drag.type}');
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
    if (_cutNoteId != null) {
      final note = _notes.where((n) => n.id == _cutNoteId).firstOrNull;
      if (note != null) {
        setState(() {
          _cutTime = _clampCut(
            note,
            _currentLayout().xToTime(_viewStart, point.dx),
          );
        });
      }
      return;
    }

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

    // Drags edit exactly one note; the full state is synced on release
    // (`_commitUndo`).
    // The preview started on press reads this note live, so the drag is
    // heard without restarting it.
    _synth.syncNote(noteIndex, note);
  }

  void _onEditorPointerUp(PointerEvent event) {
    _pointers.remove(event.pointer);
    if (_pointers.length < 2) _pinch = null;
    final drag = _drag;
    final press = _press;
    _drag = null;
    _press = null;
    _synth.releaseAudition();
    if (_cutNoteId != null) {
      final note = _notes.where((n) => n.id == _cutNoteId).firstOrNull;
      final time = _cutTime;
      setState(() {
        _cutNoteId = null;
        _cutTime = null;
      });
      if (note != null && time != null && event is PointerUpEvent) {
        _cutNote(note, time);
      }
      return;
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
    final time = clampVal(
      (event.localPosition.dx / width) * _duration,
      0.0,
      _duration,
    );
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

  // ---------------------------------------------------------------------------
  // Sheets

  void _openKeySheet() {
    showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      builder: (sheetContext) => StatefulBuilder(
        builder: (sheetContext, setSheetState) {
          void pickKey(String k) {
            _setKey(k);
            setSheetState(() {});
          }

          void pickMode(String m) {
            _setMode(m);
            setSheetState(() {});
          }

          return _buildKeySheet(pickKey, pickMode);
        },
      ),
    );
  }

  Widget _buildKeySheet(
    ValueChanged<String> pickKey,
    ValueChanged<String> pickMode,
  ) {
    final detected = _detectedKey != null && _detectedMode != null
        ? _l.detectedKey(_detectedKey!, _modeLabel(_l, _detectedMode!))
        : null;
    Widget keyTile(KeyDef k) {
      final selected = _key == k.name;
      final isDetected = _detectedKey == k.name;
      return Expanded(
        child: Pressable(
          key: Key('key-${k.name}'),
          onTap: () => pickKey(k.name),
          child: AnimatedContainer(
            duration: const Duration(milliseconds: 140),
            height: 46,
            margin: const EdgeInsets.all(3),
            decoration: BoxDecoration(
              color: selected ? CantoColors.accent : CantoColors.raised,
              borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
              border: Border.all(
                color: selected ? CantoColors.accent : CantoColors.line,
              ),
            ),
            child: Stack(
              alignment: Alignment.center,
              children: [
                Text(
                  k.name,
                  style: CantoTypography.num(
                    size: 14,
                    weight: FontWeight.w600,
                    color: selected ? CantoColors.accentInk : CantoColors.ink,
                  ),
                ),
                if (isDetected)
                  Positioned(
                    bottom: 5,
                    child: Container(
                      width: 4,
                      height: 4,
                      decoration: BoxDecoration(
                        shape: BoxShape.circle,
                        color: selected
                            ? CantoColors.accentInk
                            : CantoColors.accent,
                      ),
                    ),
                  ),
              ],
            ),
          ),
        ),
      );
    }

    Widget modeTile(String mode, String title, String sub) {
      final selected = _mode == mode;
      return Expanded(
        child: Pressable(
          key: Key('mode-$mode'),
          onTap: () => pickMode(mode),
          child: AnimatedContainer(
            duration: const Duration(milliseconds: 140),
            margin: const EdgeInsets.all(3),
            padding: const EdgeInsets.fromLTRB(10, 9, 8, 9),
            decoration: BoxDecoration(
              color: selected ? CantoColors.accentSoft : CantoColors.raised,
              borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
              border: Border.all(
                color: selected ? CantoColors.accent : CantoColors.line,
              ),
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  title,
                  style: CantoTypography.sans(
                    size: 13,
                    weight: FontWeight.w600,
                    color: selected ? CantoColors.inkBright : CantoColors.ink,
                  ),
                ),
                const SizedBox(height: 2),
                Text(
                  sub,
                  style: CantoTypography.num(
                    size: 9.5,
                    color: CantoColors.muted,
                  ),
                ),
              ],
            ),
          ),
        ),
      );
    }

    String degrees(String mode) {
      final names = scalePcs(_key, mode)
          .map((pc) => spellMidi(60 + pc, _key).replaceAll(RegExp(r'\d'), ''))
          .toList();
      return names.join(' ');
    }

    return CantoSheet(
      key: const Key('key-sheet'),
      title: _l.keySheetTitle,
      trailing: detected == null
          ? null
          : Text(
              detected,
              style: CantoTypography.sans(size: 11.5, color: CantoColors.muted),
            ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(
            _l.rootLabel.toUpperCase(),
            style: CantoTypography.label(text: _l.rootLabel),
          ),
          const SizedBox(height: 4),
          Row(children: [for (final k in kKeys.take(6)) keyTile(k)]),
          Row(children: [for (final k in kKeys.skip(6)) keyTile(k)]),
          const SizedBox(height: 14),
          Text(
            _l.scaleLabel.toUpperCase(),
            style: CantoTypography.label(text: _l.scaleLabel),
          ),
          const SizedBox(height: 4),
          Row(
            children: [
              modeTile('minor', _l.modeTitleMinor, degrees('minor')),
              modeTile('major', _l.modeTitleMajor, degrees('major')),
              modeTile('penta', _l.modeTitlePenta, degrees('penta')),
            ],
          ),
          const SizedBox(height: 12),
          Text(
            _l.keySheetNote,
            style: CantoTypography.sans(
              size: 11.5,
              color: CantoColors.muted,
              height: 1.4,
            ),
          ),
        ],
      ),
    );
  }

  void _openSessionSheet() {
    showModalBottomSheet<void>(
      context: context,
      builder: (sheetContext) {
        void run(VoidCallback action) {
          Navigator.of(sheetContext).pop();
          action();
        }

        final pitchedCount = _notes.where((n) => !n.unpitched).length;
        final rows = <(String, String)>[
          (_l.sessionLength, _hasAudio ? formatClock(_audioDuration) : '—'),
          (_l.sessionTempo, _hasAudio ? _l.bpmValue(_bpm.round()) : '—'),
          (
            _l.sessionKey,
            _detectedKey != null
                ? _l.keyChip(_detectedKey!, _modeLabel(_l, _detectedMode!))
                : '—',
          ),
          (_l.sessionNotes, _hasAudio ? '$pitchedCount' : '—'),
        ];
        return CantoSheet(
          key: const Key('session-sheet'),
          title: _fileName ?? _l.noAudioLoaded,
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            mainAxisSize: MainAxisSize.min,
            children: [
              Container(
                padding: const EdgeInsets.symmetric(vertical: 10),
                decoration: BoxDecoration(
                  color: CantoColors.raised,
                  borderRadius: BorderRadius.circular(CantoRadii.card),
                  border: Border.all(color: CantoColors.line),
                ),
                child: Row(
                  children: [
                    for (final (label, value) in rows)
                      Expanded(
                        child: Column(
                          children: [
                            Text(
                              label.toUpperCase(),
                              style: CantoTypography.label(text: label),
                            ),
                            const SizedBox(height: 4),
                            Text(
                              value,
                              style: CantoTypography.num(
                                size: 13,
                                weight: FontWeight.w600,
                                color: CantoColors.inkBright,
                              ),
                            ),
                          ],
                        ),
                      ),
                  ],
                ),
              ),
              const SizedBox(height: 4),
              SheetAction(
                key: const Key('sheet-load-btn'),
                icon: LucideIcons.filePlus,
                title: _l.addAudioFile,
                subtitle: _l.openAudioFileHint,
                onTap: _analyzing ? null : () => run(pickAndLoadAudio),
              ),
              SheetAction(
                key: const Key('analyze-btn'),
                icon: LucideIcons.scanSearch,
                title: _l.reanalyze,
                subtitle: _l.reanalyzeHint,
                onTap: _hasAudio && !_analyzing
                    ? () => run(analyzeLoadedAudio)
                    : null,
              ),
              SheetAction(
                key: const Key('history-btn'),
                icon: LucideIcons.history,
                title: _l.history,
                subtitle: _l.historyStepsHint(
                  _undoStack.length + _redoStack.length,
                ),
                onTap: _hasAudio ? () => run(_openHistory) : null,
              ),
              SheetAction(
                key: const Key('reset-btn'),
                icon: LucideIcons.rotateCcw,
                title: _l.resetAll,
                subtitle: _l.resetAllHint,
                onTap: _notes.isEmpty ? null : () => run(_resetAll),
              ),
              if (_active != null)
                SheetAction(
                  key: const Key('remove-file-btn'),
                  icon: LucideIcons.trash2,
                  title: _l.removeFile,
                  subtitle: _l.removeFileHint,
                  onTap: () => run(() => _confirmRemove(_active!)),
                ),
            ],
          ),
        );
      },
    );
  }

  Future<void> _confirmRemove(FileSession session) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (dialogContext) => AlertDialog(
        backgroundColor: CantoColors.raised,
        title: Text(_l.removeFileTitle(session.name ?? '')),
        content: Text(_l.removeFileBody),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(dialogContext, false),
            child: Text(_l.cancel),
          ),
          TextButton(
            key: const Key('confirm-remove'),
            onPressed: () => Navigator.pop(dialogContext, true),
            style: TextButton.styleFrom(foregroundColor: CantoColors.heat),
            child: Text(_l.remove),
          ),
        ],
      ),
    );
    if (ok == true && mounted) _removeFile(session);
  }

  // ---------------------------------------------------------------------------
  // Layout

  @override
  Widget build(BuildContext context) {
    final size = MediaQuery.sizeOf(context);
    final insets = MediaQuery.paddingOf(context);
    final width = size.width - insets.horizontal;
    final height = size.height - insets.vertical;
    // Landscape phones and tablets put the inspector and transport in a side
    // panel so the canvas keeps its height; portrait stacks everything.
    _wide = width > height && width >= 560;
    _pinnedSidebar = _wide && width >= 1100;
    _panelWidth = clampVal(width * 0.34, 320, 380);
    _tightPanel = _wide && _panelWidth < 345;
    // Short screens get tighter chrome. In landscape that is every phone;
    // tablets have room for a full-width title bar and the history list.
    _compact = _wide ? height < 560 : height < 720;
    final editorWidth = _wide
        ? width - _panelWidth - (_pinnedSidebar ? _sidebarWidth : 0)
        : width;
    _narrow = editorWidth < 380;
    if (_projectOpening) return _buildProjectLoading();
    // While an export renders on a background isolate the sessions' engines
    // must stay alive, so the editor can't be left until it finishes.
    return PopScope(
      canPop: !_exporting,
      child: CallbackShortcuts(
        // Hardware keyboards (tablets, Chromebooks, DeX).
        bindings: {
          const SingleActivator(LogicalKeyboardKey.keyZ, control: true): _undo,
          const SingleActivator(
            LogicalKeyboardKey.keyZ,
            control: true,
            shift: true,
          ): _redo,
          const SingleActivator(LogicalKeyboardKey.keyY, control: true): _redo,
          const SingleActivator(LogicalKeyboardKey.space): () {
            if (_hasAudio && !_exporting) _togglePlay();
          },
        },
        child: Focus(
          autofocus: true,
          child: Stack(
            children: [
              _wide ? _buildWideScaffold() : _buildScaffold(),
              // A load blocks the whole screen (edits made meanwhile would
              // be overwritten by its result) but can be cancelled.
              if (_load != null) _buildLoadingOverlay(),
            ],
          ),
        ),
      ),
    );
  }

  static const double _sidebarWidth = 272;

  Widget _buildLoadingCard(FileSession session, VoidCallback onCancel) {
    final load = _load;
    return LoadingCard(
      fileName: session.name ?? '',
      stage: load?.stage ?? LoadStage.decoding,
      progress: load?.progress ?? 0,
      hasCache: load?.hasCache ?? false,
      reanalyze: load?.reanalyze ?? false,
      cancelling: load?.cancelled ?? false,
      onCancel: onCancel,
    );
  }

  /// The Loading Project page: shown until the project's first file is
  /// ready. Cancel goes back to the projects list (the load stops).
  Widget _buildProjectLoading() {
    return ProjectLoadingView(
      projectName: _project.name,
      fileCount: _project.files.length,
      card: _buildLoadingCard(
        _load?.session ?? _sessions.first,
        () => Navigator.of(context).maybePop(),
      ),
    );
  }

  /// Any later load (switching to an unopened file, adding or recording one,
  /// re-analysing, preparing a file for export).
  Widget _buildLoadingOverlay() {
    final load = _load!;
    return Positioned.fill(
      child: Material(
        key: const Key('file-loading'),
        color: CantoColors.bg.withValues(alpha: 0.86),
        child: SafeArea(
          child: Center(
            child: SingleChildScrollView(
              padding: const EdgeInsets.all(24),
              child: ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: 380),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(
                      load.reanalyze ? _l.reanalyzingFile : _l.loadingFile,
                      textAlign: TextAlign.center,
                      style: CantoTypography.sans(
                        size: 17,
                        weight: FontWeight.w600,
                        color: CantoColors.inkBright,
                      ),
                    ),
                    const SizedBox(height: 14),
                    _buildLoadingCard(load.session, _cancelLoad),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  /// Landscape / tablet: [sidebar] | canvas column | inspector panel.
  Widget _buildWideScaffold() {
    final topBar = !_compact;
    final panel = Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (!topBar) ...[
          _buildAppBar(),
          Container(height: 1, color: CantoColors.line),
        ],
        Expanded(child: _buildPanelBody()),
        _buildTransport(),
      ],
    );
    final workspace = Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Expanded(
          child: Column(
            children: [
              _buildContextBar(),
              Expanded(child: _buildEditorArea()),
              if (_hasAudio) _buildOverviewBar(),
            ],
          ),
        ),
        Container(width: 1, color: CantoColors.line),
        SizedBox(width: _panelWidth, child: panel),
      ],
    );
    return Scaffold(
      key: _scaffoldKey,
      backgroundColor: CantoColors.surface,
      drawer: _pinnedSidebar ? null : _buildSidebar(),
      body: SafeArea(
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            if (_pinnedSidebar) ...[
              SizedBox(
                width: _sidebarWidth,
                child: _buildSidebarBody(pinned: true),
              ),
              Container(width: 1, color: CantoColors.line),
            ],
            Expanded(
              child: topBar
                  ? Column(
                      children: [
                        _buildAppBar(),
                        Container(height: 1, color: CantoColors.line),
                        Expanded(child: workspace),
                      ],
                    )
                  : workspace,
            ),
          ],
        ),
      ),
    );
  }

  /// Side panel content: the note inspector, and on tablets (room to spare)
  /// the edit history below it.
  Widget _buildPanelBody() {
    if (!_hasAudio) {
      return Padding(
        padding: const EdgeInsets.fromLTRB(16, 14, 16, 14),
        child: Align(
          alignment: Alignment.topLeft,
          child: SizedBox(height: 64, child: _buildNoteHeader(null)),
        ),
      );
    }
    final inspector = _buildInspector();
    if (_compact) return SingleChildScrollView(child: inspector);
    final rows = _historyRows();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        inspector,
        Container(height: 1, color: CantoColors.line),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 12, 8, 4),
          child: Row(
            children: [
              Expanded(
                child: Text(
                  _l.history.toUpperCase(),
                  style: CantoTypography.label(text: _l.history),
                ),
              ),
              Text(
                '${_undoStack.length}',
                style: CantoTypography.num(size: 11, color: CantoColors.faint),
              ),
              const SizedBox(width: 8),
            ],
          ),
        ),
        Expanded(
          child: HistoryList(
            key: const Key('panel-history'),
            labels: rows,
            current: _undoStack.length,
            onJump: _jumpToHistory,
            padding: const EdgeInsets.fromLTRB(8, 0, 8, 8),
          ),
        ),
      ],
    );
  }

  Widget _buildScaffold() {
    return Scaffold(
      key: _scaffoldKey,
      backgroundColor: CantoColors.surface,
      drawer: _buildSidebar(),
      body: SafeArea(
        child: Column(
          children: [
            _buildAppBar(),
            _buildContextBar(),
            Expanded(child: _buildEditorArea()),
            // Overview and inspector only mean something once a clip exists;
            // until then the empty state gets the room.
            if (_hasAudio) ...[_buildOverviewBar(), _buildInspector()],
            _buildTransport(),
          ],
        ),
      ),
    );
  }

  Widget _buildAppBar() {
    return SizedBox(
      height: _compact ? 52 : 56,
      child: Row(
        children: [
          const SizedBox(width: 4),
          if (_pinnedSidebar)
            const SizedBox(width: 8) // the sidebar is already open
          else
            CantoIconButton(
              key: const Key('menu-btn'),
              icon: LucideIcons.menu,
              tooltip: _l.openMenu,
              onTap: () => _scaffoldKey.currentState?.openDrawer(),
            ),
          Expanded(
            child: Pressable(
              key: const Key('session-title'),
              onTap: _active == null ? null : _openSessionSheet,
              child: Padding(
                padding: const EdgeInsets.only(left: 6, right: 6),
                child: Row(
                  children: [
                    Expanded(
                      child: Column(
                        mainAxisAlignment: MainAxisAlignment.center,
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            _project.name,
                            key: const Key('project-title'),
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: CantoTypography.sans(
                              size: 10.5,
                              weight: FontWeight.w600,
                              color: CantoColors.muted,
                              height: 1.1,
                            ),
                          ),
                          const SizedBox(height: 2),
                          Row(
                            children: [
                              Flexible(
                                child: Text(
                                  _fileName ?? _l.noAudioLoaded,
                                  key: const Key('clip-title'),
                                  maxLines: 1,
                                  overflow: TextOverflow.ellipsis,
                                  style: CantoTypography.sans(
                                    size: 14.5,
                                    weight: FontWeight.w600,
                                    color: _hasAudio
                                        ? CantoColors.inkBright
                                        : CantoColors.muted,
                                    height: 1.2,
                                  ),
                                ),
                              ),
                              const Icon(
                                Icons.expand_more_rounded,
                                size: 16,
                                color: CantoColors.faint,
                              ),
                            ],
                          ),
                        ],
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
          CantoIconButton(
            key: const Key('undo-btn'),
            icon: LucideIcons.undo2,
            tooltip: _l.undo,
            onTap: _undoStack.isNotEmpty ? _undo : null,
            onLongPress: _hasAudio ? _openHistory : null,
          ),
          CantoIconButton(
            key: const Key('redo-btn'),
            icon: LucideIcons.redo2,
            tooltip: _l.redo,
            onTap: _redoStack.isNotEmpty ? _redo : null,
            onLongPress: _hasAudio ? _openHistory : null,
          ),
          CantoIconButton(
            key: const Key('record-open'),
            icon: LucideIcons.mic,
            tooltip: _l.record,
            color: CantoColors.heat,
            onTap: _analyzing || _exporting ? null : openRecorder,
          ),
          const SizedBox(width: 6),
        ],
      ),
    );
  }

  // ---- Sidebar -------------------------------------------------------------

  Widget _buildSidebar() {
    return Drawer(
      key: const Key('sidebar'),
      backgroundColor: CantoColors.surface,
      width: 304,
      shape: const RoundedRectangleBorder(),
      child: SafeArea(child: _buildSidebarBody(pinned: false)),
    );
  }

  /// Files, tools and navigation: a drawer on phones, pinned beside the
  /// editor on wide tablets.
  Widget _buildSidebarBody({required bool pinned}) {
    final l = _l;
    void closeThen(VoidCallback action) {
      if (!pinned) Navigator.of(context).pop(); // close the drawer
      action();
    }

    return ColoredBox(
      key: pinned ? const Key('sidebar-pinned') : null,
      color: CantoColors.surface,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 16, 16, 14),
            child: Row(
              children: [
                const CantoLogo(size: 34),
                const SizedBox(width: 12),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        l.appName,
                        style: CantoTypography.sans(
                          size: 11,
                          weight: FontWeight.w600,
                          color: CantoColors.muted,
                        ),
                      ),
                      const SizedBox(height: 2),
                      Text(
                        _project.name,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: CantoTypography.sans(
                          size: 16,
                          weight: FontWeight.w600,
                          color: CantoColors.inkBright,
                        ),
                      ),
                    ],
                  ),
                ),
              ],
            ),
          ),
          Container(height: 1, color: CantoColors.line),
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 14, 16, 6),
            child: Text(
              '${l.files.toUpperCase()} · ${_sessions.length}',
              style: CantoTypography.label(text: l.files),
            ),
          ),
          Expanded(
            child: ListView(
              padding: const EdgeInsets.symmetric(horizontal: 8),
              children: [
                for (final s in _sessions)
                  _SidebarFile(
                    key: Key('sidebar-file-${s.file!.id}'),
                    name: s.name ?? '',
                    status: !s.loaded
                        ? l.notOpenedYet
                        : s.offCount == 0
                        ? l.inKey
                        : l.offCount(s.offCount),
                    statusHot: s.loaded && s.offCount > 0,
                    selected: identical(s, _active),
                    onTap: () => closeThen(() => _activate(s)),
                  ),
                _SidebarItem(
                  key: const Key('sidebar-record'),
                  icon: LucideIcons.mic,
                  label: l.recordTake,
                  onTap: _analyzing || _exporting
                      ? null
                      : () => closeThen(openRecorder),
                ),
                _SidebarItem(
                  key: const Key('sidebar-add-file'),
                  icon: LucideIcons.filePlus,
                  label: l.addAudioFile,
                  onTap: _analyzing ? null : () => closeThen(pickAndLoadAudio),
                ),
              ],
            ),
          ),
          Container(height: 1, color: CantoColors.line),
          Padding(
            padding: const EdgeInsets.all(8),
            child: Column(
              children: [
                _SidebarItem(
                  key: const Key('sidebar-history'),
                  icon: LucideIcons.history,
                  label: l.history,
                  onTap: _hasAudio ? () => closeThen(_openHistory) : null,
                ),
                _SidebarItem(
                  key: const Key('sidebar-advanced'),
                  icon: LucideIcons.slidersHorizontal,
                  label: l.advancedTools,
                  onTap: _hasAudio && _notes.isNotEmpty
                      ? () => closeThen(_openAdvanced)
                      : null,
                ),
                _SidebarItem(
                  key: const Key('sidebar-export'),
                  icon: LucideIcons.fileDown,
                  label: l.export,
                  onTap: _sessions.isEmpty || _exporting
                      ? null
                      : () => closeThen(_openExportSheet),
                ),
                _SidebarItem(
                  key: const Key('sidebar-settings'),
                  icon: LucideIcons.settings,
                  label: l.settings,
                  onTap: () => closeThen(
                    () => Navigator.of(context).push(
                      MaterialPageRoute<void>(
                        builder: (_) => SettingsPage(store: _store),
                      ),
                    ),
                  ),
                ),
                _SidebarItem(
                  key: const Key('sidebar-projects'),
                  icon: LucideIcons.house,
                  label: l.allProjects,
                  onTap: _exporting
                      ? null
                      : () => closeThen(() => Navigator.of(context).maybePop()),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  // ---- Export --------------------------------------------------------------

  void _openExportSheet() {
    showModalBottomSheet<void>(
      context: context,
      builder: (sheetContext) {
        void run(VoidCallback action) {
          Navigator.of(sheetContext).pop();
          action();
        }

        return CantoSheet(
          key: const Key('export-sheet'),
          title: _l.export,
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            mainAxisSize: MainAxisSize.min,
            children: [
              SheetAction(
                key: const Key('export-this'),
                icon: LucideIcons.fileDown,
                title: _l.exportThis,
                subtitle: _l.exportThisHint,
                onTap: _hasAudio && !_exporting
                    ? () => run(() => _exportSessions([_active!]))
                    : null,
              ),
              SheetAction(
                key: const Key('export-all'),
                icon: LucideIcons.files,
                title: _l.exportAll,
                subtitle: _l.exportAllHint(_sessions.length),
                onTap: _sessions.isNotEmpty && !_exporting
                    ? () => run(() => _exportSessions(List.of(_sessions)))
                    : null,
              ),
            ],
          ),
        );
      },
    );
  }

  /// Loads a file that hasn't been opened yet without switching to it.
  Future<void> _ensureLoaded(FileSession session) async {
    if (session.loaded) return;
    await _loadSession(session, activate: false);
  }

  /// Renders each file with its edits to `Exports/<name> (tuned).wav`.
  /// Rendering runs on a background isolate, one file at a time.
  Future<void> exportSessions(List<FileSession> targets) =>
      _exportSessions(targets);

  Future<void> _exportSessions(List<FileSession> targets) async {
    if (_exporting || _load != null || targets.isEmpty) return;
    if (_playing) _pause();
    _saveNow();
    final messenger = ScaffoldMessenger.of(context);
    setState(() => _exporting = true);
    messenger.showSnackBar(
      SnackBar(
        content: Text(_l.exporting),
        duration: const Duration(minutes: 5),
      ),
    );
    String? error;
    String? lastPath;
    var count = 0;
    for (final s in targets) {
      await _ensureLoaded(s);
      if (!mounted) return;
      if (!s.hasAudio) {
        error = s.error ?? _l.loadFailed;
        break;
      }
      final path = _store.exportPathFor(_project, s.file!);
      final err = await s.synth.exportWav(path);
      if (err != null) {
        error = err;
        break;
      }
      count++;
      lastPath = path;
    }
    if (!mounted) return;
    setState(() => _exporting = false);
    messenger
      ..hideCurrentSnackBar()
      ..showSnackBar(
        SnackBar(
          content: Text(
            error != null
                ? _l.exportFailed(error)
                : count == 1
                ? _l.exportDone(lastPath!)
                : _l.exportAllDone(count, _project.exportDir),
          ),
          duration: const Duration(seconds: 6),
        ),
      );
  }

  Widget _buildContextBar() {
    final hasNotes = _notes.any((n) => !n.unpitched);
    final off = _countOff();
    final snapLabels = {
      'off': _l.snapFree,
      'semi': _l.snapSemi,
      'scale': _l.snapScale,
    };
    final Widget snapControl = _narrow
        // Not enough width for three segments: one chip that cycles.
        ? Pressable(
            key: const Key('snap-btn'),
            semanticLabel: _l.snapSemantic(snapLabels[_snap]!),
            onTap: () => setState(() {
              _snap = _snap == 'off'
                  ? 'semi'
                  : _snap == 'semi'
                  ? 'scale'
                  : 'off';
            }),
            child: Container(
              height: 32,
              padding: const EdgeInsets.symmetric(horizontal: 10),
              decoration: BoxDecoration(
                color: CantoColors.well,
                borderRadius: BorderRadius.circular(16),
                border: Border.all(color: CantoColors.line),
              ),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(
                    LucideIcons.magnet,
                    size: 13,
                    color: _snap == 'off'
                        ? CantoColors.faint
                        : CantoColors.accent,
                  ),
                  const SizedBox(width: 5),
                  Text(
                    snapLabels[_snap]!,
                    style: CantoTypography.sans(
                      size: 11.5,
                      weight: FontWeight.w600,
                      color: CantoColors.ink,
                    ),
                  ),
                ],
              ),
            ),
          )
        : Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              const Icon(
                LucideIcons.magnet,
                size: 14,
                color: CantoColors.faint,
              ),
              const SizedBox(width: 6),
              CantoSegmented<String>(
                key: const Key('snap-btn'),
                selected: _snap,
                onChanged: (v) => setState(() => _snap = v),
                options: [
                  SegmentOption(
                    'off',
                    snapLabels['off']!,
                    key: const Key('snap-off'),
                  ),
                  SegmentOption(
                    'semi',
                    snapLabels['semi']!,
                    key: const Key('snap-semi'),
                  ),
                  SegmentOption(
                    'scale',
                    snapLabels['scale']!,
                    key: const Key('snap-scale'),
                  ),
                ],
              ),
            ],
          );
    return Container(
      height: _compact ? 44 : 48,
      decoration: const BoxDecoration(
        color: CantoColors.surface,
        border: Border(bottom: BorderSide(color: CantoColors.line)),
      ),
      padding: const EdgeInsets.symmetric(horizontal: 12),
      child: Row(
        children: [
          Expanded(
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(
                children: [
                  CantoSelectChip(
                    key: const Key('key-btn'),
                    icon: LucideIcons.music2,
                    label: _l.keyChip(_key, _modeLabel(_l, _mode)),
                    onTap: _openKeySheet,
                  ),
                  const SizedBox(width: 10),
                  snapControl,
                ],
              ),
            ),
          ),
          if (hasNotes) ...[
            CantoIconButton(
              key: const Key('advanced-btn'),
              icon: LucideIcons.slidersHorizontal,
              tooltip: _l.advancedTools,
              size: 40,
              iconSize: 18,
              onTap: _openAdvanced,
            ),
            const SizedBox(width: 4),
            off == 0
                ? Row(
                    key: const Key('in-key-status'),
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      const Icon(
                        LucideIcons.circleCheck,
                        size: 14,
                        color: CantoColors.success,
                      ),
                      const SizedBox(width: 5),
                      Text(
                        _l.inKey,
                        style: CantoTypography.sans(
                          size: 12,
                          weight: FontWeight.w600,
                          color: CantoColors.ink,
                        ),
                      ),
                    ],
                  )
                : Pressable(
                    key: const Key('next-off-btn'),
                    onTap: _jumpToNextOff,
                    semanticLabel: _l.nextOffSemantic,
                    child: Container(
                      height: 32,
                      padding: const EdgeInsets.only(left: 10, right: 6),
                      decoration: BoxDecoration(
                        color: CantoColors.heatSoft,
                        borderRadius: BorderRadius.circular(16),
                        border: Border.all(
                          color: CantoColors.heat.withValues(alpha: 0.45),
                        ),
                      ),
                      child: Row(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          Text(
                            _l.offCount(off),
                            style: CantoTypography.num(
                              size: 12,
                              weight: FontWeight.w600,
                              color: CantoColors.heat,
                            ),
                          ),
                          const Icon(
                            Icons.chevron_right_rounded,
                            size: 18,
                            color: CantoColors.heat,
                          ),
                        ],
                      ),
                    ),
                  ),
          ],
        ],
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

        final showEmpty = !_hasAudio || _notes.isEmpty || _errorMessage != null;
        return Container(
          color: CantoColors.well,
          child: Stack(
            clipBehavior: Clip.hardEdge,
            children: [
              Listener(
                key: _editorKey,
                behavior: HitTestBehavior.opaque,
                onPointerDown: _onEditorPointerDown,
                onPointerMove: _onEditorPointerMove,
                onPointerUp: _onEditorPointerUp,
                onPointerCancel: _onEditorPointerUp,
                // Notes and playhead sit on separate layers: playback ticks
                // repaint only the playhead layer.
                child: Stack(
                  fit: StackFit.expand,
                  children: [
                    RepaintBoundary(
                      child: CustomPaint(
                        painter: EditorPainter(
                          notes: _notes,
                          keyName: _key,
                          mode: _mode,
                          bpm: _bpm,
                          duration: _duration,
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
                          cutNoteId: _cutNoteId,
                          cutTime: _cutTime,
                          bypassLabel: _l.bypassTag,
                        ),
                      ),
                    ),
                    RepaintBoundary(
                      child: CustomPaint(
                        painter: PlayheadPainter(
                          playhead: _playheadVN,
                          viewStart: _viewStart,
                          viewDuration: _viewDuration,
                          pitchLo: _pitchLo,
                          pitchHi: _pitchHi,
                        ),
                      ),
                    ),
                  ],
                ),
              ),
              // Floating note toolbar. It sits above the lyric lane, or at
              // the top when the selected note is low, so it never covers
              // the note being edited.
              if (_buildNoteActions() case final bar?)
                Positioned(
                  left: 8,
                  right: 8,
                  top: _actionsOnTop() ? 30 : null,
                  bottom: _actionsOnTop() ? null : 30,
                  child: Center(child: bar),
                ),
              if (showEmpty) _buildEmptyOverlay(),
              Positioned(
                top: 4,
                left: 0,
                right: 0,
                child: IgnorePointer(
                  child: ValueListenableBuilder<String?>(
                    valueListenable: _toastVN,
                    builder: (context, text, _) => AnimatedOpacity(
                      duration: const Duration(milliseconds: 160),
                      opacity: text == null ? 0 : 1,
                      child: Center(
                        child: text == null
                            ? const SizedBox.shrink()
                            : _HistoryToast(text: text),
                      ),
                    ),
                  ),
                ),
              ),
              if (dragNote != null && floaterOffset != null)
                Positioned(
                  left: clampVal(floaterOffset.dx - 50, 6, _editorWidth - 120),
                  top: floaterOffset.dy < 64
                      ? floaterOffset.dy + 18
                      : floaterOffset.dy - 44,
                  child: IgnorePointer(
                    child: Container(
                      padding: const EdgeInsets.symmetric(
                        horizontal: 10,
                        vertical: 5,
                      ),
                      decoration: BoxDecoration(
                        color: CantoColors.inkBright,
                        borderRadius: BorderRadius.circular(14),
                        boxShadow: const [
                          BoxShadow(
                            color: Color.fromRGBO(0, 0, 0, 0.4),
                            blurRadius: 18,
                            offset: Offset(0, 6),
                          ),
                        ],
                      ),
                      child: Text(
                        _floaterLabel(dragNote),
                        style: CantoTypography.num(
                          size: 12,
                          weight: FontWeight.w600,
                          color: CantoColors.accentInk,
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
    final isError = _errorMessage != null;
    final noFiles = _active == null;
    // A file whose load was cancelled: offer to open it again.
    final notOpened = !isError && _active != null && !_active!.loaded;
    final title = isError
        ? _l.emptyErrorTitle
        : notOpened
        ? _l.notOpenedYet
        : _hasAudio
        ? _l.emptyNoNotesTitle
        : noFiles
        ? _l.emptyProjectTitle
        : _l.emptyTitle;
    final body =
        _errorMessage ??
        (notOpened
            ? _l.notOpenedBody
            : _hasAudio
            ? _l.emptyNoNotesBody
            : noFiles
            ? _l.emptyProjectBody
            : _l.emptyBody);
    return Positioned.fill(
      child: Container(
        color: CantoColors.well.withValues(alpha: 0.78),
        alignment: Alignment.center,
        child: SingleChildScrollView(
          padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 12),
          child: Container(
            constraints: const BoxConstraints(maxWidth: 360),
            padding: const EdgeInsets.fromLTRB(20, 22, 20, 20),
            decoration: BoxDecoration(
              color: CantoColors.surface,
              borderRadius: BorderRadius.circular(18),
              border: Border.all(color: CantoColors.lineStrong),
            ),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                Container(
                  width: 52,
                  height: 52,
                  decoration: BoxDecoration(
                    color: isError ? CantoColors.heatSoft : CantoColors.raised,
                    borderRadius: BorderRadius.circular(16),
                  ),
                  child: Icon(
                    isError ? LucideIcons.circleAlert : LucideIcons.micVocal,
                    size: 24,
                    color: isError ? CantoColors.heat : CantoColors.ink,
                  ),
                ),
                const SizedBox(height: 14),
                Text(
                  title,
                  textAlign: TextAlign.center,
                  style: CantoTypography.sans(
                    size: 18,
                    weight: FontWeight.w600,
                    color: CantoColors.inkBright,
                  ),
                ),
                const SizedBox(height: 8),
                Text(
                  body,
                  textAlign: TextAlign.center,
                  style: CantoTypography.sans(
                    size: 12.5,
                    color: CantoColors.muted,
                    height: 1.45,
                  ),
                ),
                const SizedBox(height: 18),
                if (notOpened) ...[
                  CantoButton(
                    key: const Key('open-file-btn'),
                    style: CantoButtonStyle.primary,
                    icon: LucideIcons.folderOpen,
                    label: _l.openFile,
                    height: 48,
                    expand: true,
                    onTap: _analyzing ? null : () => _activate(_active!),
                  ),
                  const SizedBox(height: 8),
                ],
                // Recording is the quickest way in; importing comes second.
                CantoButton(
                  key: const Key('empty-record-btn'),
                  style: CantoButtonStyle.record,
                  icon: LucideIcons.mic,
                  label: _l.recordTake,
                  height: 48,
                  expand: true,
                  onTap: _analyzing ? null : openRecorder,
                ),
                const SizedBox(height: 8),
                CantoButton(
                  key: const Key('empty-load-btn'),
                  style: CantoButtonStyle.outline,
                  icon: LucideIcons.filePlus,
                  label: _analyzing ? _l.analyzing : _l.addAudioFile,
                  height: 44,
                  expand: true,
                  onTap: _analyzing ? null : pickAndLoadAudio,
                ),
                if (_hasAudio) ...[
                  const SizedBox(height: 8),
                  CantoButton(
                    key: const Key('empty-analyze-btn'),
                    style: CantoButtonStyle.outline,
                    icon: LucideIcons.scanSearch,
                    label: _l.reanalyze,
                    height: 44,
                    expand: true,
                    onTap: _analyzing ? null : analyzeLoadedAudio,
                  ),
                ],
                const SizedBox(height: 14),
                Text(
                  'WAV · MP3 · FLAC · OGG · M4A',
                  style: CantoTypography.sans(
                    size: 10,
                    color: CantoColors.faint,
                    letterSpacing: 0.6,
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }

  Widget _buildOverviewBar() {
    return Container(
      height: _compact ? 40 : 44,
      padding: const EdgeInsets.fromLTRB(12, 6, 4, 6),
      decoration: const BoxDecoration(
        color: CantoColors.surface,
        border: Border(top: BorderSide(color: CantoColors.line)),
      ),
      child: Row(
        children: [
          Expanded(
            child: ClipRRect(
              borderRadius: BorderRadius.circular(CantoRadii.chip),
              child: LayoutBuilder(
                builder: (context, constraints) {
                  final w = math.max(1.0, constraints.maxWidth);
                  return Listener(
                    behavior: HitTestBehavior.opaque,
                    onPointerDown: (e) => _onOverviewDown(e, w),
                    onPointerMove: (e) => _onOverviewMove(e, w),
                    onPointerUp: (e) => _onOverviewUp(e, w),
                    onPointerCancel: (e) => _onOverviewUp(e, w),
                    child: Stack(
                      fit: StackFit.expand,
                      children: [
                        RepaintBoundary(
                          child: CustomPaint(
                            painter: OverviewPainter(
                              notes: _notes,
                              waveformPeaks: _waveformPeaks,
                              keyName: _key,
                              mode: _mode,
                              bpm: _bpm,
                              duration: _duration,
                              bypass: _bypass,
                              selectedId: _selectedId,
                              viewStart: _viewStart,
                              viewDuration: _viewDuration,
                              pitchLo: _pitchLo,
                              pitchHi: _pitchHi,
                            ),
                          ),
                        ),
                        RepaintBoundary(
                          child: CustomPaint(
                            painter: OverviewPlayheadPainter(
                              playhead: _playheadVN,
                              duration: _duration,
                            ),
                          ),
                        ),
                      ],
                    ),
                  );
                },
              ),
            ),
          ),
          CantoIconButton(
            key: const Key('zoom-out'),
            icon: LucideIcons.zoomOut,
            size: 40,
            iconSize: 17,
            tooltip: _l.zoomOut,
            color: CantoColors.muted,
            onTap: () => _zoomAt(1.25, _viewStart + _viewDuration / 2.0),
          ),
          CantoIconButton(
            key: const Key('zoom-in'),
            icon: LucideIcons.zoomIn,
            size: 40,
            iconSize: 17,
            tooltip: _l.zoomIn,
            color: CantoColors.muted,
            onTap: () => _zoomAt(0.8, _viewStart + _viewDuration / 2.0),
          ),
        ],
      ),
    );
  }

  Widget _buildInspector() {
    final note = _selectedNote;
    final model = _paramModel(note);
    final pitched = note != null && !note.unpitched;

    String chipValue(String id) {
      if (!pitched) return '—';
      switch (id) {
        case 'drift':
          return '${(note.modulation * 100).round()}%';
        case 'time':
          return formatBars(note.start, _bpm);
        case 'formant':
          return '${formatSigned(note.formant, 1)} st';
        case 'level':
          return formatDb(note.gainDb);
        default:
          return formatCents(centsFromTarget(note.pitch, _key, _mode).cents);
      }
    }

    return Container(
      padding: EdgeInsets.fromLTRB(12, _compact ? 8 : 12, 12, _compact ? 2 : 6),
      decoration: const BoxDecoration(
        color: CantoColors.surface,
        border: Border(top: BorderSide(color: CantoColors.line)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          SizedBox(
            // The side panel is narrower than a phone, so hints may wrap.
            height: _wide ? 64 : (_compact ? 52 : 58),
            child: _buildNoteHeader(note),
          ),
          SizedBox(height: _compact ? 6 : 10),
          Row(
            children: [
              for (final (i, t) in _toolDefs.indexed) ...[
                if (i > 0) const SizedBox(width: 6),
                Expanded(
                  child: ToolChip(
                    key: Key('tool-${t.id}'),
                    icon: t.icon,
                    label: _toolLabel(_l, t.id),
                    semanticLabel: _l.toolSemantic(_toolLabel(_l, t.id)),
                    value: chipValue(t.id),
                    valueMuted: !pitched,
                    height: _compact ? 48 : 54,
                    active: _tool == t.id,
                    onTap: () => _setTool(t.id),
                  ),
                ),
              ],
            ],
          ),
          const SizedBox(height: 6),
          _buildSliderRow(
            label: model.name,
            slider: CantoSlider(
              key: const Key('param-slider'),
              value: model.value,
              min: model.min,
              max: model.max,
              bipolar: model.bipolar,
              disabled: model.disabled || _bypass,
              onChangeStart: () {
                final n = _selectedNote;
                if (n == null || n.unpitched || _bypass) return;
                _pushUndo('tool:$_tool');
                if (_tool == 'pitch') {
                  _sliderTarget = centsFromTarget(n.pitch, _key, _mode).target;
                }
              },
              onChanged: _applyParam,
              onChangeEnd: () => setState(() => _commitUndo()),
            ),
            value: model.text,
          ),
          _buildSliderRow(
            label: _l.correctLabel,
            slider: CantoSlider(
              key: const Key('correct-slider'),
              value: (_correct * 100).roundToDouble(),
              min: 0,
              max: 100,
              bipolar: false,
              disabled: _notes.isEmpty || _bypass,
              onChangeStart: () => _pushUndo('correct'),
              onChanged: (val) {
                setState(() {
                  _correct = val / 100.0;
                  _reapplyCorrect();
                });
              },
              onChangeEnd: () => setState(() => _commitUndo()),
            ),
            value: '${(_correct * 100).round()}%',
            trailing: Padding(
              padding: const EdgeInsets.only(left: 8),
              child: CantoButton(
                key: const Key('tune-all-btn'),
                label: _l.tuneAll,
                icon: LucideIcons.wandSparkles,
                height: 32,
                style: CantoButtonStyle.outline,
                onTap: (_notes.isEmpty || _bypass || _correct >= 1.0)
                    ? null
                    : _tuneAll,
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildSliderRow({
    required String label,
    required Widget slider,
    required String value,
    Widget? trailing,
  }) {
    return SizedBox(
      height: _compact ? 36 : 40,
      child: Row(
        children: [
          SizedBox(
            width: _tightPanel ? 56 : 64,
            // One line always; long labels shrink instead of wrapping.
            child: FittedBox(
              fit: BoxFit.scaleDown,
              alignment: Alignment.centerLeft,
              child: Text(
                label.toUpperCase(),
                maxLines: 1,
                style: CantoTypography.label(text: label),
              ),
            ),
          ),
          Expanded(child: slider),
          SizedBox(
            width: _tightPanel ? 48 : 58,
            child: Text(
              value,
              textAlign: TextAlign.right,
              style: CantoTypography.num(
                size: 12,
                weight: FontWeight.w600,
                color: CantoColors.ink,
              ),
            ),
          ),
          ?trailing,
        ],
      ),
    );
  }

  Widget _buildNoteHeader(VocalNote? note) {
    if (note == null || note.unpitched) {
      final pitchedCount = _notes.where((n) => !n.unpitched).length;
      final off = _countOff();
      final title = note != null
          ? _l.inspBreath
          : !_hasAudio
          ? _l.noAudioLoaded
          : pitchedCount == 0
          ? _l.inspNoNotes
          : off == 0
          ? _l.inspAllInKey
          : off == 1
          ? _l.inspOffScale(off)
          : _l.inspOffScale(off);
      final sub = note != null
          ? _l.inspUnpitchedHint
          : !_hasAudio
          ? _l.inspOpenHint
          : _l.inspTapHint;
      return Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Text(
                  title,
                  key: const Key('inspector-title'),
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: CantoTypography.sans(
                    size: 15,
                    weight: FontWeight.w600,
                    color: CantoColors.inkBright,
                  ),
                ),
                const SizedBox(height: 4),
                Text(
                  sub,
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                  style: CantoTypography.sans(
                    size: 12,
                    color: CantoColors.muted,
                  ),
                ),
              ],
            ),
          ),
        ],
      );
    }

    final shown = presentNote(note, false);
    final reading = centsFromTarget(shown.pitch, _key, _mode);
    final heard = midiToHz(shown.pitch);
    final isOff = reading.cents.abs() >= 15;
    final was = centsFromTarget(note.detected, _key, _mode);
    final meta = <String>[
      if (note.lyric.isNotEmpty) note.lyric,
      _l.metaBar(formatBars(note.start, _bpm)),
      if ((note.pitch - note.detected).abs() > 0.03)
        _l.metaWas(formatCents(was.cents)),
      if (note.pinned) _l.metaEdited,
      if (note.ornStyle != kOrnNone) ornamentName(_l, note.ornStyle),
      if (note.accent > 0) _l.metaAccent((note.accent * 100).round()),
      if (note.muted) _l.muted,
    ].join(' · ');
    final canTune = !_bypass && reading.cents != 0;

    return Row(
      crossAxisAlignment: CrossAxisAlignment.center,
      children: [
        SizedBox(
          width: 104,
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              Row(
                crossAxisAlignment: CrossAxisAlignment.baseline,
                textBaseline: TextBaseline.alphabetic,
                children: [
                  Text(
                    spellMidi(reading.target, _key),
                    key: const Key('note-name'),
                    style: CantoTypography.num(
                      size: 26,
                      weight: FontWeight.w600,
                      color: CantoColors.inkBright,
                      letterSpacing: -0.8,
                      height: 1.0,
                    ),
                  ),
                  const SizedBox(width: 6),
                  Flexible(
                    child: Text(
                      formatCents(reading.cents),
                      key: const Key('note-cents'),
                      style: CantoTypography.num(
                        size: 13,
                        weight: FontWeight.w600,
                        color: isOff ? CantoColors.heat : CantoColors.accent,
                        height: 1.0,
                      ),
                    ),
                  ),
                ],
              ),
              const SizedBox(height: 4),
              Text(
                '${heard >= 1000 ? heard.toStringAsFixed(0) : heard.toStringAsFixed(1)} Hz',
                key: const Key('note-hz'),
                style: CantoTypography.num(
                  size: 11,
                  color: CantoColors.muted,
                  height: 1.0,
                ),
              ),
            ],
          ),
        ),
        Expanded(
          child: Column(
            mainAxisAlignment: MainAxisAlignment.center,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              CentsGauge(cents: reading.cents),
              const SizedBox(height: 4),
              Text(
                meta,
                key: const Key('note-meta'),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                textAlign: TextAlign.center,
                style: CantoTypography.sans(size: 11, color: CantoColors.muted),
              ),
            ],
          ),
        ),
        const SizedBox(width: 8),
        Column(
          mainAxisAlignment: MainAxisAlignment.center,
          crossAxisAlignment: CrossAxisAlignment.end,
          children: [
            CantoButton(
              key: const Key('tune-btn'),
              label: _l.snap,
              icon: LucideIcons.sparkles,
              height: _compact ? 30 : 32,
              style: CantoButtonStyle.tonal,
              onTap: canTune ? () => _tuneNote(note) : null,
            ),
            if (note.pinned)
              Pressable(
                key: const Key('revert-btn'),
                onTap: _revertNote,
                child: Padding(
                  padding: const EdgeInsets.fromLTRB(6, 4, 6, 2),
                  child: Row(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      const Icon(
                        LucideIcons.rotateCcw,
                        size: 11,
                        color: CantoColors.muted,
                      ),
                      const SizedBox(width: 4),
                      Text(
                        _l.revert,
                        style: CantoTypography.sans(
                          size: 11.5,
                          weight: FontWeight.w600,
                          color: CantoColors.muted,
                          height: 1.1,
                        ),
                      ),
                    ],
                  ),
                ),
              ),
          ],
        ),
      ],
    );
  }

  Widget _buildTransport() {
    return Container(
      height: _compact ? 60 : 68,
      padding: const EdgeInsets.symmetric(horizontal: 12),
      decoration: const BoxDecoration(
        color: CantoColors.surface,
        border: Border(top: BorderSide(color: CantoColors.line)),
      ),
      child: Row(
        children: [
          Expanded(
            child: FittedBox(
              fit: BoxFit.scaleDown,
              alignment: Alignment.centerLeft,
              child: Row(
                children: [
                  ValueListenableBuilder<double>(
                    valueListenable: _playheadVN,
                    builder: (context, playhead, _) => Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      mainAxisAlignment: MainAxisAlignment.center,
                      children: [
                        Text(
                          formatClock(playhead),
                          key: const Key('clock-text'),
                          style: CantoTypography.num(
                            size: 19,
                            weight: FontWeight.w600,
                            color: CantoColors.inkBright,
                            letterSpacing: -0.5,
                            height: 1.0,
                          ),
                        ),
                        const SizedBox(height: 5),
                        Text(
                          '${formatBars(playhead, _bpm)}   ${_bpm.round()} BPM',
                          key: const Key('bars-text'),
                          style: CantoTypography.num(
                            size: 10.5,
                            color: CantoColors.faint,
                            height: 1.0,
                          ),
                        ),
                      ],
                    ),
                  ),
                  const SizedBox(width: 10),
                  ValueListenableBuilder<double>(
                    valueListenable: _levelVN,
                    builder: (context, level, _) => LevelMeter(level: level),
                  ),
                ],
              ),
            ),
          ),
          CantoIconButton(
            key: const Key('rewind-btn'),
            icon: LucideIcons.skipBack,
            tooltip: _l.backToStart,
            onTap: _rewind,
          ),
          const SizedBox(width: 4),
          Pressable(
            key: const Key('play-btn'),
            onTap: _togglePlay,
            semanticLabel: _playing ? _l.pause : _l.play,
            child: AnimatedContainer(
              duration: const Duration(milliseconds: 160),
              width: _compact ? 46 : 52,
              height: _compact ? 46 : 52,
              decoration: BoxDecoration(
                shape: BoxShape.circle,
                color: _playing ? CantoColors.high : CantoColors.accent,
                border: Border.all(
                  color: _playing ? CantoColors.accent : Colors.transparent,
                  width: 1.5,
                ),
              ),
              child: Icon(
                _playing ? LucideIcons.pause : LucideIcons.play,
                size: 22,
                color: _playing ? CantoColors.accent : CantoColors.accentInk,
              ),
            ),
          ),
          const SizedBox(width: 4),
          CantoIconButton(
            key: const Key('loop-btn'),
            icon: LucideIcons.repeat,
            tooltip: _l.loop,
            active: _loop,
            onTap: _toggleLoop,
          ),
          Expanded(
            child: LayoutBuilder(
              builder: (context, constraints) {
                final label = _bypass ? _l.bypassOn : _l.bypassOff;
                final style = CantoTypography.sans(
                  size: 12,
                  weight: FontWeight.w600,
                  color: _bypass ? CantoColors.bypass : CantoColors.muted,
                );
                // Keeps the label only where it fits (it doesn't beside the
                // centred play button on narrow phones or the side panel);
                // the semantic label still names the button.
                final labelWidth = (TextPainter(
                  text: TextSpan(text: label, style: style),
                  textDirection: TextDirection.ltr,
                  textScaler: MediaQuery.textScalerOf(context),
                )..layout()).width;
                final showLabel =
                    10 + 14 + 5 + labelWidth + 10 + 2 <= constraints.maxWidth;
                return Align(
                  alignment: Alignment.centerRight,
                  child: Pressable(
                    key: const Key('bypass-btn'),
                    onTap: _toggleBypass,
                    semanticLabel: _l.bypassSemantic,
                    child: AnimatedContainer(
                      duration: const Duration(milliseconds: 140),
                      height: 36,
                      padding: const EdgeInsets.symmetric(horizontal: 10),
                      decoration: BoxDecoration(
                        color: _bypass
                            ? CantoColors.bypassSoft
                            : CantoColors.raised,
                        borderRadius: BorderRadius.circular(18),
                        border: Border.all(
                          color: _bypass
                              ? CantoColors.bypassLine
                              : CantoColors.lineStrong,
                        ),
                      ),
                      child: Row(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          Icon(
                            _bypass ? LucideIcons.earOff : LucideIcons.ear,
                            size: 14,
                            color: _bypass
                                ? CantoColors.bypass
                                : CantoColors.muted,
                          ),
                          if (showLabel) ...[
                            const SizedBox(width: 5),
                            Text(label, style: style),
                          ],
                        ],
                      ),
                    ),
                  ),
                );
              },
            ),
          ),
        ],
      ),
    );
  }
}

/// A background file load and what the UI shows of it.
class _FileLoad {
  final FileSession session;
  final NativeLoadJob job;
  final bool reanalyze;

  /// Becomes the edited file when done (false: prepared for export).
  final bool activate;

  /// A cached analysis was found for it.
  final bool hasCache;

  /// The file open before; reopened if this load is cancelled.
  final FileSession? previous;
  final Completer<bool> done = Completer<bool>();
  Timer? timer;
  LoadStage stage = LoadStage.decoding;
  double progress = 0;
  bool cancelled = false;

  _FileLoad({
    required this.session,
    required this.job,
    required this.reanalyze,
    required this.activate,
    required this.hasCache,
    required this.previous,
  });
}

/// Brief "Undo · Cut" confirmation over the editor after undo/redo.
class _HistoryToast extends StatelessWidget {
  final String text;

  const _HistoryToast({required this.text});

  @override
  Widget build(BuildContext context) {
    return Container(
      key: const Key('history-toast'),
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 5),
      decoration: BoxDecoration(
        color: CantoColors.raised,
        borderRadius: BorderRadius.circular(14),
        border: Border.all(color: CantoColors.lineStrong),
        boxShadow: const [
          BoxShadow(
            color: Color.fromRGBO(0, 0, 0, 0.35),
            blurRadius: 14,
            offset: Offset(0, 4),
          ),
        ],
      ),
      child: Text(
        text,
        style: CantoTypography.sans(
          size: 12,
          weight: FontWeight.w600,
          color: CantoColors.ink,
        ),
      ),
    );
  }
}

/// One project file in the sidebar. The active file uses the DESIGN.md
/// selection treatment: selected fill plus a leading accent marker.
class _SidebarFile extends StatelessWidget {
  final String name;
  final String status;
  final bool statusHot;
  final bool selected;
  final VoidCallback onTap;

  const _SidebarFile({
    super.key,
    required this.name,
    required this.status,
    required this.statusHot,
    required this.selected,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    return Pressable(
      onTap: onTap,
      child: Container(
        margin: const EdgeInsets.symmetric(vertical: 2),
        padding: const EdgeInsets.fromLTRB(0, 10, 10, 10),
        decoration: BoxDecoration(
          color: selected ? CantoColors.selected : Colors.transparent,
          borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
        ),
        child: Row(
          children: [
            Container(
              width: 3,
              height: 28,
              margin: const EdgeInsets.only(right: 10),
              decoration: BoxDecoration(
                color: selected ? CantoColors.accent : Colors.transparent,
                borderRadius: BorderRadius.circular(2),
              ),
            ),
            Icon(
              LucideIcons.audioWaveform,
              size: 18,
              color: selected ? CantoColors.ink : CantoColors.muted,
            ),
            const SizedBox(width: 10),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    name,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: CantoTypography.sans(
                      size: 13.5,
                      weight: FontWeight.w600,
                      color: CantoColors.inkBright,
                    ),
                  ),
                  const SizedBox(height: 2),
                  Text(
                    status,
                    style: CantoTypography.sans(
                      size: 11,
                      color: statusHot ? CantoColors.heat : CantoColors.muted,
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _SidebarItem extends StatelessWidget {
  final IconData icon;
  final String label;
  final VoidCallback? onTap;

  const _SidebarItem({
    super.key,
    required this.icon,
    required this.label,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    final enabled = onTap != null;
    return Pressable(
      onTap: onTap,
      child: Opacity(
        opacity: enabled ? 1 : 0.4,
        child: Padding(
          padding: const EdgeInsets.fromLTRB(13, 12, 10, 12),
          child: Row(
            children: [
              Icon(icon, size: 18, color: CantoColors.muted),
              const SizedBox(width: 12),
              Expanded(
                child: Text(
                  label,
                  style: CantoTypography.sans(
                    size: 13.5,
                    weight: FontWeight.w600,
                    color: CantoColors.ink,
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
