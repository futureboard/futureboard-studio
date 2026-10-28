import 'dart:async';
import 'dart:io' show File, Platform, Process;

import 'package:flutter/services.dart';

import 'music.dart';
import 'rust_dsp.dart';

class PickedAudioFile {
  final String path;
  final String name;

  const PickedAudioFile({required this.path, required this.name});
}

/// Owns the Rust engine and its audio output.
///
/// Preferred path: the engine renders on a native AAudio thread
/// (`outputStart`); Dart only syncs edits and reads the published playhead.
/// Fallback (no AAudio, or desktop/tests): Dart pulls blocks each frame and
/// pushes PCM to the Android `AudioTrack` over the platform channel.
class SynthEngine {
  static const MethodChannel _channel = MethodChannel('canto/audio');
  static const double sampleRate = 48000.0;
  // Long enough that tapping through notes keeps the stream warm (no restart
  // latency between previews), short enough not to hold audio when idle.
  static const Duration _idleGrace = Duration(seconds: 3);

  bool _fallbackStarted = false;
  // null until the first activation decides between native and fallback.
  bool? _native;
  Timer? _idleTimer;
  RustCantoDsp? _rustDsp;

  // Last values sent to the engine; unchanged ones are not resent.
  String? _sentKey;
  String? _sentMode;
  double? _sentCorrect;

  RustCantoDsp? get rustDsp {
    _rustDsp ??= RustCantoDsp.tryOpen(sampleRate: sampleRate);
    return _rustDsp;
  }

  bool get hasRustDsp => rustDsp?.isLoaded ?? false;

  /// True once the native output stream is driving the engine; the UI must
  /// then not render blocks itself.
  bool get nativeOutput => _native == true;

  /// Glitch counter (native path only), for diagnostics.
  int get glitches => nativeOutput ? (rustDsp?.outputGlitches ?? 0) : 0;

  void ensureStarted() {
    _rustDsp ??= RustCantoDsp.tryOpen(sampleRate: sampleRate);
  }

  /// Makes sure audio is flowing (playback or audition is about to start).
  void activateOutput() {
    ensureStarted();
    _idleTimer?.cancel();
    _idleTimer = null;
    final dsp = rustDsp;
    if (_native != false && Platform.isAndroid && dsp != null && dsp.isLoaded) {
      _native = dsp.outputStart();
      if (_native!) return;
    }
    _native = false;
    if (!_fallbackStarted) {
      _fallbackStarted = true;
      _invoke('start');
    }
  }

  /// Lets the native stream pause shortly after audio stops, so an audition
  /// tail can finish and the device isn't kept awake while idle.
  void _scheduleIdle() {
    if (!nativeOutput) return;
    _idleTimer?.cancel();
    _idleTimer = Timer(_idleGrace, () {
      _idleTimer = null;
      final dsp = rustDsp;
      if (dsp == null) return;
      // Never pause under a playing transport or a preview still finishing.
      if (dsp.isPlaying || dsp.isAuditioning) {
        _scheduleIdle();
      } else {
        dsp.outputPause();
      }
    });
  }

  /// Whether a note preview is still sounding (it outlives the press).
  bool get isAuditioning => rustDsp?.isAuditioning ?? false;

