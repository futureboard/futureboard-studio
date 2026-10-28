import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:io' show File, Platform;
import 'dart:isolate';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'music.dart';

class CantoAnalysisResult {
  final bool ok;
  final String? error;
  final int sampleRate;
  final double durationSec;
  final double bpm;
  final String key;
  final String mode;
  final List<VocalNote> notes;
  final List<double> waveformPeaks;

  /// The engine's JSON for this result (what the analysis cache stores).
  final String? json;

  /// Adopted from the analysis cache instead of analysed now.
  final bool fromCache;

  const CantoAnalysisResult({
    required this.ok,
    this.error,
    required this.sampleRate,
    required this.durationSec,
    required this.bpm,
    required this.key,
    required this.mode,
    required this.notes,
    required this.waveformPeaks,
    this.json,
    this.fromCache = false,
  });

  factory CantoAnalysisResult.fromJsonString(
    String jsonStr, {
    bool fromCache = false,
  }) {
    try {
      final map = jsonDecode(jsonStr) as Map<String, dynamic>;
      final ok = (map['ok'] as bool?) ?? false;
      final error = map['error'] as String?;
      final sampleRate = ((map['sample_rate'] as num?) ?? 48000).toInt();
      final durationSec = ((map['duration_sec'] as num?) ?? 0.0).toDouble();
      final bpm = ((map['bpm'] as num?) ?? 120.0).toDouble();
      final key = (map['key'] as String?) ?? 'A';
      final mode = (map['mode'] as String?) ?? 'minor';
      final notesRaw = (map['notes'] as List<dynamic>?) ?? const [];
      final peaksRaw = (map['waveform_peaks'] as List<dynamic>?) ?? const [];
      return CantoAnalysisResult(
        ok: ok,
        error: error,
        sampleRate: sampleRate,
        durationSec: durationSec,
        bpm: bpm,
        key: key,
        mode: mode,
        notes: notesRaw
            .map((e) => VocalNote.fromJson(e as Map<String, dynamic>))
            .toList(),
        waveformPeaks: peaksRaw.map((e) => (e as num).toDouble()).toList(),
        json: ok ? jsonStr : null,
        fromCache: ok && fromCache,
      );
    } catch (e) {
      return CantoAnalysisResult(
        ok: false,
        error: 'Failed to parse analysis result: $e',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: const [],
        waveformPeaks: const [],
      );
    }
  }
}

typedef _EngineNewNative = ffi.Pointer<ffi.Void> Function(ffi.Float sampleRate);
typedef _EngineNewDart = ffi.Pointer<ffi.Void> Function(double sampleRate);

typedef _EngineFreeNative = ffi.Void Function(ffi.Pointer<ffi.Void> ptr);
typedef _EngineFreeDart = void Function(ffi.Pointer<ffi.Void> ptr);

typedef _FreeStringNative = ffi.Void Function(ffi.Pointer<Utf8> ptr);
typedef _FreeStringDart = void Function(ffi.Pointer<Utf8> ptr);

typedef _LoadAudioFileNative = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<Utf8> pathPtr,
);
typedef _LoadAudioFileDart = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<Utf8> pathPtr,
);

typedef _LoadAudioBytesNative = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Uint8> bytesPtr,
  ffi.Size len,
  ffi.Pointer<Utf8> extPtr,
);
typedef _LoadAudioBytesDart = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Uint8> bytesPtr,
  int len,
  ffi.Pointer<Utf8> extPtr,
);

typedef _LoadAudioFileCachedNative = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<Utf8> pathPtr,
  ffi.Pointer<Utf8> analysisPtr,
);
typedef _LoadAudioFileCachedDart = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<Utf8> pathPtr,
  ffi.Pointer<Utf8> analysisPtr,
);

typedef _AnalyzeNative = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void> ptr);
typedef _AnalyzeDart = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void> ptr);

typedef _SetTransportNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Bool playing,
  ffi.Bool loopEnabled,
  ffi.Bool bypass,
);
typedef _SetTransportDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  bool playing,
  bool loopEnabled,
  bool bypass,
);

