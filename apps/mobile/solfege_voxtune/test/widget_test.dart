import 'dart:math' as math;
import 'dart:typed_data';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import 'package:solfege_voxtune/main.dart';
import 'package:solfege_voxtune/music.dart';
import 'package:solfege_voxtune/rust_dsp.dart';

/// Builds a valid 16-bit mono RIFF/WAVE byte buffer with two pitched vocal notes:
/// Note 1: detuned A3 (~224.5 Hz, +35 cents) from 0.10s to 0.85s
/// Note 2: C4 (261.63 Hz) from 1.05s to 1.85s
Uint8List _buildTestVocalWav({int sampleRate = 48000}) {
  final numSamples = sampleRate * 2;
  final dataBytes = numSamples * 2;
  final header = ByteData(44 + dataBytes);

  void writeAscii(int offset, String s) {
    for (int i = 0; i < s.length; i++) {
      header.setUint8(offset + i, s.codeUnitAt(i));
    }
  }

  writeAscii(0, 'RIFF');
  header.setUint32(4, 36 + dataBytes, Endian.little);
  writeAscii(8, 'WAVE');
  writeAscii(12, 'fmt ');
  header.setUint32(16, 16, Endian.little);
  header.setUint16(20, 1, Endian.little); // PCM
  header.setUint16(22, 1, Endian.little); // 1 channel
  header.setUint32(24, sampleRate, Endian.little);
  header.setUint32(28, sampleRate * 2, Endian.little);
  header.setUint16(32, 2, Endian.little);
  header.setUint16(34, 16, Endian.little);
  writeAscii(36, 'data');
  header.setUint32(40, dataBytes, Endian.little);

  for (int i = 0; i < numSamples; i++) {
    final t = i / sampleRate;
    double sample = 0.0;
    if (t >= 0.10 && t <= 0.85) {
      const f0 = 224.5; // A3 +35c
      sample = 0.45 * math.sin(2 * math.pi * f0 * t) +
          0.18 * math.sin(2 * math.pi * f0 * 2 * t) +
          0.08 * math.sin(2 * math.pi * f0 * 3 * t);
    } else if (t >= 1.05 && t <= 1.85) {
      const f0 = 261.63; // C4
      sample = 0.45 * math.sin(2 * math.pi * f0 * t) +
          0.18 * math.sin(2 * math.pi * f0 * 2 * t) +
          0.08 * math.sin(2 * math.pi * f0 * 3 * t);
    }
    final pcm16 = (sample.clamp(-1.0, 1.0) * 32767.0).round();
    header.setInt16(44 + i * 2, pcm16, Endian.little);
  }

  return header.buffer.asUint8List();
}

void main() {
  test('A minor scale and spellings', () {
    final pcs = scalePcs('A', 'minor')..sort();
    expect(pcs, [0, 2, 4, 5, 7, 9, 11]);
    expect(spellMidi(69, 'A'), 'A4');
    expect(spellMidi(72, 'A'), 'C5');
    expect(spellMidi(70, 'F'), 'Bb4');
  });

  test('nearest scale target and cents calculation', () {
    expect(nearestScale(72.58, 'A', 'minor'), 72);
    expect(centsFromTarget(72.58, 'A', 'minor').cents, 58);
    expect(nearestScale(66.42, 'A', 'minor'), 67);
    expect(centsFromTarget(66.42, 'A', 'minor').cents, -58);
    expect(nearestScale(67.74, 'A', 'minor'), 67);
    expect(nearestScale(69, 'A', 'major'), 69);
  });

  test('clock and bar formatting', () {
    expect(formatClock(0), '0:00.00');
    expect(formatClock(65.2), '1:05.20');
    expect(formatBars(0, 92), '1.1.1');
    expect(formatBars(60 / 92, 92), '1.2.1');
    expect((midiToHz(69) - 440).abs() < 0.001, isTrue);
  });

  testWidgets('CANTO starts without mockup and loads + analyzes real WAV audio',
      (WidgetTester tester) async {
    await tester.pumpWidget(const CantoApp());

    // Starts with no mockup data loaded
    expect(find.text('CANTO'), findsOneWidget);
    expect(find.text('No audio loaded'), findsWidgets);
    expect(find.byKey(const Key('load-audio-btn')), findsOneWidget);
    expect(find.byKey(const Key('analyze-btn')), findsOneWidget);
    expect(find.byKey(const Key('empty-load-btn')), findsOneWidget);

    // Lucide Icons present
    expect(find.byIcon(LucideIcons.moveVertical), findsOneWidget);
    expect(find.byIcon(LucideIcons.activity), findsWidgets);
    expect(find.byIcon(LucideIcons.moveHorizontal), findsOneWidget);
    expect(find.byIcon(LucideIcons.blend), findsWidgets);
    expect(find.byIcon(LucideIcons.audioLines), findsWidgets);
    expect(find.byIcon(LucideIcons.magnet), findsOneWidget);
    expect(find.byIcon(LucideIcons.play), findsOneWidget);

    // Load real WAV audio into CantoScreenState and verify real YIN analysis
    final state = tester.state<CantoScreenState>(find.byType(CantoScreen));
    final wavBytes = _buildTestVocalWav();
    state.loadAudioBytes(wavBytes, fileName: 'vocal_test.wav', extHint: 'wav');
    await tester.pump();

    expect(state.hasAudio, isTrue);
    expect(state.notes.length, 2);
    expect(find.text('vocal_test.wav'), findsOneWidget);
    expect(find.byKey(const Key('note-name')), findsOneWidget);
    expect(find.text('A3'), findsOneWidget);

    // Tune button snaps the detuned A3 note to 0c
    await tester.tap(find.byKey(const Key('tune-btn')));
    await tester.pump();
    expect(find.text('0¢'), findsWidgets);
    expect(find.text('REVERT'), findsOneWidget);
  });

  test('Rust CantoEngine FFI decodes WAV, analyzes notes, and renders TD-PSOLA audio', () {
    final dsp = RustCantoDsp.tryOpen(sampleRate: 48000.0);
    expect(dsp, isNotNull);
    expect(dsp!.isLoaded, isTrue);

    final wavBytes = _buildTestVocalWav();
    final analysis = dsp.loadAudioBytes(wavBytes, extHint: 'wav');
    expect(analysis.ok, isTrue);
    expect(analysis.notes.length, 2);
    expect((analysis.notes[0].detected - 57.35).abs() < 0.35, isTrue);
    expect((analysis.notes[1].detected - 60.0).abs() < 0.35, isTrue);
    expect(analysis.waveformPeaks.isNotEmpty, isTrue);

    dsp.setTransport(playing: true, loopEnabled: true, bypass: false);
    dsp.seek(0.25);

    double maxLevel = 0.0;
    for (int i = 0; i < 8; i++) {
      final res = dsp.renderBlockI16(512);
      expect(res.pcm16.length, 1024);
      maxLevel = math.max(maxLevel, res.level);
    }
    expect(maxLevel, greaterThan(0.05));
    expect(dsp.playhead, greaterThan(0.25));
    dsp.dispose();
  });
}
