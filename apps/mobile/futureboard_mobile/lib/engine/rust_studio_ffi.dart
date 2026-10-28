// Native C-ABI FFI bindings to `fb_studio_mobile_engine` (Rust DSP & DAW engine).

import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:io';
import 'dart:typed_data';
import 'package:ffi/ffi.dart';

typedef _FbEngineNewC = ffi.Pointer<ffi.Void> Function(ffi.Uint32 sampleRate);
typedef _FbEngineNewDart = ffi.Pointer<ffi.Void> Function(int sampleRate);

typedef _FbEngineFreeC = ffi.Void Function(ffi.Pointer<ffi.Void> handle);
typedef _FbEngineFreeDart = void Function(ffi.Pointer<ffi.Void> handle);

typedef _FbStringFreeC = ffi.Void Function(ffi.Pointer<Utf8> ptr);
typedef _FbStringFreeDart = void Function(ffi.Pointer<Utf8> ptr);

typedef _FbEngineRenderC = ffi.Void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<ffi.Float> outInterleaved,
  ffi.Uint32 frames,
);
typedef _FbEngineRenderDart = void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<ffi.Float> outInterleaved,
  int frames,
);

typedef _FbEngineSetBoolC = ffi.Void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Bool value,
);
typedef _FbEngineSetBoolDart = void Function(
  ffi.Pointer<ffi.Void> handle,
  bool value,
);

typedef _FbEngineSetF64C = ffi.Void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Double value,
);
typedef _FbEngineSetF64Dart = void Function(
  ffi.Pointer<ffi.Void> handle,
  double value,
);

typedef _FbEngineTriggerMidiC = ffi.Void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> trackId,
  ffi.Uint8 note,
  ffi.Uint8 velocity,
  ffi.Bool isNoteOn,
);
typedef _FbEngineTriggerMidiDart = void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> trackId,
  int note,
  int velocity,
  bool isNoteOn,
);

typedef _FbEngineSetTrackMixerC = ffi.Void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> trackId,
  ffi.Float volumeDb,
  ffi.Float pan,
  ffi.Int32 muted,
  ffi.Int32 soloed,
  ffi.Int32 armed,
  ffi.Int32 inputMonitoring,
);
typedef _FbEngineSetTrackMixerDart = void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> trackId,
  double volumeDb,
  double pan,
  int muted,
  int soloed,
  int armed,
  int inputMonitoring,
);

typedef _FbEngineSetPluginParamC = ffi.Void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> trackId,
  ffi.Int32 slotIndex,
  ffi.Pointer<Utf8> paramId,
  ffi.Float value,
);
typedef _FbEngineSetPluginParamDart = void Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> trackId,
  int slotIndex,
  ffi.Pointer<Utf8> paramId,
  double value,
);

typedef _FbEngineGetJsonC = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> handle,
);
typedef _FbEngineGetJsonDart = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> handle,
);

typedef _FbEngineDispatchJsonC = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> actionJson,
);
typedef _FbEngineDispatchJsonDart = ffi.Pointer<Utf8> Function(
  ffi.Pointer<ffi.Void> handle,
  ffi.Pointer<Utf8> actionJson,
);

class RustStudioFfi {
  late final ffi.DynamicLibrary _lib;
  late final _FbEngineNewDart _engineNew;
  late final _FbEngineFreeDart _engineFree;
  late final _FbStringFreeDart _stringFree;
  late final _FbEngineRenderDart _engineRender;
  late final _FbEngineSetBoolDart _setPlaying;
  late final _FbEngineSetBoolDart _setRecording;
  late final _FbEngineSetF64Dart _seekBeats;
  late final _FbEngineSetF64Dart _setBpm;
  late final _FbEngineTriggerMidiDart _triggerMidi;
  late final _FbEngineSetTrackMixerDart _setTrackMixer;
  late final _FbEngineSetPluginParamDart _setPluginParam;
  late final _FbEngineGetJsonDart _getSnapshotJson;
  late final _FbEngineGetJsonDart _getTelemetryJson;
  late final _FbEngineGetJsonDart _getCatalogJson;
  late final _FbEngineDispatchJsonDart _dispatchActionJson;

  ffi.Pointer<ffi.Void> _handle = ffi.nullptr;
  ffi.Pointer<ffi.Float> _renderScratch = ffi.nullptr;
  int _renderScratchFrames = 0;