typedef _IsPlayingNative = ffi.Bool Function(ffi.Pointer<ffi.Void> ptr);
typedef _IsPlayingDart = bool Function(ffi.Pointer<ffi.Void> ptr);

typedef _SeekNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Double timeSec,
);
typedef _SeekDart = void Function(ffi.Pointer<ffi.Void> ptr, double timeSec);

typedef _PlayheadNative = ffi.Double Function(ffi.Pointer<ffi.Void> ptr);
typedef _PlayheadDart = double Function(ffi.Pointer<ffi.Void> ptr);

typedef _DurationNative = ffi.Double Function(ffi.Pointer<ffi.Void> ptr);
typedef _DurationDart = double Function(ffi.Pointer<ffi.Void> ptr);

typedef _LevelNative = ffi.Float Function(ffi.Pointer<ffi.Void> ptr);
typedef _LevelDart = double Function(ffi.Pointer<ffi.Void> ptr);

typedef _SetKeyModeNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Uint8 key,
  ffi.Uint8 modeId,
);
typedef _SetKeyModeDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  int key,
  int modeId,
);

typedef _SetCorrectNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Float amount,
);
typedef _SetCorrectDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  double amount,
);

typedef _UpdateNoteNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Uint32 index,
  ffi.Float pitch,
  ffi.Float start,
  ffi.Float dur,
  ffi.Float modulation,
  ffi.Float formant,
  ffi.Float gainDb,
  ffi.Bool pinned,
  ffi.Bool muted,
  ffi.Uint32 ornStyle,
  ffi.Float ornDepth,
  ffi.Float ornSpeed,
  ffi.Float accent,
);
typedef _UpdateNoteDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  int index,
  double pitch,
  double start,
  double dur,
  double modulation,
  double formant,
  double gainDb,
  bool pinned,
  bool muted,
  int ornStyle,
  double ornDepth,
  double ornSpeed,
  double accent,
);

typedef _AuditionNoteNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Int32 index,
);
typedef _AuditionNoteDart = void Function(ffi.Pointer<ffi.Void> ptr, int index);

typedef _SetDirectVoiceNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Float hz,
  ffi.Float cutoff,
  ffi.Float gain,
);
typedef _SetDirectVoiceDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  double hz,
  double cutoff,
  double gain,
);

typedef _RenderF32Native = ffi.Float Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Float> outPtr,
  ffi.Size frames,
);
typedef _RenderF32Dart = double Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Float> outPtr,
  int frames,
);

typedef _RenderI16Native = ffi.Float Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Int16> outPtr,
  ffi.Size frames,
);
typedef _RenderI16Dart = double Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Int16> outPtr,
  int frames,
);

typedef _SyncNotesNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Float> data,
  ffi.Uint32 count,
);
typedef _SyncNotesDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Pointer<ffi.Float> data,
  int count,
);

typedef _OutputStartNative = ffi.Bool Function(ffi.Pointer<ffi.Void> ptr);
typedef _OutputStartDart = bool Function(ffi.Pointer<ffi.Void> ptr);

typedef _OutputPauseNative = ffi.Void Function(ffi.Pointer<ffi.Void> ptr);
typedef _OutputPauseDart = void Function(ffi.Pointer<ffi.Void> ptr);

typedef _AuditionReleaseNative = ffi.Void Function(ffi.Pointer<ffi.Void> ptr);
typedef _AuditionReleaseDart = void Function(ffi.Pointer<ffi.Void> ptr);

typedef _IsAuditioningNative = ffi.Bool Function(ffi.Pointer<ffi.Void> ptr);
typedef _IsAuditioningDart = bool Function(ffi.Pointer<ffi.Void> ptr);

typedef _OutputGlitchesNative = ffi.Uint32 Function(ffi.Pointer<ffi.Void> ptr);
typedef _OutputGlitchesDart = int Function(ffi.Pointer<ffi.Void> ptr);

/// Floats per note in `canto_engine_sync_notes` (matches `ffi::NOTE_STRIDE`).
const int _noteStride = 12;

/// Where a background load is (`load_job.rs` stages).
enum LoadStage { decoding, analyzing, installing, done }

