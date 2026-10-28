import 'dart:io' show File, Platform, Process;
import 'package:flutter/services.dart';
import 'music.dart';
import 'rust_dsp.dart';

class PickedAudioFile {
  final String path;
  final String name;

  const PickedAudioFile({required this.path, required this.name});
}

class SynthEngine {
  static const MethodChannel _channel = MethodChannel('canto/audio');
  static const double sampleRate = 48000.0;
  bool _started = false;
  RustCantoDsp? _rustDsp;

  RustCantoDsp? get rustDsp {
    _rustDsp ??= RustCantoDsp.tryOpen(sampleRate: sampleRate);
    return _rustDsp;
  }

  bool get hasRustDsp => rustDsp?.isLoaded ?? false;

  void ensureStarted() {
    _rustDsp ??= RustCantoDsp.tryOpen(sampleRate: sampleRate);
    if (_started) return;
    _started = true;
    _invoke('start');
  }

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

  CantoAnalysisResult loadAudioFile(String filePath) {
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
    _invoke('flush');
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
    _invoke('flush');
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
    return dsp.analyzeLoadedAudio();
  }

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
    dsp.setKeyMode(keyName, mode);
    dsp.setCorrect(correct);
    dsp.syncNotes(notes);
    dsp.setTransport(playing: playing, loopEnabled: loop, bypass: bypass);
  }

  void seek(double timeSec) {
    rustDsp?.seek(timeSec);
  }

  void auditionNote(int index) {
    ensureStarted();
    rustDsp?.auditionNote(index);
  }

  void stopAudition() {
    rustDsp?.stopAudition();
  }

  void silence() {
    rustDsp?.stopAudition();
    _invoke('flush');
  }

  /// Renders `frames` stereo frames in the Rust TD-PSOLA engine and streams
  /// the resulting 16-bit PCM to the native Android `AudioTrack`.
  ({double playhead, double level, bool isPlaying}) stepAudio(int frames) {
    ensureStarted();
    final dsp = rustDsp;
    if (dsp == null || !dsp.isLoaded || frames <= 0) {
      return (playhead: 0.0, level: 0.0, isPlaying: false);
    }
    final rendered = dsp.renderBlockPcm16Bytes(frames);
    if (rendered.pcmBytes.isNotEmpty && rendered.level > 0.0002) {
      _invoke('writePcm', {'pcm': rendered.pcmBytes});
    }
    return (
      playhead: dsp.playhead,
      level: rendered.level,
      isPlaying: dsp.isPlaying,
    );
  }

  void dispose() {
    _rustDsp?.dispose();
    _rustDsp = null;
    _invoke('stop');
    _started = false;
  }

  void _invoke(String method, [Map<String, dynamic>? args]) {
    try {
      _channel.invokeMethod(method, args).catchError((_) {});
    } catch (_) {
      // Ignore in widget tests or when channel is unavailable
    }
  }
}