  Future<PickedAudioFile?> pickAudioFile() async {
    if (Platform.isAndroid) {
      try {
        final res = await _channel.invokeMethod<Map<Object?, Object?>>(
          'pickAudioFile',
        );
        if (res == null) return null;
        final path = res['path'] as String?;
        final name = res['name'] as String?;
        if (path == null || path.isEmpty) return null;
        return PickedAudioFile(
          path: path,
          name: (name != null && name.isNotEmpty)
              ? name
              : path.split(RegExp(r'[/\\]')).last,
        );
      } catch (_) {
        return null;
      }
    }

    if (Platform.isWindows) {
      try {
        final proc = await Process.run('powershell', [
          '-NoProfile',
          '-Command',
          'Add-Type -AssemblyName System.Windows.Forms; '
              '\$d = New-Object System.Windows.Forms.OpenFileDialog; '
              '\$d.Filter = "Audio Files (*.wav;*.mp3;*.flac;*.ogg;*.m4a;*.aac;*.aiff)|*.wav;*.mp3;*.flac;*.ogg;*.m4a;*.aac;*.aiff|All Files (*.*)|*.*"; '
              'if (\$d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { Write-Output \$d.FileName }',
        ]);
        final path = proc.stdout.toString().trim();
        if (path.isNotEmpty && File(path).existsSync()) {
          return PickedAudioFile(
            path: path,
            name: path.split(RegExp(r'[/\\]')).last,
          );
        }
      } catch (_) {}
    }

    return null;
  }

  /// Starts decoding and analysing [filePath] on a native worker thread so
  /// the UI keeps running (see [NativeLoadJob]). Null when the Rust engine
  /// is unavailable.
  NativeLoadJob? startLoad(String filePath, {String? cachedAnalysis}) {
    ensureStarted();
    final job = rustDsp?.startLoadJob(filePath, cachedAnalysis: cachedAnalysis);
    if (job != null) {
      _invalidateSent();
      if (_fallbackStarted) _invoke('flush');
    }
    return job;
  }

  /// Background re-analysis of the loaded audio.
  NativeLoadJob? startReanalyze() {
    ensureStarted();
    final job = rustDsp?.startReanalyzeJob();
    if (job != null) _invalidateSent();
    return job;
  }

  /// Decodes and analyses [filePath]. With [cachedAnalysis] (the result's
  /// JSON from an earlier load of the same file) the analysis is adopted
  /// instead of recomputed; an unusable cache entry falls back to analysis.
  CantoAnalysisResult loadAudioFile(String filePath, {String? cachedAnalysis}) {
    ensureStarted();
    final dsp = rustDsp;
    if (dsp == null || !dsp.isLoaded) {
      return const CantoAnalysisResult(
        ok: false,
        error: 'Rust DSP engine is not available',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: [],
        waveformPeaks: [],
      );
    }
    _invalidateSent();
    if (_fallbackStarted) _invoke('flush');
    if (cachedAnalysis != null) {
      final cached = dsp.loadAudioFileCached(filePath, cachedAnalysis);
      if (cached.ok) return cached;
    }
    return dsp.loadAudioFile(filePath);
  }

  CantoAnalysisResult loadAudioBytes(Uint8List bytes, {String? extHint}) {
    ensureStarted();
    final dsp = rustDsp;
    if (dsp == null || !dsp.isLoaded) {
      return const CantoAnalysisResult(
        ok: false,
        error: 'Rust DSP engine is not available',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: [],
        waveformPeaks: [],
      );
    }
    _invalidateSent();
    if (_fallbackStarted) _invoke('flush');
    return dsp.loadAudioBytes(bytes, extHint: extHint);
  }

  CantoAnalysisResult analyzeLoadedAudio() {
    ensureStarted();
    final dsp = rustDsp;
    if (dsp == null || !dsp.isLoaded) {
      return const CantoAnalysisResult(
        ok: false,
        error: 'Rust DSP engine is not available',
        sampleRate: 48000,
        durationSec: 0.0,
        bpm: 120.0,
        key: 'A',
        mode: 'minor',
        notes: [],
        waveformPeaks: [],
      );
    }
    _invalidateSent();
    return dsp.analyzeLoadedAudio();
  }

  /// Analysis resets key/correction inside the engine, so resend next sync.
  void _invalidateSent() {
    _sentKey = null;
    _sentMode = null;
    _sentCorrect = null;
  }