/// A file load or re-analysis running on a native worker thread. Poll it
/// from the UI; [free] it once done (freeing early cancels it).
class NativeLoadJob {
  final ffi.Pointer<ffi.Void> _job;
  final _JobFns _fns;
  bool _freed = false;

  NativeLoadJob._(this._job, this._fns);

  LoadStage get stage =>
      _freed ? LoadStage.done : LoadStage.values[_fns.stage(_job).clamp(0, 3)];

  /// Analysis progress 0..1 (decoding and installing are short).
  double get progress => _freed ? 1 : _fns.progress(_job);

  /// The cached analysis was adopted; nothing was analysed.
  bool get usedCache => !_freed && _fns.usedCache(_job);

  void cancel() {
    if (!_freed) _fns.cancel(_job);
  }

  /// The result once the work has finished, else null. Returned once.
  CantoAnalysisResult? takeResult({bool fromCache = false}) {
    if (_freed) return null;
    final ptr = _fns.takeResult(_job);
    if (ptr == ffi.nullptr) return null;
    try {
      return CantoAnalysisResult.fromJsonString(
        ptr.toDartString(),
        fromCache: fromCache,
      );
    } finally {
      _fns.freeString(ptr);
    }
  }

  void free() {
    if (_freed) return;
    _freed = true;
    _fns.free(_job);
  }
}

class _JobFns {
  final int Function(ffi.Pointer<ffi.Void>) stage;
  final double Function(ffi.Pointer<ffi.Void>) progress;
  final bool Function(ffi.Pointer<ffi.Void>) usedCache;
  final void Function(ffi.Pointer<ffi.Void>) cancel;
  final ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>) takeResult;
  final void Function(ffi.Pointer<ffi.Void>) free;
  final void Function(ffi.Pointer<Utf8>) freeString;

  _JobFns(ffi.DynamicLibrary lib, this.freeString)
    : stage = lib
          .lookupFunction<
            ffi.Uint32 Function(ffi.Pointer<ffi.Void>),
            int Function(ffi.Pointer<ffi.Void>)
          >('canto_load_job_stage'),
      progress = lib
          .lookupFunction<
            ffi.Float Function(ffi.Pointer<ffi.Void>),
            double Function(ffi.Pointer<ffi.Void>)
          >('canto_load_job_progress'),
      usedCache = lib
          .lookupFunction<
            ffi.Bool Function(ffi.Pointer<ffi.Void>),
            bool Function(ffi.Pointer<ffi.Void>)
          >('canto_load_job_used_cache'),
      cancel = lib
          .lookupFunction<
            ffi.Void Function(ffi.Pointer<ffi.Void>),
            void Function(ffi.Pointer<ffi.Void>)
          >('canto_load_job_cancel'),
      takeResult = lib
          .lookupFunction<
            ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>),
            ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>)
          >('canto_load_job_take_result'),
      free = lib
          .lookupFunction<
            ffi.Void Function(ffi.Pointer<ffi.Void>),
            void Function(ffi.Pointer<ffi.Void>)
          >('canto_load_job_free');
}

/// FFI wrapper around the Rust `canto_dsp` (`CantoEngine`) audio & vocal DSP library.
class RustCantoDsp {
  late final ffi.DynamicLibrary _lib;
  late final _EngineNewDart _engineNew;
  late final _EngineFreeDart _engineFree;
  late final _FreeStringDart _freeString;
  late final _LoadAudioFileDart _loadAudioFile;
  late final _LoadAudioBytesDart _loadAudioBytes;
  late final _AnalyzeDart _analyze;
  late final _LoadAudioFileCachedDart _loadAudioFileCached;
  late final _SetTransportDart _setTransport;
  late final _IsPlayingDart _isPlaying;
  late final _SeekDart _seek;
  late final _PlayheadDart _playhead;
  late final _DurationDart _duration;
  late final _LevelDart _level;
  late final _SetKeyModeDart _setKeyMode;
  late final _SetCorrectDart _setCorrect;
  late final _UpdateNoteDart _updateNote;
  late final _AuditionNoteDart _auditionNote;
  late final _SetDirectVoiceDart _setDirectVoice;
  late final _RenderF32Dart _renderF32;
  late final _RenderI16Dart _renderI16;
  late final _SyncNotesDart _syncNotes;
  late final bool Function(ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>)
  _setNotesJson;
  late final _OutputStartDart _outputStart;
  late final _OutputPauseDart _outputPause;
  late final _OutputGlitchesDart _outputGlitches;
  late final _AuditionReleaseDart _auditionRelease;
  late final _IsAuditioningDart _isAuditioning;

