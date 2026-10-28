import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:io' show File, Platform;
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
  });

  factory CantoAnalysisResult.fromJsonString(String jsonStr) {
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
typedef _SeekDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  double timeSec,
);

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
);

typedef _AuditionNoteNative = ffi.Void Function(
  ffi.Pointer<ffi.Void> ptr,
  ffi.Int32 index,
);
typedef _AuditionNoteDart = void Function(
  ffi.Pointer<ffi.Void> ptr,
  int index,
);

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

/// FFI wrapper around the Rust `canto_dsp` (`CantoEngine`) audio & vocal DSP library.
class RustCantoDsp {
  late final ffi.DynamicLibrary _lib;
  late final _EngineNewDart _engineNew;
  late final _EngineFreeDart _engineFree;
  late final _FreeStringDart _freeString;
  late final _LoadAudioFileDart _loadAudioFile;
  late final _LoadAudioBytesDart _loadAudioBytes;
  late final _AnalyzeDart _analyze;
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

  ffi.Pointer<ffi.Void> _handle = ffi.nullptr;

  bool get isLoaded => _handle != ffi.nullptr;

  RustCantoDsp._(this._lib, double sampleRate) {
    _engineNew = _lib
        .lookupFunction<_EngineNewNative, _EngineNewDart>('canto_engine_new');
    _engineFree = _lib
        .lookupFunction<_EngineFreeNative, _EngineFreeDart>('canto_engine_free');
    _freeString = _lib
        .lookupFunction<_FreeStringNative, _FreeStringDart>('canto_free_string');
    _loadAudioFile = _lib.lookupFunction<_LoadAudioFileNative, _LoadAudioFileDart>(
      'canto_engine_load_audio_file',
    );
    _loadAudioBytes =
        _lib.lookupFunction<_LoadAudioBytesNative, _LoadAudioBytesDart>(
      'canto_engine_load_audio_bytes',
    );
    _analyze =
        _lib.lookupFunction<_AnalyzeNative, _AnalyzeDart>('canto_engine_analyze');
    _setTransport =
        _lib.lookupFunction<_SetTransportNative, _SetTransportDart>(
      'canto_engine_set_transport',
    );
    _isPlaying = _lib.lookupFunction<_IsPlayingNative, _IsPlayingDart>(
      'canto_engine_is_playing',
    );
    _seek = _lib.lookupFunction<_SeekNative, _SeekDart>('canto_engine_seek');
    _playhead = _lib
        .lookupFunction<_PlayheadNative, _PlayheadDart>('canto_engine_playhead');
    _duration = _lib
        .lookupFunction<_DurationNative, _DurationDart>('canto_engine_duration');
    _level =
        _lib.lookupFunction<_LevelNative, _LevelDart>('canto_engine_level');
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
    _setDirectVoice =
        _lib.lookupFunction<_SetDirectVoiceNative, _SetDirectVoiceDart>(
      'canto_engine_set_direct_voice',
    );
    _renderF32 = _lib
        .lookupFunction<_RenderF32Native, _RenderF32Dart>('canto_engine_render_f32');
    _renderI16 = _lib
        .lookupFunction<_RenderI16Native, _RenderI16Dart>('canto_engine_render_i16');

    _handle = _engineNew(sampleRate);
  }

  static RustCantoDsp? tryOpen({double sampleRate = 48000.0}) {
    try {
      final lib = _openDynamicLibrary();
      if (lib == null) return null;
      return RustCantoDsp._(lib, sampleRate);
    } catch (_) {
      return null;
    }
  }

  static ffi.DynamicLibrary? _openDynamicLibrary() {
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

  void syncNotes(List<VocalNote> notes) {
    if (!isLoaded) return;
    for (int i = 0; i < notes.length; i++) {
      final n = notes[i];
      _updateNote(
        _handle,
        i,
        n.pitch,
        n.start,
        n.dur,
        n.modulation,
        n.formant,
        n.gainDb,
        n.pinned,
      );
    }
  }

  void auditionNote(int index) {
    if (!isLoaded) return;
    _auditionNote(_handle, index);
  }

  void stopAudition() {
    if (!isLoaded) return;
    _auditionNote(_handle, -1);
  }

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
    final ptr = calloc<ffi.Int16>(sampleCount);
    try {
      final lvl = _renderI16(_handle, ptr, frames);
      final byteView = ptr.cast<ffi.Uint8>().asTypedList(sampleCount * 2);
      final copy = Uint8List.fromList(byteView);
      return (pcmBytes: copy, level: lvl);
    } finally {
      calloc.free(ptr);
    }
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
  }
}
