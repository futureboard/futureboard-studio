// End-to-end Flutter + Rust FFI integration tests for Futureboard Studio Mobile.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:futureboard_mobile/engine/studio_controller.dart';
import 'package:futureboard_mobile/main.dart';
import 'package:futureboard_mobile/models/studio_models.dart';

void main() {
  testWidgets(
      'Futureboard Studio Mobile mounts full DAW shell and drives real Rust DSP engine',
      (WidgetTester tester) async {
    tester.view.physicalSize = const Size(1440, 900);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(() {
      tester.view.resetPhysicalSize();
      tester.view.resetDevicePixelRatio();
    });

    final controller = StudioController(startAudioPump: false);
    addTearDown(controller.dispose);

    expect(controller.catalog.length, greaterThanOrEqualTo(25));
    expect(controller.project.tracks.length, equals(6));

    await tester.pumpWidget(
      FutureboardStudioMobileApp(controller: controller),
    );
    await tester.pump();

    expect(find.text('FUTUREBOARD STUDIO'), findsOneWidget);
    expect(find.text('ARRANGEMENT'), findsOneWidget);
    expect(find.text('MASTER BUS'), findsOneWidget);

    // Start playback and step the real Rust audio engine
    controller.togglePlay();
    controller.stepEngineForTest(frames: 2048);
    await tester.pump();

    expect(controller.project.isPlaying, isTrue);
    expect(controller.telemetry.playheadBeats, greaterThan(0.0));

    // Switch bottom dock tabs (Piano Roll -> Plugin Editor -> Perform)
    controller.setDockTab(StudioDockTab.pianoRoll);
    await tester.pump();
    expect(find.textContaining('PIANO ROLL'), findsOneWidget);

    controller.setDockTab(StudioDockTab.pluginEditor);
    await tester.pump();
    expect(find.textContaining('EQ-Z8'), findsWidgets);

    controller.setDockTab(StudioDockTab.perform);
    await tester.pump();
    expect(find.textContaining('LIVE PERFORM'), findsOneWidget);

    // Add a track and test Undo/Redo against Rust engine
    controller.addTrack('Test Lead', TrackKind.instrument,
        instrumentId: 'wrapsynth');
    expect(controller.project.tracks.length, equals(7));
    controller.undo();
    expect(controller.project.tracks.length, equals(6));
  });
}