  ffi.Pointer<ffi.Void> _handle = ffi.nullptr;

  // Reused native buffers so steady-state syncing/rendering doesn't allocate.
  ffi.Pointer<ffi.Float> _noteBuf = ffi.nullptr;
  int _noteBufCapacity = 0;
  ffi.Pointer<ffi.Int16> _pcmBuf = ffi.nullptr;
  int _pcmBufCapacity = 0;

  bool get isLoaded => _handle != ffi.nullptr;

  RustCantoDsp._(this._lib, double sampleRate) {
    _engineNew = _lib.lookupFunction<_EngineNewNative, _EngineNewDart>(
      'canto_engine_new',
    );
    _engineFree = _lib.lookupFunction<_EngineFreeNative, _EngineFreeDart>(
      'canto_engine_free',
    );
    _freeString = _lib.lookupFunction<_FreeStringNative, _FreeStringDart>(
      'canto_free_string',
    );
    _loadAudioFile = _lib
        .lookupFunction<_LoadAudioFileNative, _LoadAudioFileDart>(
          'canto_engine_load_audio_file',
        );
    _loadAudioBytes = _lib
        .lookupFunction<_LoadAudioBytesNative, _LoadAudioBytesDart>(
          'canto_engine_load_audio_bytes',
        );
    _analyze = _lib.lookupFunction<_AnalyzeNative, _AnalyzeDart>(
      'canto_engine_analyze',
    );
    _loadAudioFileCached = _lib
        .lookupFunction<_LoadAudioFileCachedNative, _LoadAudioFileCachedDart>(
          'canto_engine_load_audio_file_cached',
        );
    _setTransport = _lib.lookupFunction<_SetTransportNative, _SetTransportDart>(
      'canto_engine_set_transport',
    );
    _isPlaying = _lib.lookupFunction<_IsPlayingNative, _IsPlayingDart>(
      'canto_engine_is_playing',
    );
    _seek = _lib.lookupFunction<_SeekNative, _SeekDart>('canto_engine_seek');
    _playhead = _lib.lookupFunction<_PlayheadNative, _PlayheadDart>(
      'canto_engine_playhead',
    );
    _duration = _lib.lookupFunction<_DurationNative, _DurationDart>(
      'canto_engine_duration',
    );
    _level = _lib.lookupFunction<_LevelNative, _LevelDart>(
      'canto_engine_level',
    );
    _setKeyMode = _lib.lookupFunction<_SetKeyModeNative, _SetKeyModeDart>(
      'canto_engine_set_key_mode',
    );
    _setCorrect = _lib.lookupFunction<_SetCorrectNative, _SetCorrectDart>(
      'canto_engine_set_correct',
    );
    _updateNote = _lib.lookupFunction<_UpdateNoteNative, _UpdateNoteDart>(
      'canto_engine_update_note',
    );
    _auditionNote = _lib.lookupFunction<_AuditionNoteNative, _AuditionNoteDart>(
      'canto_engine_audition_note',
    );
    _setDirectVoice = _lib
        .lookupFunction<_SetDirectVoiceNative, _SetDirectVoiceDart>(
          'canto_engine_set_direct_voice',
        );
    _renderF32 = _lib.lookupFunction<_RenderF32Native, _RenderF32Dart>(
      'canto_engine_render_f32',
    );
    _renderI16 = _lib.lookupFunction<_RenderI16Native, _RenderI16Dart>(
      'canto_engine_render_i16',
    );
    _setNotesJson = _lib
        .lookupFunction<
          ffi.Bool Function(ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>),
          bool Function(ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>)
        >('canto_engine_set_notes_json');
    _syncNotes = _lib.lookupFunction<_SyncNotesNative, _SyncNotesDart>(
      'canto_engine_sync_notes',
    );
    _outputStart = _lib.lookupFunction<_OutputStartNative, _OutputStartDart>(
      'canto_output_start',
    );
    _outputPause = _lib.lookupFunction<_OutputPauseNative, _OutputPauseDart>(
      'canto_output_pause',
    );
    _outputGlitches = _lib
        .lookupFunction<_OutputGlitchesNative, _OutputGlitchesDart>(
          'canto_output_glitches',
        );
    _auditionRelease = _lib
        .lookupFunction<_AuditionReleaseNative, _AuditionReleaseDart>(
          'canto_engine_audition_release',
        );
    _isAuditioning = _lib
        .lookupFunction<_IsAuditioningNative, _IsAuditioningDart>(
          'canto_engine_is_auditioning',
        );

    _handle = _engineNew(sampleRate);
  }

