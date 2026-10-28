import 'dart:ffi' as ffi;
import 'dart:io' show Platform;

import 'package:ffi/ffi.dart';
import 'package:flutter/services.dart';

import 'rust_dsp.dart';

/// What the recording screen shows each frame.
class RecorderStatus {
  final bool recording;
  final double seconds;

  /// Peak level 0..1 with a short decay.
  final double level;

  /// Sung pitch in Hz; 0 when nothing voiced is heard.
  final double pitchHz;

  /// A full-scale sample arrived since the last poll.
  final bool clipped;

  const RecorderStatus({
    this.recording = false,
    this.seconds = 0,
    this.level = 0,
    this.pitchHz = 0,
    this.clipped = false,
  });
}

/// Live microphone recording. The native implementation captures on an
/// AAudio input thread in Rust; tests substitute a fake.
abstract class LiveRecorder {
  /// Asks for microphone access if it hasn't been granted yet.
  Future<bool> ensurePermission();

  /// Starts a new take. Returns null on success, otherwise the error.
  String? start();

  /// Call every frame while recording: drains audio, returns live status.
  RecorderStatus poll();

  void stop();

  /// Saves the take (48 kHz mono WAV). Null on success, otherwise the error.
  String? save(String path);

  void discard();

  void dispose();
}

class NativeLiveRecorder implements LiveRecorder {
  static const MethodChannel _mic = MethodChannel('canto/mic');

  final ffi.Pointer<ffi.Void> _rec;
  final ffi.Pointer<ffi.Float> _status = malloc<ffi.Float>(4);
  final ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>) _start;
  final bool Function(ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Float>) _poll;
  final void Function(ffi.Pointer<ffi.Void>) _stop;
  final void Function(ffi.Pointer<ffi.Void>) _discard;
  final ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>)
  _save;
  final void Function(ffi.Pointer<ffi.Void>) _free;
  final void Function(ffi.Pointer<Utf8>) _freeString;
  final void Function(ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Float>, int, int)
  _feed;
  bool _disposed = false;

  NativeLiveRecorder._(ffi.DynamicLibrary lib)
    : _rec = lib
          .lookupFunction<
            ffi.Pointer<ffi.Void> Function(),
            ffi.Pointer<ffi.Void> Function()
          >('canto_rec_new')(),
      _start = lib
          .lookupFunction<
            ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>),
            ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>)
          >('canto_rec_start'),
      _poll = lib
          .lookupFunction<
            ffi.Bool Function(ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Float>),
            bool Function(ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Float>)
          >('canto_rec_poll'),
      _stop = lib
          .lookupFunction<
            ffi.Void Function(ffi.Pointer<ffi.Void>),
            void Function(ffi.Pointer<ffi.Void>)
          >('canto_rec_stop'),
      _discard = lib
          .lookupFunction<
            ffi.Void Function(ffi.Pointer<ffi.Void>),
            void Function(ffi.Pointer<ffi.Void>)
          >('canto_rec_discard'),
      _save = lib
          .lookupFunction<
            ffi.Pointer<Utf8> Function(
              ffi.Pointer<ffi.Void>,
              ffi.Pointer<Utf8>,
            ),
            ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>)
          >('canto_rec_save'),
      _free = lib
          .lookupFunction<
            ffi.Void Function(ffi.Pointer<ffi.Void>),
            void Function(ffi.Pointer<ffi.Void>)
          >('canto_rec_free'),
      _freeString = lib
          .lookupFunction<
            ffi.Void Function(ffi.Pointer<Utf8>),
            void Function(ffi.Pointer<Utf8>)
          >('canto_free_string'),
      _feed = lib
          .lookupFunction<
            ffi.Void Function(
              ffi.Pointer<ffi.Void>,
              ffi.Pointer<ffi.Float>,
              ffi.Size,
              ffi.Uint32,
            ),
            void Function(
              ffi.Pointer<ffi.Void>,
              ffi.Pointer<ffi.Float>,
              int,
              int,
            )
          >('canto_rec_feed_for_test');

  /// Null when the native library isn't available.
  static NativeLiveRecorder? tryOpen() {
    try {
      final lib = RustCantoDsp.openLibrary();
      return lib == null ? null : NativeLiveRecorder._(lib);
    } catch (_) {
      return null;
    }
  }

  @override
  Future<bool> ensurePermission() async {
    if (!Platform.isAndroid) return true;
    try {
      return await _mic.invokeMethod<bool>('request') ?? false;
    } catch (_) {
      return false;
    }
  }

  String? _takeError(ffi.Pointer<Utf8> err) {
    if (err == ffi.nullptr) return null;
    final message = err.toDartString();
    _freeString(err);
    return message;
  }

  @override
  String? start() => _takeError(_start(_rec));

  @override
  RecorderStatus poll() {
    final recording = _poll(_rec, _status);
    return RecorderStatus(
      recording: recording,
      seconds: _status[0],
      level: _status[1],
      pitchHz: _status[2],
      clipped: _status[3] > 0.5,
    );
  }

  @override
  void stop() => _stop(_rec);

  @override
  String? save(String path) {
    final p = path.toNativeUtf8();
    try {
      return _takeError(_save(_rec, p));
    } finally {
      malloc.free(p);
    }
  }

  @override
  void discard() => _discard(_rec);

  /// Test hook: feeds samples as if sung into the microphone.
  void feedForTest(List<double> samples, {int sampleRate = 48000}) {
    final buf = malloc<ffi.Float>(samples.length);
    try {
      buf.asTypedList(samples.length).setAll(0, samples);
      _feed(_rec, buf, samples.length, sampleRate);
    } finally {
      malloc.free(buf);
    }
  }

  @override
  void dispose() {
    if (_disposed) return;
    _disposed = true;
    _free(_rec);
    malloc.free(_status);
  }
}