  /// Full sync of editor state. Key/scale and correction are only resent
  /// when they change (both rewrite every note inside the engine).
  void syncState({
    required List<VocalNote> notes,
    required String keyName,
    required String mode,
    required double correct,
    required bool playing,
    required bool loop,
    required bool bypass,
  }) {
    final dsp = rustDsp;
    if (dsp == null || !dsp.isLoaded) return;
    if (keyName != _sentKey || mode != _sentMode) {
      dsp.setKeyMode(keyName, mode);
      _sentKey = keyName;
      _sentMode = mode;
      _sentCorrect = null; // set_key_and_scale re-applies correction
    }
    if (correct != _sentCorrect) {
      dsp.setCorrect(correct);
      _sentCorrect = correct;
    }
    dsp.syncNotes(notes);
    dsp.setTransport(playing: playing, loopEnabled: loop, bypass: bypass);
  }

  /// Replaces the engine's note list after a structural edit (cut, join,
  /// undo of either).
  void replaceNotes(List<VocalNote> notes) {
    rustDsp?.replaceNotes(notes);
  }

  /// Syncs a single edited note (drag/slider hot path).
  void syncNote(int index, VocalNote note) {
    rustDsp?.updateNote(index, note);
  }

  void seek(double timeSec) {
    rustDsp?.seek(timeSec);
  }

  /// Press on a note: preview it from its start, looping while held. Queued
  /// before the stream starts so the first callback already carries it.
  void auditionNote(int index) {
    ensureStarted();
    rustDsp?.auditionNote(index);
    activateOutput();
  }

  /// Finger lifted: let the preview finish the note.
  void releaseAudition() {
    rustDsp?.releaseAudition();
    _scheduleIdle();
  }

  /// Cut the preview short (fades out).
  void stopAudition() {
    rustDsp?.stopAudition();
    _scheduleIdle();
  }

  void silence() {
    rustDsp?.stopAudition();
    if (nativeOutput) {
      _scheduleIdle();
    } else if (_fallbackStarted) {
      _invoke('flush');
    }
  }

  /// Renders this file with its edits to a WAV at [path] (background isolate).
  /// Returns null on success, otherwise the error.
  Future<String?> exportWav(String path) {
    final dsp = rustDsp;
    if (dsp == null || !dsp.isLoaded) {
      return Future.value('Rust DSP engine is not available');
    }
    return dsp.exportWav(path);
  }

  /// Per-frame transport poll. Native path: reads the playhead/level the
  /// audio thread published (no rendering here). Fallback: renders `frames`
  /// stereo frames and streams them to the Android `AudioTrack`.
  ({double playhead, double level, bool isPlaying}) stepAudio(int frames) {
    ensureStarted();
    final dsp = rustDsp;
    if (dsp == null || !dsp.isLoaded) {
      return (playhead: 0.0, level: 0.0, isPlaying: false);
    }
    if (nativeOutput) {
      return (
        playhead: dsp.playhead,
        level: dsp.level,
        isPlaying: dsp.isPlaying,
      );
    }
    if (frames <= 0) {
      return (playhead: dsp.playhead, level: 0.0, isPlaying: dsp.isPlaying);
    }
    final rendered = dsp.renderBlockPcm16Bytes(frames);
    // Always write, silence included: skipping quiet blocks starved the
    // AudioTrack and let the playhead drift from what was heard.
    if (rendered.pcmBytes.isNotEmpty) {
      _invoke('writePcm', {'pcm': rendered.pcmBytes});
    }
    return (
      playhead: dsp.playhead,
      level: rendered.level,
      isPlaying: dsp.isPlaying,
    );
  }

  void dispose() {
    _idleTimer?.cancel();
    _idleTimer = null;
    _rustDsp?.dispose(); // also stops and closes the native stream
    _rustDsp = null;
    if (_fallbackStarted) _invoke('stop');
    _fallbackStarted = false;
    _native = null;
  }

  void _invoke(String method, [Map<String, dynamic>? args]) {
    try {
      _channel.invokeMethod(method, args).catchError((_) {});
    } catch (_) {
      // Ignore in widget tests or when channel is unavailable
    }
  }
}