  /// Renders the clip with all edits to a 16-bit mono WAV at [path].
  /// Returns null on success, otherwise the error message.
  ///
  /// Runs on a background isolate: the engine is thread-safe (Rust clones it
  /// under its lock and renders without holding it), so the UI stays live.
  /// The handle must stay alive until the future completes.
  Future<String?> exportWav(String path) {
    if (!isLoaded) return Future.value('Rust DSP library is not loaded');
    final address = _handle.address;
    return Isolate.run(() => _exportOnIsolate(address, path));
  }

  static String? _exportOnIsolate(int address, String path) {
    final lib = openLibrary();
    if (lib == null) return 'Rust DSP library is not available';
    final export = lib
        .lookupFunction<
          ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>),
          ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>)
        >('canto_engine_export_wav');
    final free = lib.lookupFunction<_FreeStringNative, _FreeStringDart>(
      'canto_free_string',
    );
    final pathPtr = path.toNativeUtf8();
    try {
      final err = export(ffi.Pointer.fromAddress(address), pathPtr);
      if (err == ffi.nullptr) return null;
      final message = err.toDartString();
      free(err);
      return message;
    } finally {
      malloc.free(pathPtr);
    }
  }

  static RustCantoDsp? tryOpen({double sampleRate = 48000.0}) {
    try {
      final lib = openLibrary();
      if (lib == null) return null;
      return RustCantoDsp._(lib, sampleRate);
    } catch (_) {
      return null;
    }
  }

  /// The canto_dsp native library for this platform, or null.
  static ffi.DynamicLibrary? openLibrary() {
    if (Platform.isAndroid) {
      return ffi.DynamicLibrary.open('libcanto_dsp.so');
    }
    if (Platform.isWindows) {
      final candidates = [
        'rust/target/release/canto_dsp.dll',
        'rust/target/debug/canto_dsp.dll',
        'canto_dsp.dll',
      ];
      for (final path in candidates) {
        if (File(path).existsSync()) {
          return ffi.DynamicLibrary.open(File(path).absolute.path);
        }
      }
    }
    if (Platform.isLinux) {
      return ffi.DynamicLibrary.open('libcanto_dsp.so');
    }
    if (Platform.isMacOS || Platform.isIOS) {
      return ffi.DynamicLibrary.process();
    }
    return null;
  }

  late final _JobFns _jobFns = _JobFns(_lib, _freeString);

  /// Starts loading [filePath] on a worker thread, adopting
  /// [cachedAnalysis] if it fits the audio. Null if it couldn't start.
  NativeLoadJob? startLoadJob(String filePath, {String? cachedAnalysis}) {
    if (!isLoaded) return null;
    final start = _lib
        .lookupFunction<
          ffi.Pointer<ffi.Void> Function(
            ffi.Pointer<ffi.Void>,
            ffi.Pointer<Utf8>,
            ffi.Pointer<Utf8>,
          ),
          ffi.Pointer<ffi.Void> Function(
            ffi.Pointer<ffi.Void>,
            ffi.Pointer<Utf8>,
            ffi.Pointer<Utf8>,
          )
        >('canto_load_job_start');
    final pathPtr = filePath.toNativeUtf8();
    final cachedPtr = cachedAnalysis?.toNativeUtf8() ?? ffi.nullptr;
    try {
      final job = start(_handle, pathPtr, cachedPtr);
      return job == ffi.nullptr ? null : NativeLoadJob._(job, _jobFns);
    } finally {
      // The worker copied both strings before this returns.
      malloc.free(pathPtr);
      if (cachedPtr != ffi.nullptr) malloc.free(cachedPtr);
    }
  }

  /// Starts analysing the loaded audio again on a worker thread.
  NativeLoadJob? startReanalyzeJob() {
    if (!isLoaded) return null;
    final start = _lib
        .lookupFunction<
          ffi.Pointer<ffi.Void> Function(ffi.Pointer<ffi.Void>),
          ffi.Pointer<ffi.Void> Function(ffi.Pointer<ffi.Void>)
        >('canto_reanalyze_job_start');
    final job = start(_handle);
    return job == ffi.nullptr ? null : NativeLoadJob._(job, _jobFns);
  }

  /// Decodes [filePath] and adopts [analysisJson] (a cached result of this
  /// engine for the same file) instead of analysing. Not ok when the cache
  /// entry is unusable; the caller then falls back to [loadAudioFile].
  CantoAnalysisResult loadAudioFileCached(
    String filePath,
    String analysisJson,
  ) {
    if (!isLoaded) {
      return const CantoAnalysisResult(
        ok: false,
        error: 'Rust DSP library is not loaded',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: [],
        waveformPeaks: [],
      );
    }
    final pathPtr = filePath.toNativeUtf8();
    final analysisPtr = analysisJson.toNativeUtf8();
    ffi.Pointer<Utf8> jsonPtr = ffi.nullptr;
    try {
      jsonPtr = _loadAudioFileCached(_handle, pathPtr, analysisPtr);
      if (jsonPtr == ffi.nullptr) {
        return const CantoAnalysisResult(
          ok: false,
          error: 'Null response from Rust audio loader',
          sampleRate: 48000,
          durationSec: 0.0,
          bpm: 120.0,
          key: 'A',
          mode: 'minor',
          notes: [],
          waveformPeaks: [],
        );
      }
      return CantoAnalysisResult.fromJsonString(
        jsonPtr.toDartString(),
        fromCache: true,
      );
    } finally {
      malloc.free(pathPtr);
      malloc.free(analysisPtr);
      if (jsonPtr != ffi.nullptr) _freeString(jsonPtr);
    }
  }

  CantoAnalysisResult loadAudioFile(String filePath) {
    if (!isLoaded) {
      return const CantoAnalysisResult(
        ok: false,
        error: 'Rust DSP library is not loaded',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: [],
        waveformPeaks: [],
      );
    }
    final pathPtr = filePath.toNativeUtf8();
    ffi.Pointer<Utf8> jsonPtr = ffi.nullptr;
    try {
      jsonPtr = _loadAudioFile(_handle, pathPtr);
      if (jsonPtr == ffi.nullptr) {
        return const CantoAnalysisResult(
          ok: false,
          error: 'Null response from Rust audio loader',
          sampleRate: 48000,
          durationSec: 0.0,
          bpm: 120.0,
          key: 'A',
          mode: 'minor',
          notes: [],
          waveformPeaks: [],
        );
      }
      final jsonStr = jsonPtr.toDartString();
      return CantoAnalysisResult.fromJsonString(jsonStr);
    } finally {
      malloc.free(pathPtr);
      if (jsonPtr != ffi.nullptr) {
        _freeString(jsonPtr);
      }
    }
  }

  CantoAnalysisResult loadAudioBytes(Uint8List bytes, {String? extHint}) {
    if (!isLoaded || bytes.isEmpty) {
      return const CantoAnalysisResult(
        ok: false,
        error: 'Rust DSP library not loaded or empty audio bytes',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: [],
        waveformPeaks: [],
      );
    }
    final bytesPtr = malloc<ffi.Uint8>(bytes.length);
    final extPtr = (extHint ?? 'wav').toNativeUtf8();
    ffi.Pointer<Utf8> jsonPtr = ffi.nullptr;
    try {
      bytesPtr.asTypedList(bytes.length).setAll(0, bytes);
      jsonPtr = _loadAudioBytes(_handle, bytesPtr, bytes.length, extPtr);
      if (jsonPtr == ffi.nullptr) {
        return const CantoAnalysisResult(
          ok: false,
          error: 'Null response from Rust byte loader',
          sampleRate: 48000,
          durationSec: 0.0,
          bpm: 120.0,
          key: 'A',
          mode: 'minor',
          notes: [],
          waveformPeaks: [],
        );
      }
      return CantoAnalysisResult.fromJsonString(jsonPtr.toDartString());
    } finally {
      malloc.free(bytesPtr);
      malloc.free(extPtr);
      if (jsonPtr != ffi.nullptr) {
        _freeString(jsonPtr);
      }
    }
  }

  CantoAnalysisResult analyzeLoadedAudio() {
    if (!isLoaded) {
      return const CantoAnalysisResult(
        ok: false,
        error: 'Rust DSP library is not loaded',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: [],
        waveformPeaks: [],
      );
    }
    ffi.Pointer<Utf8> jsonPtr = ffi.nullptr;
    try {
      jsonPtr = _analyze(_handle);
      if (jsonPtr == ffi.nullptr) {
        return const CantoAnalysisResult(
          ok: false,
          error: 'Null response from Rust analyzer',
          sampleRate: 48000,
          durationSec: 0.0,
          bpm: 120.0,
          key: 'A',
          mode: 'minor',
          notes: [],
          waveformPeaks: [],
        );
      }
      return CantoAnalysisResult.fromJsonString(jsonPtr.toDartString());
    } finally {
      if (jsonPtr != ffi.nullptr) {
        _freeString(jsonPtr);
      }
    }
  }

  void setTransport({
    required bool playing,
    required bool loopEnabled,
    required bool bypass,
  }) {
    if (!isLoaded) return;
    _setTransport(_handle, playing, loopEnabled, bypass);
  }

  bool get isPlaying => isLoaded ? _isPlaying(_handle) : false;

  void seek(double timeSec) {
    if (!isLoaded) return;
    _seek(_handle, timeSec);
  }

  double get playhead => isLoaded ? _playhead(_handle) : 0.0;

  double get duration => isLoaded ? _duration(_handle) : 0.0;

  double get level => isLoaded ? _level(_handle) : 0.0;

  void setKeyMode(String keyName, String mode) {
    if (!isLoaded) return;
    final k = keyPc(keyName);
    final m = switch (mode) {
      'minor' => 0,
      'major' => 1,
      'penta' => 2,
      _ => 3,
    };
    _setKeyMode(_handle, k, m);
  }

  void setCorrect(double amount) {
    if (!isLoaded) return;
    _setCorrect(_handle, amount);
  }

  /// Sends every note's editable state in one FFI call (one engine lock).
  void syncNotes(List<VocalNote> notes) {
    if (!isLoaded || notes.isEmpty) return;
    final needed = notes.length * _noteStride;
    if (needed > _noteBufCapacity) {
      if (_noteBuf != ffi.nullptr) malloc.free(_noteBuf);
      _noteBufCapacity = needed * 2;
      _noteBuf = malloc<ffi.Float>(_noteBufCapacity);
    }
    final data = _noteBuf.asTypedList(needed);
    for (int i = 0; i < notes.length; i++) {
      final n = notes[i];
      final o = i * _noteStride;
      data[o] = n.pitch;
      data[o + 1] = n.start;
      data[o + 2] = n.dur;
      data[o + 3] = n.modulation;
      data[o + 4] = n.formant;
      data[o + 5] = n.gainDb;
      data[o + 6] = n.pinned ? 1.0 : 0.0;
      data[o + 7] = n.muted ? 1.0 : 0.0;
      data[o + 8] = n.ornStyle.toDouble();
      data[o + 9] = n.ornDepth;
      data[o + 10] = n.ornSpeed;
      data[o + 11] = n.accent;
    }
    _syncNotes(_handle, _noteBuf, notes.length);
  }

  /// Replaces the engine's whole note list (after cutting/joining notes or
  /// undoing one). Returns false if the engine rejected it.
  bool replaceNotes(List<VocalNote> notes) {
    if (!isLoaded) return false;
    final json = jsonEncode([for (final n in notes) n.toJson()]).toNativeUtf8();
    try {
      return _setNotesJson(_handle, json);
    } finally {
      malloc.free(json);
    }
  }

  /// Sends one note's editable state (used while dragging a single note).
  void updateNote(int index, VocalNote n) {
    if (!isLoaded) return;
    _updateNote(
      _handle,
      index,
      n.pitch,
      n.start,
      n.dur,
      n.modulation,
      n.formant,
      n.gainDb,
      n.pinned,
      n.muted,
      n.ornStyle,
      n.ornDepth,
      n.ornSpeed,
      n.accent,
    );
  }

  /// Starts the native (AAudio) output stream; the engine then renders on its
  /// own audio thread. Returns false when native output is unavailable.
  bool outputStart() => isLoaded && _outputStart(_handle);

  void outputPause() {
    if (isLoaded) _outputPause(_handle);
  }

  /// Device underruns plus callbacks that found the engine busy.
  int get outputGlitches => isLoaded ? _outputGlitches(_handle) : 0;

  /// Starts previewing note [index] from its beginning (loops while held).
  void auditionNote(int index) {
    if (!isLoaded) return;
    _auditionNote(_handle, index);
  }

  /// Stops the preview with a short fade.
  void stopAudition() {
    if (!isLoaded) return;
    _auditionNote(_handle, -1);
  }

  /// Finger lifted: the preview finishes the note, then stops.
  void releaseAudition() {
    if (isLoaded) _auditionRelease(_handle);
  }

  bool get isAuditioning => isLoaded && _isAuditioning(_handle);

  void setDirectVoice({
    required double hz,
    required double cutoff,
    required double gain,
  }) {
    if (!isLoaded) return;
    _setDirectVoice(_handle, hz, cutoff, gain);
  }

  /// Renders `frames` stereo frames in the Rust TD-PSOLA engine and returns the meter level.
  double renderBlockF32(int frames) {
    if (!isLoaded || frames <= 0) return 0.0;
    final ptr = calloc<ffi.Float>(frames * 2);
    try {
      return _renderF32(_handle, ptr, frames);
    } finally {
      calloc.free(ptr);
    }
  }

  /// Renders `frames` stereo `i16` frames and returns both the PCM bytes (`Uint8List`) and meter level.
  ({Uint8List pcmBytes, double level}) renderBlockPcm16Bytes(int frames) {
    if (!isLoaded || frames <= 0) {
      return (pcmBytes: Uint8List(0), level: 0.0);
    }
    final sampleCount = frames * 2;
    if (sampleCount > _pcmBufCapacity) {
      if (_pcmBuf != ffi.nullptr) malloc.free(_pcmBuf);
      _pcmBufCapacity = sampleCount;
      _pcmBuf = malloc<ffi.Int16>(_pcmBufCapacity);
    }
    final lvl = _renderI16(_handle, _pcmBuf, frames);
    // Copy out: the platform channel keeps the bytes past this call.
    final byteView = _pcmBuf.cast<ffi.Uint8>().asTypedList(sampleCount * 2);
    return (pcmBytes: Uint8List.fromList(byteView), level: lvl);
  }

  /// Renders `frames` stereo `i16` frames and returns both the PCM list and meter level.
  ({List<int> pcm16, double level}) renderBlockI16(int frames) {
    if (!isLoaded || frames <= 0) return (pcm16: const <int>[], level: 0.0);
    final ptr = calloc<ffi.Int16>(frames * 2);
    try {
      final lvl = _renderI16(_handle, ptr, frames);
      final list = ptr.asTypedList(frames * 2).toList();
      return (pcm16: list, level: lvl);
    } finally {
      calloc.free(ptr);
    }
  }

  void dispose() {
    if (_handle != ffi.nullptr) {
      _engineFree(_handle);
      _handle = ffi.nullptr;
    }
    if (_noteBuf != ffi.nullptr) {
      malloc.free(_noteBuf);
      _noteBuf = ffi.nullptr;
      _noteBufCapacity = 0;
    }
    if (_pcmBuf != ffi.nullptr) {
      malloc.free(_pcmBuf);
      _pcmBuf = ffi.nullptr;
      _pcmBufCapacity = 0;
    }
  }
}
