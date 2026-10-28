// Futureboard Studio Mobile App Chrome: Titlebar + Transport Bar (`shell_regions.rs` port).

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class AppChrome extends StatelessWidget {
  final StudioController controller;

  const AppChrome({super.key, required this.controller});

  String _formatBarsBeatsSixteenths(double beats, int beatsPerBar) {
    final bpb = beatsPerBar.clamp(1, 16);
    final totalBeats = beats.max0();
    final bar = (totalBeats / bpb).floor() + 1;
    final beatInBar = (totalBeats % bpb).floor() + 1;
    final sixteenth = ((totalBeats - totalBeats.floor()) * 4.0).floor() + 1;
    return '${bar.toString().padLeft(3, '0')}.${beatInBar.toString()}.${sixteenth.toString()}';
  }

  String _formatTimecode(double beats, double bpm) {
    final seconds = (beats.max0() * 60.0) / (bpm <= 0 ? 120.0 : bpm);
    final mins = (seconds / 60.0).floor();
    final secs = (seconds % 60.0).floor();
    final ms = ((seconds - seconds.floor()) * 100.0).floor();
    return '${mins.toString().padLeft(2, '0')}:${secs.toString().padLeft(2, '0')}.${ms.toString().padLeft(2, '0')}';
  }

  @override
  Widget build(BuildContext context) {
    final c = controller.colors;
    final p = controller.project;
    final tel = controller.telemetry;

    return Container(
      decoration: BoxDecoration(
        color: c.bgPrimary,
        border: Border(
          bottom: BorderSide(color: c.borderDefault, width: 1.0),
        ),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          // ── Row 1: Studio Titlebar + Workspace Presets + Project Actions ──
          Container(
            height: FbSize.titlebarH,
            padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
            decoration: BoxDecoration(
              color: c.bgCanvas,
              border: Border(
                bottom: BorderSide(color: c.borderSubtle, width: 1.0),
              ),
            ),
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(
                children: [
                  // Brand badge + Browser drawer toggle
                  FbIconButton(
                    icon: LucideIcons.panelLeft,
                    tooltip: 'Toggle Browser Drawer',
                    active: controller.isBrowserOpen,
                    colors: c,
                    onTap: controller.toggleBrowser,
                  ),
                  const SizedBox(width: FbSpace.s2),
                  Container(
                    padding: const EdgeInsets.symmetric(
                      horizontal: FbSpace.s2 + 2,
                      vertical: 3,
                    ),
                    decoration: BoxDecoration(
                      color: c.accentSoft,
                      borderRadius: BorderRadius.circular(FbRadius.xs),
                      border: Border.all(
                        color: c.accentPrimary.withValues(alpha: 0.5),
                      ),
                    ),
                    child: Row(
                      children: [
                        Icon(LucideIcons.audioWaveform,
                            size: 13, color: c.accentPrimary),
                        const SizedBox(width: 5),
                        Text(
                          'FUTUREBOARD STUDIO',
                          style: TextStyle(
                            fontFamily: FbTypography.fontSans,
                            fontSize: 10.5,
                            fontWeight: FontWeight.w800,
                            letterSpacing: 0.8,
                            color: c.textPrimary,
                          ),
                        ),
                      ],
                    ),
                  ),
                  const SizedBox(width: FbSpace.s3),
                  Text(
                    p.title,
                    style: FbTypography.title(c),
                  ),
                  const SizedBox(width: FbSpace.s4),

                  // Workspace Layout Presets (Arrange / Mix / Edit / Perform)
                  FbChromeCluster(
                    colors: c,
                    children: [
                      _presetChip(
                        label: 'Arrange',
                        preset: StudioLayoutPreset.arrange,
                        c: c,
                      ),
                      _presetChip(
                        label: 'Mix',
                        preset: StudioLayoutPreset.mix,
                        c: c,
                      ),
                      _presetChip(
                        label: 'Edit',
                        preset: StudioLayoutPreset.edit,
                        c: c,
                      ),
                      _presetChip(
                        label: 'Perform',
                        preset: StudioLayoutPreset.perform,
                        c: c,
                      ),
                    ],
                  ),
                  const SizedBox(width: FbSpace.s3),

                  // Undo / Redo Cluster
                  FbChromeCluster(
                    colors: c,
                    children: [
                      FbIconButton(
                        icon: LucideIcons.undo2,
                        tooltip: 'Undo',
                        colors: c,
                        onTap: p.canUndo ? controller.undo : null,
                      ),
                      FbIconButton(
                        icon: LucideIcons.redo2,
                        tooltip: 'Redo',
                        colors: c,
                        onTap: p.canRedo ? controller.redo : null,
                      ),
                    ],
                  ),
                  const SizedBox(width: FbSpace.s3),

                  // Demo reload + Bounce WAV + Theme + Inspector toggles
                  FbButton(
                    label: 'Demo Project',
                    icon: LucideIcons.sparkles,
                    compact: true,
                    colors: c,
                    onTap: controller.loadDemoProject,
                  ),
                  const SizedBox(width: FbSpace.s2),
                  FbButton(
                    label: 'Bounce WAV',
                    icon: LucideIcons.download,
                    compact: true,
                    colors: c,
                    onTap: () => controller.exportMasterWav(),
                  ),
                  const SizedBox(width: FbSpace.s2),
                  FbIconButton(
                    icon: controller.isDarkTheme
                        ? LucideIcons.sun
                        : LucideIcons.moon,
                    tooltip: 'Switch Dark / Light Studio Theme',
                    colors: c,
                    onTap: controller.toggleTheme,
                  ),
                  FbIconButton(
                    icon: LucideIcons.panelRight,
                    tooltip: 'Toggle Track & Spatial Inspector',
                    active: controller.isInspectorOpen,
                    colors: c,
                    onTap: controller.toggleInspector,
                  ),
                ],
              ),
            ),
          ),

          // ── Row 2: Transport Cluster + LCD Readouts + Master Mini-Meter ──
          Container(
            height: FbSize.transportH,
            padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(
                children: [
                  // Transport Buttons Cluster
                  FbChromeCluster(
                    colors: c,
                    children: [
                      FbIconButton(
                        icon: LucideIcons.skipBack,
                        tooltip: 'Return to Zero (1.1.1)',
                        colors: c,
                        onTap: () => controller.seekBeats(0.0),
                      ),
                      FbIconButton(
                        icon: p.isPlaying ? LucideIcons.pause : LucideIcons.play,
                        tooltip: p.isPlaying ? 'Pause' : 'Play',
                        active: p.isPlaying,
                        activeColor: c.transportPlay,
                        colors: c,
                        onTap: controller.togglePlay,
                      ),
                      FbIconButton(
                        icon: LucideIcons.square,
                        tooltip: 'Stop',
                        colors: c,
                        onTap: controller.stopTransport,
                      ),
                      FbIconButton(
                        icon: LucideIcons.circleDot,
                        tooltip: 'Record Audio Input to Armed Track',
                        active: p.isRecording,
                        activeColor: c.transportRecord,
                        colors: c,
                        onTap: controller.toggleRecord,
                      ),
                      FbIconButton(
                        icon: LucideIcons.repeat,
                        tooltip: 'Loop Arrangement Region',
                        active: p.loopEnabled,
                        activeColor: c.transportLoop,
                        colors: c,
                        onTap: controller.toggleLoop,
                      ),
                      FbIconButton(
                        icon: LucideIcons.bell,
                        tooltip: 'Metronome Click',
                        active: p.metronomeEnabled,
                        activeColor: c.stateSolo,
                        colors: c,
                        onTap: controller.toggleMetronome,
                      ),
                    ],
                  ),
                  const SizedBox(width: FbSpace.s3),

                  // LCD Position & Tempo Readout Cluster (Tabular Monospace)
                  Container(
                    height: 32,
                    padding:
                        const EdgeInsets.symmetric(horizontal: FbSpace.s3),
                    decoration: BoxDecoration(
                      color: c.bgSunken,
                      borderRadius: BorderRadius.circular(FbRadius.md),
                      border: Border.all(color: c.borderDefault),
                    ),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Column(
                          mainAxisAlignment: MainAxisAlignment.center,
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text(
                              _formatBarsBeatsSixteenths(
                                  tel.playheadBeats, p.timeSigNum),
                              style: FbTypography.transportClock(
                                c,
                                color: p.isPlaying
                                    ? c.transportPlay
                                    : c.textPrimary,
                              ),
                            ),
                          ],
                        ),
                        const SizedBox(width: FbSpace.s3),
                        Container(width: 1, height: 18, color: c.borderSubtle),
                        const SizedBox(width: FbSpace.s3),
                        Text(
                          _formatTimecode(tel.playheadBeats, p.bpm),
                          style: FbTypography.monoReadout(
                            c,
                            size: 12.5,
                            color: c.textSecondary,
                          ),
                        ),
                        const SizedBox(width: FbSpace.s3),
                        Container(width: 1, height: 18, color: c.borderSubtle),
                        const SizedBox(width: FbSpace.s3),
                        // Interactive BPM readout (drag vertically or tap +/-)
                        GestureDetector(
                          onVerticalDragUpdate: (d) {
                            controller.setBpm(p.bpm - d.delta.dy * 0.25);
                          },
                          child: Row(
                            children: [
                              InkWell(
                                onTap: () => controller.setBpm(p.bpm - 1.0),
                                child: Icon(LucideIcons.minus,
                                    size: 12, color: c.textMuted),
                              ),
                              const SizedBox(width: 4),
                              Text(
                                '${p.bpm.toStringAsFixed(1)} BPM',
                                style: FbTypography.monoReadout(
                                  c,
                                  size: 12.0,
                                  color: c.accentPrimary,
                                ),
                              ),
                              const SizedBox(width: 4),
                              InkWell(
                                onTap: () => controller.setBpm(p.bpm + 1.0),
                                child: Icon(LucideIcons.plus,
                                    size: 12, color: c.textMuted),
                              ),
                            ],
                          ),
                        ),
                        const SizedBox(width: FbSpace.s3),
                        Text(
                          '${p.timeSigNum}/${p.timeSigDen}',
                          style: FbTypography.monoReadout(
                            c,
                            size: 11.5,
                            color: c.textMuted,
                          ),
                        ),
                      ],
                    ),
                  ),
                  const SizedBox(width: FbSpace.s3),

                  // Spatial Binaural 3D Headphone Monitoring Latch
                  FbButton(
                    label: 'Binaural 3D',
                    icon: LucideIcons.headphones,
                    active: p.masterBinauralEnabled,
                    activeColor: c.stateAuto,
                    compact: true,
                    colors: c,
                    onTap: controller.toggleMasterBinaural,
                  ),
                  const SizedBox(width: FbSpace.s3),

                  // Master Output Mini-Meter + Peak Readout
                  Container(
                    padding: const EdgeInsets.symmetric(
                      horizontal: FbSpace.s2 + 2,
                      vertical: 4,
                    ),
                    decoration: BoxDecoration(
                      color: c.bgSecondary,
                      borderRadius: BorderRadius.circular(FbRadius.sm),
                      border: Border.all(color: c.borderSubtle),
                    ),
                    child: Row(
                      children: [
                        Text('MST', style: FbTypography.micro(c)),
                        const SizedBox(width: 6),
                        FbStereoMeter(
                          peakLDb: tel.masterPeakLDb,
                          peakRDb: tel.masterPeakRDb,
                          colors: c,
                          width: 68,
                          horizontal: true,
                        ),
                        const SizedBox(width: 6),
                        Text(
                          '${tel.masterPeakLDb.max(tel.masterPeakRDb).toStringAsFixed(1)} dB',
                          style: FbTypography.monoReadout(
                            c,
                            size: 10.5,
                            color: tel.masterPeakLDb > -0.5 ||
                                    tel.masterPeakRDb > -0.5
                                ? c.meterRed
                                : c.textSecondary,
                          ),
                        ),
                      ],
                    ),
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _presetChip({
    required String label,
    required StudioLayoutPreset preset,
    required FbColors c,
  }) {
    final active = controller.activePreset == preset;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 1),
      child: InkWell(
        onTap: () => controller.applyLayoutPreset(preset),
        borderRadius: BorderRadius.circular(FbRadius.xs),
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
          decoration: BoxDecoration(
            color: active ? c.accentSoft : Colors.transparent,
            borderRadius: BorderRadius.circular(FbRadius.xs),
            border: Border.all(
              color: active ? c.accentPrimary : Colors.transparent,
              width: 1.0,
            ),
          ),
          child: Text(
            label,
            style: TextStyle(
              fontFamily: FbTypography.fontSans,
              fontSize: 11.0,
              fontWeight: active ? FontWeight.w700 : FontWeight.w500,
              color: active ? c.accentPrimary : c.textSecondary,
            ),
          ),
        ),
      ),
    );
  }
}

extension _DoubleClampExt on double {
  double max0() => this < 0.0 ? 0.0 : this;
  double max(double other) => this > other ? this : other;
}
