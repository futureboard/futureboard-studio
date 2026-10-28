import 'dart:convert';
import 'dart:io';
import 'dart:math' as math;
import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import 'package:solfege_voxtune/locale_controller.dart';
import 'package:solfege_voxtune/main.dart';
import 'package:solfege_voxtune/music.dart';
import 'package:solfege_voxtune/pages/settings_page.dart';
import 'package:solfege_voxtune/project.dart';
import 'package:solfege_voxtune/recorder.dart';
import 'package:solfege_voxtune/rust_dsp.dart';
import 'package:solfege_voxtune/ui/components.dart';

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
      sample =
          0.45 * math.sin(2 * math.pi * f0 * t) +
          0.18 * math.sin(2 * math.pi * f0 * 2 * t) +
          0.08 * math.sin(2 * math.pi * f0 * 3 * t);
    } else if (t >= 1.05 && t <= 1.85) {
      const f0 = 261.63; // C4
      sample =
          0.45 * math.sin(2 * math.pi * f0 * t) +
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

  /// A projects folder in the system temp dir, so tests never touch the
  /// real Documents/Solfege Voxtune.
  ProjectStore tempStore() =>
      ProjectStore(Directory.systemTemp.createTempSync('voxtune_test').path);

  /// Files load on a native worker thread: let real time pass (runAsync)
  /// and fake time tick the UI's 40 ms poll until the load is finished.
  Future<void> settle(WidgetTester tester) async {
    final editor = find.byType(CantoScreen);
    for (var i = 0; i < 600; i++) {
      if (editor.evaluate().isEmpty) return;
      final state = tester.state<CantoScreenState>(editor);
      if (!state.isLoadingForTest) break;
      await tester.runAsync(
        () => Future<void>.delayed(const Duration(milliseconds: 5)),
      );
      await tester.pump(const Duration(milliseconds: 40));
    }
    await tester.pump();
    final state = tester.state<CantoScreenState>(editor);
    expect(state.isLoadingForTest, isFalse, reason: 'load never finished');
  }

  Future<CantoScreenState> pumpEditor(
    WidgetTester tester,
    ProjectStore store,
    Project project, {
    LocaleController? locale,
  }) async {
    await tester.pumpWidget(
      CantoApp(
        store: store,
        localeController: locale,
        home: CantoScreen(project: project, store: store),
      ),
    );
    await tester.pump(); // post-frame: opens the first file, if any
    await settle(tester);
    return tester.state<CantoScreenState>(find.byType(CantoScreen));
  }

  testWidgets('editor loads, analyzes and edits real WAV audio', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final state = await pumpEditor(tester, store, store.create('Session'));

    // Empty project: real empty state, no mock data.
    expect(find.text('Add a vocal take'), findsOneWidget);
    expect(find.text('Session'), findsOneWidget);
    expect(find.byKey(const Key('empty-load-btn')), findsOneWidget);
    expect(find.byKey(const Key('tool-pitch')), findsNothing);

    state.loadAudioBytes(
      _buildTestVocalWav(),
      fileName: 'vocal_test.wav',
      extHint: 'wav',
    );
    await settle(tester);

    expect(state.hasAudio, isTrue);
    expect(state.notes.length, 2);
    expect(find.text('vocal_test.wav'), findsOneWidget);
    expect(find.text('A3'), findsOneWidget);
    for (final id in ['pitch', 'drift', 'time', 'formant', 'level']) {
      expect(find.byKey(Key('tool-$id')), findsOneWidget);
    }
    expect(find.byIcon(LucideIcons.moveVertical), findsOneWidget);

    // Off-key pill jumps to the detuned note; Snap tunes it; undo reverts.
    expect(find.text('1 off'), findsOneWidget);
    await tester.tap(find.byKey(const Key('next-off-btn')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('tune-btn')));
    await tester.pump();
    expect(find.text('0¢'), findsWidgets);
    expect(find.byKey(const Key('revert-btn')), findsOneWidget);
    expect(find.byKey(const Key('in-key-status')), findsOneWidget);
    await tester.tap(find.byKey(const Key('undo-btn')));
    await tester.pump();
    expect(find.text('1 off'), findsOneWidget);

    // Key sheet changes the scale through real editor state.
    await tester.tap(find.byKey(const Key('key-btn')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('mode-major')));
    await tester.pump();
    expect(find.text('A major'), findsOneWidget);
  });

  testWidgets('edits are saved to the project and restored on reopen', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Persist');
    final state = await pumpEditor(tester, store, project);
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    await tester.tap(find.byKey(const Key('next-off-btn')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('tune-btn')));
    await tester.pump(const Duration(seconds: 1)); // autosave debounce

    // On disk: the audio copy and the edit.
    final json = jsonDecode(
      File('${project.dir}/project.json').readAsStringSync(),
    ) as Map<String, dynamic>;
    final files = json['files'] as List<dynamic>;
    expect(files, hasLength(1));
    expect(File('${project.dir}/Audio/take.wav').existsSync(), isTrue);
    final saved = (files.first as Map<String, dynamic>)['notes'] as List;
    expect(saved.any((n) => (n as Map)['pinned'] == true), isTrue);

    // Reopen from disk in a fresh editor: the tuned note is restored.
    await tester.pumpWidget(const SizedBox());
    final reopened = store.list().single;
    await pumpEditor(tester, store, reopened);
    expect(find.text('take.wav'), findsOneWidget);
    expect(find.byKey(const Key('in-key-status')), findsOneWidget);
  });

  testWidgets('several files per project, each keeps its own edits', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final state = await pumpEditor(tester, store, store.create('Multi'));
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'verse.wav');
    await settle(tester);
    await tester.tap(find.byKey(const Key('next-off-btn')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('tune-btn')));
    await tester.pump();
    expect(find.byKey(const Key('in-key-status')), findsOneWidget);

    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'chorus.wav');
    await settle(tester);
    expect(find.text('chorus.wav'), findsOneWidget);
    expect(find.text('1 off'), findsOneWidget); // untouched second take

    // Sidebar lists both; switching back restores the first file's edit.
    await tester.tap(find.byKey(const Key('menu-btn')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('sidebar')), findsOneWidget);
    expect(find.text('verse.wav'), findsOneWidget);
    await tester.tap(find.byKey(const Key('sidebar-file-f1')));
    await tester.pumpAndSettle();
    expect(find.text('verse.wav'), findsOneWidget);
    expect(find.byKey(const Key('in-key-status')), findsOneWidget);
    expect(state.project.files.map((f) => f.name), ['verse.wav', 'chorus.wav']);
  });

  testWidgets('export renders the edited file to the Exports folder', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Export');
    final state = await pumpEditor(tester, store, project);
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'lead.wav');
    await settle(tester);

    await tester.runAsync(
      () => state.exportSessions(List.of(state.sessionsForTest)),
    );
    await tester.pump();
    final out = File('${project.dir}/Exports/lead (tuned).wav');
    expect(out.existsSync(), isTrue);
    final bytes = out.readAsBytesSync();
    expect(String.fromCharCodes(bytes.sublist(0, 4)), 'RIFF');
    // 2 s of 48 kHz 16-bit mono after the 44-byte header.
    expect(bytes.length, 44 + 2 * 48000 * 2);
  });

  testWidgets('home lists projects; new project opens the editor', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    await tester.pumpWidget(CantoApp(store: store));
    await tester.pumpAndSettle();
    expect(find.text('Solfege Voxtune'), findsOneWidget);
    expect(find.text('No projects yet'), findsOneWidget);

    await tester.tap(find.byKey(const Key('new-project')));
    await tester.pumpAndSettle();
    await tester.enterText(
      find.byKey(const Key('project-name-field')),
      'Demo song',
    );
    await tester.tap(find.byKey(const Key('confirm-create')));
    await tester.pumpAndSettle();
    expect(find.byType(CantoScreen), findsOneWidget);
    expect(find.text('Demo song'), findsOneWidget);
    expect(Directory('${store.root}/Demo song').existsSync(), isTrue);

    // Back to Home through the sidebar: the project is listed.
    await tester.tap(find.byKey(const Key('menu-btn')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('sidebar-projects')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('project-Demo song')), findsOneWidget);

    // Delete asks first, then removes the folder.
    await tester.tap(find.byKey(const Key('delete-Demo song')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-delete')));
    await tester.pumpAndSettle();
    expect(find.text('No projects yet'), findsOneWidget);
    expect(Directory('${store.root}/Demo song').existsSync(), isFalse);
  });

  testWidgets('UI is translated to Thai; language switches in Settings', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final locale = LocaleController()..value = const Locale('th');
    final state = await pumpEditor(
      tester,
      store,
      store.create('ไทย'),
      locale: locale,
    );
    expect(find.text('เพิ่มไฟล์เสียงร้อง'), findsOneWidget);
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    expect(find.text('เพี้ยน 1'), findsOneWidget);
    expect(find.text('A ไมเนอร์'), findsOneWidget);
    expect(find.text('สแนป'), findsOneWidget);

    await tester.tap(find.byKey(const Key('menu-btn')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('sidebar-settings')));
    await tester.pumpAndSettle();
    expect(find.text('ตั้งค่า'), findsOneWidget);
    expect(find.text(store.root), findsOneWidget);
    await tester.tap(find.byKey(const Key('lang-en')));
    await tester.pumpAndSettle();
    expect(locale.value, const Locale('en'));
    expect(find.text('Settings'), findsOneWidget);
    await tester.tap(find.byKey(const Key('lang-system')));
    await tester.pumpAndSettle();
    expect(locale.value, isNull);
  });
  testWidgets('record a take: live tuner, stop, take opens analysed', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Live');
    final mic = _SimulatedMic(_pcm(_buildTestVocalWav()));
    await tester.pumpWidget(
      CantoApp(
        store: store,
        home: CantoScreen(
          project: project,
          store: store,
          recorderFactory: () => mic,
        ),
      ),
    );
    await tester.pump();

    // Empty project offers Record first.
    await tester.tap(find.byKey(const Key('empty-record-btn')));
    await tester.pumpAndSettle();
    expect(find.text('Tap to start recording'), findsOneWidget);

    await tester.tap(find.byKey(const Key('record-btn')));
    await tester.pump();
    // ~0.6 s of singing: the tuner shows the detuned A3 live.
    for (var i = 0; i < 36; i++) {
      await tester.pump(const Duration(milliseconds: 16));
    }
    expect(
      find.text('Tap to stop — the take opens in the editor'),
      findsOneWidget,
    );
    expect(find.text('A3'), findsOneWidget);
    // Sing the rest of the phrase, then stop.
    for (var i = 0; i < 100; i++) {
      await tester.pump(const Duration(milliseconds: 16));
    }
    await tester.tap(find.byKey(const Key('record-btn')));
    await tester.pumpAndSettle();
    await settle(tester);

    // Back in the editor with the take added, saved and analysed.
    final state = tester.state<CantoScreenState>(find.byType(CantoScreen));
    expect(find.text('Take 1.wav'), findsOneWidget);
    expect(File('${project.dir}/Audio/Take 1.wav').existsSync(), isTrue);
    expect(project.files.single.name, 'Take 1.wav');
    expect(state.notes.length, 2);
    expect(find.text('1 off'), findsOneWidget);
  });

  testWidgets('recording asks for the mic and can be cancelled', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Denied');
    final mic = _SimulatedMic(_pcm(_buildTestVocalWav()), granted: false);
    await tester.pumpWidget(
      CantoApp(
        store: store,
        home: CantoScreen(
          project: project,
          store: store,
          recorderFactory: () => mic,
        ),
      ),
    );
    await tester.pump();
    await tester.tap(find.byKey(const Key('record-open')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('record-btn')));
    await tester.pumpAndSettle();
    expect(find.text('Microphone access is needed to record.'), findsOneWidget);
    expect(find.byKey(const Key('record-allow-mic')), findsOneWidget);

    // Granted on retry, then closed: nothing is added to the project.
    mic.granted = true;
    await tester.tap(find.byKey(const Key('record-allow-mic')));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 16));
    await tester.tap(find.byKey(const Key('record-close')));
    await tester.pumpAndSettle();
    expect(find.byType(CantoScreen), findsOneWidget);
    expect(project.files, isEmpty);
    expect(Directory('${project.dir}/Audio').listSync(), isEmpty);
  });
  test('cutting a note keeps its audio span and re-centres each half', () {
    final note = VocalNote(
      id: 'n1',
      lyric: 'do',
      unpitched: false,
      detected: 60,
      pitch: 60.5, // user moved it up 50 cents
      detectedStart: 1.0,
      start: 1.2, // and later in time
      detectedDur: 1.0,
      dur: 1.0,
      gainDb: -3,
      detectedGain: -3,
      // A slide from -50 to +50 cents across the note.
      contour: [for (var k = 0; k < 32; k++) -50 + 100 * k / 31],
      envelope: List.filled(32, 0.8),
    );
    var seq = 0;
    final halves = splitNote(note, 1.7, (b) => '$b.${++seq}')!;
    final (a, b) = halves;
    expect(a.start, closeTo(1.2, 1e-9));
    expect(a.dur + b.dur, closeTo(1.0, 1e-9));
    expect(b.start, closeTo(1.7, 1e-9));
    // Each half plays its own part of the source.
    expect(a.detectedStart, closeTo(1.0, 1e-9));
    expect(b.detectedStart, closeTo(1.5, 1e-9));
    // Re-centred: low half sits below, high half above; the edit carries.
    expect(a.detected, lessThan(60));
    expect(b.detected, greaterThan(60));
    expect(a.pitch - a.detected, closeTo(0.5, 1e-9));
    expect(b.pitch - b.detected, closeTo(0.5, 1e-9));
    expect(a.id, isNot(b.id));
    // Too close to an edge: refused.
    expect(splitNote(note, 1.21, (b) => b), isNull);

    // Joining the halves gives back the original centre.
    final joined = joinNotes(a, b, 'j');
    expect(joined.start, closeTo(1.2, 1e-9));
    expect(joined.dur, closeTo(1.0, 1e-9));
    expect(joined.detected, closeTo(60, 0.02));
    expect(joined.pitch, closeTo(60.5, 0.02));
  });

  testWidgets('cut, join, mute and reset a note, all undoable', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final state = await pumpEditor(tester, store, store.create('Cut'));
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    expect(state.notes.length, 2);
    final first = state.notes.first.id;

    // Selecting a note shows its toolbar; Cut enters cut mode.
    expect(find.byKey(const Key('note-actions')), findsOneWidget);
    await tester.tap(find.byKey(const Key('note-cut')));
    await tester.pump();
    expect(find.byKey(const Key('cut-bar')), findsOneWidget);
    expect(find.text('Drag across a note, lift to cut'), findsOneWidget);

    // Drag across the note; lifting cuts it in two.
    final center = state.noteCenterForTest(first)!;
    final gesture = await tester.startGesture(center - const Offset(12, 0));
    await tester.pump();
    await gesture.moveBy(const Offset(12, 0));
    await tester.pump();
    await gesture.up();
    await tester.pump();
    expect(state.notes.length, 3);
    final halves = state.notes.take(2).toList();
    expect(halves[0].start + halves[0].dur, closeTo(halves[1].start, 1e-6));

    await tester.tap(find.byKey(const Key('cut-done')));
    await tester.pump();
    expect(find.byKey(const Key('note-actions')), findsOneWidget);

    // Undo restores the uncut note; redo cuts again.
    await tester.tap(find.byKey(const Key('undo-btn')));
    await tester.pump();
    expect(state.notes.length, 2);
    await tester.tap(find.byKey(const Key('redo-btn')));
    await tester.pump();
    expect(state.notes.length, 3);

    // Join the two halves back into one note (select the first half).
    final firstHalf = state.notes.first.id;
    final at = state.noteCenterForTest(firstHalf)!;
    await tester.tapAt(at);
    await tester.pump(const Duration(milliseconds: 400));
    await tester.tap(find.byKey(const Key('note-join')));
    await tester.pump();
    expect(state.notes.length, 2);

    // Mute: drops out of the off-key count and says so.
    expect(find.text('1 off'), findsOneWidget);
    await tester.tap(find.byKey(const Key('note-mute')));
    await tester.pump();
    expect(state.notes.first.muted, isTrue);
    expect(find.text('Unmute'), findsOneWidget);
    expect(find.byKey(const Key('in-key-status')), findsOneWidget);

    // Reset returns the note to its analysed state.
    await tester.tap(find.byKey(const Key('note-reset')));
    await tester.pump();
    expect(state.notes.first.muted, isFalse);
    expect(find.text('1 off'), findsOneWidget);
  });

  testWidgets('advanced tools apply macros to selection or all notes', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final state = await pumpEditor(tester, store, store.create('Adv'));
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    final before = [for (final n in state.notes) n.pitch];

    await tester.tap(find.byKey(const Key('advanced-btn')));
    await tester.pumpAndSettle();
    expect(find.text('Advanced tools'), findsOneWidget);

    // Scope defaults to the selected note: centre only that one.
    await tester.tap(find.byKey(const Key('adv-center-apply')));
    await tester.pump();
    expect(find.text('Applied to 1 note'), findsOneWidget);
    expect(find.text('1 off'), findsNothing);

    // Transpose all notes up 2 semitones.
    await tester.tap(find.byKey(const Key('adv-scope-all')));
    await tester.pump();
    await tester.ensureVisible(find.byKey(const Key('adv-transpose-up')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('adv-transpose-up')));
    await tester.tap(find.byKey(const Key('adv-transpose-up')));
    await tester.pump();
    expect(find.text('+2 st'), findsOneWidget);
    await tester.ensureVisible(find.byKey(const Key('adv-transpose-apply')));
    await tester.tap(find.byKey(const Key('adv-transpose-apply')));
    await tester.pump();
    expect(find.text('Applied to 2 notes'), findsOneWidget);
    expect(state.notes[1].pitch, closeTo(before[1] + 2, 1e-6));

    // Each macro is one undo step.
    Navigator.of(tester.element(find.byKey(const Key('advanced-sheet')))).pop();
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('undo-btn')));
    await tester.pump();
    expect(state.notes[1].pitch, closeTo(before[1], 1e-6));
  });

  testWidgets('cut notes are saved and restored with the project', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Keep cuts');
    final state = await pumpEditor(tester, store, project);
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    await tester.tap(find.byKey(const Key('note-cut')));
    await tester.pump();
    final center = state.noteCenterForTest(state.notes.first.id)!;
    final gesture = await tester.startGesture(center);
    await tester.pump();
    await gesture.up();
    await tester.pump();
    await tester.tap(find.byKey(const Key('cut-done')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('note-mute')));
    await tester.pump(const Duration(seconds: 1)); // autosave

    await tester.pumpWidget(const SizedBox());
    final reopened = await pumpEditor(tester, store, store.list().single);
    expect(reopened.notes.length, 3);
    expect(reopened.notes.where((n) => n.muted), hasLength(1));
  });
  testWidgets(
    'analysis is cached per file; stale or broken entries re-analyse',
    (WidgetTester tester) async {
      final store = tempStore();
      final project = store.create('Cache');
      final state = await pumpEditor(tester, store, project);
      state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
      await settle(tester);
      final first = state.sessionsForTest.single;
      expect(first.fromCache, isFalse);
      final analysed = [
        for (final n in state.notes) (n.id, n.detected, n.start),
      ];
      final cacheFile = File('${project.dir}/Cache/f1.analysis');
      expect(cacheFile.existsSync(), isTrue);

      Future<CantoScreenState> reopen() async {
        await tester.pumpWidget(const SizedBox());
        return pumpEditor(tester, store, store.list().single);
      }

      // Reopened: adopted from the cache, same notes as the analysis.
      var again = await reopen();
      expect(again.sessionsForTest.single.fromCache, isTrue);
      expect([
        for (final n in again.notes) (n.id, n.detected, n.start),
      ], analysed);
      expect(find.text('1 off'), findsOneWidget);

      // A broken entry falls back to analysing (and repairs the cache).
      final header = cacheFile.readAsStringSync().split('\n').first;
      cacheFile.writeAsStringSync('$header\n{"ok":tru');
      again = await reopen();
      expect(again.sessionsForTest.single.fromCache, isFalse);
      expect(again.notes, hasLength(2));
      again = await reopen();
      expect(again.sessionsForTest.single.fromCache, isTrue);

      // Audio changed on disk: the fingerprint no longer matches.
      File('${project.dir}/Audio/take.wav')
          .setLastModifiedSync(DateTime.now().add(const Duration(minutes: 5)));
      again = await reopen();
      expect(again.sessionsForTest.single.fromCache, isFalse);
      expect(again.notes, hasLength(2));
    },
  );

  testWidgets('undo history survives closing the project and jumps anywhere', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('History');
    final state = await pumpEditor(tester, store, project);
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);

    // Two edits: snap the off-key note, then mute it.
    await tester.tap(find.byKey(const Key('next-off-btn')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('tune-btn')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('note-mute')));
    await tester.pump(const Duration(seconds: 4)); // autosave + history
    expect(File('${project.dir}/Cache/f1.history.json').existsSync(), isTrue);

    // Close and reopen: both steps are still undoable, labelled.
    await tester.pumpWidget(const SizedBox());
    final reopened = await pumpEditor(tester, store, store.list().single);
    expect(reopened.notes.where((n) => n.muted), hasLength(1));
    await tester.tap(find.byKey(const Key('undo-btn')));
    await tester.pump();
    expect(find.text('Undo · Mute note'), findsOneWidget);
    expect(reopened.notes.where((n) => n.muted), isEmpty);
    await tester.pump(const Duration(seconds: 2)); // toast fades

    // History sheet (long-press undo): start, snap, (undone) mute.
    await tester.longPress(find.byKey(const Key('undo-btn')));
    await tester.pumpAndSettle();
    final sheet = find.byKey(const Key('history-sheet'));
    expect(sheet, findsOneWidget);
    Finder inSheet(Finder f) => find.descendant(of: sheet, matching: f);
    expect(inSheet(find.text('Start')), findsOneWidget);
    expect(inSheet(find.text('Snap note')), findsOneWidget);
    expect(inSheet(find.text('Mute note')), findsOneWidget);
    expect(inSheet(find.text('Now')), findsOneWidget);
    // Jump to the start: every edit is undone at once…
    await tester.tap(inSheet(find.byKey(const Key('history-row-0'))));
    await tester.pump();
    expect(find.text('1 off'), findsOneWidget);
    // …and the last step is still reachable by redo.
    await tester.tap(inSheet(find.byKey(const Key('history-row-2'))));
    await tester.pump();
    expect(reopened.notes.where((n) => n.muted), hasLength(1));
    Navigator.of(tester.element(find.byKey(const Key('history-sheet')))).pop();
    await tester.pumpAndSettle();

    // A project saved without its history (edited elsewhere): the stale
    // history is dropped instead of undoing into the wrong state.
    await tester.pump(const Duration(seconds: 4));
    await tester.pumpWidget(const SizedBox());
    final file = File('${project.dir}/project.json');
    final json = jsonDecode(file.readAsStringSync()) as Map<String, dynamic>;
    final notes = (json['files'] as List).first['notes'] as List;
    (notes.first as Map)['pitch'] = ((notes.first as Map)['pitch'] as num) + 1;
    file.writeAsStringSync(jsonEncode(json));
    final stale = await pumpEditor(tester, store, store.list().single);
    expect(stale.sessionsForTest.single.undoStack, isEmpty);
    expect(stale.sessionsForTest.single.redoStack, isEmpty);
  });

  testWidgets('advanced tools sheet has its own undo and redo', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final state = await pumpEditor(tester, store, store.create('AdvUndo'));
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    final before = state.notes.first.pitch;
    await tester.tap(find.byKey(const Key('advanced-btn')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('adv-center-apply')));
    await tester.pump();
    expect(state.notes.first.pitch, isNot(closeTo(before, 1e-6)));
    await tester.tap(find.byKey(const Key('adv-undo')));
    await tester.pump();
    expect(state.notes.first.pitch, closeTo(before, 1e-6));
    await tester.tap(find.byKey(const Key('adv-redo')));
    await tester.pump();
    expect(state.notes.first.pitch, isNot(closeTo(before, 1e-6)));
  });

  testWidgets('editor fits phones, landscape and tablets (EN and TH)', (
    WidgetTester tester,
  ) async {
    addTearDown(tester.view.reset);
    tester.view.devicePixelRatio = 1.0;
    const sizes = {
      'phone 360': Size(360, 780),
      'phone 412': Size(412, 915),
      'phone landscape 780': Size(780, 360),
      'phone landscape 915': Size(915, 412),
      'tablet portrait': Size(800, 1280),
      'tablet landscape': Size(1280, 800),
    };
    for (final lang in ['en', 'th']) {
      for (final MapEntry(key: name, value: size) in sizes.entries) {
        tester.view.physicalSize = size;
        final store = tempStore();
        final state = await pumpEditor(
          tester,
          store,
          store.create('Layout'),
          locale: LocaleController()..value = Locale(lang),
        );
        state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
        await settle(tester);
        final wide = size.width > size.height;
        final tablet = size.shortestSide >= 600;
        final reason = '$lang $name';

        // Editing works in every layout: select, snap, undo.
        await tester.tap(find.byKey(const Key('next-off-btn')));
        await tester.pump();
        await tester.tap(find.byKey(const Key('tune-btn')));
        await tester.pump();
        expect(
          find.byKey(const Key('in-key-status')),
          findsOneWidget,
          reason: reason,
        );
        await tester.tap(find.byKey(const Key('undo-btn')));
        await tester.pump();
        expect(
          find.byKey(const Key('next-off-btn')),
          findsOneWidget,
          reason: reason,
        );

        // Tablets in landscape keep the sidebar and the history list open.
        final pinned = wide && size.width >= 1100;
        expect(
          find.byKey(const Key('sidebar-pinned')),
          pinned ? findsOneWidget : findsNothing,
          reason: reason,
        );
        expect(
          find.byKey(const Key('menu-btn')),
          pinned ? findsNothing : findsOneWidget,
          reason: reason,
        );
        expect(
          find.byKey(const Key('panel-history')),
          wide && tablet ? findsOneWidget : findsNothing,
          reason: reason,
        );

        // Sheets fit too (landscape phones are short).
        await tester.tap(find.byKey(const Key('advanced-btn')));
        await tester.pumpAndSettle();
        expect(
          find.byKey(const Key('advanced-sheet')),
          findsOneWidget,
          reason: reason,
        );
        Navigator.of(tester.element(find.byKey(const Key('advanced-sheet'))))
            .pop();
        await tester.pumpAndSettle();
        await tester.longPress(find.byKey(const Key('undo-btn')));
        await tester.pumpAndSettle();
        expect(
          find.byKey(const Key('history-sheet')),
          findsOneWidget,
          reason: reason,
        );
        Navigator.of(tester.element(find.byKey(const Key('history-sheet'))))
            .pop();
        await tester.pumpAndSettle();
        await tester.pumpWidget(const SizedBox());
      }
    }
  });

  testWidgets('home, settings and recording fit landscape', (
    WidgetTester tester,
  ) async {
    addTearDown(tester.view.reset);
    tester.view.devicePixelRatio = 1.0;
    tester.view.physicalSize = const Size(780, 360);
    final store = tempStore();
    for (final name in ['One', 'Two', 'Three']) {
      store.create(name);
    }
    await tester.pumpWidget(CantoApp(store: store));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('project-One')), findsOneWidget);

    await tester.tap(find.byKey(const Key('home-settings')));
    await tester.pumpAndSettle();
    // Below the fold in landscape: the list scrolls to it.
    await tester.scrollUntilVisible(
      find.byKey(const Key('cache-size')),
      120,
      scrollable: find
          .descendant(
            of: find.byType(SettingsPage),
            matching: find.byType(Scrollable),
          )
          .first,
    );
    expect(find.byKey(const Key('cache-size')), findsOneWidget);
    await tester.tap(find.byKey(const Key('settings-back')));
    await tester.pumpAndSettle();

    final project = store.create('Live');
    final mic = _SimulatedMic(_pcm(_buildTestVocalWav()));
    await tester.pumpWidget(
      CantoApp(
        store: store,
        home: CantoScreen(
          project: project,
          store: store,
          recorderFactory: () => mic,
        ),
      ),
    );
    await tester.pump();
    await tester.tap(find.byKey(const Key('record-open')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('record-btn')));
    for (var i = 0; i < 40; i++) {
      await tester.pump(const Duration(milliseconds: 16));
    }
    expect(find.text('A3'), findsOneWidget);
    await tester.tap(find.byKey(const Key('record-btn')));
    await tester.pumpAndSettle();
    await settle(tester);
    expect(project.files, hasLength(1));
  });

  testWidgets('Settings shows the cache size and clears it', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Clear');
    final state = await pumpEditor(tester, store, project);
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    await tester.pumpWidget(const SizedBox());
    expect(Directory('${project.dir}/Cache').existsSync(), isTrue);

    await tester.pumpWidget(CantoApp(store: store));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('home-settings')));
    await tester.pumpAndSettle();
    expect(find.textContaining('KB used'), findsOneWidget);
    await tester.tap(find.byKey(const Key('cache-clear')));
    await tester.pump();
    expect(find.text('0 B used'), findsOneWidget);
    expect(Directory('${project.dir}/Cache').existsSync(), isFalse);
    // The project and its edits are untouched.
    expect(File('${project.dir}/project.json').existsSync(), isTrue);
    expect(File('${project.dir}/Audio/take.wav').existsSync(), isTrue);
  });

  /// The 2 s test take repeated to [seconds], so analysis takes long enough
  /// to watch its progress.
  Uint8List longWav(int seconds) {
    final one = _buildTestVocalWav();
    final pcm = one.sublist(44);
    final reps = (seconds / 2).ceil();
    final out = BytesBuilder()..add(one.sublist(0, 44));
    for (var i = 0; i < reps; i++) {
      out.add(pcm);
    }
    final bytes = out.toBytes();
    final header = ByteData.sublistView(bytes, 0, 44);
    header.setUint32(4, 36 + pcm.length * reps, Endian.little);
    header.setUint32(40, pcm.length * reps, Endian.little);
    return bytes;
  }

  testWidgets('opening a project shows Loading Project with live progress', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Big song');
    store.importBytes(project, longWav(40), 'lead.wav');
    await tester.pumpWidget(
      CantoApp(
        store: store,
        home: CantoScreen(project: project, store: store),
      ),
    );
    expect(find.byKey(const Key('project-loading')), findsOneWidget);
    expect(find.text('LOADING PROJECT'), findsOneWidget);
    expect(find.text('Big song'), findsOneWidget);
    expect(find.text('lead.wav'), findsOneWidget);
    await tester.pump(); // starts the load

    // The UI keeps running while the worker analyses: progress is seen
    // moving through the real analysis, not jumping 0 → done.
    final seen = <int>{};
    var sawFirstOpenHint = false;
    final firstOpenHint = find.text(
      'First time this file is opened, so it is analysed. '
      'Next time it opens instantly.',
    );
    final state = tester.state<CantoScreenState>(find.byType(CantoScreen));
    while (state.isLoadingForTest) {
      await tester.runAsync(
        () => Future<void>.delayed(const Duration(milliseconds: 5)),
      );
      await tester.pump(const Duration(milliseconds: 40));
      final pct = find.byKey(const Key('load-percent'));
      if (pct.evaluate().isNotEmpty) {
        seen.add(int.parse(tester.widget<Text>(pct).data!.replaceAll('%', '')));
      }
      sawFirstOpenHint |= firstOpenHint.evaluate().isNotEmpty;
    }
    expect(seen.where((p) => p > 0 && p < 100), isNotEmpty, reason: '$seen');
    expect(sawFirstOpenHint, isTrue);

    await tester.pump();
    expect(find.byKey(const Key('project-loading')), findsNothing);
    expect(state.notes, isNotEmpty);

    // Reopened: the page says it uses the saved analysis.
    await tester.pumpWidget(const SizedBox());
    await tester.pumpWidget(
      CantoApp(
        store: store,
        home: CantoScreen(project: store.list().single, store: store),
      ),
    );
    await tester.pump();
    expect(
      find.text('Using the saved analysis, so this is quick.'),
      findsOneWidget,
    );
    await settle(tester);
    expect(
      tester
          .state<CantoScreenState>(find.byType(CantoScreen))
          .sessionsForTest
          .single
          .fromCache,
      isTrue,
    );
  });

  testWidgets('cancelling Loading Project goes back to the projects list', (
    WidgetTester tester,
  ) async {
    // On a landscape phone, so Cancel must be reachable without scrolling.
    addTearDown(tester.view.reset);
    tester.view.devicePixelRatio = 1.0;
    tester.view.physicalSize = const Size(780, 360);
    final store = tempStore();
    final project = store.create('Long');
    store.importBytes(project, longWav(120), 'lead.wav');
    await tester.pumpWidget(CantoApp(store: store));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('project-Long')));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 400));
    expect(find.byKey(const Key('project-loading')), findsOneWidget);
    await tester.tap(find.byKey(const Key('load-cancel')));
    await tester.pumpAndSettle();
    expect(find.byType(CantoScreen), findsNothing);
    expect(find.byKey(const Key('new-project')), findsOneWidget);
    // The worker stops on its own; nothing was cached for the cancelled load.
    await tester.runAsync(
      () => Future<void>.delayed(const Duration(milliseconds: 300)),
    );
    expect(File('${project.dir}/Cache/f1.analysis').existsSync(), isFalse);
  });

  testWidgets('file loads show progress over the editor and can be cancelled', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final state = await pumpEditor(tester, store, store.create('Files'));

    // First file of an empty project, cancelled: it stays unopened and
    // offers to open it again.
    state.loadAudioBytes(longWav(60), fileName: 'long.wav');
    await tester.pump();
    expect(find.byKey(const Key('file-loading')), findsOneWidget);
    expect(find.text('Opening file'), findsOneWidget);
    await tester.tap(find.byKey(const Key('load-cancel')));
    await tester.pump();
    expect(find.text('Cancelling…'), findsOneWidget);
    await settle(tester);
    expect(find.byKey(const Key('file-loading')), findsNothing);
    expect(state.hasAudio, isFalse);
    expect(find.byKey(const Key('open-file-btn')), findsOneWidget);
    await tester.tap(find.byKey(const Key('open-file-btn')));
    await tester.pump();
    await settle(tester);
    expect(state.hasAudio, isTrue);

    // Second file, cancelled: back to the file that was open.
    state.loadAudioBytes(longWav(60), fileName: 'other.wav');
    await tester.pump();
    await tester.tap(find.byKey(const Key('load-cancel')));
    await settle(tester);
    expect(find.text('long.wav'), findsOneWidget);
    expect(state.hasAudio, isTrue);

    // Re-analysing shows the same progress and rewrites the cache.
    await tester.tap(find.byKey(const Key('session-title')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('analyze-btn')));
    await tester.pump();
    expect(find.text('Analysing again'), findsOneWidget);
    await settle(tester);
    expect(state.notes, isNotEmpty);
  });

  test('ornament shapes, saving and cut/join keep ลูกคอ and accent', () {
    VocalNote note({int style = kOrnNone, double accent = 0}) => VocalNote(
      id: 'n1',
      lyric: 'do',
      unpitched: false,
      detected: 60,
      pitch: 60,
      detectedStart: 1,
      start: 1,
      detectedDur: 0.8,
      dur: 0.8,
      gainDb: 0,
      detectedGain: 0,
      ornStyle: style,
      ornDepth: 2,
      ornSpeed: 0.5,
      accent: accent,
    );
    List<double> curve(VocalNote n) => [
      for (var k = 0; k <= 400; k++) ornamentSemis(n, k / 400),
    ];
    final up = curve(note(style: kOrnTurnUp));
    expect(up.reduce(math.max), closeTo(2, 0.05));
    expect(up.last, 0);
    expect(
      curve(note(style: kOrnTurnDown)).reduce(math.min),
      closeTo(-2, 0.05),
    );
    expect(curve(note(style: kOrnFall)).last, closeTo(-2, 1e-3));
    expect(curve(note()).every((v) => v == 0), isTrue);
    // The drawn pitch includes it.
    expect(pitchAt(note(style: kOrnFall), 1), closeTo(58, 1e-3));
    // Bypass shows the original.
    final shown = presentNote(note(style: kOrnShake, accent: 1), true);
    expect((shown.ornStyle, shown.accent), (kOrnNone, 0.0));

    // Saved and loaded; projects from before have none.
    final saved = VocalNote.fromJson(
      note(style: kOrnShake, accent: 0.5).toJson(),
    );
    expect(
      (saved.ornStyle, saved.ornDepth, saved.accent),
      (kOrnShake, 2.0, 0.5),
    );
    final old = note().toJson()
      ..remove('orn_style')
      ..remove('accent');
    expect(VocalNote.fromJson(old).hasExpression, isFalse);

    // Cut: the onset flick and accent stay at the start, a fall at the end.
    var seq = 0;
    String id(String b) => '$b.${++seq}';
    final (a, b) = splitNote(note(style: kOrnTurnUp, accent: 1), 1.4, id)!;
    expect((a.ornStyle, a.accent), (kOrnTurnUp, 1.0));
    expect((b.ornStyle, b.accent), (kOrnNone, 0.0));
    final (c, d) = splitNote(note(style: kOrnFall), 1.4, id)!;
    expect((c.ornStyle, d.ornStyle), (kOrnNone, kOrnFall));
    // Join: starts like the first note, ends like the second.
    expect(joinNotes(a, d, 'j').ornStyle, kOrnFall);
    expect(joinNotes(a, b, 'j').accent, 1.0);
  });

  testWidgets('ลูกคอ and accent: sheet, sound, undo and save', (
    WidgetTester tester,
  ) async {
    final store = tempStore();
    final project = store.create('Orn');
    final state = await pumpEditor(tester, store, project);
    state.loadAudioBytes(_buildTestVocalWav(), fileName: 'take.wav');
    await settle(tester);
    // Rendered through the file's engine (what Export writes).
    final out = File('${project.dir}/render.wav');
    Future<List<int>> render() async {
      await tester.runAsync(
        () => state.sessionsForTest.single.synth.exportWav(out.path),
      );
      return out.readAsBytesSync();
    }

    final plain = await render();

    // Selected note → toolbar → sheet with the five styles.
    await tester.tap(find.byKey(const Key('note-expression')));
    await tester.pumpAndSettle();
    expect(find.text('Ornament & accent'), findsOneWidget);
    for (final s in ['None', 'Flick up', 'Flick down', 'Shake', 'Fall']) {
      expect(find.text(s), findsOneWidget);
    }
    await tester.tap(find.byKey(Key('orn-$kOrnShake')));
    await tester.pump();
    final note = state.notes.first;
    expect(note.ornStyle, kOrnShake);
    // Accent by dragging its slider.
    await tester.drag(
      find.descendant(
        of: find.byKey(const Key('accent-amount')),
        matching: find.byType(CantoSlider),
      ),
      const Offset(400, 0),
    );
    await tester.pump();
    expect(note.accent, greaterThan(0.5));
    await tester.tap(find.byKey(const Key('expr-preview')));
    await tester.pump();
    Navigator.of(tester.element(find.byKey(const Key('expression-sheet'))))
        .pop();
    await tester.pumpAndSettle();
    expect(find.textContaining('Shake'), findsOneWidget); // note meta line

    // The engine renders it: the export now sounds different.
    expect(await render(), isNot(equals(plain)));

    // Two undo steps (accent, then style), labelled.
    await tester.tap(find.byKey(const Key('undo-btn')));
    await tester.pump();
    expect(find.text('Undo · Accent'), findsOneWidget);
    expect(state.notes.first.accent, 0);
    await tester.tap(find.byKey(const Key('redo-btn')));
    await tester.pump(const Duration(seconds: 1)); // autosave

    // Saved with the project.
    await tester.pumpWidget(const SizedBox());
    final reopened = await pumpEditor(tester, store, store.list().single);
    expect(reopened.notes.first.ornStyle, kOrnShake);
    expect(reopened.notes.first.accent, greaterThan(0.5));
  });

  test('Thai and English ARB files define the same messages', () {
    Set<String> keysOf(String path) {
      final map = jsonDecode(File(path).readAsStringSync()) as Map;
      return map.keys.cast<String>().where((k) => !k.startsWith('@')).toSet();
    }

    final en = keysOf('lib/l10n/app_en.arb');
    final th = keysOf('lib/l10n/app_th.arb');
    expect(th.difference(en), isEmpty, reason: 'keys only in TH');
    expect(en.difference(th), isEmpty, reason: 'untranslated keys');
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

/// 16-bit WAV bytes → float samples.
List<double> _pcm(Uint8List wav) {
  final data = ByteData.sublistView(wav, 44);
  return [
    for (var i = 0; i + 1 < data.lengthInBytes; i += 2)
      data.getInt16(i, Endian.little) / 32768.0,
  ];
}

/// A microphone that "sings" [samples] into the real Rust recorder, one UI
/// frame's worth (800 samples ≈ 16.7 ms at 48 kHz) per poll.
class _SimulatedMic implements LiveRecorder {
  final NativeLiveRecorder native = NativeLiveRecorder.tryOpen()!;
  final List<double> samples;
  bool granted;
  int _pos = 0;
  bool _running = false;

  _SimulatedMic(this.samples, {this.granted = true});

  @override
  Future<bool> ensurePermission() async => granted;

  @override
  String? start() {
    native.feedForTest(const []); // begins a 48 kHz take
    _running = true;
    _pos = 0;
    return null;
  }

  @override
  RecorderStatus poll() {
    if (_running && _pos < samples.length) {
      final end = (_pos + 800).clamp(0, samples.length);
      native.feedForTest(samples.sublist(_pos, end));
      _pos = end;
    }
    return native.poll();
  }

  @override
  void stop() {
    _running = false;
    native.stop();
  }

  @override
  String? save(String path) => native.save(path);

  @override
  void discard() {
    _running = false;
    native.discard();
  }

  @override
  void dispose() => native.dispose();
}