  bool get isLoaded => _handle != ffi.nullptr;

  RustStudioFfi({int sampleRate = 48000}) {
    _lib = _openLibrary();
    _engineNew = _lib
        .lookupFunction<_FbEngineNewC, _FbEngineNewDart>('fb_engine_new');
    _engineFree = _lib
        .lookupFunction<_FbEngineFreeC, _FbEngineFreeDart>('fb_engine_free');
    _stringFree = _lib
        .lookupFunction<_FbStringFreeC, _FbStringFreeDart>('fb_string_free');
    _engineRender = _lib.lookupFunction<_FbEngineRenderC, _FbEngineRenderDart>(
        'fb_engine_render_f32');
    _setPlaying = _lib.lookupFunction<_FbEngineSetBoolC, _FbEngineSetBoolDart>(
        'fb_engine_set_playing');
    _setRecording =
        _lib.lookupFunction<_FbEngineSetBoolC, _FbEngineSetBoolDart>(
            'fb_engine_set_recording');
    _seekBeats = _lib.lookupFunction<_FbEngineSetF64C, _FbEngineSetF64Dart>(
        'fb_engine_seek_beats');
    _setBpm = _lib
        .lookupFunction<_FbEngineSetF64C, _FbEngineSetF64Dart>('fb_engine_set_bpm');
    _triggerMidi =
        _lib.lookupFunction<_FbEngineTriggerMidiC, _FbEngineTriggerMidiDart>(
            'fb_engine_trigger_midi');
    _setTrackMixer = _lib.lookupFunction<_FbEngineSetTrackMixerC,
        _FbEngineSetTrackMixerDart>('fb_engine_set_track_mixer');
    _setPluginParam = _lib.lookupFunction<_FbEngineSetPluginParamC,
        _FbEngineSetPluginParamDart>('fb_engine_set_plugin_param');
    _getSnapshotJson =
        _lib.lookupFunction<_FbEngineGetJsonC, _FbEngineGetJsonDart>(
            'fb_engine_get_snapshot_json');
    _getTelemetryJson =
        _lib.lookupFunction<_FbEngineGetJsonC, _FbEngineGetJsonDart>(
            'fb_engine_get_telemetry_json');
    _getCatalogJson =
        _lib.lookupFunction<_FbEngineGetJsonC, _FbEngineGetJsonDart>(
            'fb_engine_get_catalog_json');
    _dispatchActionJson =
        _lib.lookupFunction<_FbEngineDispatchJsonC, _FbEngineDispatchJsonDart>(
            'fb_engine_dispatch_action_json');

    _handle = _engineNew(sampleRate);
  }

  static ffi.DynamicLibrary _openLibrary() {
    if (Platform.isAndroid || Platform.isLinux) {
      return ffi.DynamicLibrary.open('libfb_studio_mobile_engine.so');
    }
    if (Platform.isWindows) {
      final candidates = [
        'fb_studio_mobile_engine.dll',
        'rust/target/release/fb_studio_mobile_engine.dll',
        'rust/target/debug/fb_studio_mobile_engine.dll',
        '${Directory.current.path}/rust/target/release/fb_studio_mobile_engine.dll',
        '${Directory.current.path}/rust/target/debug/fb_studio_mobile_engine.dll',
      ];
      for (final path in candidates) {
        if (File(path).existsSync()) {
          return ffi.DynamicLibrary.open(path);
        }
      }
      return ffi.DynamicLibrary.open('fb_studio_mobile_engine.dll');
    }
    return ffi.DynamicLibrary.process();
  }

  void dispose() {
    if (_renderScratch != ffi.nullptr) {
      calloc.free(_renderScratch);
      _renderScratch = ffi.nullptr;
      _renderScratchFrames = 0;
    }
    if (_handle != ffi.nullptr) {
      _engineFree(_handle);
      _handle = ffi.nullptr;
    }
  }

  /// Renders `frames` stereo frames from the Rust engine and converts them into
  /// 16-bit little-endian interleaved PCM bytes suitable for Android `AudioTrack`.
  Uint8List renderPcm16Stereo(int frames) {
    if (_handle == ffi.nullptr || frames <= 0) {
      return Uint8List(0);
    }
    if (_renderScratchFrames < frames) {
      if (_renderScratch != ffi.nullptr) {
        calloc.free(_renderScratch);
      }
      _renderScratch = calloc<ffi.Float>(frames * 2);
      _renderScratchFrames = frames;
    }
    _engineRender(_handle, _renderScratch, frames);
    final floatView = _renderScratch.asTypedList(frames * 2);
    final pcm16 = Int16List(frames * 2);
    for (int i = 0; i < frames * 2; i++) {
      final s = floatView[i].clamp(-1.0, 1.0);
      pcm16[i] = (s * 32767.0).round();
    }
    return pcm16.buffer.asUint8List();
  }

  void setPlaying(bool playing) {
    if (_handle != ffi.nullptr) _setPlaying(_handle, playing);
  }

  void setRecording(bool recording) {
    if (_handle != ffi.nullptr) _setRecording(_handle, recording);
  }

  void seekBeats(double beats) {
    if (_handle != ffi.nullptr) _seekBeats(_handle, beats);
  }

  void setBpm(double bpm) {
    if (_handle != ffi.nullptr) _setBpm(_handle, bpm);
  }

  void triggerMidi(String trackId, int note, int velocity, bool isNoteOn) {
    if (_handle == ffi.nullptr) return;
    final ptr = trackId.toNativeUtf8();
    try {
      _triggerMidi(_handle, ptr, note, velocity, isNoteOn);
    } finally {
      calloc.free(ptr);
    }
  }

  void setTrackMixer({
    required String trackId,
    double? volumeDb,
    double? pan,
    bool? muted,
    bool? soloed,
    bool? armed,
    bool? inputMonitoring,
  }) {
    if (_handle == ffi.nullptr) return;
    final ptr = trackId.toNativeUtf8();
    try {
      _setTrackMixer(
        _handle,
        ptr,
        volumeDb ?? double.nan,
        pan ?? double.nan,
        muted == null ? -1 : (muted ? 1 : 0),
        soloed == null ? -1 : (soloed ? 1 : 0),
        armed == null ? -1 : (armed ? 1 : 0),
        inputMonitoring == null ? -1 : (inputMonitoring ? 1 : 0),
      );
    } finally {
      calloc.free(ptr);
    }
  }

  void setPluginParam({
    required String trackId,
    required int? slotIndex,
    required String paramId,
    required double value,
  }) {
    if (_handle == ffi.nullptr) return;
    final tidPtr = trackId.toNativeUtf8();
    final pidPtr = paramId.toNativeUtf8();
    try {
      _setPluginParam(_handle, tidPtr, slotIndex ?? -1, pidPtr, value);
    } finally {
      calloc.free(tidPtr);
      calloc.free(pidPtr);
    }
  }

  Map<String, dynamic> getSnapshotMap() {
    if (_handle == ffi.nullptr) return const {};
    final ptr = _getSnapshotJson(_handle);
    if (ptr == ffi.nullptr) return const {};
    try {
      final str = ptr.toDartString();
      return jsonDecode(str) as Map<String, dynamic>;
    } finally {
      _stringFree(ptr);
    }
  }

  Map<String, dynamic> getTelemetryMap() {
    if (_handle == ffi.nullptr) return const {};
    final ptr = _getTelemetryJson(_handle);
    if (ptr == ffi.nullptr) return const {};
    try {
      final str = ptr.toDartString();
      return jsonDecode(str) as Map<String, dynamic>;
    } finally {
      _stringFree(ptr);
    }
  }

  List<dynamic> getCatalogList() {
    if (_handle == ffi.nullptr) return const [];
    final ptr = _getCatalogJson(_handle);
    if (ptr == ffi.nullptr) return const [];
    try {
      final str = ptr.toDartString();
      return jsonDecode(str) as List<dynamic>;
    } finally {
      _stringFree(ptr);
    }
  }

  Map<String, dynamic> dispatchAction(Map<String, dynamic> payload) {
    if (_handle == ffi.nullptr) return const {'ok': false};
    final inPtr = jsonEncode(payload).toNativeUtf8();
    try {
      final outPtr = _dispatchActionJson(_handle, inPtr);
      if (outPtr == ffi.nullptr) return const {'ok': false};
      try {
        return jsonDecode(outPtr.toDartString()) as Map<String, dynamic>;
      } finally {
        _stringFree(outPtr);
      }
    } finally {
      calloc.free(inPtr);
    }
  }
}
